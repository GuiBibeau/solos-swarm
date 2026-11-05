//! Model spend per account: a hold before each call, closed after it by a
//! settlement, a release or a charge at the hold. The rows live in the
//! Store's file, so a restart cannot forget spend and reopen a cap, and a
//! closed row can never change or go away.

use crate::{SqliteStore, StoreError};
use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};
use solos_agent_core::model::{MicroUsd, Ms};

/// A hold, by row id. A larger id was reserved later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HoldId(pub i64);

impl std::fmt::Display for HoldId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One model call to hold for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldRequest<'a> {
    /// The account that pays, such as `router` or `ticket:tk_...`.
    pub account: &'a str,
    pub provider: &'a str,
    /// Spend from this time on counts against `cap_microusd`.
    pub since_ms: Ms,
    /// What the account may commit since `since_ms`. `None`: no cap, so the
    /// call is held and recorded but never refused for spend.
    pub cap_microusd: Option<MicroUsd>,
    pub hold_microusd: MicroUsd,
    pub at_ms: Ms,
}

/// A cost paid outside this ledger's holds, such as a whole swarm run's,
/// whose own broker held and settled each of its calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Charge<'a> {
    pub account: &'a str,
    pub provider: &'a str,
    pub cost_microusd: MicroUsd,
    /// What was paid for. An account is charged once per id.
    pub request_id: &'a str,
    pub at_ms: Ms,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reservation {
    Held(HoldId),
    /// The hold would take the account past its cap, which only an account
    /// with one can do. Nothing was written.
    Refused {
        committed: MicroUsd,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpendState {
    /// Reserved; counts at its hold until it closes.
    Held,
    /// Settled at the cost the provider's receipt reported.
    Settled,
    /// The call never reached the provider, or failed before it was billed.
    Released,
    /// The call's cost is unknown, so it is charged at its hold.
    Unknown,
}

impl SpendState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Settled => "settled",
            Self::Released => "released",
            Self::Unknown => "unknown",
        }
    }
    fn parse(text: &str) -> rusqlite::Result<Self> {
        Ok(match text {
            "held" => Self::Held,
            "settled" => Self::Settled,
            "released" => Self::Released,
            "unknown" => Self::Unknown,
            other => {
                return Err(rusqlite::Error::InvalidColumnType(
                    0,
                    format!("spend state {other}"),
                    rusqlite::types::Type::Text,
                ));
            }
        })
    }
}

/// What a settled call used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spend {
    pub id: HoldId,
    pub account: String,
    pub provider: String,
    pub at_ms: Ms,
    pub state: SpendState,
    pub hold_microusd: MicroUsd,
    /// What the account is charged once the row is closed.
    pub cost_microusd: MicroUsd,
    pub tokens: Option<Tokens>,
    pub request_id: Option<String>,
    pub closed_ms: Option<Ms>,
}

const COLUMNS: &str = "id, account, provider, at_ms, state, hold_microusd, cost_microusd, \
                       input_tokens, cached_input_tokens, output_tokens, request_id, closed_ms";

fn spend(row: &Row<'_>) -> rusqlite::Result<Spend> {
    let input: Option<u64> = row.get(7)?;
    let cached: Option<u64> = row.get(8)?;
    let output: Option<u64> = row.get(9)?;
    let tokens = match (input, cached, output) {
        (Some(input), Some(cached_input), Some(output)) => Some(Tokens {
            input,
            cached_input,
            output,
        }),
        _ => None,
    };
    Ok(Spend {
        id: HoldId(row.get(0)?),
        account: row.get(1)?,
        provider: row.get(2)?,
        at_ms: row.get(3)?,
        state: SpendState::parse(&row.get::<_, String>(4)?)?,
        hold_microusd: row.get(5)?,
        cost_microusd: row.get(6)?,
        tokens,
        request_id: row.get(10)?,
        closed_ms: row.get(11)?,
    })
}

/// An open row counts at its hold, a closed one at its cost.
const COMMITTED: &str = "SELECT COALESCE(SUM(CASE state WHEN 'held' THEN hold_microusd \
                         ELSE cost_microusd END), 0) FROM spend WHERE account = ?1 AND at_ms >= ?2";

impl SqliteStore {
    /// Hold for one call if what the account committed since
    /// `request.since_ms`, plus this hold, stays within its cap (an account
    /// with none always gets its hold). The check and the hold are one
    /// transaction, so two calls cannot both take the last of a cap.
    pub fn reserve(&mut self, request: &HoldRequest<'_>) -> Result<Reservation, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let committed: MicroUsd = tx.query_row(
            COMMITTED,
            params![request.account, request.since_ms],
            |row| row.get(0),
        )?;
        if let Some(cap) = request.cap_microusd
            && committed.saturating_add(request.hold_microusd) > cap
        {
            return Ok(Reservation::Refused { committed });
        }
        tx.execute(
            "INSERT INTO spend (account, provider, at_ms, state, hold_microusd)
             VALUES (?1, ?2, ?3, 'held', ?4)",
            params![
                request.account,
                request.provider,
                request.at_ms,
                request.hold_microusd
            ],
        )?;
        let id = HoldId(tx.last_insert_rowid());
        tx.commit()?;
        Ok(Reservation::Held(id))
    }

    /// Close a hold at the cost its receipt reported. A cost above the hold
    /// is kept as reported: the provider billed it.
    pub fn settle(
        &mut self,
        id: HoldId,
        cost_microusd: MicroUsd,
        tokens: Tokens,
        request_id: &str,
        at_ms: Ms,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE spend SET state = 'settled', cost_microusd = ?2, input_tokens = ?3,
                 cached_input_tokens = ?4, output_tokens = ?5, request_id = ?6, closed_ms = ?7
             WHERE id = ?1 AND state = 'held'",
            params![
                id.0,
                cost_microusd,
                tokens.input,
                tokens.cached_input,
                tokens.output,
                request_id,
                at_ms
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotHeld(id));
        }
        Ok(())
    }

    /// Close a hold at zero: the call never reached the provider, or failed
    /// before it was billed.
    pub fn release(&mut self, id: HoldId, at_ms: Ms) -> Result<(), StoreError> {
        self.close(id, SpendState::Released, at_ms)
    }

    /// Close a hold at its hold: the provider answered, but its receipt could
    /// not be read, so what it billed is unknown.
    pub fn charge_hold(&mut self, id: HoldId, at_ms: Ms) -> Result<(), StoreError> {
        self.close(id, SpendState::Unknown, at_ms)
    }

    fn close(&mut self, id: HoldId, state: SpendState, at_ms: Ms) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE spend SET state = ?2, closed_ms = ?3,
                 cost_microusd = CASE ?2 WHEN 'unknown' THEN hold_microusd ELSE 0 END
             WHERE id = ?1 AND state = 'held'",
            params![id.0, state.as_str(), at_ms],
        )?;
        if changed == 0 {
            return Err(StoreError::NotHeld(id));
        }
        Ok(())
    }

    /// Record `charge` as a settled row, unless its account has a row with
    /// its request id already. Returns whether it was recorded, so a repeat
    /// after a crash charges nothing twice.
    pub fn charge(&mut self, charge: &Charge<'_>) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seen: Option<i64> = tx
            .query_row(
                "SELECT id FROM spend WHERE account = ?1 AND request_id = ?2",
                params![charge.account, charge.request_id],
                |row| row.get(0),
            )
            .optional()?;
        if seen.is_some() {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO spend (account, provider, at_ms, state, hold_microusd, cost_microusd,
                 request_id, closed_ms)
             VALUES (?1, ?2, ?3, 'settled', ?4, ?4, ?5, ?3)",
            params![
                charge.account,
                charge.provider,
                charge.at_ms,
                charge.cost_microusd,
                charge.request_id
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Charge every hold still open at its hold, and return how many there
    /// were. Run at start: a process that died during a call may have been
    /// billed for it.
    pub fn close_open_holds(&mut self, at_ms: Ms) -> Result<usize, StoreError> {
        Ok(self.conn.execute(
            "UPDATE spend SET state = 'unknown', cost_microusd = hold_microusd, closed_ms = ?1
             WHERE state = 'held'",
            [at_ms],
        )?)
    }

    /// What the account committed since `since_ms`: open holds at their hold,
    /// closed rows at their cost.
    pub fn committed(&self, account: &str, since_ms: Ms) -> Result<MicroUsd, StoreError> {
        Ok(self
            .conn
            .prepare_cached(COMMITTED)?
            .query_row(params![account, since_ms], |row| row.get(0))?)
    }

    /// Every row of one account, oldest first.
    pub fn spend(&self, account: &str) -> Result<Vec<Spend>, StoreError> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM spend WHERE account = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map([account], spend)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The rows of one account made at or after `since_ms`, oldest first.
    pub fn spend_since(&self, account: &str, since_ms: Ms) -> Result<Vec<Spend>, StoreError> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM spend WHERE account = ?1 AND at_ms >= ?2 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![account, since_ms], spend)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn hold(&self, id: HoldId) -> Result<Option<Spend>, StoreError> {
        Ok(self
            .conn
            .prepare_cached(&format!("SELECT {COLUMNS} FROM spend WHERE id = ?1"))?
            .query_row([id.0], spend)
            .optional()?)
    }
}
