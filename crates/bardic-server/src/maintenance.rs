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
        let mut n = Notice::new("deletion.updated", state.now()).with_id(book.clone());
        n.book_id = Some(book);
        state.notify(n);
    }
    Ok(())
}
