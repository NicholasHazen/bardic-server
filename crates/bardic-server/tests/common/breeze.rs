//! A fake Breeze server: the endpoints Bardic uses, with switches for failure modes.
#![allow(dead_code)]
use axum::{
    body::Body,
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
    /// Requests admitted to speech, counted before any response delay.
    pub received: usize,
    pub received_texts: Vec<String>,
    pub active: usize,
    pub max_active: usize,
    /// Answer the speech endpoint with 503 this many times first.
    pub busy_left: u32,
    /// Refuse speech with this error code.
    pub refuse: Option<String>,
    /// Answer every voice and speech call with 503 (as if the server were down).
    pub down: bool,
    pub delay_ms: u64,
    pub tagged_delays: Vec<(String, u64)>,
    pub tagged_markers: Vec<(String, u8)>,
    /// Emit an SSE refusal after a matching gate is released.
    pub tagged_refusals: Vec<(String, String)>,
    /// A matching request waits for one credit, consumed when it starts producing audio.
    pub tagged_gates: Vec<(String, Arc<tokio::sync::Semaphore>)>,
}

pub type Shared = Arc<Mutex<Fake>>;

struct Active(Shared);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.lock().unwrap().active -= 1;
    }
}

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
    let text: String = body["input"].as_str().unwrap_or("").to_string();
    let (delay, marker, gate, stream_refusal, busy, refuse, down) = {
        let mut f = f.lock().unwrap();
        f.received += 1;
        f.received_texts.push(text.clone());
        f.active += 1;
        f.max_active = f.max_active.max(f.active);
        let busy = f.busy_left > 0;
        if busy {
            f.busy_left -= 1;
        }
        let delay = f
            .tagged_delays
            .iter()
            .find(|(tag, _)| text.contains(tag))
            .map_or(f.delay_ms, |(_, delay)| *delay);
        let marker = f
            .tagged_markers
            .iter()
            .find(|(tag, _)| text.contains(tag))
            .map(|(_, marker)| *marker);
        let gate = f
            .tagged_gates
            .iter()
            .find(|(tag, _)| text.contains(tag))
            .map(|(_, gate)| gate.clone());
        let stream_refusal = f
            .tagged_refusals
            .iter()
            .find(|(tag, _)| text.contains(tag))
            .map(|(_, code)| code.clone());
        (
            delay,
            marker,
            gate,
            stream_refusal,
            busy,
            f.refuse.clone(),
            f.down,
        )
    };
    let active = Active(f.clone());
    if delay > 0 && (down || busy || refuse.is_some()) {
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
    // A pending response body is dropped on client disconnect. Holding the guard in this stream
    // lets cancellation tests observe the actual provider flight instead of a detached handler.
    let stream = futures_util::stream::once(async move {
        let _active = active;
        if let Some(gate) = gate {
            gate.acquire_owned().await.unwrap().forget();
        }
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        if let Some(code) = stream_refusal {
            return Ok::<_, std::convert::Infallible>(format!(
                "data: {}\n\n",
                json!({ "type": "error", "error": { "code": code } })
            ));
        }
        let text: Vec<char> = text.chars().collect();
        f.lock().unwrap().spoken.push(text.iter().collect());
        let total_ms = text.len() as i64 * 10;
        // a constant tone-like level, so the audio is not silent
        let pcm: Vec<u8> = (0..(total_ms as usize) * 48)
            .map(|i| marker.unwrap_or(if i % 2 == 0 { 0x00 } else { 0x40 }))
            .collect();
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
        Ok::<_, std::convert::Infallible>(sse)
    });
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
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

    pub fn received(&self) -> usize {
        self.state.lock().unwrap().received
    }
}
