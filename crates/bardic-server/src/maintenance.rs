//! Work the server does for itself: permanent deletions that have come due.

use crate::{app::AppState, error::ApiError, events::Notice};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

/// Limit filesystem checks to the rows an operation is about to describe or reuse.
/// Reconciliation changes availability only; it never queues generation or deletes a file.
pub enum AudioScope {
    All,
    Audiobook(String),
    Book(String),
    BookOfAudiobook(String),
    Audio(String),
    Estimate(String),
    Job(String),
}

/// Keep immutable audio facts for held downloads, but stop claiming that unavailable bytes
/// are Ready. Called under Store's connection lock: generation publishes a row only after its
/// unique file is complete, and free-up-space marks a row unavailable before removing its file.
/// Other I/O errors fail the operation rather than treating a temporarily inaccessible disk as
/// permission to regenerate (particularly important for paid audio).
pub fn reconcile_audio(
    conn: &Connection,
    data_dir: &Path,
    scope: &AudioScope,
    at: &str,
) -> Result<usize, crate::store::StoreError> {
    let (filter, id) = match scope {
        AudioScope::All => ("1", ""),
        AudioScope::Audiobook(id) => ("audiobook_id=?1", id.as_str()),
        AudioScope::Book(id) => ("audiobook_id IN (SELECT id FROM audiobooks WHERE book_id=?1)", id.as_str()),
        AudioScope::BookOfAudiobook(id) => ("audiobook_id IN (SELECT id FROM audiobooks WHERE book_id=(SELECT book_id FROM audiobooks WHERE id=?1))", id.as_str()),
        AudioScope::Audio(id) => ("id=?1", id.as_str()),
        AudioScope::Estimate(id) => ("audiobook_id=(SELECT audiobook_id FROM estimates WHERE id=?1)", id.as_str()),
        AudioScope::Job(id) => ("audiobook_id=(SELECT audiobook_id FROM jobs WHERE id=?1)", id.as_str()),
    };
    let sql = format!("SELECT id,path,bytes FROM audio WHERE deleted_at IS NULL AND {filter}");
    let mut stmt = conn.prepare(&sql)?;
    let map = |r: &rusqlite::Row<'_>| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    };
    let rows: Vec<(String, String, i64)> = if matches!(scope, AudioScope::All) {
        stmt.query_map([], map)?.collect::<Result<_, _>>()?
    } else {
        stmt.query_map([id], map)?.collect::<Result<_, _>>()?
    };
    drop(stmt);
    let mut changed = 0;
    for (id, path, bytes) in rows {
        let unavailable = match std::fs::metadata(data_dir.join(&path)) {
            Ok(m) => !m.is_file() || u64::try_from(bytes).ok() != Some(m.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => return Err(e.into()),
        };
        if unavailable {
            changed += conn.execute(
                "UPDATE audio SET deleted_at=?2 WHERE id=?1 AND deleted_at IS NULL",
                params![id, at],
            )?;
        }
    }
    Ok(changed)
}

/// Books whose scheduled deletion time has passed.
fn due(conn: &Connection, now: &str) -> Result<Vec<String>, ApiError> {
    Ok(conn
        .prepare("SELECT book_id FROM deletions WHERE state='pending' AND executes_at<=?1 ORDER BY executes_at")?
        .query_map([now], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

pub fn pending(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM deletions WHERE state='pending')",
        [],
        |r| r.get(0),
    )
    .unwrap_or(false)
}

/// Delete one book for good: its rows (places, history, audiobooks, audio records, plans and jobs go
/// with it) and then its files. The spending ledger and the audit log are kept: money was spent and things happened.
/// Files are removed after the rows, so a crash in between leaves orphan files, never a book without files.
fn execute(conn: &mut Connection, book_id: &str, at: &str) -> Result<Vec<PathBuf>, ApiError> {
    let tx = conn.transaction()?;
    let state: Option<String> = tx
        .query_row(
            "SELECT state FROM deletions WHERE book_id=?1",
            [book_id],
            |r| r.get(0),
        )
        .ok();
    if state.as_deref() != Some("pending") {
        return Ok(vec![]);
    }
    // Anything still making audio for this book stops now: the work would have nowhere to go.
    tx.execute(
        "UPDATE jobs SET state='stopped', waiting=NULL, needs_you=NULL, wake_at=NULL, current_chapter_id=NULL, updated_at=?2 WHERE book_id=?1 AND state IN ('queued','running','waiting','paused','needs_you')",
        params![book_id, at],
    )?;
    let mut files: Vec<PathBuf> = vec![PathBuf::from(format!("originals/{book_id}"))];
    let audiobooks: Vec<String> = tx
        .prepare("SELECT id FROM audiobooks WHERE book_id=?1")?
        .query_map([book_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for a in &audiobooks {
        files.push(PathBuf::from(format!("audio/{a}")));
        let exports: Vec<String> = tx
            .prepare("SELECT path FROM exports WHERE audiobook_id=?1 AND path IS NOT NULL")?
            .query_map([a], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        files.extend(exports.into_iter().map(PathBuf::from));
    }
    tx.execute("DELETE FROM books WHERE id=?1", [book_id])?;
    tx.execute(
        "UPDATE deletions SET state='done', finished_at=?2 WHERE book_id=?1",
        params![book_id, at],
    )?;
    tx.commit()?;
    Ok(files)
}

/// Remove audio folders that belong to no audiobook: what a chapter that was still being written
/// when its book was deleted leaves behind. Safe at any time: a folder is made only for an
/// audiobook that exists, and its files are not referenced by any row once the audiobook is gone.
pub async fn sweep_orphans(state: &AppState) -> Result<usize, ApiError> {
    let root = state.store.data_dir().join("audio");
    let Ok(mut dir) = tokio::fs::read_dir(&root).await else {
        return Ok(0);
    };
    let mut names = vec![];
    while let Ok(Some(e)) = dir.next_entry().await {
        if e.file_type().await.is_ok_and(|t| t.is_dir()) {
            names.push(e.file_name().to_string_lossy().to_string());
        }
    }
    let orphans = state
        .store
        .run(move |c| {
            let mut out = vec![];
            for n in names {
                let known: bool = c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM audiobooks WHERE id=?1)",
                    [&n],
                    |r| r.get(0),
                )?;
                if !known {
                    out.push(n);
                }
            }
            Ok(out)
        })
        .await?;
    for n in &orphans {
        let _ = tokio::fs::remove_dir_all(root.join(n)).await;
    }
    Ok(orphans.len())
}

/// Run every deletion that has come due. Called by the worker loop.
pub async fn run_due_deletions(state: &AppState) -> Result<(), ApiError> {
    let now = state.now();
    let n = now.clone();
    let books = state.store.run(move |c| due(c, &n)).await?;
    for book in books {
        let (b, at) = (book.clone(), state.now());
        let files = state.store.run(move |c| execute(c, &b, &at)).await?;
        for f in files {
            let full = state.store.data_dir().join(f);
            let _ = if full.is_dir() {
                tokio::fs::remove_dir_all(&full).await
            } else {
                tokio::fs::remove_file(&full).await
            };
        }
        // A chapter that was being written may land after the files were removed: look again shortly.
        let st = state.clone();
        tokio::spawn(async move {
            for wait in [2, 30] {
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                let _ = sweep_orphans(&st).await;
            }
        });
        let mut n = Notice::new("deletion.updated", state.now()).with_id(book.clone());
        n.book_id = Some(book);
        state.notify(n);
    }
    Ok(())
}
