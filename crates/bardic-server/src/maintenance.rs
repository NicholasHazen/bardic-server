//! Work the server does for itself: permanent deletions that have come due.

use crate::{app::AppState, error::ApiError, events::Notice};
use rusqlite::{params, Connection};
use std::path::PathBuf;

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
