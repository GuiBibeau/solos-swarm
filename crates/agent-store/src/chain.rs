//! The append-only event log, hash-chained like swarm mode's `events.jsonl`
//! (`Store::record` in `src/broker.rs`): each row keeps `prev_hash` and
//! `hash = sha256(prev_hash + event JSON)`, starting from 64 zeros.

use crate::{SqliteStore, StoreError};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use solos_agent_core::model::Event;

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// A verified log: how many events it holds and the hash of the last one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chain {
    pub len: u64,
    pub head: String,
}

fn digest(prev_hash: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(body.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub(crate) fn append(tx: &Connection, events: &[Event]) -> Result<(), StoreError> {
    if events.is_empty() {
        return Ok(());
    }
    let mut prev = tx
        .query_row(
            "SELECT hash FROM events ORDER BY seq DESC LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_else(|| GENESIS.to_owned());
    let mut put =
        tx.prepare_cached("INSERT INTO events (body, prev_hash, hash) VALUES (?1, ?2, ?3)")?;
    for event in events {
        let body = serde_json::to_string(event)?;
        let hash = digest(&prev, &body);
        put.execute(params![body, prev, hash])?;
        prev = hash;
    }
    Ok(())
}

impl SqliteStore {
    pub fn events(&self) -> Result<Vec<Event>, StoreError> {
        self.bodies("SELECT body FROM events ORDER BY seq")
    }

    /// Recompute the chain from the first event. The first row that is out
    /// of sequence, does not link to its predecessor, or does not hash to its
    /// stored hash is the error.
    pub fn verify_chain(&self) -> Result<Chain, StoreError> {
        self.walk_chain(|_| Ok(()))
    }

    /// [`Self::verify_chain`] and the events it verified, from one read, so
    /// an event appended meanwhile is in neither.
    pub fn verified_events(&self) -> Result<(Chain, Vec<Event>), StoreError> {
        let mut events = Vec::new();
        let chain = self.walk_chain(|body| {
            events.push(serde_json::from_str(body)?);
            Ok(())
        })?;
        Ok((chain, events))
    }

    fn walk_chain(
        &self,
        mut each: impl FnMut(&str) -> Result<(), StoreError>,
    ) -> Result<Chain, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, body, prev_hash, hash FROM events ORDER BY seq")?;
        let mut rows = stmt.query([])?;
        let mut chain = Chain {
            len: 0,
            head: GENESIS.to_owned(),
        };
        while let Some(row) = rows.next()? {
            let seq: u64 = row.get(0)?;
            let body: String = row.get(1)?;
            let prev_hash: String = row.get(2)?;
            let hash: String = row.get(3)?;
            let broken = |reason| StoreError::Chain { seq, reason };
            if seq != chain.len + 1 {
                return Err(broken("is out of sequence"));
            }
            if prev_hash != chain.head {
                return Err(broken("breaks the hash chain"));
            }
            if digest(&prev_hash, &body) != hash {
                return Err(broken("does not match its hash"));
            }
            each(&body)?;
            chain.len = seq;
            chain.head = hash;
        }
        Ok(chain)
    }
}
