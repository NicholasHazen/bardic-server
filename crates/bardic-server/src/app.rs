use crate::{
    api,
    clock::{ts, Clock},
    config::Config,
    error::ApiError,
    events::{EventBus, Notice},
    lock::{InstanceLock, LockError},
    store::{Store, StoreError},
};
use axum::{
    extract::{DefaultBodyLimit, FromRequestParts, Request, State},
    http::{request::Parts, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Router,
};
use rusqlite::params;
use serde::Serialize;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch},
    task::JoinHandle,
};
use tower_http::trace::TraceLayer;

/// Shared by every request.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub clock: Arc<dyn Clock>,
    pub events: Arc<EventBus>,
    pub config: Arc<Config>,
    /// Flips to true on shutdown so long-lived streams end and do not block it.
    pub shutdown: Arc<watch::Sender<bool>>,
    /// Import ids whose cancellation was requested.
    pub cancelled_imports: Arc<Mutex<std::collections::HashSet<String>>>,
    ids: Arc<Mutex<ulid::Generator>>,
}

impl AppState {
    pub fn new(config: Config, clock: Arc<dyn Clock>) -> Result<Self, StoreError> {
        let store = Store::open(&config.data_dir)?;
        let default_name = config
            .server_name
            .clone()
            .or_else(|| std::env::var("HOSTNAME").ok())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "Bardic".to_string());
        let server_id = ulid::Ulid::new().to_string();
        store.run_blocking(|c| {
            c.execute(
                "INSERT OR IGNORE INTO meta(key,value) VALUES('server_id',?1)",
                [&server_id],
            )?;
            c.execute(
                "INSERT OR IGNORE INTO meta(key,value) VALUES('server_name',?1)",
                [&default_name],
            )?;
            Ok(())
        })?;
        // An import that was running when the server stopped did not finish: fail it
        // and drop the half-made book, so nothing half-added is ever left behind.
        store.run_blocking(|c| {
            c.execute(
                "UPDATE imports SET state='failed', error_code='import_interrupted', error_detail='Bardic stopped while adding this book. Add it again.' WHERE state NOT IN ('done','failed','cancelled')",
                [],
            )?;
            c.execute("DELETE FROM books WHERE state='adding'", [])?;
            Ok(())
        })?;
        Ok(AppState {
            store,
            clock,
            events: Arc::new(EventBus::new()),
            config: Arc::new(config),
            shutdown: Arc::new(watch::channel(false).0),
            cancelled_imports: Arc::new(Mutex::new(Default::default())),
            ids: Arc::new(Mutex::new(ulid::Generator::new())),
        })
    }

    /// A new opaque, sortable identifier.
    pub fn new_id(&self) -> String {
        let mut g = self.ids.lock().expect("id generator");
        g.generate()
            .unwrap_or_else(|_| ulid::Ulid::new())
            .to_string()
    }

    pub fn now(&self) -> String {
        ts(self.clock.now())
    }

    pub fn notify(&self, notice: Notice) {
        self.events.publish(notice);
    }
}

/// The device acting, set by the device layer from `X-Bardic-Device`.
#[derive(Debug, Clone)]
pub struct DeviceCtx {
    pub id: String,
    pub name: String,
}

impl<S: Send + Sync> FromRequestParts<S> for DeviceCtx {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        parts
            .extensions
            .get::<DeviceCtx>()
            .cloned()
            .ok_or_else(device_required)
    }
}

/// The listener acting, from `X-Bardic-Listener`. Extracting it checks that the
/// listener exists: a missing header is 400 `listener_required`, an unknown id is
/// 404 `listener_not_found`.
#[derive(Debug, Clone)]
pub struct ListenerCtx {
    pub id: String,
    pub name: String,
}

impl FromRequestParts<AppState> for ListenerCtx {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let id = parts
            .headers
            .get("x-bardic-listener")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| {
                ApiError::invalid(
                    "listener_required",
                    "Send X-Bardic-Listener (the id of the listener acting).",
                )
            })?;
        let lookup = id.clone();
        let name = state
            .store
            .run(move |c| {
                use rusqlite::OptionalExtension;
                Ok(
                    c.query_row("SELECT name FROM listeners WHERE id=?1", [&lookup], |r| {
                        r.get::<_, String>(0)
                    })
                    .optional()?,
                )
            })
            .await?;
        match name {
            Some(name) => Ok(ListenerCtx { id, name }),
            None => Err(ApiError::not_found(
                "listener_not_found",
                "No listener has this id.",
            )),
        }
    }
}

fn device_required() -> ApiError {
    ApiError::invalid(
        "device_required",
        "Send X-Bardic-Device (a client-generated device id) on every request that changes something.",
    )
}

/// Who acted, for the audit log and change records.
#[derive(Debug, Clone, Serialize)]
pub struct Actor {
    pub listener_id: Option<String>,
    pub listener_name: Option<String>,
    pub device_id: String,
    pub device_name: String,
}

impl Actor {
    pub fn with_listener(d: &DeviceCtx, l: &ListenerCtx) -> Self {
        Actor {
            listener_id: Some(l.id.clone()),
            listener_name: Some(l.name.clone()),
            device_id: d.id.clone(),
            device_name: d.name.clone(),
        }
    }

    pub fn device_only(d: &DeviceCtx) -> Self {
        Actor {
            listener_id: None,
            listener_name: None,
            device_id: d.id.clone(),
            device_name: d.name.clone(),
        }
    }
}

fn valid_device_id(s: &str) -> bool {
    (8..=80).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Registers the device on first sight, keeps `last_seen_at` fresh (at most every
/// 30 seconds), and requires the header on anything that changes state.
async fn device_layer(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let mutating = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    match req.headers().get("x-bardic-device") {
        None if mutating => return device_required().into_response(),
        None => {}
        Some(v) => {
            let id = match v.to_str() {
                Ok(s) if valid_device_id(s) => s.to_string(),
                _ => {
                    return ApiError::invalid(
                        "invalid_request",
                        "X-Bardic-Device must be 8 to 80 letters, digits, - or _.",
                    )
                    .into_response()
                }
            };
            match register_device(&state, id).await {
                Ok(ctx) => {
                    req.extensions_mut().insert(ctx);
                }
                Err(e) => return e.into_response(),
            }
        }
    }
    next.run(req).await
}

async fn register_device(state: &AppState, id: String) -> Result<DeviceCtx, ApiError> {
    let now = state.clock.now();
    let (now_s, stale_s) = (ts(now), ts(now - chrono::Duration::seconds(30)));
    let (ctx, created) = state
        .store
        .run(move |c| {
            let created = c.execute(
                "INSERT OR IGNORE INTO devices(id,name,first_seen_at,last_seen_at) VALUES(?1,'New device',?2,?2)",
                params![id, now_s],
            )? == 1;
            if !created {
                c.execute(
                    "UPDATE devices SET last_seen_at=?2 WHERE id=?1 AND last_seen_at < ?3",
                    params![id, now_s, stale_s],
                )?;
            }
            let name: String = c.query_row("SELECT name FROM devices WHERE id=?1", [&id], |r| r.get(0))?;
            Ok((DeviceCtx { id, name }, created))
        })
        .await?;
    if created {
        state.notify(Notice::new("device.updated", state.now()).with_id(ctx.id.clone()));
    }
    Ok(ctx)
}

/// Browser origin rules. Requests without `Origin` (curl, scripts) pass. A browser
/// request from an origin that is neither this server's own address nor in the
/// allow-list may not change anything (403 `origin_not_allowed`) and is given no
/// CORS headers, so the page cannot read the answer either.
async fn origin_layer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let origin = req
        .headers()
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let Some(origin) = origin else {
        return next.run(req).await;
    };
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let same_origin = origin
        .split_once("://")
        .map(|(_, h)| h == host)
        .unwrap_or(false);
    let listed = state
        .config
        .allow_origins
        .iter()
        .any(|o| o.trim_end_matches('/') == origin);
    let allowed = listed || same_origin;
    let mutating = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if req.method() == Method::OPTIONS && allowed {
        let mut r = StatusCode::NO_CONTENT.into_response();
        add_cors(r.headers_mut(), &origin, true);
        return r;
    }
    if !allowed && mutating {
        return ApiError::new(StatusCode::FORBIDDEN, "origin_not_allowed", "This page's address is not allowed to change anything on this server. Add it with --allow-origin.")
            .into_response();
    }
    let mut resp = next.run(req).await;
    if allowed {
        add_cors(resp.headers_mut(), &origin, false);
    }
    resp
}

fn add_cors(h: &mut axum::http::HeaderMap, origin: &str, preflight: bool) {
    use axum::http::{header::HeaderName, HeaderValue};
    if let Ok(v) = HeaderValue::from_str(origin) {
        h.insert(HeaderName::from_static("access-control-allow-origin"), v);
    }
    h.append(axum::http::header::VARY, HeaderValue::from_static("Origin"));
    if preflight {
        h.insert(
            HeaderName::from_static("access-control-allow-methods"),
            HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
        );
        h.insert(
            HeaderName::from_static("access-control-allow-headers"),
            HeaderValue::from_static(
                "content-type, x-bardic-device, x-bardic-listener, idempotency-key, last-event-id",
            ),
        );
        h.insert(
            HeaderName::from_static("access-control-max-age"),
            HeaderValue::from_static("600"),
        );
    }
}

async fn route_not_found() -> ApiError {
    ApiError::not_found("route_not_found", "There is no such operation.")
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "That method is not supported here.",
    )
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(api::system::health))
        .route(
            "/api/server",
            get(api::system::get_server).patch(api::system::update_server),
        )
        .route("/api/events", get(api::system::stream_events))
        .route("/api/devices", get(api::system::list_devices))
        .route(
            "/api/devices/{device_id}",
            patch(api::system::update_device),
        )
        .route("/api/audit", get(api::audit::list_audit))
        .route("/api/books", get(api::books::list))
        .route("/api/books/duplicates", get(api::books::duplicates))
        .route("/api/books/sample", post(api::imports::sample))
        .route(
            "/api/books/{book_id}",
            get(api::books::get_one).patch(api::books::update),
        )
        .route("/api/books/{book_id}/cover", get(api::books::cover))
        .route(
            "/api/books/{book_id}/cover/refresh",
            post(api::books::refresh_cover),
        )
        .route("/api/books/{book_id}/remove", post(api::books::remove))
        .route("/api/books/{book_id}/restore", post(api::books::restore))
        .route("/api/books/{book_id}/chapters", get(api::books::chapters))
        .route(
            "/api/books/{book_id}/chapters/{chapter_id}/text",
            get(api::books::chapter_text),
        )
        .route("/api/books/{book_id}/search", get(api::books::search))
        .route("/api/series", get(api::books::series))
        .route(
            "/api/imports",
            post(api::imports::create).layer(DefaultBodyLimit::max(
                state.config.max_upload_bytes as usize + 64 * 1024,
            )),
        )
        .route(
            "/api/imports/{import_id}",
            get(api::imports::get_one).delete(api::imports::cancel),
        )
        .route(
            "/api/listeners",
            get(api::listeners::list).post(api::listeners::create),
        )
        .route(
            "/api/listeners/{listener_id}",
            get(api::listeners::get_one)
                .patch(api::listeners::rename)
                .delete(api::listeners::delete),
        )
        .route(
            "/api/listeners/{listener_id}/impact",
            get(api::listeners::impact),
        )
        .route(
            "/api/listeners/{listener_id}/settings",
            get(api::listeners::get_settings).put(api::listeners::put_settings),
        )
        .fallback(route_not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(state.clone(), device_layer))
        .layer(middleware::from_fn_with_state(state.clone(), origin_layer))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// A running server: what `main` and the integration tests use.
pub struct Running {
    pub addr: SocketAddr,
    pub state: AppState,
    _lock: InstanceLock,
    shutdown: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot listen: {0}")]
    Bind(#[from] std::io::Error),
}

pub async fn spawn(config: Config, clock: Arc<dyn Clock>) -> Result<Running, StartError> {
    let lock = InstanceLock::acquire(&config.data_dir)?;
    let bind = config.bind;
    let state = AppState::new(config, clock)?;
    let listener = TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    let (tx, rx) = oneshot::channel::<()>();
    let app = router(state.clone());
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
    });
    Ok(Running {
        addr,
        state,
        _lock: lock,
        shutdown: Some(tx),
        handle,
    })
}

impl Running {
    pub async fn stop(mut self) {
        let _ = self.state.shutdown.send(true);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        // Streams end on the flag; the timeout is a backstop for stuck connections.
        if tokio::time::timeout(std::time::Duration::from_secs(3), &mut self.handle)
            .await
            .is_err()
        {
            self.handle.abort();
        }
    }
}
