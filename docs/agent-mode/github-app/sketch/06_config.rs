//! Lives at: src/agent/config.rs and src/agent/config/{github.rs, sources.rs},
//! and `DecideTerms` in src/agent/broker.rs.
//!
//! Sketch only: not compiled. Candidate B's config, amended by the synthesis:
//! polling keeps today's 60-second default, GitHub-origin Tickets cap their
//! budget at `auto_approve_microusd` (candidate A), and `merge: auto` is
//! documented as acting only for untainted Tickets.
//!
//! One list of repositories (`forge.repos`), one credential (`github:`), no
//! token files. The old shape had two repository lists (the Source's `repos`
//! and the forge's `repos`) and two PATs; they were hand-synced. Now the
//! Source, the Channel, the Router's repository list and the mirrors all
//! derive from `forge.repos`, and the bot identity derives from the App.
//!
//! agent.yaml, production shape (also `agent.yaml.example` beside this file):
//!
//! ```yaml
//! policy:
//!   people:    [github:alice, discord:123456789012345678]
//!   approvers: [github:alice, discord:123456789012345678]
//!   auto_approve_microusd: 500000
//! github:
//!   app_id: 123456
//!   key_file: /run/user/1000/solos-agent/github-app.pem   # one-use copy of ~/agent/tokens/github-app.pem
//!   # poll_secs: 60               # the listings; quiet polls are free 304s
//!   # reconcile_secs: 300         # every open Change read again
//!   # trigger_label: solos        # optional (PR 4): labelling an issue asks for work
//!   # writes: true                # false: read and comment nothing, mint no write token
//! forge:
//!   kind: github
//!   repos:
//!     acme/widgets:
//!       about: The widget service
//!       base: main
//!       owes: pull_request        # default AND ceiling: the Router may lower it, never raise it
//!       merge: ask                # off | ask | auto
//!       ci: expected              # expected | none
//!       protected: [".github/**"]
//!       max_pushes: 6
//!       max_repairs: 2
//!       max_change_bytes: 5000000
//!       budget_microusd: 500000
//!       notes: Rust at /rust/bin, build offline.
//! sources:  [{ kind: github }]    # no fields; reads forge.repos
//! channels: [{ kind: github }]
//! ```

use serde::Deserialize;
use solos_agent_core::change::{Deliverable, RepoName};
use solos_agent_core::model::MicroUsd;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// `github:` in agent.yaml. Present whenever a github Source, a github
/// Channel or a `forge: github` is.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    pub app_id: u64,
    /// A one-use token file like every other: read once, deleted, parsed key
    /// kept in memory. Must not sit under a sandbox-visible path.
    pub key_file: PathBuf,
    /// The listings' interval (today's `poll_secs`, moved here).
    #[serde(default = "poll_secs")]
    pub poll_secs: u64,
    #[serde(default = "reconcile_secs")]
    pub reconcile_secs: u64,
    #[serde(default)]
    pub trigger_label: Option<String>,
    #[serde(default = "yes")]
    pub writes: bool,
    /// Test seams. Defaults are GitHub's; the offline tests point both at
    /// local servers and directories. `api_base` http(s); `git_base` https://,
    /// http:// or file://.
    #[serde(default = "api_base")]
    pub api_base: String,
    #[serde(default = "git_base")]
    pub git_base: String,
}

fn poll_secs() -> u64 {
    60
}
fn reconcile_secs() -> u64 {
    300
}
fn yes() -> bool {
    true
}
fn api_base() -> String {
    "https://api.github.com/".into()
}
fn git_base() -> String {
    "https://github.com/".into()
}

/// `forge:`. `Github` loses `token_file` and `identity` (derived from the App).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ForgeConfig {
    Github {
        repos: BTreeMap<RepoName, RepoConfig>,
    },
    /// Clone-only, as today (tests, the verify overlay). It has no pull
    /// requests, so `owes: pull_request` is refused for it at load.
    Local {
        remote: PathBuf,
        identity: Identity,
        repos: BTreeMap<RepoName, RepoConfig>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    // ---- today ----
    pub about: String,
    pub base: String,
    #[serde(default)]
    pub budget_microusd: Option<MicroUsd>,
    #[serde(default)]
    pub notes: Option<String>,
    /// What the work owes when the Router names nothing, AND the most it may
    /// owe: `router_decide` refuses a `deliverable` above it. `answer` makes a
    /// repository read-only to the agent whatever a model asks. (This tightens
    /// today's "default only"; documented in the changelog.)
    #[serde(default)]
    pub owes: Deliverable,
    // ---- new ----
    /// `auto` merges with no person only when the requester may approve
    /// (`ForgeTerms::for_requester`), CI passed with at least one check, and
    /// the Ticket is untainted (`Ticket::tainted`); otherwise it asks.
    #[serde(default)]
    pub merge: MergeConfig,
    #[serde(default)]
    pub ci: CiConfig,
    #[serde(default = "protected")]
    pub protected: Vec<String>,
    #[serde(default = "max_pushes")]
    pub max_pushes: u32,
    #[serde(default = "max_repairs")]
    pub max_repairs: u32,
    #[serde(default = "max_change_bytes")]
    pub max_change_bytes: u64,
    #[serde(default)]
    pub merge_method: MergeMethodConfig,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeConfig {
    Off,
    #[default]
    Ask,
    Auto,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiConfig {
    #[default]
    Expected,
    None,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethodConfig {
    #[default]
    Squash,
    Merge,
    Rebase,
}

fn protected() -> Vec<String> {
    vec![".github/**".into()]
}
fn max_pushes() -> u32 {
    6
}
fn max_repairs() -> u32 {
    2
}
fn max_change_bytes() -> u64 {
    5_000_000
}

// ---------------------------------------------------------------------------
// Load-time validation (config.rs / config/sources.rs). Every rule is a
// refusal with the field and the fix named; none is a runtime surprise.
// ---------------------------------------------------------------------------

pub fn validate_github(_config: &super::AgentConfigView) -> Result<(), String> {
    todo!(
        "\
 - `github` present iff a github Source, a github Channel or `forge: github` is; a github\n\
   Source or Channel without `github:` -> 'add a github: section (docs/agent-mode/README.md#github-app)'.\n\
 - at most one github Source; it takes NO fields. `repos`, `token_file`, `api_base`,\n\
   `poll_secs` on it -> 'moved: repositories are forge.repos, the credential and the poll are the\n\
   github: section'. `token_file` or `identity` on `forge: github` -> 'the GitHub App replaced token\n\
   files; see docs/agent-mode/README.md#github-app'. (Detected on the raw YAML before serde's\n\
   deny_unknown_fields so the sentence is ours.)\n\
 - poll_secs >= 10 (today's floor), reconcile_secs >= 60; trigger_label is a GitHub label name.\n\
 - `policy.people`/`approvers`/`bots` entries beginning `github:` are lowercased and must be a\n\
   valid login (alphanumerics and single hyphens, optional [bot]). Each is pinned to its account\n\
   id at start (`Identities`, 02_app_auth.rs); the pin file is state, not config.\n\
 - `merge: auto` requires `ci: expected` (a repository with no CI never auto-merges) and a\n\
   non-empty `policy.approvers` containing at least one entry on a source that can be a requester.\n\
 - `owes: pull_request` requires `forge: github` and `github.writes: true`; `owes: change`\n\
   is allowed on `local`.\n\
 - `protected` globs compile; `base` is a branch name (as today); max_* >= 1;\n\
   max_change_bytes <= 50 MB.\n\
 - budgets above `auto_approve_microusd` need a button channel and approver (as today).\n\
 - key_file is not under a sandbox-visible path (Exposure::view().hides)."
    )
}

/// What `router_decide` does with the new terms (broker.rs `verdict`): clamp
/// `deliverable` to `repo.owes`, refuse above it with the ceiling named, and
/// build `Accept.forge = Some(ForgeTerms { merge, ci_expected, max_pushes,
/// max_repairs })` from config for any deliverable above `Answer`. The Router
/// names a repository and a deliverable; it never names, and cannot influence,
/// a merge mode, a limit or a path.
pub struct DecideAdditions;

/// Graft from candidate A (A/sketch/05_config.rs `DecideTermsChange`):
/// `DecideTerms::for_config`'s budget ceiling becomes per source. Today it
/// caps every accept at `auto_approve_microusd` only when *no* button channel
/// can approve, so with Discord configured a GitHub-origin Ticket could ask
/// for a budget approval on GitHub, which has no button, and wait for ever.
/// Now: a candidate whose primary Trigger's source has a channel that shows
/// buttons and an approver on that source may name any budget; any other
/// (github, mcp, schedule) is capped at `auto_approve_microusd`, so a GitHub
/// Ticket never waits on a button GitHub cannot show. (A repair or review
/// round spends inside the same envelope.)
pub struct DecideTermsChange;

/// A shared view so the sketch needn't name the whole AgentConfig.
pub struct AgentConfigView;
