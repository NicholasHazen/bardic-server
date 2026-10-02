//! M3b: making audio with a free voice (against a fake Breeze), jobs, delivery, timings and samples.
mod common;
use common::{breeze::FakeBreeze, TestServer, DEVICE};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::Duration;

const SRC: &str = "/api/voice-sources/{source_id}";
const JOB: &str = "/api/jobs/{job_id}";
const REQ: &str = "/api/audiobooks/{audiobook_id}/chapters/{chapter_id}/request";
const READY: &str = "/api/audiobooks/{audiobook_id}/make-ready";

struct Fx {
    s: TestServer,
    b: FakeBreeze,
    book: String,
    audiobook: String,
    chapters: Vec<String>,
}

impl Fx {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let s = TestServer::start_with(dir, |c| {
            c.audio_chunk_chars = 400;
            c.job_retry_ms = 20;
        })
        .await;
        Self::on(s, FakeBreeze::start().await, None).await
    }

    /// Set up a listener, the Breeze source, the sample book and an audiobook for Mara.
    async fn on(s: TestServer, b: FakeBreeze, book: Option<String>) -> Self {
        let l = s.listener("Nick").await;
        s.act_as(&l);
        s.put(
            SRC,
            "/api/voice-sources/breeze",
            json!({ "base_url": b.url }),
            200,
        )
        .await;
        let book = match book {
            Some(b) => b,
            None => s
                .post("/api/books/sample", "/api/books/sample", json!({}), 201)
                .await["id"]
                .as_str()
                .unwrap()
                .to_string(),
        };
        let v = s.get("/api/voices", "/api/voices", 200).await["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "Mara")
            .unwrap()
            .clone();
        let ab = s
            .post(
                "/api/books/{book_id}/audiobooks",
                &format!("/api/books/{book}/audiobooks"),
                json!({ "voice_id": v["id"] }),
                201,
            )
            .await;
        let audiobook = ab["id"].as_str().unwrap().to_string();
        let ch = s
            .get(
                "/api/books/{book_id}/chapters",
                &format!("/api/books/{book}/chapters"),
                200,
            )
            .await;
        let chapters = ch["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_string())
            .collect();
        Fx {
            s,
            b,
            book,
            audiobook,
            chapters,
        }
    }

    fn req_path(&self, ch: usize) -> String {
        format!(
            "/api/audiobooks/{}/chapters/{}/request",
            self.audiobook, self.chapters[ch]
        )
    }

    async fn request(&self, ch: usize, body: Value, expect: u16) -> Value {
        self.s
            .call(
                Method::POST,
                REQ,
                &self.req_path(ch),
                Some(DEVICE),
                Some(body),
                expect,
            )
            .await
    }

    async fn make_ready(&self, scope: Value, expect: u16) -> Value {
        self.s
            .post(
                READY,
                &format!("/api/audiobooks/{}/make-ready", self.audiobook),
                json!({ "scope": scope }),
                expect,
            )
            .await
    }

    async fn job(&self, id: &str) -> Value {
        self.s.get(JOB, &format!("/api/jobs/{id}"), 200).await
    }

    async fn wait(&self, id: &str, states: &[&str]) -> Value {
        for _ in 0..400 {
            let j = self.job(id).await;
            if states.contains(&j["state"].as_str().unwrap()) {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("job {id} did not reach {states:?}: {}", self.job(id).await);
    }

    async fn states(&self) -> Vec<(String, Value)> {
        let v = self
            .s
            .get(
                "/api/audiobooks/{audiobook_id}/chapters",
                &format!("/api/audiobooks/{}/chapters", self.audiobook),
                200,
            )
            .await;
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["state"].as_str().unwrap().to_string(), c.clone()))
            .collect()
    }

    async fn audiobook(&self) -> Value {
        self.s
            .get(
                "/api/audiobooks/{audiobook_id}",
                &format!("/api/audiobooks/{}", self.audiobook),
                200,
            )
            .await
    }

    async fn chapter_text(&self, ch: usize) -> Value {
        let (b, c) = (&self.book, &self.chapters[ch]);
        self.s
            .get(
                "/api/books/{book_id}/chapters/{chapter_id}/text",
                &format!("/api/books/{b}/chapters/{c}/text"),
                200,
            )
            .await
    }
}

#[tokio::test]
async fn pressing_play_makes_the_chapter_and_the_next_and_serves_them() {
    let f = Fx::new().await;
    let job = f.request(0, json!({}), 202).await;
    assert_eq!(job["kind"], "make_audio");
    assert_eq!(job["chapters_total"], 2, "the chapter and one ahead");
    assert_eq!(job["started_by"]["device_id"], DEVICE);
    let done = f.wait(job["id"].as_str().unwrap(), &["completed"]).await;
    assert_eq!(done["chapters_done"], 2);
    assert_eq!(done["current_chapter_id"], Value::Null);

    let st = f.states().await;
    assert_eq!(
        st.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
        vec!["ready", "ready", "not_yet"]
    );
    let ab = f.audiobook().await;
    assert_eq!(ab["chapters_ready"], 2);
    assert_eq!(ab["active_job_id"], Value::Null);
    assert!(ab["bytes"].as_i64().unwrap() > 1000);

    // pressing play again on a ready chapter answers at once
    let again = f.request(0, json!({}), 200).await;
    assert_eq!(again["state"], "ready");
    let audio = &again["audio"];
    assert_eq!(audio["content_type"], "audio/wav");

    // delivery: whole, ranges, immutable
    let url = audio["url"].as_str().unwrap();
    let id = audio["id"].as_str().unwrap();
    let (h, all) = f.s.raw("/api/audio/{audio_id}", url, &[], 200).await;
    assert_eq!(&all[..4], b"RIFF");
    assert_eq!(all.len() as i64, audio["bytes"].as_i64().unwrap());
    assert_eq!(h["cache-control"], "public, max-age=31536000, immutable");
    assert_eq!(h["etag"], format!("\"{id}\""));
    assert_eq!(h["accept-ranges"], "bytes");
    let (h, part) =
        f.s.raw(
            "/api/audio/{audio_id}",
            url,
            &[("range", "bytes=10-19")],
            206,
        )
        .await;
    assert_eq!(part, &all[10..20]);
    assert_eq!(h["content-range"], format!("bytes 10-19/{}", all.len()));
    let (_, tail) =
        f.s.raw("/api/audio/{audio_id}", url, &[("range", "bytes=-8")], 206)
            .await;
    assert_eq!(tail, &all[all.len() - 8..]);
    let (_, rest) =
        f.s.raw(
            "/api/audio/{audio_id}",
            url,
            &[("range", &format!("bytes={}-", all.len() - 4))],
            206,
        )
        .await;
    assert_eq!(rest.len(), 4);
    f.s.raw(
        "/api/audio/{audio_id}",
        url,
        &[("range", "bytes=99999999-")],
        416,
    )
    .await;
    f.s.raw("/api/audio/{audio_id}", "/api/audio/nope", &[], 404)
        .await;

    // the sha256 is of exactly these bytes
    assert_eq!(audio["sha256"], bardic_server::cover::sha256_hex(&all));
}

#[tokio::test]
async fn timings_cover_every_line_in_order_inside_the_audio() {
    let f = Fx::new().await;
    let job = f.request(1, json!({ "ahead": 0 }), 202).await;
    assert_eq!(job["chapters_total"], 1);
    f.wait(job["id"].as_str().unwrap(), &["completed"]).await;
    let audio = f.states().await[1].1["audio"].clone();
    let t =
        f.s.get(
            "/api/audio/{audio_id}/timings",
            audio["timings_url"].as_str().unwrap(),
            200,
        )
        .await;
    assert_eq!(t["audio_id"], audio["id"]);

    let text = f.chapter_text(1).await;
    let ids: Vec<&str> = text["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap())
        .collect();
    let lines = t["lines"].as_array().unwrap();
    assert_eq!(
        lines
            .iter()
            .map(|l| l["line_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ids
    );
    let dur_ms = (audio["duration_seconds"].as_f64().unwrap() * 1000.0).round() as i64;
    let mut prev = 0;
    for l in lines {
        let (s, e) = (
            l["start_ms"].as_i64().unwrap(),
            l["end_ms"].as_i64().unwrap(),
        );
        assert!(
            prev <= s && s <= e && e <= dur_ms,
            "line {l} in order inside {dur_ms}"
        );
        prev = s;
    }
    assert!(
        lines.last().unwrap()["end_ms"].as_i64().unwrap() >= dur_ms - 50,
        "the last line ends with the audio"
    );
    f.s.get(
        "/api/audio/{audio_id}/timings",
        "/api/audio/nope/timings",
        404,
    )
    .await;
}

#[tokio::test]
async fn the_exact_chapter_text_is_what_is_spoken_in_whole_line_requests() {
    let f = Fx::new().await;
    let job = f.request(0, json!({ "ahead": 0 }), 202).await;
    f.wait(job["id"].as_str().unwrap(), &["completed"]).await;
    let text = f.chapter_text(0).await;
    let chars: Vec<char> = text["text"].as_str().unwrap().chars().collect();
    let lines = text["lines"].as_array().unwrap();
    let spoken = f.b.spoken();
    assert!(
        spoken.len() > 1,
        "a 400-character limit splits the chapter: {}",
        spoken.len()
    );
    let first = lines[0]["start"].as_u64().unwrap() as usize;
    let last = lines.last().unwrap()["end"].as_u64().unwrap() as usize;
    // the requests, joined, are the chapter text from the first line to the last, exactly
    let joined: String = spoken.concat();
    let expected: String = chars[first..last].iter().collect();
    // requests break between lines, dropping only the whitespace between them
    let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    assert_eq!(squash(&joined), squash(&expected));
    assert!(spoken
        .iter()
        .all(|s| s.chars().count() <= 400 || !s.contains("\n\n")));
}

#[tokio::test]
async fn make_ready_joins_is_idempotent_and_skips_what_is_made() {
    let f = Fx::new().await;
    let path = format!("/api/audiobooks/{}/make-ready", f.audiobook);
    let post = |key: &'static str| {
        let (s, path) = (&f.s, path.clone());
        let listener = s.as_listener.lock().unwrap().clone().unwrap();
        async move {
            let r = s
                .client
                .post(format!("{}{}", s.base, path))
                .header("x-bardic-device", DEVICE)
                .header("x-bardic-listener", listener)
                .header("idempotency-key", key)
                .json(&json!({ "scope": { "kind": "whole_book" } }))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 202);
            let v: Value = r.json().await.unwrap();
            s.contract.check("POST", READY, 202, Some(&v)).unwrap();
            v
        }
    };
    let a = post("k1").await;
    let b = post("k1").await;
    assert_eq!(a["id"], b["id"], "the same key returns the same job");
    assert_eq!(a["chapters_total"], 3);
    f.wait(a["id"].as_str().unwrap(), &["completed"]).await;
    assert!(f.states().await.iter().all(|(s, _)| s == "ready"));
    assert_eq!(f.audiobook().await["chapters_ready"], 3);

    // nothing left to make: a job that is complete at once
    let again = f.make_ready(json!({ "kind": "whole_book" }), 202).await;
    assert_eq!(again["chapters_total"], 0);
    f.wait(again["id"].as_str().unwrap(), &["completed"]).await;

    // scopes and their errors
    let e = f.make_ready(json!({ "kind": "sideways" }), 400).await;
    assert_eq!(e["code"], "invalid_request");
    let e = f.make_ready(json!({ "kind": "from_chapter" }), 400).await;
    assert_eq!(e["code"], "invalid_request");
    let e = f
        .make_ready(
            json!({ "kind": "from_chapter", "from_chapter_id": "nope" }),
            404,
        )
        .await;
    assert_eq!(e["code"], "chapter_not_found");
    f.make_ready(json!({ "kind": "chapters", "chapter_ids": [] }), 400)
        .await;
    f.s.post(
        READY,
        "/api/audiobooks/nope/make-ready",
        json!({ "scope": { "kind": "whole_book" } }),
        404,
    )
    .await;
    f.s.stop().await;
}

#[tokio::test]
async fn scopes_make_only_what_they_name() {
    let f = Fx::new().await;
    let j = f
        .make_ready(
            json!({ "kind": "from_chapter", "from_chapter_id": f.chapters[1] }),
            202,
        )
        .await;
    assert_eq!(j["chapters_total"], 2);
    f.wait(j["id"].as_str().unwrap(), &["completed"]).await;
    let st: Vec<String> = f.states().await.into_iter().map(|(s, _)| s).collect();
    assert_eq!(st, vec!["not_yet", "ready", "ready"]);
    let j = f
        .make_ready(
            json!({ "kind": "chapters", "chapter_ids": [f.chapters[0], f.chapters[1]] }),
            202,
        )
        .await;
    assert_eq!(j["chapters_total"], 1, "chapter 1 was already made");
    f.wait(j["id"].as_str().unwrap(), &["completed"]).await;
    let listed =
        f.s.get(
            "/api/jobs",
            &format!("/api/jobs?audiobook_id={}&state=completed", f.audiobook),
            200,
        )
        .await;
    assert_eq!(listed["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        f.s.get("/api/jobs", "/api/jobs?active=true", 200).await["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    f.s.get("/api/jobs", "/api/jobs?state=weird", 400).await;
    let page = f.s.get("/api/jobs", "/api/jobs?limit=1", 200).await;
    assert!(page["next"].is_string());
    f.s.stop().await;
}

#[tokio::test]
async fn pausing_stops_the_work_and_resuming_finishes_it() {
    let f = Fx::new().await;
    f.b.state.lock().unwrap().delay_ms = 150;
    let j = f.make_ready(json!({ "kind": "whole_book" }), 202).await;
    let id = j["id"].as_str().unwrap().to_string();
    let jp = format!("/api/jobs/{id}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let paused =
        f.s.post(
            "/api/jobs/{job_id}/pause",
            &format!("{jp}/pause"),
            json!({}),
            200,
        )
        .await;
    assert_eq!(paused["state"], "paused");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let n = f.b.spoken().len();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(f.b.spoken().len(), n, "no requests while paused");
    assert_eq!(f.job(&id).await["state"], "paused");
    f.s.post(
        "/api/jobs/{job_id}/pause",
        &format!("{jp}/pause"),
        json!({}),
        409,
    )
    .await;
    let st = f.states().await;
    assert!(
        st.iter().all(|(s, _)| s != "making"),
        "a paused job is not making anything"
    );
    assert!(st.iter().any(|(_, c)| c["detail"]["code"] == "paused"));

    f.b.state.lock().unwrap().delay_ms = 0;
    let r =
        f.s.post(
            "/api/jobs/{job_id}/resume",
            &format!("{jp}/resume"),
            json!({}),
            200,
        )
        .await;
    assert!(["queued", "running"].contains(&r["state"].as_str().unwrap()));
    f.s.post(
        "/api/jobs/{job_id}/resume",
        &format!("{jp}/resume"),
        json!({}),
        409,
    )
    .await;
    f.wait(&id, &["completed"]).await;
    assert!(f.states().await.iter().all(|(s, _)| s == "ready"));
    f.s.post(
        "/api/jobs/{job_id}/pause",
        &format!("{jp}/pause"),
        json!({}),
        409,
    )
    .await;
    f.s.get(JOB, "/api/jobs/nope", 404).await;
    f.s.stop().await;
}

#[tokio::test]
async fn cancelling_keeps_what_was_finished_and_is_idempotent() {
    let f = Fx::new().await;
    let j = f.request(0, json!({ "ahead": 0 }), 202).await;
    f.wait(j["id"].as_str().unwrap(), &["completed"]).await;
    f.b.state.lock().unwrap().delay_ms = 200;
    let j = f.make_ready(json!({ "kind": "whole_book" }), 202).await;
    let id = j["id"].as_str().unwrap().to_string();
    let cp = format!("/api/jobs/{id}/cancel");
    let c =
        f.s.post("/api/jobs/{job_id}/cancel", &cp, json!({}), 200)
            .await;
    assert_eq!(c["state"], "stopped");
    assert_eq!(
        f.s.post("/api/jobs/{job_id}/cancel", &cp, json!({}), 200)
            .await["state"],
        "stopped"
    );
    f.s.post(
        "/api/jobs/{job_id}/resume",
        &format!("/api/jobs/{id}/resume"),
        json!({}),
        409,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let st: Vec<String> = f.states().await.into_iter().map(|(s, _)| s).collect();
    assert_eq!(st[0], "ready", "finished audio is kept");
    assert_eq!(f.job(&id).await["state"], "stopped");
    // a finished job cannot be cancelled into stopped
    let done = f.request(0, json!({}), 200).await;
    assert_eq!(done["state"], "ready");
    f.s.stop().await;
}

#[tokio::test]
async fn a_server_that_is_down_needs_you_and_resumes_when_it_is_back() {
    let f = Fx::new().await;
    f.b.state.lock().unwrap().down = true;
    let j = f.request(0, json!({ "ahead": 0 }), 202).await;
    let id = j["id"].as_str().unwrap().to_string();
    let stuck = f.wait(&id, &["needs_you"]).await;
    assert_eq!(stuck["needs_you"]["code"], "source_unreachable");
    let st = f.states().await;
    assert_eq!(st[0].0, "not_yet");
    assert_eq!(st[0].1["detail"]["code"], "source_unreachable");

    f.b.state.lock().unwrap().down = false;
    f.s.post(
        "/api/jobs/{job_id}/resume",
        &format!("/api/jobs/{id}/resume"),
        json!({}),
        200,
    )
    .await;
    let done = f.wait(&id, &["completed"]).await;
    assert_eq!(done["needs_you"], Value::Null);
    assert_eq!(f.states().await[0].0, "ready");
    f.s.stop().await;
}

#[tokio::test]
async fn a_busy_server_is_waited_for() {
    let f = Fx::new().await;
    f.b.state.lock().unwrap().busy_left = 2;
    let j = f.request(0, json!({ "ahead": 0 }), 202).await;
    f.wait(j["id"].as_str().unwrap(), &["completed"]).await;
    assert_eq!(f.states().await[0].0, "ready");
    f.s.stop().await;
}

#[tokio::test]
async fn a_refused_chapter_fails_alone_and_can_be_retried() {
    let f = Fx::new().await;
    f.b.state.lock().unwrap().refuse = Some("segment_too_long".into());
    let j = f
        .make_ready(
            json!({ "kind": "from_chapter", "from_chapter_id": f.chapters[1] }),
            202,
        )
        .await;
    let id = j["id"].as_str().unwrap().to_string();
    let stuck = f.wait(&id, &["needs_you"]).await;
    assert_eq!(stuck["needs_you"]["code"], "provider_refused");
    assert!(
        !stuck.to_string().contains("echoes the text"),
        "the server's own message is never passed on"
    );
    f.b.state.lock().unwrap().refuse = None;
    f.s.post(
        "/api/jobs/{job_id}/resume",
        &format!("/api/jobs/{id}/resume"),
        json!({}),
        200,
    )
    .await;
    f.wait(&id, &["completed"]).await;
    assert_eq!(f.audiobook().await["chapters_ready"], 2);
    f.s.stop().await;
}

#[tokio::test]
async fn a_voice_that_changed_is_never_used_for_an_old_audiobook() {
    let f = Fx::new().await;
    f.b.state.lock().unwrap().reference = b"a different clip".to_vec();
    let j = f.request(0, json!({ "ahead": 0 }), 202).await;
    let stuck = f.wait(j["id"].as_str().unwrap(), &["needs_you"]).await;
    assert_eq!(stuck["needs_you"]["code"], "voice_changed");
    assert_eq!(f.b.spoken().len(), 0, "nothing was spoken");
    assert_eq!(f.audiobook().await["chapters_ready"], 0);
    f.s.stop().await;
}

#[tokio::test]
async fn making_audio_needs_a_connected_source() {
    let f = Fx::new().await;
    f.s.delete(SRC, "/api/voice-sources/breeze", 204).await;
    let e = f.request(0, json!({}), 409).await;
    assert_eq!(e["code"], "source_not_set_up");
    let e =
        f.s.post(
            READY,
            &format!("/api/audiobooks/{}/make-ready", f.audiobook),
            json!({ "scope": { "kind": "whole_book" } }),
            409,
        )
        .await;
    assert_eq!(e["code"], "source_not_set_up");
    f.request(0, json!({ "ahead": 9 }), 400).await;
    f.s.call(
        Method::POST,
        REQ,
        "/api/audiobooks/nope/chapters/x/request",
        Some(DEVICE),
        None,
        404,
    )
    .await;
    let e =
        f.s.call(
            Method::POST,
            REQ,
            &format!("/api/audiobooks/{}/chapters/nope/request", f.audiobook),
            Some(DEVICE),
            None,
            404,
        )
        .await;
    assert_eq!(e["code"], "chapter_not_found");
    f.s.stop().await;
}

#[tokio::test]
async fn a_voice_sample_is_made_once_per_revision() {
    let f = Fx::new().await;
    let v = f.s.get("/api/voices", "/api/voices", 200).await["items"][0].clone();
    let path = format!("/api/voices/{}/sample", v["id"].as_str().unwrap());
    let (h, a) =
        f.s.raw(
            "/api/voices/{voice_id}/sample",
            &path,
            &[("x-bardic-device", DEVICE)],
            200,
        )
        .await;
    assert_eq!(&a[..4], b"RIFF");
    assert_eq!(h["content-type"], "audio/wav");
    let n = f.b.spoken().len();
    assert_eq!(n, 1);
    let (_, b) =
        f.s.raw(
            "/api/voices/{voice_id}/sample",
            &path,
            &[("range", "bytes=0-3"), ("x-bardic-device", DEVICE)],
            206,
        )
        .await;
    assert_eq!(b, b"RIFF");
    assert_eq!(f.b.spoken().len(), n, "the repeat is served from the cache");
    f.s.raw(
        "/api/voices/{voice_id}/sample",
        "/api/voices/nope/sample",
        &[("x-bardic-device", DEVICE)],
        404,
    )
    .await;
    f.s.stop().await;
}

#[tokio::test]
async fn a_restart_keeps_finished_chapters_and_carries_on() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = |c: &mut bardic_server::config::Config| {
        c.audio_chunk_chars = 400;
        c.job_retry_ms = 20;
    };
    let s = TestServer::start_with(dir, cfg).await;
    let b = FakeBreeze::start().await;
    let f = Fx::on(s, b, None).await;
    let j = f.request(0, json!({ "ahead": 0 }), 202).await;
    f.wait(j["id"].as_str().unwrap(), &["completed"]).await;
    f.b.state.lock().unwrap().delay_ms = 300;
    let j = f.make_ready(json!({ "kind": "whole_book" }), 202).await;
    let id = j["id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(120)).await;
    let (b, audiobook, book, chapters) = (f.b, f.audiobook, f.book, f.chapters);
    let dir = f.s.stop().await; // the server stops in the middle of a chapter

    let s = TestServer::start_with(dir, cfg).await;
    let l = s.listener("Sam").await;
    s.act_as(&l);
    b.state.lock().unwrap().delay_ms = 0;
    let f = Fx {
        s,
        b,
        book,
        audiobook,
        chapters,
    };
    let done = f.wait(&id, &["completed"]).await;
    assert_eq!(done["chapters_done"], 2, "the two that were still missing");
    assert!(f.states().await.iter().all(|(s, _)| s == "ready"));
    // no half-made files are left behind
    let audio_dir = f.s.dir.path().join("audio").join(&f.audiobook);
    let leftovers: Vec<_> = std::fs::read_dir(audio_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "part"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    f.s.stop().await;
}
