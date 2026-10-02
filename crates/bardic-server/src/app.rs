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
    extract::{DefaultBodyLimit, FromRequestParts, MatchedPath, Request, State},
    http::{request::Parts, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
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
    pub jobs: Arc<crate::jobs::JobSignal>,
    /// Concurrent sample misses share one request and its settled result.
    pub samples: Arc<crate::samples::SampleFlights>,
    /// Limits on work a client can start, so a burst cannot exhaust the computer.
    pub gates: Arc<Gates>,
    ids: Arc<Mutex<ulid::Generator>>,
}

/// Most imports processed at once; the rest stay `queued` and start in turn.
pub const MAX_IMPORTS_AT_ONCE: usize = 3;
/// Most open event streams (a household has a few devices).
pub const MAX_EVENT_STREAMS: usize = 64;

pub struct Gates {
    pub imports: tokio::sync::Semaphore,
    /// One ffmpeg at a time.
    pub exports: tokio::sync::Semaphore,
    pub streams: std::sync::atomic::AtomicUsize,
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
        // Books added before covers were generated get one now.
        store.run_blocking(crate::api::books::backfill_generated_covers)?;
        store.reconcile_audio_startup(&ts(clock.now()))?;
        Ok(AppState {
            store,
            clock,
            events: Arc::new(EventBus::new()),
            config: Arc::new(config),
            shutdown: Arc::new(watch::channel(false).0),
            cancelled_imports: Arc::new(Mutex::new(Default::default())),
            jobs: Arc::new(crate::jobs::JobSignal::new()),
            samples: Arc::new(crate::samples::SampleFlights::default()),
            gates: Arc::new(Gates {
                imports: tokio::sync::Semaphore::new(MAX_IMPORTS_AT_ONCE),
                exports: tokio::sync::Semaphore::new(1),
                streams: std::sync::atomic::AtomicUsize::new(0),
            }),
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

/// The listener when `X-Bardic-Listener` is present (then it must be valid), else none.
/// For operations that return a book's per-listener part but do not require it.
pub struct MaybeListener(pub Option<ListenerCtx>);

impl FromRequestParts<AppState> for MaybeListener {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if parts.headers.contains_key("x-bardic-listener") {
            Ok(MaybeListener(Some(
                ListenerCtx::from_request_parts(parts, state).await?,
            )))
        } else {
            Ok(MaybeListener(None))
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

/// Browser origin rules apply to writes and voice-sample GET/HEAD, since even
/// HEAD uses the sample handler and an uncached premium sample can spend money.
/// Media/no-cors requests can omit Origin: Referer and Fetch Metadata identify
/// those browser requests. Samples without provenance require the existing
/// device header: a browser's no-cors request cannot attach that header.
async fn origin_layer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    // DNS rebinding: a page on a public name that was pointed at this machine is "same origin"
    // by the rule below, so the name the request arrived by must itself be one we expect.
    let host_header = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !host_allowed(host_header, &state.config) {
        return ApiError::new(StatusCode::FORBIDDEN, "host_not_allowed", "This server is not reachable by that name. Use its address, or add the name with --allow-host.").into_response();
    }
    let origin = req
        .headers()
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let allowed = browser_page_allowed(req.headers(), host_header, &state.config);
    let mutating = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let sample = matches!(*req.method(), Method::GET | Method::HEAD)
        && req
            .extensions()
            .get::<MatchedPath>()
            .is_some_and(|path| path.as_str() == "/api/voices/{voice_id}/sample");
    let ambiguous_sample = sample
        && !req.headers().contains_key("x-bardic-device")
        && !["origin", "referer", "sec-fetch-site"]
            .iter()
            .any(|name| req.headers().contains_key(*name));
    if req.method() == Method::OPTIONS && allowed {
        if let Some(origin) = origin.as_deref() {
            let mut r = StatusCode::NO_CONTENT.into_response();
            add_cors(r.headers_mut(), origin, true);
            return r;
        }
    }
    if (!allowed && (mutating || sample)) || ambiguous_sample {
        let detail = if ambiguous_sample {
            "Voice samples need trusted browser provenance or X-Bardic-Device. Scripts can send their existing device id."
        } else {
            "This page's address is not allowed to change anything on this server. Add it with --allow-origin."
        };
        let mut resp =
            ApiError::new(StatusCode::FORBIDDEN, "origin_not_allowed", detail).into_response();
        if sample {
            add_sample_vary(resp.headers_mut());
        }
        return resp;
    }
    let mut resp = next.run(req).await;
    if sample {
        add_sample_vary(resp.headers_mut());
    }
    if allowed {
        if let Some(origin) = origin.as_deref() {
            add_cors(resp.headers_mut(), origin, false);
        }
    }
    resp
}

fn add_sample_vary(headers: &mut axum::http::HeaderMap) {
    // A cache must not reuse an allowed sample or a refusal for a request with
    // different browser provenance. These are all inputs to the guard below.
    headers.append(
        axum::http::header::VARY,
        axum::http::HeaderValue::from_static(
            "Origin, Referer, Sec-Fetch-Site, Sec-Fetch-Mode, Sec-Fetch-Dest, Sec-Fetch-User, X-Bardic-Device",
        ),
    );
}

fn page_origin_allowed(origin: &str, host: &str, config: &Config) -> bool {
    let Ok(page) = reqwest::Url::parse(origin) else {
        return false;
    };
    if !matches!(page.scheme(), "http" | "https")
        || !page.username().is_empty()
        || page.password().is_some()
        || page.path() != "/"
        || page.query().is_some()
        || page.fragment().is_some()
    {
        return false;
    }
    let same_address =
        reqwest::Url::parse(&format!("{}://{host}", page.scheme())).is_ok_and(|server| {
            page.host_str() == server.host_str()
                && page.port_or_known_default() == server.port_or_known_default()
        });
    same_address
        || config.allow_origins.iter().any(|listed| {
            reqwest::Url::parse(listed).is_ok_and(|url| url.origin() == page.origin())
        })
}

fn browser_page_allowed(headers: &axum::http::HeaderMap, host: &str, config: &Config) -> bool {
    // A present opaque/malformed Origin must not fall through to the CLI rule.
    if let Some(origin) = headers.get(axum::http::header::ORIGIN) {
        return origin
            .to_str()
            .is_ok_and(|origin| page_origin_allowed(origin, host, config));
    }
    let site = headers.get("sec-fetch-site");
    match site.map(|site| site.to_str()) {
        // Fetch Metadata describes the browser-visible address. A same-origin
        // reverse proxy can legitimately forward a Referer with another Host.
        Some(Ok("same-origin" | "none")) => return true,
        Some(Ok("cross-site" | "same-site")) | None => {}
        Some(_) => return false,
    }
    if let Some(referer) = headers.get(axum::http::header::REFERER) {
        return referer
            .to_str()
            .ok()
            .and_then(|referer| reqwest::Url::parse(referer).ok())
            .is_some_and(|page| {
                page_origin_allowed(&page.origin().ascii_serialization(), host, config)
            });
    }
    match site {
        // A no-referrer policy must not bypass a known foreign browser request.
        Some(_) => false,
        // Node's fetch sends Mode without Site. A script can identify its
        // device; browser no-cors cannot send this header, while cross-origin
        // CORS sends Origin and goes through the check above. The device layer
        // validates the id before any handler can execute.
        None => {
            headers.contains_key("x-bardic-device")
                || !["sec-fetch-mode", "sec-fetch-dest", "sec-fetch-user"]
                    .iter()
                    .any(|name| headers.contains_key(*name))
        }
    }
}

/// The host part of a `Host` header: no port, no brackets, lower case, no trailing dot.
fn host_name(h: &str) -> String {
    let h = h.trim();
    let name = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        h.rsplit_once(':').map_or(h, |(n, p)| {
            if p.chars().all(|c| c.is_ascii_digit()) {
                n
            } else {
                h
            }
        })
    };
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Names a visitor on the owner's own network uses. Anything with a public suffix is a name an
/// outside page could have rebound to this machine, so it must be listed.
fn host_allowed(header: &str, config: &crate::config::Config) -> bool {
    let name = host_name(header);
    if name.is_empty() {
        // HTTP/1.0 or a bare client: nothing to rebind.
        return true;
    }
    if name.parse::<std::net::IpAddr>().is_ok() || name == "localhost" || !name.contains('.') {
        return true;
    }
    if [".local", ".lan", ".home.arpa", ".ts.net", ".localhost"]
        .iter()
        .any(|s| name.ends_with(s))
    {
        return true;
    }
    config.allow_hosts.iter().any(|h| host_name(h) == name)
        || config.allow_origins.iter().any(|o| {
            o.split_once("://")
                .is_some_and(|(_, h)| host_name(h.trim_end_matches('/')) == name)
        })
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
            HeaderValue::from_static("GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"),
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
        .route(
            "/api/books/{book_id}/place",
            get(api::places::get)
                .put(api::places::put)
                .delete(api::places::clear),
        )
        .route(
            "/api/books/{book_id}/place/finished",
            put(api::places::set_finished),
        )
        .route(
            "/api/books/{book_id}/place/history",
            get(api::places::history),
        )
        .route("/api/series", get(api::books::series))
        .route("/api/voice-sources", get(api::voices::list))
        .route(
            "/api/voice-sources/{source_id}",
            get(api::voices::get_one)
                .put(api::voices::configure)
                .delete(api::voices::remove),
        )
        .route(
            "/api/voice-sources/{source_id}/test",
            post(api::voices::test),
        )
        .route(
            "/api/voice-sources/{source_id}/refresh",
            post(api::voices::refresh),
        )
        .route(
            "/api/audiobooks/{audiobook_id}/plan-preview",
            post(api::plans::preview),
        )
        .route("/api/plans", get(api::plans::list).post(api::plans::create))
        .route("/api/plans/{plan_id}", get(api::plans::get_one))
        .route("/api/plans/{plan_id}/pause", post(api::plans::pause))
        .route("/api/plans/{plan_id}/resume", post(api::plans::resume))
        .route("/api/plans/{plan_id}/stop", post(api::plans::stop))
        .route(
            "/api/audiobooks/{audiobook_id}/space",
            get(api::space::get_space).delete(api::space::free_space),
        )
        .route(
            "/api/audiobooks/{audiobook_id}/manifest",
            get(api::space::manifest),
        )
        .route(
            "/api/audiobooks/{audiobook_id}/sync-check",
            post(api::space::sync_check),
        )
        .route(
            "/api/books/{book_id}/deletion",
            get(api::admin::get_deletion)
                .post(api::admin::schedule)
                .delete(api::admin::cancel),
        )
        .route(
            "/api/backups",
            get(api::admin::list_backups).post(api::admin::create_backup),
        )
        .route(
            "/api/audiobooks/{audiobook_id}/exports",
            post(api::exports::create),
        )
        .route("/api/exports/{export_id}", get(api::exports::get_one))
        .route("/api/exports/{export_id}/file", get(api::exports::download))
        .route(
            "/api/allowance",
            get(api::money::get_allowance).put(api::money::put_allowance),
        )
        .route(
            "/api/prices",
            get(api::money::list_prices).post(api::money::refresh_prices),
        )
        .route("/api/prices/{provider}", put(api::money::put_price))
        .route("/api/voices", get(api::voices::list_voices))
        .route(
            "/api/voices/{voice_id}/sample",
            get(api::audio::voice_sample),
        )
        .route(
            "/api/audiobooks/{audiobook_id}/chapters/{chapter_id}/request",
            post(api::audio::request_chapter),
        )
        .route(
            "/api/audiobooks/{audiobook_id}/make-ready",
            post(api::audio::make_ready),
        )
        .route("/api/audio/{audio_id}", get(api::audio::get_audio))
        .route(
            "/api/audio/{audio_id}/timings",
            get(api::audio::get_timings),
        )
        .route("/api/jobs", get(api::audio::list_jobs))
        .route("/api/jobs/{job_id}", get(api::audio::get_job))
        .route("/api/jobs/{job_id}/pause", post(api::audio::pause_job))
        .route("/api/jobs/{job_id}/resume", post(api::audio::resume_job))
        .route("/api/jobs/{job_id}/cancel", post(api::audio::cancel_job))
        .route(
            "/api/books/{book_id}/audiobooks",
            get(api::audiobooks::list).post(api::audiobooks::create),
        )
        .route(
            "/api/audiobooks/{audiobook_id}",
            get(api::audiobooks::get_one),
        )
        .route(
            "/api/audiobooks/{audiobook_id}/chapters",
            get(api::audiobooks::chapters),
        )
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
    worker: JoinHandle<()>,
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
    let worker = tokio::spawn(crate::jobs::run_worker(state.clone()));
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
        worker,
    })
}

impl Running {
    pub async fn stop(mut self) {
        let _ = self.state.shutdown.send(true);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        // Detached sample workers must settle spending and finish their file
        // commits before this instance releases the data-folder lock.
        self.state.samples.shutdown().await;
        // Streams end on the flag; the timeout is a backstop for stuck connections.
        if tokio::time::timeout(std::time::Duration::from_secs(3), &mut self.handle)
            .await
            .is_err()
        {
            self.handle.abort();
        }
        if tokio::time::timeout(std::time::Duration::from_secs(3), &mut self.worker)
            .await
            .is_err()
        {
            self.worker.abort();
        }
    }
}
