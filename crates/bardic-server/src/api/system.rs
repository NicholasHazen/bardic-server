use super::{audit, clean_name, ApiJson};
use crate::{
    app::{Actor, AppState, DeviceCtx},
    error::ApiError,
    events::Notice,
    API_VERSION,
};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures_util::{stream, Stream, StreamExt};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{convert::Infallible, time::Duration};
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};

/// `getHealth`
pub async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "ok": true, "time": state.now() }))
}

#[derive(Serialize)]
pub struct ServerView {
    id: String,
    name: String,
    version: &'static str,
    api_version: &'static str,
    max_upload_bytes: u64,
    free_bytes: Option<u64>,
    time: String,
}

async fn server_view(state: &AppState) -> Result<ServerView, ApiError> {
    let (id, name) = state
        .store
        .run(|c| {
            let get = |k: &str| -> rusqlite::Result<String> {
                c.query_row("SELECT value FROM meta WHERE key=?1", [k], |r| r.get(0))
            };
            Ok((get("server_id")?, get("server_name")?))
        })
        .await?;
    Ok(ServerView {
        id,
        name,
        version: env!("CARGO_PKG_VERSION"),
        api_version: API_VERSION,
        max_upload_bytes: state.config.max_upload_bytes,
        free_bytes: fs4::available_space(state.store.data_dir()).ok(),
        time: state.now(),
    })
}

/// `getServer`
pub async fn get_server(State(state): State<AppState>) -> Result<Json<ServerView>, ApiError> {
    Ok(Json(server_view(&state).await?))
}

#[derive(Deserialize)]
pub struct NameInput {
    name: String,
}

/// `updateServer`
pub async fn update_server(
    State(state): State<AppState>,
    device: DeviceCtx,
    ApiJson(input): ApiJson<NameInput>,
) -> Result<Json<ServerView>, ApiError> {
    let name = clean_name(&input.name, "server", 60)?;
    let (audit_id, at, actor) = (state.new_id(), state.now(), Actor::device_only(&device));
    let new_name = name.clone();
    let server_id = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let old: String =
                tx.query_row("SELECT value FROM meta WHERE key='server_name'", [], |r| {
                    r.get(0)
                })?;
            tx.execute(
                "UPDATE meta SET value=?1 WHERE key='server_name'",
                [&new_name],
            )?;
            audit::record(
                &tx,
                &audit_id,
                &at,
                "server.renamed",
                &actor,
                &json!({ "from": old, "to": new_name }),
            )?;
            let id: String =
                tx.query_row("SELECT value FROM meta WHERE key='server_id'", [], |r| {
                    r.get(0)
                })?;
            tx.commit()?;
            Ok(id)
        })
        .await?;
    state.notify(Notice::new("server.updated", state.now()).with_id(server_id));
    Ok(Json(server_view(&state).await?))
}

#[derive(Serialize)]
pub struct DeviceView {
    id: String,
    name: String,
    first_seen_at: String,
    last_seen_at: String,
}

#[derive(Serialize)]
pub struct DeviceList {
    items: Vec<DeviceView>,
}

fn device_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceView> {
    Ok(DeviceView {
        id: r.get(0)?,
        name: r.get(1)?,
        first_seen_at: r.get(2)?,
        last_seen_at: r.get(3)?,
    })
}

/// `listDevices`
pub async fn list_devices(State(state): State<AppState>) -> Result<Json<DeviceList>, ApiError> {
    let items = state
        .store
        .run(|c| {
            let mut stmt =
                c.prepare("SELECT id,name,first_seen_at,last_seen_at FROM devices ORDER BY last_seen_at DESC, id DESC")?;
            let rows = stmt.query_map([], device_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    Ok(Json(DeviceList { items }))
}

/// `updateDevice`
pub async fn update_device(
    State(state): State<AppState>,
    device: DeviceCtx,
    Path(device_id): Path<String>,
    ApiJson(input): ApiJson<NameInput>,
) -> Result<Json<DeviceView>, ApiError> {
    let name = clean_name(&input.name, "device", 60)?;
    let (audit_id, at, actor) = (state.new_id(), state.now(), Actor::device_only(&device));
    let (target_id, new_name) = (device_id.clone(), name.clone());
    let view = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let old: Option<String> = tx
                .query_row("SELECT name FROM devices WHERE id=?1", [&target_id], |r| {
                    r.get(0)
                })
                .optional()?;
            let Some(old) = old else {
                return Err(ApiError::not_found(
                    "device_not_found",
                    "No device has this id.",
                ));
            };
            tx.execute(
                "UPDATE devices SET name=?2 WHERE id=?1",
                params![target_id, new_name],
            )?;
            audit::record(
                &tx,
                &audit_id,
                &at,
                "device.renamed",
                &actor,
                &json!({ "device_id": target_id, "from": old, "to": new_name }),
            )?;
            let view = tx.query_row(
                "SELECT id,name,first_seen_at,last_seen_at FROM devices WHERE id=?1",
                [&target_id],
                device_row,
            )?;
            tx.commit()?;
            Ok(view)
        })
        .await?;
    state.notify(Notice::new("device.updated", state.now()).with_id(device_id));
    Ok(Json(view))
}

fn to_event(id: Option<u64>, notice: &Notice) -> Event {
    let mut e = Event::default()
        .event("notice")
        .json_data(notice)
        .expect("notice serializes");
    if let Some(id) = id {
        e = e.id(id.to_string());
    }
    e
}

/// `streamEvents`
pub async fn stream_events(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let last = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let listener = headers
        .get("x-bardic-listener")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let sub = state.events.subscribe(last);
    let mut stop = state.shutdown.subscribe();
    let now = state.now();
    let mut initial: Vec<Result<Event, Infallible>> = Vec::new();
    if sub.resync {
        initial.push(Ok(to_event(None, &Notice::new("resync", now.clone()))));
    }
    for (id, n) in &sub.backlog {
        if visible(&n.listener_id, &listener) {
            initial.push(Ok(to_event(Some(*id), n)));
        }
    }
    let l2 = listener.clone();
    let live = BroadcastStream::new(sub.rx).filter_map(move |item| {
        let l = l2.clone();
        let at = now.clone();
        async move {
            match item {
                Ok((id, n)) if visible(&n.listener_id, &l) => Some(Ok(to_event(Some(id), &n))),
                Ok(_) => None,
                // The subscriber fell behind: tell it to reload.
                Err(BroadcastStreamRecvError::Lagged(_)) => {
                    Some(Ok(to_event(None, &Notice::new("resync", at))))
                }
            }
        }
    });
    let live = live.take_until(async move {
        let _ = stop.wait_for(|v| *v).await;
    });
    Sse::new(stream::iter(initial).chain(live))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Listener-scoped notices go only to that listener's clients; others go to all.
fn visible(notice_listener: &Option<String>, subscriber: &Option<String>) -> bool {
    match (notice_listener, subscriber) {
        (Some(n), Some(s)) => n == s,
        _ => true,
    }
}
