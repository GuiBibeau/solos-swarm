//! The SQLite Store for agent mode: agent-core's aggregates as JSON rows with
//! index columns for the port's queries, the durable Effect queue, and the
//! hash-chained event log, in one database file. Every
//! [`Repositories::commit`] writes state, Effects and events in one
//! transaction, or nothing.
//!
//! A connection that writes runs with `secure_delete`, so what a write
//! replaces or deletes is zeroed instead of left in free space, and a commit
//! that forgets a memo empties the WAL after it. Forgetting a memo then
//! leaves its text in neither the database file nor its WAL.

#![forbid(unsafe_code)]

mod chain;
mod error;
mod ledger;
mod queue;
mod schema;
mod watchdog;

pub use chain::Chain;
pub use error::StoreError;
pub use ledger::{Charge, HoldId, HoldRequest, Reservation, Spend, SpendState, Tokens};
pub use queue::{EffectId, QueuedEffect};
pub use watchdog::AlertRow;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use solos_agent_core::memo::{Memo, MemoState};
use solos_agent_core::model::{
    AssignmentId, ChangeSet, Decision, DecisionId, EvalId, MemoId, Ms, Thread, ThreadKey, Ticket,
    TicketId, Trigger, TriggerId,
};
use solos_agent_core::trail::TrailCursor;
use solos_agent_core::{Error, Repositories};
use std::cell::Cell;
use std::path::Path;
use std::time::Duration;

pub struct SqliteStore {
    conn: Connection,
    /// The first port read that failed since the last commit. Those reads
    /// cannot return an error, so the next commit does instead, and the
    /// command that saw the failed read commits nothing.
    read_error: Cell<Option<String>>,
    /// A forgotten memo's text may still be in the WAL, in a frame from
    /// before the forget: a reader held the WAL when it was to be emptied,
    /// so the next commit tries again.
    wal_holds_forgotten: bool,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::Journal(mode));
        }
        // The effect runner may act on an Effect as soon as its commit
        // returns, so the commit has to survive power loss too; WAL's usual
        // NORMAL can lose the latest commits.
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Self::start(conn)
    }

    /// Open an existing file to read only: no migration, no change of
    /// journal mode, and SQLite refuses every write. A file of a newer
    /// schema is refused, as by [`Self::open`].
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let found = schema::version(&conn)?;
        if found > schema::LATEST {
            return Err(StoreError::Schema {
                found,
                known: schema::LATEST,
            });
        }
        Ok(Self {
            conn,
            read_error: Cell::new(None),
            wal_holds_forgotten: false,
        })
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::start(Connection::open_in_memory()?)
    }

    fn start(mut conn: Connection) -> Result<Self, StoreError> {
        // Zero what a write frees. On the Store's own rows this costs some
        // CPU on rows that spill onto overflow pages (about 15% per commit
        // on 20 KB rows, nothing measurable on small ones) and no more pages.
        conn.pragma_update(None, "secure_delete", "ON")?;
        schema::migrate(&mut conn)?;
        Ok(Self {
            conn,
            read_error: Cell::new(None),
            wal_holds_forgotten: false,
        })
    }

    /// Copy the WAL into the database file and empty it, so no frame from
    /// before a forget is left. False while a reader still needs the WAL.
    fn empty_wal(&self) -> Result<bool, StoreError> {
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        Ok(busy == 0)
    }

    pub fn schema_version(&self) -> Result<u32, StoreError> {
        schema::version(&self.conn)
    }

    /// SQLite's `data_version`: a number that differs between two calls on
    /// this connection exactly when another connection committed in between.
    /// A reader that holds its connection open uses it to skip a read when
    /// nothing was written. It says nothing about this connection's own
    /// commits, which a read-only one has none of.
    pub fn data_version(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .pragma_query_value(None, "data_version", |row| row.get(0))?)
    }

    /// Every Trigger, by id.
    pub fn triggers(&self) -> Result<Vec<Trigger>, StoreError> {
        self.bodies("SELECT body FROM triggers ORDER BY id")
    }

    /// Every Thread, by key.
    pub fn threads(&self) -> Result<Vec<Thread>, StoreError> {
        self.bodies("SELECT body FROM threads ORDER BY key")
    }

    /// Every Ticket, by id.
    pub fn tickets(&self) -> Result<Vec<Ticket>, StoreError> {
        self.bodies("SELECT body FROM tickets ORDER BY id")
    }

    /// The dream's Tickets, by id: those whose Mandate names the Source
    /// `dream`, read without parsing every other Ticket.
    pub fn dream_tickets(&self) -> Result<Vec<Ticket>, StoreError> {
        let found: Vec<Ticket> = self.bodies(
            "SELECT body FROM tickets WHERE json_extract(body, '$.mandate.source') = 'dream' \
             ORDER BY id",
        )?;
        Ok(found.into_iter().filter(Ticket::is_dream).collect())
    }

    /// The Tickets that ended after `since_ms` and have not resumed since,
    /// by id.
    pub fn ended_since(&self, since_ms: Ms) -> Result<Vec<Ticket>, StoreError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT body FROM tickets WHERE open = 0 \
             AND json_extract(body, '$.ended.at_ms') > ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([since_ms], |row| row.get::<_, String>(0))?;
        rows.map(|body| Ok(serde_json::from_str(&body?)?)).collect()
    }

    /// Every Decision, by id.
    pub fn decisions(&self) -> Result<Vec<Decision>, StoreError> {
        self.bodies("SELECT body FROM decisions ORDER BY id")
    }

    /// Every Assignment's trail position, by Assignment.
    pub fn trails(&self) -> Result<Vec<TrailCursor>, StoreError> {
        self.bodies("SELECT body FROM trails ORDER BY assignment")
    }

    /// Every memo, in any state, by id.
    pub fn memos(&self) -> Result<Vec<Memo>, StoreError> {
        self.bodies("SELECT body FROM memos ORDER BY id")
    }

    /// One memo. Unlike the port's read, a failed read is an error here.
    pub fn read_memo(&self, id: &MemoId) -> Result<Option<Memo>, StoreError> {
        self.body("SELECT body FROM memos WHERE id = ?1", id.as_str())
    }

    /// Decisions recorded at or after `since_ms`, newest first, at most
    /// `limit`. Decisions are written once, in time order, so the read
    /// stops at the first older one instead of reading them all.
    pub fn recent_decisions(
        &self,
        since_ms: Ms,
        limit: usize,
    ) -> Result<Vec<Decision>, StoreError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT body FROM decisions ORDER BY rowid DESC")?;
        let mut rows = stmt.query([])?;
        let mut found = Vec::new();
        while found.len() < limit
            && let Some(row) = rows.next()?
        {
            let decision: Decision = serde_json::from_str(&row.get::<_, String>(0)?)?;
            if decision.at_ms < since_ms {
                break;
            }
            found.push(decision);
        }
        Ok(found)
    }

    /// One Trigger. Unlike the port's read, a failed read is an error here.
    pub fn read_trigger(&self, id: &TriggerId) -> Result<Option<Trigger>, StoreError> {
        self.body("SELECT body FROM triggers WHERE id = ?1", id.as_str())
    }

    pub fn read_thread(&self, key: &ThreadKey) -> Result<Option<Thread>, StoreError> {
        self.body("SELECT body FROM threads WHERE key = ?1", key.as_str())
    }

    pub fn read_ticket(&self, id: &TicketId) -> Result<Option<Ticket>, StoreError> {
        self.body("SELECT body FROM tickets WHERE id = ?1", id.as_str())
    }

    pub fn read_decision(&self, id: &DecisionId) -> Result<Option<Decision>, StoreError> {
        self.body("SELECT body FROM decisions WHERE id = ?1", id.as_str())
    }

    /// Tickets that have not ended, by id.
    pub fn open_tickets(&self) -> Result<Vec<TicketId>, StoreError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id FROM tickets WHERE open = 1 ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.map(|id| id.map(TicketId)).collect::<Result<_, _>>()?)
    }

    fn body<T: DeserializeOwned>(&self, sql: &str, key: &str) -> Result<Option<T>, StoreError> {
        let body: Option<String> = self
            .conn
            .prepare_cached(sql)?
            .query_row([key], |row| row.get(0))
            .optional()?;
        Ok(body.map(|body| serde_json::from_str(&body)).transpose()?)
    }

    fn bodies<T: DeserializeOwned>(&self, sql: &str) -> Result<Vec<T>, StoreError> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|body| Ok(serde_json::from_str(&body?)?)).collect()
    }

    fn thread_keys(&self, sql: &str, params: &[&str]) -> Result<Vec<ThreadKey>, StoreError> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| row.get(0))?;
        Ok(rows
            .map(|key| key.map(ThreadKey))
            .collect::<Result<_, _>>()?)
    }

    fn port<T: Default>(&self, read: Result<T, StoreError>) -> T {
        read.unwrap_or_else(|err| {
            let first = self.read_error.take().unwrap_or_else(|| err.to_string());
            self.read_error.set(Some(first));
            T::default()
        })
    }

    fn write(&mut self, changes: ChangeSet) -> Result<(), StoreError> {
        let forgets = changes
            .memos
            .values()
            .any(|memo| matches!(memo.state, MemoState::Forgotten { .. }));
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut put = tx.prepare_cached(
                "INSERT INTO triggers (id, body) VALUES (?1, ?2)
                 ON CONFLICT (id) DO UPDATE SET body = excluded.body",
            )?;
            for trigger in &changes.triggers {
                put.execute(params![
                    trigger.id.as_str(),
                    serde_json::to_string(trigger)?
                ])?;
            }
            let mut put = tx.prepare_cached(
                "INSERT INTO threads (key, active, eval, body) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (key) DO UPDATE
                 SET active = excluded.active, eval = excluded.eval, body = excluded.body",
            )?;
            for (key, thread) in &changes.threads {
                put.execute(params![
                    key.as_str(),
                    thread.is_active(),
                    thread.eval.as_ref().map(|eval| eval.id.as_str()),
                    serde_json::to_string(thread)?
                ])?;
            }
            let mut put = tx.prepare_cached(
                "INSERT INTO tickets (id, open, body) VALUES (?1, ?2, ?3)
                 ON CONFLICT (id) DO UPDATE SET open = excluded.open, body = excluded.body",
            )?;
            for (id, ticket) in &changes.tickets {
                put.execute(params![
                    id.as_str(),
                    ticket.ended.is_none(),
                    serde_json::to_string(ticket)?
                ])?;
            }
            let mut put = tx.prepare_cached(
                "INSERT INTO decisions (id, body) VALUES (?1, ?2)
                 ON CONFLICT (id) DO UPDATE SET body = excluded.body",
            )?;
            for decision in &changes.decisions {
                put.execute(params![
                    decision.id.as_str(),
                    serde_json::to_string(decision)?
                ])?;
            }
            let mut put = tx.prepare_cached(
                "INSERT INTO trails (assignment, ticket, body) VALUES (?1, ?2, ?3)
                 ON CONFLICT (assignment) DO UPDATE SET body = excluded.body",
            )?;
            for (assignment, cursor) in &changes.trails {
                put.execute(params![
                    assignment.as_str(),
                    cursor.ticket.as_str(),
                    serde_json::to_string(cursor)?
                ])?;
            }
            let mut put = tx.prepare_cached(
                "INSERT INTO memos (id, scope, state, body) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (id) DO UPDATE
                 SET scope = excluded.scope, state = excluded.state, body = excluded.body",
            )?;
            for (id, memo) in &changes.memos {
                put.execute(params![
                    id.as_str(),
                    memo.scope.to_string(),
                    memo.state.name(),
                    serde_json::to_string(memo)?
                ])?;
            }
            queue::append(&tx, &changes.effects)?;
            chain::append(&tx, &changes.events)?;
        }
        tx.commit()?;
        // The commit stands either way: a WAL that cannot be emptied now is
        // tried again after the next commit.
        if forgets || self.wal_holds_forgotten {
            self.wal_holds_forgotten = !self.empty_wal().unwrap_or(false);
        }
        Ok(())
    }
}

/// Upserts mirror [`solos_agent_core::MemoryRepositories`]: the last write of
/// an id wins.
impl Repositories for SqliteStore {
    fn trigger(&self, id: &TriggerId) -> Option<Trigger> {
        self.port(self.body("SELECT body FROM triggers WHERE id = ?1", id.as_str()))
    }
    fn thread(&self, key: &ThreadKey) -> Option<Thread> {
        self.port(self.body("SELECT body FROM threads WHERE key = ?1", key.as_str()))
    }
    fn ticket(&self, id: &TicketId) -> Option<Ticket> {
        self.port(self.body("SELECT body FROM tickets WHERE id = ?1", id.as_str()))
    }
    fn decision(&self, id: &DecisionId) -> Option<Decision> {
        self.port(self.body("SELECT body FROM decisions WHERE id = ?1", id.as_str()))
    }
    fn trail(&self, assignment: &AssignmentId) -> Option<TrailCursor> {
        self.port(self.body(
            "SELECT body FROM trails WHERE assignment = ?1",
            assignment.as_str(),
        ))
    }
    fn memo(&self, id: &MemoId) -> Option<Memo> {
        self.port(self.body("SELECT body FROM memos WHERE id = ?1", id.as_str()))
    }
    fn live_memos(&self) -> Vec<Memo> {
        self.port(
            self.bodies("SELECT body FROM memos WHERE state IN ('proposed', 'active') ORDER BY id"),
        )
    }
    fn active_threads(&self) -> Vec<ThreadKey> {
        self.port(self.thread_keys("SELECT key FROM threads WHERE active = 1 ORDER BY key", &[]))
    }
    fn thread_of_eval(&self, eval: &EvalId) -> Option<ThreadKey> {
        self.port(self.thread_keys(
            "SELECT key FROM threads WHERE eval = ?1 ORDER BY key LIMIT 1",
            &[eval.as_str()],
        ))
        .into_iter()
        .next()
    }
    fn commit(&mut self, changes: ChangeSet) -> Result<(), Error> {
        if let Some(why) = self.read_error.take() {
            return Err(Error::Store(format!(
                "a read failed, so nothing was committed: {why}"
            )));
        }
        self.write(changes).map_err(Error::from)
    }
}
