use super::ApiQuery;
use crate::{
    app::{Actor, AppState},
    error::ApiError,
};
use axum::{extract::State, Json};
use rusqlite::{params, types::Value as Sql, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Append one audit record. Audit is never updated or deleted.
pub fn record(
    conn: &Connection,
    id: &str,
    at: &str,
    action: &str,
    actor: &Actor,
    target: &Value,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO audit(id,at,action,listener_id,listener_name,device_id,device_name,target) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![id, at, action, actor.listener_id, actor.listener_name, actor.device_id, actor.device_name, target.to_string()],
    )?;
    Ok(())
}

#[derive(Deserialize)]
pub struct AuditQuery {
    listener_id: Option<String>,
    action: Option<String>,
    limit: Option<u32>,
    after: Option<String>,
}

#[derive(Serialize)]
pub struct AuditRecord {
    id: String,
    at: String,
    action: String,
    actor: Actor,
    target: Value,
}

#[derive(Serialize)]
pub struct AuditPage {
    items: Vec<AuditRecord>,
    next: Option<String>,
}

pub fn page_limit(limit: Option<u32>) -> Result<usize, ApiError> {
    match limit {
        None => Ok(50),
        Some(n) if (1..=200).contains(&n) => Ok(n as usize),
        Some(_) => Err(ApiError::invalid(
            "invalid_request",
            "limit must be between 1 and 200.",
        )),
    }
}

/// `listAudit`
pub async fn list_audit(
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<AuditQuery>,
) -> Result<Json<AuditPage>, ApiError> {
    let limit = page_limit(q.limit)?;
    let page = state
        .store
        .run(move |c| {
            let mut sql = String::from(
                "SELECT id,at,action,listener_id,listener_name,device_id,device_name,target FROM audit WHERE 1=1",
            );
            let mut args: Vec<Sql> = Vec::new();
            if let Some(l) = q.listener_id {
                sql.push_str(" AND listener_id=?");
                args.push(Sql::Text(l));
            }
            if let Some(a) = q.action {
                sql.push_str(" AND action=?");
                args.push(Sql::Text(a));
            }
            if let Some(after) = q.after {
                sql.push_str(" AND id < ?");
                args.push(Sql::Text(after));
            }
            sql.push_str(" ORDER BY id DESC LIMIT ?");
            args.push(Sql::Integer(limit as i64 + 1));
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
                let target: String = r.get(7)?;
                Ok(AuditRecord {
                    id: r.get(0)?,
                    at: r.get(1)?,
                    action: r.get(2)?,
                    actor: Actor {
                        listener_id: r.get(3)?,
                        listener_name: r.get(4)?,
                        device_id: r.get(5)?,
                        device_name: r.get(6)?,
                    },
                    target: serde_json::from_str(&target).unwrap_or(Value::Null),
                })
            })?;
            let mut items: Vec<AuditRecord> = rows.collect::<Result<_, _>>()?;
            let next = if items.len() > limit {
                items.truncate(limit);
                items.last().map(|i| i.id.clone())
            } else {
                None
            };
            Ok(AuditPage { items, next })
        })
        .await?;
    Ok(Json(page))
}
