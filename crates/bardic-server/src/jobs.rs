//! Jobs and the worker that makes audio.
//!
//! One worker makes one request at a time, because the voice server is one
//! machine's GPU. A job is a list of chapters to make for an audiobook. The
//! worker always takes the next chapter of the oldest *urgent* job (someone
//! pressed play), then of the oldest other job. A chapter is written only
//! when complete, so stopping, crashing or losing the voice server never
//! leaves a half-made chapter behind and finished chapters are kept.

use crate::{
    app::AppState,
    audio::{self, Line},
    error::ApiError,
    events::Notice,
    plans, spend,
    voices::{
        breeze::{self, SpeakError},
        gemini,
    },
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{future::Future, time::Duration};
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

/// The contract's `Job`.
pub fn job_value(conn: &Connection, id: &str) -> Result<Value, ApiError> {
    conn.query_row(
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
    .ok_or_else(|| ApiError::not_found("job_not_found", "No job has this id."))
}

pub fn announce(state: &AppState, job_id: &str) {
    state.notify(Notice::new("job.updated", state.now()).with_id(job_id.to_string()));
    state.jobs.poke();
}

/// Queue chapters on a job after its current items. Already queued or finished ones are kept as they are.
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
             ON CONFLICT(job_id,chapter_id) DO UPDATE SET state='queued', detail=NULL, position=excluded.position WHERE state IN ('failed','skipped')",
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
    tokio::pin!(fut);
    let mut changed = state.jobs.changed.subscribe();
    let mut shutdown = state.shutdown.subscribe();
    if *shutdown.borrow() || (!paid && !is_active(state, job_id).await) {
        return None;
    }
    loop {
        tokio::select! {
            v = &mut fut => return Some(v),
            _ = changed.changed() => { if !paid && !is_active(state, job_id).await { return None; } }
            _ = shutdown.changed() => return None,
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
async fn attempt<T, F, Fut>(state: &AppState, job_id: &str, paid: bool, mut f: F) -> Result<T, Stop>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Fail>>,
{
    const TRIES: u32 = 3;
    for n in 0..=TRIES {
        match interruptible(state, job_id, paid, f()).await {
            None => return Err(Stop::Interrupted),
            Some(Ok(v)) => return Ok(v),
            Some(Err(Fail::Unreachable)) if n < TRIES => {
                let wait = Duration::from_millis(state.config.job_retry_ms << n);
                if interruptible(state, job_id, paid, tokio::time::sleep(wait))
                    .await
                    .is_none()
                {
                    return Err(Stop::Interrupted);
                }
            }
            Some(Err(Fail::Busy(secs))) if n < TRIES => {
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
                if interruptible(state, job_id, paid, tokio::time::sleep(wait))
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
            Some(Err(e)) => return Err(Stop::Failed(e)),
        }
    }
    Err(Stop::Failed(Fail::Unreachable))
}

struct Ctx {
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
            "SELECT 1 FROM audio WHERE audiobook_id=?1 AND chapter_id=?2",
            [&audiobook_id, chapter_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let price: i64 = conn.query_row(
        "SELECT COALESCE((SELECT per_unit FROM prices WHERE provider=?1),0)",
        [&source],
        |r| r.get(0),
    )?;
    conn.execute("UPDATE jobs SET state='running', current_chapter_id=?2, updated_at=?3 WHERE id=?1 AND state='queued'", params![job_id, chapter_id, at])?;
    conn.execute(
        "UPDATE jobs SET current_chapter_id=?2 WHERE id=?1",
        params![job_id, chapter_id],
    )?;
    Ok(Some(Ctx {
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
struct Parts {
    audio_id: String,
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
        let len = tokio::fs::metadata(dir.join(format!("{chapter_id}.part")))
            .await
            .map(|m| m.len())
            .ok();
        if chars == state.config.audio_chunk_chars as i64 && len == Some(44 + bytes as u64) {
            return Ok(Parts {
                audio_id,
                chunks_done: done as usize,
                pcm_bytes: bytes as usize,
                timings: serde_json::from_str(&timings).unwrap_or_default(),
            });
        }
        // The chunking changed or the file is gone: start the chapter again.
        let _ = tokio::fs::remove_file(dir.join(format!("{chapter_id}.part"))).await;
    }
    Ok(Parts {
        audio_id: state.new_id(),
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
) -> Result<Vec<u8>, Stop> {
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
    let sid = spend_id.clone();
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
        let result = match interruptible(
            state,
            job_id,
            true,
            gemini::speak(&base, &key, &ctx.voice, text),
        )
        .await
        {
            // Shut down mid-request: it stays reserved and becomes unknown on the next start.
            None => return Err(Stop::Interrupted),
            Some(r) => r,
        };
        return match result {
            Ok(speech) => {
                match speech.usage.cost_micros() {
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
                            note: "Gemini did not report complete usage.",
                        })
                        .await
                    }
                }
                Ok(speech.pcm)
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
                        match u.cost_micros() {
                            Some(m) => spend::Outcome::Known {
                                micros: m,
                                input: u.input_tokens,
                                output: u.output_tokens,
                            },
                            None => spend::Outcome::Unknown {
                                note: "Gemini answered without audio and without complete usage.",
                            },
                        },
                        Fail::NoAudio,
                    ),
                    gemini::SpeakError::Uncertain => (
                        spend::Outcome::Unknown {
                            note: "The request may have been processed; it was not sent again.",
                        },
                        Fail::Uncertain,
                    ),
                    gemini::SpeakError::Failed(m) => (
                        spend::Outcome::Unknown {
                            note: "The response could not be used; the request was billed.",
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
        let (live, s) = attempt(state, job_id, false, || async {
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
    let chunks = audio::chunk_lines(&ctx.lines, state.config.audio_chunk_chars as i64);
    let mut parts = load_parts(state, ctx, chapter_id, &dir)
        .await
        .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
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

    for (i, range) in chunks.iter().enumerate().skip(parts.chunks_done) {
        let lines = &ctx.lines[range.clone()];
        let chunk_start = lines[0].start;
        let text: String = ctx.text
            [chunk_start as usize..lines.last().expect("non-empty chunk").end as usize]
            .iter()
            .collect();
        let (pcm, segments) = if ctx.source == "gemini" {
            (
                gemini_chunk(state, job_id, ctx, chapter_id, &text).await?,
                vec![],
            )
        } else {
            let s = attempt(state, job_id, false, || async {
                breeze::speak(breeze_base, key, &ctx.voice, seed, &text)
                    .await
                    .map_err(Fail::from)
            })
            .await?;
            (s.pcm, s.segments)
        };
        let chunk_ms = audio::pcm_ms(pcm.len());
        let offset_ms = audio::pcm_ms(parts.pcm_bytes);
        for (l, (s, e)) in
            lines
                .iter()
                .zip(audio::line_times(lines, chunk_start, chunk_ms, &segments))
        {
            parts.timings.push(
                json!({ "line_id": l.id, "start_ms": offset_ms + s, "end_ms": offset_ms + e }),
            );
        }
        file.write_all(&pcm).await.map_err(fail_io)?;
        file.flush().await.map_err(fail_io)?;
        parts.pcm_bytes += pcm.len();
        parts.chunks_done = i + 1;
        let (a, c, id, n, done, bytes, t) = (
            ctx.audiobook_id.clone(),
            chapter_id.to_string(),
            parts.audio_id.clone(),
            state.config.audio_chunk_chars as i64,
            parts.chunks_done as i64,
            parts.pcm_bytes as i64,
            Value::Array(parts.timings.clone()).to_string(),
        );
        state
            .store
            .run(move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO chapter_parts(audiobook_id,chapter_id,audio_id,chunk_chars,chunks_done,pcm_bytes,timings) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![a, c, id, n, done, bytes, t],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| Stop::Failed(Fail::Failed(e.detail)))?;
    }

    file.seek(std::io::SeekFrom::Start(0))
        .await
        .map_err(fail_io)?;
    file.write_all(&audio::wav_header(parts.pcm_bytes as u32))
        .await
        .map_err(fail_io)?;
    file.flush().await.map_err(fail_io)?;
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
    let Some(ctx) = state.store.run(move |c2| load_ctx(c2, &j, &c, &at)).await? else {
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
    match make_chapter(state, job_id, &ctx, chapter_id).await {
        Ok(made) => {
            finish_item("done", None, Some(made)).await?;
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
                .prepare("SELECT id FROM jobs WHERE state IN ('queued','running') AND NOT EXISTS (SELECT 1 FROM job_items WHERE job_id=jobs.id AND state='queued')")?
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
            c.execute("UPDATE jobs SET state='queued', current_chapter_id=NULL WHERE state='running' OR (state='waiting' AND wake_at IS NULL)", [])?;
            spend::recover(c)?;
            Ok(())
        })
        .await;
    let mut shutdown = state.shutdown.subscribe();
    let mut last_refresh = tokio::time::Instant::now();
    while !*shutdown.borrow() {
        if last_refresh.elapsed() >= Duration::from_secs(24 * 3600) {
            last_refresh = tokio::time::Instant::now();
            crate::api::voices::refresh_all(&state).await;
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
                let nap = Duration::from_secs(if waiting { 1 } else { 30 });
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
