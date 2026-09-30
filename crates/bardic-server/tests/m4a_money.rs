//! M4a: the Gemini source, prices and the Allowance (against a fake Gemini).
mod common;
use common::{
    gemini::{money, FakeGemini},
    TestServer,
};
use serde_json::{json, Value};

const SRC: &str = "/api/voice-sources/{source_id}";

async fn start(g: &FakeGemini) -> TestServer {
    let url = g.url.clone();
    let s = TestServer::start_with(tempfile::tempdir().unwrap(), move |c| c.gemini_url = url).await;
    let l = s.listener("Nick").await;
    s.act_as(&l);
    s
}

#[tokio::test]
async fn gemini_offers_its_thirty_voices_once_the_key_works_and_never_shows_the_key() {
    let g = FakeGemini::start("good-key").await;
    let s = start(&g).await;

    let e = s
        .put(SRC, "/api/voice-sources/gemini", json!({}), 400)
        .await;
    assert_eq!(e["code"], "invalid_request");
    let e = s
        .put(
            SRC,
            "/api/voice-sources/gemini",
            json!({ "api_key": "bad-key" }),
            400,
        )
        .await;
    assert_eq!(e["code"], "key_rejected");
    assert!(!e.to_string().contains("bad-key"));
    assert_eq!(
        s.get(SRC, "/api/voice-sources/gemini", 200).await["state"],
        "not_set_up"
    );

    let src = s
        .put(
            SRC,
            "/api/voice-sources/gemini",
            json!({ "api_key": "good-key" }),
            200,
        )
        .await;
    assert_eq!(
        (
            src["state"].as_str().unwrap(),
            src["tier"].as_str().unwrap()
        ),
        ("connected", "premium")
    );
    assert_eq!(src["voice_count"], 30);
    assert_eq!(src["has_key"], true);
    assert_eq!(src["base_url"], Value::Null);
    let voices = s.get("/api/voices", "/api/voices?tier=premium", 200).await;
    let items = voices["items"].as_array().unwrap();
    assert_eq!(items.len(), 30);
    let kore = items.iter().find(|v| v["name"] == "Kore").unwrap();
    assert_eq!(kore["description"], "Firm");
    assert_eq!(kore["source_id"], "gemini");
    assert_eq!(
        s.get("/api/voices", "/api/voices?tier=free", 200).await["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // omitting the key keeps it; the key is in no response and not in the audit log
    s.put(SRC, "/api/voice-sources/gemini", json!({}), 200)
        .await;
    for body in [
        s.get("/api/voice-sources", "/api/voice-sources", 200).await,
        s.get("/api/audit", "/api/audit?limit=50", 200).await,
    ] {
        assert!(!body.to_string().contains("good-key"));
    }

    // the key is checked again on a refresh, and a revoked key is reported
    g.state.lock().unwrap().key = "rotated".into();
    let t = s
        .post(
            "/api/voice-sources/{source_id}/test",
            "/api/voice-sources/gemini/test",
            json!({}),
            200,
        )
        .await;
    assert_eq!(t["state"], "key_rejected");
    assert_eq!(
        s.get("/api/voices", "/api/voices?source_id=gemini", 200)
            .await["items"][0]["available"],
        false
    );

    s.delete(SRC, "/api/voice-sources/gemini", 204).await;
    assert_eq!(
        s.get(SRC, "/api/voice-sources/gemini", 200).await["has_key"],
        false
    );
    s.stop().await;
}

#[tokio::test]
async fn premium_audio_is_never_made_without_a_plan() {
    let g = FakeGemini::start("k").await;
    let s = start(&g).await;
    s.put(
        SRC,
        "/api/voice-sources/gemini",
        json!({ "api_key": "k" }),
        200,
    )
    .await;
    let voice = s.get("/api/voices", "/api/voices", 200).await["items"][0]["id"].clone();
    let book = s
        .post("/api/books/sample", "/api/books/sample", json!({}), 201)
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let ab = s
        .post(
            "/api/books/{book_id}/audiobooks",
            &format!("/api/books/{book}/audiobooks"),
            json!({ "voice_id": voice }),
            201,
        )
        .await;
    assert_eq!(ab["tier"], "premium");
    let id = ab["id"].as_str().unwrap();
    let ch = s
        .get(
            "/api/audiobooks/{audiobook_id}/chapters",
            &format!("/api/audiobooks/{id}/chapters"),
            200,
        )
        .await["items"][0]["chapter_id"]
        .as_str()
        .unwrap()
        .to_string();
    let e = s
        .post(
            "/api/audiobooks/{audiobook_id}/chapters/{chapter_id}/request",
            &format!("/api/audiobooks/{id}/chapters/{ch}/request"),
            json!({}),
            409,
        )
        .await;
    assert_eq!(e["code"], "plan_required");
    let e = s
        .post(
            "/api/audiobooks/{audiobook_id}/make-ready",
            &format!("/api/audiobooks/{id}/make-ready"),
            json!({ "scope": { "kind": "whole_book" } }),
            409,
        )
        .await;
    assert_eq!(e["code"], "plan_required");
    assert_eq!(
        s.get("/api/jobs", "/api/jobs", 200).await["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    s.stop().await;
}

#[tokio::test]
async fn the_allowance_starts_unlimited_and_changes_are_audited() {
    let g = FakeGemini::start("k").await;
    let s = start(&g).await;
    let a = s.get("/api/allowance", "/api/allowance", 200).await;
    assert_eq!(
        a["monthly_limit"],
        Value::Null,
        "no monthly limit by default"
    );
    assert_eq!(a["default_plan_limit"], money(10_000_000));
    assert_eq!(a["spent"], json!({ "known": money(0), "unknown_items": 0 }));
    assert_eq!(a["period_start"], "2026-01-01T00:00:00.000Z");
    assert_eq!(a["period_end"], "2026-02-01T00:00:00.000Z");

    let a = s
        .put(
            "/api/allowance",
            "/api/allowance",
            json!({ "monthly_limit": money(25_000_000), "default_plan_limit": money(5_000_000) }),
            200,
        )
        .await;
    assert_eq!(a["monthly_limit"], money(25_000_000));
    assert_eq!(a["default_plan_limit"], money(5_000_000));
    assert_eq!(
        s.get("/api/allowance", "/api/allowance", 200).await["monthly_limit"],
        money(25_000_000)
    );
    let a = s
        .put(
            "/api/allowance",
            "/api/allowance",
            json!({ "monthly_limit": null, "default_plan_limit": money(5_000_000) }),
            200,
        )
        .await;
    assert_eq!(a["monthly_limit"], Value::Null);

    for bad in [
        json!({ "monthly_limit": { "micros": 1, "currency": "EUR" }, "default_plan_limit": money(1) }),
        json!({ "monthly_limit": money(-1), "default_plan_limit": money(1) }),
        json!({ "monthly_limit": null, "default_plan_limit": money(0) }),
        json!({ "monthly_limit": null }),
    ] {
        s.put("/api/allowance", "/api/allowance", bad, 400).await;
    }
    let audit = s
        .get("/api/audit", "/api/audit?action=allowance.changed", 200)
        .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 2);
    assert_eq!(audit["items"][0]["actor"]["listener_name"], "Nick");
    s.stop().await;
}

#[tokio::test]
async fn prices_are_dated_editable_and_honest_about_where_they_come_from() {
    let g = FakeGemini::start("k").await;
    let s = start(&g).await;
    let p = s.get("/api/prices", "/api/prices", 200).await;
    let gem = &p["items"][0];
    assert_eq!(
        (
            gem["provider"].as_str().unwrap(),
            gem["unit"].as_str().unwrap(),
            gem["basis"].as_str().unwrap()
        ),
        ("gemini", "million_characters", "manual")
    );
    assert_eq!(gem["refresh_error"], Value::Null);

    let r = s.post("/api/prices", "/api/prices", json!({}), 200).await;
    assert!(r["items"][0]["refresh_error"]
        .as_str()
        .unwrap()
        .contains("manual"));

    s.clock.advance(chrono::Duration::days(3));
    let put = s
        .put(
            "/api/prices/{provider}",
            "/api/prices/gemini",
            json!({ "unit": "million_characters", "per_unit": money(20_000_000) }),
            200,
        )
        .await;
    assert_eq!(put["per_unit"], money(20_000_000));
    assert_eq!(put["as_of"], "2026-01-18T12:00:00.000Z");
    assert_eq!(put["refresh_error"], Value::Null);
    s.put(
        "/api/prices/{provider}",
        "/api/prices/gemini",
        json!({ "unit": "per_word", "per_unit": money(1) }),
        400,
    )
    .await;
    s.put(
        "/api/prices/{provider}",
        "/api/prices/gemini",
        json!({ "unit": "million_characters", "per_unit": money(0) }),
        400,
    )
    .await;
    let e = s
        .put(
            "/api/prices/{provider}",
            "/api/prices/nobody",
            json!({ "unit": "million_characters", "per_unit": money(1) }),
            404,
        )
        .await;
    assert_eq!(e["code"], "provider_not_found");
    s.stop().await;
}
