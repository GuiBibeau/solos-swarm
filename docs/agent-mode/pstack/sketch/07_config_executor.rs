//! Lives at: src/agent/config.rs (new sections), src/agent/mod.rs (wiring and
//! `ServeOptions`), src/agent/pi_executor.rs (the launch), src/agent/ports.rs
//! (`Launch`), src/agent/pi_router.rs (the Router's prompt) and src/main.rs
//! (`--credential-file` may repeat).
//!
//! Without a `pstack` section and a `forge` section, agent mode is exactly
//! what it is today: no Plan gets a method, no Mandate a repository, and no
//! seat holds `Read`, `Repo` or `Delegates`.

use solos_agent_core::change::{CheckName, Entry, RepoName};
use solos_agent_core::model::MicroUsd;
use std::collections::BTreeMap;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// agent.yaml
// ---------------------------------------------------------------------------

/// `pstack:` in agent.yaml.
///
/// ```yaml
/// pstack:
///   entries: [poteto-mode, how, why]      # what the Router may name; the first is the default
///   models:                               # aliases beyond the workers' own model
///     glm-5: {provider: venice, model: zai-org-glm-5, prices: {input_microusd_per_million: 600000, output_microusd_per_million: 2200000}}
///   roles:                                # the workers' pstack-models rule; a missing role maps per the adapter
///     "feature, refactoring": deepseek-flash
///     bug-fix: deepseek-flash
///     judgment and prose: glm-5
///     hardest tasks: glm-5
///     how explorer: deepseek-flash
///     how explainer: glm-5
///     arena runners: [deepseek-flash, glm-5]
///     arena cross-judge pool: [glm-5, deepseek-flash]
///     swarm workers: deepseek-flash
///     architect runners: [deepseek-flash, glm-5]
///     interrogate reviewers: [deepseek-flash, glm-5]
///   delegates: {max_live: 4, per_assignment: 24, max_microusd: 400000, timeout_secs: 3600}
/// ```
///
/// There is no `root`: the tree is `<pi-root>/vendor/pstack`, verified
/// against its MANIFEST at start. One way to pick a pstack version: the
/// solos-swarm commit that is running.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PstackConfig {
    /// Each must name a skill in the vendored tree.
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
    /// Role label to one alias or a list. Labels are checked against the
    /// vendored setup-pstack's rule block, aliases against the model table.
    #[serde(default)]
    pub roles: BTreeMap<String, OneOrMany>,
    #[serde(default)]
    pub delegates: Option<DelegatesConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// `deepseek` or `venice`, as `executor.provider` takes.
    pub provider: String,
    pub model: String,
    pub prices: crate::gateway::PriceTable,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatesConfig {
    pub max_live: Option<usize>,
    pub per_assignment: Option<usize>,
    pub max_microusd: Option<MicroUsd>,
    pub timeout_secs: Option<u64>,
}

/// `forge:` in agent.yaml.
///
/// ```yaml
/// forge:
///   kind: github
///   token_file: secrets/forge-token        # read once and deleted, like every token file
///   identity: {name: solos-swarm-bot, email: bot@example.com}
///   # api_base: https://api.github.com/
///   repos:
///     GuiBibeau/solos-swarm:
///       about: The agent's own code, Rust and TypeScript.   # the Router reads this
///       base: main
///       budget_microusd: 3000000           # an accept naming this repo gets it unless it names one
///       notes: >-                          # the worker's prompt reads this
///         Rust is at /rust/bin and its crates at /cargo; build offline. Tests: cargo test --offline.
///       checks:
///         unit: {command: "PATH=/rust/bin:$PATH CARGO_HOME=/cargo CARGO_TARGET_DIR=/checks/cache cargo test --offline --workspace --lib"}
///         ci: {forge: true}                # the forge's CI at the commit counts as this check
///       merge: ask                         # ask (default): a person clicks Merge. auto: the worker may merge.
///       # protected: [".github/workflows/**"]   # the default
///       # answerable_by: [discord:1234…]        # who may click Merge; default the Policy's approvers
/// ```
///
/// Validation at load: every repo has at least one check (a `ready` with an
/// open pull request is impossible otherwise); `merge: ask` needs a Channel
/// that shows buttons and has an approver; the forge needs `executor` to be
/// Pi with `run_code` (publishing bundles in a sandbox); `kind: local` takes
/// `remote: <path>` instead of a token, for tests and dry runs.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeConfig {
    pub kind: ForgeKind,
    pub token_file: Option<PathBuf>,
    pub remote: Option<PathBuf>,
    pub identity: Identity,
    #[serde(default)]
    pub api_base: Option<String>,
    pub repos: BTreeMap<RepoName, RepoConfig>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeKind {
    Github,
    Local,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub about: String,
    pub base: String,
    #[serde(default)]
    pub budget_microusd: Option<MicroUsd>,
    #[serde(default)]
    pub notes: Option<String>,
    pub checks: BTreeMap<CheckName, CheckConfig>,
    #[serde(default)]
    pub merge: MergeMode,
    #[serde(default = "default_protected")]
    pub protected: Vec<String>,
    #[serde(default)]
    pub answerable_by: Vec<String>,
}

fn default_protected() -> Vec<String> {
    vec![".github/workflows/**".into()]
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum CheckConfig {
    /// Run in the Ticket's run_code sandbox on a clean checkout of the
    /// commit, with the job limits.
    Command { command: String },
    /// The forge's CI at the commit.
    Forge { forge: bool },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    #[default]
    Ask,
    Auto,
}

/// `ServeOptions.credential` becomes `credentials`, by provider. The CLI's
/// `--credential-file <path>` keeps meaning the Pi upstream's provider, and
/// `--credential-file venice=<path>` adds another; each file is read once.
/// Config load refuses a `pstack.models` alias whose provider has none. The
/// broker's single `provider` becomes `Providers`, chosen per call by
/// `grant.terms.upstream.provider`; holds, settlement and the model contract
/// are unchanged, since each grant still names exactly one upstream.
pub struct ServeOptionsChange {
    pub credentials: BTreeMap<String, zeroize::Zeroizing<String>>,
}

// ---------------------------------------------------------------------------
// The launch (pi_executor.rs).
// ---------------------------------------------------------------------------

/// `Launch` gains what the session needs to enter its method, read from the
/// Ticket by the runtime when it builds the Launch:
///
/// ```ignore
/// pub method: Option<Entry>,        // Plan::Session.method
/// pub repo: Option<RepoName>,       // Mandate.repo
/// /// The text of the Ticket's latest `progress` Report, for a recovery: the
/// /// todo list the last session kept (the adapter maps pstack's todolist to it).
/// pub last_progress: Option<String>,
/// ```
pub struct LaunchAdditions;

/// `Inner::session` (pi_executor.rs:531-618) gains, in order, before the
/// session starts:
///
/// 1. `ticket_dir` also makes `method/`, `host/` and `children/`.
/// 2. For a repository: `forge.prepare(repo, ticket, dirs)` instead of
///    nothing, then the operator's `prepare` as today. Refused for a
///    remote environment: `decide` already refuses a repository with a
///    remote environment, so this is a guard, not a path.
/// 3. The seat gets `Read` when the Plan has a method or the Mandate a
///    repository, `Repo` with a repository, `Delegates` when
///    `pstack.delegates` is set; its `View` per 06_broker.rs.
/// 4. The system prompt is `system_prompt(budget, grants)` plus
///    `pstack::section(..)` when the Plan has a method.
/// 5. `Fact::Entered` is sent.
/// 6. The first prompt is `tree.enter(skill, &assignment_text(..))` for a
///    method, else `assignment_text(..)` as today. On a recovery,
///    `assignment_text` adds: "Your last progress report, which held your
///    todo list, was: <quoted>. Your notes are in /method. Delegates of the
///    earlier assignment are gone; their workspaces and reports are under
///    /children." Later deliveries are unchanged: the skill is not pasted
///    again, and the system prompt names its path in case compaction drops
///    it.
pub struct SessionChanges;

// ---------------------------------------------------------------------------
// The Router (pi_router.rs, broker.rs DecideTerms).
// ---------------------------------------------------------------------------

/// `DecideTerms` gains `entries: Vec<(Entry, String)>` (name, pstack's
/// description) and `repos: Vec<(RepoName, String, Option<MicroUsd>)>`
/// (name, about, budget). `DecideArgs::verdict` checks `method` against
/// `entries` and fills the first when it is omitted, checks `repo` against
/// `repos`, gives a repo's budget when the accept names none, and refuses a
/// repo with a remote `environment`. The Router's prompt gains:
///
/// ```text
/// Work on code is work: a bug fix, a change, a review, and also a question about how or why a
/// repository works. Accept it as a session and name its repo:
/// - GuiBibeau/solos-swarm: The agent's own code, Rust and TypeScript. (budget 3000000)
/// Every session works the pstack way. Its method is poteto-mode unless you name another:
/// - how: Use for "how does X work", code walkthroughs before changing something, …
/// - why: Use for 'why does X work this way', 'why we picked Y', …
/// ```
///
/// The descriptions are pstack's frontmatter, so a new entry costs one line
/// of config. The Router itself does not enter pstack: it judges cards on a
/// daily cap, and poteto-mode's 21.9 KB per turn would buy it a doer's
/// method for a triage job.
pub struct RouterChanges;

// Tests: config load refuses an entry not in the tree, a role not in
// setup-pstack, an alias with no credential, a repo with no check, `merge:
// ask` with no button Channel, and a forge without run_code; `decide`
// refuses an unknown repo, a repo with a remote environment and an unknown
// method, fills the default method and the repo's budget, and records both
// on the Ticket; the Router's prompt lists the entries with pstack's own
// descriptions; a recovery Launch's first prompt holds the skill block and
// the last progress text.
