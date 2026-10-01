//! Making and delivering audio: requests, jobs, audio bytes with ranges, timings and voice samples.

use super::{audiobooks, audit::page_limit, ApiQuery};
use crate::{
    app::{Actor, AppState, DeviceCtx, ListenerCtx},
    error::ApiError,
    jobs,
    voices::breeze::{self, SpeakError},
};
use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const ACTIVE: &str = "('queued','running','waiting','paused','needs_you')";

/// What a request to make audio needs to know about the audiobook.
pub struct Target {
    pub book_id: String,
    pub tier: String,
    pub source_state: String,
    pub available: bool,
}

pub fn target(conn: &Connection, audiobook: &str) -> Result<Target, ApiError> {
    conn.query_row(
        "SELECT a.book_id,v.tier,s.state,v.available FROM audiobooks a JOIN voices v ON v.id=a.voice_id JOIN voice_sources s ON s.id=v.source_id WHERE a.id=?1",
        [audiobook],
        |r| Ok(Target { book_id: r.get(0)?, tier: r.get(1)?, source_state: r.get(2)?, available: r.get::<_, i64>(3)? != 0 }),
    )
    .optional()?
    .ok_or_else(|| ApiError::not_found("audiobook_not_found", "No audiobook has this id."))
}

/// No audio is made for a book that is scheduled for deletion: the work would be lost, or leave
/// files behind when the deletion runs.
pub fn require_not_deleting(conn: &Connection, book_id: &str) -> Result<(), ApiError> {
    let deleting: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM books WHERE id=?1 AND state='deleting')",
        [book_id],
        |r| r.get(0),
    )?;
    if deleting {
        return Err(ApiError::conflict(
            "deletion_pending",
            "This book is scheduled for deletion. Cancel the deletion before making more audio.",
        ));
    }
    Ok(())
}

/// Free audio needs a source that is connected now. Premium audio needs an approved plan.
fn require_ready_to_make(t: &Target) -> Result<(), ApiError> {
    if t.tier == "premium" {
        return Err(ApiError::conflict(
            "plan_required",
            "Premium audio is made only under an approved plan. Preview and approve a plan first.",
        ));
    }
    match (t.source_state.as_str(), t.available) {
        ("connected", true) => Ok(()),
        ("not_set_up", _) => Err(ApiError::conflict(
            "source_not_set_up",
            "This voice's source is not set up.",
        )),
        ("key_rejected", _) => Err(ApiError::conflict(
            "key_rejected",
            "The voice server rejected the API key.",
        )),
        _ => Err(ApiError::conflict(
            "source_unreachable",
            "The voice server could not be reached. Check that it is running, then try again.",
        )),
    }
}

fn chapter_index(conn: &Connection, book: &str, chapter: &str) -> Result<i64, ApiError> {
    conn.query_row(
        "SELECT idx FROM chapters WHERE id=?1 AND book_id=?2",
        [chapter, book],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| {
        ApiError::not_found(
            "chapter_not_found",
            "This book has no chapter with this id.",
        )
    })
}

pub fn has_audio(conn: &Connection, audiobook: &str, chapter: &str) -> Result<bool, ApiError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM audio WHERE audiobook_id=?1 AND chapter_id=?2 AND deleted_at IS NULL",
            [audiobook, chapter],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The audiobook's job that is (or will soon be) making chapters.
fn running_job(conn: &Connection, audiobook: &str) -> Result<Option<String>, ApiError> {
    Ok(conn
        .query_row(
            "SELECT id FROM jobs WHERE audiobook_id=?1 AND kind='make_audio' AND state IN ('queued','running','waiting') ORDER BY created_at DESC LIMIT 1",
            [audiobook],
            |r| r.get(0),
        )
        .optional()?)
}

struct NewJob<'a> {
    id: &'a str,
    audiobook: &'a str,
    book: &'a str,
    urgent: bool,
    actor: &'a Actor,
    key: Option<&'a str>,
    at: &'a str,
}

fn new_job(conn: &Connection, j: NewJob) -> Result<(), ApiError> {
    conn.execute(
        "INSERT INTO jobs(id,kind,state,audiobook_id,book_id,urgent,started_by,idempotency_key,created_at,updated_at) VALUES(?1,'make_audio','queued',?2,?3,?4,?5,?6,?7,?7)",
        params![j.id, j.audiobook, j.book, j.urgent as i64, serde_json::to_string(j.actor).expect("actor"), j.key, j.at],
    )?;
    Ok(())
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RequestIn {
    ahead: Option<i64>,
}

/// `requestChapterAudio`: press play.
pub async fn request_chapter(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: ListenerCtx,
    Path((audiobook, chapter)): Path<(String, String)>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: RequestIn = if body.iter().all(u8::is_ascii_whitespace) {
        RequestIn::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            ApiError::invalid(
                "invalid_request",
                format!("The request body is not valid: {e}"),
            )
        })?
    };
    let ahead = input.ahead.unwrap_or(1);
    if !(0..=5).contains(&ahead) {
        return Err(ApiError::invalid(
            "invalid_request",
            "ahead must be from 0 to 5.",
        ));
    }
    let (new_id, at, actor) = (
        state.new_id(),
        state.now(),
        Actor::with_listener(&device, &listener),
    );
    let out = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let t = target(&tx, &audiobook)?;
            let idx = chapter_index(&tx, &t.book_id, &chapter)?;
            if has_audio(&tx, &audiobook, &chapter)? {
                let states = audiobooks::chapter_states(&tx, &audiobook)?;
                let item = states["items"].as_array().and_then(|a| a.iter().find(|i| i["chapter_id"] == chapter.as_str())).cloned().unwrap_or(Value::Null);
                return Ok((StatusCode::OK, item, false));
            }
            if t.tier == "premium" {
                // Premium audio is made only under a running plan that still has this chapter to do.
                let job: Option<String> = tx
                    .query_row(
                        "SELECT j.id FROM jobs j JOIN job_items i ON i.job_id=j.id
                         WHERE j.audiobook_id=?1 AND j.plan_id IS NOT NULL AND j.state IN ('queued','running','waiting') AND i.chapter_id=?2 AND i.state='queued' LIMIT 1",
                        [&audiobook, &chapter],
                        |r| r.get(0),
                    )
                    .optional()?;
                let Some(j) = job else {
                    return Err(ApiError::conflict("plan_required", "Premium audio is made only under an approved plan. Preview and approve a plan first."));
                };
                let front: i64 = tx.query_row("SELECT COALESCE(MIN(position),1)-1 FROM job_items WHERE job_id=?1", [&j], |r| r.get(0))?;
                tx.execute("UPDATE job_items SET position=?2 WHERE job_id=?1 AND chapter_id=?3", params![j, front, chapter])?;
                tx.execute("UPDATE jobs SET urgent=1 WHERE id=?1", [&j])?;
                let v = jobs::job_value(&tx, &j)?;
                tx.commit()?;
                return Ok((StatusCode::ACCEPTED, v, true));
            }
            require_not_deleting(&tx, &t.book_id)?;
            require_ready_to_make(&t)?;
            // The chapter first, then the next few that are not made yet.
            let mut wanted = vec![chapter.clone()];
            let following: Vec<String> = tx
                .prepare("SELECT id FROM chapters WHERE book_id=?1 AND idx>?2 ORDER BY idx LIMIT ?3")?
                .query_map(params![t.book_id, idx, ahead], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for f in following {
                if !has_audio(&tx, &audiobook, &f)? {
                    wanted.push(f);
                }
            }
            let job_id = match running_job(&tx, &audiobook)? {
                Some(j) => {
                    // Join it: the chapter jumps to the front and the job becomes urgent.
                    let front: i64 = tx.query_row("SELECT COALESCE(MIN(position),1)-1 FROM job_items WHERE job_id=?1", [&j], |r| r.get(0))?;
                    tx.execute(
                        "INSERT INTO job_items(job_id,chapter_id,position,state) VALUES(?1,?2,?3,'queued')
                         ON CONFLICT(job_id,chapter_id) DO UPDATE SET position=?3, state='queued', detail=NULL WHERE state<>'done'",
                        params![j, chapter, front],
                    )?;
                    tx.execute("UPDATE jobs SET urgent=1 WHERE id=?1", [&j])?;
                    jobs::add_items(&tx, &j, &wanted[1..])?;
                    j
                }
                None => {
                    new_job(&tx, NewJob { id: &new_id, audiobook: &audiobook, book: &t.book_id, urgent: true, actor: &actor, key: None, at: &at })?;
                    jobs::add_items(&tx, &new_id, &wanted)?;
                    new_id
                }
            };
            jobs::recount(&tx, &job_id, &at)?;
            let v = jobs::job_value(&tx, &job_id)?;
            tx.commit()?;
            Ok((StatusCode::ACCEPTED, v, true))
        })
        .await?;
    if out.2 {
        jobs::announce(&state, out.1["id"].as_str().unwrap_or_default());
    }
    Ok((out.0, Json(out.1)).into_response())
}

#[derive(Deserialize, serde::Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ScopeIn {
    pub kind: String,
    pub from_chapter_id: Option<String>,
    pub chapter_ids: Option<Vec<String>>,
}

impl ScopeIn {
    /// The contract's `Scope`.
    pub fn value(&self) -> Value {
        let mut v = json!({ "kind": self.kind, "from_chapter_id": self.from_chapter_id });
        if let Some(c) = &self.chapter_ids {
            v["chapter_ids"] = json!(c);
        }
        v
    }
}

/// The chapters a scope names, in reading order.
pub fn resolve_scope(
    tx: &Connection,
    book_id: &str,
    scope: &ScopeIn,
) -> Result<Vec<String>, ApiError> {
    let ordered: Vec<(String, i64)> = tx
        .prepare("SELECT id,idx FROM chapters WHERE book_id=?1 ORDER BY idx")?
        .query_map([book_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(match scope.kind.as_str() {
        "whole_book" => ordered.iter().map(|(id, _)| id.clone()).collect(),
        "from_chapter" => {
            let from = scope.from_chapter_id.as_deref().ok_or_else(|| {
                ApiError::invalid("invalid_request", "from_chapter needs from_chapter_id.")
            })?;
            let idx = chapter_index(tx, book_id, from)?;
            ordered
                .iter()
                .filter(|(_, i)| *i >= idx)
                .map(|(id, _)| id.clone())
                .collect()
        }
        "chapters" => {
            let ids = scope
                .chapter_ids
                .as_deref()
                .filter(|l| !l.is_empty())
                .ok_or_else(|| {
                    ApiError::invalid("invalid_request", "chapters needs a non-empty chapter_ids.")
                })?;
            for id in ids {
                chapter_index(tx, book_id, id)?;
            }
            ordered
                .iter()
                .filter(|(id, _)| ids.contains(id))
                .map(|(id, _)| id.clone())
                .collect()
        }
        _ => {
            return Err(ApiError::invalid(
                "invalid_request",
                "scope.kind must be whole_book, from_chapter or chapters.",
            ))
        }
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MakeReadyIn {
    scope: ScopeIn,
}

/// `makeAudiobookReady`: free voices, in the background.
pub async fn make_ready(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: ListenerCtx,
    headers: HeaderMap,
    Path(audiobook): Path<String>,
    super::ApiJson(input): super::ApiJson<MakeReadyIn>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|k| !k.is_empty() && k.len() <= 200)
        .map(|k| format!("{audiobook}:{k}"));
    let (new_id, at, actor) = (
        state.new_id(),
        state.now(),
        Actor::with_listener(&device, &listener),
    );
    let v = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let t = target(&tx, &audiobook)?;
            if let Some(k) = &key {
                let existing: Option<String> = tx
                    .query_row("SELECT id FROM jobs WHERE idempotency_key=?1", [k], |r| {
                        r.get(0)
                    })
                    .optional()?;
                if let Some(j) = existing {
                    return Ok((jobs::job_value(&tx, &j)?, false));
                }
            }
            let chosen = resolve_scope(&tx, &t.book_id, &input.scope)?;
            require_not_deleting(&tx, &t.book_id)?;
            require_ready_to_make(&t)?;
            let mut missing = vec![];
            for ch in chosen {
                if !has_audio(&tx, &audiobook, &ch)? {
                    missing.push(ch);
                }
            }
            let (job_id, created) = match running_job(&tx, &audiobook)? {
                Some(j) => (j, false),
                None => {
                    new_job(
                        &tx,
                        NewJob {
                            id: &new_id,
                            audiobook: &audiobook,
                            book: &t.book_id,
                            urgent: false,
                            actor: &actor,
                            key: key.as_deref(),
                            at: &at,
                        },
                    )?;
                    (new_id, true)
                }
            };
            jobs::add_items(&tx, &job_id, &missing)?;
            jobs::recount(&tx, &job_id, &at)?;
            let v = jobs::job_value(&tx, &job_id)?;
            tx.commit()?;
            Ok((v, created || !missing.is_empty()))
        })
        .await?;
    if v.1 {
        jobs::announce(&state, v.0["id"].as_str().unwrap_or_default());
    }
    Ok((StatusCode::ACCEPTED, Json(v.0)))
}

// ------------------------------------------------------------------ jobs

#[derive(Deserialize)]
pub struct JobsQuery {
    book_id: Option<String>,
    audiobook_id: Option<String>,
    state: Option<String>,
    active: Option<bool>,
    limit: Option<u32>,
    after: Option<String>,
}

/// `listJobs`
pub async fn list_jobs(
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<JobsQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = page_limit(q.limit)?;
    if let Some(s) = &q.state {
        if ![
            "queued",
            "running",
            "waiting",
            "paused",
            "needs_you",
            "completed",
            "stopped",
            "failed",
        ]
        .contains(&s.as_str())
        {
            return Err(ApiError::invalid(
                "invalid_request",
                "state is not a job state.",
            ));
        }
    }
    let offset = match &q.after {
        None => 0usize,
        Some(a) => a
            .strip_prefix('o')
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| {
                ApiError::invalid(
                    "invalid_request",
                    "after is not a cursor this server issued.",
                )
            })?,
    };
    let v = state
        .store
        .run(move |c| {
            let mut ids: Vec<String> = c
                .prepare(&format!(
                    "SELECT id FROM jobs WHERE (?1 IS NULL OR book_id=?1) AND (?2 IS NULL OR audiobook_id=?2) AND (?3 IS NULL OR state=?3)
                     AND (?4 IS NULL OR (state IN {ACTIVE})=?4) ORDER BY created_at DESC, id DESC LIMIT {} OFFSET {}",
                    limit + 1,
                    offset
                ))?
                .query_map(params![q.book_id, q.audiobook_id, q.state, q.active], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            let next = if ids.len() > limit {
                ids.truncate(limit);
                json!(format!("o{}", offset + limit))
            } else {
                Value::Null
            };
            let items = ids.iter().map(|i| jobs::job_value(c, i)).collect::<Result<Vec<_>, _>>()?;
            Ok(json!({ "items": items, "next": next }))
        })
        .await?;
    Ok(Json(v))
}

/// `getJob`
pub async fn get_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        state.store.run(move |c| jobs::job_value(c, &id)).await?,
    ))
}

async fn transition(
    state: AppState,
    device: DeviceCtx,
    id: String,
    what: &'static str,
) -> Result<Json<Value>, ApiError> {
    let (audit_id, at, actor) = (state.new_id(), state.now(), Actor::device_only(&device));
    let job = id.clone();
    let v = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let cur: (String, Option<String>) = tx
                .query_row("SELECT state,plan_id FROM jobs WHERE id=?1", [&job], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
                .ok_or_else(|| ApiError::not_found("job_not_found", "No job has this id."))?;
            let (st, plan) = cur;
            let changed = match what {
                "pause" => {
                    if plan.is_some() || !["queued", "running", "waiting"].contains(&st.as_str()) {
                        return Err(ApiError::conflict("job_not_pausable", "Only a free-voice job that is queued or running can be paused."));
                    }
                    tx.execute("UPDATE jobs SET state='paused', waiting=NULL, updated_at=?2 WHERE id=?1", params![job, at])?;
                    true
                }
                "resume" => {
                    if plan.is_some() {
                        return Err(ApiError::conflict("job_not_resumable", "A plan's job is resumed with resumePlan."));
                    }
                    match st.as_str() {
                        "paused" | "needs_you" => {
                            let book: String = tx.query_row("SELECT book_id FROM jobs WHERE id=?1", [&job], |r| r.get(0))?;
                            require_not_deleting(&tx, &book)?;
                            tx.execute("UPDATE job_items SET state='queued', detail=NULL WHERE job_id=?1 AND state='failed'", [&job])?;
                            tx.execute("UPDATE jobs SET state='queued', waiting=NULL, needs_you=NULL, updated_at=?2 WHERE id=?1", params![job, at])?;
                            true
                        }
                        "queued" | "running" | "waiting" => return Err(ApiError::conflict("job_running", "This job is already running.")),
                        _ => return Err(ApiError::conflict("job_not_resumable", "This job has finished; start a new one.")),
                    }
                }
                _ => {
                    if ACTIVE.contains(&format!("'{st}'")) {
                        tx.execute("UPDATE jobs SET state='stopped', waiting=NULL, needs_you=NULL, current_chapter_id=NULL, updated_at=?2 WHERE id=?1", params![job, at])?;
                        true
                    } else {
                        false
                    }
                }
            };
            if changed {
                super::audit::record(&tx, &audit_id, &at, &format!("job.{what}"), &actor, &json!({ "job_id": job }))?;
            }
            let v = jobs::job_value(&tx, &job)?;
            tx.commit()?;
            Ok((v, changed))
        })
        .await?;
    if v.1 {
        jobs::announce(&state, &id);
    }
    Ok(Json(v.0))
}

/// `pauseJob`
pub async fn pause_job(
    State(s): State<AppState>,
    d: DeviceCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    transition(s, d, id, "pause").await
}
/// `resumeJob`
pub async fn resume_job(
    State(s): State<AppState>,
    d: DeviceCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    transition(s, d, id, "resume").await
}
/// `cancelJob`: keeps finished chapters.
pub async fn cancel_job(
    State(s): State<AppState>,
    d: DeviceCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    transition(s, d, id, "cancel").await
}

// ---------------------------------------------------------------- delivery

fn audio_not_found() -> ApiError {
    ApiError::not_found("audio_not_found", "No audio has this id.")
}

/// A byte range from a `Range` header: Some(Ok) usable, Some(Err) unsatisfiable, None ignore.
fn parse_range(h: Option<&str>, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = h?.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // several ranges: serve the whole thing, which the spec allows
    }
    let (a, b) = spec.split_once('-')?;
    let range = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            if n == 0 {
                return Some(Err(()));
            }
            (len.saturating_sub(n), len.checked_sub(1)?)
        }
        (a, "") => (a.parse().ok()?, len.checked_sub(1)?),
        (a, b) => (
            a.parse().ok()?,
            b.parse::<u64>().ok()?.min(len.saturating_sub(1)),
        ),
    };
    Some(if range.0 <= range.1 && range.0 < len {
        Ok(range)
    } else {
        Err(())
    })
}

pub async fn file_response_pub(
    path: std::path::PathBuf,
    content_type: &str,
    etag: &str,
    range_header: Option<&str>,
    immutable: bool,
) -> Result<Response, ApiError> {
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| audio_not_found())?;
    let len = file.metadata().await.map_err(ApiError::internal)?.len();
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, format!("\"{etag}\""));
    if immutable {
        builder = builder.header(header::CACHE_CONTROL, "public, max-age=31536000, immutable");
    }
    let (status, start, count) = match parse_range(range_header, len) {
        None => (StatusCode::OK, 0, len),
        Some(Ok((a, b))) => {
            builder = builder.header(header::CONTENT_RANGE, format!("bytes {a}-{b}/{len}"));
            (StatusCode::PARTIAL_CONTENT, a, b - a + 1)
        }
        Some(Err(())) => {
            return Ok(Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .expect("response"));
        }
    };
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(ApiError::internal)?;
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file.take(count)));
    Ok(builder
        .status(status)
        .header(header::CONTENT_LENGTH, count)
        .body(body)
        .expect("response"))
}

/// `getAudio`: the id names the exact bytes, so they are immutable.
pub async fn get_audio(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let key = id.clone();
    let row: Option<(String, String)> = state
        .store
        .run(move |c| {
            Ok(c.query_row(
                "SELECT path,content_type FROM audio WHERE id=?1 AND deleted_at IS NULL",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .await?;
    let (path, ct) = row.ok_or_else(audio_not_found)?;
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    file_response_pub(state.store.data_dir().join(path), &ct, &id, range, true).await
}

/// `getAudioTimings`
pub async fn get_timings(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let key = id.clone();
    let t: Option<String> = state
        .store
        .run(move |c| {
            Ok(c.query_row(
                "SELECT timings FROM audio WHERE id=?1 AND deleted_at IS NULL",
                [&key],
                |r| r.get(0),
            )
            .optional()?)
        })
        .await?;
    let lines: Value =
        serde_json::from_str(&t.ok_or_else(audio_not_found)?).map_err(ApiError::internal)?;
    let mut r = Json(json!({ "audio_id": id, "lines": lines })).into_response();
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    Ok(r)
}

// ----------------------------------------------------------------- samples

const SAMPLE_TEXT: &str =
    "The lantern swung low, and the road ahead began to show itself, one patient step at a time.";

/// `getVoiceSample`: free voices only for now; a cached repeat costs nothing.
pub async fn voice_sample(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(voice_id): Path<String>,
) -> Result<Response, ApiError> {
    type VoiceRow = (String, String, String, String, bool, String, String);
    let v = voice_id.clone();
    let row: Option<VoiceRow> = state
        .store
        .run(move |c| {
            Ok(c.query_row(
                "SELECT v.external_id,v.revision,v.tier,s.state,v.available,s.config,s.id FROM voices v JOIN voice_sources s ON s.id=v.source_id WHERE v.id=?1",
                [&v],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, i64>(4)? != 0, r.get(5)?, r.get(6)?)),
            )
            .optional()?)
        })
        .await?;
    let (external, revision, tier, source_state, available, config, source) =
        row.ok_or_else(|| ApiError::not_found("voice_not_found", "No voice has this id."))?;
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (vid, rev) = (voice_id.clone(), revision.clone());
    let cached: Option<String> = state
        .store
        .run(move |c| {
            Ok(c.query_row(
                "SELECT path FROM voice_samples WHERE voice_id=?1 AND revision=?2",
                [&vid, &rev],
                |r| r.get(0),
            )
            .optional()?)
        })
        .await?;
    if let Some(p) = cached {
        if let Ok(resp) = file_response_pub(
            state.store.data_dir().join(p),
            "audio/wav",
            &format!("{voice_id}-{revision}"),
            range.as_deref(),
            false,
        )
        .await
        {
            return Ok(resp);
        }
    }
    if source_state == "not_set_up" {
        return Err(ApiError::conflict(
            "source_not_set_up",
            format!("The {source} source is not set up."),
        ));
    }
    if source_state == "key_rejected" {
        return Err(ApiError::conflict(
            "key_rejected",
            "The API key was rejected.",
        ));
    }
    if tier == "premium" && available {
        return premium_sample(
            &state,
            &voice_id,
            &external,
            &revision,
            &config,
            range.as_deref(),
        )
        .await;
    }
    if !available || source_state != "connected" {
        return Err(ApiError::conflict(
            "source_unreachable",
            "The voice server could not be reached.",
        ));
    }
    let cfg: Value = serde_json::from_str(&config).unwrap_or(Value::Null);
    let base = cfg["base_url"].as_str().unwrap_or_default().to_string();
    let key = cfg["api_key"].as_str().map(str::to_string);
    let speak = async {
        let (live, seed) = breeze::live_voice(&base, key.as_deref(), &external).await?;
        if live != revision {
            return Err(SpeakError::VoiceChanged);
        }
        breeze::speak(&base, key.as_deref(), &external, seed, SAMPLE_TEXT).await
    };
    let speech = speak.await.map_err(|e| match e {
        SpeakError::Unreachable => ApiError::conflict(
            "source_unreachable",
            "The voice server could not be reached.",
        ),
        SpeakError::KeyRejected => {
            ApiError::conflict("key_rejected", "The voice server rejected the API key.")
        }
        SpeakError::VoiceGone => ApiError::conflict(
            "voice_unavailable",
            "The voice is no longer on the voice server.",
        ),
        SpeakError::VoiceChanged => ApiError::conflict(
            "voice_changed",
            "The voice changed on the server. Refresh the source and choose it again.",
        ),
        SpeakError::Busy(s) => {
            let mut e = ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "The voice server is busy. Try again shortly.",
            );
            e.retryable = Some(true);
            e.retry_after_seconds = Some(s as u32);
            e
        }
        SpeakError::Refused(code) => ApiError::conflict(
            "provider_refused",
            format!("The voice server refused the sample ({code})."),
        ),
        SpeakError::Failed(m) => ApiError::conflict("provider_refused", m),
    })?;
    let rel = format!("samples/{voice_id}/{revision}.wav");
    let full = state.store.data_dir().join(&rel);
    let mut wav = crate::audio::wav_header(speech.pcm.len() as u32).to_vec();
    wav.extend_from_slice(&speech.pcm);
    if let Some(dir) = full.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(ApiError::internal)?;
    }
    tokio::fs::write(&full, &wav)
        .await
        .map_err(ApiError::internal)?;
    let (vid, rev, p, at, n) = (
        voice_id.clone(),
        revision.clone(),
        rel,
        state.now(),
        wav.len() as i64,
    );
    state
        .store
        .run(move |c| {
            c.execute("INSERT OR REPLACE INTO voice_samples(voice_id,revision,path,bytes,content_type,created_at) VALUES(?1,?2,?3,?4,'audio/wav',?5)", params![vid, rev, p, n, at])?;
            Ok(())
        })
        .await?;
    file_response_pub(
        full,
        "audio/wav",
        &format!("{voice_id}-{revision}"),
        range.as_deref(),
        false,
    )
    .await
}

/// A premium sample is a short real request: reserved against this month's Allowance, settled from
/// the usage Gemini reports, and cached per voice revision so a repeat costs nothing.
async fn premium_sample(
    state: &AppState,
    voice_id: &str,
    external: &str,
    revision: &str,
    config: &str,
    range: Option<&str>,
) -> Result<Response, ApiError> {
    use crate::{plans, spend, voices::gemini};
    let cfg: Value = serde_json::from_str(config).unwrap_or(Value::Null);
    let key = cfg["api_key"].as_str().unwrap_or_default().to_string();
    let (spend_id, vid, now) = (state.new_id(), voice_id.to_string(), state.clock.now());
    let sid = spend_id.clone();
    let denied = state
        .store
        .run(move |c| {
            let (price, as_of): (i64, String) = c.query_row(
                "SELECT per_unit,as_of FROM prices WHERE provider='gemini'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let price = super::money::effective_gemini_price(price, &as_of, &crate::clock::ts(now));
            let amount = plans::estimate(SAMPLE_TEXT.chars().count() as i64, price)
                .high
                .max(1);
            spend::reserve(
                c,
                &spend::Reservation {
                    id: &sid,
                    plan: None,
                    audiobook_id: "",
                    chapter_id: Some(&vid),
                    job_id: None,
                    amount,
                    now,
                },
            )
        })
        .await?;
    if denied.is_err() {
        return Err(ApiError::conflict(
            "allowance_exceeded",
            "This month's Allowance is used up, so a premium sample cannot be made.",
        ));
    }
    let result = gemini::speak(&state.config.gemini_url, &key, external, SAMPLE_TEXT).await;
    let (outcome, speech) = match result {
        Ok(s) => (
            match s.usage.cost_micros(state.clock.now()) {
                Some(m) => spend::Outcome::Known {
                    micros: m,
                    input: s.usage.input_tokens,
                    output: s.usage.output_tokens,
                },
                None => spend::Outcome::Unknown {
                    note: format!("Gemini did not report complete usage ({}).", s.usage.detail)
                        .into(),
                },
            },
            Ok(s),
        ),
        Err(e) => match e {
            gemini::SpeakError::NoAudio(u) => (
                match u.cost_micros(state.clock.now()) {
                    Some(m) => spend::Outcome::Known {
                        micros: m,
                        input: u.input_tokens,
                        output: u.output_tokens,
                    },
                    None => spend::Outcome::Unknown {
                        note: format!(
                            "Gemini answered without audio and without complete usage ({}).",
                            u.detail
                        )
                        .into(),
                    },
                },
                Err(ApiError::conflict(
                    "provider_refused",
                    "Gemini answered without audio. The request is counted as spent.",
                )),
            ),
            gemini::SpeakError::Uncertain => (
                spend::Outcome::Unknown {
                    note: "The request may have been processed.".into(),
                },
                Err(ApiError::conflict(
                    "provider_uncertain",
                    "The request may have been processed; its cost is counted as unknown.",
                )),
            ),
            gemini::SpeakError::Failed(m) => (
                spend::Outcome::Unknown {
                    note: "The response could not be used; the request was billed.".into(),
                },
                Err(ApiError::conflict("provider_refused", m)),
            ),
            gemini::SpeakError::KeyRejected => (
                spend::Outcome::Nothing,
                Err(ApiError::conflict(
                    "key_rejected",
                    "Gemini rejected the API key.",
                )),
            ),
            gemini::SpeakError::Unreachable => (
                spend::Outcome::Nothing,
                Err(ApiError::conflict(
                    "source_unreachable",
                    "Gemini could not be reached.",
                )),
            ),
            gemini::SpeakError::Refused(c) => (
                spend::Outcome::Nothing,
                Err(ApiError::conflict(
                    "provider_refused",
                    format!("Gemini refused the sample ({c})"),
                )),
            ),
            gemini::SpeakError::Quota { retry_after, .. } => {
                let mut e = ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "provider_quota",
                    "Gemini's quota or rate limit was reached. Try again later.",
                );
                e.retryable = Some(true);
                e.retry_after_seconds = Some(retry_after.min(u32::MAX as u64) as u32);
                (spend::Outcome::Nothing, Err(e))
            }
        },
    };
    let sid = spend_id.clone();
    state
        .store
        .run(move |c| spend::settle(c, &sid, &outcome))
        .await?;
    let speech = speech?;
    let rel = format!("samples/{voice_id}/{revision}.wav");
    let full = state.store.data_dir().join(&rel);
    let mut wav = crate::audio::wav_header(speech.pcm.len() as u32).to_vec();
    wav.extend_from_slice(&speech.pcm);
    if let Some(dir) = full.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(ApiError::internal)?;
    }
    tokio::fs::write(&full, &wav)
        .await
        .map_err(ApiError::internal)?;
    let (vid, rev, p, at, n) = (
        voice_id.to_string(),
        revision.to_string(),
        rel,
        state.now(),
        wav.len() as i64,
    );
    state
        .store
        .run(move |c| {
            c.execute("INSERT OR REPLACE INTO voice_samples(voice_id,revision,path,bytes,content_type,created_at) VALUES(?1,?2,?3,?4,'audio/wav',?5)", params![vid, rev, p, n, at])?;
            Ok(())
        })
        .await?;
    file_response_pub(
        full,
        "audio/wav",
        &format!("{voice_id}-{revision}"),
        range,
        false,
    )
    .await
}
