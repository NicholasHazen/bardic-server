//! Browser origin rules: foreign pages may not change anything; allowed ones get CORS headers.
mod common;
use common::{TestServer, DEVICE};
use serde_json::json;

async fn server() -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    TestServer::start_with(dir, |c| {
        c.allow_origins = vec!["http://localhost:5173".to_string()]
    })
    .await
}

#[tokio::test]
async fn a_foreign_page_cannot_change_anything_or_read_anything() {
    let s = server().await;
    let r = s
        .client
        .post(format!("{}/api/listeners", s.base))
        .header("origin", "http://evil.example")
        .header("x-bardic-device", DEVICE)
        .json(&json!({"name": "X"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    assert!(r.headers().get("access-control-allow-origin").is_none());
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["code"], "origin_not_allowed");
    // Nothing was created.
    assert_eq!(
        s.get("/api/listeners", "/api/listeners", 200).await["items"],
        json!([])
    );
    // Reads are answered but carry no CORS headers, so the page cannot see them.
    let r = s
        .client
        .get(format!("{}/api/health", s.base))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("access-control-allow-origin").is_none());
    s.stop().await;
}

#[tokio::test]
async fn an_allowed_page_works_and_preflight_is_answered() {
    let s = server().await;
    let r = s
        .client
        .request(
            reqwest::Method::OPTIONS,
            format!("{}/api/listeners", s.base),
        )
        .header("origin", "http://localhost:5173")
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(
        r.headers()["access-control-allow-origin"],
        "http://localhost:5173"
    );
    assert!(r.headers()["access-control-allow-headers"]
        .to_str()
        .unwrap()
        .contains("x-bardic-device"));
    let r = s
        .client
        .post(format!("{}/api/listeners", s.base))
        .header("origin", "http://localhost:5173")
        .header("x-bardic-device", DEVICE)
        .json(&json!({"name": "Nick"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    assert_eq!(
        r.headers()["access-control-allow-origin"],
        "http://localhost:5173"
    );
    // Preflight from a page that is not allowed gets nothing useful.
    let r = s
        .client
        .request(
            reqwest::Method::OPTIONS,
            format!("{}/api/listeners", s.base),
        )
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert!(r.headers().get("access-control-allow-origin").is_none());
    s.stop().await;
}

#[tokio::test]
async fn the_servers_own_pages_and_scripts_are_allowed() {
    let s = server().await;
    // Same origin: the Origin header names this server's own address.
    let host = s.base.trim_start_matches("http://").to_string();
    let r = s
        .client
        .post(format!("{}/api/listeners", s.base))
        .header("origin", format!("http://{host}"))
        .header("x-bardic-device", DEVICE)
        .json(&json!({"name": "Sam"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    // No Origin header at all (curl): allowed.
    s.listener("Riley").await;
    s.stop().await;
}

#[tokio::test]
async fn a_rebound_public_name_is_refused_for_reads_and_writes() {
    let s = server().await;
    // DNS rebinding: the page's origin and the Host header are both the attacker's name.
    let post = s
        .client
        .post(format!("{}/api/listeners", s.base))
        .header("host", "evil.example:8765")
        .header("origin", "http://evil.example:8765")
        .header("x-bardic-device", DEVICE)
        .json(&json!({"name": "X"}))
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 403);
    assert!(post.headers().get("access-control-allow-origin").is_none());
    let b: serde_json::Value = post.json().await.unwrap();
    assert_eq!(b["code"], "host_not_allowed");
    // a same-origin read from the rebound page carries no Origin at all
    let get = s
        .client
        .get(format!("{}/api/listeners", s.base))
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), 403);
    assert_eq!(
        s.get("/api/listeners", "/api/listeners", 200).await["items"],
        json!([])
    );
    s.stop().await;
}

#[tokio::test]
async fn the_names_a_household_uses_are_accepted() {
    let s = TestServer::start_with(tempfile::tempdir().unwrap(), |c| {
        c.allow_hosts = vec!["bardic.example.org".to_string()];
        c.allow_origins = vec!["https://reader.example.org".to_string()];
    })
    .await;
    for host in [
        "127.0.0.1:8765",
        "localhost:8765",
        "[::1]:8765",
        "100.114.183.42:8765",
        "macbook-pro:8765",
        "Macbook-Pro.local:8765",
        "macbook-pro.tail1234.ts.net",
        "nas.home.arpa",
        "bardic.example.org",
        "BARDIC.example.org:443",
        "reader.example.org",
    ] {
        let r = s
            .client
            .get(format!("{}/api/health", s.base))
            .header("host", host)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{host}");
    }
    for host in [
        "evil.example",
        "evil.example:8765",
        "127.0.0.1.evil.example",
        "localhost.evil.example",
    ] {
        let r = s
            .client
            .get(format!("{}/api/health", s.base))
            .header("host", host)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "{host}");
    }
    s.stop().await;
}
