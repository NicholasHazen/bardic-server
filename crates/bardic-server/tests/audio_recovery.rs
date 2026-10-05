//! Missing completed files must not stay Ready, spend during reads, or discard held-copy facts.
//! All speech comes from the local synthetic fake providers.
mod common;

use common::{breeze::FakeBreeze, gemini::FakeGemini, TestServer};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

const AB: &str = "/api/audiobooks/{audiobook_id}";
const CH: &str = "/api/audiobooks/{audiobook_id}/chapters";
const REQUEST: &str = "/api/audiobooks/{audiobook_id}/chapters/{chapter_id}/request";
const MANIFEST: &str = "/api/audiobooks/{audiobook_id}/manifest";
const PREVIEW: &str = "/api/audiobooks/{audiobook_id}/plan-preview";

enum Provider {
    Free(FakeBreeze),
    Premium(FakeGemini),
}

impl Provider {
    fn requests(&self) -> usize {
        match self {
            Self::Free(f) => f.state.lock().unwrap().spoken.len(),
            Self::Premium(f) => f.state.lock().unwrap().received,
        }
    }

    fn delay(&self, ms: u64) {
        match self {
            Self::Free(f) => f.state.lock().unwrap().delay_ms = ms,
            Self::Premium(f) => f.state.lock().unwrap().delay_ms = ms,
        }
    }
}

struct Fx {
    s: TestServer,
    provider: Provider,
    book: String,
    ab: String,
    chapters: Vec<String>,
    listener: String,
}

impl Fx {
    async fn new(premium: bool) -> Self {
        let provider = if premium {
            Provider::Premium(FakeGemini::start("synthetic-key").await)
        } else {
            Provider::Free(FakeBreeze::start().await)
        };
        let url = match &provider {
            Provider::Free(f) => f.url.clone(),
            Provider::Premium(f) => f.url.clone(),
        };
        let s = TestServer::start_with(tempfile::tempdir().unwrap(), |c| {
            c.audio_chunk_chars = 10_000;
            c.job_retry_ms = 10;
            if premium {
                c.gemini_url = url.clone();
            }
        })
        .await;
        let listener = s.listener("Synthetic reader").await;
        s.act_as(&listener);
        let source = if premium { "gemini" } else { "breeze" };
        s.put(
            "/api/voice-sources/{source_id}",
            &format!("/api/voice-sources/{source}"),
            if premium {
                json!({"api_key":"synthetic-key"})
            } else {
                json!({"base_url":url})
            },
            200,
        )
        .await;
        let book = s
            .post("/api/books/sample", "/api/books/sample", json!({}), 201)
            .await["id"]
            .as_str()
            .unwrap()
            .to_string();
        let voices = s
            .get(
                "/api/voices",
                &format!("/api/voices?source_id={source}"),
                200,
            )
            .await;
        let voice = voices["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == if premium { "Kore" } else { "Mara" })
            .unwrap();
        let ab = s
            .post(
                "/api/books/{book_id}/audiobooks",
                &format!("/api/books/{book}/audiobooks"),
                json!({"voice_id":voice["id"]}),
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
            .await["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_string())
            .collect();
        Self {
            s,
            provider,
            book,
            ab,
            chapters,
            listener,
        }
    }

    async fn states(&self) -> Value {
        self.s
            .get(CH, &format!("/api/audiobooks/{}/chapters", self.ab), 200)
            .await
    }

    async fn summary(&self) -> Value {
        self.s
            .get(AB, &format!("/api/audiobooks/{}", self.ab), 200)
            .await
    }

    async fn request(&self, chapter: usize, status: u16) -> Value {
        self.s
            .post(
                REQUEST,
                &format!(
                    "/api/audiobooks/{}/chapters/{}/request",
                    self.ab, self.chapters[chapter]
                ),
                json!({"ahead":0}),
                status,
            )
            .await
    }

    async fn preview(&self) -> Value {
        self.s
            .post(
                PREVIEW,
                &format!("/api/audiobooks/{}/plan-preview", self.ab),
                json!({"scope":{"kind":"whole_book"}}),
                200,
            )
            .await
    }

    async fn approve(&self, preview: &Value) -> Value {
        self.s.post("/api/plans", "/api/plans", json!({"estimate_id":preview["estimate_id"],"limit":{"micros":1_000_000,"currency":"USD"}}), 201).await
    }

    async fn make_all(&self) -> String {
        match self.provider {
            Provider::Free(_) => self
                .s
                .post(
                    "/api/audiobooks/{audiobook_id}/make-ready",
                    &format!("/api/audiobooks/{}/make-ready", self.ab),
                    json!({"scope":{"kind":"whole_book"}}),
                    202,
                )
                .await["id"]
                .as_str()
                .unwrap()
                .to_string(),
            Provider::Premium(_) => self.approve(&self.preview().await).await["job_id"]
                .as_str()
                .unwrap()
                .to_string(),
        }
    }

    async fn wait_job(&self, job: &str) {
        for _ in 0..300 {
            let value = self
                .s
                .get("/api/jobs/{job_id}", &format!("/api/jobs/{job}"), 200)
                .await;
            if value["state"] == "completed" {
                return;
            }
            assert!(value["state"] != "needs_you", "{value}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job did not finish");
    }

    async fn wait_first(&self) -> Value {
        for _ in 0..300 {
            let states = self.states().await;
            if states["items"][0]["state"] == "ready" {
                return states["items"][0]["audio"].clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("first chapter did not finish");
    }

    async fn path(&self, audio: &Value) -> PathBuf {
        let id = audio["id"].as_str().unwrap().to_string();
        let rel: String = self
            .s
            .running
            .as_ref()
            .unwrap()
            .state
            .store
            .run(
                move |c| Ok(c.query_row("SELECT path FROM audio WHERE id=?1", [id], |r| r.get(0))?),
            )
            .await
            .unwrap();
        self.s.dir.path().join(rel)
    }

    async fn ledger(&self) -> Value {
        self.s.get("/api/allowance", "/api/allowance", 200).await
    }
}

#[tokio::test]
async fn runtime_reads_invalidate_missing_audio_but_preserve_held_facts_and_places() {
    let f = Fx::new(false).await;
    f.wait_job(&f.make_all().await).await;
    let before = f.states().await;
    let old = before["items"][0]["audio"].clone();
    let surviving = before["items"][1]["audio"].clone();
    let (_, surviving_bytes) =
        f.s.raw(
            "/api/audio/{audio_id}",
            surviving["url"].as_str().unwrap(),
            &[],
            200,
        )
        .await;
    let first_place = f.s.put("/api/books/{book_id}/place", &format!("/api/books/{}/place", f.book), json!({"chapter_id":f.chapters[0],"offset":0,"mode":"listening","audiobook_id":f.ab,"base_revision":0}), 200).await;
    let place = f.s.put("/api/books/{book_id}/place", &format!("/api/books/{}/place", f.book), json!({"chapter_id":f.chapters[1],"offset":20,"mode":"listening","audiobook_id":f.ab,"base_revision":first_place["revision"]}), 200).await;
    let history =
        f.s.get(
            "/api/books/{book_id}/place/history",
            &format!("/api/books/{}/place/history", f.book),
            200,
        )
        .await;
    assert!(!history["items"].as_array().unwrap().is_empty());
    let ledger = f.ledger().await;
    let requests = f.provider.requests();
    std::fs::remove_file(f.path(&old).await).unwrap();

    f.s.raw(
        "/api/audio/{audio_id}",
        old["url"].as_str().unwrap(),
        &[],
        404,
    )
    .await;
    f.s.get(
        "/api/audio/{audio_id}/timings",
        old["timings_url"].as_str().unwrap(),
        404,
    )
    .await;
    assert_eq!(f.states().await["items"][0]["state"], "not_yet");
    assert_eq!(f.summary().await["chapters_ready"], 2);
    assert_eq!(
        f.s.get(MANIFEST, &format!("/api/audiobooks/{}/manifest", f.ab), 200)
            .await["chapters"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        f.provider.requests(),
        requests,
        "reads must never regenerate"
    );
    assert_eq!(f.ledger().await, ledger);
    assert_eq!(
        f.s.get(
            "/api/books/{book_id}/place",
            &format!("/api/books/{}/place", f.book),
            200
        )
        .await,
        place
    );
    assert_eq!(
        f.s.get(
            "/api/books/{book_id}/place/history",
            &format!("/api/books/{}/place/history", f.book),
            200
        )
        .await,
        history
    );
    assert_eq!(
        f.s.raw(
            "/api/audio/{audio_id}",
            surviving["url"].as_str().unwrap(),
            &[],
            200
        )
        .await
        .1,
        surviving_bytes
    );
    let old_id = old["id"].as_str().unwrap().to_string();
    let retained =
        f.s.running
            .as_ref()
            .unwrap()
            .state
            .store
            .run(move |c| {
                Ok(c.query_row(
                "SELECT deleted_at IS NOT NULL AND timings<>'' AND path<>'' FROM audio WHERE id=?1",
                [old_id],
                |r| r.get::<_, bool>(0),
            )?)
            })
            .await
            .unwrap();
    assert!(retained, "immutable facts remain for downloaded copies");

    f.s.clock.advance(chrono::Duration::seconds(1));
    f.wait_job(f.request(0, 202).await["id"].as_str().unwrap())
        .await;
    assert_ne!(f.states().await["items"][0]["audio"]["id"], old["id"]);
    assert_eq!(f.states().await["items"][1]["audio"]["id"], surviving["id"]);
    let comparison =
        f.s.post(
            "/api/audiobooks/{audiobook_id}/sync-check",
            &format!("/api/audiobooks/{}/sync-check", f.ab),
            json!({"have":[{"chapter_id":f.chapters[0],"audio_id":old["id"]}]}),
            200,
        )
        .await;
    assert_eq!(comparison["newer"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn each_read_first_reconciles_its_ready_claim_without_provider_work() {
    let f = Fx::new(false).await;
    f.wait_job(&f.make_all().await).await;
    let states = f.states().await;
    let paths = [
        f.path(&states["items"][0]["audio"]).await,
        f.path(&states["items"][1]["audio"]).await,
        f.path(&states["items"][2]["audio"]).await,
    ];
    let requests = f.provider.requests();
    std::fs::remove_file(&paths[0]).unwrap();
    assert_eq!(f.summary().await["chapters_ready"], 2);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&paths[1])
        .unwrap()
        .set_len(44)
        .unwrap();
    assert_eq!(f.states().await["items"][1]["state"], "not_yet");
    std::fs::remove_file(&paths[2]).unwrap();
    assert!(f
        .s
        .get(MANIFEST, &format!("/api/audiobooks/{}/manifest", f.ab), 200)
        .await["chapters"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(f.provider.requests(), requests);
    assert!(
        paths[1].is_file(),
        "a truncated file is kept for diagnosis, never silently deleted"
    );
}

#[tokio::test]
async fn startup_invalidates_missing_truncated_and_non_file_audio_without_generation() {
    let f = Fx::new(false).await;
    f.wait_job(&f.make_all().await).await;
    let states = f.states().await;
    let paths = [
        f.path(&states["items"][0]["audio"]).await,
        f.path(&states["items"][1]["audio"]).await,
        f.path(&states["items"][2]["audio"]).await,
    ];
    let requests = f.provider.requests();
    let ab = f.ab.clone();
    let listener = f.listener.clone();
    let dir = f.s.stop().await;
    std::fs::remove_file(&paths[0]).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&paths[1])
        .unwrap()
        .set_len(44)
        .unwrap();
    std::fs::remove_file(&paths[2]).unwrap();
    std::fs::create_dir(&paths[2]).unwrap();
    let s = TestServer::start_in(dir).await;
    s.act_as(&listener);
    // Examine the database before making any API call: this proves startup reconciliation.
    let live = s
        .running
        .as_ref()
        .unwrap()
        .state
        .store
        .run(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM audio WHERE deleted_at IS NULL",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(live, 0);
    assert_eq!(
        s.get(AB, &format!("/api/audiobooks/{ab}"), 200).await["chapters_ready"],
        0
    );
    assert_eq!(f.provider.requests(), requests);
    assert!(
        paths[1].is_file() && paths[2].is_dir(),
        "reconciliation does not delete existing files"
    );
}

#[tokio::test]
async fn missing_premium_audio_needs_a_new_approved_plan_and_reads_never_spend() {
    let f = Fx::new(true).await;
    f.wait_job(&f.make_all().await).await;
    let old = f.states().await["items"][0]["audio"].clone();
    let ledger = f.ledger().await;
    let requests = f.provider.requests();
    std::fs::remove_file(f.path(&old).await).unwrap();
    assert_eq!(f.request(0, 409).await["code"], "plan_required");
    assert_eq!(
        f.summary().await["chapters_ready"],
        2,
        "invalidation survives the request conflict"
    );
    let preview = f.preview().await;
    assert_eq!(preview["chapters_to_make"], 1);
    assert_eq!(preview["chapters_reused"], 2);
    f.s.get(MANIFEST, &format!("/api/audiobooks/{}/manifest", f.ab), 200)
        .await;
    assert_eq!(f.provider.requests(), requests);
    assert_eq!(f.ledger().await, ledger);
    let plan = f.approve(&preview).await;
    f.wait_job(plan["job_id"].as_str().unwrap()).await;
    assert_eq!(f.summary().await["chapters_ready"], 3);
    assert_eq!(
        f.provider.requests(),
        requests + 1,
        "only the approved repair contacts Gemini"
    );
}

#[tokio::test]
async fn explicit_free_request_requeues_a_done_item_in_the_running_job() {
    let f = Fx::new(false).await;
    f.provider.delay(300);
    let job = f.make_all().await;
    let old = f.wait_first().await;
    std::fs::remove_file(f.path(&old).await).unwrap();
    assert_eq!(f.request(0, 202).await["id"], job);
    f.wait_job(&job).await;
    assert_eq!(f.summary().await["chapters_ready"], 3);
    assert_eq!(f.provider.requests(), 4);
    assert_ne!(f.states().await["items"][0]["audio"]["id"], old["id"]);
}

#[tokio::test]
async fn missing_done_premium_item_does_not_reopen_the_running_paid_job() {
    let f = Fx::new(true).await;
    f.provider.delay(300);
    let job = f.make_all().await;
    let old = f.wait_first().await;
    std::fs::remove_file(f.path(&old).await).unwrap();
    assert_eq!(f.request(0, 409).await["code"], "plan_required");
    f.wait_job(&job).await;
    assert_eq!(f.summary().await["chapters_ready"], 2);
    assert_eq!(
        f.provider.requests(),
        3,
        "the finished paid item is never implicitly charged again"
    );
}
