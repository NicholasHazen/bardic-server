//! A fake Gemini API: the calls Bardic makes, with switches for failure modes.
#![allow(dead_code)]
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Fake {
    pub key: String,
}

pub type Shared = Arc<Mutex<Fake>>;

async fn models(State(f): State<Shared>, h: HeaderMap) -> Response {
    let ok = h.get("x-goog-api-key").and_then(|v| v.to_str().ok())
        == Some(f.lock().unwrap().key.as_str());
    if !ok {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "message": "API key not valid" } })),
        )
            .into_response();
    }
    Json(json!({ "models": [{ "name": "models/gemini-3.8-flash-tts" }] })).into_response()
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
        }));
        let app = Router::new()
            .route("/v1beta/models", get(models))
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
}

pub fn money(micros: i64) -> Value {
    json!({ "micros": micros, "currency": "USD" })
}
