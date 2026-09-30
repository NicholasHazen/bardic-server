//! Breeze: the owner's own voice server on the local network.
//!
//! Only cloned voices are offered: a designed voice sounds different on every
//! request, so audio made with it could never be made the same again. A voice's
//! revision hashes only what changes speech (settings, instruction, reference
//! text and the reference clip), not its label or description.

use super::{Catalog, CatalogVoice, Check};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;

const MAX_VOICES: usize = 200;

/// A plain http(s) server root: no credentials, path, query or fragment.
pub fn normalize_base_url(raw: &str) -> Result<String, String> {
    let s = raw.trim().trim_end_matches('/');
    if s.is_empty() || s.len() > 500 {
        return Err("Enter the Breeze server address, for example http://host.local:7860.".into());
    }
    let url = reqwest::Url::parse(s).map_err(|_| {
        "Use an http:// or https:// address, for example http://host.local:7860.".to_string()
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(
            "Use an http:// or https:// address, for example http://host.local:7860.".into(),
        );
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(
            "Enter only the server address and port, without a path, query or credentials.".into(),
        );
    }
    Ok(s.to_string())
}

fn client() -> Client {
    // A LAN server: never through a system proxy, never follow redirects.
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .expect("http client")
}

fn revision(voice: &Value, reference_sha256: &str) -> String {
    let reference = voice.get("reference").cloned().unwrap_or(Value::Null);
    let material = json!({
        "schema": 1,
        "id": voice.get("id"),
        "created_at": voice.get("created_at"),
        "instruction": voice.get("instruction"),
        "settings": voice.get("settings"),
        "reference_text": reference.get("text"),
        "reference_sha256": reference_sha256,
    });
    // serde_json objects are sorted, so this text is canonical.
    hex(Sha256::digest(material.to_string().as_bytes()).as_slice())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn valid_id(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
}

fn clip(v: Option<&Value>, max: usize) -> String {
    v.and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(max)
        .collect()
}

/// Check the server and read its voices. Free: never generates audio.
pub async fn fetch_catalog(base_url: &str, api_key: Option<&str>) -> Catalog {
    let http = client();
    let auth = |r: reqwest::RequestBuilder| match api_key.filter(|k| !k.is_empty()) {
        Some(k) => r.bearer_auth(k),
        None => r,
    };
    let unreachable = || {
        Catalog::failed(
            Check::Unreachable,
            format!("Could not reach the Breeze server at {base_url}."),
        )
    };

    match http.get(format!("{base_url}/health")).send().await {
        Err(_) => return unreachable(),
        Ok(r) if r.status() == StatusCode::SERVICE_UNAVAILABLE => {
            return Catalog::failed(
                Check::Unreachable,
                "The Breeze server is starting or its model is loading. Try again in a minute.",
            )
        }
        Ok(r) if !r.status().is_success() => {
            return Catalog::failed(
                Check::Unreachable,
                format!(
                    "The Breeze server answered its health check with HTTP {}.",
                    r.status().as_u16()
                ),
            )
        }
        Ok(_) => {}
    }
    let listing = match auth(http.get(format!("{base_url}/v1/voices"))).send().await {
        Err(_) => return unreachable(),
        Ok(r) => r,
    };
    if listing.status() == StatusCode::UNAUTHORIZED || listing.status() == StatusCode::FORBIDDEN {
        return Catalog::failed(
            Check::KeyRejected,
            "The Breeze server rejected the API key.",
        );
    }
    if !listing.status().is_success() {
        return Catalog::failed(
            Check::Unreachable,
            format!(
                "The Breeze server answered HTTP {} when listing voices.",
                listing.status().as_u16()
            ),
        );
    }
    let Ok(body) = listing.json::<Value>().await else {
        return Catalog::failed(
            Check::Unreachable,
            "The Breeze server sent a voice list this server cannot read.",
        );
    };
    let mut voices = vec![];
    let mut hidden = 0usize;
    for v in body
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_VOICES)
    {
        let Some(id) = v.get("id").and_then(Value::as_str).filter(|i| valid_id(i)) else {
            continue;
        };
        if v.get("kind").and_then(Value::as_str) != Some("cloned") {
            hidden += 1;
            continue;
        }
        let sha = match auth(http.get(format!("{base_url}/v1/voices/{id}/reference")))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => match r.bytes().await {
                Ok(b) => hex(Sha256::digest(&b).as_slice()),
                Err(_) => return unreachable(),
            },
            Ok(_) => {
                hidden += 1;
                continue;
            }
            Err(_) => return unreachable(),
        };
        let language = v
            .get("labels")
            .and_then(|l| l.get("language"))
            .and_then(Value::as_str)
            .filter(|l| !l.is_empty() && l.len() <= 20)
            .unwrap_or("und")
            .to_string();
        let name = clip(v.get("name"), 200);
        voices.push(CatalogVoice {
            external_id: id.to_string(),
            name: if name.is_empty() {
                id.to_string()
            } else {
                name
            },
            description: clip(v.get("description"), 500),
            language,
            revision: revision(v, &sha),
        });
    }
    let mut detail = format!(
        "Connected. {} voice{}.",
        voices.len(),
        if voices.len() == 1 { "" } else { "s" }
    );
    if hidden > 0 {
        detail.push_str(&format!(" {hidden} designed or unreadable voice{} not offered: save a preview as a cloned voice in Breeze to use it.", if hidden == 1 { " is" } else { "s are" }));
    }
    Catalog {
        check: Check::Connected,
        detail,
        voices,
    }
}

// ---------------------------------------------------------------- speaking

pub const MODEL: &str = "breeze-tts-2";
const DEFAULT_SEED: i64 = 42;
/// One request's audio is bounded; 24 kHz 16-bit mono is 48 kB a second.
const MAX_PCM_BYTES: usize = 200 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub enum SpeakError {
    Unreachable,
    KeyRejected,
    /// The voice is no longer on the server.
    VoiceGone,
    /// The voice sounds different from when the audiobook was made.
    VoiceChanged,
    /// Busy or loading: nothing was made; try again after this many seconds.
    Busy(u64),
    /// The server refused this text; carries its fixed error code only.
    Refused(String),
    Failed(String),
}

pub struct Speech {
    pub pcm: Vec<u8>,
    pub segments: Vec<crate::audio::Segment>,
}

fn keyed(rb: reqwest::RequestBuilder, key: Option<&str>) -> reqwest::RequestBuilder {
    match key.filter(|k| !k.is_empty()) {
        Some(k) => rb.bearer_auth(k),
        None => rb,
    }
}

/// The voice as the server has it now: its revision and seed. Checked right
/// before speaking, so audio is never made with a voice that changed.
pub async fn live_voice(
    base_url: &str,
    key: Option<&str>,
    voice: &str,
) -> Result<(String, i64), SpeakError> {
    let http = client();
    let r = keyed(http.get(format!("{base_url}/v1/voices/{voice}")), key)
        .send()
        .await
        .map_err(|_| SpeakError::Unreachable)?;
    match r.status() {
        StatusCode::NOT_FOUND => return Err(SpeakError::VoiceGone),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Err(SpeakError::KeyRejected),
        s if !s.is_success() => return Err(SpeakError::Unreachable),
        _ => {}
    }
    let v: Value = r
        .json()
        .await
        .map_err(|_| SpeakError::Failed("The voice record was unreadable.".into()))?;
    if v.get("kind").and_then(Value::as_str) != Some("cloned") {
        return Err(SpeakError::VoiceChanged);
    }
    let reference = keyed(
        http.get(format!("{base_url}/v1/voices/{voice}/reference")),
        key,
    )
    .send()
    .await
    .map_err(|_| SpeakError::Unreachable)?;
    if !reference.status().is_success() {
        return Err(SpeakError::VoiceChanged);
    }
    let sha = hex(Sha256::digest(
        &reference
            .bytes()
            .await
            .map_err(|_| SpeakError::Unreachable)?,
    )
    .as_slice());
    let seed = v
        .get("settings")
        .and_then(|s| s.get("seed"))
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_SEED);
    Ok((revision(&v, &sha), seed))
}

/// Speak `text` (one request). Streams, so dropping this future closes the
/// connection, which stops the server's work. Free.
pub async fn speak(
    base_url: &str,
    key: Option<&str>,
    voice: &str,
    seed: i64,
    text: &str,
) -> Result<Speech, SpeakError> {
    use base64::Engine;
    use futures_util::StreamExt;
    let http = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(120))
        .build()
        .expect("http client");
    let body = json!({
        "model": MODEL, "input": text, "voice": voice,
        "settings": { "seed": seed },
        // Sent explicitly so a server default change cannot silently change the sound.
        "segmentation": { "mode": "auto", "max_chars": 300, "sentence_pause_ms": 120, "paragraph_pause_ms": 500 },
        "speed": 1.0, "output_format": "pcm_24000", "stream_format": "sse",
    });
    let resp = keyed(
        http.post(format!("{base_url}/v1/speech/stream"))
            .json(&body),
        key,
    )
    .header("accept", "text/event-stream")
    .send()
    .await
    .map_err(|_| SpeakError::Unreachable)?;
    let status = resp.status();
    if status == StatusCode::SERVICE_UNAVAILABLE {
        let wait = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(5);
        return Err(SpeakError::Busy(wait.clamp(1, 90)));
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(SpeakError::KeyRejected);
    }
    if !status.is_success() {
        // The server's message can echo the text; keep only its fixed code.
        let code = resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v["error"]["code"].as_str().map(str::to_string));
        let code = code
            .filter(|c| c.len() <= 40 && c.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
            .unwrap_or_else(|| format!("http_{}", status.as_u16()));
        return Err(SpeakError::Refused(code));
    }
    let (mut pcm, mut segments, mut done) = (Vec::<u8>::new(), Vec::new(), false);
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk.map_err(|_| SpeakError::Unreachable)?);
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim_end().strip_prefix("data:") else {
                continue;
            };
            let event: Value = serde_json::from_str(data.trim())
                .map_err(|_| SpeakError::Failed("An unreadable stream event.".into()))?;
            match event.get("type").and_then(Value::as_str) {
                Some("speech.audio.delta") => {
                    let raw = base64::engine::general_purpose::STANDARD
                        .decode(event.get("audio").and_then(Value::as_str).unwrap_or(""))
                        .map_err(|_| SpeakError::Failed("Invalid audio data.".into()))?;
                    pcm.extend_from_slice(&raw);
                    if pcm.len() > MAX_PCM_BYTES {
                        return Err(SpeakError::Failed("Too much audio for one request.".into()));
                    }
                }
                Some("speech.segment") => {
                    let s = &event["segment"];
                    if let (Some(a), Some(b), Some(c), Some(d)) = (
                        s["char_start"].as_i64(),
                        s["char_end"].as_i64(),
                        s["start_ms"].as_i64(),
                        s["end_ms"].as_i64(),
                    ) {
                        segments.push(crate::audio::Segment {
                            char_start: a,
                            char_end: b,
                            start_ms: c,
                            end_ms: d,
                        });
                    }
                }
                Some("speech.audio.done") => done = true,
                Some("error") => {
                    let code = event["error"]["code"].as_str().unwrap_or("error");
                    let code: String = code
                        .chars()
                        .filter(|c| c.is_ascii_lowercase() || *c == '_')
                        .take(40)
                        .collect();
                    return Err(SpeakError::Refused(code));
                }
                _ => {} // unknown events are ignored, as the server's compatibility policy asks
            }
        }
        if done {
            break;
        }
    }
    if !done || pcm.is_empty() {
        return Err(SpeakError::Failed(
            "The stream ended before the audio was complete.".into(),
        ));
    }
    Ok(Speech { pcm, segments })
}
