//! The spending ledger. A paid request is reserved before it is sent, at its
//! highest likely cost, and settled when it ends. Limits are checked against
//! what is known plus what is reserved, so a request that could pass a limit is
//! never sent. A request whose cost cannot be stated is recorded as unknown,
//! never as zero.

use crate::{api::money, error::ApiError};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

#[derive(Debug, PartialEq)]
pub enum Denied {
    /// The plan's own limit.
    Plan,
    /// The monthly Allowance.
    Allowance,
}

/// Known plus reserved money of a plan.
pub fn plan_used(conn: &Connection, plan_id: &str) -> Result<i64, ApiError> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(CASE WHEN status='known' THEN known_micros WHEN status='reserved' THEN reserved END),0) FROM spend WHERE plan_id=?1",
        [plan_id],
        |r| r.get(0),
    )?)
}

/// Would `amount` fit under the plan's limit and this month's Allowance?
pub fn check(
    conn: &Connection,
    plan: Option<(&str, i64)>,
    amount: i64,
    now: DateTime<Utc>,
) -> Result<Result<(), Denied>, ApiError> {
    if let Some((id, limit)) = plan {
        if plan_used(conn, id)? + amount > limit {
            return Ok(Err(Denied::Plan));
        }
    }
    if let Some(monthly) = money::limits(conn)?.monthly {
        let (s, e) = money::month_bounds(now);
        let used = money::spent_between(conn, &crate::clock::ts(s), &crate::clock::ts(e))?;
        if used.known + used.reserved + amount > monthly {
            return Ok(Err(Denied::Allowance));
        }
    }
    Ok(Ok(()))
}

pub struct Reservation<'a> {
    pub id: &'a str,
    pub plan: Option<(&'a str, i64)>,
    pub audiobook_id: &'a str,
    pub chapter_id: Option<&'a str>,
    pub amount: i64,
    pub now: DateTime<Utc>,
}

/// Check the limits and hold the money, atomically. Nothing is held if a limit would be passed.
pub fn reserve(conn: &mut Connection, r: &Reservation) -> Result<Result<(), Denied>, ApiError> {
    let tx = conn.transaction()?;
    if let Err(d) = check(&tx, r.plan, r.amount, r.now)? {
        return Ok(Err(d));
    }
    tx.execute(
        "INSERT INTO spend(id,plan_id,audiobook_id,chapter_id,at,status,reserved) VALUES(?1,?2,?3,?4,?5,'reserved',?6)",
        params![r.id, r.plan.map(|p| p.0), r.audiobook_id, r.chapter_id, crate::clock::ts(r.now), r.amount],
    )?;
    tx.commit()?;
    Ok(Ok(()))
}

/// How a reserved request ended.
pub enum Outcome {
    /// Billed, and the cost is known.
    Known {
        micros: i64,
        input: Option<i64>,
        output: Option<i64>,
    },
    /// Possibly billed, cost not stated.
    Unknown { note: &'static str },
    /// Not billed: the request was refused, rate-limited or never sent.
    Nothing,
}

pub fn settle(conn: &Connection, id: &str, outcome: &Outcome) -> Result<(), ApiError> {
    match outcome {
        Outcome::Known { micros, input, output } => conn.execute(
            "UPDATE spend SET status='known', known_micros=?2, input_tokens=?3, output_tokens=?4 WHERE id=?1",
            params![id, micros, input, output],
        )?,
        Outcome::Unknown { note } => conn.execute("UPDATE spend SET status='unknown', note=?2 WHERE id=?1", params![id, note])?,
        Outcome::Nothing => conn.execute("UPDATE spend SET status='none', reserved=0 WHERE id=?1", [id])?,
    };
    Ok(())
}

/// A request that was reserved when the server stopped may have been sent: it becomes unknown, never zero.
pub fn recover(conn: &Connection) -> Result<usize, ApiError> {
    Ok(conn.execute("UPDATE spend SET status='unknown', note='The server stopped while this request was in flight.' WHERE status='reserved'", [])?)
}
