//! A fake Gemini API: the calls Bardic makes, with switches for failure modes.
#![allow(dead_code)]
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::Engine;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Fake {
    pub key: String,
    /// The text of every speech request that was billed or answered.
    pub spoken: Vec<String>,
    /// Count of every speech request received, answered or not.
    pub received: usize,
    pub active: usize,
    pub max_active: usize,
    pub gate: Option<Arc<tokio::sync::Semaphore>>,
    pub delay_ms: u64,
    /// Answer the next speech requests with 429 and this retry-after (seconds).
    pub quota: Vec<u64>,
    /// Next responses: success with no usage report.
    pub no_usage: u32,
    /// Next responses: success with usage but no audio.
    pub no_audio: u32,
    /// Answer the next speech requests with these statuses (an empty JSON error body).
    pub statuses: Vec<u16>,
    /// Next responses: 200 with a body that is not JSON.
    pub garbage: u32,
    /// Next responses: 200 with audio that is not valid base64.
    pub bad_audio: u32,
    /// Audio tokens billed per spoken character (a voice speaks about 1.8).
    pub out_tokens_per_char: f64,
}

pub type Shared = Arc<Mutex<Fake>>;

struct Active(Shared);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.lock().unwrap().active -= 1;
    }
}

fn key_ok(f: &Fake, h: &HeaderMap) -> bool {
    h.get("x-goog-api-key").and_then(|v| v.to_str().ok()) == Some(f.key.as_str())
}

async fn models(State(f): State<Shared>, h: HeaderMap) -> Response {
    if !key_ok(&f.lock().unwrap(), &h) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "message": "API key not valid" } })),
        )
            .into_response();
    }
    Json(json!({ "models": [{ "name": "models/gemini-3.8-flash-tts" }] })).into_response()
}

/// 10 ms of audio per character.
async fn interactions(State(f): State<Shared>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    let (delay, valid, gate) = {
        let mut f = f.lock().unwrap();
        f.received += 1;
        f.active += 1;
        f.max_active = f.max_active.max(f.active);
        (f.delay_ms, key_ok(&f, &h), f.gate.clone())
    };
    let _active = Active(f.clone());
    if let Some(gate) = gate {
        gate.acquire_owned().await.unwrap().forget();
    }
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    if !valid {
        return StatusCode::FORBIDDEN.into_response();
    }
    let text = body["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let (status, garbage, bad_audio) = {
        let mut f = f.lock().unwrap();
        let s = (!f.statuses.is_empty()).then(|| f.statuses.remove(0));
        let g = s.is_none() && f.garbage > 0;
        let b = s.is_none() && !g && f.bad_audio > 0;
        f.garbage -= g as u32;
        f.bad_audio -= b as u32;
        (s, g, b)
    };
    if let Some(s) = status {
        return (
            StatusCode::from_u16(s).unwrap(),
            Json(json!({ "error": { "message": "refused by the fake" } })),
        )
            .into_response();
    }
    if garbage {
        return (StatusCode::OK, "<html>not json</html>").into_response();
    }
    if bad_audio {
        return Json(json!({ "steps": [{ "type": "model_output", "content": [{ "type": "audio", "mime_type": "audio/L16;codec=pcm;rate=24000", "data": "***not base64***" }] }] })).into_response();
    }
    let (quota, no_usage, no_audio, per_char) = {
        let mut f = f.lock().unwrap();
        let q = if f.quota.is_empty() {
            None
        } else {
            Some(f.quota.remove(0))
        };
        let (nu, na) = (f.no_usage > 0, f.no_audio > 0);
        if q.is_none() {
            if nu {
                f.no_usage -= 1;
            } else if na {
                f.no_audio -= 1;
            }
            f.spoken.push(text.clone());
        }
        (q, nu, na, f.out_tokens_per_char)
    };
    if let Some(secs) = quota {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", secs.to_string())],
            Json(json!({ "error": { "message": "quota" } })),
        )
            .into_response();
    }
    let chars = text.chars().count();
    let pcm = vec![1u8; chars * 10 * 48];
    let usage = json!({
        "total_input_tokens": (chars as f64 / 4.0).ceil() as i64 + 201,
        "total_output_tokens": (chars as f64 * per_char).round() as i64,
        "total_cached_tokens": 0,
        // the live API also reports about 200 audio input tokens on every request (not billed)
        "input_tokens_by_modality": [{ "modality": "audio", "tokens": 201 }, { "modality": "text", "tokens": (chars as f64 / 4.0).ceil() as i64 }],
        "output_tokens_by_modality": [{ "modality": "audio", "tokens": (chars as f64 * per_char).round() as i64 }],
    });
    let mut out = json!({ "steps": [] });
    if !no_audio {
        out["steps"] = json!([{ "type": "model_output", "content": [{ "type": "audio", "mime_type": "audio/L16;codec=pcm;rate=24000", "data": base64::engine::general_purpose::STANDARD.encode(&pcm) }] }]);
    }
    if !no_usage {
        out["usage"] = usage;
    }
    Json(out).into_response()
}

pub struct FakeGemini {
    pub url: String,
    pub state: Shared,
    task: tokio::task::JoinHandle<()>,
}

impl FakeGemini {
    pub async fn start(key: &str) -> Self {
        let state: Shared = Arc::new(Mutex::new(Fake {
            key: key.to_string(),
            out_tokens_per_char: 2.0,
            ..Default::default()
        }));
        let app = Router::new()
            .route("/v1beta/models", get(models))
            .route("/v1beta/interactions", post(interactions))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        FakeGemini { url, state, task }
    }

    pub fn stop(&self) {
        self.task.abort();
    }

    pub fn spoken(&self) -> Vec<String> {
        self.state.lock().unwrap().spoken.clone()
    }

    pub fn received(&self) -> usize {
        self.state.lock().unwrap().received
    }
}

pub fn money(micros: i64) -> Value {
    json!({ "micros": micros, "currency": "USD" })
}
