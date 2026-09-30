use super::{audit, clean_name, ApiJson};
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
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize)]
pub struct ListenerView {
    id: String,
    name: String,
    created_at: String,
    last_listened_at: Option<String>,
    books_started: i64,
}

#[derive(Serialize)]
pub struct ListenerList {
    items: Vec<ListenerView>,
}

#[derive(Deserialize)]
pub struct ListenerInput {
    name: String,
}

/// Number of books this listener has a place in.
fn books_started(conn: &Connection, listener_id: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM places WHERE listener_id=?1",
        [listener_id],
        |r| r.get(0),
    )
}

fn view(conn: &Connection, id: &str) -> Result<ListenerView, ApiError> {
    let row = conn
        .query_row(
            "SELECT id,name,created_at,last_listened_at FROM listeners WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((id, name, created_at, last_listened_at)) = row else {
        return Err(ApiError::not_found(
            "listener_not_found",
            "No listener has this id.",
        ));
    };
    let books_started = books_started(conn, &id)?;
    Ok(ListenerView {
        id,
        name,
        created_at,
        last_listened_at,
        books_started,
    })
}

fn name_taken() -> ApiError {
    ApiError::conflict(
        "name_taken",
        "Another listener already has that name (names are not case-sensitive).",
    )
}

/// `listListeners`
pub async fn list(State(state): State<AppState>) -> Result<Json<ListenerList>, ApiError> {
    let items = state
        .store
        .run(|c| {
            let ids: Vec<String> = c
                .prepare("SELECT id FROM listeners ORDER BY name_key, id")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            ids.iter().map(|id| view(c, id)).collect()
        })
        .await?;
    Ok(Json(ListenerList { items }))
}

/// `createListener`
pub async fn create(
    State(state): State<AppState>,
    device: DeviceCtx,
    ApiJson(input): ApiJson<ListenerInput>,
) -> Result<(StatusCode, Json<ListenerView>), ApiError> {
    let name = clean_name(&input.name, "listener", 40)?;
    let (id, audit_id, at, actor) = (
        state.new_id(),
        state.new_id(),
        state.now(),
        Actor::device_only(&device),
    );
    let new_id = id.clone();
    let view = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let key = name.to_lowercase();
            let taken: bool = tx
                .query_row("SELECT 1 FROM listeners WHERE name_key=?1", [&key], |_| {
                    Ok(true)
                })
                .optional()?
                .unwrap_or(false);
            if taken {
                return Err(name_taken());
            }
            tx.execute(
                "INSERT INTO listeners(id,name,name_key,created_at) VALUES(?1,?2,?3,?4)",
                params![new_id, name, key, at],
            )?;
            tx.execute(
                "INSERT INTO listener_settings(listener_id) VALUES(?1)",
                [&new_id],
            )?;
            audit::record(
                &tx,
                &audit_id,
                &at,
                "listener.created",
                &actor,
                &json!({ "listener_id": new_id, "name": name }),
            )?;
            let v = view(&tx, &new_id)?;
            tx.commit()?;
            Ok(v)
        })
        .await?;
    state.notify(Notice::new("listener.updated", state.now()).with_id(id));
    Ok((StatusCode::CREATED, Json(view)))
}

/// `getListener`
pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ListenerView>, ApiError> {
    Ok(Json(state.store.run(move |c| view(c, &id)).await?))
}

/// `renameListener`
pub async fn rename(
    State(state): State<AppState>,
    device: DeviceCtx,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<ListenerInput>,
) -> Result<Json<ListenerView>, ApiError> {
    let name = clean_name(&input.name, "listener", 40)?;
    let (audit_id, at, actor) = (state.new_id(), state.now(), Actor::device_only(&device));
    let target = id.clone();
    let v = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let old: Option<String> = tx
                .query_row("SELECT name FROM listeners WHERE id=?1", [&target], |r| {
                    r.get(0)
                })
                .optional()?;
            let Some(old) = old else {
                return Err(ApiError::not_found(
                    "listener_not_found",
                    "No listener has this id.",
                ));
            };
            let key = name.to_lowercase();
            let clash: bool = tx
                .query_row(
                    "SELECT 1 FROM listeners WHERE name_key=?1 AND id<>?2",
                    params![key, target],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if clash {
                return Err(name_taken());
            }
            tx.execute(
                "UPDATE listeners SET name=?2, name_key=?3 WHERE id=?1",
                params![target, name, key],
            )?;
            audit::record(
                &tx,
                &audit_id,
                &at,
                "listener.renamed",
                &actor,
                &json!({ "listener_id": target, "from": old, "to": name }),
            )?;
            let v = view(&tx, &target)?;
            tx.commit()?;
            Ok(v)
        })
        .await?;
    state.notify(Notice::new("listener.updated", state.now()).with_id(id));
    Ok(Json(v))
}

/// `deleteListener`: removes that listener's places, history and settings only.
pub async fn delete(
    State(state): State<AppState>,
    device: DeviceCtx,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let (audit_id, at, actor) = (state.new_id(), state.now(), Actor::device_only(&device));
    let target = id.clone();
    state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let name: Option<String> = tx
                .query_row("SELECT name FROM listeners WHERE id=?1", [&target], |r| {
                    r.get(0)
                })
                .optional()?;
            let Some(name) = name else {
                return Err(ApiError::not_found(
                    "listener_not_found",
                    "No listener has this id.",
                ));
            };
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM listeners", [], |r| r.get(0))?;
            if count <= 1 {
                return Err(ApiError::conflict(
                    "last_listener",
                    "Bardic always keeps one listener, so the last one cannot be deleted.",
                ));
            }
            let started = books_started(&tx, &target)?;
            tx.execute("DELETE FROM listeners WHERE id=?1", [&target])?;
            audit::record(
                &tx,
                &audit_id,
                &at,
                "listener.deleted",
                &actor,
                &json!({ "listener_id": target, "name": name, "books_started": started }),
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    state.notify(Notice::new("listener.updated", state.now()).with_id(id));
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct Impact {
    books_started: i64,
    places: i64,
    history_entries: i64,
}

/// `getListenerImpact`: what deleting would remove.
pub async fn impact(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Impact>, ApiError> {
    let v = state
        .store
        .run(move |c| {
            let exists: bool = c
                .query_row("SELECT 1 FROM listeners WHERE id=?1", [&id], |_| Ok(true))
                .optional()?
                .unwrap_or(false);
            if !exists {
                return Err(ApiError::not_found(
                    "listener_not_found",
                    "No listener has this id.",
                ));
            }
            let started = books_started(c, &id)?;
            // One current place per started book, plus its earlier places.
            let history_entries: i64 = c.query_row(
                "SELECT COUNT(*) FROM place_history WHERE listener_id=?1",
                [&id],
                |r| r.get(0),
            )?;
            Ok(Impact {
                books_started: started,
                places: started,
                history_entries,
            })
        })
        .await?;
    Ok(Json(v))
}

#[derive(Serialize, Deserialize)]
pub struct Settings {
    default_voice_id: Option<String>,
    place_conflict: String,
    continue_into_next_chapter: bool,
}

fn load_settings(conn: &Connection, id: &str) -> Result<Settings, ApiError> {
    let row = conn
        .query_row(
            "SELECT default_voice_id,place_conflict,continue_into_next_chapter FROM listener_settings WHERE listener_id=?1",
            [id],
            |r| Ok(Settings { default_voice_id: r.get(0)?, place_conflict: r.get(1)?, continue_into_next_chapter: r.get::<_, i64>(2)? != 0 }),
        )
        .optional()?;
    row.ok_or_else(|| ApiError::not_found("listener_not_found", "No listener has this id."))
}

/// `getListenerSettings`
pub async fn get_settings(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Settings>, ApiError> {
    Ok(Json(state.store.run(move |c| load_settings(c, &id)).await?))
}

/// `putListenerSettings`
pub async fn put_settings(
    State(state): State<AppState>,
    device: DeviceCtx,
    Path(id): Path<String>,
    ApiJson(s): ApiJson<Settings>,
) -> Result<Json<Settings>, ApiError> {
    if !["ask", "newest", "this_device"].contains(&s.place_conflict.as_str()) {
        return Err(ApiError::invalid(
            "invalid_request",
            "place_conflict must be ask, newest or this_device.",
        ));
    }
    // Voices arrive with M3; until then no id can be valid.
    if let Some(v) = &s.default_voice_id {
        let v = v.clone();
        if !state.store.run(move |c| voice_exists(c, &v)).await? {
            return Err(ApiError::not_found(
                "voice_not_found",
                "No voice has this id.",
            ));
        }
    }
    let (audit_id, at, actor) = (state.new_id(), state.now(), Actor::device_only(&device));
    let target = id.clone();
    let out = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            load_settings(&tx, &target)?;
            tx.execute(
                "UPDATE listener_settings SET default_voice_id=?2, place_conflict=?3, continue_into_next_chapter=?4 WHERE listener_id=?1",
                params![target, s.default_voice_id, s.place_conflict, s.continue_into_next_chapter as i64],
            )?;
            audit::record(
                &tx,
                &audit_id,
                &at,
                "listener.settings_changed",
                &actor,
                &json!({ "listener_id": target, "place_conflict": s.place_conflict, "default_voice_id": s.default_voice_id, "continue_into_next_chapter": s.continue_into_next_chapter }),
            )?;
            let out = load_settings(&tx, &target)?;
            tx.commit()?;
            Ok(out)
        })
        .await?;
    state.notify(Notice::new("listener.updated", state.now()).with_id(id));
    Ok(Json(out))
}

fn voice_exists(_conn: &Connection, _id: &str) -> Result<bool, ApiError> {
    Ok(false)
}
