//! The durable Effect queue. Effects are committed with the state change that
//! asked for them; the effect runner performs them afterwards and records the
//! outcome here. A crash between performing an Effect and recording it means
//! it runs again, so adapters deduplicate by the ids Effects carry.

use crate::{SqliteStore, StoreError};
use rusqlite::{Connection, OptionalExtension, Row, params};
use solos_agent_core::model::{Effect, Ms};

/// Commit order: a larger id was committed later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EffectId(pub i64);

impl std::fmt::Display for EffectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueuedEffect {
    pub id: EffectId,
    pub effect: Effect,
    /// Failed attempts so far.
    pub attempts: u32,
    /// Not due before this time.
    pub due_ms: Ms,
    pub last_error: Option<String>,
    pub done: bool,
}

const COLUMNS: &str = "id, body, attempts, due_ms, error, done";

type Raw = (i64, String, u32, Ms, Option<String>, bool);

fn raw(row: &Row<'_>) -> rusqlite::Result<Raw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

fn parse((id, body, attempts, due_ms, last_error, done): Raw) -> Result<QueuedEffect, StoreError> {
    Ok(QueuedEffect {
        id: EffectId(id),
        effect: serde_json::from_str(&body)?,
        attempts,
        due_ms,
        last_error,
        done,
    })
}

/// Effects in one lane run in commit order. Sends share a lane per
/// conversation, so a reply never overtakes an earlier one.
fn lane(effect: &Effect) -> Result<Option<String>, StoreError> {
    match effect {
        Effect::Send(message) => Ok(Some(serde_json::to_string(&message.channel)?)),
        _ => Ok(None),
    }
}

pub(crate) fn append(tx: &Connection, effects: &[Effect]) -> Result<(), StoreError> {
    let mut put = tx.prepare_cached("INSERT INTO effects (lane, body) VALUES (?1, ?2)")?;
    for effect in effects {
        put.execute(params![lane(effect)?, serde_json::to_string(effect)?])?;
    }
    Ok(())
}

impl SqliteStore {
    /// Every Effect ever committed, done ones too, in commit order.
    pub fn effects(&self) -> Result<Vec<QueuedEffect>, StoreError> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("SELECT {COLUMNS} FROM effects ORDER BY id"))?;
        let rows = stmt.query_map([], raw)?;
        rows.map(|row| parse(row?)).collect()
    }

    pub fn effect(&self, id: EffectId) -> Result<Option<QueuedEffect>, StoreError> {
        let row = self
            .conn
            .prepare_cached(&format!("SELECT {COLUMNS} FROM effects WHERE id = ?1"))?
            .query_row([id.0], raw)
            .optional()?;
        row.map(parse).transpose()
    }

    /// Pending Effects due at `now_ms`, oldest first, at most `limit`. An
    /// Effect is held while an earlier one in its lane is pending, due or not,
    /// so a retry keeps its place.
    pub fn due_effects(&self, now_ms: Ms, limit: usize) -> Result<Vec<QueuedEffect>, StoreError> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM effects AS e
             WHERE e.done = 0 AND e.due_ms <= ?1
               AND NOT EXISTS (
                   SELECT 1 FROM effects AS p
                   WHERE p.done = 0 AND p.lane = e.lane AND p.id < e.id)
             ORDER BY e.id LIMIT ?2"
        ))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![now_ms, limit], raw)?;
        rows.map(|row| parse(row?)).collect()
    }

    /// Record that an Effect was performed. Returns whether this call changed
    /// it: a repeat finds it done and changes nothing.
    pub fn complete_effect(&self, id: EffectId) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .prepare_cached("UPDATE effects SET done = 1 WHERE id = ?1 AND done = 0")?
            .execute([id.0])?;
        self.changed(id, changed)
    }

    /// Record a failed attempt and when to try again. `attempts` is the count
    /// the runner saw when it listed the Effect, so a repeat of the same
    /// failure counts once. Returns whether this call changed it; a done
    /// Effect stays done.
    pub fn fail_effect(
        &self,
        id: EffectId,
        attempts: u32,
        error: &str,
        retry_at_ms: Ms,
    ) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE effects SET attempts = attempts + 1, error = ?3, due_ms = ?4
                 WHERE id = ?1 AND done = 0 AND attempts = ?2",
            )?
            .execute(params![id.0, attempts, error, retry_at_ms])?;
        self.changed(id, changed)
    }

    fn changed(&self, id: EffectId, rows: usize) -> Result<bool, StoreError> {
        if rows == 0 && self.effect(id)?.is_none() {
            return Err(StoreError::NoEffect(id));
        }
        Ok(rows > 0)
    }
}
