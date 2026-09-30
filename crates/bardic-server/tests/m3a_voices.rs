//! M3a: voice sources (against a fake Breeze server), voices and audiobooks.
mod common;
use common::{breeze::FakeBreeze, TestServer};
use serde_json::{json, Value};

const SRC: &str = "/api/voice-sources/{source_id}";

async fn configure(s: &TestServer, body: Value, expect: u16) -> Value {
    s.put(SRC, "/api/voice-sources/breeze", body, expect).await
}

async fn voices(s: &TestServer, q: &str) -> Vec<Value> {
    s.get("/api/voices", &format!("/api/voices{q}"), 200).await["items"]
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test]
async fn the_three_sources_start_unset_and_gemini_and_unknown_are_refused() {
    let s = TestServer::start().await;
    let l = s.get("/api/voice-sources", "/api/voice-sources", 200).await;
    let kinds: Vec<_> = l["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["id"].as_str().unwrap(),
                i["state"].as_str().unwrap(),
                i["tier"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("breeze", "not_set_up", "free"),
            ("gemini", "not_set_up", "premium"),
            ("local", "unavailable", "free")
        ]
    );
    assert_eq!(l["items"][0]["base_url"], Value::Null);
    assert_eq!(l["items"][1]["has_key"], false);

    let e = s
        .put(
            SRC,
            "/api/voice-sources/gemini",
            json!({ "api_key": "x" }),
            400,
        )
        .await;
    assert_eq!(e["code"], "source_unsupported");
    let e = s.get(SRC, "/api/voice-sources/nope", 404).await;
    assert_eq!(e["code"], "source_not_found");
    // an unconfigured source checks out as unconfigured, without any network
    let t = s
        .post(
            "/api/voice-sources/{source_id}/test",
            "/api/voice-sources/breeze/test",
            json!({}),
            200,
        )
        .await;
    assert_eq!(t["state"], "not_set_up");
    assert_eq!(voices(&s, "").await.len(), 0);
    s.stop().await;
}

#[tokio::test]
async fn setting_up_breeze_lists_only_its_cloned_voices() {
    let s = TestServer::start().await;
    let b = FakeBreeze::start().await;

    let e = configure(&s, json!({ "base_url": "ftp://host" }), 400).await;
    assert_eq!(e["code"], "invalid_request");
    let e = configure(&s, json!({ "base_url": "http://host:1/path" }), 400).await;
    assert_eq!(e["code"], "invalid_request");
    let e = configure(&s, json!({}), 400).await;
    assert_eq!(e["code"], "invalid_request");
    // an unreachable address is refused and nothing is stored
    let e = configure(&s, json!({ "base_url": "http://127.0.0.1:1" }), 400).await;
    assert_eq!(e["code"], "source_unreachable");
    assert_eq!(
        s.get(SRC, "/api/voice-sources/breeze", 200).await["state"],
        "not_set_up"
    );

    let src = configure(&s, json!({ "base_url": format!("{}/", b.url) }), 200).await;
    assert_eq!(src["state"], "connected");
    assert_eq!(src["voice_count"], 2);
    assert_eq!(src["base_url"], b.url);
    assert!(src["detail"].as_str().unwrap().contains("1 designed"));
    assert!(src["checked_at"].is_string());

    let all = voices(&s, "").await;
    assert_eq!(all.len(), 2);
    assert_eq!(all[0]["name"], "Mara");
    assert_eq!(all[0]["tier"], "free");
    assert_eq!(all[0]["language"], "en");
    assert_eq!(all[1]["language"], "und");
    assert!(all
        .iter()
        .all(|v| v["available"] == true && v["source_id"] == "breeze"));
    assert_eq!(voices(&s, "?tier=premium").await.len(), 0);
    assert_eq!(voices(&s, "?language=EN").await.len(), 1);
    assert_eq!(voices(&s, "?source_id=gemini").await.len(), 0);
    s.get("/api/voices", "/api/voices?tier=gold", 400).await;
    s.stop().await;
}

#[tokio::test]
async fn an_api_key_is_checked_kept_and_never_returned() {
    let s = TestServer::start().await;
    let b = FakeBreeze::start().await;
    b.state.lock().unwrap().key = Some("sekret-key".into());

    let e = configure(&s, json!({ "base_url": b.url }), 400).await;
    assert_eq!(e["code"], "key_rejected");
    let e = configure(&s, json!({ "base_url": b.url, "api_key": "wrong" }), 400).await;
    assert_eq!(e["code"], "key_rejected");
    assert!(!e.to_string().contains("wrong"));

    let ok = configure(
        &s,
        json!({ "base_url": b.url, "api_key": "sekret-key" }),
        200,
    )
    .await;
    assert_eq!(ok["has_key"], true);
    // omitting the key keeps it
    let again = configure(&s, json!({ "base_url": b.url }), 200).await;
    assert_eq!(again["state"], "connected");
    let dump = s
        .get("/api/voice-sources", "/api/voice-sources", 200)
        .await
        .to_string();
    assert!(!dump.contains("sekret-key"));
    // an empty key clears it, and the server then refuses
    let e = configure(&s, json!({ "base_url": b.url, "api_key": "" }), 400).await;
    assert_eq!(e["code"], "key_rejected");
    // and the audit log does not hold the key either
    let audit = s
        .get("/api/audit", "/api/audit?limit=50", 200)
        .await
        .to_string();
    assert!(!audit.contains("sekret-key"));
    s.stop().await;
}

#[tokio::test]
async fn refresh_tracks_revisions_and_outages_keep_voices_but_mark_them_unavailable() {
    let s = TestServer::start().await;
    let b = FakeBreeze::start().await;
    configure(&s, json!({ "base_url": b.url }), 200).await;
    let before = voices(&s, "").await;
    let mara_before = before.iter().find(|v| v["name"] == "Mara").unwrap().clone();

    // the reference clip changes under the same id: the revision changes, the id does not
    b.state.lock().unwrap().reference = b"clip-two".to_vec();
    let src = s
        .post(
            "/api/voice-sources/{source_id}/refresh",
            "/api/voice-sources/breeze/refresh",
            json!({}),
            200,
        )
        .await;
    assert_eq!(src["state"], "connected");
    let mara_after = voices(&s, "")
        .await
        .into_iter()
        .find(|v| v["name"] == "Mara")
        .unwrap();
    assert_eq!(mara_after["id"], mara_before["id"]);
    assert_ne!(mara_after["revision"], mara_before["revision"]);

    // the server goes away
    b.stop();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let src = s
        .post(
            "/api/voice-sources/{source_id}/test",
            "/api/voice-sources/breeze/test",
            json!({}),
            200,
        )
        .await;
    assert_eq!(src["state"], "unreachable");
    assert_eq!(src["voice_count"], 0);
    let listed = voices(&s, "").await;
    assert_eq!(listed.len(), 2, "last known voices stay listed");
    assert!(listed.iter().all(|v| v["available"] == false));

    // removing forgets the address, keeps the voices
    s.delete(SRC, "/api/voice-sources/breeze", 204).await;
    s.delete(SRC, "/api/voice-sources/breeze", 204).await;
    let src = s.get(SRC, "/api/voice-sources/breeze", 200).await;
    assert_eq!(
        (src["state"].as_str().unwrap(), src["base_url"].clone()),
        ("not_set_up", Value::Null)
    );
    assert_eq!(voices(&s, "").await.len(), 2);
    s.stop().await;
}

#[tokio::test]
async fn audiobooks_are_one_per_book_and_voice_revision_and_spend_nothing() {
    let s = TestServer::start().await;
    let l = s.listener("Nick").await;
    s.act_as(&l);
    let b = FakeBreeze::start().await;
    configure(&s, json!({ "base_url": b.url }), 200).await;
    let book = s
        .post("/api/books/sample", "/api/books/sample", json!({}), 201)
        .await;
    let book_id = book["id"].as_str().unwrap().to_string();
    let path = format!("/api/books/{book_id}/audiobooks");
    let tpl = "/api/books/{book_id}/audiobooks";
    let mara = voices(&s, "")
        .await
        .into_iter()
        .find(|v| v["name"] == "Mara")
        .unwrap();
    let vid = mara["id"].as_str().unwrap();

    assert_eq!(
        s.get(tpl, &path, 200).await["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let e = s.post(tpl, &path, json!({ "voice_id": "nope" }), 404).await;
    assert_eq!(e["code"], "voice_not_found");
    s.post(tpl, &path, json!({}), 400).await;

    let ab = s.post(tpl, &path, json!({ "voice_id": vid }), 201).await;
    assert_eq!(ab["voice_name"], "Mara");
    assert_eq!(ab["source_id"], "breeze");
    assert_eq!(ab["tier"], "free");
    assert_eq!(ab["chapters_total"], 3);
    assert_eq!(ab["chapters_ready"], 0);
    assert_eq!(ab["active_job_id"], Value::Null);
    assert_eq!(ab["voice_revision"], mara["revision"]);
    let again = s.post(tpl, &path, json!({ "voice_id": vid }), 200).await;
    assert_eq!(again["id"], ab["id"]);

    let ab_id = ab["id"].as_str().unwrap();
    let got = s
        .get(
            "/api/audiobooks/{audiobook_id}",
            &format!("/api/audiobooks/{ab_id}"),
            200,
        )
        .await;
    assert_eq!(got["id"], ab["id"]);
    s.get(
        "/api/audiobooks/{audiobook_id}",
        "/api/audiobooks/nope",
        404,
    )
    .await;
    let ch = s
        .get(
            "/api/audiobooks/{audiobook_id}/chapters",
            &format!("/api/audiobooks/{ab_id}/chapters"),
            200,
        )
        .await;
    let items = ch["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert!(items
        .iter()
        .all(|c| c["state"] == "not_yet" && c["audio"].is_null()));

    // the voice changes on the server: the same book gets a second audiobook, the first is untouched
    b.state.lock().unwrap().reference = b"clip-two".to_vec();
    s.post(
        "/api/voice-sources/{source_id}/refresh",
        "/api/voice-sources/breeze/refresh",
        json!({}),
        200,
    )
    .await;
    let second = s.post(tpl, &path, json!({ "voice_id": vid }), 201).await;
    assert_ne!(second["id"], ab["id"]);
    assert_ne!(second["voice_revision"], ab["voice_revision"]);
    assert_eq!(
        s.get(tpl, &path, 200).await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // a listener may choose it as their default voice
    let st = format!("/api/listeners/{l}/settings");
    s.put("/api/listeners/{listener_id}/settings", &st, json!({ "default_voice_id": vid, "place_conflict": "ask", "continue_into_next_chapter": true }), 200).await;

    // a removed source: existing audiobooks stay, new ones for its voices are refused
    b.stop();
    s.delete(SRC, "/api/voice-sources/breeze", 204).await;
    let other = s
        .post("/api/books/sample", "/api/books/sample", json!({}), 201)
        .await;
    let opath = format!("/api/books/{}/audiobooks", other["id"].as_str().unwrap());
    let e = s.post(tpl, &opath, json!({ "voice_id": vid }), 409).await;
    assert_eq!(e["code"], "voice_unavailable");
    assert_eq!(
        s.post(tpl, &path, json!({ "voice_id": vid }), 200).await["id"],
        second["id"]
    );

    // a removed book cannot get audiobooks
    let rm = format!("/api/books/{book_id}/remove");
    s.post("/api/books/{book_id}/remove", &rm, json!({}), 200)
        .await;
    let e = s.post(tpl, &path, json!({ "voice_id": vid }), 409).await;
    assert_eq!(e["code"], "book_removed");
    s.get(tpl, "/api/books/nope/audiobooks", 404).await;
    s.stop().await;
}
