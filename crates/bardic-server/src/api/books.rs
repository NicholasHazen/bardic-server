//! Books: list, read, edit, remove and restore; chapters and text; search;
//! series; covers; duplicate check. Views are built as JSON values that mirror
//! the contract's schemas; the conformance tests check them.

use super::{
    audit,
    places::{self, Viewer},
    ApiJson, ApiQuery,
};
use crate::{
    app::{Actor, AppState, DeviceCtx, ListenerCtx, MaybeListener},
    cover::{sha256_hex, Thumbnail},
    error::ApiError,
    events::Notice,
    importer::ParsedBook,
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};

pub fn book_not_found() -> ApiError {
    ApiError::not_found("book_not_found", "No book has this id.")
}

fn cover_value(
    id: &str,
    sha: Option<String>,
    w: Option<i64>,
    h: Option<i64>,
    sample: Option<String>,
    generated: bool,
) -> Value {
    match (sha, w, h) {
        (Some(sha), Some(w), Some(h)) => json!({
            "url": format!("/api/books/{id}/cover?v={sha}"),
            "sha256": sha,
            "width": w,
            "height": h,
            "sample": sample.and_then(|s| serde_json::from_str::<Value>(&s).ok()),
            "generated": generated,
        }),
        _ => Value::Null,
    }
}

/// The contract's `Book`. `place` is the viewer's place summary, null without a viewer.
pub fn book_value(conn: &Connection, id: &str, viewer: Option<&Viewer>) -> Result<Value, ApiError> {
    let row = conn
        .query_row(
            "SELECT id,title,author,state,added_at,series_name,series_order,source_sha256,word_count,chapter_count,story_chapter_count,cover_sha256,cover_width,cover_height,cover_sample,cover_generated FROM books WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<f64>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, i64>(10)?,
                    r.get::<_, Option<String>>(11)?,
                    r.get::<_, Option<i64>>(12)?,
                    r.get::<_, Option<i64>>(13)?,
                    r.get::<_, Option<String>>(14)?,
                    r.get::<_, i64>(15)? != 0,
                ))
            },
        )
        .optional()?;
    let Some((
        id,
        title,
        author,
        state,
        added_at,
        sname,
        sorder,
        sha,
        words,
        chapters,
        story_chapters,
        csha,
        cw,
        ch,
        csample,
        cgen,
    )) = row
    else {
        return Err(book_not_found());
    };
    if state == "deleting" {
        return Err(book_not_found());
    }
    let audiobooks: i64 = conn.query_row(
        "SELECT COUNT(*) FROM audiobooks WHERE book_id=?1",
        [&id],
        |r| r.get(0),
    )?;
    let place = match viewer {
        Some(v) => places::summary(conn, v, &id)?,
        None => Value::Null,
    };
    Ok(json!({
        "id": id,
        "title": title,
        "author": author,
        "state": state,
        "added_at": added_at,
        "series": sname.map(|n| json!({ "name": n, "order": sorder })),
        "cover": cover_value(&id, csha, cw, ch, csample, cgen),
        "chapter_count": chapters,
        "story_chapter_count": story_chapters,
        "word_count": words,
        "source_sha256": sha,
        "place": place,
        "audiobook_count": audiobooks,
    }))
}

/// Store a parsed book and its chapters in one transaction, and mark it readable.
/// Chapter text is written once here and never updated (a trigger enforces it).
pub fn store_parsed(
    conn: &mut Connection,
    state: &AppState,
    book_id: &str,
    parsed: &ParsedBook,
    thumb: Option<&Thumbnail>,
) -> Result<(), ApiError> {
    // A book without a cover gets a generated one (drawn before the transaction opens).
    let made;
    let (thumb, generated) = match thumb {
        Some(t) => (t, false),
        None => {
            made = crate::cover::generated(&parsed.title, &parsed.author);
            (&made, true)
        }
    };
    let tx = conn.transaction()?;
    let (mut story_words, mut story_chapters) = (0i64, 0i64);
    for (idx, ch) in parsed.chapters.iter().enumerate() {
        let words = crate::text::word_count(&ch.text);
        if ch.kind == "story" {
            story_words += words;
            story_chapters += 1;
        }
        let chapter_id = state.new_id();
        tx.execute(
            "INSERT INTO chapters(id,book_id,idx,title,kind,text,text_sha256,word_count,char_len) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![chapter_id, book_id, idx as i64, ch.title, ch.kind, ch.text, sha256_hex(ch.text.as_bytes()), words, ch.text.chars().count() as i64],
        )?;
        for (li, (start, end)) in ch.lines.iter().enumerate() {
            tx.execute(
                "INSERT INTO lines(id,chapter_id,idx,start,end) VALUES(?1,?2,?3,?4,?5)",
                params![
                    state.new_id(),
                    chapter_id,
                    li as i64,
                    *start as i64,
                    *end as i64
                ],
            )?;
        }
    }
    tx.execute(
        "UPDATE books SET title=?2, author=?3, state='readable', word_count=?4, chapter_count=?5, story_chapter_count=?6, cover_sha256=?7, cover_width=?8, cover_height=?9, cover_jpeg=?10, cover_sample=?11, cover_generated=?12 WHERE id=?1",
        params![
            book_id,
            parsed.title,
            parsed.author,
            story_words,
            parsed.chapters.len() as i64,
            story_chapters,
            thumb.sha256,
            thumb.width as i64,
            thumb.height as i64,
            thumb.jpeg,
            serde_json::to_string(&thumb.sample).expect("sample serializes"),
            generated
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// Give every book that has no cover a generated one. Runs at start-up. One transaction, so a crash
/// leaves either all of them or none; idempotent, because it only touches books with no cover image.
pub fn backfill_generated_covers(conn: &mut Connection) -> rusqlite::Result<usize> {
    let missing: Vec<(String, String, String)> = conn
        .prepare("SELECT id,title,author FROM books WHERE cover_jpeg IS NULL AND state IN ('readable','removed')")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    if missing.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    for (id, title, author) in &missing {
        let t = crate::cover::generated(title, author);
        tx.execute(
            "UPDATE books SET cover_sha256=?2,cover_width=?3,cover_height=?4,cover_jpeg=?5,cover_sample=?6,cover_generated=1 WHERE id=?1 AND cover_jpeg IS NULL",
            params![id, t.sha256, t.width as i64, t.height as i64, t.jpeg, serde_json::to_string(&t.sample).expect("sample serializes")],
        )?;
    }
    tx.commit()?;
    Ok(missing.len())
}

// ------------------------------------------------------------------ list

#[derive(Deserialize)]
pub struct ListQuery {
    q: Option<String>,
    filter: Option<String>,
    sort: Option<String>,
    include_removed: Option<bool>,
    limit: Option<u32>,
    after: Option<String>,
}

fn offset_cursor(after: &Option<String>) -> Result<usize, ApiError> {
    match after {
        None => Ok(0),
        Some(s) => s
            .strip_prefix('o')
            .and_then(|n| n.parse::<usize>().ok())
            .ok_or_else(|| {
                ApiError::invalid(
                    "invalid_request",
                    "after is not a cursor this server issued.",
                )
            }),
    }
}

fn like_pattern(q: &str) -> String {
    let mut s = String::from("%");
    for c in q.chars() {
        if matches!(c, '%' | '_' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('%');
    s
}

/// `listBooks`
pub async fn list(
    State(state): State<AppState>,
    listener: ListenerCtx,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = super::audit::page_limit(q.limit)?;
    let filter = q.filter.unwrap_or_else(|| "all".into());
    if !["all", "in_progress", "not_started", "finished"].contains(&filter.as_str()) {
        return Err(ApiError::invalid(
            "invalid_request",
            "filter must be all, in_progress, not_started or finished.",
        ));
    }
    let sort = q.sort.unwrap_or_else(|| "recent".into());
    let order = match sort.as_str() {
        // Books with a place first, latest place first; the rest newest added first.
        "recent" => "COALESCE(p.updated_at,'') DESC, b.added_at DESC, b.id DESC",
        "added" => "b.added_at DESC, b.id DESC",
        "title" => "b.title COLLATE NOCASE, b.id",
        "author" => "b.author COLLATE NOCASE, b.title COLLATE NOCASE, b.id",
        _ => {
            return Err(ApiError::invalid(
                "invalid_request",
                "sort must be recent, title, author or added.",
            ))
        }
    };
    let offset = offset_cursor(&q.after)?;
    let include_removed = q.include_removed.unwrap_or(false);
    let search = q.q.filter(|s| !s.trim().is_empty());
    let viewer = Viewer {
        listener: listener.id,
        now: state.clock.now(),
    };
    let page = state
        .store
        .run(move |c| {
            let mut sql = String::from(
                "SELECT b.id, p.progress, p.updated_at, p.marked_at FROM books b \
                 LEFT JOIN places p ON p.book_id=b.id AND p.listener_id=?1 WHERE b.state IN ('adding','readable'",
            );
            if include_removed {
                sql.push_str(",'removed'");
            }
            sql.push(')');
            let mut args: Vec<rusqlite::types::Value> =
                vec![rusqlite::types::Value::Text(viewer.listener.clone())];
            if let Some(s) = &search {
                sql.push_str(" AND (b.title LIKE ?2 ESCAPE '\\' OR b.author LIKE ?2 ESCAPE '\\' OR b.series_name LIKE ?2 ESCAPE '\\')");
                args.push(rusqlite::types::Value::Text(like_pattern(s)));
            }
            sql.push_str(&format!(" ORDER BY {order}"));
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(args), |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<f64>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            // Whether a place counts as finished is decided when read, so the
            // filters are applied here rather than in SQL.
            let mut ids: Vec<String> = rows
                .into_iter()
                .filter(|(_, progress, updated, marked)| {
                    let finished = match (progress, updated) {
                        (Some(p), Some(u)) => Some(
                            crate::places::finished_state(
                                marked.as_deref().map(places::parse),
                                *p,
                                places::parse(u),
                                viewer.now,
                            )
                            .finished,
                        ),
                        _ => None,
                    };
                    match filter.as_str() {
                        "in_progress" => finished == Some(false),
                        "finished" => finished == Some(true),
                        "not_started" => finished.is_none(),
                        _ => true,
                    }
                })
                .map(|(id, ..)| id)
                .skip(offset)
                .take(limit + 1)
                .collect();
            let next = if ids.len() > limit {
                ids.truncate(limit);
                Value::String(format!("o{}", offset + limit))
            } else {
                Value::Null
            };
            let items: Vec<Value> = ids.iter().map(|id| book_value(c, id, Some(&viewer))).collect::<Result<_, _>>()?;
            Ok(json!({ "items": items, "next": next }))
        })
        .await?;
    Ok(Json(page))
}

// ------------------------------------------------------------- duplicates

#[derive(Deserialize)]
pub struct DupQuery {
    sha256: String,
    title: Option<String>,
    author: Option<String>,
}

fn norm(s: &str) -> String {
    crate::cover::normalise(s)
}

/// `findDuplicateBooks`: informs; never blocks an import.
pub async fn duplicates(
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<DupQuery>,
) -> Result<Json<Value>, ApiError> {
    if q.sha256.len() != 64 || !q.sha256.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        return Err(ApiError::invalid(
            "invalid_request",
            "sha256 must be 64 lowercase hex characters.",
        ));
    }
    let out = state
        .store
        .run(move |c| {
            let dup = |c: &Connection, id: &str| -> Result<Value, ApiError> {
                let b = book_value(c, id, None)?;
                Ok(json!({
                    "book_id": b["id"], "title": b["title"], "author": b["author"],
                    "added_at": b["added_at"], "state": b["state"], "cover": b["cover"],
                }))
            };
            let exact_ids: Vec<String> = c
                .prepare("SELECT id FROM books WHERE source_sha256=?1 AND state<>'deleting' ORDER BY added_at DESC")?
                .query_map([&q.sha256], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            let mut similar_ids: Vec<String> = Vec::new();
            if let Some(title) = &q.title {
                let (nt, na) = (norm(title), norm(q.author.as_deref().unwrap_or("")));
                let all: Vec<(String, String, String, Option<String>)> = c
                    .prepare("SELECT id,title,author,source_sha256 FROM books WHERE state<>'deleting' ORDER BY added_at DESC")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                    .collect::<Result<_, _>>()?;
                for (id, t, a, sha) in all {
                    if norm(&t) == nt && norm(&a) == na && sha.as_deref() != Some(q.sha256.as_str()) {
                        similar_ids.push(id);
                    }
                }
            }
            let exact = exact_ids.iter().map(|i| dup(c, i)).collect::<Result<Vec<_>, _>>()?;
            let similar = similar_ids.iter().map(|i| dup(c, i)).collect::<Result<Vec<_>, _>>()?;
            Ok(json!({ "exact": exact, "similar": similar }))
        })
        .await?;
    Ok(Json(out))
}

// ------------------------------------------------------------ get / edit

/// `getBook`
pub async fn get_one(
    State(state): State<AppState>,
    listener: ListenerCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let viewer = Viewer {
        listener: listener.id,
        now: state.clock.now(),
    };
    Ok(Json(
        state
            .store
            .run(move |c| book_value(c, &id, Some(&viewer)))
            .await?,
    ))
}

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(d)?))
}

#[derive(Deserialize)]
pub struct SeriesIn {
    name: String,
    order: Option<f64>,
}

#[derive(Deserialize)]
pub struct BookUpdate {
    title: Option<String>,
    author: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    series: Option<Option<SeriesIn>>,
}

fn require_editable(conn: &Connection, id: &str) -> Result<String, ApiError> {
    let state: Option<String> = conn
        .query_row("SELECT state FROM books WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    match state.as_deref() {
        None => Err(book_not_found()),
        Some("removed") => Err(ApiError::conflict(
            "book_removed",
            "This book is removed. Restore it first.",
        )),
        Some("deleting") => Err(ApiError::conflict(
            "deletion_pending",
            "This book is being deleted.",
        )),
        Some("adding") => Err(ApiError::conflict(
            "book_adding",
            "This book is still being added.",
        )),
        Some(s) => Ok(s.to_string()),
    }
}

/// `updateBook`: never changes text.
pub async fn update(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: MaybeListener,
    Path(id): Path<String>,
    ApiJson(u): ApiJson<BookUpdate>,
) -> Result<Json<Value>, ApiError> {
    let viewer = listener.0.map(|l| Viewer {
        listener: l.id,
        now: state.clock.now(),
    });
    let bad = |d: &str| ApiError::invalid("invalid_request", d.to_string());
    if let Some(t) = &u.title {
        if t.trim().is_empty() || t.chars().count() > 200 {
            return Err(bad("title must be 1 to 200 characters."));
        }
    }
    if let Some(a) = &u.author {
        if a.chars().count() > 200 {
            return Err(bad("author must be at most 200 characters."));
        }
    }
    if let Some(Some(s)) = &u.series {
        if s.name.trim().is_empty()
            || s.name.chars().count() > 100
            || s.order.map(|o| !o.is_finite()).unwrap_or(false)
        {
            return Err(bad(
                "series needs a name of 1 to 100 characters and a finite order.",
            ));
        }
    }
    let (audit_id, at, actor) = (state.new_id(), state.now(), Actor::device_only(&device));
    let target = id.clone();
    let v = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            require_editable(&tx, &target)?;
            if let Some(t) = &u.title {
                tx.execute(
                    "UPDATE books SET title=?2 WHERE id=?1",
                    params![target, t.trim()],
                )?;
            }
            if let Some(a) = &u.author {
                tx.execute(
                    "UPDATE books SET author=?2 WHERE id=?1",
                    params![target, a.trim()],
                )?;
            }
            match &u.series {
                None => {}
                Some(None) => {
                    tx.execute(
                        "UPDATE books SET series_name=NULL, series_order=NULL WHERE id=?1",
                        [&target],
                    )?;
                }
                Some(Some(s)) => {
                    tx.execute(
                        "UPDATE books SET series_name=?2, series_order=?3 WHERE id=?1",
                        params![target, s.name.trim(), s.order],
                    )?;
                }
            }
            audit::record(
                &tx,
                &audit_id,
                &at,
                "book.updated",
                &actor,
                &json!({ "book_id": target }),
            )?;
            let v = book_value(&tx, &target, viewer.as_ref())?;
            tx.commit()?;
            Ok(v)
        })
        .await?;
    announce(&state, &id);
    Ok(Json(v))
}

pub fn announce(state: &AppState, book_id: &str) {
    let mut n = Notice::new("book.updated", state.now()).with_id(book_id.to_string());
    n.book_id = Some(book_id.to_string());
    state.notify(n);
}

async fn set_removed(
    state: AppState,
    device: DeviceCtx,
    listener: MaybeListener,
    id: String,
    removed: bool,
) -> Result<Json<Value>, ApiError> {
    let viewer = listener.0.map(|l| Viewer {
        listener: l.id,
        now: state.clock.now(),
    });
    let (audit_id, at, actor) = (state.new_id(), state.now(), Actor::device_only(&device));
    let target = id.clone();
    let v = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let cur: Option<String> = tx
                .query_row("SELECT state FROM books WHERE id=?1", [&target], |r| {
                    r.get(0)
                })
                .optional()?;
            let cur = cur.ok_or_else(book_not_found)?;
            match (cur.as_str(), removed) {
                ("adding", _) => {
                    return Err(ApiError::conflict(
                        "book_adding",
                        "This book is still being added.",
                    ))
                }
                ("deleting", _) => {
                    return Err(ApiError::conflict(
                        "deletion_pending",
                        "This book is being deleted.",
                    ))
                }
                ("readable", true) => {
                    tx.execute(
                        "UPDATE books SET state='removed', removed_at=?2 WHERE id=?1",
                        params![target, at],
                    )?;
                    audit::record(
                        &tx,
                        &audit_id,
                        &at,
                        "book.removed",
                        &actor,
                        &json!({ "book_id": target }),
                    )?;
                }
                ("removed", false) => {
                    tx.execute(
                        "UPDATE books SET state='readable', removed_at=NULL WHERE id=?1",
                        [&target],
                    )?;
                    audit::record(
                        &tx,
                        &audit_id,
                        &at,
                        "book.restored",
                        &actor,
                        &json!({ "book_id": target }),
                    )?;
                }
                _ => {} // already in the wanted state: idempotent
            }
            let v = book_value(&tx, &target, viewer.as_ref())?;
            tx.commit()?;
            Ok(v)
        })
        .await?;
    announce(&state, &id);
    Ok(Json(v))
}

/// `removeBook`
pub async fn remove(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: MaybeListener,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    set_removed(state, device, listener, id, true).await
}

/// `restoreBook`
pub async fn restore(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: MaybeListener,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    set_removed(state, device, listener, id, false).await
}

// ----------------------------------------------------------------- series

/// `listSeries`
pub async fn series(
    State(state): State<AppState>,
    listener: ListenerCtx,
) -> Result<Json<Value>, ApiError> {
    let viewer = Viewer {
        listener: listener.id,
        now: state.clock.now(),
    };
    let out = state
        .store
        .run(move |c| {
            let rows: Vec<(String, String, Option<f64>)> = c
                .prepare(
                    "SELECT series_name, id, series_order FROM books WHERE series_name IS NOT NULL AND state IN ('adding','readable') \
                     ORDER BY series_name COLLATE NOCASE, series_order IS NULL, series_order, title COLLATE NOCASE",
                )?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<_, _>>()?;
            type Member = (String, Option<f64>);
            let mut groups: Vec<(String, Vec<Member>)> = Vec::new();
            for (name, id, order) in rows {
                match groups.last_mut() {
                    Some((n, v)) if n.to_lowercase() == name.to_lowercase() => v.push((id, order)),
                    _ => groups.push((name, vec![(id, order)])),
                }
            }
            let mut items = Vec::new();
            for (name, members) in groups {
                let orders: Vec<f64> = members.iter().filter_map(|(_, o)| *o).collect();
                let mut missing = Vec::new();
                if !orders.is_empty() && orders.iter().all(|o| o.fract() == 0.0) {
                    let (lo, hi) = (orders.iter().cloned().fold(f64::MAX, f64::min) as i64, orders.iter().cloned().fold(f64::MIN, f64::max) as i64);
                    for n in lo..=hi.min(lo + 200) {
                        if !orders.iter().any(|o| *o as i64 == n) {
                            missing.push(json!(n));
                        }
                    }
                }
                let books = members.iter().map(|(id, _)| book_value(c, id, Some(&viewer))).collect::<Result<Vec<_>, _>>()?;
                items.push(json!({ "name": name, "books": books, "missing_orders": missing }));
            }
            Ok(json!({ "items": items }))
        })
        .await?;
    Ok(Json(out))
}

// ------------------------------------------------------ chapters and text

fn book_exists(conn: &Connection, id: &str) -> Result<(), ApiError> {
    conn.query_row("SELECT 1 FROM books WHERE id=?1", [id], |_| Ok(()))
        .optional()?
        .ok_or_else(book_not_found)
}

/// `listChapters`
pub async fn chapters(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let out = state
        .store
        .run(move |c| {
            book_exists(c, &id)?;
            let items: Vec<Value> = c
                .prepare("SELECT id,idx,title,kind,word_count,text_sha256 FROM chapters WHERE book_id=?1 ORDER BY idx")?
                .query_map([&id], |r| {
                    Ok(json!({
                        "id": r.get::<_, String>(0)?, "index": r.get::<_, i64>(1)?, "title": r.get::<_, String>(2)?,
                        "kind": r.get::<_, String>(3)?, "word_count": r.get::<_, i64>(4)?, "text_sha256": r.get::<_, String>(5)?,
                    }))
                })?
                .collect::<Result<_, _>>()?;
            Ok(json!({ "items": items }))
        })
        .await?;
    Ok(Json(out))
}

/// `getChapterText`: the exact text and line spans (code points).
pub async fn chapter_text(
    State(state): State<AppState>,
    Path((book_id, chapter_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let out = state
        .store
        .run(move |c| {
            book_exists(c, &book_id)?;
            let row: Option<(String, String)> = c
                .query_row("SELECT text,text_sha256 FROM chapters WHERE id=?1 AND book_id=?2", params![chapter_id, book_id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()?;
            let (text, sha) = row.ok_or_else(|| ApiError::not_found("chapter_not_found", "This book has no chapter with this id."))?;
            let lines: Vec<Value> = c
                .prepare("SELECT id,start,end FROM lines WHERE chapter_id=?1 ORDER BY idx")?
                .query_map([&chapter_id], |r| Ok(json!({ "id": r.get::<_, String>(0)?, "start": r.get::<_, i64>(1)?, "end": r.get::<_, i64>(2)? })))?
                .collect::<Result<_, _>>()?;
            Ok(json!({ "chapter_id": chapter_id, "text": text, "text_sha256": sha, "lines": lines }))
        })
        .await?;
    Ok(Json(out))
}

// ----------------------------------------------------------------- search

#[derive(Deserialize)]
pub struct SearchQuery {
    q: String,
    limit: Option<u32>,
    after: Option<String>,
}

/// One char folded to lower case, keeping a 1:1 mapping so offsets stay valid.
fn fold(c: char) -> char {
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

/// `searchBook`: case-insensitive, over stored text only.
pub async fn search(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    if q.q.is_empty() {
        return Err(ApiError::invalid("invalid_request", "q must not be empty."));
    }
    if q.q.chars().count() > 200 {
        return Err(ApiError::invalid(
            "invalid_request",
            "q must be at most 200 characters.",
        ));
    }
    let limit = super::audit::page_limit(q.limit)?;
    let skip = offset_cursor(&q.after)?;
    let needle: Vec<char> = q.q.chars().map(fold).collect();
    let out = state
        .store
        .run(move |c| {
            book_exists(c, &id)?;
            let chapters: Vec<(String, String)> = c
                .prepare("SELECT id,text FROM chapters WHERE book_id=?1 ORDER BY idx")?
                .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            let (mut total, mut items) = (0usize, Vec::new());
            for (chapter_id, text) in chapters {
                let chars: Vec<char> = text.chars().collect();
                let folded: Vec<char> = chars.iter().copied().map(fold).collect();
                let mut spans: Option<Vec<(String, usize, usize)>> = None;
                let mut i = 0;
                while i + needle.len() <= folded.len() {
                    if folded[i..i + needle.len()] == needle[..] {
                        if total >= skip && items.len() < limit {
                            let lines = spans.get_or_insert_with(|| {
                                c.prepare("SELECT id,start,end FROM lines WHERE chapter_id=?1 ORDER BY idx")
                                    .and_then(|mut s| {
                                        s.query_map([&chapter_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize, r.get::<_, i64>(2)? as usize)))?
                                            .collect::<Result<Vec<_>, _>>()
                                    })
                                    .unwrap_or_default()
                            });
                            let line = lines.iter().find(|(_, s, e)| i >= *s && i < *e).or_else(|| lines.first());
                            let (lo, hi) = (i.saturating_sub(40), (i + needle.len() + 40).min(chars.len()));
                            let snippet = |a: usize, b: usize| -> String { chars[a..b].iter().collect::<String>().replace('\n', " ") };
                            items.push(json!({
                                "chapter_id": chapter_id,
                                "line_id": line.map(|l| l.0.clone()).unwrap_or_default(),
                                "start": i, "end": i + needle.len(),
                                "before": snippet(lo, i), "match": snippet(i, i + needle.len()), "after": snippet(i + needle.len(), hi),
                            }));
                        }
                        total += 1;
                        i += needle.len().max(1);
                    } else {
                        i += 1;
                    }
                }
            }
            let next = if skip + items.len() < total { Value::String(format!("o{}", skip + items.len())) } else { Value::Null };
            Ok(json!({ "items": items, "total": total, "next": next }))
        })
        .await?;
    Ok(Json(out))
}

// ------------------------------------------------------------------ cover

/// `getBookCover`: immutable, because the URL carries the image hash.
pub async fn cover(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let row = state
        .store
        .run(move |c| {
            book_exists(c, &id)?;
            let r: Option<(Vec<u8>, String)> = c
                .query_row("SELECT cover_jpeg,cover_sha256 FROM books WHERE id=?1 AND cover_jpeg IS NOT NULL", [&id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            Ok(r)
        })
        .await?;
    let Some((bytes, sha)) = row else {
        return Err(ApiError::not_found(
            "cover_not_found",
            "This book has no cover.",
        ));
    };
    let mut resp = (StatusCode::OK, bytes).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    h.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{sha}\"")).expect("hex is a valid header"),
    );
    Ok(resp)
}

/// `refreshBookCover`: re-read a real cover from the saved original, if there is one; a generated
/// cover is drawn again from the current title and author. A generated cover never replaces a real one.
pub async fn refresh_cover(
    State(state): State<AppState>,
    _device: DeviceCtx,
    listener: MaybeListener,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let viewer = listener.0.map(|l| Viewer {
        listener: l.id,
        now: state.clock.now(),
    });
    let data_dir = state.store.data_dir().to_path_buf();
    let lookup = id.clone();
    let (ext, name, generated, title, author): (
        Option<String>,
        Option<String>,
        bool,
        String,
        String,
    ) = state
        .store
        .run(move |c| {
            book_exists(c, &lookup)?;
            Ok(c.query_row(
                "SELECT source_ext,source_name,cover_generated,title,author FROM books WHERE id=?1",
                [&lookup],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get::<_, i64>(2)? != 0,
                        r.get(3)?,
                        r.get(4)?,
                    ))
                },
            )?)
        })
        .await?;
    if generated {
        let t = tokio::task::spawn_blocking(move || crate::cover::generated(&title, &author))
            .await
            .map_err(ApiError::internal)?;
        let target = id.clone();
        let changed = state
            .store
            .run(move |c| {
                // Only while the cover is still a generated one: a real cover is never replaced.
                let n = c.execute(
                    "UPDATE books SET cover_sha256=?2,cover_width=?3,cover_height=?4,cover_jpeg=?5,cover_sample=?6 WHERE id=?1 AND cover_generated=1 AND cover_sha256 IS NOT ?2",
                    params![target, t.sha256, t.width as i64, t.height as i64, t.jpeg, serde_json::to_string(&t.sample).expect("sample")],
                )?;
                Ok(n > 0)
            })
            .await?;
        if changed {
            announce(&state, &id);
        }
    } else if let (Some(ext), Some(name)) = (ext, name) {
        let path = data_dir
            .join("originals")
            .join(&id)
            .join(format!("source.{ext}"));
        if let Ok(bytes) = tokio::fs::read(&path).await {
            let thumb = tokio::task::spawn_blocking(move || {
                crate::importer::parse(&name, &bytes)
                    .ok()
                    .and_then(|p| p.cover)
                    .and_then(|c| crate::cover::thumbnail(&c).ok())
            })
            .await
            .map_err(ApiError::internal)?;
            if let Some(t) = thumb {
                let target = id.clone();
                state
                    .store
                    .run(move |c| {
                        c.execute(
                            "UPDATE books SET cover_sha256=?2,cover_width=?3,cover_height=?4,cover_jpeg=?5,cover_sample=?6,cover_generated=0 WHERE id=?1",
                            params![target, t.sha256, t.width as i64, t.height as i64, t.jpeg, serde_json::to_string(&t.sample).expect("sample")],
                        )?;
                        Ok(())
                    })
                    .await?;
                announce(&state, &id);
            }
        }
    }
    let target = id.clone();
    Ok(Json(
        state
            .store
            .run(move |c| book_value(c, &target, viewer.as_ref()))
            .await?,
    ))
}
