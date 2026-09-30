//! Audiobooks: a book read in one voice at one voice revision.
//! Creating one is free and makes no audio.

use super::{audit, ApiJson};
use crate::{
    app::{Actor, AppState, DeviceCtx},
    error::ApiError,
    events::Notice,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

fn audiobook_not_found() -> ApiError {
    ApiError::not_found("audiobook_not_found", "No audiobook has this id.")
}

/// The contract's `Audiobook`.
pub fn audiobook_value(conn: &Connection, id: &str) -> Result<Value, ApiError> {
    let row = conn
        .query_row(
            "SELECT a.id,a.book_id,a.voice_id,a.voice_name,v.source_id,v.tier,a.voice_revision,a.created_at,b.chapter_total,
                    (SELECT COUNT(*) FROM audio x WHERE x.audiobook_id=a.id),
                    (SELECT COALESCE(SUM(bytes),0) FROM audio x WHERE x.audiobook_id=a.id),
                    (SELECT j.id FROM jobs j WHERE j.audiobook_id=a.id AND j.state IN ('queued','running','waiting','paused','needs_you') ORDER BY j.created_at DESC LIMIT 1)
             FROM audiobooks a JOIN voices v ON v.id=a.voice_id
             JOIN (SELECT book_id, COUNT(*) AS chapter_total FROM chapters GROUP BY book_id) b ON b.book_id=a.book_id
             WHERE a.id=?1",
            [id],
            |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?,
                    "book_id": r.get::<_, String>(1)?,
                    "voice_id": r.get::<_, String>(2)?,
                    "voice_name": r.get::<_, String>(3)?,
                    "source_id": r.get::<_, String>(4)?,
                    "tier": r.get::<_, String>(5)?,
                    "voice_revision": r.get::<_, String>(6)?,
                    "chapters_total": r.get::<_, i64>(8)?,
                    "chapters_ready": r.get::<_, i64>(9)?,
                    "bytes": r.get::<_, i64>(10)?,
                    "created_at": r.get::<_, String>(7)?,
                    "active_job_id": r.get::<_, Option<String>>(11)?,
                }))
            },
        )
        .optional()?;
    row.ok_or_else(audiobook_not_found)
}

fn book_state(conn: &Connection, book: &str) -> Result<String, ApiError> {
    let s: Option<String> = conn
        .query_row("SELECT state FROM books WHERE id=?1", [book], |r| r.get(0))
        .optional()?;
    match s.as_deref() {
        None | Some("deleting") => Err(super::books::book_not_found()),
        Some(s) => Ok(s.to_string()),
    }
}

/// `listAudiobooks`
pub async fn list(
    State(state): State<AppState>,
    Path(book): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let v = state
        .store
        .run(move |c| {
            book_state(c, &book)?;
            let ids: Vec<String> = c
                .prepare("SELECT id FROM audiobooks WHERE book_id=?1 ORDER BY created_at, id")?
                .query_map([&book], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            let items = ids
                .iter()
                .map(|i| audiobook_value(c, i))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({ "items": items }))
        })
        .await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateIn {
    voice_id: String,
}

/// `createAudiobook`: one per book and voice revision; a repeat returns it.
pub async fn create(
    State(state): State<AppState>,
    device: DeviceCtx,
    Path(book): Path<String>,
    ApiJson(input): ApiJson<CreateIn>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (new_id, audit_id, at, actor) = (
        state.new_id(),
        state.new_id(),
        state.now(),
        Actor::device_only(&device),
    );
    let b = book.clone();
    let (created, v) = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            if book_state(&tx, &b)? == "removed" {
                return Err(ApiError::conflict("book_removed", "Restore this book before making an audiobook of it."));
            }
            let voice: Option<(String, String, bool)> = tx
                .query_row("SELECT name,revision,available FROM voices WHERE id=?1", [&input.voice_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0))
                })
                .optional()?;
            let Some((name, revision, available)) = voice else {
                return Err(ApiError::not_found("voice_not_found", "No voice has this id."));
            };
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM audiobooks WHERE book_id=?1 AND voice_id=?2 AND voice_revision=?3",
                    params![b, input.voice_id, revision],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                return Ok((false, audiobook_value(&tx, &id)?));
            }
            if !available {
                return Err(ApiError::conflict(
                    "voice_unavailable",
                    "This voice's source is not set up or cannot be reached right now.",
                ));
            }
            tx.execute(
                "INSERT INTO audiobooks(id,book_id,voice_id,voice_name,voice_revision,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
                params![new_id, b, input.voice_id, name, revision, at],
            )?;
            audit::record(&tx, &audit_id, &at, "audiobook.created", &actor, &json!({ "book_id": b, "audiobook_id": new_id, "voice_id": input.voice_id }))?;
            let v = audiobook_value(&tx, &new_id)?;
            tx.commit()?;
            Ok((true, v))
        })
        .await?;
    if created {
        let mut n = Notice::new("audiobook.updated", state.now())
            .with_id(v["id"].as_str().unwrap_or_default().to_string());
        n.book_id = Some(book);
        state.notify(n);
    }
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(v),
    ))
}

/// `getAudiobook`
pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        state.store.run(move |c| audiobook_value(c, &id)).await?,
    ))
}

/// An audio file's `AudioRef`.
pub fn audio_ref(r: &rusqlite::Row, at: usize) -> rusqlite::Result<Value> {
    let id: String = r.get(at)?;
    Ok(json!({
        "id": id,
        "duration_seconds": r.get::<_, i64>(at + 1)? as f64 / 1000.0,
        "bytes": r.get::<_, i64>(at + 2)?,
        "sha256": r.get::<_, String>(at + 3)?,
        "content_type": r.get::<_, String>(at + 4)?,
        "voice_revision": r.get::<_, String>(at + 5)?,
        "url": format!("/api/audio/{id}"),
        "timings_url": format!("/api/audio/{id}/timings"),
    }))
}

pub const AUDIO_COLS: &str = "id,duration_ms,bytes,sha256,content_type,voice_revision";

/// `listAudiobookChapters`: every chapter of the book, in reading order.
pub async fn chapters(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let v = state.store.run(move |c| chapter_states(c, &id)).await?;
    Ok(Json(v))
}

/// One word per chapter: ready, making (a job is on it), or not yet (with the reason when a job is stuck).
pub fn chapter_states(c: &Connection, id: &str) -> Result<Value, ApiError> {
    let (book, voice_id, created): (String, String, String) = c
        .query_row(
            "SELECT book_id,voice_id,created_at FROM audiobooks WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or_else(audiobook_not_found)?;
    let chapter_ids: Vec<String> = c
        .prepare("SELECT id FROM chapters WHERE book_id=?1 ORDER BY idx")?
        .query_map([&book], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut items = Vec::with_capacity(chapter_ids.len());
    for ch in chapter_ids {
        let audio = c
            .query_row(
                &format!("SELECT {AUDIO_COLS} FROM audio WHERE audiobook_id=?1 AND chapter_id=?2"),
                [id, &ch],
                |r| audio_ref(r, 0),
            )
            .optional()?;
        let newer = c
            .query_row(
                &format!(
                    "SELECT {} FROM audio x JOIN audiobooks a ON a.id=x.audiobook_id
                     WHERE a.book_id=?1 AND a.voice_id=?2 AND a.created_at>?3 AND x.chapter_id=?4 ORDER BY a.created_at DESC LIMIT 1",
                    AUDIO_COLS.split(',').map(|c| format!("x.{c}")).collect::<Vec<_>>().join(",")
                ),
                params![book, voice_id, created, ch],
                |r| audio_ref(r, 0),
            )
            .optional()?;
        let (state, detail) = if audio.is_some() {
            ("ready", Value::Null)
        } else {
            // The newest job that still has this chapter queued decides.
            let job: Option<(String, Option<String>, Option<String>)> = c
                .query_row(
                    "SELECT j.state,j.waiting,j.needs_you FROM job_items i JOIN jobs j ON j.id=i.job_id
                     WHERE j.audiobook_id=?1 AND i.chapter_id=?2 AND i.state='queued' AND j.state IN ('queued','running','waiting','paused','needs_you')
                     ORDER BY j.created_at DESC LIMIT 1",
                    [id, &ch],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            match job {
                Some((s, _, _)) if s == "queued" || s == "running" => ("making", Value::Null),
                Some((s, w, n)) => {
                    let d = w
                        .or(n)
                        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                        .unwrap_or_else(
                            || json!({ "code": s, "text": "Paused.", "until": Value::Null }),
                        );
                    ("not_yet", d)
                }
                None => ("not_yet", Value::Null),
            }
        };
        items.push(json!({ "chapter_id": ch, "state": state, "audio": audio, "newer_audio": newer, "detail": detail }));
    }
    Ok(json!({ "items": items }))
}
