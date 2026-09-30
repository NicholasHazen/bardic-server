//! Places: one per (listener, book), revisioned, with a short history.
//! The rules live in `crate::places`; this module is storage and HTTP.

use super::{audit, ApiJson};
use crate::{
    app::{Actor, AppState, DeviceCtx, ListenerCtx},
    error::ApiError,
    events::Notice,
    places::{self, ChapterLen},
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

/// Who is looking and when, for the per-listener parts of a `Book`.
pub struct Viewer {
    pub listener: String,
    pub now: DateTime<Utc>,
}

struct Row {
    chapter_id: String,
    offset: i64,
    progress: f64,
    mode: String,
    audiobook_id: Option<String>,
    device_id: String,
    device_name: String,
    revision: i64,
    updated_at: String,
    marked_at: Option<String>,
}

const COLS: &str = "chapter_id,char_offset,progress,mode,audiobook_id,device_id,device_name,revision,updated_at,marked_at";

fn row_of(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    Ok(Row {
        chapter_id: r.get(0)?,
        offset: r.get(1)?,
        progress: r.get(2)?,
        mode: r.get(3)?,
        audiobook_id: r.get(4)?,
        device_id: r.get(5)?,
        device_name: r.get(6)?,
        revision: r.get(7)?,
        updated_at: r.get(8)?,
        marked_at: r.get(9)?,
    })
}

pub fn parse(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_default()
}

fn load(conn: &Connection, listener: &str, book: &str) -> Result<Option<Row>, ApiError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM places WHERE listener_id=?1 AND book_id=?2"),
            [listener, book],
            row_of,
        )
        .optional()?)
}

fn finished_of(row: &Row, now: DateTime<Utc>) -> places::Finished {
    places::finished_state(
        row.marked_at.as_deref().map(parse),
        row.progress,
        parse(&row.updated_at),
        now,
    )
}

fn finished_json(f: &places::Finished) -> Value {
    json!({ "finished": f.finished, "since": f.since.map(crate::clock::ts), "reason": f.reason })
}

fn place_json(book_id: &str, row: &Row, now: DateTime<Utc>) -> Value {
    json!({
        "book_id": book_id,
        "chapter_id": row.chapter_id,
        "offset": row.offset,
        "progress": row.progress,
        "mode": row.mode,
        "audiobook_id": row.audiobook_id,
        "device_id": row.device_id,
        "device_name": row.device_name,
        "revision": row.revision,
        "updated_at": row.updated_at,
        "finished": finished_json(&finished_of(row, now)),
    })
}

/// The `PlaceSummary` on a `Book`, or null if the listener never opened it.
pub fn summary(conn: &Connection, viewer: &Viewer, book: &str) -> Result<Value, ApiError> {
    Ok(match load(conn, &viewer.listener, book)? {
        None => Value::Null,
        Some(r) => json!({
            "chapter_id": r.chapter_id,
            "progress": r.progress,
            "finished": finished_of(&r, viewer.now).finished,
            "updated_at": r.updated_at,
        }),
    })
}

struct Chapters {
    ids: Vec<String>,
    lens: Vec<ChapterLen>,
}

fn chapters_of(conn: &Connection, book: &str) -> Result<Chapters, ApiError> {
    let mut stmt =
        conn.prepare("SELECT id,kind,char_len FROM chapters WHERE book_id=?1 ORDER BY idx")?;
    let mut out = Chapters {
        ids: vec![],
        lens: vec![],
    };
    for r in stmt.query_map([book], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (id, kind, len) = r?;
        out.ids.push(id);
        out.lens.push(ChapterLen {
            kind_is_story: kind == "story",
            kind_is_back: kind == "back_matter",
            len,
        });
    }
    Ok(out)
}

fn require_book(conn: &Connection, book: &str) -> Result<(), ApiError> {
    let state: Option<String> = conn
        .query_row("SELECT state FROM books WHERE id=?1", [book], |r| r.get(0))
        .optional()?;
    match state.as_deref() {
        None | Some("deleting") => Err(super::books::book_not_found()),
        Some(_) => Ok(()),
    }
}

fn no_place() -> ApiError {
    ApiError::not_found("place_not_found", "This listener has not opened this book.")
}

fn announce(state: &AppState, book_id: &str, listener: &str) {
    let mut n = Notice::new("place.updated", state.now()).with_id(book_id.to_string());
    n.book_id = Some(book_id.to_string());
    n.listener_id = Some(listener.to_string());
    state.notify(n);
}

/// `getPlace`
pub async fn get(
    State(state): State<AppState>,
    listener: ListenerCtx,
    Path(book): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let now = state.clock.now();
    let v = state
        .store
        .run(move |c| {
            require_book(c, &book)?;
            let row = load(c, &listener.id, &book)?.ok_or_else(no_place)?;
            Ok(place_json(&book, &row, now))
        })
        .await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaceIn {
    chapter_id: String,
    offset: i64,
    mode: String,
    #[serde(default)]
    audiobook_id: Option<String>,
    base_revision: i64,
}

/// `putPlace`
pub async fn put(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: ListenerCtx,
    Path(book): Path<String>,
    ApiJson(input): ApiJson<PlaceIn>,
) -> Result<Json<Value>, ApiError> {
    if !["listening", "reading"].contains(&input.mode.as_str()) {
        return Err(ApiError::invalid(
            "invalid_request",
            "mode must be listening or reading.",
        ));
    }
    if input.offset < 0 || input.base_revision < 0 {
        return Err(ApiError::invalid(
            "invalid_request",
            "offset and base_revision cannot be negative.",
        ));
    }
    let now = state.clock.now();
    let at = crate::clock::ts(now);
    let book2 = book.clone();
    let (lid, dev) = (listener.id.clone(), device.clone());
    let result = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            require_book(&tx, &book2)?;
            let chapters = chapters_of(&tx, &book2)?;
            let idx = chapters
                .ids
                .iter()
                .position(|i| *i == input.chapter_id)
                .ok_or_else(|| ApiError::not_found("chapter_not_found", "This book has no chapter with this id."))?;
            if input.offset > chapters.lens[idx].len {
                return Err(ApiError::invalid(
                    "offset_out_of_range",
                    "The offset is past the end of the chapter text.",
                ));
            }
            let progress = places::progress(&chapters.lens, idx, input.offset);
            let current = load(&tx, &lid, &book2)?;
            let same_position = |r: &Row| r.chapter_id == input.chapter_id && r.offset == input.offset;
            if let Some(cur) = &current {
                let identical = same_position(cur)
                    && cur.mode == input.mode
                    && cur.audiobook_id == input.audiobook_id;
                if identical {
                    return Ok(Ok(place_json(&book2, cur, now)));
                }
                if input.base_revision != cur.revision && cur.device_id != dev.id {
                    return Ok(Err(place_json(&book2, cur, now)));
                }
            }
            let revision = current.as_ref().map_or(1, |r| r.revision + 1);
            let moved = current.as_ref().is_none_or(|r| !same_position(r));
            if let Some(prev) = current.as_ref().filter(|_| moved) {
                if places::keep_in_history(parse(&prev.updated_at), now, prev.progress, progress) {
                    tx.execute(
                        "INSERT INTO place_history(listener_id,book_id,chapter_id,char_offset,progress,mode,audiobook_id,device_id,device_name,revision,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                        params![lid, book2, prev.chapter_id, prev.offset, prev.progress, prev.mode, prev.audiobook_id, prev.device_id, prev.device_name, prev.revision, prev.updated_at],
                    )?;
                    tx.execute(
                        "DELETE FROM place_history WHERE listener_id=?1 AND book_id=?2 AND id NOT IN (SELECT id FROM place_history WHERE listener_id=?1 AND book_id=?2 ORDER BY id DESC LIMIT ?3)",
                        params![lid, book2, places::HISTORY_MAX],
                    )?;
                }
            }
            let marked = if moved { None } else { current.as_ref().and_then(|r| r.marked_at.clone()) };
            tx.execute(
                "INSERT INTO places(listener_id,book_id,chapter_id,char_offset,progress,mode,audiobook_id,device_id,device_name,revision,updated_at,marked_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
                 ON CONFLICT(listener_id,book_id) DO UPDATE SET chapter_id=excluded.chapter_id, char_offset=excluded.char_offset, progress=excluded.progress, mode=excluded.mode, audiobook_id=excluded.audiobook_id, device_id=excluded.device_id, device_name=excluded.device_name, revision=excluded.revision, updated_at=excluded.updated_at, marked_at=excluded.marked_at",
                params![lid, book2, input.chapter_id, input.offset, progress, input.mode, input.audiobook_id, dev.id, dev.name, revision, at, marked],
            )?;
            tx.execute("UPDATE listeners SET last_listened_at=?2 WHERE id=?1", params![lid, at])?;
            let row = load(&tx, &lid, &book2)?.expect("just written");
            let v = place_json(&book2, &row, now);
            tx.commit()?;
            Ok(Ok(v))
        })
        .await?;
    match result {
        Ok(v) => {
            announce(&state, &book, &listener.id);
            Ok(Json(v))
        }
        Err(server_place) => {
            let mut e = ApiError::conflict(
                "place_conflict",
                "Another device has a newer place in this book. Choose which one to keep.",
            );
            e.extra.insert("server_place".into(), server_place);
            Err(e)
        }
    }
}

/// `clearPlace`: idempotent.
pub async fn clear(
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
    let (b, l) = (book.clone(), listener.id.clone());
    state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            require_book(&tx, &b)?;
            let n = tx.execute(
                "DELETE FROM places WHERE listener_id=?1 AND book_id=?2",
                [&l, &b],
            )?;
            tx.execute(
                "DELETE FROM place_history WHERE listener_id=?1 AND book_id=?2",
                [&l, &b],
            )?;
            if n > 0 {
                audit::record(
                    &tx,
                    &audit_id,
                    &at,
                    "place.cleared",
                    &actor,
                    &json!({ "book_id": b }),
                )?;
            }
            tx.commit()?;
            Ok(n > 0)
        })
        .await
        .map(|changed| {
            if changed {
                announce(&state, &book, &listener.id);
            }
            StatusCode::NO_CONTENT
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinishedIn {
    finished: bool,
}

/// `setFinished`
pub async fn set_finished(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: ListenerCtx,
    Path(book): Path<String>,
    ApiJson(input): ApiJson<FinishedIn>,
) -> Result<Json<Value>, ApiError> {
    let now = state.clock.now();
    let at = crate::clock::ts(now);
    let (audit_id, actor) = (state.new_id(), Actor::with_listener(&device, &listener));
    let (b, l, dev) = (book.clone(), listener.id.clone(), device.clone());
    let v = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            require_book(&tx, &b)?;
            let current = load(&tx, &l, &b)?;
            match (input.finished, current) {
                (false, None) => return Err(no_place()),
                (false, Some(_)) => {
                    tx.execute(
                        "UPDATE places SET marked_at=NULL, revision=revision+1, updated_at=?3, device_id=?4, device_name=?5 WHERE listener_id=?1 AND book_id=?2",
                        params![l, b, at, dev.id, dev.name],
                    )?;
                }
                (true, Some(_)) => {
                    tx.execute(
                        "UPDATE places SET marked_at=?3, revision=revision+1, updated_at=?3, device_id=?4, device_name=?5 WHERE listener_id=?1 AND book_id=?2",
                        params![l, b, at, dev.id, dev.name],
                    )?;
                }
                (true, None) => {
                    let ch = chapters_of(&tx, &b)?;
                    let last = ch.lens.iter().rposition(|x| x.kind_is_story).or(ch.ids.len().checked_sub(1)).ok_or_else(|| {
                        ApiError::invalid("invalid_request", "This book has no text to finish.")
                    })?;
                    let (cid, len) = (ch.ids[last].clone(), ch.lens[last].len);
                    let progress = places::progress(&ch.lens, last, len);
                    tx.execute(
                        "INSERT INTO places(listener_id,book_id,chapter_id,char_offset,progress,mode,audiobook_id,device_id,device_name,revision,updated_at,marked_at) VALUES(?1,?2,?3,?4,?5,'reading',NULL,?6,?7,1,?8,?8)",
                        params![l, b, cid, len, progress, dev.id, dev.name, at],
                    )?;
                }
            }
            audit::record(&tx, &audit_id, &at, "place.finished", &actor, &json!({ "book_id": b, "finished": input.finished }))?;
            let row = load(&tx, &l, &b)?.expect("place exists");
            let v = place_json(&b, &row, now);
            tx.commit()?;
            Ok(v)
        })
        .await?;
    announce(&state, &book, &listener.id);
    Ok(Json(v))
}

/// `listPlaceHistory`
pub async fn history(
    State(state): State<AppState>,
    listener: ListenerCtx,
    Path(book): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let now = state.clock.now();
    let v = state
        .store
        .run(move |c| {
            require_book(c, &book)?;
            let mut stmt = c.prepare(&format!(
                "SELECT {} FROM place_history WHERE listener_id=?1 AND book_id=?2 ORDER BY id DESC LIMIT {}",
                "chapter_id,char_offset,progress,mode,audiobook_id,device_id,device_name,revision,updated_at,NULL",
                places::HISTORY_MAX
            ))?;
            let items = stmt
                .query_map([&listener.id, &book], row_of)?
                .map(|r| {
                    r.map(|row| {
                        let mut v = place_json(&book, &row, now);
                        v["finished"] = finished_json(&places::Finished { finished: false, since: None, reason: None });
                        v
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({ "items": items }))
        })
        .await?;
    Ok(Json(v))
}
