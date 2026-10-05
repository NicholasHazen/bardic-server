//! Adding books. `createImport` returns 202 at once; the work runs in the
//! background and is read with `getImport` or the event stream. A failed or
//! cancelled import leaves nothing behind: no book, no saved original.

use super::{audit, books};
use crate::{
    app::{Actor, AppState, DeviceCtx},
    cover::{sha256_hex, thumbnail},
    error::ApiError,
    events::Notice,
    importer::{self, ImportFailure},
};
use axum::{
    extract::{Multipart, Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

fn import_not_found() -> ApiError {
    ApiError::not_found("import_not_found", "No import has this id.")
}

pub fn import_value(conn: &Connection, id: &str) -> Result<Value, ApiError> {
    let row = conn
        .query_row(
            "SELECT id,state,file_name,created_at,progress,book_id,error_code,error_detail FROM imports WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, f64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((id, state, file_name, created_at, progress, book_id, ecode, edetail)) = row else {
        return Err(import_not_found());
    };
    Ok(json!({
        "id": id, "state": state, "file_name": file_name, "created_at": created_at, "progress": progress,
        "book_id": book_id,
        "error": ecode.map(|c| json!({ "code": c, "detail": edetail.unwrap_or_default() })),
    }))
}

/// File name without any directory, or `book.txt` when empty.
fn clean_file_name(raw: Option<&str>) -> String {
    let n = raw
        .unwrap_or("")
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim();
    if n.is_empty() {
        "book.txt".to_string()
    } else {
        n.chars().take(200).collect()
    }
}

/// `createImport`
pub async fn create(
    State(state): State<AppState>,
    device: DeviceCtx,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let idem = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(key) = &idem {
        let key = key.clone();
        let existing = state
            .store
            .run(move |c| {
                Ok(
                    c.query_row("SELECT id FROM imports WHERE idem_key=?1", [&key], |r| {
                        r.get::<_, String>(0)
                    })
                    .optional()?,
                )
            })
            .await?;
        if let Some(id) = existing {
            return Ok((
                StatusCode::ACCEPTED,
                Json(state.store.run(move |c| import_value(c, &id)).await?),
            ));
        }
    }

    let max = state.config.max_upload_bytes as usize;
    let mut file: Option<(String, Vec<u8>)> = None;
    while let Some(mut field) = multipart.next_field().await? {
        if field.name() != Some("file") {
            continue;
        }
        let name = clean_file_name(field.file_name());
        let mut bytes = Vec::new();
        while let Some(chunk) = field.chunk().await? {
            if bytes.len() + chunk.len() > max {
                return Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "import_too_large",
                    "The file is larger than this server accepts.",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        file = Some((name, bytes));
        break;
    }
    let Some((file_name, bytes)) = file else {
        return Err(ApiError::invalid(
            "invalid_request",
            "Send the book as a multipart field named file.",
        ));
    };
    let Some(ext) = importer::extension(&file_name) else {
        return Err(ApiError::invalid(
            "import_unreadable",
            "Only .epub and .txt files can be added.",
        ));
    };

    let (import_id, book_id, at) = (state.new_id(), state.new_id(), state.now());
    let sha = sha256_hex(&bytes);
    let title = importer::title_from_file_name(&file_name);
    let tmp = state
        .store
        .data_dir()
        .join("tmp")
        .join(format!("{import_id}.{ext}"));
    tokio::fs::create_dir_all(tmp.parent().expect("tmp has a parent"))
        .await
        .map_err(ApiError::internal)?;
    tokio::fs::write(&tmp, &bytes)
        .await
        .map_err(ApiError::internal)?;
    drop(bytes);

    let (i2, b2, at2, f2, t2, s2, e2) = (
        import_id.clone(),
        book_id.clone(),
        at.clone(),
        file_name.clone(),
        title,
        sha,
        ext.to_string(),
    );
    let view = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO books(id,title,author,state,added_at,source_sha256,source_name,source_ext) VALUES(?1,?2,'','adding',?3,?4,?5,?6)",
                params![b2, t2, at2, s2, f2, e2],
            )?;
            tx.execute(
                "INSERT INTO imports(id,state,file_name,created_at,progress,book_id,idem_key) VALUES(?1,'queued',?2,?3,0,?4,?5)",
                params![i2, f2, at2, b2, idem],
            )?;
            let v = import_value(&tx, &i2)?;
            tx.commit()?;
            Ok(v)
        })
        .await?;
    announce(&state, &import_id, Some(&book_id));
    tokio::spawn(run_import(
        state.clone(),
        import_id,
        book_id,
        tmp,
        file_name,
        Actor::device_only(&device),
    ));
    Ok((StatusCode::ACCEPTED, Json(view)))
}

fn announce(state: &AppState, import_id: &str, book_id: Option<&str>) {
    let mut n = Notice::new("import.updated", state.now()).with_id(import_id.to_string());
    n.book_id = book_id.map(str::to_string);
    state.notify(n);
}

async fn set_state(state: &AppState, import_id: &str, book_id: &str, st: &str, progress: f64) {
    let (i, s) = (import_id.to_string(), st.to_string());
    let _ = state
        .store
        .run(move |c| {
            c.execute("UPDATE imports SET state=?2, progress=?3 WHERE id=?1 AND state NOT IN ('cancelled','failed','done')", params![i, s, progress])?;
            Ok(())
        })
        .await;
    announce(state, import_id, Some(book_id));
}

fn cancelled(state: &AppState, import_id: &str) -> bool {
    state
        .cancelled_imports
        .lock()
        .map(|s| s.contains(import_id))
        .unwrap_or(false)
}

/// Remove everything an unfinished import made.
async fn clean_up(state: &AppState, book_id: &str, tmp: &std::path::Path) {
    let b = book_id.to_string();
    let _ = state
        .store
        .run(move |c| {
            c.execute("DELETE FROM books WHERE id=?1 AND state='adding'", [&b])?;
            Ok(())
        })
        .await;
    let _ = tokio::fs::remove_file(tmp).await;
    let _ = tokio::fs::remove_dir_all(state.store.data_dir().join("originals").join(book_id)).await;
}

async fn fail(
    state: &AppState,
    import_id: &str,
    book_id: &str,
    tmp: &std::path::Path,
    code: &str,
    detail: String,
) {
    clean_up(state, book_id, tmp).await;
    let (i, c2, d2) = (import_id.to_string(), code.to_string(), detail);
    let _ = state
        .store
        .run(move |c| {
            c.execute(
                "UPDATE imports SET state='failed', error_code=?2, error_detail=?3, book_id=NULL WHERE id=?1 AND state NOT IN ('cancelled','done')",
                params![i, c2, d2],
            )?;
            Ok(())
        })
        .await;
    announce(state, import_id, None);
}

async fn finish_cancelled(state: &AppState, import_id: &str, book_id: &str, tmp: &std::path::Path) {
    clean_up(state, book_id, tmp).await;
    let i = import_id.to_string();
    let _ = state
        .store
        .run(move |c| {
            c.execute(
                "UPDATE imports SET state='cancelled', book_id=NULL WHERE id=?1",
                [&i],
            )?;
            Ok(())
        })
        .await;
    announce(state, import_id, None);
}

async fn run_import(
    state: AppState,
    import_id: String,
    book_id: String,
    tmp: std::path::PathBuf,
    file_name: String,
    actor: Actor,
) {
    // A burst of uploads is worked through a few at a time; the others stay `queued`.
    let _slot = state.gates.imports.acquire().await.ok();
    run_import_inner(&state, import_id.clone(), book_id, tmp, file_name, actor).await;
    if let Ok(mut set) = state.cancelled_imports.lock() {
        set.remove(&import_id);
    }
}

async fn run_import_inner(
    state: &AppState,
    import_id: String,
    book_id: String,
    tmp: std::path::PathBuf,
    file_name: String,
    actor: Actor,
) {
    set_state(state, &import_id, &book_id, "reading", 0.1).await;
    let bytes = match tokio::fs::read(&tmp).await {
        Ok(b) => b,
        Err(e) => {
            return fail(
                state,
                &import_id,
                &book_id,
                &tmp,
                "internal_error",
                e.to_string(),
            )
            .await
        }
    };
    if cancelled(state, &import_id) {
        return finish_cancelled(state, &import_id, &book_id, &tmp).await;
    }
    set_state(state, &import_id, &book_id, "finding_chapters", 0.35).await;
    let name = file_name.clone();
    let parsed = tokio::task::spawn_blocking(move || importer::parse(&name, &bytes)).await;
    let parsed = match parsed {
        Ok(Ok(p)) => p,
        Ok(Err(f)) => return fail_with(state, &import_id, &book_id, &tmp, f).await,
        Err(e) => {
            return fail(
                state,
                &import_id,
                &book_id,
                &tmp,
                "internal_error",
                e.to_string(),
            )
            .await
        }
    };
    if cancelled(state, &import_id) {
        return finish_cancelled(state, &import_id, &book_id, &tmp).await;
    }
    set_state(state, &import_id, &book_id, "preparing_text", 0.7).await;
    let cover_bytes = parsed.cover.clone();
    let thumb = tokio::task::spawn_blocking(move || cover_bytes.and_then(|b| thumbnail(&b).ok()))
        .await
        .ok()
        .flatten();
    if cancelled(state, &import_id) {
        return finish_cancelled(state, &import_id, &book_id, &tmp).await;
    }

    // Keep the original for cover refresh and later repair.
    let dest_dir = state.store.data_dir().join("originals").join(&book_id);
    let dest = dest_dir.join(format!("source.{}", parsed.ext));
    if let Err(e) = async {
        tokio::fs::create_dir_all(&dest_dir).await?;
        tokio::fs::copy(&tmp, &dest).await.map(|_| ())
    }
    .await
    {
        return fail(
            state,
            &import_id,
            &book_id,
            &tmp,
            "internal_error",
            e.to_string(),
        )
        .await;
    }

    // The original is now saved. Clear the upload before publishing `done`,
    // so a client that observes completion cannot still find it in tmp.
    let _ = tokio::fs::remove_file(&tmp).await;

    let (st, bid, iid, at, audit_id) = (
        state.clone(),
        book_id.clone(),
        import_id.clone(),
        state.now(),
        state.new_id(),
    );
    let stored = state
        .store
        .run(move |c| {
            books::store_parsed(c, &st, &bid, &parsed, thumb.as_ref())?;
            let tx = c.transaction()?;
            tx.execute("UPDATE imports SET state='done', progress=1 WHERE id=?1 AND state NOT IN ('cancelled','failed')", [&iid])?;
            audit::record(&tx, &audit_id, &at, "book.imported", &actor, &json!({ "book_id": bid, "import_id": iid }))?;
            tx.commit()?;
            Ok(())
        })
        .await;
    match stored {
        Ok(()) => {
            announce(state, &import_id, Some(&book_id));
            books::announce(state, &book_id);
        }
        Err(e) => {
            fail(
                state,
                &import_id,
                &book_id,
                &tmp,
                "internal_error",
                e.detail,
            )
            .await
        }
    }
}

async fn fail_with(
    state: &AppState,
    import_id: &str,
    book_id: &str,
    tmp: &std::path::Path,
    f: ImportFailure,
) {
    fail(state, import_id, book_id, tmp, f.code(), f.detail()).await
}

/// `getImport`
pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.store.run(move |c| import_value(c, &id)).await?))
}

/// `cancelImport`: asks the running import to stop. The import reports `cancelled`
/// only after everything it made has been removed, so a cancelled import never
/// leaves a book behind. No effect, and 204, when it already finished.
pub async fn cancel(
    State(state): State<AppState>,
    _device: DeviceCtx,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let target = id.clone();
    let st: String = state
        .store
        .run(move |c| {
            c.query_row("SELECT state FROM imports WHERE id=?1", [&target], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(import_not_found)
        })
        .await?;
    if !matches!(st.as_str(), "done" | "failed" | "cancelled") {
        if let Ok(mut set) = state.cancelled_imports.lock() {
            set.insert(id);
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `createSampleBook`: an original short story, free and instant.
pub async fn sample(
    State(state): State<AppState>,
    device: DeviceCtx,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    use crate::{sample, text};
    let chapters = sample::CHAPTERS
        .iter()
        .map(|(title, body)| {
            let paragraphs = text::paragraphs_from_plain(body);
            let (t, lines) = text::build_chapter(&paragraphs);
            importer::ParsedChapter {
                title: title.to_string(),
                kind: "story",
                text: t,
                lines,
            }
        })
        .collect();
    let parsed = importer::ParsedBook {
        title: sample::TITLE.into(),
        author: sample::AUTHOR.into(),
        chapters,
        cover: None,
        ext: "txt",
    };
    let (book_id, at, audit_id, actor, st) = (
        state.new_id(),
        state.now(),
        state.new_id(),
        Actor::device_only(&device),
        state.clone(),
    );
    let b2 = book_id.clone();
    let v = state
        .store
        .run(move |c| {
            c.execute(
                "INSERT INTO books(id,title,author,state,added_at,source_name) VALUES(?1,?2,?3,'adding',?4,'sample')",
                params![b2, parsed.title, parsed.author, at],
            )?;
            books::store_parsed(c, &st, &b2, &parsed, None)?;
            audit::record(c, &audit_id, &at, "book.imported", &actor, &json!({ "book_id": b2, "sample": true }))?;
            books::book_value(c, &b2, None)
        })
        .await?;
    books::announce(&state, &book_id);
    Ok((StatusCode::CREATED, Json(v)))
}
