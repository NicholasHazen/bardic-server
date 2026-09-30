//! M1a: listeners and their settings.
mod common;
use common::{name_body, TestServer};
use serde_json::{json, Value};

const L: &str = "/api/listeners";
const LID: &str = "/api/listeners/{listener_id}";

#[tokio::test]
async fn a_fresh_server_has_no_listeners_and_the_first_is_created_like_any_other() {
    let s = TestServer::start().await;
    let list = s.get(L, L, 200).await;
    assert_eq!(list["items"], json!([]));
    let b = s.post(L, L, name_body("  Nick "), 201).await;
    assert_eq!(b["name"], "Nick");
    assert_eq!(b["books_started"], 0);
    assert_eq!(b["last_listened_at"], Value::Null);
    assert_eq!(b["id"].as_str().unwrap().len(), 26);
    s.stop().await;
}

#[tokio::test]
async fn names_are_unique_ignoring_case_and_validated() {
    let s = TestServer::start().await;
    s.listener("Nick").await;
    let e = s.post(L, L, name_body("nICk"), 409).await;
    assert_eq!(e["code"], "name_taken");
    for bad in ["", "  ", &"x".repeat(41)] {
        let e = s.post(L, L, name_body(bad), 400).await;
        assert_eq!(e["code"], "name_invalid", "{bad:?}");
    }
    s.stop().await;
}

#[tokio::test]
async fn listeners_are_listed_by_name_ignoring_case() {
    let s = TestServer::start().await;
    for n in ["sam", "Nick", "Riley", "jo"] {
        s.listener(n).await;
    }
    let names: Vec<String> = s.get(L, L, 200).await["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["jo", "Nick", "Riley", "sam"]);
    s.stop().await;
}

#[tokio::test]
async fn get_rename_and_unknown() {
    let s = TestServer::start().await;
    let id = s.listener("Nick").await;
    s.listener("Sam").await;
    let path = format!("{L}/{id}");
    assert_eq!(s.get(LID, &path, 200).await["name"], "Nick");
    // A case-only change of your own name is allowed.
    assert_eq!(
        s.patch(LID, &path, name_body("NICK"), 200).await["name"],
        "NICK"
    );
    // Taking another listener's name is not.
    let e = s.patch(LID, &path, name_body("sam"), 409).await;
    assert_eq!(e["code"], "name_taken");
    let e = s
        .get(LID, &format!("{L}/01AAAAAAAAAAAAAAAAAAAAAAAA"), 404)
        .await;
    assert_eq!(e["code"], "listener_not_found");
    let e = s
        .patch(
            LID,
            &format!("{L}/01AAAAAAAAAAAAAAAAAAAAAAAA"),
            name_body("x"),
            404,
        )
        .await;
    assert_eq!(e["code"], "listener_not_found");
    s.stop().await;
}

#[tokio::test]
async fn deleting_removes_only_that_listener_and_never_the_last() {
    let s = TestServer::start().await;
    let nick = s.listener("Nick").await;
    let sam = s.listener("Sam").await;
    let imp = s
        .get(
            "/api/listeners/{listener_id}/impact",
            &format!("{L}/{sam}/impact"),
            200,
        )
        .await;
    assert_eq!(
        imp,
        json!({ "books_started": 0, "places": 0, "history_entries": 0 })
    );

    s.delete(LID, &format!("{L}/{sam}"), 204).await;
    let e = s.get(LID, &format!("{L}/{sam}"), 404).await;
    assert_eq!(e["code"], "listener_not_found");
    // Settings went with it.
    s.get(
        "/api/listeners/{listener_id}/settings",
        &format!("{L}/{sam}/settings"),
        404,
    )
    .await;
    // Nick is untouched and is now the last listener.
    assert_eq!(
        s.get(LID, &format!("{L}/{nick}"), 200).await["name"],
        "Nick"
    );
    let e = s.delete(LID, &format!("{L}/{nick}"), 409).await;
    assert_eq!(e["code"], "last_listener");
    let e = s.delete(LID, &format!("{L}/{sam}"), 404).await;
    assert_eq!(e["code"], "listener_not_found");
    s.get(
        "/api/listeners/{listener_id}/impact",
        &format!("{L}/{sam}/impact"),
        404,
    )
    .await;

    let audit = s.get("/api/audit", "/api/audit?limit=50", 200).await;
    let actions: Vec<&str> = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["action"].as_str().unwrap())
        .collect();
    assert!(
        actions.contains(&"listener.created") && actions.contains(&"listener.deleted"),
        "{actions:?}"
    );
    // The name of a deleted listener can be used again.
    s.listener("Sam").await;
    s.stop().await;
}

#[tokio::test]
async fn settings_have_defaults_and_are_validated() {
    let s = TestServer::start().await;
    let id = s.listener("Nick").await;
    let path = format!("{L}/{id}/settings");
    let t = "/api/listeners/{listener_id}/settings";
    let d = s.get(t, &path, 200).await;
    assert_eq!(
        d,
        json!({ "default_voice_id": null, "place_conflict": "ask", "continue_into_next_chapter": true })
    );

    let body = json!({ "default_voice_id": null, "place_conflict": "newest", "continue_into_next_chapter": false });
    assert_eq!(s.put(t, &path, body.clone(), 200).await, body);
    assert_eq!(s.get(t, &path, 200).await, body);

    let bad = json!({ "default_voice_id": null, "place_conflict": "whatever", "continue_into_next_chapter": true });
    assert_eq!(s.put(t, &path, bad, 400).await["code"], "invalid_request");
    let partial = json!({ "place_conflict": "ask" });
    assert_eq!(
        s.put(t, &path, partial, 400).await["code"],
        "invalid_request"
    );
    // No voice exists yet (voices arrive in M3).
    let voice = json!({ "default_voice_id": "v1", "place_conflict": "ask", "continue_into_next_chapter": true });
    assert_eq!(s.put(t, &path, voice, 404).await["code"], "voice_not_found");
    // The failed changes did not stick.
    assert_eq!(s.get(t, &path, 200).await, body);
    s.stop().await;
}

#[tokio::test]
async fn listener_changes_are_announced() {
    let s = TestServer::start().await;
    let mut resp = s
        .client
        .get(format!("{}/api/events", s.base))
        .header("x-bardic-device", common::DEVICE)
        .send()
        .await
        .unwrap();
    let id = s.listener("Nick").await;
    let mut seen = String::new();
    let ok = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(chunk) = resp.chunk().await.unwrap() {
            seen.push_str(&String::from_utf8_lossy(&chunk));
            if seen.contains("listener.updated") && seen.contains(&id) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(ok, "no listener.updated for {id}; saw {seen}");
    drop(resp);
    s.stop().await;
}
