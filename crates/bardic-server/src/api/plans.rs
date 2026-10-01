//! Plans: the approval that lets premium audio be made, and the limit it is made within.
//!
//! `previewPlan` spends nothing and returns a range. `createPlan` is the approval: it needs the
//! preview's `estimate_id` and the limit the listener accepted, so what was shown is what is approved.
//! A plan's state is its job's state (one source of truth); its limit and estimate are its own.

use super::{
    audio::{has_audio, resolve_scope, target, ScopeIn},
    audit, money, ApiJson, ApiQuery,
};
use crate::{
    app::{Actor, AppState, DeviceCtx, ListenerCtx},
    error::ApiError,
    events::Notice,
    jobs, plans, spend,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::Duration;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

const EXPIRES_MINUTES: i64 = 15;
const ACTIVE: &str = "('queued','running','waiting','paused','needs_you')";

fn plan_not_found() -> ApiError {
    ApiError::not_found("plan_not_found", "No plan has this id.")
}

fn detail(code: &str, text: &str) -> Value {
    json!({ "code": code, "text": text, "until": Value::Null })
}

/// The contract's `Plan`.
pub fn plan_value(conn: &Connection, id: &str) -> Result<Value, ApiError> {
    let row = conn
        .query_row(
            "SELECT audiobook_id,book_id,scope,est_low,est_likely,est_high,prices_as_of,basis,limit_micros,job_id,approved_by,created_at FROM plans WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    (r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?),
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, String>(10)?,
                    r.get::<_, String>(11)?,
                ))
            },
        )
        .optional()?;
    let Some((
        audiobook,
        book,
        scope,
        (low, likely, high),
        as_of,
        basis,
        limit,
        job_id,
        approved,
        created,
    )) = row
    else {
        return Err(plan_not_found());
    };
    let job = jobs::job_value(conn, &job_id)?;
    let (known, unknown): (i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(CASE WHEN status='known' THEN known_micros END),0), COALESCE(SUM(status='unknown'),0) FROM spend WHERE plan_id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // A plan that is queued or running is running; every other state is the job's own.
    let state = match job["state"].as_str().unwrap_or("failed") {
        "queued" => "running",
        s => s,
    };
    Ok(json!({
        "id": id,
        "audiobook_id": audiobook,
        "book_id": book,
        "scope": serde_json::from_str::<ScopeIn>(&scope).map(|s| s.value()).unwrap_or(Value::Null),
        "state": state,
        "estimate": { "low": money::money(low), "likely": money::money(likely), "high": money::money(high), "prices_as_of": as_of, "basis": basis },
        "limit": money::money(limit),
        "spent": { "known": money::money(known), "unknown_items": unknown },
        "job_id": job_id,
        "chapters_total": job["chapters_total"],
        "chapters_done": job["chapters_done"],
        "waiting": job["waiting"],
        "needs_you": job["needs_you"],
        "approved_by": serde_json::from_str::<Value>(&approved).unwrap_or(Value::Null),
        "created_at": created,
        "updated_at": job["updated_at"],
    }))
}

fn premium_target(conn: &Connection, audiobook: &str) -> Result<super::audio::Target, ApiError> {
    let t = target(conn, audiobook)?;
    if t.tier != "premium" {
        return Err(ApiError::conflict(
            "plan_not_needed",
            "Free voices never need a plan.",
        ));
    }
    match t.source_state.as_str() {
        "not_set_up" => Err(ApiError::conflict(
            "source_not_set_up",
            "This voice's source is not set up.",
        )),
        "key_rejected" => Err(ApiError::conflict(
            "key_rejected",
            "The API key was rejected.",
        )),
        _ => Ok(t),
    }
}

struct Quote {
    chapter_ids: Vec<String>,
    reused: i64,
    chars: i64,
    range: plans::Range,
    per_unit: i64,
    as_of: String,
    basis: String,
}

fn quote(
    conn: &Connection,
    audiobook: &str,
    book: &str,
    scope: &ScopeIn,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Quote, ApiError> {
    let chosen = resolve_scope(conn, book, scope)?;
    let (mut make, mut reused, mut chars) = (vec![], 0, 0i64);
    for ch in chosen {
        if has_audio(conn, audiobook, &ch)? {
            reused += 1;
        } else {
            chars += conn.query_row(
                "SELECT COALESCE(SUM(end-start),0) FROM lines WHERE chapter_id=?1",
                [&ch],
                |r| r.get::<_, i64>(0),
            )?;
            make.push(ch);
        }
    }
    let (per_unit, as_of, basis): (i64, String, String) = conn.query_row(
        "SELECT per_unit,as_of,basis FROM prices WHERE provider='gemini'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let per_unit = money::effective_gemini_price(per_unit, &as_of, &crate::clock::ts(now));
    Ok(Quote {
        chapter_ids: make,
        reused,
        chars,
        range: plans::estimate(chars, per_unit),
        per_unit,
        as_of,
        basis,
    })
}

fn remaining(
    conn: &Connection,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<i64>, ApiError> {
    let Some(monthly) = money::limits(conn)?.monthly else {
        return Ok(None);
    };
    let (s, e) = money::month_bounds(now);
    let used = money::spent_between(conn, &crate::clock::ts(s), &crate::clock::ts(e))?;
    Ok(Some(monthly - used.known - used.reserved))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewIn {
    scope: ScopeIn,
}

/// `previewPlan`: spends nothing; valid for 15 minutes.
pub async fn preview(
    State(state): State<AppState>,
    _device: DeviceCtx,
    _listener: ListenerCtx,
    Path(audiobook): Path<String>,
    ApiJson(input): ApiJson<PreviewIn>,
) -> Result<Json<Value>, ApiError> {
    let (id, now) = (state.new_id(), state.clock.now());
    let v = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let t = premium_target(&tx, &audiobook)?;
            let q = quote(&tx, &audiobook, &t.book_id, &input.scope, now)?;
            let suggested = plans::suggested_limit(q.range.high);
            let left = remaining(&tx, now)?;
            let blocked = match left {
                Some(l) if q.range.low > l => Some(detail("allowance_exceeded", "Even the low end of this estimate is more than is left of this month's Allowance.")),
                _ => None,
            };
            let expires = crate::clock::ts(now + Duration::minutes(EXPIRES_MINUTES));
            tx.execute(
                "INSERT INTO estimates(id,audiobook_id,scope,chapter_ids,chars,low,likely,high,per_unit,prices_as_of,basis,suggested,expires_at,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![id, audiobook, serde_json::to_string(&input.scope).expect("scope"), json!(q.chapter_ids).to_string(), q.chars, q.range.low, q.range.likely, q.range.high, q.per_unit, q.as_of, q.basis, suggested, expires, crate::clock::ts(now)],
            )?;
            let limits = money::limits(&tx)?;
            tx.commit()?;
            Ok(json!({
                "estimate_id": id,
                "expires_at": expires,
                "audiobook_id": audiobook,
                "scope": input.scope.value(),
                "text_characters": q.chars,
                "chapters_to_make": q.chapter_ids.len(),
                "chapters_reused": q.reused,
                "seconds_estimate": plans::seconds(q.chars),
                "cost": { "low": money::money(q.range.low), "likely": money::money(q.range.likely), "high": money::money(q.range.high), "prices_as_of": q.as_of, "basis": q.basis },
                "suggested_limit": money::money(if suggested == 0 { limits.default_plan } else { suggested }),
                "allowance": { "monthly_limit": limits.monthly.map(money::money), "remaining": left.map(money::money) },
                "blocked": blocked,
            }))
        })
        .await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalIn {
    estimate_id: String,
    limit: money::MoneyIn,
}

/// `createPlan`: the approval.
pub async fn create(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: ListenerCtx,
    headers: HeaderMap,
    ApiJson(input): ApiJson<ApprovalIn>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let limit = input.limit.micros("limit", 1)?;
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|k| !k.is_empty() && k.len() <= 200)
        .map(|k| format!("{}:{k}", listener.id));
    let (plan_id, job_id, audit_id, now) = (
        state.new_id(),
        state.new_id(),
        state.new_id(),
        state.clock.now(),
    );
    let actor = Actor::with_listener(&device, &listener);
    let out = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            if let Some(k) = &key {
                let existing: Option<String> = tx.query_row("SELECT id FROM plans WHERE idempotency_key=?1", [k], |r| r.get(0)).optional()?;
                if let Some(p) = existing {
                    return Ok((plan_value(&tx, &p)?, false));
                }
            }
            type Est = (String, String, i64, i64, i64, i64, String, String, String, Option<String>);
            let est: Est = tx
                .query_row(
                    "SELECT audiobook_id,chapter_ids,low,likely,high,per_unit,prices_as_of,basis,expires_at,used_by FROM estimates WHERE id=?1",
                    [&input.estimate_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
                )
                .optional()?
                .ok_or_else(|| ApiError::not_found("estimate_not_found", "No estimate has this id."))?;
            let (audiobook, chapters, low, likely, high, _per_unit, as_of, basis, expires, used_by) = est;
            if used_by.is_some() {
                return Err(ApiError::conflict("estimate_used", "This estimate was already approved. Preview again."));
            }
            if expires < crate::clock::ts(now) {
                return Err(ApiError::conflict("estimate_expired", "This estimate is more than 15 minutes old. Preview again."));
            }
            let t = premium_target(&tx, &audiobook)?;
            let chapter_ids: Vec<String> = serde_json::from_str(&chapters).unwrap_or_default();
            // What was shown must still be true: the same chapters to make, and prices inside the stated range.
            let mut still = vec![];
            for ch in &chapter_ids {
                if !has_audio(&tx, &audiobook, ch)? {
                    still.push(ch.clone());
                }
            }
            let scope_json: String = tx.query_row("SELECT scope FROM estimates WHERE id=?1", [&input.estimate_id], |r| r.get(0))?;
            let scope: ScopeIn = serde_json::from_str(&scope_json).map_err(ApiError::internal)?;
            let now_q = quote(&tx, &audiobook, &t.book_id, &scope, now)?;
            if now_q.chapter_ids != chapter_ids || now_q.range.likely < low || now_q.range.likely > high {
                return Err(ApiError::conflict("estimate_changed", "The chapters or the prices changed since this estimate. Preview again."));
            }
            let _ = still;
            if chapter_ids.is_empty() {
                return Err(ApiError::conflict("nothing_to_make", "Every chapter in this scope is already made."));
            }
            if limit < likely {
                return Err(ApiError::conflict("limit_below_estimate", "The limit is below the likely cost, so the plan could not finish."));
            }
            if let Some(left) = remaining(&tx, now)? {
                if low > left {
                    return Err(ApiError::conflict("allowance_exceeded", "This is more than is left of this month's Allowance."));
                }
            }
            let active: Option<String> = tx
                .query_row(&format!("SELECT p.id FROM plans p JOIN jobs j ON j.id=p.job_id WHERE p.audiobook_id=?1 AND j.state IN {ACTIVE} LIMIT 1"), [&audiobook], |r| r.get(0))
                .optional()?;
            if let Some(p) = active {
                let mut e = ApiError::conflict("plan_active", "This audiobook already has a plan in progress.");
                e.context = Some(json!({ "plan_id": p }));
                return Err(e);
            }
            let at = crate::clock::ts(now);
            tx.execute(
                "INSERT INTO jobs(id,kind,state,audiobook_id,book_id,plan_id,urgent,started_by,created_at,updated_at) VALUES(?1,'make_audio','queued',?2,?3,?4,0,?5,?6,?6)",
                params![job_id, audiobook, t.book_id, plan_id, serde_json::to_string(&actor).expect("actor"), at],
            )?;
            jobs::add_items(&tx, &job_id, &chapter_ids)?;
            jobs::recount(&tx, &job_id, &at)?;
            tx.execute(
                "INSERT INTO plans(id,audiobook_id,book_id,scope,est_low,est_likely,est_high,prices_as_of,basis,limit_micros,job_id,approved_by,idempotency_key,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?14)",
                params![plan_id, audiobook, t.book_id, scope_json, low, likely, high, as_of, basis, limit, job_id, serde_json::to_string(&actor).expect("actor"), key, at],
            )?;
            tx.execute("UPDATE estimates SET used_by=?2 WHERE id=?1", params![input.estimate_id, plan_id])?;
            audit::record(&tx, &audit_id, &at, "plan.approved", &actor, &json!({ "plan_id": plan_id, "audiobook_id": audiobook, "limit": limit, "estimate_id": input.estimate_id }))?;
            let v = plan_value(&tx, &plan_id)?;
            tx.commit()?;
            Ok((v, true))
        })
        .await?;
    if out.1 {
        announce(&state, out.0["id"].as_str().unwrap_or_default());
        jobs::announce(&state, out.0["job_id"].as_str().unwrap_or_default());
    }
    Ok((StatusCode::CREATED, Json(out.0)))
}

fn announce(state: &AppState, id: &str) {
    state.notify(Notice::new("plan.updated", state.now()).with_id(id.to_string()));
}

/// `getPlan`
pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.store.run(move |c| plan_value(c, &id)).await?))
}

#[derive(Deserialize)]
pub struct ListQuery {
    book_id: Option<String>,
    audiobook_id: Option<String>,
    state: Option<String>,
    limit: Option<u32>,
    after: Option<String>,
}

/// `listPlans`
pub async fn list(
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = super::audit::page_limit(q.limit)?;
    if let Some(s) = &q.state {
        if ![
            "approved",
            "running",
            "waiting",
            "paused",
            "needs_you",
            "completed",
            "stopped",
            "failed",
        ]
        .contains(&s.as_str())
        {
            return Err(ApiError::invalid(
                "invalid_request",
                "state is not a plan state.",
            ));
        }
    }
    let offset = match &q.after {
        None => 0usize,
        Some(a) => a
            .strip_prefix('o')
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| {
                ApiError::invalid(
                    "invalid_request",
                    "after is not a cursor this server issued.",
                )
            })?,
    };
    let v = state
        .store
        .run(move |c| {
            let ids: Vec<String> = c
                .prepare("SELECT id FROM plans WHERE (?1 IS NULL OR book_id=?1) AND (?2 IS NULL OR audiobook_id=?2) ORDER BY created_at DESC, id DESC")?
                .query_map(params![q.book_id, q.audiobook_id], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            let mut items = vec![];
            for id in ids {
                let p = plan_value(c, &id)?;
                if q.state.as_deref().is_none_or(|s| p["state"] == s) {
                    items.push(p);
                }
            }
            let next = if items.len() > offset + limit { json!(format!("o{}", offset + limit)) } else { Value::Null };
            let page: Vec<Value> = items.into_iter().skip(offset).take(limit).collect();
            Ok(json!({ "items": page, "next": next }))
        })
        .await?;
    Ok(Json(v))
}

fn job_of(conn: &Connection, plan: &str) -> Result<(String, String, i64, String), ApiError> {
    conn.query_row(
        "SELECT p.job_id,j.state,p.limit_micros,p.audiobook_id FROM plans p JOIN jobs j ON j.id=p.job_id WHERE p.id=?1",
        [plan],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .optional()?
    .ok_or_else(plan_not_found)
}

/// `pausePlan`: at the next chapter boundary. The chapter being made is finished and kept.
pub async fn pause(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: ListenerCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    change(state, device, listener, id, "pause", None).await
}

/// `stopPlan`: idempotent; keeps finished chapters.
pub async fn stop(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: ListenerCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    change(state, device, listener, id, "stop", None).await
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ResumeIn {
    new_limit: Option<money::MoneyIn>,
}

/// `resumePlan`: inside the current limit, or raise it with `new_limit` (an approval, audited).
pub async fn resume(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: ListenerCtx,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let input: ResumeIn = if body.iter().all(u8::is_ascii_whitespace) {
        ResumeIn::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            ApiError::invalid(
                "invalid_request",
                format!("The request body is not valid: {e}"),
            )
        })?
    };
    let new_limit = input
        .new_limit
        .as_ref()
        .map(|m| m.micros("new_limit", 1))
        .transpose()?;
    change(state, device, listener, id, "resume", new_limit).await
}

async fn change(
    state: AppState,
    device: DeviceCtx,
    listener: ListenerCtx,
    id: String,
    what: &'static str,
    new_limit: Option<i64>,
) -> Result<Json<Value>, ApiError> {
    let (audit_id, audit2, at, actor, now) = (
        state.new_id(),
        state.new_id(),
        state.now(),
        Actor::with_listener(&device, &listener),
        state.clock.now(),
    );
    let plan = id.clone();
    let out = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let (job, st, limit, audiobook) = job_of(&tx, &plan)?;
            let mut changed = true;
            match what {
                "pause" => {
                    if !["queued", "running", "waiting"].contains(&st.as_str()) {
                        return Err(ApiError::conflict("job_not_pausable", "Only a running or waiting plan can be paused."));
                    }
                    tx.execute("UPDATE jobs SET state='paused', waiting=NULL, wake_at=NULL, updated_at=?2 WHERE id=?1", params![job, at])?;
                }
                "stop" => {
                    if ACTIVE.contains(&format!("'{st}'")) {
                        tx.execute("UPDATE jobs SET state='stopped', waiting=NULL, needs_you=NULL, wake_at=NULL, current_chapter_id=NULL, updated_at=?2 WHERE id=?1", params![job, at])?;
                    } else {
                        changed = false;
                    }
                }
                _ => {
                    if !["paused", "needs_you"].contains(&st.as_str()) {
                        let code = if ["queued", "running", "waiting"].contains(&st.as_str()) { "job_running" } else { "plan_not_resumable" };
                        return Err(ApiError::conflict(code, "Only a paused plan, or one that needs you, can be resumed."));
                    }
                    let t = target(&tx, &audiobook)?;
                    let deleting: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM deletions WHERE book_id=?1 AND state='pending')",
                        [&t.book_id],
                        |r| r.get(0),
                    )?;
                    if deleting {
                        return Err(ApiError::conflict("deletion_pending", "This book is scheduled for deletion. Cancel the deletion before making more audio."));
                    }
                    if t.source_state == "key_rejected" {
                        return Err(ApiError::conflict("key_rejected", "The API key was rejected. Set a working key first."));
                    }
                    let effective = new_limit.unwrap_or(limit);
                    let used = spend::plan_used(&tx, &plan)?;
                    if effective <= used {
                        return Err(ApiError::conflict("limit_exceeded", "The plan has already reached its limit. Raise it with new_limit."));
                    }
                    if let Some(l) = new_limit {
                        tx.execute("UPDATE plans SET limit_micros=?2 WHERE id=?1", params![plan, l])?;
                        audit::record(&tx, &audit_id, &at, "plan.limit_raised", &actor, &json!({ "plan_id": plan, "from": limit, "to": l }))?;
                    }
                    if let Some(left) = remaining(&tx, now)? {
                        if left <= 0 {
                            return Err(ApiError::conflict("allowance_exceeded", "This month's Allowance is used up."));
                        }
                    }
                    tx.execute("UPDATE job_items SET state='queued', detail=NULL WHERE job_id=?1 AND state='failed'", [&job])?;
                    tx.execute("UPDATE jobs SET state='queued', waiting=NULL, needs_you=NULL, wake_at=NULL, updated_at=?2 WHERE id=?1", params![job, at])?;
                }
            }
            if changed {
                audit::record(&tx, &audit2, &at, &format!("plan.{what}"), &actor, &json!({ "plan_id": plan }))?;
                tx.execute("UPDATE plans SET updated_at=?2 WHERE id=?1", params![plan, at])?;
            }
            let v = plan_value(&tx, &plan)?;
            tx.commit()?;
            Ok((v, changed, job))
        })
        .await?;
    if out.1 {
        announce(&state, &id);
        jobs::announce(&state, &out.2);
    }
    Ok(Json(out.0))
}
