//! Browser origin rules: foreign pages may not change anything; allowed ones get CORS headers.
mod common;
use common::{gemini::FakeGemini, TestServer, DEVICE};
use reqwest::Method;
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

async fn premium_samples() -> (TestServer, FakeGemini, Vec<String>) {
    let g = FakeGemini::start("origin-test-key").await;
    let s = TestServer::start_with(tempfile::tempdir().unwrap(), |c| {
        c.gemini_url = g.url.clone();
        c.allow_origins = vec!["http://localhost:5173".to_string()];
    })
    .await;
    let listener = s.listener("Sample tester").await;
    s.act_as(&listener);
    s.put(
        "/api/voice-sources/{source_id}",
        "/api/voice-sources/gemini",
        json!({"api_key": "origin-test-key"}),
        200,
    )
    .await;
    let voices = s
        .get("/api/voices", "/api/voices?source_id=gemini", 200)
        .await["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|voice| voice["id"].as_str().unwrap().to_string())
        .collect();
    (s, g, voices)
}

async fn denied_sample(s: &TestServer, voice: &str, method: Method, headers: &[(&str, &str)]) {
    // Intentionally no device/listener headers: an <audio> tag supplies neither.
    let mut request = s.client.request(
        method.clone(),
        format!("{}/api/voices/{voice}/sample", s.base),
    );
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.send().await.unwrap();
    assert_eq!(response.status(), 403, "{method} {headers:?}");
    assert!(response
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    assert_sample_vary(response.headers());
    if method == Method::HEAD {
        assert!(response.bytes().await.unwrap().is_empty());
    } else {
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["code"], "origin_not_allowed");
        s.contract
            .check("GET", "/api/voices/{voice_id}/sample", 403, Some(&body))
            .unwrap();
    }
}

fn assert_sample_vary(headers: &reqwest::header::HeaderMap) {
    let vary = headers
        .get_all("vary")
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect::<Vec<_>>()
        .join(", ");
    for name in [
        "Origin",
        "Referer",
        "Sec-Fetch-Site",
        "Sec-Fetch-Mode",
        "Sec-Fetch-Dest",
        "Sec-Fetch-User",
        "X-Bardic-Device",
    ] {
        assert!(
            vary.split(", ").any(|part| part == name),
            "missing Vary {name}"
        );
    }
}

#[tokio::test]
async fn foreign_sample_get_and_head_never_contact_a_provider_or_reserve_spending() {
    let (s, g, voices) = premium_samples().await;
    let own_page = format!("{}/reader", s.base);
    let cases: Vec<Vec<(&str, &str)>> = vec![
        vec![],
        // Plain HTTP LAN no-cors requests can omit every provenance header.
        vec![("user-agent", "Mozilla/5.0 Synthetic browser")],
        vec![("origin", "https://evil.example")],
        vec![
            ("origin", "https://evil.example"),
            ("x-bardic-device", DEVICE),
        ],
        vec![("origin", "null")],
        vec![("origin", "not-an-origin")],
        vec![("origin", "http://localhost:5173.evil.example")],
        // Origin remains authoritative, even with apparently trusted fallback headers.
        vec![
            ("origin", "https://evil.example"),
            ("sec-fetch-site", "same-origin"),
            ("referer", &own_page),
        ],
        // Modern no-cors fetch, audio and no-referrer requests omit Origin.
        vec![("sec-fetch-site", "cross-site")],
        vec![
            ("sec-fetch-site", "cross-site"),
            ("sec-fetch-mode", "no-cors"),
            ("sec-fetch-dest", "audio"),
        ],
        vec![("sec-fetch-site", "same-site")],
        vec![
            ("sec-fetch-site", "cross-site"),
            ("referer", "https://evil.example/page"),
        ],
        // Older browsers can supply only Referer; its full URL is reduced to an origin.
        vec![("referer", "https://evil.example/page")],
        vec![("referer", "http://localhost:5173.evil.example/page")],
        vec![("referer", "not-a-url")],
        vec![("sec-fetch-site", "unexpected")],
        vec![
            ("sec-fetch-site", "unexpected"),
            ("x-bardic-device", DEVICE),
        ],
        vec![("sec-fetch-mode", "no-cors"), ("sec-fetch-dest", "audio")],
        vec![("sec-fetch-mode", "cors")],
    ];
    for method in [Method::GET, Method::HEAD] {
        for headers in &cases {
            denied_sample(&s, &voices[0], method.clone(), headers).await;
        }
    }
    assert_eq!(g.received(), 0, "all rejections precede provider contact");
    let spend_rows: i64 = s
        .running
        .as_ref()
        .unwrap()
        .state
        .store
        .run(|c| Ok(c.query_row("SELECT COUNT(*) FROM spend", [], |row| row.get(0))?))
        .await
        .unwrap();
    assert_eq!(spend_rows, 0, "no request even reserved an Allowance item");

    // Presence of a device header is not enough: the normal device layer still validates it.
    for method in [Method::GET, Method::HEAD] {
        let response = s
            .client
            .request(
                method.clone(),
                format!("{}/api/voices/{}/sample", s.base, voices[0]),
            )
            .header("x-bardic-device", "short")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "invalid device on {method}");
        if method == Method::GET {
            assert_eq!(
                response.json::<serde_json::Value>().await.unwrap()["code"],
                "invalid_request"
            );
        } else {
            assert!(response.bytes().await.unwrap().is_empty());
        }
    }
    assert_eq!(
        g.received(),
        0,
        "invalid device ids cannot start generation"
    );

    // Normal read-only GET remains available without CORS permission.
    let response = s
        .client
        .get(format!("{}/api/health", s.base))
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    s.stop().await;
    g.stop();
}

#[tokio::test]
async fn allowed_sample_media_scripts_and_implicit_head_keep_their_existing_behavior() {
    let (s, g, voices) = premium_samples().await;
    let own_page = format!("{}/reader", s.base);
    let cases: Vec<(Method, Vec<(&str, &str)>)> = vec![
        (Method::GET, vec![("origin", &s.base)]),
        (Method::HEAD, vec![("origin", "http://localhost:5173")]),
        (Method::GET, vec![("sec-fetch-site", "same-origin")]),
        (Method::GET, vec![("sec-fetch-site", "none")]),
        (Method::GET, vec![("referer", &own_page)]),
        // A separate allowlisted web client can load a sample with a native media tag.
        (
            Method::GET,
            vec![
                ("sec-fetch-site", "cross-site"),
                ("referer", "http://localhost:5173/voices"),
            ],
        ),
        (
            Method::GET,
            vec![("referer", "http://localhost:5173/voices")],
        ),
        // Same-origin reverse proxies rewrite Host but preserve browser metadata/Referer.
        (
            Method::GET,
            vec![
                ("sec-fetch-site", "same-origin"),
                ("referer", "http://localhost:8080/voices"),
            ],
        ),
        // Scripts without browser provenance identify their existing device.
        (Method::GET, vec![("x-bardic-device", DEVICE)]),
        (Method::HEAD, vec![("x-bardic-device", DEVICE)]),
        (
            Method::GET,
            vec![("x-bardic-device", DEVICE), ("sec-fetch-mode", "cors")],
        ),
        (
            Method::HEAD,
            vec![("x-bardic-device", DEVICE), ("sec-fetch-mode", "cors")],
        ),
    ];
    for (index, (method, headers)) in cases.iter().enumerate() {
        let mut request = s.client.request(
            method.clone(),
            format!("{}/api/voices/{}/sample", s.base, voices[index]),
        );
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 200, "{method} {headers:?}");
        assert_eq!(response.headers()["content-type"], "audio/wav");
        assert_sample_vary(response.headers());
        if let Some((_, origin)) = headers.iter().find(|(name, _)| *name == "origin") {
            assert_eq!(response.headers()["access-control-allow-origin"], *origin);
        } else {
            assert!(response
                .headers()
                .get("access-control-allow-origin")
                .is_none());
        }
        let bytes = response.bytes().await.unwrap();
        if *method == Method::HEAD {
            assert!(
                bytes.is_empty(),
                "allowed implicit HEAD generates but sends no body"
            );
        } else {
            assert_eq!(&bytes[..4], b"RIFF");
        }
        assert_eq!(
            g.received(),
            index + 1,
            "each fresh voice makes one real fake request"
        );
    }
    let allowance = s.get("/api/allowance", "/api/allowance", 200).await;
    assert!(allowance["spent"]["known"]["micros"].as_i64().unwrap() > 0);

    // The guard also applies to cached samples, so behavior does not depend on cache state.
    denied_sample(
        &s,
        &voices[0],
        Method::GET,
        &[("origin", "https://evil.example")],
    )
    .await;
    denied_sample(
        &s,
        &voices[0],
        Method::HEAD,
        &[("sec-fetch-site", "cross-site")],
    )
    .await;
    let response = s
        .client
        .get(format!("{}/api/voices/{}/sample", s.base, voices[0]))
        .header("origin", "http://localhost:5173")
        .header("range", "bytes=0-15")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    assert_eq!(response.bytes().await.unwrap().len(), 16);
    assert_eq!(
        g.received(),
        cases.len(),
        "cached ranges and rejections spend nothing"
    );
    assert_eq!(
        s.get("/api/allowance", "/api/allowance", 200).await["spent"],
        allowance["spent"]
    );
    s.stop().await;
    g.stop();
}

#[tokio::test]
async fn no_origin_browser_writes_use_the_same_provenance_check() {
    let s = server().await;
    let response = s
        .client
        .post(format!("{}/api/listeners", s.base))
        .header("x-bardic-device", DEVICE)
        .header("sec-fetch-site", "cross-site")
        .json(&json!({"name": "Foreign"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["code"],
        "origin_not_allowed"
    );
    assert_eq!(
        s.get("/api/listeners", "/api/listeners", 200).await["items"],
        json!([])
    );
    let response = s
        .client
        .post(format!("{}/api/listeners", s.base))
        .header("x-bardic-device", DEVICE)
        .header("sec-fetch-site", "cross-site")
        .header("referer", "http://localhost:5173/listeners")
        .json(&json!({"name": "Allowed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    s.stop().await;
}

#[tokio::test]
async fn script_fetch_with_a_device_can_configure_a_source_with_partial_metadata() {
    let (s, g, _) = premium_samples().await;
    let listener = s.as_listener.lock().unwrap().clone().unwrap();
    let response = s
        .client
        .put(format!("{}/api/voice-sources/gemini", s.base))
        .header("x-bardic-device", DEVICE)
        .header("x-bardic-listener", &listener)
        .header("sec-fetch-mode", "cors")
        .json(&json!({"api_key": "origin-test-key"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    s.contract
        .check("PUT", "/api/voice-sources/{source_id}", 200, Some(&body))
        .unwrap();
    assert_eq!(body["state"], "connected");
    assert_eq!(g.received(), 0, "configuration never generates paid audio");

    // Partial metadata without the device header remains an untrusted browser request.
    let response = s
        .client
        .put(format!("{}/api/voice-sources/gemini", s.base))
        .header("x-bardic-listener", &listener)
        .header("sec-fetch-mode", "cors")
        .json(&json!({"api_key": "not-a-real-key"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["code"],
        "origin_not_allowed"
    );
    assert_eq!(g.received(), 0);
    s.stop().await;
    g.stop();
}
