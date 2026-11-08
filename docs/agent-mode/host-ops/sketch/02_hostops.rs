//! Lives at: crates/hostops/ (new workspace member), binary `solos-hostops`,
//! installed root-owned at /usr/local/libexec/solos-hostops and run by
//! solos-hostops.service as `debian`, with NoNewPrivileges, no network, an
//! empty /home (ProtectHome=tmpfs) and only its roots bound back in
//! (BindPaths=). Its allowlist is /etc/solos-hostops/hostops.yaml, root-owned.
//!
//! Depends on solos-agent-core (the plan types and digest), serde,
//! serde_json, serde_yaml and rustix (openat2, renameat2, unlinkat, flock).
//! No async runtime and no HTTP.
//!
//! The helper trusts nothing the agent sends but a digest and names from its
//! own tables. It lists candidates itself, proves them itself, stores the plan
//! it built, and on apply runs its own stored copy.

use solos_agent_core::host::{
    HostAction, HostApplied, HostItem, HostPlanBody, HostReceipt, Pins, RelPath, RootName,
    SurveyId, TargetKind,
};
use solos_agent_core::model::{Ms, TicketId};
use solos_agent_core::trail::Digest;
use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// config.rs: /etc/solos-hostops/hostops.yaml
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub agent_uid: u32,
    #[serde(default = "thirty")]
    pub max_plan_ttl_mins: u32,
    pub roots: BTreeMap<String, RootConfig>,
    #[serde(default)]
    pub units: UnitsConfig,
    #[serde(default)]
    pub ops_files: Vec<PathBuf>,
    /// Added to BUILT_IN_DENY, never replacing it.
    #[serde(default)]
    pub deny: Vec<PathBuf>,
    #[serde(default = "ninety")]
    pub backups_keep_days: u32,
}
fn thirty() -> u32 {
    30
}
fn ninety() -> u32 {
    90
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootConfig {
    pub path: PathBuf,
    pub kinds: Vec<TargetKind>,
    pub idle_days: u32,
    #[serde(default = "six")]
    pub max_depth: u32,
}
fn six() -> u32 {
    6
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitsConfig {
    /// Exact names or `*` patterns, read-only.
    #[serde(default)]
    pub status: Vec<String>,
    /// Exact names only.
    #[serde(default)]
    pub restart: Vec<String>,
    #[serde(default = "yes")]
    pub require_no_new_privileges: bool,
    #[serde(default)]
    pub journal_lines: u32,
}
fn yes() -> bool {
    true
}

/// Relative to the helper's home. A root or ops file under any of these is
/// refused at start. ProtectHome=tmpfs already removes the home from the
/// helper's namespace, so this list is a second fence on the config side: a
/// mistyped root cannot point into a credential directory even if the unit
/// gains a wider BindPaths= later. System units are read from /etc/systemd
/// and /usr/lib/systemd, outside the home.
pub const BUILT_IN_DENY: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".pi",
    ".claude",
    ".claude.json",
    ".config/gh",
    ".modal.toml",
    ".aws",
    ".netrc",
    ".docker",
    ".npmrc",
    ".git-credentials",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".local/share/keyrings",
    "agent",
];

impl Config {
    /// Root-owned, mode 0640 root:debian, not group- or world-writable; each
    /// root and ops file absolute, without a symlink component, owned by this
    /// uid, not `/` or a home, not under a denied path; ops files not under
    /// any systemd directory.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        todo!()
    }
}

/// Refuse to serve unless: uid is neither 0 nor `agent_uid`, NoNewPrivs is
/// 1, /run/user is hidden, the binary is not writable by `agent_uid`,
/// openat2 works, every root and ops file is a BindPaths= entry in its own
/// unit, and the polkit rule names exactly the restart list. Each accepted
/// connection is dropped unless its SO_PEERCRED uid is `agent_uid`.
pub fn preflight(config: &Config) -> anyhow::Result<()> {
    todo!()
}

// ---------------------------------------------------------------------------
// protocol.rs: one JSON line each way, on the socket systemd passes in
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Hello,
    Survey {
        ticket: TicketId,
        kind: TargetKind,
        root: Option<RootName>,
        min_bytes: Option<u64>,
    },
    Units {
        units: Option<Vec<String>>,
    },
    Read {
        file: String,
    },
    Plan {
        ticket: TicketId,
        seq: u32,
        ttl_mins: u32,
        /// The requester's Trigger words, clipped; stored in the body.
        asked: String,
        delete: Vec<SurveyId>,
        restart: Vec<String>,
        edit: Option<EditRequest>,
        restore: Option<RestoreRequest>,
    },
    /// The helper runs its own stored body for `digest`. `approved_by` and
    /// `agent_head` are the agent's word, logged, not verified.
    Apply {
        digest: Digest,
        approved_by: String,
        agent_head: Digest,
    },
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditRequest {
    pub file: String,
    /// Each `old` must occur exactly once in the current content.
    pub replace: Vec<(String, String)>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    pub file: String,
    pub backup: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum Answer {
    Hello {
        helper: String,
        agent_uid: u32,
        version: u32,
        /// The helper user's home, so the agent can prove it cannot open it.
        home: String,
    },
    Survey { rows: Vec<SurveyRow>, partial: bool },
    Units { units: Vec<UnitStatus> },
    Read { file: String, sha256: Digest, content: String },
    Plan { body: HostPlanBody, digest: Digest },
    Applied(HostApplied),
    Refused { why: String },
}

/// Called for each connection, after SO_PEERCRED says `agent_uid`.
pub fn serve_one(config: &Config, state: &mut State, request: Request) -> Answer {
    todo!()
}

// ---------------------------------------------------------------------------
// beneath.rs: every path operation is fd-relative and never follows a link
// ---------------------------------------------------------------------------

/// A root opened once per request, checked against the (dev, ino) recorded
/// at start.
pub struct Root {
    pub name: RootName,
    pub fd: OwnedFd,
    pub dev: u64,
    pub ino: u64,
}

impl Root {
    /// openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS |
    /// RESOLVE_NO_XDEV), O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC.
    pub fn open_dir(&self, path: &RelPath) -> std::io::Result<OwnedFd> {
        todo!()
    }
    /// The entry's (dev, ino, mode, nlink) without following it.
    pub fn stat(&self, path: &RelPath) -> std::io::Result<rustix::fs::Stat> {
        todo!()
    }
    /// renameat2(RENAME_NOREPLACE) of `path` into `<root>/.solos-hostops-trash/<name>`,
    /// after checking the trash is on the root's device.
    pub fn to_trash(&self, path: &RelPath, name: &str) -> std::io::Result<()> {
        todo!()
    }
}

/// Delete a tree held by `dir`: openat(O_DIRECTORY | O_NOFOLLOW) per
/// subdirectory, unlinkat per entry, a directory on another device refused,
/// a read-only directory made writable first. Returns the blocks freed: the
/// entries unlinked whose link count was 1.
pub fn delete_tree(dir: OwnedFd, dev: u64) -> std::io::Result<u64> {
    todo!()
}

// ---------------------------------------------------------------------------
// survey.rs and proof.rs
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, serde::Serialize)]
pub struct SurveyRow {
    pub id: SurveyId,
    pub root: RootName,
    pub path: RelPath,
    pub target: TargetKind,
    pub bytes: u64,
    pub newest_ms: Ms,
    pub idle_days: u32,
    pub proof: Proof,
    #[serde(skip)]
    pub dev: u64,
    #[serde(skip)]
    pub ino: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Proof {
    pub idle: bool,
    /// Every `<profile>/.cargo-lock` taken with LOCK_EX | LOCK_NB.
    pub no_cargo_lock: bool,
    /// No process of this uid with cwd, root, exe, an fd or a map inside.
    pub no_process_inside: bool,
}

/// Walk the roots to their depth within 60 s; do not descend into a found
/// candidate; size by allocated blocks, links counted once.
pub fn survey(roots: &[Root], config: &Config, ticket: &TicketId, now: Ms) -> (Vec<SurveyRow>, bool) {
    todo!()
}

/// Holds the cargo locks it took until dropped, so apply keeps a build out
/// from the check to the rename.
pub struct Held {
    locks: Vec<OwnedFd>,
}

pub fn prove_unused(root: &Root, path: &RelPath, idle_days: u32, now: Ms) -> Result<(Proof, Held), String> {
    todo!()
}

// ---------------------------------------------------------------------------
// plan.rs and apply.rs
// ---------------------------------------------------------------------------

/// Survey rows by Ticket for 10 minutes, the plans this helper built, and
/// the receipts of plans it applied.
pub struct State {
    pub helper: String,
    pub surveys: BTreeMap<TicketId, (Ms, Vec<SurveyRow>)>,
    pub plans_dir: PathBuf,
    pub log: Log,
}

/// Resolve every name from the helper's own tables, prove each item again,
/// pin it, and store the body at plans/<digest>.json before answering.
pub fn plan(state: &mut State, config: &Config, request: Request, now: Ms) -> Result<(HostPlanBody, Digest), String> {
    todo!()
}

/// Load plans/<digest>.json; refuse an unknown digest and one past
/// expires_ms + 10 minutes; return the stored receipts of one already
/// applied. Otherwise log `applying`, then per item `asked`, act, `done`.
pub fn apply(state: &mut State, config: &Config, digest: &Digest, approved_by: &str, now: Ms) -> Result<HostApplied, String> {
    todo!()
}

fn apply_item(state: &mut State, config: &Config, item: &HostItem) -> HostReceipt {
    match (&item.action, &item.pins) {
        (HostAction::Delete { .. }, Pins::Delete { .. }) => todo!(
            "open parent beneath root; stat; dev+ino; prove_unused; newest <= pin; \
             to_trash; delete_tree; drop Held"
        ),
        (HostAction::Restart { .. }, Pins::Restart { .. }) => todo!(
            "recompute every pin; /usr/bin/systemctl --no-ask-password restart -- <unit> over the \
             system bus, 60 s, which the polkit rule allows for that exact name"
        ),
        (HostAction::Edit { .. }, Pins::Edit { .. }) => todo!(
            "open beneath its directory; dev, ino, mode, nlink == 1, old digest; backup (0600, fsync); \
             temp O_CREAT|O_EXCL|O_NOFOLLOW; fchmod; fsync; renameat; fsync dir"
        ),
        (HostAction::Restore { .. }, Pins::Restore { .. }) => todo!("as Edit, from the backup"),
        _ => todo!("a mismatched pin is refused"),
    }
}

// ---------------------------------------------------------------------------
// units.rs
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, serde::Serialize)]
pub struct UnitStatus {
    pub unit: String,
    /// ActiveState, SubState, Result, NRestarts, ExecMainStartTimestamp,
    /// ExecMainExitTimestamp, ExecMainStatus, MainPID, MemoryCurrent,
    /// Description, ExecStart; scrubbed.
    pub properties: BTreeMap<String, String>,
    /// Only where journal_lines > 0; scrubbed.
    pub journal: Vec<String>,
}

/// `systemctl show --property=... -- <unit>` over the system bus, for names
/// the status list admits. The argv is fixed; the name is matched, never
/// interpolated. Reading needs no privilege.
pub fn status(config: &Config, units: Option<&[String]>) -> Vec<UnitStatus> {
    todo!()
}

/// Exact restart-list name; a system unit with User=debian; loaded; not
/// solos-agent* or solos-hostops*; fragment, drop-ins, EnvironmentFile and
/// every Exec* file owned by a uid this helper does not write as and not
/// writable by agent_uid; NoNewPrivileges=yes; /run/user hidden.
pub fn restart_pins(config: &Config, unit: &str) -> Result<Pins, String> {
    todo!()
}

// ---------------------------------------------------------------------------
// files.rs and scrub.rs
// ---------------------------------------------------------------------------

/// An exact ops-file path: regular, at most 256 KiB, UTF-8, one link, no
/// secret-shaped string.
pub fn read_ops_file(config: &Config, file: &str) -> Result<(String, Digest), String> {
    todo!()
}

/// Private key blocks, ghp_, github_pat_, gho_, sk-, AKIA, xoxb-, xoxp-, and
/// long base64 or hex runs beside key=, token=, secret= or password=.
pub fn secret_shaped(text: &str) -> bool {
    todo!()
}

pub fn scrub(text: &str) -> String {
    todo!()
}

// ---------------------------------------------------------------------------
// log.rs: /var/lib/solos-hostops/log.jsonl, hash-chained
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Started { helper: String, config_sha256: Digest },
    Surveyed { ticket: TicketId, rows: u32, bytes: u64, partial: bool },
    Planned { ticket: TicketId, digest: Digest, items: u16 },
    Applying { digest: Digest, approved_by: String },
    Asked { digest: Digest, n: u16 },
    Done { digest: Digest, receipt: HostReceipt },
    Applied { digest: Digest, freed_bytes: u64, free_before: u64, free_after: u64 },
}

pub struct Log {
    pub path: PathBuf,
    pub head: Digest,
}

impl Log {
    /// Appends `{at_ms, prev, record}` and fsyncs before returning.
    pub fn append(&mut self, at_ms: Ms, record: Record) -> std::io::Result<()> {
        todo!()
    }
    /// `solos-hostops log verify`.
    pub fn verify(path: &std::path::Path) -> anyhow::Result<u64> {
        todo!()
    }
}

// The cross-check against agent.db is `solos-swarm agent host-check` in the
// agent's binary (03_agent.rs), so this crate needs no SQLite.
