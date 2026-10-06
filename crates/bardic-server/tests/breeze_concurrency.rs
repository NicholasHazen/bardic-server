mod common;

use bardic_server::{app::AppState, clock::FakeClock, config::Config, store::StoreError};
use common::{
    breeze::FakeBreeze,
    gemini::{money, FakeGemini},
    TestServer,
};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

const TAGS: [&str; 7] = ["Alpha", "Beta", "Gamma", "Delta", "Epsilon", "Zeta", "Eta"];
const JOB: &str = "/api/jobs/{job_id}";

struct Fx {
    s: TestServer,
    b: FakeBreeze,
    audiobook: String,
    chapter: String,
    voice: String,
    requests: Vec<String>,
    lines: Vec<Value>,
    gates: Vec<Arc<Semaphore>>,
}

impl Fx {
    async fn new(concurrency: usize, gated: bool) -> Self {
        let s = TestServer::start_with(tempfile::tempdir().unwrap(), |c| {
            c.breeze_concurrency = concurrency;
            c.audio_chunk_chars = 400;
            c.job_retry_ms = 10;
        })
        .await;
        let listener = s.listener("Concurrent gardener").await;
        s.act_as(&listener);
        let b = FakeBreeze::start().await;
        s.put(
            "/api/voice-sources/{source_id}",
            "/api/voice-sources/breeze",
            json!({ "base_url": b.url }),
            200,
        )
        .await;
        let sentence =
            "Café lanterns floated over the quiet garden while 林🌿 carried a brass bird home. ";
        let text = TAGS
            .iter()
            .map(|tag| format!("{tag}: {}", sentence.repeat(6)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let book = s.add_book("Concurrent Garden.txt", text.into_bytes()).await;
        let voices = s
            .get("/api/voices", "/api/voices?source_id=breeze", 200)
            .await;
        let voice = voices["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|voice| voice["name"] == "Mara")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let audiobook = s
            .post(
                "/api/books/{book_id}/audiobooks",
                &format!("/api/books/{book}/audiobooks"),
                json!({ "voice_id": voice }),
                201,
            )
            .await["id"]
            .as_str()
            .unwrap()
            .to_string();
        let chapters = s
            .get(
                "/api/books/{book_id}/chapters",
                &format!("/api/books/{book}/chapters"),
                200,
            )
            .await;
        assert_eq!(chapters["items"].as_array().unwrap().len(), 1);
        let chapter = chapters["items"][0]["id"].as_str().unwrap().to_string();
        let text = s
            .get(
                "/api/books/{book_id}/chapters/{chapter_id}/text",
                &format!("/api/books/{book}/chapters/{chapter}/text"),
                200,
            )
            .await;
        let chars: Vec<char> = text["text"].as_str().unwrap().chars().collect();
        let lines = text["lines"].as_array().unwrap().clone();
        assert_eq!(
            lines.len(),
            TAGS.len(),
            "one original paragraph per request"
        );
        let requests: Vec<String> = lines
            .iter()
            .map(|line| {
                chars[line["start"].as_u64().unwrap() as usize
                    ..line["end"].as_u64().unwrap() as usize]
                    .iter()
                    .collect()
            })
            .collect();
        for (request, tag) in requests.iter().zip(TAGS) {
            assert!(request.starts_with(tag));
            assert!(request.chars().count() > 400 && request.chars().count() < 600);
        }
        let gates: Vec<_> = TAGS.iter().map(|_| Arc::new(Semaphore::new(0))).collect();
        {
            let mut fake = b.state.lock().unwrap();
            fake.tagged_markers = TAGS
                .iter()
                .enumerate()
                .map(|(i, tag)| (format!("{tag}:"), i as u8 + 11))
                .collect();
            if gated {
                fake.tagged_gates = TAGS
                    .iter()
                    .zip(&gates)
                    .map(|(tag, gate)| (format!("{tag}:"), gate.clone()))
                    .collect();
            }
        }
        Fx {
            s,
            b,
            audiobook,
            chapter,
            voice,
            requests,
            lines,
            gates,
        }
    }

    async fn start(&self) -> String {
        self.s
            .post(
                "/api/audiobooks/{audiobook_id}/chapters/{chapter_id}/request",
                &format!(
                    "/api/audiobooks/{}/chapters/{}/request",
                    self.audiobook, self.chapter
                ),
                json!({ "ahead": 0 }),
                202,
            )
            .await["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn job(&self, id: &str) -> Value {
        self.s.get(JOB, &format!("/api/jobs/{id}"), 200).await
    }

    async fn complete(&self, id: &str) {
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let job = self.job(id).await;
                if job["state"] == "completed" {
                    return;
                }
                assert_ne!(job["state"], "needs_you", "{job}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("job completed");
    }

    async fn retained(&self) -> Vec<usize> {
        let (a, ch) = (self.audiobook.clone(), self.chapter.clone());
        self.s.running.as_ref().unwrap().state.store.run(move |c| {
            Ok(c.prepare("SELECT request_index FROM chapter_requests WHERE audiobook_id=?1 AND chapter_id=?2 ORDER BY request_index")?
                .query_map([a, ch], |row| row.get::<_, usize>(0))?
                .collect::<Result<Vec<_>, _>>()?)
        }).await.unwrap()
    }

    async fn wait_retained(&self, count: usize) -> Vec<usize> {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let rows = self.retained().await;
                if rows.len() >= count {
                    return rows;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("out-of-order requests became durable")
    }

    async fn chapter(&self) -> Value {
        self.s
            .get(
                "/api/audiobooks/{audiobook_id}/chapters",
                &format!("/api/audiobooks/{}/chapters", self.audiobook),
                200,
            )
            .await["items"][0]
            .clone()
    }

    async fn verify_ordered_audio(&self) {
        let chapter = self.chapter().await;
        assert_eq!(chapter["state"], "ready");
        let audio = &chapter["audio"];
        let (_, wav) = self
            .s
            .raw(
                "/api/audio/{audio_id}",
                audio["url"].as_str().unwrap(),
                &[],
                200,
            )
            .await;
        let mut cursor = 44;
        for (i, text) in self.requests.iter().enumerate() {
            let bytes = text.chars().count() * 480;
            assert!(
                wav[cursor..cursor + bytes]
                    .iter()
                    .all(|byte| *byte == i as u8 + 11),
                "request {i} was assembled out of source order"
            );
            cursor += bytes;
        }
        assert_eq!(cursor, wav.len());
        let timings = self
            .s
            .get(
                "/api/audio/{audio_id}/timings",
                audio["timings_url"].as_str().unwrap(),
                200,
            )
            .await;
        let mut millis = 0;
        for ((line, text), timing) in self
            .lines
            .iter()
            .zip(&self.requests)
            .zip(timings["lines"].as_array().unwrap())
        {
            assert_eq!(timing["line_id"], line["id"]);
            assert_eq!(timing["start_ms"], millis);
            millis += text.chars().count() * 10;
            assert_eq!(timing["end_ms"], millis);
        }
        assert_eq!(timings["lines"].as_array().unwrap().len(), self.lines.len());
        assert_eq!(
            audio["duration_seconds"].as_f64().unwrap(),
            millis as f64 / 1000.0
        );
        assert!(
            self.retained().await.is_empty(),
            "Ready cleans indexed request rows"
        );
    }
}

async fn wait_received(b: &FakeBreeze, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while b.received() < count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fake received expected concurrent requests");
}

async fn wait_inactive(b: &FakeBreeze) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while b.state.lock().unwrap().active != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("cancelled streams released provider flights");
}

async fn stored_generation(f: &Fx, job: &str) -> Value {
    let job = job.to_string();
    f.s.running
        .as_ref()
        .unwrap()
        .state
        .store
        .run(move |c| {
            let value: String =
                c.query_row("SELECT generation FROM jobs WHERE id=?1", [job], |r| {
                    r.get(0)
                })?;
            Ok(serde_json::from_str(&value).unwrap())
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn one_refusal_cancels_siblings_without_admitting_more_and_keeps_completed_requests() {
    let f = Fx::new(3, true).await;
    f.b.state.lock().unwrap().tagged_refusals = vec![("Alpha:".into(), "synthetic_refusal".into())];
    let job = f.start().await;
    wait_received(&f.b, 3).await;
    f.gates[2].add_permits(1);
    assert_eq!(f.wait_retained(1).await, vec![2]);
    wait_received(&f.b, 4).await;
    assert_eq!(f.b.state.lock().unwrap().active, 3);
    f.gates[0].add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.job(&job).await["state"] != "needs_you" {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("refusal reached Needs you after draining siblings");
    wait_inactive(&f.b).await;
    assert_eq!(f.b.received(), 4, "no new admission after the refusal");
    assert_eq!(f.retained().await, vec![2]);
    assert!(f.chapter().await["audio"].is_null());
    let generation = stored_generation(&f, &job).await;
    assert_eq!(generation["requests_done"], 1);
    assert_eq!(generation["active_requests"], 0);
    assert!(generation["request_started_at"].is_null());
    assert_eq!(generation["attempt_id"], "");
    assert_eq!(
        f.s.running
            .as_ref()
            .unwrap()
            .state
            .gates
            .breeze
            .available_permits(),
        3
    );
    {
        let mut fake = f.b.state.lock().unwrap();
        fake.tagged_refusals.clear();
        fake.tagged_gates.clear();
    }
    f.s.post(
        "/api/jobs/{job_id}/resume",
        &format!("/api/jobs/{job}/resume"),
        json!({}),
        200,
    )
    .await;
    f.complete(&job).await;
    f.verify_ordered_audio().await;
    assert_eq!(
        f.b.spoken()
            .iter()
            .filter(|text| *text == &f.requests[2])
            .count(),
        1
    );
    f.s.stop().await;
    f.b.stop();
}

#[tokio::test]
async fn pause_and_immediate_resume_drain_a_completed_sibling_blocked_on_the_store() {
    let f = Fx::new(3, true).await;
    let job = f.start().await;
    wait_received(&f.b, 3).await;
    let state = f.s.running.as_ref().unwrap().state.clone();
    let store = state.store.clone();
    let paused_job = job.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (pause_tx, pause_rx) = std::sync::mpsc::channel();
    let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocked = tokio::spawn(async move {
        store
            .run(move |c| {
                entered_tx.send(()).unwrap();
                pause_rx.recv_timeout(Duration::from_secs(8)).unwrap();
                // The real pause endpoint needs this same connection. Applying its state change
                // inside the controlled lock makes the completed-provider/pending-store race exact.
                c.execute(
                    "UPDATE jobs SET state='paused',waiting=NULL WHERE id=?1",
                    [paused_job],
                )?;
                paused_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(8)).unwrap();
                Ok(())
            })
            .await
            .unwrap();
    });
    entered_rx.await.unwrap();
    f.gates[2].add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.b.state.lock().unwrap().active != 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Gamma returned its bytes while the connection was held");
    assert_eq!(f.b.spoken(), vec![f.requests[2].clone()]);
    pause_tx.send(()).unwrap();
    paused_rx.await.unwrap();
    state.jobs.poke();
    f.b.state.lock().unwrap().tagged_gates.clear();
    release_tx.send(()).unwrap();
    // Resume through the public API immediately: the worker must finish the old attempt's
    // durable writes before any fresh attempt can inspect or replace those checkpoints.
    f.s.post(
        "/api/jobs/{job_id}/resume",
        &format!("/api/jobs/{job}/resume"),
        json!({}),
        200,
    )
    .await;
    blocked.await.unwrap();
    f.complete(&job).await;
    f.verify_ordered_audio().await;
    let generation = stored_generation(&f, &job).await;
    assert_eq!(generation["requests_done"], 7);
    assert_eq!(generation["active_requests"], 0);
    assert!(generation["request_started_at"].is_null());
    assert_eq!(generation["attempt_id"], "");
    assert!(generation["elapsed_seconds"].as_f64().unwrap().is_finite());
    {
        let fake = f.b.state.lock().unwrap();
        assert_eq!(fake.max_active, 3);
        assert_eq!(fake.spoken.len(), 7);
        assert_eq!(
            fake.received_texts
                .iter()
                .filter(|text| *text == &f.requests[2])
                .count(),
            1,
            "completed Gamma must survive cancellation during its blocking store cleanup"
        );
    }
    assert_eq!(state.gates.breeze.available_permits(), 3);
    f.s.stop().await;
    f.b.stop();
}

#[tokio::test]
async fn premium_requests_stay_sequential_when_breeze_concurrency_is_three() {
    let g = FakeGemini::start("synthetic-key").await;
    let url = g.url.clone();
    let s = TestServer::start_with(tempfile::tempdir().unwrap(), move |c| {
        c.breeze_concurrency = 3;
        c.audio_chunk_chars = 400;
        c.gemini_url = url;
    })
    .await;
    let listener = s.listener("Premium gardener").await;
    s.act_as(&listener);
    s.put(
        "/api/voice-sources/{source_id}",
        "/api/voice-sources/gemini",
        json!({"api_key":"synthetic-key"}),
        200,
    )
    .await;
    let voice = s
        .get("/api/voices", "/api/voices?source_id=gemini", 200)
        .await["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "Kore")
        .unwrap()["id"]
        .clone();
    let text = TAGS.iter().map(|tag| format!("{tag}: {}",
        "Café lanterns floated over the quiet garden while 林🌿 carried a brass bird home. ".repeat(6)))
        .collect::<Vec<_>>().join("\n\n");
    let book = s
        .add_book("Premium Concurrent Garden.txt", text.into_bytes())
        .await;
    let audiobook = s
        .post(
            "/api/books/{book_id}/audiobooks",
            &format!("/api/books/{book}/audiobooks"),
            json!({"voice_id":voice}),
            201,
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let estimate = s
        .post(
            "/api/audiobooks/{audiobook_id}/plan-preview",
            &format!("/api/audiobooks/{audiobook}/plan-preview"),
            json!({"scope":{"kind":"whole_book"}}),
            200,
        )
        .await;
    let gate = Arc::new(Semaphore::new(0));
    g.state.lock().unwrap().gate = Some(gate.clone());
    let plan = s.post("/api/plans", "/api/plans", json!({"estimate_id":estimate["estimate_id"], "limit":money(estimate["suggested_limit"]["micros"].as_i64().unwrap())}), 201).await;
    for count in 1..=7 {
        tokio::time::timeout(Duration::from_secs(5), async {
            while g.received() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("next sequential premium request reached its gate");
        assert_eq!(g.received(), count);
        assert_eq!(g.state.lock().unwrap().active, 1);
        assert_eq!(
            s.running
                .as_ref()
                .unwrap()
                .state
                .gates
                .breeze
                .available_permits(),
            3
        );
        gate.add_permits(1);
    }
    let plan_id = plan["id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let p = s
                .get(
                    "/api/plans/{plan_id}",
                    &format!("/api/plans/{plan_id}"),
                    200,
                )
                .await;
            if p["state"] == "completed" {
                break;
            }
            assert_eq!(p["state"], "running", "{p}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("approved premium plan completed");
    assert_eq!(g.state.lock().unwrap().max_active, 1);
    assert_eq!(g.spoken().len(), 7);
    assert_eq!(g.received(), 7);
    s.stop().await;
    g.stop();
}

#[test]
fn programmatic_invalid_breeze_concurrency_is_rejected_before_opening_a_database() {
    for value in [0, 17] {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::for_data_dir(dir.path());
        config.breeze_concurrency = value;
        let clock = Arc::new(FakeClock::new("2026-10-06T00:00:00Z".parse().unwrap()));
        assert!(
            matches!(AppState::new(config, clock), Err(StoreError::Io(e)) if e.kind() == std::io::ErrorKind::InvalidInput)
        );
        assert!(!dir.path().join("bardic.db").exists());
    }
}

#[tokio::test]
async fn requests_overlap_up_to_the_limit_and_assemble_pcm_and_timings_in_source_order() {
    let f = Fx::new(3, true).await;
    let job = f.start().await;
    wait_received(&f.b, 3).await;
    assert_eq!(f.b.state.lock().unwrap().active, 3);
    f.gates[2].add_permits(1);
    assert_eq!(f.wait_retained(1).await, vec![2]);
    f.gates[1].add_permits(1);
    f.wait_retained(2).await;
    for gate in f.gates.iter().skip(3) {
        gate.add_permits(1);
    }
    assert_eq!(f.wait_retained(6).await, (1..7).collect::<Vec<_>>());
    let progress = f.job(&job).await;
    assert_eq!(progress["generation"]["requests_done"], 6);
    assert_eq!(
        progress["generation"]["characters_done"],
        f.requests[1..]
            .iter()
            .map(|text| text.chars().count())
            .sum::<usize>()
    );
    assert_eq!(
        f.chapter().await["state"],
        "making",
        "durable later requests do not make the chapter Ready"
    );
    assert!(f.chapter().await["audio"].is_null());
    f.gates[0].add_permits(1);
    f.complete(&job).await;
    f.verify_ordered_audio().await;
    {
        let fake = f.b.state.lock().unwrap();
        assert_eq!(fake.max_active, 3);
        assert_eq!(fake.spoken.len(), 7);
        assert!(
            fake.spoken[0].starts_with("Gamma:"),
            "completion order was deliberately changed"
        );
    }
    assert_eq!(
        f.s.running
            .as_ref()
            .unwrap()
            .state
            .gates
            .breeze
            .available_permits(),
        3
    );
    f.s.stop().await;
    f.b.stop();
}

async fn restart_reuses_later_requests(paused: bool) {
    let f = Fx::new(2, true).await;
    for gate in f.gates.iter().skip(1) {
        gate.add_permits(1);
    }
    let job = f.start().await;
    assert_eq!(f.wait_retained(6).await, (1..7).collect::<Vec<_>>());
    if paused {
        f.s.post(
            "/api/jobs/{job_id}/pause",
            &format!("/api/jobs/{job}/pause"),
            json!({}),
            200,
        )
        .await;
        wait_inactive(&f.b).await;
        assert_eq!(f.retained().await.len(), 6);
        assert_eq!(f.job(&job).await["generation"]["requests_done"], 6);
    }
    let dir = f.s.stop().await;
    wait_inactive(&f.b).await;
    f.b.state.lock().unwrap().tagged_gates.clear();
    let s = TestServer::start_with(dir, |c| {
        c.audio_chunk_chars = 10_000;
        c.breeze_concurrency = 1;
        c.job_retry_ms = 10;
    })
    .await;
    let f = Fx { s, ..f };
    if paused {
        assert_eq!(f.job(&job).await["state"], "paused");
        f.s.post(
            "/api/jobs/{job_id}/resume",
            &format!("/api/jobs/{job}/resume"),
            json!({}),
            200,
        )
        .await;
    }
    f.complete(&job).await;
    f.verify_ordered_audio().await;
    {
        let fake = f.b.state.lock().unwrap();
        assert_eq!(fake.max_active, 2);
        for request in f.requests.iter().skip(1) {
            assert_eq!(
                fake.received_texts
                    .iter()
                    .filter(|text| *text == request)
                    .count(),
                1,
                "changing chunk size/concurrency repeated a durable later request"
            );
        }
        assert_eq!(
            fake.received_texts
                .iter()
                .filter(|text| *text == &f.requests[0])
                .count(),
            2,
            "only interrupted first request is repeated"
        );
        assert_eq!(fake.spoken.len(), 7);
    }
    f.s.stop().await;
    f.b.stop();
}

#[tokio::test]
async fn pausing_then_restarting_keeps_later_requests_when_the_first_was_blocked() {
    restart_reuses_later_requests(true).await;
}

#[tokio::test]
async fn restarting_in_flight_keeps_later_requests_when_the_first_was_blocked() {
    restart_reuses_later_requests(false).await;
}

#[tokio::test]
async fn busy_retries_release_slots_and_finish_without_repeating_durable_requests() {
    let f = Fx::new(2, false).await;
    {
        let mut fake = f.b.state.lock().unwrap();
        fake.busy_left = 2;
        fake.delay_ms = 20;
    }
    let job = f.start().await;
    f.complete(&job).await;
    f.verify_ordered_audio().await;
    assert!(f.b.state.lock().unwrap().max_active <= 2);
    assert_eq!(f.b.spoken().len(), 7);
    assert_eq!(
        f.s.running
            .as_ref()
            .unwrap()
            .state
            .gates
            .breeze
            .available_permits(),
        2
    );
    f.s.stop().await;
    f.b.stop();
}

#[tokio::test]
async fn free_samples_use_the_shared_gate_and_cached_samples_need_no_permit() {
    let f = Fx::new(1, false).await;
    let sample_gate = Arc::new(Semaphore::new(0));
    f.b.state.lock().unwrap().tagged_gates = vec![(String::new(), sample_gate.clone())];
    let client = f.s.client.clone();
    let url = format!("{}/api/voices/{}/sample", f.s.base, f.voice);
    let sample = tokio::spawn(async move {
        client
            .get(url)
            .header("x-bardic-device", common::DEVICE)
            .send()
            .await
            .unwrap()
    });
    wait_received(&f.b, 1).await;
    assert_eq!(
        f.s.running
            .as_ref()
            .unwrap()
            .state
            .gates
            .breeze
            .available_permits(),
        0,
        "uncached sample owns the same gate as chapter requests"
    );
    sample_gate.add_permits(1);
    let response = sample.await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let audio = response.bytes().await.unwrap();
    f.s.contract
        .check("GET", "/api/voices/{voice_id}/sample", 200, None)
        .unwrap();
    assert_eq!(&audio[..4], b"RIFF");
    let held =
        f.s.running
            .as_ref()
            .unwrap()
            .state
            .gates
            .breeze
            .acquire()
            .await
            .unwrap();
    let cached = tokio::time::timeout(
        Duration::from_secs(3),
        f.s.client
            .get(format!("{}/api/voices/{}/sample", f.s.base, f.voice))
            .header("x-bardic-device", common::DEVICE)
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(cached.status().as_u16(), 200);
    assert_eq!(f.b.received(), 1);
    drop(held);
    f.s.stop().await;
    f.b.stop();
}

#[tokio::test]
async fn an_urgent_chapter_yields_background_work_at_a_retained_request_boundary() {
    let f = Fx::new(3, true).await;
    let background =
        f.s.post(
            "/api/audiobooks/{audiobook_id}/make-ready",
            &format!("/api/audiobooks/{}/make-ready", f.audiobook),
            json!({ "scope": { "kind": "whole_book" } }),
            202,
        )
        .await["id"]
            .as_str()
            .unwrap()
            .to_string();
    wait_received(&f.b, 3).await;
    let book =
        f.s.add_book(
            "Urgent Garden.txt",
            b"The urgent visitor waited by a small golden gate.".to_vec(),
        )
        .await;
    let audiobook =
        f.s.post(
            "/api/books/{book_id}/audiobooks",
            &format!("/api/books/{book}/audiobooks"),
            json!({ "voice_id": f.voice }),
            201,
        )
        .await["id"]
            .as_str()
            .unwrap()
            .to_string();
    let chapter =
        f.s.get(
            "/api/books/{book_id}/chapters",
            &format!("/api/books/{book}/chapters"),
            200,
        )
        .await["items"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
    let urgent =
        f.s.post(
            "/api/audiobooks/{audiobook_id}/chapters/{chapter_id}/request",
            &format!("/api/audiobooks/{audiobook}/chapters/{chapter}/request"),
            json!({ "ahead": 0 }),
            202,
        )
        .await["id"]
            .as_str()
            .unwrap()
            .to_string();
    // Alpha stays blocked. Completing Gamma gives the worker a durable boundary at which to yield.
    f.gates[2].add_permits(1);
    f.complete(&urgent).await;
    assert_ne!(f.job(&background).await["state"], "completed");
    assert_eq!(f.chapter().await["state"], "making");
    assert!(f
        .b
        .spoken()
        .iter()
        .any(|text| text.contains("urgent visitor")));
    f.b.state.lock().unwrap().tagged_gates.clear();
    for gate in &f.gates {
        gate.add_permits(2);
    }
    f.complete(&background).await;
    f.verify_ordered_audio().await;
    assert_eq!(
        f.b.state
            .lock()
            .unwrap()
            .received_texts
            .iter()
            .filter(|text| *text == &f.requests[2])
            .count(),
        1,
        "yielding retained Gamma instead of repeating it"
    );
    f.s.stop().await;
    f.b.stop();
}

#[tokio::test]
async fn a_complete_renamed_wav_is_adopted_after_a_crash_before_the_ready_row() {
    let f = Fx::new(3, false).await;
    let state = &f.s.running.as_ref().unwrap().state;
    let audio_id = state.new_id();
    let job = state.new_id();
    let mut pcm = Vec::new();
    let mut timings = Vec::new();
    let mut millis = 0;
    for (i, (line, text)) in f.lines.iter().zip(&f.requests).enumerate() {
        pcm.extend(std::iter::repeat_n(
            i as u8 + 11,
            text.chars().count() * 480,
        ));
        let start = millis;
        millis += text.chars().count() * 10;
        timings.push(json!({ "line_id": line["id"], "start_ms": start, "end_ms": millis }));
    }
    let (a, ch, id, jid, bytes, done, saved_timings, at) = (
        f.audiobook.clone(),
        f.chapter.clone(),
        audio_id.clone(),
        job.clone(),
        pcm.len() as i64,
        f.requests.len() as i64,
        Value::Array(timings).to_string(),
        state.now(),
    );
    state.store.run(move |c| {
        let actor = json!({ "listener_id": null, "listener_name": null, "device_id": common::DEVICE, "device_name": "Synthetic device" }).to_string();
        let tx = c.transaction()?;
        tx.execute("INSERT INTO jobs(id,kind,state,audiobook_id,book_id,chapters_total,started_by,created_at,updated_at) VALUES(?1,'make_audio','paused',?2,(SELECT book_id FROM audiobooks WHERE id=?2),1,?3,?4,?4)", rusqlite::params![jid, a, actor, at])?;
        tx.execute("INSERT INTO job_items(job_id,chapter_id,position,state) VALUES(?1,?2,0,'queued')", rusqlite::params![jid, ch])?;
        tx.execute("INSERT INTO chapter_parts(audiobook_id,chapter_id,audio_id,chunk_chars,chunks_done,pcm_bytes,timings) VALUES(?1,?2,?3,400,?4,?5,?6)", rusqlite::params![a, ch, id, done, bytes, saved_timings])?;
        tx.commit()?;
        Ok(())
    }).await.unwrap();
    let directory = state.store.data_dir().join("audio").join(&f.audiobook);
    tokio::fs::create_dir_all(&directory).await.unwrap();
    let final_path = directory.join(format!("{audio_id}.wav"));
    let mut wav = bardic_server::audio::wav_header(pcm.len() as u32).to_vec();
    wav.extend_from_slice(&pcm);
    tokio::fs::write(&final_path, &wav).await.unwrap();
    assert!(!directory.join(format!("{}.part", f.chapter)).exists());
    assert!(f.retained().await.is_empty());
    f.s.post(
        "/api/jobs/{job_id}/resume",
        &format!("/api/jobs/{job}/resume"),
        json!({}),
        200,
    )
    .await;
    f.complete(&job).await;
    assert_eq!(
        f.b.received(),
        0,
        "complete durable audio must not be synthesized again"
    );
    assert_eq!(f.chapter().await["audio"]["id"], audio_id);
    assert_eq!(tokio::fs::read(final_path).await.unwrap(), wav);
    f.verify_ordered_audio().await;
    f.s.stop().await;
    f.b.stop();
}
