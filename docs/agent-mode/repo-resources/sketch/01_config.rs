//! Lives at: src/agent/config.rs (modified) and src/code.rs (modified).
//!
//! Sketch only: not compiled. It names the types and their checks,
//! not working code.
//!
//! The per-repository settings of #99 and #100, beside `deps` and `evidence`
//! on `RepoConfig` (src/agent/config.rs:287), and the `disk` section that
//! `agent serve` reads. Unknown fields stay errors at every level.

use crate::code::Mount;
use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Added to `RepoConfig`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfigAdditions {
    /// Read-only binds for this repository's Tickets, after `/work`. Same
    /// entries and `Mount::validate` as `executor.run_code.mounts`, plus the
    /// rules in `validate_repo_mounts`.
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub scratch: Option<ScratchConfig>,
    #[serde(default)]
    pub egress: Option<EgressConfig>,
}

/// `disk`: the floor `agent serve` never takes free space below. Required
/// when any repository has `scratch`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskConfig {
    /// GiB, as `health.min_free_gb`. At least that.
    pub floor_gb: u64,
    /// The floor guard acts this far above the floor.
    #[serde(default = "margin_gb")]
    pub margin_gb: u64,
}

fn margin_gb() -> u64 {
    32
}

/// Added to `RunCodeConfig`: the `/work` cap for a Ticket whose repository
/// has no `scratch` reservation. 0 turns it off. The same accountant
/// measures it, and the same kill applies.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCodeConfigAdditions {
    #[serde(default = "work_max_gb")]
    pub work_max_gb: u64,
}

fn work_max_gb() -> u64 {
    32
}

/// `forge.repos.<repo>.scratch`: a writable `/scratch` per Ticket.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScratchConfig {
    /// The quota for the Ticket's `/work` and `/scratch` together, 1..=1024
    /// GiB, so moving files into the clone escapes nothing. Also the
    /// sandbox's `--fsize`, capped at 64 GiB.
    pub max_gb: u64,
    #[serde(default = "max_files")]
    pub max_files: u64,
    /// After the Ticket ends, `agent prune` frees it.
    #[serde(default = "keep_hours")]
    pub keep_hours: u64,
}

fn max_files() -> u64 {
    1_000_000
}

fn keep_hours() -> u64 {
    24
}

/// `forge.repos.<repo>.egress`: what the gate lets this repository's
/// Tickets reach.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressConfig {
    pub hosts: Vec<HostRule>,
    /// Bytes both ways, per Ticket, in GB.
    #[serde(default = "ticket_gb")]
    pub ticket_gb: u64,
    /// Bytes both ways, per repository per UTC day, in GB.
    #[serde(default = "daily_gb")]
    pub daily_gb: u64,
    /// Sandbox paths the CA bundle is bound over, for clients that trust only
    /// their own file (grpclib reads certifi's).
    #[serde(default)]
    pub ca_paths: Vec<PathBuf>,
}

fn ticket_gb() -> u64 {
    100
}

fn daily_gb() -> u64 {
    200
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostRule {
    /// An exact name: lowercase ASCII, no wildcard, no IP literal.
    pub host: String,
    #[serde(default = "https_port")]
    pub ports: Vec<u16>,
    /// Derived when absent: `Inspect` if any of the request rules, keys,
    /// `github` or `price` is set.
    #[serde(default)]
    pub mode: Option<Mode>,
    /// Sandbox variable -> vault key name. The variable holds the placeholder
    /// `vault:<name>`; the gate puts the key's own header on each request.
    #[serde(default)]
    pub keys: BTreeMap<String, String>,
    #[serde(default)]
    pub github: Option<GithubAccess>,
    #[serde(default)]
    pub methods: Option<Vec<String>>,
    /// Globs over the request's path, without its query.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    #[serde(default)]
    pub price: Option<Price>,
}

fn https_port() -> Vec<u16> {
    vec![443]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// CONNECT, SNI checked, bytes copied. One record per connection.
    Tunnel,
    /// TLS ended by the gate. One record per request; rules and keys apply.
    Inspect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubAccess {
    /// A read-only installation token for this one repository.
    Read,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub per_gb_microusd: u64,
}

impl HostRule {
    pub fn mode(&self) -> Mode {
        todo!("explicit mode, else Inspect when keys, github, methods, paths or price")
    }

    /// Exact name; ports 1..=65535 and not empty; `Tunnel` with any request
    /// rule, key, `github` or `price` refused; methods known to the gate;
    /// paths that parse as globs.
    pub fn validate(&self) -> Result<()> {
        todo!()
    }
}

/// Checked at load, for each repository, with the global list and the host's
/// paths:
/// - each mount passes `Mount::validate` (src/code.rs:83);
/// - no target twice across `executor.run_code.mounts` and this list;
/// - no `/pstack` with pstack on; `/scratch` is reserved (src/code.rs:70);
/// - each source canonical, an existing directory, not `/`, not `$HOME`, and
///   no ancestor of the state directory, the config's directory, the vault,
///   its key or the GitHub App's key.
pub fn validate_repo_mounts(
    repo: &str,
    global: &[Mount],
    mounts: &[Mount],
    forbidden_inside: &[PathBuf],
) -> Result<()> {
    let _ = (repo, global, mounts, forbidden_inside);
    todo!()
}

// src/code.rs (modified)

/// `RESERVED_TARGETS` gains `scratch`, and any top-level target that
/// `feat/cargo-deps` mounts.
pub const RESERVED_TARGETS: [&str; 12] = [
    "bin", "dev", "etc", "lib", "lib64", "proc", "runtime", "sbin", "scratch", "tmp", "usr", "work",
];

/// `Bind` gains a writable variant, for `/scratch` only; the host chose the
/// source.
pub enum BindAdditions {
    Writable { source: PathBuf, target: PathBuf },
}

/// `Extras` gains what egress and scratch need.
pub struct ExtrasAdditions {
    /// A program the sandbox runs `bash -c CODE` under: the egress relay.
    pub wrapper: Option<Vec<std::ffi::OsString>>,
    /// `--fsize` in place of 256 MiB: the scratch quota (at most 64 GiB), or 0
    /// while the Ticket is over it.
    pub file_size_bytes: Option<u64>,
}
