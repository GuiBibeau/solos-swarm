use crate::StoreError;
use rusqlite::{Connection, TransactionBehavior};

pub(crate) const LATEST: u32 = 1;

/// Aggregates are JSON in `body`; the other columns index the port's queries.
const V1: &str = "
CREATE TABLE IF NOT EXISTS triggers (
    id TEXT PRIMARY KEY NOT NULL,
    body TEXT NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS threads (
    key TEXT PRIMARY KEY NOT NULL,
    active INTEGER NOT NULL,
    eval TEXT,
    body TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS threads_active ON threads (key) WHERE active = 1;
CREATE INDEX IF NOT EXISTS threads_eval ON threads (eval) WHERE eval IS NOT NULL;

CREATE TABLE IF NOT EXISTS tickets (
    id TEXT PRIMARY KEY NOT NULL,
    open INTEGER NOT NULL,
    body TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS tickets_open ON tickets (id) WHERE open = 1;

CREATE TABLE IF NOT EXISTS decisions (
    id TEXT PRIMARY KEY NOT NULL,
    body TEXT NOT NULL
) STRICT;

-- AUTOINCREMENT: an id the effect runner holds never names a later Effect,
-- even after old rows are pruned.
CREATE TABLE IF NOT EXISTS effects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    lane TEXT,
    body TEXT NOT NULL,
    done INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    due_ms INTEGER NOT NULL DEFAULT 0,
    error TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS effects_pending ON effects (id) WHERE done = 0;
CREATE INDEX IF NOT EXISTS effects_lane ON effects (lane, id) WHERE done = 0 AND lane IS NOT NULL;

CREATE TABLE IF NOT EXISTS events (
    seq INTEGER PRIMARY KEY,
    body TEXT NOT NULL,
    prev_hash TEXT NOT NULL,
    hash TEXT NOT NULL
) STRICT;
CREATE TRIGGER IF NOT EXISTS events_no_update BEFORE UPDATE ON events
BEGIN SELECT RAISE(ABORT, 'the event log is append-only'); END;
CREATE TRIGGER IF NOT EXISTS events_no_delete BEFORE DELETE ON events
BEGIN SELECT RAISE(ABORT, 'the event log is append-only'); END;
";

/// Migration `n` is `MIGRATIONS[n - 1]`.
const MIGRATIONS: [&str; LATEST as usize] = [V1];

/// Applies the migrations the file lacks. The write lock makes a second
/// process that opens the file at the same time wait, then find nothing to do.
pub(crate) fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY NOT NULL) STRICT;",
    )?;
    let found = version(&tx)?;
    if found > LATEST {
        return Err(StoreError::Schema {
            found,
            known: LATEST,
        });
    }
    for (version, sql) in (1..=LATEST).zip(MIGRATIONS).filter(|(v, _)| *v > found) {
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [version],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn version(conn: &Connection) -> Result<u32, StoreError> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::{LATEST, V1, migrate, version};
    use rusqlite::Connection;

    #[test]
    fn migrating_twice_changes_nothing() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(version(&conn).unwrap(), LATEST);
        let rows: u32 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        // The v1 script itself is idempotent, not only gated by the version.
        conn.execute_batch(V1).unwrap();
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [LATEST + 1],
        )
        .unwrap();
        let err = migrate(&mut conn).unwrap_err().to_string();
        assert!(err.contains("newer than this build"), "{err}");
    }
}
