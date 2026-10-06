//! Jobs and the worker that makes audio.
//!
//! One scheduler owns chapter order. Breeze passages use bounded concurrency;
//! premium passages retain their sequential spending gate. A job is a list of chapters. The
//! worker always takes the next chapter of the oldest *urgent* job (someone
//! pressed play), then of the oldest other job. A chapter is written only
//! when complete, so stopping, crashing or losing the voice server never
//! leaves a half-made chapter behind and finished chapters are kept.

use crate::{
    app::AppState,
    audio::{self, Line},
    chapter_requests::{self, RetainedRequest},
    error::ApiError,
    events::Notice,
    plans, spend,
    voices::{
        breeze::{self, SpeakError},
        gemini,
    },
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    future::Future,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{watch, Notify};

/// Wakes the worker when there is work, and tells in-flight requests when a job's state changed.
pub struct JobSignal {
    wake: Notify,
    changed: watch::Sender<u64>,
}

impl JobSignal {
    pub fn new() -> Self {
        JobSignal {
            wake: Notify::new(),
            changed: watch::channel(0).0,
        }
    }
    /// Call after any change to a job or its items.
    pub fn poke(&self) {
        self.changed.send_modify(|g| *g += 1);
        self.wake.notify_one();
    }
}

impl Default for JobSignal {
    fn default() -> Self {
        Self::new()
    }
}

pub fn detail(code: &str, text: &str, until: Option<&str>) -> String {
    json!({ "code": code, "text": text, "until": until }).to_string()
}

fn parse_detail(s: Option<String>) -> Value {
    s.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

/// Stored separately from Ready: only retained request parts advance these counts.
/// Timing samples belong to one job/voice and survive a chapter boundary or restart.
#[derive(Default, Deserialize, Serialize)]
struct Generation {
    chapter_id: String,
    requests_done: usize,
    requests_total: usize,
    characters_done: i64,
    characters_total: i64,
    elapsed_seconds: f64,
    sample_characters: i64,
    sample_seconds: f64,
    request_started_at: Option<f64>,
    chunk_chars: i64,
    #[serde(default)]
    active_requests: usize,
    #[serde(default)]
    parallelism: usize,
    /// Successful intervals for this attempt only. Their union counts overlap once.
    #[serde(default)]
    sample_intervals: Vec<[f64; 2]>,
    #[serde(default)]
    elapsed_intervals: Vec<[f64; 2]>,
    #[serde(default)]
    active_started_at: BTreeMap<String, f64>,
    /// Prevent detached blocking database work from updating a later attempt.
    #[serde(default)]
    attempt_id: String,
}

#[derive(Clone, Copy)]
struct RequestTiming {
    started_at: f64,
    seconds: f64,
}

fn union_seconds(intervals: &mut Vec<[f64; 2]>) -> f64 {
    intervals.sort_by(|a, b| a[0].total_cmp(&b[0]));
    let mut union: Vec<[f64; 2]> = Vec::with_capacity(intervals.len());
    for interval in intervals.iter() {
        if let Some(last) = union.last_mut().filter(|last| last[1] >= interval[0]) {
            last[1] = last[1].max(interval[1]);
        } else {
            union.push(*interval);
        }
    }
    let seconds = union.iter().map(|r| r[1] - r[0]).sum();
    *intervals = union;
    seconds
}

fn record_sample(g: &mut Generation, characters: i64, timing: RequestTiming) {
    let before: f64 = g.sample_intervals.iter().map(|r| r[1] - r[0]).sum();
    g.sample_intervals
        .push([timing.started_at, timing.started_at + timing.seconds]);
    let after = union_seconds(&mut g.sample_intervals);
    g.sample_seconds += (after - before).max(0.0);
    g.sample_characters = g.sample_characters.saturating_add(characters);
}

fn elapsed_seconds(g: &Generation, now: f64) -> f64 {
    if g.elapsed_intervals.is_empty() && g.active_started_at.is_empty() {
        // Older persisted attempts did not have individual active intervals.
        return g.elapsed_seconds + g.request_started_at.map_or(0.0, |at| (now - at).max(0.0));
    }
    let mut intervals = g.elapsed_intervals.clone();
    intervals.extend(g.active_started_at.values().map(|&at| [at, now.max(at)]));
    union_seconds(&mut intervals)
}

fn wall_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn generation_row(conn: &Connection, job_id: &str) -> Result<Option<Generation>, ApiError> {
    let value: Option<String> =
        conn.query_row("SELECT generation FROM jobs WHERE id=?1", [job_id], |r| {
            r.get(0)
        })?;
    Ok(value.and_then(|v| serde_json::from_str(&v).ok()))
}

fn save_generation(conn: &Connection, job_id: &str, g: &Generation) -> Result<(), ApiError> {
    conn.execute(
        "UPDATE jobs SET generation=?2 WHERE id=?1",
        params![
            job_id,
            serde_json::to_string(g).expect("finite generation timings")
        ],
    )?;
    Ok(())
}

fn chunk_characters(lines: &[Line], chunks: &[std::ops::Range<usize>]) -> Vec<i64> {
    chunks
        .iter()
        .map(|r| lines[r.end - 1].end - lines[r.start].start)
        .collect()
}

fn remaining_characters(conn: &Connection, job_id: &str, g: &Generation) -> Result<i64, ApiError> {
    let chapters = conn
        .prepare(
            "SELECT i.chapter_id FROM job_items i JOIN jobs j ON j.id=i.job_id
             WHERE i.job_id=?1 AND i.state='queued' AND i.chapter_id<>?2
             AND NOT EXISTS (SELECT 1 FROM audio a WHERE a.audiobook_id=j.audiobook_id AND a.chapter_id=i.chapter_id AND a.deleted_at IS NULL)",
        )?
        .query_map(params![job_id, g.chapter_id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut total = g.characters_total.saturating_sub(g.characters_done);
    for chapter in chapters {
        let lines = conn
            .prepare("SELECT id,start,end FROM lines WHERE chapter_id=?1 ORDER BY idx")?
            .query_map([chapter], |r| {
                Ok(Line {
                    id: r.get(0)?,
                    start: r.get(1)?,
                    end: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let chunks = audio::chunk_lines(&lines, g.chunk_chars.max(1));
        total = total.saturating_add(chunk_characters(&lines, &chunks).iter().sum::<i64>());
    }
    Ok(total)
}

fn generation_value(conn: &Connection, job: &Value) -> Result<Value, ApiError> {
    let id = job["id"].as_str().unwrap_or_default();
    let Some(g) = generation_row(conn, id)? else {
        return Ok(Value::Null);
    };
    if job["kind"] != "make_audio"
        || job["state"] == "queued"
        || job["current_chapter_id"] != g.chapter_id
    {
        return Ok(Value::Null);
    }
    let can_estimate = job["state"] == "running"
        && g.sample_characters > 0
        && g.sample_seconds.is_finite()
        && g.sample_seconds > 0.0;
    let estimate = |chars: i64| {
        if !can_estimate {
            return None;
        }
        let seconds = chars.max(0) as f64 * g.sample_seconds / g.sample_characters as f64;
        seconds.is_finite().then_some(seconds)
    };
    // Null estimates need no scan of the queued chapter line metadata. Besides
    // polling, listJobs can read several paused/waiting jobs under one lock.
    let job_seconds_remaining = if can_estimate {
        estimate(remaining_characters(conn, id, &g)?)
    } else {
        None
    };
    Ok(json!({
        "chapter_id": g.chapter_id,
        "requests_done": g.requests_done.min(g.requests_total),
        "requests_total": g.requests_total,
        "characters_done": g.characters_done.clamp(0, g.characters_total.max(0)),
        "characters_total": g.characters_total.max(0),
        "elapsed_seconds": elapsed_seconds(&g, wall_seconds()).max(0.0),
        "chapter_seconds_remaining": estimate(g.characters_total.saturating_sub(g.characters_done)),
        "job_seconds_remaining": job_seconds_remaining,
    }))
}

async fn generation_request<T>(
    state: &AppState,
    job_id: &str,
    chapter_id: &str,
    attempt_id: &str,
    paid: bool,
    fut: impl Future<Output = T>,
    cancellation: Option<watch::Receiver<bool>>,
) -> Option<(T, RequestTiming)> {
    // Admission is outside measured provider time, and the permit is released
    // before any retry backoff. Samples share this same source-wide limit.
    let permit = if paid {
        None
    } else {
        Some(
            interruptible_with_cancellation(
                state,
                job_id,
                false,
                state.gates.breeze.acquire(),
                cancellation.clone(),
            )
            .await?
            .ok()?,
        )
    };
    let request_id = state.new_id();
    let (id, chapter, attempt, request) = (
        job_id.to_string(),
        chapter_id.to_string(),
        attempt_id.to_string(),
        request_id.clone(),
    );
    let _ = state
        .store
        .run(move |conn| {
            if let Some(mut g) = generation_row(conn, &id)? {
                if g.chapter_id == chapter && g.attempt_id == attempt {
                    let started = wall_seconds();
                    g.active_started_at.insert(request, started);
                    g.active_requests = g.active_started_at.len();
                    g.request_started_at =
                        g.active_started_at.values().copied().min_by(f64::total_cmp);
                    save_generation(conn, &id, &g)?;
                }
            }
            Ok(())
        })
        .await;
    let began = Instant::now();
    let sample_started_at = wall_seconds();
    let result = interruptible_with_cancellation(state, job_id, paid, fut, cancellation).await;
    let seconds = began.elapsed().as_secs_f64();
    drop(permit);
    let (id, chapter, attempt, request) = (
        job_id.to_string(),
        chapter_id.to_string(),
        attempt_id.to_string(),
        request_id,
    );
    let _ = state
        .store
        .run(move |conn| {
            if let Some(mut g) = generation_row(conn, &id)? {
                if g.chapter_id == chapter && g.attempt_id == attempt {
                    g.active_started_at.remove(&request);
                    g.elapsed_intervals
                        .push([sample_started_at, sample_started_at + seconds]);
                    g.elapsed_seconds = union_seconds(&mut g.elapsed_intervals);
                    g.active_requests = g.active_started_at.len();
                    g.request_started_at =
                        g.active_started_at.values().copied().min_by(f64::total_cmp);
                    save_generation(conn, &id, &g)?;
                }
            }
            Ok(())
        })
        .await;
    result.map(|v| {
        (
            v,
            RequestTiming {
                started_at: sample_started_at,
                seconds,
            },
        )
    })
}

/// Freeze the attempt before recovery and invalidate any stale blocking writes.
async fn finish_generation_attempt(state: &AppState, job_id: &str, attempt_id: &str) {
    let (id, attempt) = (job_id.to_string(), attempt_id.to_string());
    let _ = state
        .store
        .run(move |conn| {
            if let Some(mut g) = generation_row(conn, &id)? {
                if g.attempt_id != attempt {
                    return Ok(());
                }
                let now = wall_seconds();
                if !g.active_started_at.is_empty() {
                    g.elapsed_intervals
                        .extend(g.active_started_at.values().map(|&at| [at, now.max(at)]));
                    g.elapsed_seconds = union_seconds(&mut g.elapsed_intervals);
                } else {
                    g.elapsed_seconds = elapsed_seconds(&g, now);
                }
                g.request_started_at = None;
                g.active_requests = 0;
                g.active_started_at.clear();
                g.attempt_id.clear();
                save_generation(conn, &id, &g)?;
            }
            Ok(())
        })
        .await;
}

/// The contract's `Job`.
pub fn job_value(conn: &Connection, id: &str) -> Result<Value, ApiError> {
    let mut job = conn.query_row(
        "SELECT id,kind,state,audiobook_id,book_id,plan_id,chapters_total,chapters_done,current_chapter_id,waiting,needs_you,started_by,created_at,updated_at FROM jobs WHERE id=?1",
        [id],
        |r| {
            let by: Value = serde_json::from_str(&r.get::<_, String>(11)?).unwrap_or(Value::Null);
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "kind": r.get::<_, String>(1)?,
                "state": r.get::<_, String>(2)?,
                "audiobook_id": r.get::<_, Option<String>>(3)?,
                "book_id": r.get::<_, Option<String>>(4)?,
                "plan_id": r.get::<_, Option<String>>(5)?,
                "chapters_total": r.get::<_, i64>(6)?,
                "chapters_done": r.get::<_, i64>(7)?,
                "current_chapter_id": r.get::<_, Option<String>>(8)?,
                "waiting": parse_detail(r.get(9)?),
                "needs_you": parse_detail(r.get(10)?),
                "started_by": by,
                "created_at": r.get::<_, String>(12)?,
                "updated_at": r.get::<_, String>(13)?,
            }))
        },
    )
    .optional()?
    .ok_or_else(|| ApiError::not_found("job_not_found", "No job has this id."))?;
    job["generation"] = generation_value(conn, &job)?;
    Ok(job)
}

pub fn announce(state: &AppState, job_id: &str) {
    state.notify(Notice::new("job.updated", state.now()).with_id(job_id.to_string()));
    state.jobs.poke();
}

/// Queue chapters on a job after its current items. Explicit free requests can requeue a
/// finished item whose audio is unavailable. Finished paid items require a new approved plan.
/// Returns how many were added.
pub fn add_items(conn: &Connection, job_id: &str, chapters: &[String]) -> Result<i64, ApiError> {
    let mut pos: i64 = conn.query_row(
        "SELECT COALESCE(MAX(position),0) FROM job_items WHERE job_id=?1",
        [job_id],
        |r| r.get(0),
    )?;
    let mut added = 0;
    for c in chapters {
        let n = conn.execute(
            "INSERT INTO job_items(job_id,chapter_id,position,state) VALUES(?1,?2,?3,'queued')
             ON CONFLICT(job_id,chapter_id) DO UPDATE SET state='queued', detail=NULL, position=excluded.position
             WHERE state IN ('failed','skipped') OR (state='done'
               AND EXISTS(SELECT 1 FROM jobs j WHERE j.id=?1 AND j.plan_id IS NULL)
               AND NOT EXISTS(SELECT 1 FROM audio x JOIN jobs j ON j.audiobook_id=x.audiobook_id WHERE j.id=?1 AND x.chapter_id=?2 AND x.deleted_at IS NULL))",
            params![job_id, c, pos + 1],
        )?;
        if n > 0 {
            pos += 1;
            added += 1;
        }
    }
    Ok(added)
}

/// Keep the job's totals in step with its items.
pub fn recount(conn: &Connection, job_id: &str, at: &str) -> Result<(), ApiError> {
    conn.execute(
        "UPDATE jobs SET chapters_total=(SELECT COUNT(*) FROM job_items WHERE job_id=?1),
                         chapters_done=(SELECT COUNT(*) FROM job_items WHERE job_id=?1 AND state IN ('done','skipped')),
                         updated_at=?2 WHERE id=?1",
        params![job_id, at],
    )?;
    Ok(())
}

fn set_state(
    conn: &Connection,
    job_id: &str,
    from: &[&str],
    to: &str,
    waiting: Option<String>,
    needs_you: Option<String>,
    at: &str,
) -> Result<bool, ApiError> {
    set_state_wake(conn, job_id, from, to, waiting, needs_you, None, at)
}

#[allow(clippy::too_many_arguments)]
fn set_state_wake(
    conn: &Connection,
    job_id: &str,
    from: &[&str],
    to: &str,
    waiting: Option<String>,
    needs_you: Option<String>,
    wake_at: Option<String>,
    at: &str,
) -> Result<bool, ApiError> {
    let list = from
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    let n = conn.execute(
        &format!("UPDATE jobs SET state=?2, waiting=?3, needs_you=?4, wake_at=?5, updated_at=?6 WHERE id=?1 AND state IN ({list})"),
        params![job_id, to, waiting, needs_you, wake_at, at],
    )?;
    Ok(n > 0)
}

async fn is_active(state: &AppState, job_id: &str) -> bool {
    let id = job_id.to_string();
    state
        .store
        .run(move |c| {
            Ok(c.query_row(
                "SELECT state IN ('running','waiting') FROM jobs WHERE id=?1",
                [&id],
                |r| r.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false))
        })
        .await
        .unwrap_or(false)
}

/// Run `fut` until the job is paused, stopped or the server shuts down; then drop it
/// (which closes any open request to the voice server) and return None.
/// A paid job is not interrupted by pause or stop: the request is already being billed,
/// so it finishes and its result is kept. Only shutdown interrupts it.
async fn interruptible<T>(
    state: &AppState,
    job_id: &str,
    paid: bool,
    fut: impl Future<Output = T>,
) -> Option<T> {
    interruptible_with_cancellation(state, job_id, paid, fut, None).await
}

async fn interruptible_with_cancellation<T>(
    state: &AppState,
    job_id: &str,
    paid: bool,
    fut: impl Future<Output = T>,
    mut cancellation: Option<watch::Receiver<bool>>,
) -> Option<T> {
    tokio::pin!(fut);
    let mut changed = state.jobs.changed.subscribe();
    let mut shutdown = state.shutdown.subscribe();
    if *shutdown.borrow()
        || cancellation.as_ref().is_some_and(|rx| *rx.borrow())
        || (!paid && !is_active(state, job_id).await)
    {
        return None;
    }
    loop {
        tokio::select! {
            biased;
            v = &mut fut => return Some(v),
            _ = changed.changed() => { if !paid && !is_active(state, job_id).await { return None; } }
            _ = shutdown.changed() => return None,
            _ = async {
                if let Some(rx) = cancellation.as_mut() {
                    let _ = rx.changed().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => return None,
        }
    }
}

/// Why making a chapter stopped, from either voice source.
#[derive(Debug)]
enum Fail {
    Unreachable,
    KeyRejected,
    VoiceGone,
    VoiceChanged,
    Busy(u64),
    Refused(String),
    Failed(String),
    /// The plan's own limit would be passed.
    Limit,
    /// The monthly Allowance would be passed.
    Allowance,
    /// Provider quota: wait this many seconds (`daily` when the day's quota is gone).
    Quota(u64, bool),
    /// Answered without audio; the request is counted as spent.
    NoAudio,
    /// May have been processed; its cost is unknown.
    Uncertain,
}

impl From<SpeakError> for Fail {
    fn from(e: SpeakError) -> Self {
        match e {
            SpeakError::Unreachable => Fail::Unreachable,
            SpeakError::KeyRejected => Fail::KeyRejected,
            SpeakError::VoiceGone => Fail::VoiceGone,
            SpeakError::VoiceChanged => Fail::VoiceChanged,
            SpeakError::Busy(s) => Fail::Busy(s),
            SpeakError::Refused(c) => Fail::Refused(c),
            SpeakError::Failed(m) => Fail::Failed(m),
        }
    }
}

enum Stop {
    Interrupted,
    Failed(Fail),
}

/// Try a request, waiting and retrying when the server is unreachable or busy.
/// `f` returns Unreachable only when nothing was sent, so retrying is always free.
async fn attempt<T, F, Fut>(
    state: &AppState,
    job_id: &str,
    chapter_id: Option<(&str, &str)>,
    paid: bool,
    cancellation: Option<watch::Receiver<bool>>,
    mut f: F,
) -> Result<(T, RequestTiming), Stop>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Fail>>,
{
    const TRIES: u32 = 3;
    for n in 0..=TRIES {
        let result = match chapter_id {
            Some((chapter, attempt)) => {
                generation_request(
                    state,
                    job_id,
                    chapter,
                    attempt,
                    paid,
                    f(),
                    cancellation.clone(),
                )
                .await
            }
            None => interruptible(state, job_id, paid, f()).await.map(|v| {
                (
                    v,
                    RequestTiming {
                        started_at: wall_seconds(),
                        seconds: 0.0,
                    },
                )
            }),
        };
        match result {
            None => return Err(Stop::Interrupted),
            Some((Ok(v), seconds)) => return Ok((v, seconds)),
            Some((Err(Fail::Unreachable), _)) if n < TRIES => {
                let wait = Duration::from_millis(state.config.job_retry_ms << n);
                if interruptible_with_cancellation(
                    state,
                    job_id,
                    paid,
                    tokio::time::sleep(wait),
                    cancellation.clone(),
                )
                .await
                .is_none()
                {
                    return Err(Stop::Interrupted);
                }
            }
            Some((Err(Fail::Busy(secs)), _)) if n < TRIES => {
                let until =
                    crate::clock::ts(state.clock.now() + chrono::Duration::seconds(secs as i64));
                let (id, at, d) = (
                    job_id.to_string(),
                    state.now(),
                    detail(
                        "waiting_busy",
                        "The voice server is busy or loading. Bardic will try again.",
                        Some(&until),
                    ),
                );
                let _ = state
                    .store
                    .run(move |c| set_state(c, &id, &["running"], "waiting", Some(d), None, &at))
                    .await;
                announce(state, job_id);
                let wait = Duration::from_secs(if state.config.job_retry_ms < 100 {
                    0
                } else {
                    secs
                });
                if interruptible_with_cancellation(
                    state,
                    job_id,
                    paid,
                    tokio::time::sleep(wait),
                    cancellation.clone(),
                )
                .await
                .is_none()
                {
                    return Err(Stop::Interrupted);
                }
                let (id, at) = (job_id.to_string(), state.now());
                let _ = state
                    .store
                    .run(move |c| set_state(c, &id, &["waiting"], "running", None, None, &at))
                    .await;
                announce(state, job_id);
            }
            Some((Err(e), _)) => return Err(Stop::Failed(e)),
        }
    }
    Err(Stop::Failed(Fail::Unreachable))
}

struct Ctx {
    attempt_id: String,
    audiobook_id: String,
    book_id: String,
    source: String,
    voice: String,
    revision: String,
    base_url: Option<String>,
    api_key: Option<String>,
    /// (plan id, limit in micros) for a paid job.
    plan: Option<(String, i64)>,
    /// Micros per million characters, for reserving money before a paid request.
    price: i64,
    text: Vec<char>,
    lines: Vec<Line>,
    has_audio: bool,
}

fn load_ctx(
    conn: &Connection,
    job_id: &str,
    chapter_id: &str,
    at: &str,
    attempt_id: String,
) -> Result<Option<Ctx>, ApiError> {
    type Row = (
        String,
        String,
        String,
        String,
        String,
        String,
        Option<(String, i64)>,
    );
    let row: Option<Row> = conn
        .query_row(
            "SELECT a.id,a.book_id,v.external_id,a.voice_revision,s.config,s.id,p.id,p.limit_micros FROM jobs j
             JOIN audiobooks a ON a.id=j.audiobook_id JOIN voices v ON v.id=a.voice_id JOIN voice_sources s ON s.id=v.source_id
             LEFT JOIN plans p ON p.id=j.plan_id WHERE j.id=?1",
            [job_id],
            |r| {
                let plan = match (r.get::<_, Option<String>>(6)?, r.get::<_, Option<i64>>(7)?) {
                    (Some(id), Some(l)) => Some((id, l)),
                    _ => None,
                };
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, plan))
            },
        )
        .optional()?;
    let Some((audiobook_id, book_id, voice, revision, config, source, plan)) = row else {
        return Ok(None);
    };
    let cfg: Value = serde_json::from_str(&config).unwrap_or(Value::Null);
    let text: String =
        conn.query_row("SELECT text FROM chapters WHERE id=?1", [chapter_id], |r| {
            r.get(0)
        })?;
    let lines = conn
        .prepare("SELECT id,start,end FROM lines WHERE chapter_id=?1 ORDER BY idx")?
        .query_map([chapter_id], |r| {
            Ok(Line {
                id: r.get(0)?,
                start: r.get(1)?,
                end: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let has_audio = conn
        .query_row(
            "SELECT 1 FROM audio WHERE audiobook_id=?1 AND chapter_id=?2 AND deleted_at IS NULL",
            [&audiobook_id, chapter_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let (price, price_as_of): (i64, String) = conn.query_row(
        "SELECT COALESCE((SELECT per_unit FROM prices WHERE provider=?1),0), COALESCE((SELECT as_of FROM prices WHERE provider=?1),'')",
        [&source],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let price = if source == "gemini" {
        crate::api::money::effective_gemini_price(price, &price_as_of, at)
    } else {
        price
    };
    conn.execute("UPDATE jobs SET state='running', current_chapter_id=?2, updated_at=?3 WHERE id=?1 AND state='queued'", params![job_id, chapter_id, at])?;
    conn.execute(
        "UPDATE jobs SET current_chapter_id=?2 WHERE id=?1",
        params![job_id, chapter_id],
    )?;
    // Preserve this job's successful throughput samples, but never expose the
    // previous attempt's timer or a different chapter's progress while loading.
    if let Some(mut g) = generation_row(conn, job_id)? {
        g.chapter_id.clear();
        g.elapsed_seconds = 0.0;
        g.request_started_at = None;
        g.active_requests = 0;
        g.sample_intervals.clear();
        g.elapsed_intervals.clear();
        g.active_started_at.clear();
        g.attempt_id.clear();
        save_generation(conn, job_id, &g)?;
    }
    Ok(Some(Ctx {
        attempt_id,
        audiobook_id,
        book_id,
        source,
        voice,
        revision,
        base_url: cfg
            .get("base_url")
            .and_then(Value::as_str)
            .map(str::to_string),
        api_key: cfg
            .get("api_key")
            .and_then(Value::as_str)
            .map(str::to_string),
        plan,
        price,
        text: text.chars().collect(),
        lines,
        has_audio,
    }))
}

struct Made {
    audio_id: String,
    path: String,
    bytes: i64,
    sha256: String,
    duration_ms: i64,
    timings: Vec<Value>,
}

/// Where a chapter's finished requests are kept, and how far it got.
#[derive(Clone)]
struct Parts {
    audio_id: String,
    chunk_chars: i64,
    chunks_done: usize,
    pcm_bytes: usize,
    timings: Vec<Value>,
}

async fn load_parts(
    state: &AppState,
    ctx: &Ctx,
    chapter_id: &str,
    dir: &std::path::Path,
) -> Result<Parts, ApiError> {
    let (a, c) = (ctx.audiobook_id.clone(), chapter_id.to_string());
    let row: Option<(String, i64, i64, i64, String)> = state
        .store
        .run(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT audio_id,chunk_chars,chunks_done,pcm_bytes,timings FROM chapter_parts WHERE audiobook_id=?1 AND chapter_id=?2",
                    [&a, &c],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?)
        })
        .await?;
    if let Some((audio_id, chars, done, bytes, timings)) = row {
        let tmp = dir.join(format!("{chapter_id}.part"));
        let metadata = match tokio::fs::metadata(&tmp).await {
            Ok(metadata) => Some(metadata),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(ApiError::internal(e)),
        };
        let mut len = metadata.as_ref().filter(|m| m.is_file()).map(|m| m.len());
        let chunks = audio::chunk_lines(&ctx.lines, chars.max(1));
        // The final rename is durable before Ready's transaction. If the
        // process stopped in that gap, adopt the exact unreferenced file rather
        // than synthesizing an already complete chapter again.
        if metadata.is_none()
            && chars > 0
            && done >= 0
            && done as usize == chunks.len()
            && bytes >= 0
            && bytes % 2 == 0
        {
            let final_path = dir.join(format!("{audio_id}.wav"));
            let final_metadata = match tokio::fs::metadata(&final_path).await {
                Ok(metadata) => Some(metadata),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(ApiError::internal(e)),
            };
            if final_metadata.is_some_and(|m| m.is_file() && m.len() == 44 + bytes as u64) {
                let id = audio_id.clone();
                let unreferenced = state
                    .store
                    .run(move |conn| {
                        Ok(conn.query_row(
                            "SELECT NOT EXISTS(SELECT 1 FROM audio WHERE id=?1)",
                            [id],
                            |r| r.get::<_, bool>(0),
                        )?)
                    })
                    .await?;
                if unreferenced {
                    tokio::fs::rename(&final_path, &tmp)
                        .await
                        .map_err(ApiError::internal)?;
                    sync_audio_directory(state, dir).await?;
                    len = Some(44 + bytes as u64);
                }
            }
        }
        if chars > 0
            && done >= 0
            && done as usize <= chunks.len()
            && bytes >= 0
            && bytes % 2 == 0
            && len.is_some_and(|len| len >= 44 + bytes as u64)
        {
            // Appending and syncing bytes precedes the prefix transaction. A
            // crash between those steps leaves a valid prefix plus extra bytes.
            // Keep that prefix and replay the independently retained request.
            if len != Some(44 + bytes as u64) {
                let file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .open(&tmp)
                    .await
                    .map_err(ApiError::internal)?;
                file.set_len(44 + bytes as u64)
                    .await
                    .map_err(ApiError::internal)?;
                file.sync_data().await.map_err(ApiError::internal)?;
            }
            return Ok(Parts {
                audio_id,
                chunk_chars: chars,
                chunks_done: done as usize,
                pcm_bytes: bytes as usize,
                timings: serde_json::from_str(&timings).unwrap_or_default(),
            });
        }
        // A saved chapter pins its request size so tuning the default never
        // discards paid work. Invalid recovery data or missing bytes starts over.
        let _ = tokio::fs::remove_file(dir.join(format!("{chapter_id}.part"))).await;
    }
    Ok(Parts {
        audio_id: state.new_id(),
        chunk_chars: state.config.audio_chunk_chars as i64,
        chunks_done: 0,
        pcm_bytes: 0,
        timings: vec![],
    })
}

fn fail_io(e: std::io::Error) -> Stop {
    Stop::Failed(Fail::Failed(format!("Could not write audio: {e}")))
}

/// One paid request: reserve the money, send, settle. Returns the audio.
async fn gemini_chunk(
    state: &AppState,
    job_id: &str,
    ctx: &Ctx,
    chapter_id: &str,
    text: &str,
) -> Result<(Vec<u8>, RequestTiming), Stop> {
    let key = ctx.api_key.clone().unwrap_or_default();
    let amount = plans::estimate(text.chars().count() as i64, ctx.price)
        .high
        .max(1);
    let (spend_id, st, aid, cid, plan) = (
        state.new_id(),
        state.clone(),
        ctx.audiobook_id.clone(),
        chapter_id.to_string(),
        ctx.plan.clone(),
    );
    let (sid, jid) = (spend_id.clone(), job_id.to_string());
    let now = state.clock.now();
    let denied = state
        .store
        .run(move |c| {
            let _ = &st;
            spend::reserve(
                c,
                &spend::Reservation {
                    id: &sid,
                    plan: plan.as_ref().map(|(i, l)| (i.as_str(), *l)),
                    audiobook_id: &aid,
                    chapter_id: Some(&cid),
                    job_id: Some(&jid),
                    amount,
                    now,
                },
            )
        })
        .await
        .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
    match denied {
        Err(spend::Denied::Plan) => return Err(Stop::Failed(Fail::Limit)),
        Err(spend::Denied::Allowance) => return Err(Stop::Failed(Fail::Allowance)),
        Err(spend::Denied::Stopped) => return Err(Stop::Interrupted),
        Ok(()) => {}
    }
    let settle = |outcome: spend::Outcome| {
        let (id, store) = (spend_id.clone(), state.store.clone());
        async move {
            let _ = store.run(move |c| spend::settle(c, &id, &outcome)).await;
        }
    };
    let base = state.config.gemini_url.clone();
    for n in 0..=3u32 {
        let (result, seconds) = match generation_request(
            state,
            job_id,
            chapter_id,
            &ctx.attempt_id,
            true,
            gemini::speak(&base, &key, &ctx.voice, text),
            None,
        )
        .await
        {
            // Shut down mid-request: it stays reserved and becomes unknown on the next start.
            None => return Err(Stop::Interrupted),
            Some(r) => r,
        };
        return match result {
            Ok(speech) => {
                match speech.usage.cost_micros(state.clock.now()) {
                    Some(m) => {
                        settle(spend::Outcome::Known {
                            micros: m,
                            input: speech.usage.input_tokens,
                            output: speech.usage.output_tokens,
                        })
                        .await
                    }
                    None => {
                        settle(spend::Outcome::Unknown {
                            note: format!(
                                "Gemini did not report complete usage ({}).",
                                speech.usage.detail
                            )
                            .into(),
                        })
                        .await
                    }
                }
                Ok((speech.pcm, seconds))
            }
            Err(gemini::SpeakError::Unreachable) if n < 3 => {
                let wait = Duration::from_millis(state.config.job_retry_ms << n);
                if interruptible(state, job_id, true, tokio::time::sleep(wait))
                    .await
                    .is_none()
                {
                    return Err(Stop::Interrupted);
                }
                continue;
            }
            Err(e) => {
                let (outcome, fail) = match e {
                    gemini::SpeakError::Unreachable => (spend::Outcome::Nothing, Fail::Unreachable),
                    gemini::SpeakError::KeyRejected => (spend::Outcome::Nothing, Fail::KeyRejected),
                    gemini::SpeakError::Quota { retry_after, daily } => {
                        (spend::Outcome::Nothing, Fail::Quota(retry_after, daily))
                    }
                    gemini::SpeakError::Refused(c) => (spend::Outcome::Nothing, Fail::Refused(c)),
                    gemini::SpeakError::NoAudio(u) => (
                        match u.cost_micros(state.clock.now()) {
                            Some(m) => spend::Outcome::Known {
                                micros: m,
                                input: u.input_tokens,
                                output: u.output_tokens,
                            },
                            None => spend::Outcome::Unknown {
                                note: format!("Gemini answered without audio and without complete usage ({}).", u.detail).into(),
                            },
                        },
                        Fail::NoAudio,
                    ),
                    gemini::SpeakError::Uncertain => (
                        spend::Outcome::Unknown {
                            note: "The request may have been processed; it was not sent again.".into(),
                        },
                        Fail::Uncertain,
                    ),
                    gemini::SpeakError::Failed(m) => (
                        spend::Outcome::Unknown {
                            note: "The response could not be used; the request was billed.".into(),
                        },
                        Fail::Failed(m),
                    ),
                };
                settle(outcome).await;
                Err(Stop::Failed(fail))
            }
        };
    }
    Err(Stop::Failed(Fail::Unreachable))
}

fn save_prefix(
    conn: &Connection,
    audiobook: &str,
    chapter: &str,
    parts: &Parts,
) -> Result<(), ApiError> {
    // REPLACE would delete the parent and cascade independently completed requests.
    conn.execute(
        "INSERT INTO chapter_parts(audiobook_id,chapter_id,audio_id,chunk_chars,chunks_done,pcm_bytes,timings) VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(audiobook_id,chapter_id) DO UPDATE SET audio_id=excluded.audio_id,chunk_chars=excluded.chunk_chars,chunks_done=excluded.chunks_done,pcm_bytes=excluded.pcm_bytes,timings=excluded.timings",
        params![audiobook, chapter, parts.audio_id, parts.chunk_chars, parts.chunks_done as i64, parts.pcm_bytes as i64, Value::Array(parts.timings.clone()).to_string()],
    )?;
    Ok(())
}

fn request_text(ctx: &Ctx, range: &std::ops::Range<usize>) -> String {
    ctx.text[ctx.lines[range.start].start as usize..ctx.lines[range.end - 1].end as usize]
        .iter()
        .collect()
}

fn request_timings(lines: &[Line], pcm: &[u8], segments: &[audio::Segment]) -> Vec<Value> {
    lines
        .iter()
        .zip(audio::line_times(
            lines,
            lines[0].start,
            audio::pcm_ms(pcm.len()),
            segments,
        ))
        .map(|(line, (start, end))| json!({"line_id": line.id, "start_ms": start, "end_ms": end}))
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn append_request(
    state: &AppState,
    job_id: &str,
    ctx: &Ctx,
    chapter: &str,
    parts: &mut Parts,
    file: &mut tokio::fs::File,
    pcm: &[u8],
    timings: &[Value],
    sample: Option<(i64, RequestTiming)>,
    characters_done: i64,
) -> Result<(), Stop> {
    use tokio::io::AsyncWriteExt;
    let offset = audio::pcm_ms(parts.pcm_bytes);
    parts.timings.extend(timings.iter().map(|timing| {
        let mut timing = timing.clone();
        timing["start_ms"] = json!(offset + timing["start_ms"].as_i64().unwrap());
        timing["end_ms"] = json!(offset + timing["end_ms"].as_i64().unwrap());
        timing
    }));
    file.write_all(pcm).await.map_err(fail_io)?;
    file.sync_data().await.map_err(fail_io)?;
    parts.pcm_bytes += pcm.len();
    parts.chunks_done += 1;
    let (a, c, j, saved) = (
        ctx.audiobook_id.clone(),
        chapter.to_string(),
        job_id.to_string(),
        parts.clone(),
    );
    state
        .store
        .run(move |conn| {
            let tx = conn.unchecked_transaction()?;
            save_prefix(&tx, &a, &c, &saved)?;
            if let Some((characters, timing)) = sample {
                if let Some(mut g) = generation_row(&tx, &j)? {
                    if g.chapter_id == c {
                        g.requests_done = saved.chunks_done;
                        g.characters_done = characters_done;
                        record_sample(&mut g, characters, timing);
                        save_generation(&tx, &j, &g)?;
                    }
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
    announce(state, job_id);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn free_request(
    state: &AppState,
    job_id: &str,
    ctx: &Ctx,
    chapter: &str,
    audio_id: &str,
    index: usize,
    range: &std::ops::Range<usize>,
    seed: i64,
    cancellation: watch::Receiver<bool>,
) -> Result<RetainedRequest, Stop> {
    if *cancellation.borrow() {
        return Err(Stop::Interrupted);
    }
    let text = request_text(ctx, range);
    let (speech, timing) = attempt(
        state,
        job_id,
        Some((chapter, &ctx.attempt_id)),
        false,
        Some(cancellation),
        || async {
            breeze::speak(
                ctx.base_url.as_deref().unwrap_or_default(),
                ctx.api_key.as_deref(),
                &ctx.voice,
                seed,
                &text,
            )
            .await
            .map_err(Fail::from)
        },
    )
    .await?;
    // Once the provider returns, finish persistence even if another request
    // fails. The controller drains this stage before starting another attempt.
    let timings = request_timings(&ctx.lines[range.clone()], &speech.pcm, &speech.segments);
    let retained = chapter_requests::persist(
        state,
        &ctx.audiobook_id,
        chapter,
        audio_id,
        index,
        &speech.pcm,
        &timings,
    )
    .await
    .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
    let (j, c, chars, attempt) = (
        job_id.to_string(),
        chapter.to_string(),
        text.chars().count() as i64,
        ctx.attempt_id.clone(),
    );
    state
        .store
        .run(move |conn| {
            if let Some(mut g) = generation_row(conn, &j)? {
                if g.chapter_id == c && g.attempt_id == attempt {
                    g.requests_done += 1;
                    g.characters_done = g.characters_done.saturating_add(chars);
                    record_sample(&mut g, chars, timing);
                    save_generation(conn, &j, &g)?;
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
    announce(state, job_id);
    Ok(retained)
}

/// Recheck the scheduler at retained-request boundaries, including a listener
/// moving another chapter to the front of this same job.
async fn yield_to_priority(state: &AppState, job_id: &str, chapter_id: &str) -> Result<bool, Stop> {
    let (j, ch, at) = (job_id.to_string(), chapter_id.to_string(), state.now());
    let yielded = state.store.run(move |conn| {
        let running: bool = conn.query_row("SELECT state='running' FROM jobs WHERE id=?1", [&j], |r| r.get(0))?;
        if running && next_item(conn)?.is_some_and(|next| next != (j.clone(), ch)) {
            conn.execute("UPDATE jobs SET state='queued',current_chapter_id=NULL,updated_at=?2 WHERE id=?1 AND state='running'", params![j, at])?;
            return Ok(true);
        }
        Ok(false)
    }).await.map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
    if yielded {
        announce(state, job_id);
    }
    Ok(yielded)
}

#[allow(clippy::too_many_arguments)]
async fn make_free_passages(
    state: &AppState,
    job_id: &str,
    ctx: &Ctx,
    chapter: &str,
    seed: i64,
    chunks: &[std::ops::Range<usize>],
    parts: &mut Parts,
    file: &mut tokio::fs::File,
    mut retained: BTreeMap<usize, RetainedRequest>,
) -> Result<(), Stop> {
    use futures_util::{stream::FuturesUnordered, StreamExt};
    let audio_id = parts.audio_id.clone();
    let mut pending = FuturesUnordered::new();
    let (cancellation, cancel_receiver) = watch::channel(false);
    let mut next = parts.chunks_done;
    let result = async {
        loop {
            while let Some(request) = retained.remove(&parts.chunks_done) {
                let pcm = chapter_requests::read(state, &ctx.audiobook_id, &audio_id, &request)
                    .await
                    .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
                append_request(
                    state,
                    job_id,
                    ctx,
                    chapter,
                    parts,
                    file,
                    &pcm,
                    &request.timings,
                    None,
                    0,
                )
                .await?;
                chapter_requests::discard_prefix(
                    state,
                    &ctx.audiobook_id,
                    chapter,
                    &audio_id,
                    parts.chunks_done,
                )
                .await
                .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
            }
            if parts.chunks_done == chunks.len() {
                return Ok(());
            }
            if !is_active(state, job_id).await || yield_to_priority(state, job_id, chapter).await? {
                return Err(Stop::Interrupted);
            }
            while pending.len() < state.config.breeze_concurrency && next < chunks.len() {
                let index = next;
                next += 1;
                if index < parts.chunks_done || retained.contains_key(&index) {
                    continue;
                }
                pending.push(free_request(
                    state,
                    job_id,
                    ctx,
                    chapter,
                    &audio_id,
                    index,
                    &chunks[index],
                    seed,
                    cancel_receiver.clone(),
                ));
            }
            let request = pending.next().await.ok_or_else(|| {
                Stop::Failed(Fail::Failed("Missing retained audio request.".into()))
            })??;
            // No PCM accumulates here; completed requests already live on disk.
            retained.insert(request.index, request);
        }
    }
    .await;
    if result.is_err() {
        let _ = cancellation.send(true);
        // Speech futures stop promptly; a request already saving durable bytes
        // finishes its checkpoint. No blocking store write can race recovery.
        while pending.next().await.is_some() {}
    }
    result
}

async fn sync_audio_directory(state: &AppState, dir: &std::path::Path) -> Result<(), ApiError> {
    #[cfg(unix)]
    {
        let dirs = [
            dir.to_path_buf(),
            state.store.data_dir().join("audio"),
            state.store.data_dir().to_path_buf(),
        ];
        tokio::task::spawn_blocking(move || {
            for dir in dirs {
                std::fs::File::open(dir)?.sync_all()?;
            }
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    }
    #[cfg(not(unix))]
    let _ = (state, dir);
    Ok(())
}

async fn make_chapter(
    state: &AppState,
    job_id: &str,
    ctx: &Ctx,
    chapter_id: &str,
) -> Result<Made, Stop> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    let key = ctx.api_key.as_deref();
    let breeze_base = ctx.base_url.as_deref().unwrap_or_default();
    let mut seed = 0;
    if ctx.source == "breeze" {
        let ((live, s), _) = attempt(state, job_id, None, false, None, || async {
            breeze::live_voice(breeze_base, key, &ctx.voice)
                .await
                .map_err(Fail::from)
        })
        .await?;
        if live != ctx.revision {
            return Err(Stop::Failed(Fail::VoiceChanged));
        }
        seed = s;
    }
    let dir = state.store.data_dir().join("audio").join(&ctx.audiobook_id);
    tokio::fs::create_dir_all(&dir).await.map_err(fail_io)?;
    let mut parts = load_parts(state, ctx, chapter_id, &dir)
        .await
        .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
    let chunks = audio::chunk_lines(&ctx.lines, parts.chunk_chars);
    let characters = chunk_characters(&ctx.lines, &chunks);
    let tmp = dir.join(format!("{chapter_id}.part"));

    // A paid chapter is started only if the rest of it fits under the limits, so it is not
    // abandoned half paid for. Each request then reserves its own share.
    if let Some((plan_id, limit)) = &ctx.plan {
        let remaining: i64 = chunks[parts.chunks_done..]
            .iter()
            .map(|r| ctx.lines[r.end - 1].end - ctx.lines[r.start].start)
            .sum();
        let need = plans::estimate(remaining, ctx.price).high;
        let (pid, l, now) = (plan_id.clone(), *limit, state.clock.now());
        let verdict = state
            .store
            .run(move |c| spend::check(c, Some((&pid, l)), need, now))
            .await
            .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
        match verdict {
            Err(spend::Denied::Plan) => return Err(Stop::Failed(Fail::Limit)),
            Err(spend::Denied::Allowance) => return Err(Stop::Failed(Fail::Allowance)),
            Err(spend::Denied::Stopped) => return Err(Stop::Interrupted),
            Ok(()) => {}
        }
    }

    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&tmp)
        .await
        .map_err(fail_io)?;
    if parts.chunks_done == 0 {
        file.set_len(0).await.map_err(fail_io)?;
        file.write_all(&[0u8; 44]).await.map_err(fail_io)?;
        parts.pcm_bytes = 0;
        parts.timings.clear();
    }
    file.seek(std::io::SeekFrom::End(0))
        .await
        .map_err(fail_io)?;

    file.sync_all().await.map_err(fail_io)?;
    sync_audio_directory(state, &dir)
        .await
        .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
    let (a, c, saved) = (
        ctx.audiobook_id.clone(),
        chapter_id.to_string(),
        parts.clone(),
    );
    state
        .store
        .run(move |conn| save_prefix(conn, &a, &c, &saved))
        .await
        .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
    let retained = if ctx.source == "breeze" {
        chapter_requests::load(
            state,
            &ctx.audiobook_id,
            chapter_id,
            &parts.audio_id,
            chunks.len(),
            parts.chunks_done,
        )
        .await
        .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?
        .into_iter()
        .map(|request| (request.index, request))
        .collect::<BTreeMap<_, _>>()
    } else {
        BTreeMap::new()
    };
    let (
        id,
        chapter,
        requests_done,
        requests_total,
        characters_done,
        characters_total,
        chunk_chars,
        parallelism,
        attempt_id,
    ) = (
        job_id.to_string(),
        chapter_id.to_string(),
        parts.chunks_done + retained.len(),
        chunks.len(),
        characters[..parts.chunks_done].iter().sum::<i64>()
            + retained.keys().map(|&i| characters[i]).sum::<i64>(),
        characters.iter().sum::<i64>(),
        state.config.audio_chunk_chars as i64,
        if ctx.source == "breeze" {
            state.config.breeze_concurrency
        } else {
            1
        },
        ctx.attempt_id.clone(),
    );
    state
        .store
        .run(move |conn| {
            let previous = generation_row(conn, &id)?.unwrap_or_default();
            let same_capacity = previous.parallelism.max(1) == parallelism;
            save_generation(
                conn,
                &id,
                &Generation {
                    chapter_id: chapter,
                    requests_done,
                    requests_total,
                    characters_done,
                    characters_total,
                    sample_characters: if same_capacity {
                        previous.sample_characters
                    } else {
                        0
                    },
                    sample_seconds: if same_capacity {
                        previous.sample_seconds
                    } else {
                        0.0
                    },
                    chunk_chars,
                    parallelism,
                    attempt_id,
                    ..Default::default()
                },
            )
        })
        .await
        .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
    announce(state, job_id);

    if ctx.source == "breeze" {
        make_free_passages(
            state, job_id, ctx, chapter_id, seed, &chunks, &mut parts, &mut file, retained,
        )
        .await?;
    } else {
        // Premium requests continue to reserve and settle one passage at a time.
        for range in chunks.iter().skip(parts.chunks_done) {
            let text = request_text(ctx, range);
            let (pcm, timing) = gemini_chunk(state, job_id, ctx, chapter_id, &text).await?;
            let timings = request_timings(&ctx.lines[range.clone()], &pcm, &[]);
            let done = parts.chunks_done + 1;
            append_request(
                state,
                job_id,
                ctx,
                chapter_id,
                &mut parts,
                &mut file,
                &pcm,
                &timings,
                Some((text.chars().count() as i64, timing)),
                characters[..done].iter().sum(),
            )
            .await?;
        }
    }

    file.seek(std::io::SeekFrom::Start(0))
        .await
        .map_err(fail_io)?;
    file.write_all(&audio::wav_header(parts.pcm_bytes as u32))
        .await
        .map_err(fail_io)?;
    file.sync_all().await.map_err(fail_io)?;
    file.seek(std::io::SeekFrom::Start(0))
        .await
        .map_err(fail_io)?;
    let (mut hasher, mut buf) = (Sha256::new(), vec![0u8; 1 << 20]);
    loop {
        let n = file.read(&mut buf).await.map_err(fail_io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    drop(file);
    let rel = format!("audio/{}/{}.wav", ctx.audiobook_id, parts.audio_id);
    tokio::fs::rename(&tmp, state.store.data_dir().join(&rel))
        .await
        .map_err(fail_io)?;
    sync_audio_directory(state, &dir)
        .await
        .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
    Ok(Made {
        audio_id: parts.audio_id,
        path: rel,
        bytes: 44 + parts.pcm_bytes as i64,
        sha256: hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        duration_ms: audio::pcm_ms(parts.pcm_bytes),
        timings: parts.timings,
    })
}

async fn needs_you(state: &AppState, job_id: &str, code: &str, text: &str) {
    let (id, at, d) = (job_id.to_string(), state.now(), detail(code, text, None));
    let _ = state
        .store
        .run(move |c| {
            set_state(
                c,
                &id,
                &["queued", "running", "waiting"],
                "needs_you",
                None,
                Some(d),
                &at,
            )
        })
        .await;
    announce(state, job_id);
}

async fn process(state: &AppState, job_id: &str, chapter_id: &str) -> Result<(), ApiError> {
    let (j, c, at) = (job_id.to_string(), chapter_id.to_string(), state.now());
    let attempt_id = state.new_id();
    let Some(ctx) = state
        .store
        .run_audio(
            crate::maintenance::AudioScope::Job(j.clone()),
            at.clone(),
            move |c2| load_ctx(c2, &j, &c, &at, attempt_id),
        )
        .await?
    else {
        return Ok(());
    };
    announce(state, job_id);
    let finish_item = |item_state: &'static str, detail: Option<String>, made: Option<Made>| {
        let (j, c, at, a) = (
            job_id.to_string(),
            chapter_id.to_string(),
            state.now(),
            ctx.audiobook_id.clone(),
        );
        let revision = ctx.revision.clone();
        let store = state.store.clone();
        async move {
            store
                .run(move |conn| {
                    let tx = conn.unchecked_transaction()?;
                    if let Some(m) = made {
                        tx.execute(
                            "INSERT INTO audio(id,audiobook_id,chapter_id,voice_revision,path,bytes,sha256,duration_ms,content_type,timings,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'audio/wav',?9,?10)",
                            params![m.audio_id, a, c, revision, m.path, m.bytes, m.sha256, m.duration_ms, Value::Array(m.timings).to_string(), at],
                        )?;
                        tx.execute("DELETE FROM chapter_parts WHERE audiobook_id=?1 AND chapter_id=?2", params![a, c])?;
                    }
                    tx.execute("UPDATE job_items SET state=?3, detail=?4 WHERE job_id=?1 AND chapter_id=?2", params![j, c, item_state, detail])?;
                    tx.execute("UPDATE jobs SET current_chapter_id=NULL WHERE id=?1 AND current_chapter_id=?2", params![j, c])?;
                    recount(&tx, &j, &at)?;
                    tx.commit()?;
                    Ok(())
                })
                .await
        }
    };
    if ctx.has_audio {
        finish_item("done", None, None).await?;
        announce(state, job_id);
        return Ok(());
    }
    if ctx.lines.is_empty() {
        finish_item(
            "skipped",
            Some(detail(
                "nothing_to_say",
                "This chapter has no text to speak.",
                None,
            )),
            None,
        )
        .await?;
        announce(state, job_id);
        return Ok(());
    }
    if ctx.source == "breeze" && ctx.base_url.is_none()
        || ctx.source == "gemini" && ctx.api_key.is_none()
    {
        needs_you(
            state,
            job_id,
            "source_not_set_up",
            "The voice source for this audiobook is not set up. Set it up, then resume.",
        )
        .await;
        return Ok(());
    }
    if ctx.source == "gemini" && ctx.plan.is_none() {
        // Premium audio is only ever made under an approved plan.
        needs_you(
            state,
            job_id,
            "plan_required",
            "Premium audio is made only under an approved plan.",
        )
        .await;
        return Ok(());
    }
    let result = make_chapter(state, job_id, &ctx, chapter_id).await;
    finish_generation_attempt(state, job_id, &ctx.attempt_id).await;
    match result {
        Ok(made) => {
            let audio_id = made.audio_id.clone();
            finish_item("done", None, Some(made)).await?;
            chapter_requests::cleanup(state, &ctx.audiobook_id, &audio_id).await?;
            let mut n = Notice::new("audiobook.updated", state.now()).with_id(ctx.audiobook_id.clone());
            n.book_id = Some(ctx.book_id.clone());
            state.notify(n);
            announce(state, job_id);
        }
        Err(Stop::Interrupted) => {}
        Err(Stop::Failed(e)) => match e {
            Fail::Unreachable => needs_you(state, job_id, "source_unreachable", "Could not reach the voice server. Check that it is running, then resume.").await,
            Fail::KeyRejected => needs_you(state, job_id, "key_rejected", "The voice source rejected the API key.").await,
            Fail::VoiceGone => needs_you(state, job_id, "voice_not_found", "The voice is no longer on the voice server.").await,
            Fail::VoiceChanged => needs_you(state, job_id, "voice_changed", "The voice sounds different from when this audiobook was started. Make a new audiobook with the current voice.").await,
            Fail::Busy(_) => needs_you(state, job_id, "source_busy", "The voice server stayed busy. Resume to try again.").await,
            Fail::Limit => needs_you(state, job_id, "limit_exceeded", "The next chapter could pass this plan's limit. What is finished is kept. Raise the limit to continue.").await,
            Fail::Allowance => needs_you(state, job_id, "allowance_exceeded", "The next chapter could pass this month's Allowance. What is finished is kept.").await,
            Fail::Quota(secs, daily) => {
                let until = crate::clock::ts(state.clock.now() + chrono::Duration::seconds(secs as i64));
                let text = if daily { "The daily request quota is used up. Bardic will continue when it resets, inside the same limit." } else { "The provider's quota or rate limit was reached. Bardic will continue shortly, inside the same limit." };
                let (id, at, d, w) = (job_id.to_string(), state.now(), detail("waiting_quota", text, Some(&until)), until);
                let _ = state.store.run(move |c| set_state_wake(c, &id, &["queued", "running", "waiting"], "waiting", Some(d), None, Some(w), &at)).await;
                announce(state, job_id);
            }
            other => {
                let (code, text) = match other {
                    Fail::Refused(code) => ("provider_refused", format!("The voice service refused this chapter ({code}).")),
                    Fail::NoAudio => ("no_audio", "The voice service answered without audio. The request is counted as spent.".to_string()),
                    Fail::Uncertain => ("uncertain", "The request may have been processed, so it was not sent again. Its cost is counted as unknown.".to_string()),
                    Fail::Failed(msg) => ("failed", msg),
                    _ => ("failed", "The chapter could not be made.".to_string()),
                };
                finish_item("failed", Some(detail(code, &text, None)), None).await?;
                announce(state, job_id);
                // A paid job that keeps failing stops and waits for the listener.
                if ctx.plan.is_some() {
                    let j = job_id.to_string();
                    let failed: i64 = state.store.run(move |c| Ok(c.query_row("SELECT COUNT(*) FROM job_items WHERE job_id=?1 AND state='failed'", [&j], |r| r.get(0))?)).await?;
                    if failed >= 3 {
                        needs_you(state, job_id, "repeated_failure", "Three chapters failed in a row, so the plan stopped spending. What is finished is kept.").await;
                    }
                }
            }
        },
    }
    Ok(())
}

/// Jobs with nothing left to do are completed, or need attention if a chapter failed.
async fn finalize_idle(state: &AppState) -> Result<(), ApiError> {
    let at = state.now();
    let done: Vec<String> = state
        .store
        .run(move |c| {
            // A job waiting on a quota goes back in the queue when its time comes.
            c.execute("UPDATE jobs SET state='queued', waiting=NULL, wake_at=NULL, updated_at=?1 WHERE state='waiting' AND wake_at IS NOT NULL AND wake_at<=?1", [&at])?;
            let ids: Vec<String> = c
                .prepare("SELECT id FROM jobs WHERE kind='make_audio' AND state IN ('queued','running') AND NOT EXISTS (SELECT 1 FROM job_items WHERE job_id=jobs.id AND state='queued')")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for id in &ids {
                let failed: i64 = c.query_row("SELECT COUNT(*) FROM job_items WHERE job_id=?1 AND state='failed'", [id], |r| r.get(0))?;
                if failed > 0 {
                    let d = detail("provider_refused", &format!("{failed} chapter(s) could not be made. Resume to try them again."), None);
                    c.execute("UPDATE jobs SET state='needs_you', needs_you=?2, current_chapter_id=NULL, updated_at=?3 WHERE id=?1", params![id, d, at])?;
                } else {
                    c.execute("UPDATE jobs SET state='completed', current_chapter_id=NULL, waiting=NULL, needs_you=NULL, updated_at=?2 WHERE id=?1", params![id, at])?;
                }
            }
            Ok(ids)
        })
        .await?;
    for id in done {
        announce(state, &id);
    }
    Ok(())
}

fn next_item(conn: &Connection) -> Result<Option<(String, String)>, ApiError> {
    Ok(conn
        .query_row(
            "SELECT i.job_id,i.chapter_id FROM job_items i JOIN jobs j ON j.id=i.job_id
             WHERE i.state='queued' AND j.state IN ('queued','running')
             ORDER BY j.urgent DESC, j.created_at, i.position LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// The worker loop. Runs until shutdown.
pub async fn run_worker(state: AppState) {
    // Work that was in flight when the server stopped: a job goes back in the queue, and a paid
    // request that may have been sent is recorded as unknown, never as zero.
    let _ = state
        .store
        .run(|c| {
            // No process-local request is still active after a restart. Leave
            // throughput samples and durable chapter parts available to resume.
            c.execute("UPDATE jobs SET generation=json_set(generation,'$.request_started_at',NULL,'$.active_requests',0,'$.active_started_at',json('{}'),'$.sample_intervals',json('[]'),'$.elapsed_intervals',json('[]')) WHERE generation IS NOT NULL", [])?;
            c.execute("UPDATE jobs SET state='queued', current_chapter_id=NULL WHERE kind='make_audio' AND (state='running' OR (state='waiting' AND wake_at IS NULL))", [])?;
            spend::recover(c)?;
            // An export or backup that was running did not finish.
            c.execute("UPDATE exports SET state='failed', error='The server stopped while exporting. Export again.' WHERE state='running'", [])?;
            c.execute("UPDATE jobs SET state='failed' WHERE kind='export' AND state IN ('queued','running')", [])?;
            c.execute("UPDATE backups SET state='failed', error='The server stopped during the backup.' WHERE state='running'", [])?;
            Ok(())
        })
        .await;
    let mut shutdown = state.shutdown.subscribe();
    let mut last_refresh = tokio::time::Instant::now();
    // Leftovers from a crash or a deletion that raced a chapter being written.
    let _ = crate::maintenance::sweep_orphans(&state).await;
    while !*shutdown.borrow() {
        if last_refresh.elapsed() >= Duration::from_secs(24 * 3600) {
            last_refresh = tokio::time::Instant::now();
            crate::api::voices::refresh_all(&state).await;
        }
        if let Err(e) = crate::maintenance::run_due_deletions(&state).await {
            tracing::error!(error = %e.detail, "running deletions");
        }
        if let Err(e) = finalize_idle(&state).await {
            tracing::error!(error = %e.detail, "finalizing jobs");
        }
        match state.store.run(|c| next_item(c)).await {
            Ok(Some((job, chapter))) => {
                if let Err(e) = process(&state, &job, &chapter).await {
                    tracing::error!(error = %e.detail, "making audio");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            Ok(None) => {
                // While a job waits for a quota, look again every second; otherwise sleep until poked.
                let waiting = state.store.run(|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE state='waiting' AND wake_at IS NOT NULL)", [], |r| r.get::<_, bool>(0))?)).await.unwrap_or(false);
                let deleting = state
                    .store
                    .run(|c| Ok(crate::maintenance::pending(c)))
                    .await
                    .unwrap_or(false);
                let nap = Duration::from_secs(if waiting || deleting { 1 } else { 30 });
                tokio::select! {
                    _ = state.jobs.wake.notified() => {}
                    _ = shutdown.changed() => {}
                    _ = tokio::time::sleep(nap) => {}
                }
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

#[cfg(test)]
mod concurrency_progress_tests {
    use super::*;
    use crate::{clock::SystemClock, config::Config};
    use std::sync::Arc;

    fn fixture() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(Config::for_data_dir(dir.path()), Arc::new(SystemClock)).unwrap();
        state.store.run_blocking(|c| {
            c.execute_batch("INSERT INTO books(id,title,state,added_at) VALUES('book','Synthetic prefix recovery','readable','2026-10-06T00:00:00Z');
                INSERT INTO chapters(id,book_id,idx,title,kind,text,text_sha256,word_count) VALUES('chapter','book',0,'One','story','Alpha.\n\nBeta.\n\nGamma.','text',3);
                INSERT INTO lines(id,chapter_id,idx,start,end) VALUES('line0','chapter',0,0,6),('line1','chapter',1,8,13),('line2','chapter',2,15,21);
                INSERT INTO voices(id,source_id,external_id,name,tier,language,revision,updated_at) VALUES('voice','breeze','voice','Synthetic','free','en','r1','2026-10-06T00:00:00Z');
                INSERT INTO audiobooks(id,book_id,voice_id,voice_name,voice_revision,created_at) VALUES('audiobook','book','voice','Synthetic','r1','2026-10-06T00:00:00Z');
                INSERT INTO jobs(id,kind,state,audiobook_id,book_id,chapters_total,current_chapter_id,started_by,created_at,updated_at) VALUES('job','make_audio','running','audiobook','book',1,'chapter','{}','2026-10-06T00:00:00Z','2026-10-06T00:00:00Z');
                INSERT INTO job_items(job_id,chapter_id,position,state) VALUES('job','chapter',0,'queued');")
        }).unwrap();
        (dir, state)
    }

    #[test]
    fn successful_intervals_count_overlap_once_even_when_completion_is_out_of_order() {
        let mut generation = Generation {
            sample_characters: 70,
            sample_seconds: 7.0,
            ..Default::default()
        };
        let intervals = [
            (30.0, 5.0, 50, 12.0),
            (10.0, 10.0, 100, 22.0),
            (15.0, 20.0, 200, 32.0),
            (5.0, 5.0, 50, 37.0),
            (17.0, 1.0, 10, 37.0),
            (50.0, 10.0, 100, 47.0),
        ];
        for (started_at, seconds, characters, expected_seconds) in intervals {
            record_sample(
                &mut generation,
                characters,
                RequestTiming {
                    started_at,
                    seconds,
                },
            );
            assert_eq!(generation.sample_seconds, expected_seconds);
        }
        assert_eq!(generation.sample_characters, 580);
        assert_eq!(generation.sample_intervals, [[5.0, 35.0], [50.0, 60.0]]);

        // A new chapter resets its interval union while retaining prior samples.
        // Its wall-clock spans must never subtract or double-count earlier work.
        generation.chapter_id = "next-chapter".into();
        generation.sample_intervals.clear();
        record_sample(
            &mut generation,
            30,
            RequestTiming {
                started_at: 100.0,
                seconds: 3.0,
            },
        );
        record_sample(
            &mut generation,
            30,
            RequestTiming {
                started_at: 101.0,
                seconds: 3.0,
            },
        );
        assert_eq!(generation.sample_seconds, 51.0);
        assert_eq!(generation.sample_characters, 640);
        assert_eq!(generation.sample_intervals, [[100.0, 104.0]]);
    }

    #[test]
    fn elapsed_counts_overlapping_active_intervals_once_and_freezes_provider_endpoints() {
        let mut generation = Generation {
            elapsed_seconds: 10.0,
            elapsed_intervals: vec![[10.0, 20.0]],
            active_started_at: BTreeMap::from([("active-request".into(), 15.0)]),
            request_started_at: Some(15.0),
            ..Default::default()
        };
        assert_eq!(elapsed_seconds(&generation, 25.0), 15.0);

        // Provider work ended at 25, while its database cleanup waited until
        // 100. The recorded endpoint excludes that wait and the stale legacy
        // summary clock cannot add another 85 seconds.
        generation.active_started_at.clear();
        generation.elapsed_intervals.push([15.0, 25.0]);
        assert_eq!(elapsed_seconds(&generation, 100.0), 15.0);

        // A later disjoint request adds only its own active duration.
        generation.elapsed_intervals.push([30.0, 35.0]);
        assert_eq!(elapsed_seconds(&generation, 100.0), 20.0);
        assert_eq!(elapsed_seconds(&generation, 1000.0), 20.0);
    }

    #[tokio::test]
    async fn measured_throughput_is_not_divided_again_by_configured_capacity() {
        let (_dir, state) = fixture();
        let mut generation = Generation {
            chapter_id: "chapter".into(),
            requests_done: 4,
            requests_total: 10,
            characters_done: 400,
            characters_total: 1000,
            chunk_chars: 4,
            ..Default::default()
        };
        // Two successful 100-character requests overlap completely for 10 s:
        // measured throughput is already 20 characters per active second.
        record_sample(
            &mut generation,
            100,
            RequestTiming {
                started_at: 10.0,
                seconds: 10.0,
            },
        );
        record_sample(
            &mut generation,
            100,
            RequestTiming {
                started_at: 10.0,
                seconds: 10.0,
            },
        );
        assert_eq!(generation.sample_seconds, 10.0);
        assert_eq!(generation.sample_characters, 200);
        for capacity in [1, 2, 16] {
            generation.parallelism = capacity;
            let encoded = serde_json::to_string(&generation).unwrap();
            let progress = state
                .store
                .run(move |c| {
                    c.execute("UPDATE jobs SET generation=?1 WHERE id='job'", [encoded])?;
                    generation_value(
                        c,
                        &json!({
                            "id": "job", "kind": "make_audio", "state": "running",
                            "current_chapter_id": "chapter"
                        }),
                    )
                })
                .await
                .unwrap();
            // 600 ungenerated characters / 20 measured characters per second.
            assert_eq!(progress["chapter_seconds_remaining"], 30.0);
            assert_eq!(progress["job_seconds_remaining"], 30.0);
        }
    }

    #[tokio::test]
    async fn finishing_an_old_attempt_cannot_clear_a_new_attempt_clock_or_samples() {
        let (_dir, state) = fixture();
        let generation = Generation {
            chapter_id: "chapter".into(),
            attempt_id: "new-attempt".into(),
            active_requests: 2,
            request_started_at: Some(wall_seconds()),
            elapsed_seconds: 7.0,
            sample_characters: 100,
            sample_seconds: 5.0,
            ..Default::default()
        };
        let encoded = serde_json::to_string(&generation).unwrap();
        let saved = encoded.clone();
        state
            .store
            .run(move |c| {
                c.execute("UPDATE jobs SET generation=?1 WHERE id='job'", [saved])?;
                Ok(())
            })
            .await
            .unwrap();
        finish_generation_attempt(&state, "job", "old-attempt").await;
        let unchanged: String = state
            .store
            .run(|c| {
                Ok(
                    c.query_row("SELECT generation FROM jobs WHERE id='job'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!(unchanged, encoded);
        finish_generation_attempt(&state, "job", "new-attempt").await;
        let ended = state
            .store
            .run(|c| generation_row(c, "job"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ended.active_requests, 0);
        assert!(ended.request_started_at.is_none());
        assert!(ended.attempt_id.is_empty());
        assert_eq!(ended.sample_characters, 100);
        assert_eq!(ended.sample_seconds, 5.0);
        assert!(ended.elapsed_seconds >= 7.0 && ended.elapsed_seconds.is_finite());
    }

    #[tokio::test]
    async fn appended_uncommitted_tail_keeps_the_synced_prefix_and_indexed_request() {
        use tokio::io::AsyncWriteExt;
        let (_dir, state) = fixture();
        let prefix_timings = vec![json!({ "line_id": "line0", "start_ms": 0, "end_ms": 100 })];
        let encoded = serde_json::to_string(&prefix_timings).unwrap();
        state.store.run(move |c| {
            c.execute("INSERT INTO chapter_parts(audiobook_id,chapter_id,audio_id,chunk_chars,chunks_done,pcm_bytes,timings) VALUES('audiobook','chapter','saved-audio',4,1,4800,?1)", [encoded])?;
            Ok(())
        }).await.unwrap();
        let second_timings = vec![json!({ "line_id": "line1", "start_ms": 0, "end_ms": 100 })];
        let retained = chapter_requests::persist(
            &state,
            "audiobook",
            "chapter",
            "saved-audio",
            1,
            &[2; 4800],
            &second_timings,
        )
        .await
        .unwrap();
        let audio_dir = state.store.data_dir().join("audio/audiobook");
        let prefix_path = audio_dir.join("chapter.part");
        let mut prefix = tokio::fs::File::create(&prefix_path).await.unwrap();
        prefix.write_all(&[0; 44]).await.unwrap();
        prefix.write_all(&[1; 4800]).await.unwrap();
        prefix.write_all(&[2; 4800]).await.unwrap();
        prefix.sync_all().await.unwrap();
        drop(prefix);
        assert_eq!(tokio::fs::metadata(&prefix_path).await.unwrap().len(), 9644);

        let at = state.now();
        let ctx = state
            .store
            .run(move |c| load_ctx(c, "job", "chapter", &at, "recovery-attempt".into()))
            .await
            .unwrap()
            .unwrap();
        let parts = load_parts(&state, &ctx, "chapter", &audio_dir)
            .await
            .unwrap();
        assert_eq!(parts.audio_id, "saved-audio");
        assert_eq!(
            parts.chunk_chars, 4,
            "the saved request boundaries remain pinned"
        );
        assert_eq!(parts.chunks_done, 1);
        assert_eq!(parts.pcm_bytes, 4800);
        assert_eq!(parts.timings, prefix_timings);
        let prefix = tokio::fs::read(&prefix_path).await.unwrap();
        assert_eq!(prefix.len(), 4844, "only the uncommitted tail is truncated");
        assert_eq!(&prefix[44..], &[1; 4800]);
        let requests = chapter_requests::load(&state, "audiobook", "chapter", "saved-audio", 3, 1)
            .await
            .unwrap();
        assert_eq!(requests.iter().map(|r| r.index).collect::<Vec<_>>(), [1]);
        assert_eq!(requests[0].sha256, retained.sha256);
        assert_eq!(
            chapter_requests::read(&state, "audiobook", "saved-audio", &requests[0])
                .await
                .unwrap(),
            [2; 4800]
        );
        let stored: (i64, i64) = state.store.run(move |c| {
            Ok(c.query_row("SELECT chunks_done,pcm_bytes FROM chapter_parts WHERE audiobook_id='audiobook' AND chapter_id='chapter'", [], |r| Ok((r.get(0)?,r.get(1)?)))?)
        }).await.unwrap();
        assert_eq!(stored, (1, 4800));
    }
}
