//! A fake Breeze server: the endpoints Bardic uses, with switches for failure modes.
#![allow(dead_code)]
use axum::{
    extract::{Path, State},
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
    pub key: Option<String>,
    pub voices: Vec<Value>,
    pub reference: Vec<u8>,
    /// Every text sent to the speech endpoint, in order.
    pub spoken: Vec<String>,
    /// Answer the speech endpoint with 503 this many times first.
    pub busy_left: u32,
    /// Refuse speech with this error code.
    pub refuse: Option<String>,
    /// Answer every voice and speech call with 503 (as if the server were down).
    pub down: bool,
    pub delay_ms: u64,
}

pub type Shared = Arc<Mutex<Fake>>;

fn authorized(f: &Fake, h: &HeaderMap) -> bool {
    match &f.key {
        None => true,
        Some(k) => {
            h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {k}"))
        }
    }
}

async fn list(State(f): State<Shared>, h: HeaderMap) -> Result<Json<Value>, StatusCode> {
    let f = f.lock().unwrap();
    if f.down {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    if !authorized(&f, &h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(json!({ "data": f.voices })))
}

async fn one(
    State(f): State<Shared>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let f = f.lock().unwrap();
    if f.down {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    if !authorized(&f, &h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    f.voices
        .iter()
        .find(|v| v["id"] == id.as_str())
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn reference(
    State(f): State<Shared>,
    h: HeaderMap,
    Path(_id): Path<String>,
) -> Result<Vec<u8>, StatusCode> {
    let f = f.lock().unwrap();
    if f.down {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    if !authorized(&f, &h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(f.reference.clone())
}

/// 10 ms of audio per character; one segment per sentence.
async fn speech(State(f): State<Shared>, Json(body): Json<Value>) -> Response {
    let (delay, busy, refuse, down) = {
        let mut f = f.lock().unwrap();
        let busy = f.busy_left > 0;
        if busy {
            f.busy_left -= 1;
        }
        (f.delay_ms, busy, f.refuse.clone(), f.down)
    };
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    if down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if busy {
        return (StatusCode::SERVICE_UNAVAILABLE, [("retry-after", "1")]).into_response();
    }
    if let Some(code) = refuse {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "code": code, "message": "echoes the text" } })),
        )
            .into_response();
    }
    let text: Vec<char> = body["input"].as_str().unwrap_or("").chars().collect();
    f.lock().unwrap().spoken.push(text.iter().collect());
    let total_ms = text.len() as i64 * 10;
    let pcm = vec![1u8; (total_ms as usize) * 48];
    let mut sse = String::new();
    let mut start = 0usize;
    let mut i = 0;
    while i < text.len() {
        if text[i] == '.' || i == text.len() - 1 {
            let end = i + 1;
            let seg: String = text[start..end].iter().collect();
            sse += &format!(
                "data: {}\n\n",
                json!({ "type": "speech.segment", "segment": { "text": seg, "char_start": start, "char_end": end, "start_ms": start as i64 * 10, "end_ms": end as i64 * 10 } })
            );
            start = end;
        }
        i += 1;
    }
    let half = pcm.len() / 2;
    for part in [&pcm[..half], &pcm[half..]] {
        sse += &format!(
            "data: {}\n\n",
            json!({ "type": "speech.audio.delta", "audio": base64::engine::general_purpose::STANDARD.encode(part) })
        );
    }
    sse += &format!(
        "data: {}\n\n",
        json!({ "type": "speech.audio.done", "duration_ms": total_ms, "output_format": "pcm_24000" })
    );
    ([("content-type", "text/event-stream")], sse).into_response()
}

pub struct FakeBreeze {
    pub url: String,
    pub state: Shared,
    task: tokio::task::JoinHandle<()>,
}

pub fn cloned(id: &str, name: &str) -> Value {
    json!({ "id": id, "kind": "cloned", "name": name, "labels": { "language": "en" }, "created_at": "2026-01-01T00:00:00Z", "settings": { "seed": 7 }, "reference": { "text": "hello" } })
}

impl FakeBreeze {
    pub async fn start() -> Self {
        let state: Shared = Arc::new(Mutex::new(Fake {
            reference: b"clip-one".to_vec(),
            voices: vec![
                json!({ "id": "mara", "kind": "cloned", "name": "Mara", "description": "Warm, unhurried", "labels": { "language": "en" }, "created_at": "2026-01-01T00:00:00Z", "settings": { "seed": 7 }, "reference": { "text": "hello" } }),
                json!({ "id": "tobias", "kind": "cloned", "name": "Tobias", "created_at": "2026-01-02T00:00:00Z" }),
                json!({ "id": "sketch", "kind": "designed", "name": "Sketch" }),
            ],
            ..Default::default()
        }));
        let app = Router::new()
            .route(
                "/health",
                get(|| async { Json(json!({ "status": "ok", "model": "breeze-tts-2" })) }),
            )
            .route("/v1/voices", get(list))
            .route("/v1/voices/{id}", get(one))
            .route("/v1/voices/{id}/reference", get(reference))
            .route("/v1/speech/stream", post(speech))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        FakeBreeze { url, state, task }
    }

    pub fn stop(&self) {
        self.task.abort();
    }

    pub fn spoken(&self) -> Vec<String> {
        self.state.lock().unwrap().spoken.clone()
    }
}
