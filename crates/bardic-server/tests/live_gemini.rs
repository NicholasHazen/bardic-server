//! A bounded, PAID check against the real Gemini API. Not run by default.
//!
//!   BARDIC_LIVE_GEMINI_KEY=... \
//!   cargo test -p bardic-server --test live_gemini -- --ignored --nocapture
//!
//! (`GEMINI_API_KEY` in the environment or in the repository's git-ignored `.env` is also read.)
//!
//! It makes one voice sample, then one plan for one tiny chapter (about 120 characters of
//! original text) under a hard limit of $0.05, and prints the estimate range next to the actual
//! settled cost. It refuses to start the plan if the estimate's high end is over the limit.
//! The key is never printed.
mod common;
use common::{gemini::money, TestServer};
use serde_json::{json, Value};

const LIMIT_MICROS: i64 = 50_000;

fn key() -> String {
    for name in ["BARDIC_LIVE_GEMINI_KEY", "GEMINI_API_KEY"] {
        if let Ok(k) = std::env::var(name) {
            if !k.is_empty() {
                return k;
            }
        }
    }
    let env = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env");
    if let Ok(text) = std::fs::read_to_string(env) {
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("GEMINI_API_KEY=") {
                return v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
            }
        }
    }
    panic!("set BARDIC_LIVE_GEMINI_KEY");
}

fn micros(v: &Value) -> i64 {
    v["micros"].as_i64().unwrap()
}

#[tokio::test]
#[ignore = "PAID: needs a real Gemini key; at most $0.05 for the plan plus one sample"]
async fn a_real_gemini_sample_and_a_tiny_plan_under_five_cents() {
    let s = TestServer::start_with(tempfile::tempdir().unwrap(), |c| {
        c.gemini_url = "https://generativelanguage.googleapis.com".to_string();
    })
    .await;
    let l = s.listener("Live").await;
    s.act_as(&l);

    let src = s
        .put(
            "/api/voice-sources/{source_id}",
            "/api/voice-sources/gemini",
            json!({ "api_key": key() }),
            200,
        )
        .await;
    eprintln!("source: {} ({})", src["state"], src["detail"]);
    assert_eq!(src["state"], "connected");

    let voice = s
        .get("/api/voices", "/api/voices?source_id=gemini", 200)
        .await["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "Kore")
        .expect("no Kore voice")["id"]
        .as_str()
        .unwrap()
        .to_string();

    // one sample (the counted exception to the plan gate)
    let t0 = std::time::Instant::now();
    let (h, wav) = s
        .raw(
            "/api/voices/{voice_id}/sample",
            &format!("/api/voices/{voice}/sample"),
            &[],
            200,
        )
        .await;
    eprintln!(
        "sample: {:?} {} bytes in {:.1}s",
        h["content-type"],
        wav.len(),
        t0.elapsed().as_secs_f64()
    );
    assert!(wav.len() > 20_000, "the sample is nearly empty");
    let a0 = s.get("/api/allowance", "/api/allowance", 200).await;
    eprintln!("allowance after sample: spent {}", a0["spent"]);

    // one tiny book, one chapter
    let text = "Chapter One\n\nThe lantern swung low. The road ahead began to show itself.\n\nA small wind came up, and the grass bowed to it.\n";
    let book = s
        .add_book("live-gemini.txt", text.as_bytes().to_vec())
        .await;
    let ab = s
        .post(
            "/api/books/{book_id}/audiobooks",
            &format!("/api/books/{book}/audiobooks"),
            json!({ "voice_id": voice }),
            201,
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let est = s
        .post(
            "/api/audiobooks/{audiobook_id}/plan-preview",
            &format!("/api/audiobooks/{ab}/plan-preview"),
            json!({ "scope": { "kind": "whole_book" } }),
            200,
        )
        .await;
    let cost = &est["cost"];
    eprintln!(
        "estimate: {} chars, {} chapter(s), low {} likely {} high {} micros (basis {}), ~{:?}s",
        est["text_characters"],
        est["chapters_to_make"],
        micros(&cost["low"]),
        micros(&cost["likely"]),
        micros(&cost["high"]),
        cost["basis"],
        est["seconds_estimate"]
    );
    assert!(est["blocked"].is_null(), "{}", est["blocked"]);
    assert!(
        micros(&cost["high"]) <= LIMIT_MICROS,
        "the high estimate is over $0.05; not starting a paid plan"
    );

    let plan = s
        .post(
            "/api/plans",
            "/api/plans",
            json!({ "estimate_id": est["estimate_id"], "limit": money(LIMIT_MICROS) }),
            201,
        )
        .await;
    let plan_id = plan["id"].as_str().unwrap().to_string();
    let mut p = plan;
    for _ in 0..480 {
        p = s
            .get(
                "/api/plans/{plan_id}",
                &format!("/api/plans/{plan_id}"),
                200,
            )
            .await;
        if !["approved", "running"].contains(&p["state"].as_str().unwrap()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    eprintln!(
        "plan: {} chapters {}/{} spent {} micros, {} unknown, waiting {}, needs_you {}",
        p["state"],
        p["chapters_done"],
        p["chapters_total"],
        micros(&p["spent"]["known"]),
        p["spent"]["unknown_items"],
        p["waiting"],
        p["needs_you"]
    );
    let actual = micros(&p["spent"]["known"]);
    let (low, likely, high) = (
        micros(&cost["low"]),
        micros(&cost["likely"]),
        micros(&cost["high"]),
    );
    eprintln!(
        "actual {actual} vs estimate low {low} / likely {likely} / high {high}: {:.2}x likely, {}",
        actual as f64 / likely.max(1) as f64,
        if (low..=high).contains(&actual) {
            "inside the range"
        } else {
            "OUTSIDE the range"
        }
    );
    let a1 = s.get("/api/allowance", "/api/allowance", 200).await;
    eprintln!("allowance after plan: spent {}", a1["spent"]);

    // audio is kept and plays
    if p["state"] == "completed" {
        let items = s
            .get(
                "/api/audiobooks/{audiobook_id}/chapters",
                &format!("/api/audiobooks/{ab}/chapters"),
                200,
            )
            .await["items"]
            .clone();
        let url = items[0]["audio"]["url"].as_str().unwrap().to_string();
        let (h, bytes) = s.raw("/api/audio/{audio_id}", &url, &[], 200).await;
        eprintln!(
            "chapter audio: {:?} {} bytes",
            h["content-type"],
            bytes.len()
        );
    }
    let dir = s.stop().await;
    let db = rusqlite::Connection::open(dir.path().join("bardic.db")).unwrap();
    let mut q = db
        .prepare("SELECT status, reserved, known_micros, input_tokens, output_tokens, note FROM spend ORDER BY at, rowid")
        .unwrap();
    let rows = q
        .query_map([], |r| {
            Ok(format!(
                "ledger: {} reserved={:?} known={:?} in={:?} out={:?} note={:?}",
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<String>>(5)?
            ))
        })
        .unwrap();
    for r in rows {
        eprintln!("{}", r.unwrap());
    }
    assert_eq!(p["state"], "completed", "{p}");
    assert_eq!(p["spent"]["unknown_items"], 0, "{p}");
    assert!(actual <= LIMIT_MICROS, "spent over the limit");
}

/// A longer chapter (about 1,200 characters of original prose with dialogue and paragraph
/// breaks) to measure audio tokens per character, seconds per character and cost per character,
/// which the price table's estimate is built on. Limit $0.05; refuses to start above it.
#[tokio::test]
#[ignore = "PAID: about $0.03 for one plan of ~1,200 characters, limit $0.05"]
async fn a_real_gemini_long_chapter_measures_the_cost_per_character() {
    let s = TestServer::start_with(tempfile::tempdir().unwrap(), |c| {
        c.gemini_url = "https://generativelanguage.googleapis.com".to_string();
    })
    .await;
    let l = s.listener("Live").await;
    s.act_as(&l);
    let src = s
        .put(
            "/api/voice-sources/{source_id}",
            "/api/voice-sources/gemini",
            json!({ "api_key": key() }),
            200,
        )
        .await;
    assert_eq!(src["state"], "connected");
    let voice = s
        .get("/api/voices", "/api/voices?source_id=gemini", 200)
        .await["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "Kore")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let text = "Chapter Two\n\n\
The ferry left the dock at dusk, and nobody on it spoke for the first mile. The water was flat and dark, and the lamps along the far bank looked like a string of small, patient moons.\n\n\
\"You've done this crossing before,\" the ferryman said at last, without turning round.\n\n\
\"Twice,\" said Ines. \"Both times in the other direction.\"\n\n\
He laughed, a short dry sound, and shifted his grip on the long pole. \"Then you know the secret of it. The river doesn't care which way you're going. It only cares that you keep going.\"\n\n\
She thought about that while the lamps grew larger. Somewhere behind them a bell rang, once, and the sound travelled out over the water and was gone. By the time the hull touched the far landing her hands had stopped shaking, and the town above the bank had begun to put its lights out, one window after another.\n";
    let book = s.add_book("long.txt", text.as_bytes().to_vec()).await;
    let ab = s
        .post(
            "/api/books/{book_id}/audiobooks",
            &format!("/api/books/{book}/audiobooks"),
            json!({ "voice_id": voice }),
            201,
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let est = s
        .post(
            "/api/audiobooks/{audiobook_id}/plan-preview",
            &format!("/api/audiobooks/{ab}/plan-preview"),
            json!({ "scope": { "kind": "whole_book" } }),
            200,
        )
        .await;
    let chars = est["text_characters"].as_i64().unwrap();
    let (low, likely, high) = (
        micros(&est["cost"]["low"]),
        micros(&est["cost"]["likely"]),
        micros(&est["cost"]["high"]),
    );
    eprintln!(
        "estimate: {chars} chars, {} chapter(s), low {low} likely {likely} high {high} micros",
        est["chapters_to_make"]
    );
    assert!(high <= LIMIT_MICROS, "the high estimate is over $0.05");
    let plan = s
        .post(
            "/api/plans",
            "/api/plans",
            json!({ "estimate_id": est["estimate_id"], "limit": money(LIMIT_MICROS) }),
            201,
        )
        .await;
    let id = plan["id"].as_str().unwrap().to_string();
    let mut p = plan;
    for _ in 0..720 {
        p = s
            .get("/api/plans/{plan_id}", &format!("/api/plans/{id}"), 200)
            .await;
        if !["approved", "running"].contains(&p["state"].as_str().unwrap()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let actual = micros(&p["spent"]["known"]);
    eprintln!(
        "plan: {} spent {actual} micros, {} unknown",
        p["state"], p["spent"]["unknown_items"]
    );
    let items = s
        .get(
            "/api/audiobooks/{audiobook_id}/chapters",
            &format!("/api/audiobooks/{ab}/chapters"),
            200,
        )
        .await["items"]
        .clone();
    let ms = items[0]["audio"]["duration_ms"].as_i64().unwrap_or(0);
    let dir = s.stop().await;
    let db = rusqlite::Connection::open(dir.path().join("bardic.db")).unwrap();
    let (tin, tout): (i64, i64) = db
        .query_row(
            "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0) FROM spend WHERE status='known'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    eprintln!(
        "measured over {chars} chars: {:.2}s audio ({:.1} chars/s), {tout} audio tokens ({:.2}/char, {:.1}/s), {tin} text tokens ({:.2}/char)",
        ms as f64 / 1000.0,
        chars as f64 / (ms as f64 / 1000.0),
        tout as f64 / chars as f64,
        tout as f64 / (ms as f64 / 1000.0),
        tin as f64 / chars as f64
    );
    eprintln!(
        "cost per million characters: {:.2} USD (table: 25.00); estimate range {low}..{high}, actual {actual}: {}",
        actual as f64 / chars as f64,
        if (low..=high).contains(&actual) { "inside" } else { "OUTSIDE" }
    );
    assert_eq!(p["state"], "completed", "{p}");
    assert!(actual <= LIMIT_MICROS);
}
