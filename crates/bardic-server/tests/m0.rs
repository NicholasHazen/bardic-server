//! M0: server identity, devices, audit, events, instance lock, migrations.
//! Every response is validated against docs/contract/openapi.yaml.
mod common;
use bardic_server::{lock::InstanceLock, store::Store};
use chrono::Duration;
use common::{name_body, TestServer, DEVICE};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::Duration as StdDuration;

#[tokio::test]
async fn health_needs_no_headers() {
    let s = TestServer::start().await;
    let b = s
        .call(Method::GET, "/api/health", "/api/health", None, None, 200)
        .await;
    assert_eq!(b["ok"], true);
    s.stop().await;
}

#[tokio::test]
async fn server_describes_itself() {
    let s = TestServer::start().await;
    let b = s.get("/api/server", "/api/server", 200).await;
    assert_eq!(b["name"], "Test Bardic");
    assert_eq!(b["api_version"], s.contract.version());
    assert_eq!(b["max_upload_bytes"], 31_457_280);
    assert!(b["free_bytes"].is_u64());
    assert_eq!(b["id"].as_str().unwrap().len(), 26, "ULID");
    // The id is stable across requests.
    let again = s.get("/api/server", "/api/server", 200).await;
    assert_eq!(b["id"], again["id"]);
    s.stop().await;
}

#[tokio::test]
async fn changes_need_a_device() {
    let s = TestServer::start().await;
    let e = s
        .call(
            Method::PATCH,
            "/api/server",
            "/api/server",
            None,
            Some(name_body("X")),
            400,
        )
        .await;
    assert_eq!(e["code"], "device_required");
    // A malformed device id is refused, even on a read.
    let e = s
        .call(
            Method::GET,
            "/api/server",
            "/api/server",
            Some("bad id!"),
            None,
            400,
        )
        .await;
    assert_eq!(e["code"], "invalid_request");
    s.stop().await;
}

#[tokio::test]
async fn renaming_the_server_is_audited_and_announced() {
    let s = TestServer::start().await;
    let b = s
        .patch(
            "/api/server",
            "/api/server",
            name_body("  Nick's Mac mini  "),
            200,
        )
        .await;
    assert_eq!(b["name"], "Nick's Mac mini", "trimmed");
    let audit = s
        .get("/api/audit", "/api/audit?action=server.renamed", 200)
        .await;
    let items = audit["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["actor"]["device_id"], DEVICE);
    assert_eq!(items[0]["actor"]["listener_id"], Value::Null);
    assert_eq!(items[0]["target"]["from"], "Test Bardic");
    assert_eq!(items[0]["target"]["to"], "Nick's Mac mini");
    s.stop().await;
}

#[tokio::test]
async fn names_are_validated() {
    let s = TestServer::start().await;
    for bad in ["", "   ", &"x".repeat(61), "bad\nname"] {
        let e = s
            .patch("/api/server", "/api/server", name_body(bad), 400)
            .await;
        assert_eq!(e["code"], "name_invalid", "{bad:?}");
    }
    let e = s
        .call(
            Method::PATCH,
            "/api/server",
            "/api/server",
            Some(DEVICE),
            Some(json!({"nope": 1})),
            400,
        )
        .await;
    assert_eq!(e["code"], "invalid_request");
    s.stop().await;
}

#[tokio::test]
async fn devices_register_on_first_use_and_can_be_named() {
    let s = TestServer::start().await;
    s.get("/api/server", "/api/server", 200).await;
    let list = s.get("/api/devices", "/api/devices", 200).await;
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], DEVICE);
    assert_eq!(items[0]["name"], "New device");

    let path = format!("/api/devices/{DEVICE}");
    let d = s
        .patch(
            "/api/devices/{device_id}",
            &path,
            name_body("Nick's phone"),
            200,
        )
        .await;
    assert_eq!(d["name"], "Nick's phone");

    let e = s
        .patch(
            "/api/devices/{device_id}",
            "/api/devices/unknown-device-id",
            name_body("x"),
            404,
        )
        .await;
    assert_eq!(e["code"], "device_not_found");

    // The audit record names the device by its current name.
    let audit = s
        .get("/api/audit", "/api/audit?action=device.renamed", 200)
        .await;
    assert_eq!(audit["items"][0]["actor"]["device_name"], "New device");
    s.stop().await;
}

#[tokio::test]
async fn last_seen_moves_at_most_every_thirty_seconds() {
    let s = TestServer::start().await;
    s.get("/api/server", "/api/server", 200).await;
    let first =
        s.get("/api/devices", "/api/devices", 200).await["items"][0]["last_seen_at"].clone();
    s.clock.advance(Duration::seconds(10));
    s.get("/api/server", "/api/server", 200).await;
    let second =
        s.get("/api/devices", "/api/devices", 200).await["items"][0]["last_seen_at"].clone();
    assert_eq!(first, second, "within 30 s nothing is written");
    s.clock.advance(Duration::seconds(31));
    s.get("/api/server", "/api/server", 200).await;
    let third =
        s.get("/api/devices", "/api/devices", 200).await["items"][0]["last_seen_at"].clone();
    assert_ne!(second, third);
    s.stop().await;
}

#[tokio::test]
async fn audit_pages_newest_first() {
    let s = TestServer::start().await;
    for n in ["One", "Two", "Three"] {
        s.patch("/api/server", "/api/server", name_body(n), 200)
            .await;
    }
    let p1 = s.get("/api/audit", "/api/audit?limit=2", 200).await;
    assert_eq!(p1["items"].as_array().unwrap().len(), 2);
    assert_eq!(p1["items"][0]["target"]["to"], "Three");
    let next = p1["next"].as_str().expect("more pages").to_string();
    let p2 = s
        .get(
            "/api/audit",
            &format!("/api/audit?limit=2&after={next}"),
            200,
        )
        .await;
    assert_eq!(p2["items"].as_array().unwrap().len(), 1);
    assert_eq!(p2["items"][0]["target"]["to"], "One");
    assert_eq!(p2["next"], Value::Null);
    let e = s.get("/api/audit", "/api/audit?limit=0", 400).await;
    assert_eq!(e["code"], "invalid_request");
    s.stop().await;
}

async fn read_until(resp: &mut reqwest::Response, needle: &str) -> String {
    let mut seen = String::new();
    let found = tokio::time::timeout(StdDuration::from_secs(5), async {
        while let Some(chunk) = resp.chunk().await.expect("chunk") {
            seen.push_str(&String::from_utf8_lossy(&chunk));
            if seen.contains(needle) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(found, "did not see {needle:?} in the stream; saw:\n{seen}");
    seen
}

#[tokio::test]
async fn events_stream_announces_changes() {
    let s = TestServer::start().await;
    let mut resp = s
        .client
        .get(format!("{}/api/events", s.base))
        .header("x-bardic-device", DEVICE)
        .send()
        .await
        .expect("connect");
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    s.patch("/api/server", "/api/server", name_body("Live"), 200)
        .await;
    let seen = read_until(&mut resp, "server.updated").await;
    assert!(seen.contains("event: notice"));
    assert!(seen.contains("id: "));
    s.stop().await;
}

#[tokio::test]
async fn events_resume_replays_or_asks_to_resync() {
    let s = TestServer::start().await;
    s.patch("/api/server", "/api/server", name_body("A"), 200)
        .await; // device.updated + server.updated
                // Resume from before anything: replays what was missed.
    let mut r = s
        .client
        .get(format!("{}/api/events", s.base))
        .header("last-event-id", "0")
        .send()
        .await
        .expect("connect");
    read_until(&mut r, "server.updated").await;
    // Resume from an id the server never issued: reload.
    let mut r = s
        .client
        .get(format!("{}/api/events", s.base))
        .header("last-event-id", "999999")
        .send()
        .await
        .expect("connect");
    read_until(&mut r, "resync").await;
    s.stop().await;
}

#[tokio::test]
async fn unknown_routes_and_methods_answer_in_json() {
    let s = TestServer::start().await;
    let r = s
        .client
        .get(format!("{}/api/nope", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let b: Value = r.json().await.unwrap();
    assert_eq!(b["code"], "route_not_found");
    let r = s
        .client
        .delete(format!("{}/api/health", s.base))
        .header("x-bardic-device", DEVICE)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 405);
    s.stop().await;
}

#[tokio::test]
async fn only_one_server_may_use_a_data_folder() {
    let s = TestServer::start().await;
    let err = InstanceLock::acquire(s.dir.path())
        .err()
        .expect("second lock must fail");
    assert!(err.to_string().contains("already using"), "{err}");
    // A second server on the same folder refuses to start.
    let cfg = bardic_server::config::Config::for_data_dir(s.dir.path());
    let clock: std::sync::Arc<dyn bardic_server::clock::Clock> = s.clock.clone();
    assert!(bardic_server::app::spawn(cfg, clock).await.is_err());
    let dir = s.stop().await;
    InstanceLock::acquire(dir.path()).expect("free again after stop");
}

#[tokio::test]
async fn data_survives_a_restart_and_migrations_are_idempotent() {
    let s = TestServer::start().await;
    let id = s.get("/api/server", "/api/server", 200).await["id"].clone();
    s.patch(
        "/api/server",
        "/api/server",
        name_body("Keeps its name"),
        200,
    )
    .await;
    let dir = s.stop().await;
    let s2 = TestServer::start_in(dir).await;
    let b = s2.get("/api/server", "/api/server", 200).await;
    assert_eq!(b["id"], id);
    assert_eq!(b["name"], "Keeps its name");
    assert_eq!(s2.running.as_ref().unwrap().state.store.schema_version(), 1);
    s2.stop().await;
}

#[test]
fn a_newer_data_folder_is_refused_not_damaged() {
    let dir = tempfile::tempdir().unwrap();
    {
        let conn = rusqlite::Connection::open(dir.path().join("bardic.db")).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    let err = Store::open(dir.path()).err().expect("must refuse");
    assert!(err.to_string().contains("newer Bardic"), "{err}");
}
