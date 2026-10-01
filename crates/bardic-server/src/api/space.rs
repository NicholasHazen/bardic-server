//! Space, offline downloads and what changed since a device last synced.
//!
//! Freeing space deletes audio files but keeps their records, so a device that still holds
//! a freed chapter can be told, truthfully, what it has and what replaced it.

use super::{
    audiobooks::{audio_ref, AUDIO_COLS},
    audit, ApiJson,
};
use crate::{
    app::{Actor, AppState, DeviceCtx},
    error::ApiError,
    events::Notice,
    plans,
};
use axum::{
    extract::{Path, State},
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

fn audiobook_not_found() -> ApiError {
    ApiError::not_found("audiobook_not_found", "No audiobook has this id.")
}

struct Book {
    book_id: String,
    voice_id: String,
    tier: String,
}

fn load(conn: &Connection, id: &str) -> Result<Book, ApiError> {
    conn.query_row("SELECT a.book_id,a.voice_id,v.tier FROM audiobooks a JOIN voices v ON v.id=a.voice_id WHERE a.id=?1", [id], |r| Ok(Book { book_id: r.get(0)?, voice_id: r.get(1)?, tier: r.get(2)? }))
        .optional()?
        .ok_or_else(audiobook_not_found)
}

/// `getAudiobookSpace`: what it uses, and for a premium voice what making it again would cost.
pub async fn get_space(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let v = state
        .store
        .run(move |c| {
            let b = load(c, &id)?;
            let (bytes, chapters, chars): (i64, i64, i64) = c.query_row(
                "SELECT COALESCE(SUM(x.bytes),0), COUNT(*), COALESCE(SUM((SELECT SUM(end-start) FROM lines l WHERE l.chapter_id=x.chapter_id)),0)
                 FROM audio x WHERE x.audiobook_id=?1 AND x.deleted_at IS NULL",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let remake = if b.tier == "premium" {
                let (per_unit, as_of, basis): (i64, String, String) = c.query_row("SELECT per_unit,as_of,basis FROM prices WHERE provider='gemini'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                let r = plans::estimate(chars, per_unit);
                json!({ "low": super::money::money(r.low), "likely": super::money::money(r.likely), "high": super::money::money(r.high), "prices_as_of": as_of, "basis": basis })
            } else {
                Value::Null
            };
            Ok(json!({ "audiobook_id": id, "bytes": bytes, "chapters": chapters, "remake_estimate": remake }))
        })
        .await?;
    Ok(Json(v))
}

/// `freeAudiobookSpace`: deletes the audio, keeps everything else. Refused while a job is working on it.
pub async fn free_space(
    State(state): State<AppState>,
    device: DeviceCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let (audit_id, at, actor) = (state.new_id(), state.now(), Actor::device_only(&device));
    let aid = id.clone();
    let (freed, book, paths) = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let b = load(&tx, &aid)?;
            let busy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE audiobook_id=?1 AND state IN ('queued','running','waiting'))", [&aid], |r| r.get(0))?;
            if busy {
                return Err(ApiError::conflict("job_running", "Audio is being made for this audiobook. Pause or cancel the job first."));
            }
            let rows: Vec<(String, i64)> = tx
                .prepare("SELECT path,bytes FROM audio WHERE audiobook_id=?1 AND deleted_at IS NULL")?
                .query_map([&aid], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            tx.execute("UPDATE audio SET deleted_at=?2, path='' WHERE audiobook_id=?1 AND deleted_at IS NULL", params![aid, at])?;
            // Chapters half made are regenerable too, and half of a premium chapter is not worth keeping once its audiobook is emptied.
            let parts: Vec<String> = tx.prepare("SELECT chapter_id FROM chapter_parts WHERE audiobook_id=?1")?.query_map([&aid], |r| r.get(0))?.collect::<Result<_, _>>()?;
            tx.execute("DELETE FROM chapter_parts WHERE audiobook_id=?1", [&aid])?;
            audit::record(&tx, &audit_id, &at, "audiobook.space_freed", &actor, &json!({ "audiobook_id": aid, "chapters": rows.len() }))?;
            tx.commit()?;
            let mut paths: Vec<String> = rows.iter().map(|(p, _)| p.clone()).collect();
            paths.extend(parts.into_iter().map(|ch| format!("audio/{aid}/{ch}.part")));
            Ok((rows.iter().map(|(_, b)| b).sum::<i64>(), b.book_id, paths))
        })
        .await?;
    for p in paths {
        let _ = tokio::fs::remove_file(state.store.data_dir().join(p)).await;
    }
    let mut n = Notice::new("audiobook.updated", state.now()).with_id(id);
    n.book_id = Some(book);
    state.notify(n);
    Ok(Json(json!({ "freed_bytes": freed })))
}

/// `getAudiobookManifest`
pub async fn manifest(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let at = state.now();
    let v = state
        .store
        .run(move |c| {
            let b = load(c, &id)?;
            let cols = AUDIO_COLS.split(',').map(|c| format!("x.{c}")).collect::<Vec<_>>().join(",");
            let items: Vec<Value> = c
                .prepare(&format!(
                    "SELECT {cols}, ch.id, ch.text_sha256 FROM chapters ch JOIN audio x ON x.chapter_id=ch.id AND x.audiobook_id=?1 AND x.deleted_at IS NULL WHERE ch.book_id=?2 ORDER BY ch.idx"
                ))?
                .query_map(params![id, b.book_id], |r| Ok(json!({ "chapter_id": r.get::<_, String>(6)?, "audio": audio_ref(r, 0)?, "text_sha256": r.get::<_, String>(7)? })))?
                .collect::<Result<_, _>>()?;
            Ok(json!({ "audiobook_id": id, "generated_at": at, "chapters": items }))
        })
        .await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Have {
    chapter_id: String,
    audio_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncIn {
    have: Vec<Have>,
}

/// `checkDownloads`: read-only. A held copy stays valid until the listener chooses the newer one.
pub async fn sync_check(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<SyncIn>,
) -> Result<Json<Value>, ApiError> {
    if input.have.len() > 5000 {
        return Err(ApiError::invalid(
            "invalid_request",
            "have lists at most 5000 chapters.",
        ));
    }
    let v = state
        .store
        .run(move |c| {
            let b = load(c, &id)?;
            let (mut current, mut newer) = (vec![], vec![]);
            for h in &input.have {
                let in_book: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM chapters WHERE id=?1 AND book_id=?2)", [&h.chapter_id, &b.book_id], |r| r.get(0))?;
                if !in_book {
                    return Err(ApiError::not_found("chapter_not_found", "This book has no chapter with this id."));
                }
                // What the device holds, even if the file has since been freed on the server.
                let held: Option<(i64, i64, String, String)> = c
                    .query_row(
                        "SELECT x.duration_ms,x.bytes,x.voice_revision,x.created_at FROM audio x JOIN audiobooks a ON a.id=x.audiobook_id WHERE x.id=?1 AND x.chapter_id=?2 AND a.book_id=?3 AND a.voice_id=?4",
                        params![h.audio_id, h.chapter_id, b.book_id, b.voice_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()?;
                let Some((old_ms, old_bytes, old_rev, old_at)) = held else {
                    current.push(h.chapter_id.clone());
                    continue;
                };
                let cols = AUDIO_COLS.split(',').map(|c| format!("x.{c}")).collect::<Vec<_>>().join(",");
                let found: Option<(Value, i64, i64, String, String)> = c
                    .query_row(
                        &format!(
                            "SELECT {cols}, x.duration_ms, x.bytes, x.voice_revision, a.voice_name FROM audio x JOIN audiobooks a ON a.id=x.audiobook_id
                             WHERE a.book_id=?1 AND a.voice_id=?2 AND x.chapter_id=?3 AND x.deleted_at IS NULL AND x.id<>?4 AND x.created_at>?5 ORDER BY x.created_at DESC LIMIT 1"
                        ),
                        params![b.book_id, b.voice_id, h.chapter_id, h.audio_id, old_at],
                        |r| Ok((audio_ref(r, 0)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
                    )
                    .optional()?;
                match found {
                    None => current.push(h.chapter_id.clone()),
                    Some((new_audio, new_ms, new_bytes, new_rev, voice_name)) => newer.push(json!({
                        "chapter_id": h.chapter_id,
                        "old_audio_id": h.audio_id,
                        "new_audio": new_audio,
                        "changes": {
                            "voice_name": voice_name,
                            "old_voice_revision": old_rev,
                            "new_voice_revision": new_rev,
                            "old_duration_seconds": old_ms as f64 / 1000.0,
                            "new_duration_seconds": new_ms as f64 / 1000.0,
                            "old_bytes": old_bytes,
                            "new_bytes": new_bytes,
                        },
                    })),
                }
            }
            Ok(json!({ "up_to_date": current, "newer": newer }))
        })
        .await?;
    Ok(Json(v))
}
