//! Gemini speech: premium voices through the owner's API key.
//!
//! Gemini has no voice-list call: its prebuilt voices are a fixed set, offered
//! here with Google's own one-word descriptions. Checking a key reads the model
//! list, which is free and never speaks.

use super::{Catalog, CatalogVoice, Check};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::time::Duration;

pub const MODEL: &str = "gemini-3.8-flash-tts";

const VOICES: [(&str, &str); 30] = [
    ("Zephyr", "Bright"),
    ("Puck", "Upbeat"),
    ("Charon", "Informative"),
    ("Kore", "Firm"),
    ("Fenrir", "Excitable"),
    ("Leda", "Youthful"),
    ("Orus", "Firm"),
    ("Aoede", "Breezy"),
    ("Callirrhoe", "Easy-going"),
    ("Autonoe", "Bright"),
    ("Enceladus", "Breathy"),
    ("Iapetus", "Clear"),
    ("Umbriel", "Easy-going"),
    ("Algieba", "Smooth"),
    ("Despina", "Smooth"),
    ("Erinome", "Clear"),
    ("Algenib", "Gravelly"),
    ("Rasalgethi", "Informative"),
    ("Laomedeia", "Upbeat"),
    ("Achernar", "Soft"),
    ("Alnilam", "Firm"),
    ("Schedar", "Even"),
    ("Gacrux", "Mature"),
    ("Pulcherrima", "Forward"),
    ("Achird", "Friendly"),
    ("Zubenelgenubi", "Casual"),
    ("Vindemiatrix", "Gentle"),
    ("Sadachbia", "Lively"),
    ("Sadaltager", "Knowledgeable"),
    ("Sulafat", "Warm"),
];

/// The revision changes only when Bardic changes the model it speaks with.
pub fn revision() -> String {
    format!("{MODEL}/1")
}

pub fn voices() -> Vec<CatalogVoice> {
    VOICES
        .iter()
        .map(|(name, style)| CatalogVoice {
            external_id: name.to_string(),
            name: name.to_string(),
            description: style.to_string(),
            language: "und".into(),
            revision: revision(),
        })
        .collect()
}

pub fn client() -> Client {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .expect("http client")
}

/// Check the key against the model list (free) and report the voices.
pub async fn check(base_url: &str, key: &str) -> Catalog {
    let resp = client()
        .get(format!("{base_url}/v1beta/models"))
        .header("x-goog-api-key", key)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(_) => {
            return Catalog::failed(
                Check::Unreachable,
                "Could not reach Gemini. Check the network connection.",
            )
        }
    };
    match resp.status() {
        StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            return Catalog::failed(Check::KeyRejected, "Gemini rejected the API key.")
        }
        s if !s.is_success() => {
            return Catalog::failed(
                Check::Unreachable,
                format!("Gemini answered HTTP {} when checking the key.", s.as_u16()),
            )
        }
        _ => {}
    }
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let has_model = body["models"].as_array().is_none_or(|m| {
        m.iter()
            .any(|x| x["name"].as_str().is_some_and(|n| n.ends_with(MODEL)))
    });
    let mut detail = format!("Connected. {} voices.", VOICES.len());
    if !has_model {
        detail.push_str(&format!(
            " This key's project does not list {MODEL}; speech may be refused."
        ));
    }
    Catalog {
        check: Check::Connected,
        detail,
        voices: voices(),
    }
}

// ---------------------------------------------------------------- speaking

/// Dated list prices for `MODEL`, in micros of a dollar per million tokens (2026-09-27).
/// Actual spending is always computed from the token counts Gemini reports.
pub const INPUT_MICROS_PER_MILLION: i64 = 500_000;
pub const OUTPUT_MICROS_PER_MILLION: i64 = 9_000_000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

impl Usage {
    /// Cost in micros, only when the report is complete and unambiguous: input is all text,
    /// output is all audio, and nothing else was billed. Anything else is unknown, never zero.
    pub fn cost_micros(&self) -> Option<i64> {
        let (i, o) = (self.input_tokens?, self.output_tokens?);
        // Round up, so a known cost is never understated.
        Some(
            (i * INPUT_MICROS_PER_MILLION + 999_999) / 1_000_000
                + (o * OUTPUT_MICROS_PER_MILLION + 999_999) / 1_000_000,
        )
    }
}

pub fn parse_usage(body: &Value) -> Usage {
    let u = &body["usage"];
    let count = |v: &Value| v.as_i64().filter(|n| *n >= 0);
    let only = |field: &str, modality: &str, total: Option<i64>| -> Option<i64> {
        let parts = u[field].as_array()?;
        let sum: i64 = parts
            .iter()
            .map(|p| count(&p["tokens"]))
            .sum::<Option<i64>>()?;
        (!parts.is_empty() && parts.iter().all(|p| p["modality"] == modality) && Some(sum) == total)
            .then_some(sum)
    };
    let (input, output) = (
        count(&u["total_input_tokens"]),
        count(&u["total_output_tokens"]),
    );
    let clean = u["total_thought_tokens"].as_i64().unwrap_or(0) == 0
        && u["total_tool_use_tokens"].as_i64().unwrap_or(0) == 0
        && u["total_cached_tokens"].as_i64().unwrap_or(0) == 0;
    if !clean {
        return Usage::default();
    }
    Usage {
        input_tokens: only("input_tokens_by_modality", "text", input),
        output_tokens: only("output_tokens_by_modality", "audio", output),
    }
}

#[derive(Debug)]
pub enum SpeakError {
    KeyRejected,
    /// Quota or rate limit: nothing was made or billed. Wait this many seconds (`daily` when the day's quota is gone).
    Quota {
        retry_after: u64,
        daily: bool,
    },
    /// Gemini refused the request (for example blocked text): not billed.
    Refused(String),
    /// Gemini answered without audio. The request may still have been billed: carries the usage.
    NoAudio(Usage),
    /// The request may or may not have been processed (timeout, dropped connection): spending unknown.
    Uncertain,
    /// Could not connect: nothing was sent.
    Unreachable,
    Failed(String),
}

pub struct Speech {
    pub pcm: Vec<u8>,
    pub usage: Usage,
}

/// Speak one request. Not retried here: the caller decides, because every attempt can cost money.
pub async fn speak(
    base_url: &str,
    key: &str,
    voice: &str,
    text: &str,
) -> Result<Speech, SpeakError> {
    use base64::Engine;
    let http = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(240))
        .build()
        .expect("http client");
    let body = serde_json::json!({
        "model": MODEL,
        "input": [{ "type": "user_input", "content": [{ "type": "text", "text": text }] }],
        "response_format": { "type": "audio" },
        "generation_config": { "speech_config": [{ "voice": voice }] },
    });
    let resp = http
        .post(format!("{base_url}/v1beta/interactions"))
        .header("x-goog-api-key", key)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            if e.is_connect() {
                SpeakError::Unreachable
            } else {
                SpeakError::Uncertain
            }
        })?;
    let status = resp.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let text = resp.text().await.unwrap_or_default().to_lowercase();
        let daily = text.contains("perday") || text.contains("per day") || text.contains("daily");
        return Err(SpeakError::Quota {
            retry_after: retry.unwrap_or(60).clamp(1, 86_400),
            daily,
        });
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(SpeakError::KeyRejected);
    }
    if status.is_client_error() {
        // A rejected request is not billed. The message can echo the text, so only the status is kept.
        return Err(SpeakError::Refused(format!("http_{}", status.as_u16())));
    }
    if !status.is_success() {
        // A server error may have done work before failing.
        return Err(SpeakError::Uncertain);
    }
    let body: Value = resp.json().await.map_err(|_| SpeakError::Uncertain)?;
    let usage = parse_usage(&body);
    let block = body["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["type"] == "model_output")
        .flat_map(|s| s["content"].as_array().into_iter().flatten())
        .rfind(|c| c["type"] == "audio");
    let Some(data) = block.and_then(|b| b["data"].as_str()) else {
        return Err(SpeakError::NoAudio(usage));
    };
    let raw = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| SpeakError::Failed("Gemini returned invalid audio data.".into()))?;
    let mime = block
        .and_then(|b| b["mime_type"].as_str())
        .unwrap_or("")
        .to_lowercase();
    let pcm = if raw.starts_with(b"RIFF") && raw.get(8..12) == Some(b"WAVE") {
        wav_pcm(&raw).ok_or_else(|| {
            SpeakError::Failed("Gemini returned audio in a format this server cannot read.".into())
        })?
    } else if mime.starts_with("audio/l16") || mime.starts_with("audio/pcm") || mime.is_empty() {
        let rate = mime
            .split("rate=")
            .nth(1)
            .and_then(|r| r.split(';').next())
            .and_then(|r| r.trim().parse::<u32>().ok())
            .unwrap_or(24_000);
        if rate != 24_000 {
            return Err(SpeakError::Failed(
                "Gemini returned an unsupported sample rate.".into(),
            ));
        }
        raw
    } else {
        return Err(SpeakError::Failed(
            "Gemini returned an unsupported audio format.".into(),
        ));
    };
    if pcm.is_empty() || pcm.len() % 2 != 0 {
        return Err(SpeakError::NoAudio(usage));
    }
    Ok(Speech { pcm, usage })
}

/// The PCM of a 24 kHz 16-bit mono WAV, or None for any other layout.
fn wav_pcm(wav: &[u8]) -> Option<Vec<u8>> {
    let fmt = wav.get(20..36)?;
    let ok = u16::from_le_bytes([fmt[0], fmt[1]]) == 1
        && u16::from_le_bytes([fmt[2], fmt[3]]) == 1
        && u32::from_le_bytes(fmt[4..8].try_into().ok()?) == 24_000
        && u16::from_le_bytes([fmt[14], fmt[15]]) == 16;
    let data = wav.windows(4).position(|w| w == b"data")?;
    ok.then(|| wav.get(data + 8..).map(<[u8]>::to_vec))?
}
