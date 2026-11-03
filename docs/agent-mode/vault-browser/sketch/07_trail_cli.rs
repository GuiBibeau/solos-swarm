//! Lives at: src/agent/trail.rs (the blob store the broker writes, and the
//! verifier and replay `agent trail` runs), plus the clap additions in
//! src/main.rs shown at the bottom.
//!
//! A trail is verified from three things, each checkable on its own:
//! 1. the chain: `SqliteStore::verify_chain` (unchanged hash format);
//! 2. the signatures: each `TrailDone { Signed }` under the key trusted for
//!    its `environment`, from config or `--trust`, never from the database;
//! 3. the blobs: each digest a signed step names, re-hashed from
//!    `<state>/trail/`.
//! and the pairing and sequence rules that tie them together.

use crate::browse::{Blob, TrailKey};
use anyhow::Result;
use solos_agent_core::model::{AssignmentId, TicketId};
use solos_agent_core::trail::Digest;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `trail:` in agent.yaml (optional, these defaults).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrailStoreConfig {
    /// The whole store; past it, browser steps are refused (signed, with a
    /// reason) until the operator archives. 8 GiB.
    #[serde(default = "default_max")]
    pub max_bytes: u64,
    /// One Ticket's share. 256 MiB.
    #[serde(default = "default_ticket_max")]
    pub ticket_max_bytes: u64,
}
fn default_max() -> u64 { 8 << 30 }
fn default_ticket_max() -> u64 { 256 << 20 }

/// Content-addressed files: `<state>/trail/<2 hex>/<64 hex>`, 0600 in a
/// 0700 directory, never under a Ticket's `work/` (which a Claude worker
/// reads and run_code writes). Written temp + fsync + rename, so a put is
/// idempotent and a crash leaves a whole blob or none. Plain files: the
/// same directory's agent.db and `<assignment>.rpc.jsonl` transcripts
/// already hold what the model saw in plaintext, so encrypting blobs alone
/// would protect nothing; what matters is that they hold no vault value.
pub struct BlobStore {
    root: PathBuf,
    limits: TrailStoreConfig,
    /// Bytes on disk, counted once at open (a directory walk) and added to
    /// by every new blob.
    used: std::sync::atomic::AtomicU64,
}

impl BlobStore {
    pub fn open(state_dir: &Path, limits: TrailStoreConfig) -> Result<Self> {
        todo!()
    }

    /// Re-hash `blob.bytes` (refuse a mismatch), skip if present, else
    /// write. Refused past either limit.
    pub fn put(&self, ticket: &TicketId, blob: &Blob) -> Result<()> {
        todo!()
    }

    pub fn get(&self, digest: &Digest) -> Result<Option<Vec<u8>>> {
        todo!()
    }
}

/// Which key verifies which environment's steps.
pub struct Trust(pub BTreeMap<String, TrailKey>);

impl Trust {
    /// From agent.yaml: `local` from `browser.audit_key`'s public half (read
    /// the seed, derive, drop), each node from `nodes.allow.*.audit_key`;
    /// then every `--trust <env>=ed25519:<hex>` replaces or adds. A verifier
    /// on another machine passes only `--trust` and needs no secret.
    pub fn from_config(config: Option<&Path>, overrides: &[(String, TrailKey)]) -> Result<Self> {
        todo!()
    }
}

/// Everything `agent trail` found.
pub struct TrailReport {
    pub chain_len: u64,
    pub chain_head: String,
    pub tickets: Vec<TicketTrail>,
    pub problems: Vec<Problem>,
}

pub struct TicketTrail {
    pub ticket: TicketId,
    pub title: String,
    pub assignments: Vec<AssignmentTrail>,
    /// SecretAsked / SecretAnswered / KeyUsed, in chain order.
    pub secrets: Vec<String>,
}

pub struct AssignmentTrail {
    pub assignment: AssignmentId,
    /// One line per n: time, environment, action, outcome, digests.
    pub steps: Vec<StepLine>,
}

pub struct StepLine;

/// Each is reported with its chain seq. Any one makes the exit status 1.
pub enum Problem {
    /// The chain itself (`StoreError::Chain`): nothing after it is trusted.
    Chain { seq: u64, reason: String },
    /// A TrailDone with no TrailAsked of that (assignment, n) before it, or a second Done.
    Unpaired { assignment: AssignmentId, n: u32 },
    /// Asked, never answered: the host stopped mid-step. Reported, not fatal
    /// on its own (`--strict` makes it fatal), since it is the truth.
    Unanswered { assignment: AssignmentId, n: u32 },
    /// The signed body's action differs from the asked one, or its
    /// ticket/assignment/n do.
    Mismatch { assignment: AssignmentId, n: u32, what: &'static str },
    NoTrustedKey { environment: String },
    BadSignature { assignment: AssignmentId, n: u32 },
    /// Within one session, session_step must be 0, 1, 2 … with no gap or
    /// repeat, and n must increase.
    SessionGap { session: String, expected: u32, found: u32 },
    /// A session with no Closed step, and no later evidence it ended
    /// (its Assignment's Close was Lost, or the host stopped).
    Unclosed { session: String },
    BlobMissing { digest: Digest },
    BlobCorrupt { digest: Digest },
}

/// Read-only: `SqliteStore::open_read_only`, so it is safe while the agent
/// serves.
///
/// ```text
/// chain = store.verify_chain()                 // Err -> Problem::Chain, stop
/// for event in store.events() (in seq order), keeping the ticket filter:
///   TrailAsked { a, n, action }  -> asked[(a, n)] = action          (second -> Unpaired)
///   TrailDone  { a, n, Lost }    -> pair it; line "lost: why"
///   TrailDone  { a, n, Signed(s) } ->
///       pair it with asked[(a, n)]                  else Unpaired
///       trail::answers(..)                          else Mismatch
///       key = trust[s.body.environment]             else NoTrustedKey
///       browse::verify_step(&s, key)                else BadSignature
///       sessions[s.body.session].push((s.body.session_step, n))
///       for digest in outcome digests: blobs.get -> re-hash   else BlobMissing / BlobCorrupt
///   SecretAsked / SecretAnswered / KeyUsed -> secrets lines
/// asked without done -> Unanswered
/// per session: steps sorted by n have session_step 0..k contiguous, last is Closed   else SessionGap / Unclosed
/// ```
pub fn verify(state_dir: &Path, trust: &Trust, ticket: Option<&TicketId>) -> Result<TrailReport> {
    todo!()
}

// ---------------------------------------------------------------------------
// src/main.rs: the commands
// ---------------------------------------------------------------------------

/// `solos-swarm vault …`. Works on agent.yaml or node.yaml alike: it reads
/// only the `vault:` key of `--config` (a struct with just that field and no
/// `deny_unknown_fields`), resolving paths against the file's directory. No
/// subcommand prints a value; `list` prints metadata.
#[derive(clap::Subcommand)]
pub enum VaultCommand {
    /// Write the key file (0600, refuses to replace one) and an empty vault.
    Init {
        #[arg(long)]
        config: PathBuf,
    },
    /// Add a login. The password comes from --password-file: `-` for stdin,
    /// or a file that is read once and deleted. Never from an argument.
    AddLogin {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        name: String,
        /// An origin it may be filled on, such as https://github.com. Repeatable.
        #[arg(long = "site", required = true)]
        sites: Vec<String>,
        #[arg(long)]
        username: String,
        #[arg(long, default_value = "")]
        hint: String,
        /// Whose Tickets may use it, as the Policy names people
        /// (slack:U024BE7LH). Repeatable; none: any requester.
        #[arg(long = "for")]
        users: Vec<String>,
        /// first-use (default): a person approves its first use on each
        /// Ticket. none: unattended.
        #[arg(long, default_value = "first-use")]
        approval: String,
        #[arg(long)]
        password_file: PathBuf,
        /// Overwrite an item of the same name.
        #[arg(long)]
        replace: bool,
    },
    /// Add an API key, bound to its hosts and the header that carries it.
    AddKey {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long = "host", required = true)]
        hosts: Vec<String>,
        #[arg(long, default_value = "authorization")]
        header: String,
        /// bearer: `Bearer <value>`; raw: the value as is.
        #[arg(long, default_value = "bearer")]
        scheme: String,
        #[arg(long, default_value = "")]
        about: String,
        #[arg(long = "for")]
        users: Vec<String>,
        #[arg(long, default_value = "first-use")]
        approval: String,
        #[arg(long)]
        value_file: PathBuf,
        #[arg(long)]
        replace: bool,
    },
    List {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Remove {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        name: String,
    },
}

/// `solos-swarm browser init-key --out PATH`: a trail key; prints
/// `ed25519:<hex>` on stdout for agent.yaml (`nodes.allow.<n>.audit_key`) or
/// for a verifier's `--trust`.
#[derive(clap::Subcommand)]
pub enum BrowserCommand {
    InitKey {
        #[arg(long)]
        out: PathBuf,
    },
}

/// `AgentCommand` gains:
///
/// ```ignore
/// /// Verify the event chain, every browser step's signature and every blob
/// /// it names, and print the trail of one Ticket or a summary of all.
/// /// Reads only; safe while the agent serves. Exit 1 on any problem.
/// Trail {
///     #[arg(long)] state_dir: PathBuf,
///     /// For the trusted keys (browser.audit_key, nodes.allow.*.audit_key).
///     #[arg(long)] config: Option<PathBuf>,
///     /// `<environment>=ed25519:<hex>`, repeatable; replaces the config's.
///     #[arg(long)] trust: Vec<String>,
///     #[arg(long)] ticket: Option<String>,
///     /// With --ticket: print one step's blob.
///     #[arg(long)] step: Option<u32>,
///     /// view (what the model got), snapshot, typed.
///     #[arg(long, default_value = "view")] show: String,
///     /// Unanswered steps are fatal too.
///     #[arg(long)] strict: bool,
///     #[arg(long)] json: bool,
/// },
/// ```
pub struct AgentTrailCommand;

#[cfg(test)]
mod tests {
    // Over a chain an agent_browser.rs run wrote:
    // - clean: no problems, every line printed, exit 0.
    // - flip one byte of a blob -> BlobCorrupt; delete one -> BlobMissing.
    // - rewrite one TrailDone body in a copy of agent.db and re-hash the whole chain from
    //   the start (what anyone with write access can do today) -> BadSignature.
    // - re-sign a step with a different key -> NoTrustedKey or BadSignature.
    // - drop a TrailDone and re-chain -> Unanswered and SessionGap.
    // - cut the last rows and re-chain -> Unclosed.
    // - swap two steps' bodies -> Mismatch.
}
