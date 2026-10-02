//! Export an audiobook as one M4B with chapter markers.
//!
//! Encoding AAC is not done in-process: the server runs `ffmpeg` (a program the owner installs) on the
//! finished chapters. Chapters that are not made yet are left out, and the markers say where the rest are.

use super::{
    audio::{file_response_pub, target},
    audit,
};
use crate::{
    app::{Actor, AppState, DeviceCtx},
    error::ApiError,
    events::Notice,
    jobs,
    maintenance::AudioScope,
};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

fn export_value(conn: &Connection, id: &str) -> Result<Value, ApiError> {
    conn.query_row("SELECT id,audiobook_id,state,format,bytes,job_id,created_at FROM exports WHERE id=?1", [id], |r| {
        Ok(json!({ "id": r.get::<_, String>(0)?, "audiobook_id": r.get::<_, String>(1)?, "state": r.get::<_, String>(2)?, "format": r.get::<_, String>(3)?, "bytes": r.get::<_, Option<i64>>(4)?, "job_id": r.get::<_, String>(5)?, "created_at": r.get::<_, String>(6)? }))
    })
    .optional()?
    .ok_or_else(|| ApiError::not_found("export_not_found", "No export has this id."))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ExportIn {
    format: Option<String>,
}

struct Chapter {
    title: String,
    path: String,
    ms: i64,
}

/// FFMETADATA1 with the book's title and one marker per included chapter.
fn metadata(title: &str, author: &str, chapters: &[Chapter]) -> String {
    let esc = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('=', "\\=")
            .replace(';', "\\;")
            .replace('#', "\\#")
            .replace('\n', " ")
    };
    let mut out = format!(
        ";FFMETADATA1\ntitle={}\nartist={}\nalbum={}\n",
        esc(title),
        esc(author),
        esc(title)
    );
    let mut at = 0;
    for c in chapters {
        out += &format!(
            "[CHAPTER]\nTIMEBASE=1/1000\nSTART={at}\nEND={}\ntitle={}\n",
            at + c.ms,
            esc(&c.title)
        );
        at += c.ms;
    }
    out
}

/// `createExport`
pub async fn create(
    State(state): State<AppState>,
    device: DeviceCtx,
    Path(audiobook): Path<String>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let input: ExportIn = if body.iter().all(u8::is_ascii_whitespace) {
        ExportIn::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            ApiError::invalid(
                "invalid_request",
                format!("The request body is not valid: {e}"),
            )
        })?
    };
    if input.format.as_deref().is_some_and(|f| f != "m4b") {
        return Err(ApiError::invalid("invalid_request", "format must be m4b."));
    }
    let (id, job_id, audit_id, at, actor) = (
        state.new_id(),
        state.new_id(),
        state.new_id(),
        state.now(),
        Actor::device_only(&device),
    );
    let ffmpeg = state.config.ffmpeg.clone();
    let encoder_ok = tokio::process::Command::new(&ffmpeg)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());
    if !encoder_ok {
        return Err(ApiError::conflict(
            "encoder_missing",
            "Exporting needs ffmpeg on the Bardic computer, and it was not found.",
        ));
    }
    let (i, j) = (id.clone(), job_id.clone());
    let (v, title, author, chapters) = match state
        .store
        .run_audio(AudioScope::Audiobook(audiobook.clone()), at.clone(), move |c| {
            let tx = c.transaction()?;
            let t = target(&tx, &audiobook)?;
            let (title, author): (String, String) = tx.query_row("SELECT title,author FROM books WHERE id=?1", [&t.book_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let chapters: Vec<Chapter> = tx
                .prepare("SELECT ch.title,x.path,x.duration_ms FROM chapters ch JOIN audio x ON x.chapter_id=ch.id AND x.audiobook_id=?1 AND x.deleted_at IS NULL WHERE ch.book_id=?2 ORDER BY ch.idx")?
                .query_map(params![audiobook, t.book_id], |r| Ok(Chapter { title: r.get(0)?, path: r.get(1)?, ms: r.get(2)? }))?
                .collect::<Result<_, _>>()?;
            if chapters.is_empty() {
                return Err(ApiError::conflict("nothing_ready", "No chapter of this audiobook is ready to export."));
            }
            // One export per audiobook at a time: asking again returns the one under way.
            let running: Option<String> = tx
                .query_row("SELECT id FROM exports WHERE audiobook_id=?1 AND state='running'", [&audiobook], |r| r.get(0))
                .optional()?;
            if let Some(r) = running {
                return Ok((export_value(&tx, &r)?, title, author, Vec::new()));
            }
            tx.execute(
                "INSERT INTO jobs(id,kind,state,audiobook_id,book_id,chapters_total,started_by,created_at,updated_at) VALUES(?1,'export','running',?2,?3,?4,?5,?6,?6)",
                params![j, audiobook, t.book_id, chapters.len() as i64, serde_json::to_string(&actor).expect("actor"), at],
            )?;
            tx.execute("INSERT INTO exports(id,audiobook_id,state,format,job_id,created_at) VALUES(?1,?2,'running','m4b',?3,?4)", params![i, audiobook, j, at])?;
            audit::record(&tx, &audit_id, &at, "export.started", &actor, &json!({ "export_id": i, "audiobook_id": audiobook }))?;
            let v = export_value(&tx, &i)?;
            tx.commit()?;
            Ok((v, title, author, chapters))
        })
        .await?
    {
        (v, _, _, c) if c.is_empty() => return Ok((StatusCode::ACCEPTED, Json(v))),
        other => other,
    };
    jobs::announce(&state, &job_id);
    let st = state.clone();
    tokio::spawn(async move { run(st, id, job_id, title, author, chapters).await });
    Ok((StatusCode::ACCEPTED, Json(v)))
}

async fn run(
    state: AppState,
    id: String,
    job_id: String,
    title: String,
    author: String,
    chapters: Vec<Chapter>,
) {
    // One ffmpeg at a time, across audiobooks.
    let _slot = state.gates.exports.acquire().await.ok();
    let data = state.store.data_dir().to_path_buf();
    let result: Result<i64, String> = async {
        let dir = data.join("exports");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| e.to_string())?;
        let (list, meta, out) = (
            dir.join(format!("{id}.txt")),
            dir.join(format!("{id}.meta")),
            dir.join(format!("{id}.m4b")),
        );
        let list_body: String = chapters
            .iter()
            .map(|c| {
                format!(
                    "file '{}'\n",
                    data.join(&c.path).to_string_lossy().replace('\'', "'\\''")
                )
            })
            .collect();
        tokio::fs::write(&list, list_body)
            .await
            .map_err(|e| e.to_string())?;
        tokio::fs::write(&meta, metadata(&title, &author, &chapters))
            .await
            .map_err(|e| e.to_string())?;
        let status = tokio::process::Command::new(&state.config.ffmpeg)
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
            ])
            .arg(&list)
            .arg("-i")
            .arg(&meta)
            .args([
                "-map",
                "0:a",
                "-map_metadata",
                "1",
                "-map_chapters",
                "1",
                "-c:a",
                "aac",
                "-b:a",
                "64k",
                "-ac",
                "1",
                "-f",
                "ipod",
            ])
            .arg(&out)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map_err(|e| e.to_string())?;
        let _ = tokio::fs::remove_file(&list).await;
        let _ = tokio::fs::remove_file(&meta).await;
        if !status.success() {
            let _ = tokio::fs::remove_file(&out).await;
            return Err("ffmpeg could not encode the audiobook.".to_string());
        }
        Ok(tokio::fs::metadata(&out)
            .await
            .map_err(|e| e.to_string())?
            .len() as i64)
    }
    .await;
    let (i, j, at) = (id.clone(), job_id.clone(), state.now());
    let total = chapters.len() as i64;
    let _ = state
        .store
        .run(move |c| {
            match result {
                Ok(bytes) => {
                    c.execute("UPDATE exports SET state='ready', bytes=?2, path=?3 WHERE id=?1", params![i, bytes, format!("exports/{i}.m4b")])?;
                    c.execute("UPDATE jobs SET state='completed', chapters_done=?2, updated_at=?3 WHERE id=?1", params![j, total, at])?;
                }
                Err(e) => {
                    c.execute("UPDATE exports SET state='failed', error=?2 WHERE id=?1", params![i, e])?;
                    c.execute("UPDATE jobs SET state='failed', needs_you=?2, updated_at=?3 WHERE id=?1", params![j, jobs::detail("export_failed", &e, None), at])?;
                }
            }
            Ok(())
        })
        .await;
    jobs::announce(&state, &job_id);
    state.notify(Notice::new("job.updated", state.now()).with_id(job_id));
}

/// `getExport`
pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.store.run(move |c| export_value(c, &id)).await?))
}

/// `downloadExport`
pub async fn download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let key = id.clone();
    let row: (String, Option<String>, String) = state
        .store
        .run(move |c| {
            c.query_row("SELECT e.state,e.path,b.title FROM exports e JOIN audiobooks a ON a.id=e.audiobook_id JOIN books b ON b.id=a.book_id WHERE e.id=?1", [&key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?
                .ok_or_else(|| ApiError::not_found("export_not_found", "No export has this id."))
        })
        .await?;
    let (st, path, title) = row;
    let Some(path) = path.filter(|_| st == "ready") else {
        return Err(ApiError::conflict(
            "export_not_ready",
            "This export is not ready to download.",
        ));
    };
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let mut resp = file_response_pub(
        state.store.data_dir().join(path),
        "audio/mp4",
        &id,
        range,
        true,
    )
    .await?;
    let name: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == ' ' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    resp.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{}.m4b\"", name.trim())
            .parse()
            .expect("header"),
    );
    Ok(resp)
}
