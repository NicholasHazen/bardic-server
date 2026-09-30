//! Voice sources and the voices they report. Listing is free and read-only.

use super::{audit, ApiJson, ApiQuery};
use crate::{
    app::{Actor, AppState, DeviceCtx},
    error::ApiError,
    events::Notice,
    voices::{breeze, Catalog, Check},
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

fn source_not_found() -> ApiError {
    ApiError::not_found("source_not_found", "No voice source has this id.")
}

fn tier_of(id: &str) -> &'static str {
    if id == "gemini" {
        "premium"
    } else {
        "free"
    }
}

fn name_of(id: &str) -> &'static str {
    match id {
        "breeze" => "Breeze",
        "gemini" => "Gemini",
        _ => "This computer",
    }
}

#[derive(Default, Clone)]
struct Config {
    base_url: Option<String>,
    api_key: Option<String>,
}

impl Config {
    fn parse(s: &str) -> Self {
        let v: Value = serde_json::from_str(s).unwrap_or(Value::Null);
        let get = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Config {
            base_url: get("base_url"),
            api_key: get("api_key"),
        }
    }
    fn to_json(&self) -> String {
        json!({ "base_url": self.base_url, "api_key": self.api_key }).to_string()
    }
}

struct Row {
    config: Config,
    state: String,
    detail: Option<String>,
    checked_at: Option<String>,
}

fn load(conn: &Connection, id: &str) -> Result<Row, ApiError> {
    conn.query_row(
        "SELECT config,state,detail,checked_at FROM voice_sources WHERE id=?1",
        [id],
        |r| {
            Ok(Row {
                config: Config::parse(&r.get::<_, String>(0)?),
                state: r.get(1)?,
                detail: r.get(2)?,
                checked_at: r.get(3)?,
            })
        },
    )
    .optional()?
    .ok_or_else(source_not_found)
}

fn view(conn: &Connection, id: &str) -> Result<Value, ApiError> {
    let row = load(conn, id)?;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM voices WHERE source_id=?1 AND available=1",
        [id],
        |r| r.get(0),
    )?;
    Ok(json!({
        "id": id,
        "kind": id,
        "name": name_of(id),
        "tier": tier_of(id),
        "state": row.state,
        "voice_count": count,
        "checked_at": row.checked_at,
        "detail": row.detail,
        "base_url": if id == "breeze" { json!(row.config.base_url) } else { Value::Null },
        "has_key": row.config.api_key.as_deref().is_some_and(|k| !k.is_empty()),
    }))
}

/// Record a check: the state, and the voices. A failed check keeps the last
/// known voices but marks them unavailable.
fn apply(
    conn: &mut Connection,
    state: &AppState,
    id: &str,
    catalog: &Catalog,
    at: &str,
) -> Result<(), ApiError> {
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE voice_sources SET state=?2, detail=?3, checked_at=?4, updated_at=?4 WHERE id=?1",
        params![id, catalog.check.state(), catalog.detail, at],
    )?;
    tx.execute("UPDATE voices SET available=0 WHERE source_id=?1", [id])?;
    if catalog.check == Check::Connected {
        for v in &catalog.voices {
            tx.execute(
                "INSERT INTO voices(id,source_id,external_id,name,tier,language,description,revision,available,updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1,?9)
                 ON CONFLICT(source_id,external_id) DO UPDATE SET name=excluded.name, language=excluded.language, description=excluded.description, revision=excluded.revision, available=1, updated_at=excluded.updated_at",
                params![state.new_id(), id, v.external_id, v.name, tier_of(id), v.language, v.description, v.revision, at],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn announce(state: &AppState, id: &str) {
    state.notify(Notice::new("source.updated", state.now()).with_id(id.to_string()));
}

/// `listVoiceSources`: always the three known kinds.
pub async fn list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let v = state
        .store
        .run(|c| {
            let items = ["breeze", "gemini", "local"]
                .iter()
                .map(|id| view(c, id))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({ "items": items }))
        })
        .await?;
    Ok(Json(v))
}

/// `getVoiceSource`
pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.store.run(move |c| view(c, &id)).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigIn {
    base_url: Option<String>,
    api_key: Option<String>,
    #[allow(dead_code)]
    enabled: Option<bool>,
}

/// Read the stored config, contact the source (outside any database lock), store the outcome.
async fn check_source(state: &AppState, id: &str, config: Config) -> Result<Catalog, ApiError> {
    match (id, &config.base_url) {
        ("breeze", Some(url)) => {
            let catalog = breeze::fetch_catalog(url, config.api_key.as_deref()).await;
            let (sid, at, st) = (id.to_string(), state.now(), state.clone());
            let c2 = catalog.clone();
            state
                .store
                .run(move |c| apply(c, &st, &sid, &c2, &at))
                .await?;
            Ok(catalog)
        }
        _ => Err(ApiError::invalid(
            "source_not_set_up",
            "This source is not set up.",
        )),
    }
}

/// `configureVoiceSource`: tests the configuration first; stores nothing on failure.
pub async fn configure(
    State(state): State<AppState>,
    device: DeviceCtx,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<ConfigIn>,
) -> Result<Json<Value>, ApiError> {
    let sid = id.clone();
    let current = state.store.run(move |c| load(c, &sid)).await?;
    match id.as_str() {
        "breeze" => {
            let base_url = match &input.base_url {
                Some(u) => breeze::normalize_base_url(u)
                    .map_err(|m| ApiError::invalid("invalid_request", m))?,
                None => current.config.base_url.clone().ok_or_else(|| {
                    ApiError::invalid(
                        "invalid_request",
                        "Breeze needs base_url, the address of your Breeze server.",
                    )
                })?,
            };
            // Omitted keeps the stored key; an empty string clears it.
            let api_key = match &input.api_key {
                Some(k) if k.trim().is_empty() => None,
                Some(k) => Some(k.trim().to_string()),
                None => current.config.api_key.clone(),
            };
            let config = Config {
                base_url: Some(base_url.clone()),
                api_key: api_key.clone(),
            };
            let catalog = breeze::fetch_catalog(&base_url, api_key.as_deref()).await;
            match catalog.check {
                Check::Unreachable => {
                    return Err(ApiError::invalid("source_unreachable", catalog.detail))
                }
                Check::KeyRejected => {
                    return Err(ApiError::invalid("key_rejected", catalog.detail))
                }
                Check::Connected => {}
            }
            let (audit_id, at, actor, st) = (
                state.new_id(),
                state.now(),
                Actor::device_only(&device),
                state.clone(),
            );
            let sid = id.clone();
            let v = state
                .store
                .run(move |c| {
                    c.execute(
                        "UPDATE voice_sources SET config=?2 WHERE id=?1",
                        params![sid, config.to_json()],
                    )?;
                    apply(c, &st, &sid, &catalog, &at)?;
                    audit::record(
                        c,
                        &audit_id,
                        &at,
                        "voice_source.configured",
                        &actor,
                        &json!({ "source_id": sid, "base_url": base_url }),
                    )?;
                    view(c, &sid)
                })
                .await?;
            announce(&state, &id);
            Ok(Json(v))
        }
        "gemini" => Err(ApiError::invalid(
            "source_unsupported",
            "Gemini voices arrive with premium audio; this server cannot use them yet.",
        )),
        _ => {
            let (audit_id, at, actor) = (state.new_id(), state.now(), Actor::device_only(&device));
            let sid = id.clone();
            let v = state
                .store
                .run(move |c| {
                    audit::record(
                        c,
                        &audit_id,
                        &at,
                        "voice_source.configured",
                        &actor,
                        &json!({ "source_id": sid }),
                    )?;
                    view(c, &sid)
                })
                .await?;
            Ok(Json(v))
        }
    }
}

async fn recheck(state: AppState, id: String) -> Result<Json<Value>, ApiError> {
    let sid = id.clone();
    let current = state.store.run(move |c| load(c, &sid)).await?;
    if id == "breeze" && current.config.base_url.is_some() {
        check_source(&state, &id, current.config).await?;
        announce(&state, &id);
    }
    Ok(Json(state.store.run(move |c| view(c, &id)).await?))
}

/// `testVoiceSource`: reads the voice list only.
pub async fn test(
    State(state): State<AppState>,
    _device: DeviceCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    recheck(state, id).await
}

/// `refreshVoiceSource`
pub async fn refresh(
    State(state): State<AppState>,
    _device: DeviceCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    recheck(state, id).await
}

/// `removeVoiceSource`: forgets configuration and key; voices stay (unavailable) so audio made with them stays playable.
pub async fn remove(
    State(state): State<AppState>,
    device: DeviceCtx,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let (audit_id, at, actor) = (state.new_id(), state.now(), Actor::device_only(&device));
    let sid = id.clone();
    state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            load(&tx, &sid)?;
            let (st, detail) = if sid == "local" { ("unavailable", Some("This server has no voices of its own.")) } else { ("not_set_up", None) };
            tx.execute(
                "UPDATE voice_sources SET config='{}', state=?2, detail=?3, checked_at=NULL, updated_at=?4 WHERE id=?1",
                params![sid, st, detail, at],
            )?;
            tx.execute("UPDATE voices SET available=0 WHERE source_id=?1", [&sid])?;
            audit::record(&tx, &audit_id, &at, "voice_source.removed", &actor, &json!({ "source_id": sid }))?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    announce(&state, &id);
    Ok(StatusCode::NO_CONTENT)
}

pub fn voice_value(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, String>(0)?,
        "source_id": r.get::<_, String>(1)?,
        "name": r.get::<_, String>(2)?,
        "tier": r.get::<_, String>(3)?,
        "language": r.get::<_, String>(4)?,
        "description": r.get::<_, String>(5)?,
        "revision": r.get::<_, String>(6)?,
        "available": r.get::<_, i64>(7)? != 0,
    }))
}

pub const VOICE_COLS: &str = "id,source_id,name,tier,language,description,revision,available";

#[derive(Deserialize)]
pub struct VoiceQuery {
    source_id: Option<String>,
    tier: Option<String>,
    language: Option<String>,
}

/// `listVoices`
pub async fn list_voices(
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<VoiceQuery>,
) -> Result<Json<Value>, ApiError> {
    if let Some(t) = &q.tier {
        if !["free", "premium"].contains(&t.as_str()) {
            return Err(ApiError::invalid(
                "invalid_request",
                "tier must be free or premium.",
            ));
        }
    }
    let v = state
        .store
        .run(move |c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {VOICE_COLS} FROM voices WHERE (?1 IS NULL OR source_id=?1) AND (?2 IS NULL OR tier=?2) AND (?3 IS NULL OR language=?3 COLLATE NOCASE)
                 ORDER BY available DESC, tier, name COLLATE NOCASE, id"
            ))?;
            let items = stmt
                .query_map(params![q.source_id, q.tier, q.language], voice_value)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({ "items": items }))
        })
        .await?;
    Ok(Json(v))
}

pub fn voice_exists(conn: &Connection, id: &str) -> Result<bool, ApiError> {
    Ok(conn
        .query_row("SELECT 1 FROM voices WHERE id=?1", [id], |_| Ok(()))
        .optional()?
        .is_some())
}
