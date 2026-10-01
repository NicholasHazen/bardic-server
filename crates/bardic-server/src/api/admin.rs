//! Permanent deletion with an undo window, and backups.

use super::audit;
use crate::{
    app::{Actor, AppState, DeviceCtx, ListenerCtx},
    error::ApiError,
    events::Notice,
};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::Duration;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path as FsPath, PathBuf};

const MIN_DELAY: i64 = 60;
const MAX_DELAY: i64 = 7 * 24 * 3600;

fn deletion_value(conn: &Connection, book: &str) -> Result<Option<Value>, ApiError> {
    Ok(conn
        .query_row("SELECT state,scheduled_at,executes_at,scheduled_by FROM deletions WHERE book_id=?1", [book], |r| {
            let by: Value = serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(Value::Null);
            Ok(json!({ "book_id": book, "state": r.get::<_, String>(0)?, "scheduled_at": r.get::<_, String>(1)?, "executes_at": r.get::<_, String>(2)?, "scheduled_by": by }))
        })
        .optional()?)
}

fn book_exists(conn: &Connection, id: &str) -> Result<bool, ApiError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM books WHERE id=?1)",
        [id],
        |r| r.get(0),
    )?)
}

fn not_found(conn: &Connection, book: &str) -> Result<ApiError, ApiError> {
    Ok(if book_exists(conn, book)? {
        ApiError::not_found(
            "deletion_not_found",
            "No deletion is scheduled for this book.",
        )
    } else {
        super::books::book_not_found()
    })
}

/// `getBookDeletion`: a pending or finished deletion; a cancelled one is as if none was scheduled.
pub async fn get_deletion(
    State(state): State<AppState>,
    Path(book): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let v = state
        .store
        .run(move |c| match deletion_value(c, &book)? {
            Some(d) if d["state"] != "cancelled" => Ok(d),
            _ => Err(not_found(c, &book)?),
        })
        .await?;
    Ok(Json(v))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ScheduleIn {
    delay_seconds: Option<i64>,
}

/// `scheduleBookDeletion`: hides the book now and deletes it for good later.
pub async fn schedule(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: ListenerCtx,
    Path(book): Path<String>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let input: ScheduleIn = if body.iter().all(u8::is_ascii_whitespace) {
        ScheduleIn::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            ApiError::invalid(
                "invalid_request",
                format!("The request body is not valid: {e}"),
            )
        })?
    };
    let delay = input.delay_seconds.unwrap_or(MIN_DELAY);
    if !(MIN_DELAY..=MAX_DELAY).contains(&delay) {
        return Err(ApiError::invalid(
            "invalid_request",
            "delay_seconds must be from 60 seconds to 7 days.",
        ));
    }
    let (audit_id, now, actor) = (
        state.new_id(),
        state.clock.now(),
        Actor::with_listener(&device, &listener),
    );
    let b = book.clone();
    let v = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let st: Option<String> = tx.query_row("SELECT state FROM books WHERE id=?1", [&b], |r| r.get(0)).optional()?;
            let prior = match st.as_deref() {
                None => return Err(super::books::book_not_found()),
                Some("readable") => "readable",
                Some("removed") => "removed",
                Some("adding") => return Err(ApiError::conflict("book_adding", "This book is still being added.")),
                Some(_) => return Err(ApiError::conflict("deletion_pending", "This book is already scheduled for deletion.")),
            };
            let busy: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE book_id=?1 AND state IN ('queued','running','waiting'))",
                [&b],
                |r| r.get(0),
            )?;
            if busy {
                return Err(ApiError::conflict("job_running", "Audio is being made for this book. Pause or cancel the job first."));
            }
            let (at, runs) = (crate::clock::ts(now), crate::clock::ts(now + Duration::seconds(delay)));
            tx.execute(
                "INSERT OR REPLACE INTO deletions(book_id,prior_state,state,scheduled_at,executes_at,scheduled_by) VALUES(?1,?2,'pending',?3,?4,?5)",
                params![b, prior, at, runs, serde_json::to_string(&actor).expect("actor")],
            )?;
            tx.execute("UPDATE books SET state='deleting' WHERE id=?1", [&b])?;
            audit::record(&tx, &audit_id, &at, "book.deletion_scheduled", &actor, &json!({ "book_id": b, "executes_at": runs }))?;
            let v = deletion_value(&tx, &b)?.expect("just written");
            tx.commit()?;
            Ok(v)
        })
        .await?;
    announce(&state, &book);
    state.jobs.poke();
    Ok((StatusCode::ACCEPTED, Json(v)))
}

fn announce(state: &AppState, book: &str) {
    let mut n = Notice::new("deletion.updated", state.now()).with_id(book.to_string());
    n.book_id = Some(book.to_string());
    state.notify(n);
}

/// `cancelBookDeletion`: the book returns exactly as it was.
pub async fn cancel(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: ListenerCtx,
    Path(book): Path<String>,
) -> Result<StatusCode, ApiError> {
    let (audit_id, at, actor) = (
        state.new_id(),
        state.now(),
        Actor::with_listener(&device, &listener),
    );
    let b = book.clone();
    state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let row: Option<(String, String)> = tx
                .query_row(
                    "SELECT state,prior_state FROM deletions WHERE book_id=?1",
                    [&b],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match row {
                Some((s, prior)) if s == "pending" => {
                    tx.execute(
                        "UPDATE deletions SET state='cancelled' WHERE book_id=?1",
                        [&b],
                    )?;
                    tx.execute("UPDATE books SET state=?2 WHERE id=?1", params![b, prior])?;
                    audit::record(
                        &tx,
                        &audit_id,
                        &at,
                        "book.deletion_cancelled",
                        &actor,
                        &json!({ "book_id": b }),
                    )?;
                    tx.commit()?;
                    Ok(())
                }
                Some((s, _)) if s == "done" => Err(ApiError::conflict(
                    "deletion_done",
                    "This book was already deleted.",
                )),
                _ => Err(not_found(&tx, &b)?),
            }
        })
        .await?;
    announce(&state, &book);
    Ok(StatusCode::NO_CONTENT)
}

// ----------------------------------------------------------------- backups

fn backup_value(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(
        json!({ "id": r.get::<_, String>(0)?, "state": r.get::<_, String>(1)?, "created_at": r.get::<_, String>(2)?, "bytes": r.get::<_, Option<i64>>(3)?, "path": r.get::<_, Option<String>>(4)? }),
    )
}

const BACKUP_COLS: &str = "id,state,created_at,bytes,path";

/// `listBackups`: newest first.
pub async fn list_backups(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let v = state
        .store
        .run(|c| {
            let items = c
                .prepare(&format!(
                    "SELECT {BACKUP_COLS} FROM backups ORDER BY created_at DESC, id DESC"
                ))?
                .query_map([], backup_value)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({ "items": items }))
        })
        .await?;
    Ok(Json(v))
}

/// Link (or, across filesystems, copy) every immutable media file under `src` into `dst`. Returns total bytes.
fn link_tree(src: &FsPath, dst: &FsPath) -> std::io::Result<i64> {
    let mut total = 0;
    if !src.exists() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            total += link_tree(&from, &to)?;
        } else if from.extension().is_some_and(|e| e == "part") {
            continue; // a chapter still being made is not a finished file
        } else {
            let len = entry.metadata()?.len() as i64;
            if std::fs::hard_link(&from, &to).is_err() {
                std::fs::copy(&from, &to)?;
            }
            total += len;
        }
    }
    Ok(total)
}

/// `createBackup`: a consistent database copy plus the media files, which are immutable and so only linked.
pub async fn create_backup(
    State(state): State<AppState>,
    device: DeviceCtx,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (id, at, audit_id, actor) = (
        state.new_id(),
        state.now(),
        state.new_id(),
        Actor::device_only(&device),
    );
    let (i, a) = (id.clone(), at.clone());
    let (v, fresh) = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            // One backup at a time: asking again returns the one under way.
            let running: Option<String> = tx
                .query_row("SELECT id FROM backups WHERE state='running'", [], |r| {
                    r.get(0)
                })
                .optional()?;
            if let Some(r) = running {
                let v = tx.query_row(
                    &format!("SELECT {BACKUP_COLS} FROM backups WHERE id=?1"),
                    [&r],
                    backup_value,
                )?;
                return Ok((v, false));
            }
            tx.execute(
                "INSERT INTO backups(id,state,created_at) VALUES(?1,'running',?2)",
                params![i, a],
            )?;
            audit::record(
                &tx,
                &audit_id,
                &a,
                "backup.started",
                &actor,
                &json!({ "backup_id": i }),
            )?;
            let v = tx.query_row(
                &format!("SELECT {BACKUP_COLS} FROM backups WHERE id=?1"),
                [&i],
                backup_value,
            )?;
            tx.commit()?;
            Ok((v, true))
        })
        .await?;
    if fresh {
        let st = state.clone();
        tokio::spawn(async move { run_backup(st, id).await });
    }
    Ok((StatusCode::ACCEPTED, Json(v)))
}

async fn run_backup(state: AppState, id: String) {
    let dir: PathBuf = state.store.data_dir().join("backups").join(&id);
    let result: Result<i64, String> = async {
        tokio::fs::create_dir_all(dir.join("media"))
            .await
            .map_err(|e| e.to_string())?;
        let db = dir.join("bardic.db");
        let target = db.to_string_lossy().replace('\'', "''");
        // VACUUM INTO is one consistent snapshot, taken while the server keeps running.
        state
            .store
            .run(move |c| Ok(c.execute_batch(&format!("VACUUM INTO '{target}'"))?))
            .await
            .map_err(|e| e.detail)?;
        // The copy may be taken elsewhere: it does not carry the API keys (enter them again after
        // a restore), and the vacuum drops the old pages that still held them.
        let copy = db.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let c = rusqlite::Connection::open(copy)?;
            c.execute_batch(
                "UPDATE voice_sources SET config = json_remove(config, '$.api_key'); VACUUM;",
            )
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        let data = state.store.data_dir().to_path_buf();
        let media = dir.join("media");
        let linked = tokio::task::spawn_blocking(move || -> std::io::Result<i64> {
            let mut total = 0;
            for sub in ["originals", "audio", "samples"] {
                let to = media.join(sub);
                std::fs::create_dir_all(&to)?;
                total += link_tree(&data.join(sub), &to)?;
            }
            Ok(total)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        let db_bytes = tokio::fs::metadata(&db)
            .await
            .map_err(|e| e.to_string())?
            .len() as i64;
        Ok(db_bytes + linked)
    }
    .await;
    // relative to the data folder: clients are not told where the server keeps its files
    let (i, path) = (id.clone(), format!("backups/{id}"));
    let _ = state
        .store
        .run(move |c| {
            match result {
                Ok(bytes) => c.execute(
                    "UPDATE backups SET state='done', bytes=?2, path=?3 WHERE id=?1",
                    params![i, bytes, path],
                )?,
                Err(e) => c.execute(
                    "UPDATE backups SET state='failed', error=?2 WHERE id=?1",
                    params![i, e],
                )?,
            };
            Ok(())
        })
        .await;
}
