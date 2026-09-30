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
    voices::breeze::{self, SpeakError},
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{future::Future, path::PathBuf, time::Duration};
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
    let list = from
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    let n = conn.execute(
        &format!("UPDATE jobs SET state=?2, waiting=?3, needs_you=?4, updated_at=?5 WHERE id=?1 AND state IN ({list})"),
        params![job_id, to, waiting, needs_you, at],
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
async fn interruptible<T>(
    state: &AppState,
    job_id: &str,
    fut: impl Future<Output = T>,
) -> Option<T> {
    tokio::pin!(fut);
    let mut changed = state.jobs.changed.subscribe();
    let mut shutdown = state.shutdown.subscribe();
    if *shutdown.borrow() || !is_active(state, job_id).await {
        return None;
    }
    loop {
        tokio::select! {
            v = &mut fut => return Some(v),
            _ = changed.changed() => { if !is_active(state, job_id).await { return None; } }
            _ = shutdown.changed() => return None,
        }
    }
}

enum Stop {
    Interrupted,
    Failed(SpeakError),
}

/// Try a request, waiting and retrying when the server is unreachable or busy.
async fn attempt<T, F, Fut>(state: &AppState, job_id: &str, mut f: F) -> Result<T, Stop>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SpeakError>>,
{
    const TRIES: u32 = 3;
    for n in 0..=TRIES {
        match interruptible(state, job_id, f()).await {
            None => return Err(Stop::Interrupted),
            Some(Ok(v)) => return Ok(v),
            Some(Err(SpeakError::Unreachable)) if n < TRIES => {
                let wait = Duration::from_millis(state.config.job_retry_ms << n);
                if interruptible(state, job_id, tokio::time::sleep(wait))
                    .await
                    .is_none()
                {
                    return Err(Stop::Interrupted);
                }
            }
            Some(Err(SpeakError::Busy(secs))) if n < TRIES => {
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
                if interruptible(state, job_id, tokio::time::sleep(wait))
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
    Err(Stop::Failed(SpeakError::Unreachable))
}

struct Ctx {
    audiobook_id: String,
    book_id: String,
    voice: String,
    revision: String,
    base_url: Option<String>,
    api_key: Option<String>,
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
    let row = conn
        .query_row(
            "SELECT a.id,a.book_id,v.external_id,a.voice_revision,s.config FROM jobs j
             JOIN audiobooks a ON a.id=j.audiobook_id JOIN voices v ON v.id=a.voice_id JOIN voice_sources s ON s.id=v.source_id WHERE j.id=?1",
            [job_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?)),
        )
        .optional()?;
    let Some((audiobook_id, book_id, voice, revision, config)) = row else {
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
    conn.execute("UPDATE jobs SET state='running', current_chapter_id=?2, updated_at=?3 WHERE id=?1 AND state='queued'", params![job_id, chapter_id, at])?;
    conn.execute(
        "UPDATE jobs SET current_chapter_id=?2 WHERE id=?1",
        params![job_id, chapter_id],
    )?;
    Ok(Some(Ctx {
        audiobook_id,
        book_id,
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

async fn make_chapter(
    state: &AppState,
    job_id: &str,
    ctx: &Ctx,
    base_url: &str,
) -> Result<Made, Stop> {
    let key = ctx.api_key.as_deref();
    let (live_revision, seed) = attempt(state, job_id, || {
        breeze::live_voice(base_url, key, &ctx.voice)
    })
    .await?;
    if live_revision != ctx.revision {
        return Err(Stop::Failed(SpeakError::VoiceChanged));
    }
    let audio_id = state.new_id();
    let rel = format!("audio/{}/{}.wav", ctx.audiobook_id, audio_id);
    let dir = state.store.data_dir().join("audio").join(&ctx.audiobook_id);
    let tmp = dir.join(format!("{audio_id}.part"));
    let fail =
        |e: std::io::Error| Stop::Failed(SpeakError::Failed(format!("Could not write audio: {e}")));
    tokio::fs::create_dir_all(&dir).await.map_err(fail)?;
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .await
        .map_err(fail)?;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    file.write_all(&[0u8; 44]).await.map_err(fail)?;

    let (mut total_pcm, mut timings) = (0usize, Vec::<Value>::new());
    let result: Result<(), Stop> = async {
        for range in audio::chunk_lines(&ctx.lines, state.config.audio_chunk_chars as i64) {
            let lines = &ctx.lines[range];
            let chunk_start = lines[0].start;
            let text: String = ctx.text
                [chunk_start as usize..lines.last().expect("non-empty chunk").end as usize]
                .iter()
                .collect();
            let speech = attempt(state, job_id, || {
                breeze::speak(base_url, key, &ctx.voice, seed, &text)
            })
            .await?;
            let chunk_ms = audio::pcm_ms(speech.pcm.len());
            let offset_ms = audio::pcm_ms(total_pcm);
            for (l, (s, e)) in lines.iter().zip(audio::line_times(
                lines,
                chunk_start,
                chunk_ms,
                &speech.segments,
            )) {
                timings.push(
                    json!({ "line_id": l.id, "start_ms": offset_ms + s, "end_ms": offset_ms + e }),
                );
            }
            file.write_all(&speech.pcm).await.map_err(fail)?;
            total_pcm += speech.pcm.len();
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        drop(file);
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    let header = audio::wav_header(total_pcm as u32);
    file.seek(std::io::SeekFrom::Start(0)).await.map_err(fail)?;
    file.write_all(&header).await.map_err(fail)?;
    file.flush().await.map_err(fail)?;
    file.seek(std::io::SeekFrom::Start(0)).await.map_err(fail)?;
    let (mut hasher, mut buf) = (Sha256::new(), vec![0u8; 1 << 20]);
    loop {
        let n = file.read(&mut buf).await.map_err(fail)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    drop(file);
    let final_path = state.store.data_dir().join(&rel);
    tokio::fs::rename(&tmp, &final_path).await.map_err(fail)?;
    Ok(Made {
        audio_id,
        path: rel,
        bytes: 44 + total_pcm as i64,
        sha256: hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        duration_ms: audio::pcm_ms(total_pcm),
        timings,
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
    let Some(base_url) = ctx.base_url.clone() else {
        needs_you(
            state,
            job_id,
            "source_not_set_up",
            "The voice source for this audiobook is not set up. Set it up, then resume.",
        )
        .await;
        return Ok(());
    };
    match make_chapter(state, job_id, &ctx, &base_url).await {
        Ok(made) => {
            finish_item("done", None, Some(made)).await?;
            let mut n = Notice::new("audiobook.updated", state.now()).with_id(ctx.audiobook_id.clone());
            n.book_id = Some(ctx.book_id.clone());
            state.notify(n);
            announce(state, job_id);
        }
        Err(Stop::Interrupted) => {}
        Err(Stop::Failed(e)) => match e {
            SpeakError::Unreachable => needs_you(state, job_id, "source_unreachable", "Could not reach the voice server. Check that it is running, then resume.").await,
            SpeakError::KeyRejected => needs_you(state, job_id, "key_rejected", "The voice server rejected the API key.").await,
            SpeakError::VoiceGone => needs_you(state, job_id, "voice_not_found", "The voice is no longer on the voice server.").await,
            SpeakError::VoiceChanged => needs_you(state, job_id, "voice_changed", "The voice sounds different from when this audiobook was started. Make a new audiobook with the current voice.").await,
            SpeakError::Busy(_) => needs_you(state, job_id, "source_busy", "The voice server stayed busy. Resume to try again.").await,
            SpeakError::Refused(code) => {
                tracing::warn!(job = job_id, chapter = chapter_id, %code, "chapter refused");
                finish_item("failed", Some(detail("provider_refused", &format!("The voice server refused this chapter ({code})."), None)), None).await?;
                announce(state, job_id);
            }
            SpeakError::Failed(msg) => {
                tracing::warn!(job = job_id, chapter = chapter_id, %msg, "chapter failed");
                finish_item("failed", Some(detail("failed", &msg, None)), None).await?;
                announce(state, job_id);
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
    // A job that was running when the server stopped goes back in the queue.
    let _ = state.store.run(|c| Ok(c.execute("UPDATE jobs SET state='queued', current_chapter_id=NULL WHERE state IN ('running','waiting')", [])?)).await;
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
                tokio::select! {
                    _ = state.jobs.wake.notified() => {}
                    _ = shutdown.changed() => {}
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

pub fn audio_path(data_dir: &std::path::Path, rel: &str) -> PathBuf {
    data_dir.join(rel)
}
