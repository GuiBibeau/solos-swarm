//! Lives at: src/agent/models.rs (new: the policy and clearances) and
//! src/agent/config.rs (`RepoConfig.models`, `ExecutorConfig::Pi.upstreams`).
//!
//! Sketch only: not compiled. The one place that answers "may R's text reach
//! this provider in this role". Pure: no I/O. Everything that builds a prompt
//! or serves a read asks it.

use serde::Deserialize;
use solos_agent_core::change::RepoName;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// Who receives the text and bills for it. The policy keys on this alone:
/// the wire and the harness do not change who reads the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Deepseek,
    Venice,
    Anthropic,
}

/// How the broker talks to a provider. `Anthropic` comes from
/// `feat/deepseek-anthropic-wire`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    Chat,
    Anthropic,
}

/// What reads a repository's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Cards, `router_read`, the Router's system prompt.
    Router,
    /// The repository's Tickets' workers, and any worker shown a memo that
    /// carries it.
    Worker,
    /// The memo review and the dream.
    Memory,
}

/// An upstream's name: `default` (the executor's own fields) or a key of
/// `executor.upstreams`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
pub struct UpstreamName(String);

impl UpstreamName {
    pub const DEFAULT: &'static str = "default";
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// `forge.repos.<repo>.models`. Required on every repository: no repository
/// reaches any provider by default.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    pub allow: Allow,
    /// The upstream its workers run on. Without it: `default` when its
    /// provider is allowed, else the one allowed upstream.
    #[serde(default)]
    pub worker: Option<UpstreamName>,
}

/// A list applies to every role; a map names all three.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Allow {
    Every(BTreeSet<Provider>),
    PerRole(PerRole),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerRole {
    pub router: BTreeSet<Provider>,
    pub worker: BTreeSet<Provider>,
    pub memory: BTreeSet<Provider>,
}

/// `executor.upstreams.<name>`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub provider: Provider,
    pub model: String,
    pub wire: Wire,
    /// `pi` needs `chat`; `anthropic` has only its own wire, so `claude-code`.
    pub harness: crate::agent::config::HarnessKind,
    /// A one-use token file, refused where a sandbox can read it, as
    /// `github.key_file` is.
    pub key_file: PathBuf,
    pub prices: Prices,
    #[serde(default)]
    pub max_input_bytes: Option<usize>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// `CLAUDE_CODE_EFFORT_LEVEL` for a `claude-code` session.
    #[serde(default)]
    pub effort: Option<String>,
}

/// `PriceTable` plus the cache-write rate Anthropic bills (1.25 times the
/// input rate). The hold prices input at the higher of input and write, so a
/// settlement never passes its hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prices {
    pub input_microusd_per_million: u64,
    pub output_microusd_per_million: u64,
    #[serde(default)]
    pub cache_hit_microusd_per_million: Option<u64>,
    #[serde(default)]
    pub cache_write_microusd_per_million: Option<u64>,
}

impl Prices {
    /// Bounds per provider: DeepSeek's floor and today's $10 ceiling stay for
    /// `deepseek` and `venice`; `anthropic` gets its own.
    pub fn validate(&self, provider: Provider) -> anyhow::Result<()> {
        todo!()
    }

    pub fn hold(&self, input_bytes: usize, max_output_tokens: u64) -> u64 {
        todo!("input at max(input, cache_write), all-miss, as today")
    }
}

// ---------------------------------------------------------------------------
// The policy
// ---------------------------------------------------------------------------

/// Built once from config. `allow` per repository, and each upstream's
/// provider.
pub struct ModelPolicy {
    repos: BTreeMap<RepoName, PerRole>,
    upstreams: BTreeMap<UpstreamName, Provider>,
    /// Each repository's worker upstream, or `None` where no upstream here is
    /// allowed: its Tickets are declined.
    workers: BTreeMap<RepoName, Option<UpstreamName>>,
}

impl ModelPolicy {
    pub fn from_config(config: &crate::agent::AgentConfig) -> anyhow::Result<Self> {
        todo!("every repo names models; worker resolves or is None; harness and wire fit")
    }

    /// Whether `repo`'s text may reach `provider` in `role`. `None` (no
    /// repository) is always admitted: such text is a person's own.
    pub fn admits(&self, repo: Option<&RepoName>, role: Role, provider: Provider) -> bool {
        todo!()
    }

    /// The upstream `repo`'s workers run on, or why there is none.
    pub fn worker_upstream(&self, repo: &RepoName) -> Result<(&UpstreamName, Provider), Declined> {
        todo!()
    }

    /// The repositories a session on `provider` in `role` may read: what the
    /// runtime hands the core as a memo audience's clearance.
    pub fn cleared(&self, role: Role, provider: Provider) -> BTreeSet<RepoName> {
        todo!()
    }

    /// Whether a memo, a Ticket or an Input that carries `repos` may reach
    /// this session.
    pub fn admits_all(&self, repos: &BTreeSet<RepoName>, role: Role, provider: Provider) -> bool {
        repos.iter().all(|repo| self.admits(Some(repo), role, provider))
    }
}

/// Why a repository's work cannot run here. Its `tell` is what the person
/// reads, written by the host, never by a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declined {
    pub repo: RepoName,
    pub allowed: BTreeSet<Provider>,
}

impl Declined {
    /// "I can't work on <repo> here: its code may go only to anthropic, and
    /// this agent runs no anthropic model. Nothing was started."
    pub fn tell(&self) -> String {
        todo!()
    }
}

/// The provider a session's calls go to, and its role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clearance {
    pub role: Role,
    pub provider: Provider,
}

/// What a session reads in place of text it may not: the repository and the
/// providers it allows, never the text.
pub fn withheld(repo: &RepoName, allowed: &BTreeSet<Provider>) -> String {
    todo!("[withheld: <repo> allows its text only on <providers>]")
}

/// The repository whose text a Trigger is: the GitHub source's threads are
/// `github:<owner>/<name>#<n>` (src/agent/github/mod.rs:57).
pub fn repo_of(trigger: &solos_agent_core::model::Trigger) -> Option<RepoName> {
    todo!()
}
