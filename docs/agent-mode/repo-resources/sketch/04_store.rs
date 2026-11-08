//! Lives at: crates/agent-store/src/schema.rs (modified).
//!
//! Sketch only: not compiled. The SQL is the part meant to be read; the
//! rest names the port.
//!
//! Two migrations, numbered 10 and 11 here, or the next free numbers when
//! they land. Each bumps `LATEST` (crates/agent-store/src/schema.rs:4), so an
//! older build refuses the file rather than ignoring rows it cannot read.
//! Neither is an Engine table: `agent serve`'s adapters write them, as the
//! broker writes `spend`.

/// One row per Ticket that asked for scratch. `agent serve` writes it;
/// `agent prune` marks `freed`.
pub const V10: &str = "
CREATE TABLE IF NOT EXISTS scratch (
    ticket TEXT PRIMARY KEY NOT NULL,
    repo TEXT NOT NULL,
    quota_bytes INTEGER NOT NULL,
    max_files INTEGER NOT NULL,
    used_bytes INTEGER NOT NULL DEFAULT 0,
    files INTEGER NOT NULL DEFAULT 0,
    peak_bytes INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL CHECK (state IN ('reserved', 'over', 'released', 'freed')),
    measured_ms INTEGER,
    created_ms INTEGER NOT NULL,
    released_ms INTEGER,
    freed_ms INTEGER
) STRICT;
CREATE INDEX IF NOT EXISTS scratch_open ON scratch (state) WHERE state IN ('reserved', 'over');
";

/// One row per connection, and in inspect mode one per request (`parent` is
/// its connection's id). Written when a connection opens, completed when it
/// closes, so a crash still leaves the attempt. Only the gate writes it.
pub const V11: &str = "
CREATE TABLE IF NOT EXISTS egress (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    parent INTEGER,
    ticket TEXT NOT NULL,
    assignment TEXT,
    repo TEXT NOT NULL,
    at_ms INTEGER NOT NULL,
    ended_ms INTEGER,
    host TEXT NOT NULL,
    port INTEGER NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('tunnel', 'inspect')),
    method TEXT,
    path TEXT,
    status INTEGER,
    verdict TEXT NOT NULL CHECK (verdict IN ('allowed', 'refused')),
    reason TEXT,
    bytes_up INTEGER NOT NULL DEFAULT 0,
    bytes_down INTEGER NOT NULL DEFAULT 0,
    cost_microusd INTEGER NOT NULL DEFAULT 0,
    keys TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS egress_ticket ON egress (ticket, at_ms);
CREATE INDEX IF NOT EXISTS egress_repo_day ON egress (repo, at_ms);
";

/// What the gate writes, through a Store port of its own.
pub trait EgressLog: Send + Sync {
    fn opened(&self, row: &EgressRow) -> anyhow::Result<i64>;
    fn closed(&self, id: i64, ended_ms: u64, bytes_up: u64, bytes_down: u64, cost: u64)
    -> anyhow::Result<()>;
    /// Bytes moved by `repo` since `since_ms`, for `daily_gb`.
    fn repo_bytes(&self, repo: &str, since_ms: u64) -> anyhow::Result<u64>;
}

pub struct EgressRow {
    pub parent: Option<i64>,
    pub ticket: String,
    pub assignment: Option<String>,
    pub repo: String,
    pub at_ms: u64,
    pub host: String,
    pub port: u16,
    pub mode: &'static str,
    pub method: Option<String>,
    /// Without its query, which can carry tokens.
    pub path: Option<String>,
    pub status: Option<u16>,
    pub verdict: &'static str,
    pub reason: Option<&'static str>,
    pub keys: Vec<String>,
}
