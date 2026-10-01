//! A bounded check against a real Breeze server. Not run by default.
//!
//!   BARDIC_LIVE_BREEZE_URL=http://host:7860 \
//!   [BARDIC_LIVE_BREEZE_KEY=...] [BARDIC_LIVE_BREEZE_VOICE=<voice name or id>] \
//!   cargo test --test live_breeze -- --ignored --nocapture
//!
//! It makes one voice sample and one tiny chapter (about 120 characters of original text, a few
//! seconds of audio) and prints what it measured. Nothing leaves your network, and nothing is paid for.
mod common;
use common::TestServer;
use serde_json::{json, Value};

fn pcm_stats(wav: &[u8]) -> (usize, f64, i16) {
    let pcm = &wav[44..];
    let samples: Vec<i16> = pcm
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    let rms = (samples.iter().map(|s| (*s as f64).powi(2)).sum::<f64>()
        / samples.len().max(1) as f64)
        .sqrt();
    (
        samples.len(),
        rms,
        samples
            .iter()
            .map(|s| s.saturating_abs())
            .max()
            .unwrap_or(0),
    )
}

#[tokio::test]
#[ignore = "needs a real Breeze server: set BARDIC_LIVE_BREEZE_URL"]
async fn a_real_breeze_server_makes_a_sample_and_a_timed_chapter() {
    let url = std::env::var("BARDIC_LIVE_BREEZE_URL").expect("set BARDIC_LIVE_BREEZE_URL");
    check(url).await;
}

/// The same check against the fake, so the check itself is known to work.
#[tokio::test]
async fn the_live_check_passes_against_the_fake() {
    let b = common::breeze::FakeBreeze::start().await;
    check(b.url.clone()).await;
}

async fn check(url: String) {
    let s = TestServer::start().await;
    let l = s.listener("Live").await;
    s.act_as(&l);

    let mut cfg = json!({ "base_url": url });
    if let Ok(k) = std::env::var("BARDIC_LIVE_BREEZE_KEY") {
        cfg["api_key"] = json!(k);
    }
    let src = s
        .put(
            "/api/voice-sources/{source_id}",
            "/api/voice-sources/breeze",
            cfg,
            200,
        )
        .await;
    eprintln!("source: {} ({})", src["state"], src["detail"]);
    assert_eq!(src["state"], "connected");

    let voices = s
        .get("/api/voices", "/api/voices?source_id=breeze", 200)
        .await["items"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        !voices.is_empty(),
        "Breeze offers no cloned voices; save one in Breeze first"
    );
    for v in &voices {
        eprintln!(
            "voice: {} ({}) language={} revision={}…",
            v["name"],
            v["id"],
            v["language"],
            &v["revision"].as_str().unwrap()[..12]
        );
    }
    let wanted = std::env::var("BARDIC_LIVE_BREEZE_VOICE").ok();
    let voice = voices
        .iter()
        .find(|v| {
            wanted
                .as_deref()
                .is_none_or(|w| v["name"] == w || v["id"] == w)
        })
        .expect("no voice matches BARDIC_LIVE_BREEZE_VOICE")
        .clone();

    // a sample
    let started = std::time::Instant::now();
    let (h, wav) = s
        .raw(
            "/api/voices/{voice_id}/sample",
            &format!("/api/voices/{}/sample", voice["id"].as_str().unwrap()),
            &[],
            200,
        )
        .await;
    assert_eq!(h["content-type"], "audio/wav");
    let (n, rms, peak) = pcm_stats(&wav);
    eprintln!(
        "sample: {:.1}s of audio in {:.1}s, rms {rms:.0}, peak {peak}",
        n as f64 / 24000.0,
        started.elapsed().as_secs_f64()
    );
    assert!(n > 12_000, "less than half a second of audio");
    assert!(peak > 500, "the sample is silent");

    // a tiny chapter
    let text = "Chapter One\n\nThe lantern swung low. The road ahead began to show itself.\n\nA small wind came up, and the grass bowed to it.\n";
    let book = s.add_book("live.txt", text.as_bytes().to_vec()).await;
    let ab = s
        .post(
            "/api/books/{book_id}/audiobooks",
            &format!("/api/books/{book}/audiobooks"),
            json!({ "voice_id": voice["id"] }),
            201,
        )
        .await;
    let ab_id = ab["id"].as_str().unwrap();
    let chapters = s
        .get(
            "/api/audiobooks/{audiobook_id}/chapters",
            &format!("/api/audiobooks/{ab_id}/chapters"),
            200,
        )
        .await["items"]
        .as_array()
        .unwrap()
        .clone();
    let ch = chapters[0]["chapter_id"].as_str().unwrap();
    let t0 = std::time::Instant::now();
    let job = s
        .call(
            reqwest::Method::POST,
            "/api/audiobooks/{audiobook_id}/chapters/{chapter_id}/request",
            &format!("/api/audiobooks/{ab_id}/chapters/{ch}/request"),
            Some(common::DEVICE),
            Some(json!({ "ahead": 0 })),
            202,
        )
        .await;
    let mut state = Value::Null;
    for _ in 0..1200 {
        state = s
            .get(
                "/api/jobs/{job_id}",
                &format!("/api/jobs/{}", job["id"].as_str().unwrap()),
                200,
            )
            .await;
        if !["queued", "running", "waiting"].contains(&state["state"].as_str().unwrap()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    eprintln!(
        "chapter job: {} in {:.1}s {}",
        state["state"],
        t0.elapsed().as_secs_f64(),
        state["needs_you"]
    );
    assert_eq!(state["state"], "completed", "{state}");

    let audio = s
        .get(
            "/api/audiobooks/{audiobook_id}/chapters",
            &format!("/api/audiobooks/{ab_id}/chapters"),
            200,
        )
        .await["items"][0]["audio"]
        .clone();
    let (_, wav) = s
        .raw(
            "/api/audio/{audio_id}",
            audio["url"].as_str().unwrap(),
            &[],
            200,
        )
        .await;
    let (n, rms, peak) = pcm_stats(&wav);
    let secs = n as f64 / 24000.0;
    eprintln!(
        "chapter audio: {secs:.1}s, rms {rms:.0}, peak {peak}, {} bytes",
        wav.len()
    );
    assert!(peak > 500, "the chapter is silent");
    assert!(
        (0.5..120.0).contains(&secs),
        "implausible duration {secs}s for ~120 characters"
    );

    let timings = s
        .get(
            "/api/audio/{audio_id}/timings",
            audio["timings_url"].as_str().unwrap(),
            200,
        )
        .await;
    let lines = timings["lines"].as_array().unwrap();
    for l in lines {
        eprintln!(
            "line {}: {} ms - {} ms",
            l["line_id"], l["start_ms"], l["end_ms"]
        );
    }
    let mut prev = 0;
    for l in lines {
        let (a, b) = (
            l["start_ms"].as_i64().unwrap(),
            l["end_ms"].as_i64().unwrap(),
        );
        assert!(prev <= a && a <= b && b as f64 <= secs * 1000.0 + 100.0);
        prev = a;
    }
    s.stop().await;
}
