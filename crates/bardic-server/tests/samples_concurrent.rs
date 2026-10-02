mod common;

use common::{breeze::FakeBreeze, gemini::FakeGemini, TestServer};
use futures_util::future::join_all;
use reqwest::Method;
use serde_json::json;
use std::time::Duration;

const SAMPLE: &str = "/api/voices/{voice_id}/sample";
const SOURCE: &str = "/api/voice-sources/{source_id}";

async fn premium() -> (TestServer, FakeGemini, String) {
    let g = FakeGemini::start("sample-key").await;
    let url = g.url.clone();
    let s = TestServer::start_with(tempfile::tempdir().unwrap(), move |c| c.gemini_url = url).await;
    s.put(
        SOURCE,
        "/api/voice-sources/gemini",
        json!({ "api_key": "sample-key" }),
        200,
    )
    .await;
    let voice = s
        .get("/api/voices", "/api/voices?source_id=gemini", 200)
        .await["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    (s, g, voice)
}

async fn received(g: &FakeGemini, n: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while g.received() < n {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fake provider received the sample");
}

async fn ledger(s: &TestServer) -> (i64, i64, i64, i64) {
    s.running.as_ref().unwrap().state.store.run(|c| {
        Ok(c.query_row("SELECT COUNT(*),COALESCE(SUM(status='reserved'),0),COALESCE(SUM(status='known'),0),COALESCE(SUM(status='unknown'),0) FROM spend", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?)
    }).await.unwrap()
}

async fn request(
    s: &TestServer,
    voice: &str,
    method: Method,
    range: Option<&str>,
    expected: u16,
) -> (reqwest::header::HeaderMap, Vec<u8>) {
    let mut req = s
        .client
        .request(
            method.clone(),
            format!("{}/api/voices/{voice}/sample", s.base),
        )
        .header("x-bardic-device", common::DEVICE);
    if let Some(range) = range {
        req = req.header("range", range);
    }
    let response = req.send().await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.bytes().await.unwrap().to_vec();
    assert_eq!(status, expected, "{}", String::from_utf8_lossy(&body));
    let json = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with("application/json"))
        .map(|_| serde_json::from_slice(&body).unwrap());
    // Axum's automatic HEAD mirrors GET's headers/status and removes the body.
    s.contract
        .check("GET", SAMPLE, status, json.as_ref())
        .unwrap();
    (headers, body)
}

#[tokio::test]
async fn simultaneous_premium_get_head_and_range_misses_share_one_request_and_usage() {
    let (s, g, voice) = premium().await;
    g.state.lock().unwrap().delay_ms = 350;
    let batch = async {
        join_all((0..10).map(|i| {
            request(
                &s,
                &voice,
                if i == 0 { Method::HEAD } else { Method::GET },
                if i % 3 == 1 { Some("bytes=0-3") } else { None },
                if i % 3 == 1 { 206 } else { 200 },
            )
        }))
        .await
    };
    let check = async {
        received(&g, 1).await;
        assert_eq!(
            ledger(&s).await,
            (1, 1, 0, 0),
            "only one reservation while the provider is delayed"
        );
        // A normal write succeeds during the provider await: no database lock is retained.
        s.listener("Concurrent listener").await;
    };
    let (outcomes, _) = tokio::join!(batch, check);
    assert!(outcomes[0].1.is_empty());
    assert_eq!(outcomes[1].1, b"RIFF");
    assert_eq!(g.received(), 1);
    assert_eq!(ledger(&s).await, (1, 0, 1, 0));
    request(&s, &voice, Method::GET, Some("bytes=999999999-"), 416).await;
    request(&s, &voice, Method::GET, Some("bytes=0-3"), 206).await;
    assert_eq!(
        g.received(),
        1,
        "invalid and cached ranges never start another request"
    );
    assert_eq!(ledger(&s).await, (1, 0, 1, 0));
    s.stop().await;
    g.stop();
}

#[tokio::test]
async fn a_shared_billed_failure_is_settled_once_and_an_explicit_retry_is_not_wedged() {
    let (s, g, voice) = premium().await;
    {
        let mut f = g.state.lock().unwrap();
        f.delay_ms = 350;
        f.no_audio = 1;
    }
    let failures = join_all((0..8).map(|_| request(&s, &voice, Method::GET, None, 409))).await;
    for (_, body) in failures {
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["code"],
            "provider_refused"
        );
    }
    assert_eq!(g.received(), 1);
    assert_eq!(ledger(&s).await, (1, 0, 1, 0));
    g.state.lock().unwrap().delay_ms = 0;
    let (_, audio) = request(&s, &voice, Method::GET, None, 200).await;
    assert_eq!(&audio[..4], b"RIFF");
    assert_eq!(g.received(), 2);
    assert_eq!(ledger(&s).await, (2, 0, 2, 0));
    request(&s, &voice, Method::GET, None, 200).await;
    assert_eq!(g.received(), 2);
    s.stop().await;
    g.stop();
}

#[tokio::test]
async fn cancellation_and_server_stop_drain_the_sample_before_a_restart_reuses_it() {
    let (s, g, voice) = premium().await;
    g.state.lock().unwrap().delay_ms = 350;
    let client = s.client.clone();
    let url = format!("{}/api/voices/{voice}/sample", s.base);
    let waiter = tokio::spawn(async move {
        client
            .get(url)
            .header("x-bardic-device", common::DEVICE)
            .send()
            .await
    });
    received(&g, 1).await;
    waiter.abort();
    assert_eq!(ledger(&s).await, (1, 1, 0, 0));
    let dir = s.stop().await; // must keep the single-writer lock until provider, settlement and cache commit end
    let url = g.url.clone();
    let s = TestServer::start_with(dir, move |c| c.gemini_url = url).await;
    request(&s, &voice, Method::GET, None, 200).await;
    assert_eq!(
        g.received(),
        1,
        "restart reads the completed sample instead of paying again"
    );
    assert_eq!(ledger(&s).await, (1, 0, 1, 0));
    s.stop().await;
    g.stop();
}

#[tokio::test]
async fn removing_the_key_during_an_admitted_sample_keeps_its_audio_and_usage() {
    let (s, g, voice) = premium().await;
    g.state.lock().unwrap().delay_ms = 350;
    let sample = request(&s, &voice, Method::GET, None, 200);
    let remove = async {
        received(&g, 1).await;
        s.delete(SOURCE, "/api/voice-sources/gemini", 204).await;
        let voices = s
            .get("/api/voices", "/api/voices?source_id=gemini", 200)
            .await;
        let other = voices["items"][1]["id"].as_str().unwrap();
        let (_, e) = request(&s, other, Method::GET, None, 409).await;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&e).unwrap()["code"],
            "source_not_set_up"
        );
    };
    tokio::join!(sample, remove);
    request(&s, &voice, Method::GET, None, 200).await;
    assert_eq!(
        g.received(),
        1,
        "the completed cache stays usable after source removal"
    );
    assert_eq!(ledger(&s).await, (1, 0, 1, 0));
    s.stop().await;
    g.stop();
}

#[tokio::test]
async fn rotating_the_key_during_a_sample_does_not_duplicate_a_same_revision_miss() {
    let (s, g, voice) = premium().await;
    g.state.lock().unwrap().delay_ms = 350;
    let first = request(&s, &voice, Method::GET, None, 200);
    let rotate = async {
        received(&g, 1).await;
        g.state.lock().unwrap().key = "rotated-sample-key".into();
        s.put(
            SOURCE,
            "/api/voice-sources/gemini",
            json!({ "api_key": "rotated-sample-key" }),
            200,
        )
        .await;
        request(&s, &voice, Method::GET, None, 200).await
    };
    let (first, second) = tokio::join!(first, rotate);
    assert_eq!(first.1, second.1);
    assert_eq!(g.received(), 1);
    assert_eq!(ledger(&s).await, (1, 0, 1, 0));
    s.stop().await;
    g.stop();
}

#[tokio::test]
async fn changed_free_revisions_have_separate_flights_and_never_overwrite_the_old_sample() {
    let s = TestServer::start().await;
    let b = FakeBreeze::start().await;
    s.put(
        SOURCE,
        "/api/voice-sources/breeze",
        json!({ "base_url": b.url }),
        200,
    )
    .await;
    let voice = s
        .get("/api/voices", "/api/voices?source_id=breeze", 200)
        .await["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    b.state.lock().unwrap().delay_ms = 350;
    let first = join_all((0..5).map(|_| request(&s, &voice, Method::GET, None, 200)));
    let refresh = async {
        // Refresh only after the first revision passed preflight and reached speech.
        tokio::time::timeout(Duration::from_secs(3), async {
            while b.received() < 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        b.state.lock().unwrap().reference = b"synthetic-revision-two".to_vec();
        s.post(
            "/api/voice-sources/{source_id}/refresh",
            "/api/voice-sources/breeze/refresh",
            json!({}),
            200,
        )
        .await;
        request(&s, &voice, Method::GET, None, 200).await
    };
    let (old, new) = tokio::join!(first, refresh);
    assert_ne!(old[0].0["etag"], new.0["etag"]);
    assert!(old.iter().all(|o| o.0["etag"] == old[0].0["etag"]));
    assert_eq!(b.spoken().len(), 2, "one free sample for each revision");
    let vid = voice.clone();
    let cached: i64 = s
        .running
        .as_ref()
        .unwrap()
        .state
        .store
        .run(move |c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM voice_samples WHERE voice_id=?1",
                [vid],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(cached, 2);
    assert_eq!(ledger(&s).await, (0, 0, 0, 0));
    s.stop().await;
    b.stop();
}

#[cfg(unix)]
#[tokio::test]
async fn a_cache_inspection_io_error_never_falls_back_to_another_paid_request() {
    let (s, g, voice) = premium().await;
    request(&s, &voice, Method::GET, None, 200).await;
    let vid = voice.clone();
    let path: String = s
        .running
        .as_ref()
        .unwrap()
        .state
        .store
        .run(move |c| {
            Ok(c.query_row(
                "SELECT path FROM voice_samples WHERE voice_id=?1",
                [vid],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    let path = s.dir.path().join(path);
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(path.file_name().unwrap(), &path).unwrap(); // ELOOP instead of NotFound
    request(&s, &voice, Method::GET, None, 500).await;
    assert_eq!(g.received(), 1);
    assert_eq!(ledger(&s).await, (1, 0, 1, 0));
    s.stop().await;
    g.stop();
}
