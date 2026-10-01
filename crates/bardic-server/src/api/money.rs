//! Prices, the Allowance and spending. Money is integer micros of a currency; never a float.
//! Spending that cannot be stated is counted in `unknown_items`, never as zero.

use super::{audit, ApiJson};
use crate::{
    app::{Actor, AppState, DeviceCtx, ListenerCtx},
    error::ApiError,
    events::Notice,
};
use axum::{
    extract::{Path, State},
    Json,
};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

pub const CURRENCY: &str = "USD";
pub const PRICE_UNIT: &str = "million_characters";

pub fn money(micros: i64) -> Value {
    json!({ "micros": micros, "currency": CURRENCY })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoneyIn {
    micros: i64,
    currency: String,
}

impl MoneyIn {
    pub fn micros(&self, what: &str, min: i64) -> Result<i64, ApiError> {
        if self.currency != CURRENCY {
            return Err(ApiError::invalid(
                "invalid_request",
                format!("{what}: only {CURRENCY} is supported."),
            ));
        }
        if self.micros < min || self.micros > 1_000_000_000_000 {
            return Err(ApiError::invalid(
                "invalid_request",
                format!("{what} is out of range."),
            ));
        }
        Ok(self.micros)
    }
}

/// The calendar month (UTC) containing `now`.
pub fn month_bounds(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .unwrap();
    let (y, m) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    (start, Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).unwrap())
}

/// What has been spent this month: known money, and how many items could not be priced.
pub struct Spent {
    pub known: i64,
    pub unknown_items: i64,
    /// Money held back for requests not settled yet, and for unknown ones at their reserved amount;
    /// counts toward limits, not toward "spent".
    pub reserved: i64,
}

pub fn spent_between(conn: &Connection, start: &str, end: &str) -> Result<Spent, ApiError> {
    let (known, unknown, reserved): (i64, i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(CASE WHEN status='known' THEN known_micros END),0),
                COALESCE(SUM(status='unknown'),0),
                COALESCE(SUM(CASE WHEN status IN ('reserved','unknown') THEN reserved END),0)
         FROM spend WHERE at>=?1 AND at<?2",
        [start, end],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(Spent {
        known,
        unknown_items: unknown,
        reserved,
    })
}

pub struct Limits {
    pub monthly: Option<i64>,
    pub default_plan: i64,
}

pub fn limits(conn: &Connection) -> Result<Limits, ApiError> {
    Ok(conn.query_row(
        "SELECT monthly_limit,default_plan_limit FROM allowance WHERE id=1",
        [],
        |r| {
            Ok(Limits {
                monthly: r.get(0)?,
                default_plan: r.get(1)?,
            })
        },
    )?)
}

pub fn allowance_value(conn: &Connection, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let (start, end) = month_bounds(now);
    let (s, e) = (crate::clock::ts(start), crate::clock::ts(end));
    let l = limits(conn)?;
    let spent = spent_between(conn, &s, &e)?;
    Ok(json!({
        "monthly_limit": l.monthly.map(money),
        "default_plan_limit": money(l.default_plan),
        "period_start": s,
        "period_end": e,
        "spent": { "known": money(spent.known), "unknown_items": spent.unknown_items },
        "currency": CURRENCY,
    }))
}

/// `getAllowance`
pub async fn get_allowance(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let now = state.clock.now();
    Ok(Json(
        state.store.run(move |c| allowance_value(c, now)).await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowanceIn {
    monthly_limit: Option<MoneyIn>,
    default_plan_limit: MoneyIn,
}

/// `putAllowance`
pub async fn put_allowance(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: ListenerCtx,
    ApiJson(input): ApiJson<AllowanceIn>,
) -> Result<Json<Value>, ApiError> {
    let monthly = input
        .monthly_limit
        .as_ref()
        .map(|m| m.micros("monthly_limit", 0))
        .transpose()?;
    let default_plan = input.default_plan_limit.micros("default_plan_limit", 1)?;
    let (audit_id, at, actor, now) = (
        state.new_id(),
        state.now(),
        Actor::with_listener(&device, &listener),
        state.clock.now(),
    );
    let v = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "UPDATE allowance SET monthly_limit=?1, default_plan_limit=?2 WHERE id=1",
                params![monthly, default_plan],
            )?;
            audit::record(
                &tx,
                &audit_id,
                &at,
                "allowance.changed",
                &actor,
                &json!({ "monthly_limit": monthly, "default_plan_limit": default_plan }),
            )?;
            let v = allowance_value(&tx, now)?;
            tx.commit()?;
            Ok(v)
        })
        .await?;
    state.notify(Notice::new("allowance.updated", state.now()));
    state.jobs.poke(); // a lowered limit stops running plans at their next chapter boundary
    Ok(Json(v))
}

// ---------------------------------------------------------------- prices

fn price_value(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "provider": r.get::<_, String>(0)?,
        "unit": r.get::<_, String>(1)?,
        "per_unit": money(r.get(2)?),
        "as_of": r.get::<_, String>(3)?,
        "basis": r.get::<_, String>(4)?,
        "refresh_error": r.get::<_, Option<String>>(5)?,
    }))
}

const PRICE_COLS: &str = "provider,unit,per_unit,as_of,basis,refresh_error";

pub fn prices(conn: &Connection) -> Result<Vec<Value>, ApiError> {
    Ok(conn
        .prepare(&format!(
            "SELECT {PRICE_COLS} FROM prices ORDER BY provider"
        ))?
        .query_map([], price_value)?
        .collect::<Result<_, _>>()?)
}

/// `listPrices`
pub async fn list_prices(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({ "items": state.store.run(|c| prices(c)).await? }),
    ))
}

/// `refreshPrices`: no provider price interface is wired yet, so the manual table stays and says so.
pub async fn refresh_prices(
    State(state): State<AppState>,
    _device: DeviceCtx,
) -> Result<Json<Value>, ApiError> {
    let v = state
        .store
        .run(|c| {
            c.execute(
                "UPDATE prices SET refresh_error='No provider price interface is connected; this is the manual price table.' WHERE basis='manual'",
                [],
            )?;
            prices(c)
        })
        .await?;
    Ok(Json(json!({ "items": v })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceIn {
    unit: String,
    per_unit: MoneyIn,
}

/// `putPriceTable`
pub async fn put_price(
    State(state): State<AppState>,
    device: DeviceCtx,
    listener: ListenerCtx,
    Path(provider): Path<String>,
    ApiJson(input): ApiJson<PriceIn>,
) -> Result<Json<Value>, ApiError> {
    if input.unit != PRICE_UNIT {
        return Err(ApiError::invalid(
            "invalid_request",
            format!("unit must be {PRICE_UNIT}."),
        ));
    }
    let per_unit = input.per_unit.micros("per_unit", 1)?;
    let (audit_id, at, actor) = (
        state.new_id(),
        state.now(),
        Actor::with_listener(&device, &listener),
    );
    let v = state
        .store
        .run(move |c| {
            let tx = c.transaction()?;
            let exists = tx.query_row("SELECT 1 FROM prices WHERE provider=?1", [&provider], |_| Ok(())).optional()?.is_some();
            if !exists {
                return Err(ApiError::not_found("provider_not_found", "No provider with this name is priced here."));
            }
            tx.execute("UPDATE prices SET per_unit=?2, as_of=?3, basis='manual', refresh_error=NULL WHERE provider=?1", params![provider, per_unit, at])?;
            audit::record(&tx, &audit_id, &at, "prices.changed", &actor, &json!({ "provider": provider, "per_unit": per_unit }))?;
            let v = tx.query_row(&format!("SELECT {PRICE_COLS} FROM prices WHERE provider=?1"), [&provider], price_value)?;
            tx.commit()?;
            Ok(v)
        })
        .await?;
    Ok(Json(v))
}
