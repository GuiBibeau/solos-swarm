use crate::StoreError;
use rusqlite::{Connection, TransactionBehavior};

pub(crate) const LATEST: u32 = 9;

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

/// Model spend per account. A closed row is final, and no row is ever
/// deleted, so a cap counts every call ever made against it.
const V2: &str = "
CREATE TABLE IF NOT EXISTS spend (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account TEXT NOT NULL,
    provider TEXT NOT NULL,
    at_ms INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('held', 'settled', 'released', 'unknown')),
    hold_microusd INTEGER NOT NULL,
    cost_microusd INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER,
    cached_input_tokens INTEGER,
    output_tokens INTEGER,
    request_id TEXT,
    closed_ms INTEGER
) STRICT;
CREATE INDEX IF NOT EXISTS spend_account ON spend (account, at_ms);
CREATE TRIGGER IF NOT EXISTS spend_closed_is_final BEFORE UPDATE ON spend
WHEN OLD.state != 'held'
BEGIN SELECT RAISE(ABORT, 'a closed hold cannot change'); END;
CREATE TRIGGER IF NOT EXISTS spend_no_delete BEFORE DELETE ON spend
BEGIN SELECT RAISE(ABORT, 'spend is never deleted'); END;
";

/// What the Watchdog told the operator, one row per alert id. Only the
/// Watchdog writes it: a row is recorded before its message goes out and
/// marked sent after, so a restart neither repeats an alert nor loses one.
const V3: &str = "
CREATE TABLE IF NOT EXISTS watchdog_alerts (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    subject TEXT NOT NULL,
    ticket TEXT,
    retry INTEGER NOT NULL DEFAULT 0,
    raised_ms INTEGER NOT NULL,
    message TEXT NOT NULL,
    sent_ms INTEGER
) STRICT;
CREATE INDEX IF NOT EXISTS watchdog_unsent ON watchdog_alerts (raised_ms) WHERE sent_ms IS NULL;
CREATE INDEX IF NOT EXISTS watchdog_retries ON watchdog_alerts (ticket) WHERE retry = 1;
";

/// A file that holds trail events is also one an older build refuses to open,
/// instead of failing on an event it cannot read.
const V4: &str = "
CREATE TABLE IF NOT EXISTS trails (
    assignment TEXT PRIMARY KEY NOT NULL,
    ticket TEXT NOT NULL,
    body TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS trails_ticket ON trails (ticket);
";

/// No new table. A file that can hold secret grants, `InputBody::Secret` and
/// the secret events is one an older build refuses to open, instead of
/// failing on an Input or an event it cannot read, or dropping
/// `Ticket.secrets` when it rewrites a Ticket.
const V5: &str = "";

/// No new table. A file whose Tickets can name a repository and a method is
/// one an older build refuses to open, instead of dropping `Mandate.repo`
/// and `Plan::Session.method` when it rewrites a Ticket.
const V6: &str = "";

/// No new table. A file whose Tickets can hold a `Change`, whose Mandates can
/// carry `ForgeTerms`, and whose Effects and events can be a forge job, a
/// proposal, a delivery or a withheld change, is one an older build refuses
/// to open, instead of dropping `Ticket.change` and `Mandate.forge` when it
/// rewrites a Ticket, or failing on an Effect or an event it cannot read.
const V7: &str = "";

/// Memos, one row each, so writers never overwrite each other's. The text is
/// in `body`; the events hold only its digest. A file that holds memos, or
/// memo events, is one an older build refuses to open, instead of failing on
/// an event it cannot read.
const V8: &str = "
CREATE TABLE IF NOT EXISTS memos (
    id TEXT PRIMARY KEY NOT NULL,
    scope TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('proposed', 'active', 'retired', 'forgotten')),
    body TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS memos_live ON memos (scope, id) WHERE state IN ('proposed', 'active');
";

/// No new table. A file whose memos can supersede one another, cite a line
/// of a transcript, be retired by the dream with what it cites, or name the
/// memo that replaced them, and whose events can say what a proposal
/// supersedes, is one an older build refuses to open, instead of dropping
/// `Memo.supersedes` and a retirement's `replaced_by` and `evidence` when it
/// rewrites a memo, or failing on a source or an actor it cannot read. A
/// version 8 memo reads as one that supersedes nothing.
const V9: &str = "";

/// Migration `n` is `MIGRATIONS[n - 1]`.
const MIGRATIONS: [&str; LATEST as usize] = [V1, V2, V3, V4, V5, V6, V7, V8, V9];

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
    use super::{LATEST, V1, V2, V3, V4, V5, V6, V7, V8, V9, migrate, version};
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
        assert_eq!(rows, LATEST);
        // Each script itself is idempotent, not only gated by the version.
        conn.execute_batch(V1).unwrap();
        conn.execute_batch(V2).unwrap();
        conn.execute_batch(V3).unwrap();
        conn.execute_batch(V4).unwrap();
        conn.execute_batch(V5).unwrap();
        conn.execute_batch(V6).unwrap();
        conn.execute_batch(V7).unwrap();
        conn.execute_batch(V8).unwrap();
        conn.execute_batch(V9).unwrap();
    }

    #[test]
    fn a_v1_file_gains_the_later_tables_and_keeps_its_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY NOT NULL) STRICT;",
        )
        .unwrap();
        conn.execute_batch(V1).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (1)", [])
            .unwrap();
        conn.execute("INSERT INTO triggers (id, body) VALUES ('t1', '{}')", [])
            .unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(version(&conn).unwrap(), LATEST);
        let kept: String = conn
            .query_row("SELECT body FROM triggers WHERE id = 't1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kept, "{}");
        let spend: u32 = conn
            .query_row("SELECT COUNT(*) FROM spend", [], |row| row.get(0))
            .unwrap();
        assert_eq!(spend, 0);
        let alerts: u32 = conn
            .query_row("SELECT COUNT(*) FROM watchdog_alerts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(alerts, 0);
        let trails: u32 = conn
            .query_row("SELECT COUNT(*) FROM trails", [], |row| row.get(0))
            .unwrap();
        assert_eq!(trails, 0);
    }

    #[test]
    fn a_v3_file_gains_trails_and_keeps_its_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY NOT NULL) STRICT;",
        )
        .unwrap();
        for (version, sql) in [(1, V1), (2, V2), (3, V3)] {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                [version],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO tickets (id, open, body) VALUES ('tk1', 1, '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO watchdog_alerts (id, kind, subject, raised_ms, message)
             VALUES ('wd1', 'stalled', 'tk1', 5, 'stalled')",
            [],
        )
        .unwrap();
        assert!(
            conn.prepare("SELECT 1 FROM trails").is_err(),
            "a V3 file has no trails table"
        );

        migrate(&mut conn).unwrap();

        assert_eq!(version(&conn).unwrap(), LATEST);
        let table = |sql: &str| -> String { conn.query_row(sql, [], |row| row.get(0)).unwrap() };
        assert_eq!(table("SELECT body FROM tickets WHERE id = 'tk1'"), "{}");
        assert_eq!(table("SELECT id FROM watchdog_alerts"), "wd1");
        let trails: u32 = conn
            .query_row("SELECT COUNT(*) FROM trails", [], |row| row.get(0))
            .unwrap();
        assert_eq!(trails, 0);
    }

    #[test]
    fn a_v4_file_opens_at_the_latest_version_and_keeps_its_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY NOT NULL) STRICT;",
        )
        .unwrap();
        for (version, sql) in [(1, V1), (2, V2), (3, V3), (4, V4)] {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                [version],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO tickets (id, open, body) VALUES ('tk1', 1, '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trails (assignment, ticket, body) VALUES ('as1', 'tk1', '{}')",
            [],
        )
        .unwrap();

        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();

        assert_eq!(version(&conn).unwrap(), LATEST);
        let rows: u32 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, LATEST, "one row per migration");
        let table = |sql: &str| -> String { conn.query_row(sql, [], |row| row.get(0)).unwrap() };
        assert_eq!(table("SELECT body FROM tickets WHERE id = 'tk1'"), "{}");
        assert_eq!(table("SELECT ticket FROM trails"), "tk1");
    }

    #[test]
    fn a_v7_file_gains_memos_and_keeps_its_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY NOT NULL) STRICT;",
        )
        .unwrap();
        for (version, sql) in (1..).zip([V1, V2, V3, V4, V5, V6, V7]) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                [version],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO tickets (id, open, body) VALUES ('tk1', 1, '{}')",
            [],
        )
        .unwrap();
        assert!(
            conn.prepare("SELECT 1 FROM memos").is_err(),
            "a V7 file has no memos table"
        );

        migrate(&mut conn).unwrap();

        assert_eq!(version(&conn).unwrap(), LATEST);
        let kept: String = conn
            .query_row("SELECT body FROM tickets WHERE id = 'tk1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kept, "{}");
        let memos: u32 = conn
            .query_row("SELECT COUNT(*) FROM memos", [], |row| row.get(0))
            .unwrap();
        assert_eq!(memos, 0);
        let odd = conn.execute(
            "INSERT INTO memos (id, scope, state, body) VALUES ('mm1', 'global', 'kept', '{}')",
            [],
        );
        assert!(odd.is_err(), "a state outside the four is refused");
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
