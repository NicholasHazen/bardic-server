//! M4b/c: plans, estimates, the Allowance and paid audio (against a fake Gemini).
mod common;
use chrono::Duration;
use common::{
    gemini::{money, FakeGemini},
    TestServer, DEVICE,
};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::Duration as StdDuration;

const SRC: &str = "/api/voice-sources/{source_id}";
const PREVIEW: &str = "/api/audiobooks/{audiobook_id}/plan-preview";
const PLAN: &str = "/api/plans/{plan_id}";

struct Fx {
    s: TestServer,
    g: FakeGemini,
    book: String,
    audiobook: String,
    chapters: Vec<String>,
    chars: Vec<i64>,
}

fn cfg(url: String) -> impl FnOnce(&mut bardic_server::config::Config) {
    move |c| {
        c.audio_chunk_chars = 400;
        c.job_retry_ms = 20;
        c.gemini_url = url;
    }
}

impl Fx {
    async fn new() -> Self {
        let g = FakeGemini::start("k").await;
        let s = TestServer::start_with(tempfile::tempdir().unwrap(), cfg(g.url.clone())).await;
        Self::on(s, g).await
    }

    async fn on(s: TestServer, g: FakeGemini) -> Self {
        let l = s.listener("Nick").await;
        s.act_as(&l);
        s.put(
            SRC,
            "/api/voice-sources/gemini",
            json!({ "api_key": "k" }),
            200,
        )
        .await;
        let book = s
            .post("/api/books/sample", "/api/books/sample", json!({}), 201)
            .await["id"]
            .as_str()
            .unwrap()
            .to_string();
        let voice = s
            .get("/api/voices", "/api/voices?source_id=gemini", 200)
            .await["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "Kore")
            .unwrap()["id"]
            .clone();
        let ab = s
            .post(
                "/api/books/{book_id}/audiobooks",
                &format!("/api/books/{book}/audiobooks"),
                json!({ "voice_id": voice }),
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
        let chapters: Vec<String> = ch["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_string())
            .collect();
        let mut chars = vec![];
        for c in &chapters {
            let t = s
                .get(
                    "/api/books/{book_id}/chapters/{chapter_id}/text",
                    &format!("/api/books/{book}/chapters/{c}/text"),
                    200,
                )
                .await;
            chars.push(
                t["lines"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|l| l["end"].as_i64().unwrap() - l["start"].as_i64().unwrap())
                    .sum(),
            );
        }
        Fx {
            s,
            g,
            book,
            audiobook,
            chapters,
            chars,
        }
    }

    async fn preview(&self, scope: Value, expect: u16) -> Value {
        self.s
            .post(
                PREVIEW,
                &format!("/api/audiobooks/{}/plan-preview", self.audiobook),
                json!({ "scope": scope }),
                expect,
            )
            .await
    }

    async fn whole(&self) -> Value {
        self.preview(json!({ "kind": "whole_book" }), 200).await
    }

    async fn approve(&self, est: &Value, limit: i64, expect: u16) -> Value {
        self.s
            .post(
                "/api/plans",
                "/api/plans",
                json!({ "estimate_id": est["estimate_id"], "limit": money(limit) }),
                expect,
            )
            .await
    }

    async fn plan(&self, id: &str) -> Value {
        self.s.get(PLAN, &format!("/api/plans/{id}"), 200).await
    }

    async fn wait(&self, id: &str, states: &[&str]) -> Value {
        for _ in 0..400 {
            let p = self.plan(id).await;
            if states.contains(&p["state"].as_str().unwrap()) {
                return p;
            }
            tokio::time::sleep(StdDuration::from_millis(25)).await;
        }
        panic!(
            "plan {id} did not reach {states:?}: {}",
            self.plan(id).await
        );
    }

    async fn act(&self, id: &str, what: &str, body: Value, expect: u16) -> Value {
        self.s
            .call(
                Method::POST,
                "/api/plans/{plan_id}/pause",
                &format!("/api/plans/{id}/{what}"),
                Some(DEVICE),
                Some(body),
                expect,
            )
            .await
            .clone()
    }

    async fn allowance(&self) -> Value {
        self.s.get("/api/allowance", "/api/allowance", 200).await
    }

    async fn ready(&self) -> usize {
        self.s
            .get(
                "/api/audiobooks/{audiobook_id}",
                &format!("/api/audiobooks/{}", self.audiobook),
                200,
            )
            .await["chapters_ready"]
            .as_u64()
            .unwrap() as usize
    }
}

fn micros(v: &Value) -> i64 {
    v["micros"].as_i64().unwrap()
}

#[tokio::test]
async fn a_preview_is_a_dated_range_and_spends_nothing() {
    let f = Fx::new().await;
    let est = f.whole().await;
    assert_eq!(est["chapters_to_make"], 3);
    assert_eq!(est["chapters_reused"], 0);
    assert_eq!(est["text_characters"], f.chars.iter().sum::<i64>());
    let (low, likely, high) = (
        micros(&est["cost"]["low"]),
        micros(&est["cost"]["likely"]),
        micros(&est["cost"]["high"]),
    );
    assert!(0 < low && low < likely && likely < high);
    assert_eq!(
        likely,
        (est["text_characters"].as_i64().unwrap() * 25_000_000 + 999_999) / 1_000_000
    );
    assert_eq!(est["cost"]["basis"], "manual");
    assert_eq!(est["cost"]["prices_as_of"], "2026-09-30T00:00:00.000Z");
    assert!(micros(&est["suggested_limit"]) >= high);
    assert_eq!(est["expires_at"], "2026-01-15T12:15:00.000Z");
    assert_eq!(
        est["allowance"],
        json!({ "monthly_limit": null, "remaining": null })
    );
    assert_eq!(est["blocked"], Value::Null);
    assert_eq!(est["scope"]["kind"], "whole_book");
    assert_eq!(
        f.g.received(),
        0,
        "a preview never contacts the provider's speech endpoint"
    );
    assert_eq!(
        f.allowance().await["spent"],
        json!({ "known": money(0), "unknown_items": 0 })
    );
    assert_eq!(
        f.preview(
            json!({ "kind": "from_chapter", "from_chapter_id": f.chapters[2] }),
            200
        )
        .await["chapters_to_make"],
        1
    );
    f.preview(json!({ "kind": "sideways" }), 400).await;
    f.s.stop().await;
}

#[tokio::test]
async fn an_approved_plan_makes_the_audio_and_records_what_it_cost() {
    let f = Fx::new().await;
    let est = f.whole().await;
    let limit = micros(&est["suggested_limit"]);
    let plan = f.approve(&est, limit, 201).await;
    assert_eq!(plan["state"], "running");
    assert_eq!(plan["limit"], money(limit));
    assert_eq!(plan["approved_by"]["listener_name"], "Nick");
    assert_eq!(plan["approved_by"]["device_id"], DEVICE);
    assert_eq!(plan["estimate"]["likely"], est["cost"]["likely"]);
    assert_eq!(plan["chapters_total"], 3);
    let id = plan["id"].as_str().unwrap();

    let done = f.wait(id, &["completed"]).await;
    assert_eq!(done["chapters_done"], 3);
    assert_eq!(f.ready().await, 3);
    let spent = micros(&done["spent"]["known"]);
    assert!(spent > 0 && spent <= limit, "spent {spent} within {limit}");
    assert_eq!(done["spent"]["unknown_items"], 0);
    // every request is a whole number of lines of at most the chunk size, and the money adds up from the tokens
    let spoken = f.g.spoken();
    assert!(
        spoken.len() >= 3
            && spoken
                .iter()
                .all(|t| t.chars().count() <= 400 || !t.contains("\n\n"))
    );
    let expected: i64 = spoken
        .iter()
        .map(|t| {
            let c = t.chars().count() as i64;
            ((c + 3) / 4 * 500_000 + 999_999) / 1_000_000
                + (c * 2 * 9_000_000 + 999_999) / 1_000_000
        })
        .sum();
    assert_eq!(spent, expected);
    let a = f.allowance().await;
    assert_eq!(a["spent"]["known"], done["spent"]["known"]);

    // the audio is real and timed
    let st =
        f.s.get(
            "/api/audiobooks/{audiobook_id}/chapters",
            &format!("/api/audiobooks/{}/chapters", f.audiobook),
            200,
        )
        .await;
    assert!(st["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["state"] == "ready"));

    // plans are listed and audited
    let list =
        f.s.get(
            "/api/plans",
            &format!("/api/plans?audiobook_id={}&state=completed", f.audiobook),
            200,
        )
        .await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        f.s.get("/api/plans", "/api/plans?state=running", 200).await["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    f.s.get("/api/plans", "/api/plans?state=weird", 400).await;
    let audit =
        f.s.get("/api/audit", "/api/audit?action=plan.approved", 200)
            .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1);
    f.s.get(PLAN, "/api/plans/nope", 404).await;
    f.s.stop().await;
}

#[tokio::test]
async fn an_approval_must_match_what_was_shown() {
    let f = Fx::new().await;
    let est = f.whole().await;
    let likely = micros(&est["cost"]["likely"]);
    let high = micros(&est["cost"]["high"]);

    let e = f.approve(&est, likely - 1, 409).await;
    assert_eq!(e["code"], "limit_below_estimate");
    f.s.post(
        "/api/plans",
        "/api/plans",
        json!({ "estimate_id": "nope", "limit": money(high) }),
        404,
    )
    .await;
    f.s.post("/api/plans", "/api/plans", json!({ "estimate_id": est["estimate_id"], "limit": { "micros": high, "currency": "EUR" } }), 400).await;

    // prices moved outside the range since the preview
    f.s.put(
        "/api/prices/{provider}",
        "/api/prices/gemini",
        json!({ "unit": "million_characters", "per_unit": money(40_000_000) }),
        200,
    )
    .await;
    let e = f.approve(&est, high, 409).await;
    assert_eq!(e["code"], "estimate_changed");
    f.s.put(
        "/api/prices/{provider}",
        "/api/prices/gemini",
        json!({ "unit": "million_characters", "per_unit": money(25_000_000) }),
        200,
    )
    .await;

    // approving once consumes the estimate
    let plan = f.approve(&est, high, 201).await;
    let e = f.approve(&est, high, 409).await;
    assert_eq!(e["code"], "estimate_used");
    f.act(plan["id"].as_str().unwrap(), "stop", json!({}), 200)
        .await;

    // estimates expire after 15 minutes
    let est2 = f.whole().await;
    f.s.clock.advance(Duration::minutes(16));
    let e = f.approve(&est2, high, 409).await;
    assert_eq!(e["code"], "estimate_expired");
    f.s.stop().await;
}

#[tokio::test]
async fn the_monthly_allowance_blocks_estimates_and_approvals() {
    let f = Fx::new().await;
    f.s.put(
        "/api/allowance",
        "/api/allowance",
        json!({ "monthly_limit": money(10_000), "default_plan_limit": money(1_000_000) }),
        200,
    )
    .await;
    let est = f.whole().await;
    assert_eq!(est["allowance"]["remaining"], money(10_000));
    assert_eq!(est["blocked"]["code"], "allowance_exceeded");
    let e = f.approve(&est, micros(&est["suggested_limit"]), 409).await;
    assert_eq!(e["code"], "allowance_exceeded");
    assert_eq!(f.g.received(), 0);
    f.s.stop().await;
}

#[tokio::test]
async fn only_one_plan_runs_per_audiobook_and_the_error_says_which() {
    let f = Fx::new().await;
    f.g.state.lock().unwrap().delay_ms = 200;
    let est = f.whole().await;
    let plan = f.approve(&est, micros(&est["suggested_limit"]), 201).await;
    let est2 = f.whole().await;
    let e = f
        .approve(&est2, micros(&est2["suggested_limit"]), 409)
        .await;
    assert_eq!(e["code"], "plan_active");
    assert_eq!(e["context"]["plan_id"], plan["id"]);
    f.act(plan["id"].as_str().unwrap(), "stop", json!({}), 200)
        .await;
    f.s.stop().await;
}

#[tokio::test]
async fn a_limit_stops_the_plan_before_a_request_that_could_pass_it_and_raising_it_continues() {
    let f = Fx::new().await;
    let est = f.whole().await;
    let likely = micros(&est["cost"]["likely"]);
    let plan = f.approve(&est, likely, 201).await;
    let id = plan["id"].as_str().unwrap().to_string();
    let stuck = f.wait(&id, &["needs_you"]).await;
    assert_eq!(stuck["needs_you"]["code"], "limit_exceeded");
    assert!(
        stuck["chapters_done"].as_i64().unwrap() < 3,
        "the last chapter could have passed the limit"
    );
    let done_before = stuck["chapters_done"].as_i64().unwrap();
    assert!(done_before >= 1 && micros(&stuck["spent"]["known"]) <= likely);
    assert_eq!(
        f.ready().await as i64,
        done_before,
        "what was finished is kept"
    );

    // resuming inside the same limit cannot help
    let e = f.act(&id, "resume", json!({}), 200).await;
    assert_eq!(e["state"], "running");
    let again = f.wait(&id, &["needs_you"]).await;
    assert_eq!(again["needs_you"]["code"], "limit_exceeded");

    // raising it is an approval, and is audited
    let raised = micros(&est["cost"]["high"]) * 2;
    f.act(&id, "resume", json!({ "new_limit": money(raised) }), 200)
        .await;
    let done = f.wait(&id, &["completed"]).await;
    assert_eq!(done["limit"], money(raised));
    assert_eq!(f.ready().await, 3);
    assert_eq!(
        f.s.get("/api/audit", "/api/audit?action=plan.limit_raised", 200)
            .await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    f.s.stop().await;
}

#[tokio::test]
async fn a_provider_quota_makes_the_plan_wait_and_it_resumes_by_itself() {
    let f = Fx::new().await;
    f.g.state.lock().unwrap().quota = vec![30];
    let est = f.whole().await;
    let plan = f.approve(&est, micros(&est["suggested_limit"]), 201).await;
    let id = plan["id"].as_str().unwrap().to_string();
    let waiting = f.wait(&id, &["waiting"]).await;
    assert_eq!(waiting["waiting"]["code"], "waiting_quota");
    assert_eq!(waiting["waiting"]["until"], "2026-01-15T12:00:30.000Z");
    assert_eq!(
        waiting["spent"],
        json!({ "known": money(0), "unknown_items": 0 }),
        "a refused request costs nothing"
    );
    let j =
        f.s.get(
            "/api/jobs/{job_id}",
            &format!("/api/jobs/{}", waiting["job_id"].as_str().unwrap()),
            200,
        )
        .await;
    assert_eq!(j["state"], "waiting");
    f.act(&id, "resume", json!({}), 409).await;

    f.s.clock.advance(Duration::seconds(31));
    let done = f.wait(&id, &["completed"]).await;
    assert_eq!(done["limit"], plan["limit"], "no new approval, same limit");
    assert_eq!(f.ready().await, 3);
    f.s.stop().await;
}

#[tokio::test]
async fn cost_that_cannot_be_stated_is_counted_unknown_never_zero() {
    let f = Fx::new().await;
    f.g.state.lock().unwrap().no_usage = 1;
    let est = f.whole().await;
    let plan = f.approve(&est, micros(&est["suggested_limit"]), 201).await;
    let done = f.wait(plan["id"].as_str().unwrap(), &["completed"]).await;
    assert_eq!(done["spent"]["unknown_items"], 1);
    assert!(micros(&done["spent"]["known"]) > 0);
    assert_eq!(f.allowance().await["spent"]["unknown_items"], 1);
    f.s.stop().await;
}

#[tokio::test]
async fn an_answer_without_audio_is_counted_as_spent_and_fails_that_chapter() {
    let f = Fx::new().await;
    f.g.state.lock().unwrap().no_audio = 1;
    let est = f
        .preview(
            json!({ "kind": "chapters", "chapter_ids": [f.chapters[0]] }),
            200,
        )
        .await;
    let plan = f.approve(&est, micros(&est["suggested_limit"]), 201).await;
    let id = plan["id"].as_str().unwrap().to_string();
    let stuck = f.wait(&id, &["needs_you"]).await;
    assert_eq!(stuck["needs_you"]["code"], "provider_refused");
    assert!(
        micros(&stuck["spent"]["known"]) > 0,
        "the request was billed"
    );
    assert_eq!(f.ready().await, 0);
    let st =
        f.s.get(
            "/api/audiobooks/{audiobook_id}/chapters",
            &format!("/api/audiobooks/{}/chapters", f.audiobook),
            200,
        )
        .await;
    assert_eq!(st["items"][0]["state"], "not_yet");
    // a retry is a new request with new spending, and the listener chose it
    let before = micros(&stuck["spent"]["known"]);
    f.act(&id, "resume", json!({}), 200).await;
    let done = f.wait(&id, &["completed"]).await;
    assert!(micros(&done["spent"]["known"]) > before);
    f.s.stop().await;
}

#[tokio::test]
async fn pausing_finishes_the_chapter_in_hand_and_stopping_keeps_it() {
    let f = Fx::new().await;
    f.g.state.lock().unwrap().delay_ms = 150;
    let est = f.whole().await;
    let plan = f.approve(&est, micros(&est["suggested_limit"]), 201).await;
    let id = plan["id"].as_str().unwrap().to_string();
    tokio::time::sleep(StdDuration::from_millis(100)).await;
    let paused = f.act(&id, "pause", json!({}), 200).await;
    assert_eq!(paused["state"], "paused");
    f.act(&id, "pause", json!({}), 409).await;
    tokio::time::sleep(StdDuration::from_millis(1500)).await;
    let now = f.plan(&id).await;
    assert_eq!(now["state"], "paused");
    let n = f.g.received();
    tokio::time::sleep(StdDuration::from_millis(400)).await;
    assert_eq!(f.g.received(), n, "nothing is sent while paused");
    let ready = f.ready().await;
    assert!(
        (1..3).contains(&ready),
        "the chapter in hand was finished and kept: {ready}"
    );

    f.g.state.lock().unwrap().delay_ms = 0;
    f.act(&id, "resume", json!({}), 200).await;
    f.wait(&id, &["completed"]).await;
    assert_eq!(f.ready().await, 3);
    f.act(&id, "resume", json!({}), 409).await;
    // stop is idempotent and does nothing to a finished plan
    assert_eq!(
        f.act(&id, "stop", json!({}), 200).await["state"],
        "completed"
    );
    f.s.stop().await;
}

#[tokio::test]
async fn stopping_a_plan_keeps_what_is_finished() {
    let f = Fx::new().await;
    f.g.state.lock().unwrap().delay_ms = 100;
    let est = f.whole().await;
    let plan = f.approve(&est, micros(&est["suggested_limit"]), 201).await;
    let id = plan["id"].as_str().unwrap().to_string();
    tokio::time::sleep(StdDuration::from_millis(50)).await;
    assert_eq!(f.act(&id, "stop", json!({}), 200).await["state"], "stopped");
    assert_eq!(f.act(&id, "stop", json!({}), 200).await["state"], "stopped");
    tokio::time::sleep(StdDuration::from_millis(1500)).await;
    let n = f.g.received();
    tokio::time::sleep(StdDuration::from_millis(400)).await;
    assert_eq!(f.g.received(), n, "no further requests");
    f.act(&id, "resume", json!({}), 409).await;
    // it may have finished the chapter it was in; nothing is lost either way
    let st = f.plan(&id).await;
    assert_eq!(st["state"], "stopped");
    assert!(f.ready().await <= 3);
    f.s.stop().await;
}

#[tokio::test]
async fn pressing_play_on_premium_audio_works_only_inside_a_running_plan() {
    let f = Fx::new().await;
    let req = |ch: usize| {
        format!(
            "/api/audiobooks/{}/chapters/{}/request",
            f.audiobook, f.chapters[ch]
        )
    };
    let tpl = "/api/audiobooks/{audiobook_id}/chapters/{chapter_id}/request";
    let e =
        f.s.call(Method::POST, tpl, &req(0), Some(DEVICE), None, 409)
            .await;
    assert_eq!(e["code"], "plan_required");

    f.g.state.lock().unwrap().delay_ms = 100;
    let est = f
        .preview(
            json!({ "kind": "from_chapter", "from_chapter_id": f.chapters[1] }),
            200,
        )
        .await;
    let plan = f.approve(&est, micros(&est["suggested_limit"]), 201).await;
    let e =
        f.s.call(Method::POST, tpl, &req(0), Some(DEVICE), None, 409)
            .await;
    assert_eq!(e["code"], "plan_required", "chapter 0 is not in the plan");
    let job =
        f.s.call(Method::POST, tpl, &req(2), Some(DEVICE), None, 202)
            .await;
    assert_eq!(job["plan_id"], plan["id"]);
    f.wait(plan["id"].as_str().unwrap(), &["completed"]).await;
    assert_eq!(
        f.s.call(Method::POST, tpl, &req(1), Some(DEVICE), None, 200)
            .await["state"],
        "ready"
    );
    f.s.stop().await;
}

#[tokio::test]
async fn a_rejected_key_stops_the_plan_without_spending() {
    let f = Fx::new().await;
    f.g.state.lock().unwrap().key = "rotated".into();
    let est = f.whole().await;
    let plan = f.approve(&est, micros(&est["suggested_limit"]), 201).await;
    let id = plan["id"].as_str().unwrap().to_string();
    let stuck = f.wait(&id, &["needs_you"]).await;
    assert_eq!(stuck["needs_you"]["code"], "key_rejected");
    assert_eq!(
        stuck["spent"],
        json!({ "known": money(0), "unknown_items": 0 })
    );
    f.g.state.lock().unwrap().key = "k".into();
    f.act(&id, "resume", json!({}), 200).await;
    f.wait(&id, &["completed"]).await;
    f.s.stop().await;
}

#[tokio::test]
async fn a_premium_sample_is_a_counted_request_made_once_per_revision() {
    let f = Fx::new().await;
    let voice =
        f.s.get("/api/voices", "/api/voices?source_id=gemini", 200)
            .await["items"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
    let path = format!("/api/voices/{voice}/sample");
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
    let spent = micros(&f.allowance().await["spent"]["known"]);
    assert!(spent > 0, "a premium sample is counted");
    assert_eq!(f.g.received(), 1);
    f.s.raw(
        "/api/voices/{voice_id}/sample",
        &path,
        &[("x-bardic-device", DEVICE)],
        200,
    )
    .await;
    assert_eq!(f.g.received(), 1, "the repeat is free");
    assert_eq!(micros(&f.allowance().await["spent"]["known"]), spent);

    // a spent Allowance refuses a new premium sample
    let other =
        f.s.get("/api/voices", "/api/voices?source_id=gemini", 200)
            .await["items"][1]["id"]
            .as_str()
            .unwrap()
            .to_string();
    f.s.put(
        "/api/allowance",
        "/api/allowance",
        json!({ "monthly_limit": money(1), "default_plan_limit": money(1_000_000) }),
        200,
    )
    .await;
    let e =
        f.s.raw(
            "/api/voices/{voice_id}/sample",
            &format!("/api/voices/{other}/sample"),
            &[("x-bardic-device", DEVICE)],
            409,
        )
        .await;
    assert!(String::from_utf8_lossy(&e.1).contains("allowance_exceeded"));
    f.s.stop().await;
}

#[tokio::test]
async fn a_request_in_flight_when_the_server_stops_becomes_unknown_spend_and_the_plan_carries_on() {
    let dir = tempfile::tempdir().unwrap();
    let g = FakeGemini::start("k").await;
    let s = TestServer::start_with(dir, cfg(g.url.clone())).await;
    let f = Fx::on(s, g).await;
    f.g.state.lock().unwrap().delay_ms = 400;
    let est = f.whole().await;
    let plan = f.approve(&est, micros(&est["suggested_limit"]), 201).await;
    let id = plan["id"].as_str().unwrap().to_string();
    tokio::time::sleep(StdDuration::from_millis(150)).await;
    let (g, book, audiobook, chapters, chars) = (f.g, f.book, f.audiobook, f.chapters, f.chars);
    let dir = f.s.stop().await;

    let s = TestServer::start_with(dir, cfg(g.url.clone())).await;
    let l = s.listener("Sam").await;
    s.act_as(&l);
    g.state.lock().unwrap().delay_ms = 0;
    let f = Fx {
        s,
        g,
        book,
        audiobook,
        chapters,
        chars,
    };
    let done = f.wait(&id, &["completed"]).await;
    assert_eq!(
        done["spent"]["unknown_items"], 1,
        "the request that may have been sent is unknown, not zero"
    );
    assert_eq!(f.ready().await, 3);
    assert_eq!(f.allowance().await["spent"]["unknown_items"], 1);
    f.s.stop().await;
}

/// A provider failing in ways other than a quota: what it costs and what happens next.
async fn one_chapter_plan(f: &Fx) -> String {
    let est = f
        .preview(
            json!({ "kind": "chapters", "chapter_ids": [f.chapters[0]] }),
            200,
        )
        .await;
    let plan = f.approve(&est, micros(&est["suggested_limit"]), 201).await;
    plan["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_content_refusal_costs_nothing_and_fails_only_that_chapter() {
    let f = Fx::new().await;
    f.g.state.lock().unwrap().statuses = vec![400];
    let id = one_chapter_plan(&f).await;
    let stuck = f.wait(&id, &["needs_you"]).await;
    assert_eq!(stuck["needs_you"]["code"], "provider_refused", "{stuck}");
    assert_eq!(micros(&stuck["spent"]["known"]), 0);
    assert_eq!(stuck["spent"]["unknown_items"], 0, "a 400 is not billed");
    assert_eq!(f.ready().await, 0);
    assert!(
        !stuck.to_string().contains("refused by the fake"),
        "the provider's own text is never passed on"
    );
    f.act(&id, "resume", json!({}), 200).await;
    let done = f.wait(&id, &["completed"]).await;
    assert_eq!(done["spent"]["unknown_items"], 0);
    assert_eq!(f.ready().await, 1);
    f.s.stop().await;
}

#[tokio::test]
async fn provider_server_errors_and_garbage_are_unknown_spend_and_never_retried_by_themselves() {
    for fault in ["500", "503", "garbage", "bad_audio"] {
        let f = Fx::new().await;
        {
            let mut g = f.g.state.lock().unwrap();
            match fault {
                "500" => g.statuses = vec![500],
                "503" => g.statuses = vec![503],
                "garbage" => g.garbage = 1,
                _ => g.bad_audio = 1,
            }
        }
        let id = one_chapter_plan(&f).await;
        let stuck = f.wait(&id, &["needs_you"]).await;
        assert_eq!(stuck["spent"]["unknown_items"], 1, "{fault}: {stuck}");
        assert_eq!(micros(&stuck["spent"]["known"]), 0, "{fault}");
        // it was sent once: no hidden second attempt that would spend again
        tokio::time::sleep(StdDuration::from_millis(150)).await;
        assert_eq!(f.g.state.lock().unwrap().received, 1, "{fault}");
        assert_eq!(f.ready().await, 0, "{fault}");
        assert_eq!(f.allowance().await["spent"]["unknown_items"], 1, "{fault}");
        // the listener may choose to try again
        f.act(&id, "resume", json!({}), 200).await;
        let done = f.wait(&id, &["completed"]).await;
        assert_eq!(done["spent"]["unknown_items"], 1, "{fault}");
        assert_eq!(f.ready().await, 1, "{fault}");
        f.s.stop().await;
    }
}

#[tokio::test]
async fn stopping_a_plan_halts_the_chapter_at_the_next_request_not_the_end_of_it() {
    let g = FakeGemini::start("k").await;
    // small requests, so every chapter is many of them
    let s = TestServer::start_with(tempfile::tempdir().unwrap(), |c| {
        c.audio_chunk_chars = 40;
        c.job_retry_ms = 20;
        c.gemini_url = g.url.clone();
    })
    .await;
    let f = Fx::on(s, g).await;
    f.g.state.lock().unwrap().delay_ms = 80;
    let id = one_chapter_plan(&f).await;
    for _ in 0..200 {
        if f.g.received() >= 2 {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(10)).await;
    }
    let at_stop = f.g.received();
    assert!(at_stop >= 2);
    f.act(&id, "stop", json!({}), 200).await;
    tokio::time::sleep(StdDuration::from_millis(1500)).await;
    let total_requests = f.g.received();
    assert!(
        total_requests <= at_stop + 1,
        "{total_requests} requests after a stop at {at_stop}: only the one in flight may finish"
    );
    let chapter_chars = f.chars[0];
    assert!(
        (total_requests as i64) * 40 < chapter_chars,
        "the whole chapter was made after the stop"
    );
    // what was paid for is a known cost, and nothing is left reserved
    let st = f.plan(&id).await;
    assert_eq!(st["state"], "stopped");
    assert_eq!(st["spent"]["unknown_items"], 0);
    f.s.stop().await;
}

#[tokio::test]
async fn an_unknown_cost_still_uses_up_the_limit_so_resuming_cannot_spend_it_twice() {
    let f = Fx::new().await;
    f.g.state.lock().unwrap().statuses = vec![500];
    let est = f
        .preview(
            json!({ "kind": "chapters", "chapter_ids": [f.chapters[0]] }),
            200,
        )
        .await;
    // room for the chapter once, not twice
    let limit = micros(&est["cost"]["high"]) * 12 / 10;
    let plan = f.approve(&est, limit, 201).await;
    let id = plan["id"].as_str().unwrap().to_string();
    let stuck = f.wait(&id, &["needs_you"]).await;
    assert_eq!(stuck["spent"]["unknown_items"], 1, "{stuck}");
    let sent = f.g.received();
    assert_eq!(sent, 1, "the first request must have been sent");
    f.act(&id, "resume", json!({}), 200).await;
    let again = f.wait(&id, &["needs_you"]).await;
    assert_eq!(
        f.g.received(),
        sent,
        "a request that may already have been billed must not make room to send it again: {again}"
    );
    assert_eq!(again["needs_you"]["code"], "limit_exceeded", "{again}");
    f.s.stop().await;
}

#[tokio::test]
async fn a_plan_cannot_be_resumed_into_a_book_that_is_about_to_be_deleted() {
    let f = Fx::new().await;
    f.g.state.lock().unwrap().statuses = vec![500];
    let id = one_chapter_plan(&f).await;
    f.wait(&id, &["needs_you"]).await;
    let dp = format!("/api/books/{}/deletion", f.book);
    f.s.post("/api/books/{book_id}/deletion", &dp, json!({}), 202)
        .await;
    let sent = f.g.received();
    let e = f.act(&id, "resume", json!({}), 409).await;
    assert_eq!(e["code"], "deletion_pending", "{e}");
    assert_eq!(f.g.received(), sent);
    // after the undo it can go on
    f.s.delete("/api/books/{book_id}/deletion", &dp, 204).await;
    f.act(&id, "resume", json!({}), 200).await;
    f.wait(&id, &["completed"]).await;
    f.s.stop().await;
}

/// A seeded soak: a whole-book plan with small requests, random provider faults and restarts
/// at random moments. Whatever happens, the ledger accounts for every request, nothing is left
/// reserved, spending stays inside the limit, and every chapter marked ready has its file.
#[tokio::test]
async fn a_soak_of_faults_and_restarts_keeps_the_ledger_and_the_audio_honest() {
    let (mut all_restarts, mut all_unknown) = (0u64, 0i64);
    for seed in 1..=4u64 {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let mut dir = tempfile::tempdir().unwrap();
        let g = FakeGemini::start("k").await;
        let mk = |g: &FakeGemini| {
            let url = g.url.clone();
            move |c: &mut bardic_server::config::Config| {
                c.audio_chunk_chars = 120;
                c.job_retry_ms = 10;
                c.gemini_url = url;
            }
        };
        let s = TestServer::start_with(dir, mk(&g)).await;
        let mut f = Fx::on(s, g).await;
        f.g.state.lock().unwrap().delay_ms = 20;
        // two 5xx or garbage faults, one content refusal, one answer without audio, one 429
        {
            let mut st = f.g.state.lock().unwrap();
            for _ in 0..2 {
                match next(3) {
                    0 => st.statuses.push(500),
                    1 => st.garbage += 1,
                    _ => st.bad_audio += 1,
                }
            }
            st.statuses.push(400);
            st.no_audio = 1;
            st.quota = vec![1];
        }
        let est = f.whole().await;
        let limit = micros(&est["cost"]["high"]) * 3;
        let plan = f.approve(&est, limit, 201).await;
        let id = plan["id"].as_str().unwrap().to_string();

        let mut restarts = 0u64;
        for round in 0..40 {
            tokio::time::sleep(StdDuration::from_millis(60 + next(200))).await;
            let p = f.plan(&id).await;
            match p["state"].as_str().unwrap() {
                "completed" => break,
                "needs_you" | "paused" => {
                    f.act(&id, "resume", json!({}), 200).await;
                }
                // the quota's wait is on the fake clock
                "waiting" => f.s.clock.advance(Duration::seconds(2)),
                _ => {}
            }
            if round % 3 == 1 && next(2) == 0 && restarts < 3 {
                restarts += 1;
                let (g, book, audiobook, chapters, chars) =
                    (f.g, f.book, f.audiobook, f.chapters, f.chars);
                dir = f.s.stop().await;
                let s = TestServer::start_with(dir, mk(&g)).await;
                let l = s.listener(&format!("Soak{restarts}")).await;
                s.act_as(&l);
                f = Fx {
                    s,
                    g,
                    book,
                    audiobook,
                    chapters,
                    chars,
                };
            }
        }
        let done = f.wait(&id, &["completed"]).await;
        assert_eq!(f.ready().await, 3, "seed {seed}: {done}");
        all_restarts += restarts;
        all_unknown += done["spent"]["unknown_items"].as_i64().unwrap();
        let known = micros(&done["spent"]["known"]);
        assert!(known > 0 && known <= limit, "seed {seed}: {done}");
        let received = f.g.received();
        let dir = f.s.stop().await;

        let db = rusqlite::Connection::open(dir.path().join("bardic.db")).unwrap();
        let rows: i64 = db
            .query_row("SELECT COUNT(*) FROM spend", [], |r| r.get(0))
            .unwrap();
        let reserved: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM spend WHERE status='reserved'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reserved, 0, "seed {seed}: a request is still reserved");
        // a request is reserved before it is sent, so no request goes unrecorded; a restart
        // can leave a reservation that never reached the provider, never the other way round
        assert!(
            rows >= received as i64 && rows <= received as i64 + restarts as i64,
            "seed {seed}: {rows} ledger rows for {received} requests and {restarts} restarts"
        );
        let held: i64 = db
            .query_row(
                "SELECT COALESCE(SUM(CASE WHEN status='known' THEN known_micros WHEN status='unknown' THEN reserved END),0) FROM spend",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            held <= limit,
            "seed {seed}: {held} held against a limit of {limit}"
        );
        let mut q = db
            .prepare("SELECT path, bytes FROM audio WHERE deleted_at IS NULL")
            .unwrap();
        let files: Vec<(String, i64)> = q
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(files.len(), 3, "seed {seed}");
        for (path, bytes) in files {
            let len = std::fs::metadata(dir.path().join(&path))
                .unwrap_or_else(|_| panic!("seed {seed}: ready chapter has no file {path}"))
                .len();
            assert_eq!(len as i64, bytes, "seed {seed}: {path}");
        }
        let stray: Vec<_> = walk(dir.path())
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "part"))
            .collect();
        assert!(
            stray.is_empty(),
            "seed {seed}: leftover partial files {stray:?}"
        );
    }
    eprintln!("soak: {all_restarts} restarts, {all_unknown} unknown items over 4 seeds");
    assert!(
        all_restarts > 0 && all_unknown > 0,
        "the soak did not exercise faults"
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[tokio::test]
async fn estimates_double_when_google_doubles_its_rates_unless_the_owner_set_the_price_after() {
    let f = Fx::new().await;
    let before = micros(&f.whole().await["cost"]["likely"]);
    // 2026-01-15 plus 352 days is 2027-01-02
    f.s.clock.advance(Duration::days(352));
    let after = f.whole().await;
    assert_eq!(micros(&after["cost"]["likely"]), before * 2, "{after}");
    // an approval of an estimate made before the change would be refused as changed; the new one works
    let space =
        f.s.get(
            "/api/audiobooks/{audiobook_id}/space",
            &format!("/api/audiobooks/{}/space", f.audiobook),
            200,
        )
        .await;
    assert!(
        space["remake_estimate"].is_null() || space["remake_estimate"]["likely"]["micros"].is_i64()
    );
    // the owner sets the price after the change: taken as meant, not doubled again
    f.s.put(
        "/api/prices/{provider}",
        "/api/prices/gemini",
        json!({ "unit": "million_characters", "per_unit": money(25_000_000) }),
        200,
    )
    .await;
    let set = f.whole().await;
    assert_eq!(micros(&set["cost"]["likely"]), before, "{set}");
    f.s.stop().await;
}

#[tokio::test]
async fn a_plan_running_after_the_change_holds_back_the_doubled_amount() {
    let f = Fx::new().await;
    f.s.clock.advance(Duration::days(352));
    f.g.state.lock().unwrap().statuses = vec![500];
    let est = f
        .preview(
            json!({ "kind": "chapters", "chapter_ids": [f.chapters[0]] }),
            200,
        )
        .await;
    let id = f.approve(&est, micros(&est["suggested_limit"]), 201).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let stuck = f.wait(&id, &["needs_you"]).await;
    assert_eq!(stuck["spent"]["unknown_items"], 1);
    // the unknown request was held at its high estimate at the new rate: more than the old rate
    let db_dir = f.s.dir.path().join("bardic.db");
    let db = rusqlite::Connection::open(db_dir).unwrap();
    let held: i64 = db
        .query_row(
            "SELECT reserved FROM spend WHERE status='unknown'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let old_rate_chunk_high = plans_high(f.chars[0].min(400), 25_000_000);
    assert!(
        held > old_rate_chunk_high,
        "held {held} should exceed {old_rate_chunk_high}, the same request at the old rate"
    );
    f.s.stop().await;
}

fn plans_high(chars: i64, per_million: i64) -> i64 {
    let likely = (chars * per_million + 999_999) / 1_000_000;
    (likely * 140 + 99) / 100
}
