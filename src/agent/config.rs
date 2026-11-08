//! `agent.yaml`. Unknown fields are errors, like a swarm Goal's, so a typo
//! cannot silently fall back to a default.

mod health;
mod nodes;
mod sources;
mod watchdog;
mod web;

pub use health::HealthConfig;
pub use nodes::{AllowedNode, NodesConfig};
pub use sources::{Interval, ReplyTo, ScheduleConfig, TimeOfDay};
pub use watchdog::{
    DiskFloor, JobCheck, JobConfig, OperatorConfig, Progress, ThresholdsConfig, WatchdogConfig,
};
pub use web::{DEFAULT_API_URL, PublishConfig, WebConfig};

use super::broker::ModelTerms;
use super::broker::memory::{DreamTerms, MemoryParts};
use super::broker::review::{Effort, ReviewTerms};
use super::dream::{self, DreamSchedule};
use super::goal_template::GoalTemplates;
use super::jobs::{self, JobLimits};
use super::links::TicketLinks;
use super::simulated::TimeFactor;
use super::trail::TrailStoreConfig;
use super::{discord, github, mcp, pstack, slack};
use crate::browse::BrowserConfig;
use crate::code::{self, Mount};
use crate::gateway::{PriceTable, Temperature, upstream_profile};
use crate::vault::VaultConfig;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Deserializer, Serialize};
use solos_agent_core::AllowlistPolicy;
use solos_agent_core::change::{Deliverable, ForgeTerms, MergeMode, RepoName};
use solos_agent_core::model::MicroUsd;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub policy: AllowlistPolicy,
    pub sources: Vec<SourceConfig>,
    pub channels: Vec<ChannelConfig>,
    pub router: RouterConfig,
    pub executor: ExecutorConfig,
    /// Without it, nothing the agent does is capped except a swarm, which
    /// always needs a budget.
    #[serde(default)]
    pub budgets: Budgets,
    /// Without it, no Watchdog runs.
    #[serde(default)]
    pub watchdog: Option<WatchdogConfig>,
    /// Remote nodes that run workers' commands. Without it, none connect.
    #[serde(default)]
    pub nodes: Option<NodesConfig>,
    /// An allowed person's follow-up on a subscribed Thread goes only to its
    /// Ticket. `false` also sends it to the Router, as `agent bench`'s
    /// `no-pass-through` design does.
    #[serde(default = "pass_through")]
    pub pass_through: bool,
    #[serde(default)]
    pub vault: Option<VaultConfig>,
    /// Workers' headless browser. Without it, no browser tools.
    #[serde(default)]
    pub browser: Option<BrowserConfig>,
    /// Where the browser's trail blobs are kept, and how much.
    #[serde(default)]
    pub trail: TrailStoreConfig,
    #[serde(default)]
    pub pstack: Option<PstackConfig>,
    /// Where repository Tickets' code comes from. Without it, no Ticket
    /// names a repository.
    #[serde(default)]
    pub forge: Option<ForgeConfig>,
    /// The GitHub App the agent acts as. Present whenever a github Source, a
    /// github Channel or `forge: github` is.
    #[serde(default)]
    pub github: Option<GithubConfig>,
    /// The web dashboard's address, and the sidecar that publishes the
    /// Store's tickets to it. Without it, there is no dashboard and replies
    /// link nowhere.
    #[serde(default)]
    pub web: Option<WebConfig>,
    /// What `agent health` (the check systemd runs beside the agent) treats
    /// as trouble. Without it, its defaults apply.
    #[serde(default)]
    pub health: Option<HealthConfig>,
    /// Workers' memory: whether they propose memos, and the review that
    /// decides each one that is not outside text. On by default where workers
    /// run on Pi.
    #[serde(default)]
    pub memory: MemoryConfig,
}

fn pass_through() -> bool {
    true
}

/// How long one review call may take: the transport's own limit.
const REVIEW_CALL_LIMIT: Duration = Duration::from_secs(600);

/// `memory:` in agent.yaml.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    /// Workers may propose memos (`memo_propose`, and `/memos` in `read`),
    /// each decided by the review. `false`: only the owner writes memos.
    #[serde(default = "proposals")]
    pub proposals: bool,
    /// The review's model and thinking; each field the workers' unless set.
    #[serde(default)]
    pub review: ReviewConfig,
    /// What the review and the dream may commit per UTC day on the
    /// `memory` account. Absent: no cap, and every call is still held and
    /// settled there.
    #[serde(default)]
    pub daily_microusd: Option<MicroUsd>,
    /// The daily dream: a Ticket that checks the memos against the
    /// transcripts of the Tickets that ended since the last one. On by
    /// default wherever workers propose memos.
    #[serde(default)]
    pub dream: DreamConfig,
}

fn proposals() -> bool {
    true
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            proposals: proposals(),
            review: ReviewConfig::default(),
            daily_microusd: None,
            dream: DreamConfig::default(),
        }
    }
}

/// `memory.dream`: when the daily dream is asked for, and how it thinks.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DreamConfig {
    #[serde(default = "dream_enabled")]
    pub enabled: bool,
    /// The time of day, UTC, `HH:MM`: before the daily prune (04:30 by
    /// default), so the transcripts it reads are still there.
    #[serde(default = "dream_at")]
    pub at: TimeOfDay,
    /// DeepSeek's `reasoning_effort` for the dream's calls, with thinking on:
    /// `low`, `high` or `max`, the default.
    #[serde(default)]
    pub thinking: Effort,
    /// What one of the dream's calls may answer, reasoning included. By
    /// default what DeepSeek allows in thinking mode at that effort.
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

fn dream_enabled() -> bool {
    true
}

fn dream_at() -> TimeOfDay {
    TimeOfDay(3 * 3_600_000 + 30 * 60_000)
}

impl Default for DreamConfig {
    fn default() -> Self {
        Self {
            enabled: dream_enabled(),
            at: dream_at(),
            thinking: Effort::default(),
            max_output_tokens: None,
        }
    }
}

/// `memory.review`: the separate model call that decides each proposal that
/// is not outside text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfig {
    /// The workers' provider, which is the only one allowed: one credential
    /// serves both.
    #[serde(default)]
    pub provider: Option<String>,
    /// The workers' model unless set.
    #[serde(default)]
    pub model: Option<String>,
    /// The workers' prices unless set.
    #[serde(default)]
    pub prices: Option<PriceTable>,
    /// DeepSeek's `reasoning_effort` with thinking on: `low`, `high` or
    /// `max`, the default.
    #[serde(default)]
    pub thinking: Effort,
    /// What one review may answer, reasoning included. By default what
    /// DeepSeek allows in thinking mode at that effort: 65536, or 131072 at
    /// `max`.
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

/// Where the setup is written down, for the sentences that tell an owner of
/// the old shape what replaced it.
const GITHUB_APP_DOC: &str = "docs/agent-mode/how-to/github-app.md";

/// `github:` in agent.yaml: the GitHub App every GitHub call is made as. The
/// Source, the Channel and the forge share it; the repositories they act on
/// are `forge.repos`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    /// The App's numeric id, at the top of its settings page.
    pub app_id: u64,
    /// The App's private key (PEM), a one-use token file like every other:
    /// read once and deleted, the parsed key kept in memory. It must not sit
    /// under a path any sandbox can read.
    pub key_file: PathBuf,
    /// The Source's interval. A quiet poll costs 304s, which GitHub does not
    /// count against the rate limit.
    #[serde(default = "github_poll_secs")]
    pub poll_secs: u64,
    /// How often every open change is read again, whatever the listings said.
    #[serde(default = "github_reconcile_secs")]
    pub reconcile_secs: u64,
    /// `false`: no write token is ever minted, so the agent reads and writes
    /// nothing on GitHub (a restart applies it).
    #[serde(default = "github_writes")]
    pub writes: bool,
    /// Test seam: GitHub's REST root, or GitHub Enterprise Server's
    /// `https://HOST/api/v3/`.
    #[serde(default = "github_api_base")]
    pub api_base: String,
    /// Test seam: where the host's mirrors fetch `<owner>/<name>.git` from.
    /// `https://github.com/` by default; the offline tests use `file://`.
    #[serde(default = "github_git_base")]
    pub git_base: String,
}

/// `forge:` in agent.yaml: the repositories a Ticket may name, and where
/// the host's mirror of each fetches from.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ForgeConfig {
    /// GitHub, through the App of the `github:` section. The clone's commits
    /// are by the App's bot.
    Github {
        repos: BTreeMap<RepoName, RepoConfig>,
    },
    /// Bare repositories on this disk, `<remote>/<owner>/<name>.git`, for
    /// tests and dry runs.
    Local {
        remote: PathBuf,
        identity: Identity,
        repos: BTreeMap<RepoName, RepoConfig>,
    },
}

impl ForgeConfig {
    pub fn repos(&self) -> &BTreeMap<RepoName, RepoConfig> {
        match self {
            Self::Github { repos, .. } | Self::Local { repos, .. } => repos,
        }
    }

    /// Who a local clone's commits are by. GitHub's come from the App.
    pub fn identity(&self) -> Option<&Identity> {
        match self {
            Self::Github { .. } => None,
            Self::Local { identity, .. } => Some(identity),
        }
    }
}

/// Who the commits in a clone are by.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

/// One repository, `forge.repos.<owner>/<name>`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// What the Router reads to tell which repository a request is about.
    pub about: String,
    /// The branch a Ticket's clone starts from.
    pub base: String,
    /// What an accept that names this repository and no budget gets.
    #[serde(default)]
    pub budget_microusd: Option<MicroUsd>,
    /// What the worker's prompt says about working in it, such as where its
    /// toolchain is.
    #[serde(default)]
    pub notes: Option<String>,
    /// What its work owes when the Router names nothing, and the most it may
    /// owe: the Router may lower it for one request, never raise it, so
    /// `answer` makes a repository read-only to the agent whatever a model
    /// asks.
    #[serde(default)]
    pub owes: Deliverable,
    /// Whether the agent merges its own pull requests. Read at accept and
    /// recorded in the Ticket's terms.
    #[serde(default)]
    pub merge: MergeConfig,
    /// Whether the repository has CI that a merge waits for.
    #[serde(default)]
    pub ci: CiConfig,
    /// Paths no commit of the agent may change, as globs. A pull request whose
    /// commits touch one is refused when the worker reports ready. The App
    /// holds no `workflows` permission either way.
    #[serde(default = "protected")]
    pub protected: Vec<String>,
    /// Heads the agent publishes for one Ticket, repairs and review rounds
    /// included.
    #[serde(default = "max_pushes")]
    pub max_pushes: u32,
    /// CI failures that may resume a delivered Ticket.
    #[serde(default = "max_repairs")]
    pub max_repairs: u32,
    /// The largest change, in bytes of commits, a ready is admitted with.
    #[serde(default = "max_change_bytes")]
    pub max_change_bytes: u64,
    /// Dependencies the host installs for the repository's base commit, in a
    /// cache every Ticket's `run_code` mounts read-only. Without it, nothing
    /// is installed, and the sandbox is offline.
    #[serde(default)]
    pub deps: Option<DepsConfig>,
    /// What a pull request of this repository must carry in its description,
    /// and the command whose output it is. Without it, a ready report is not
    /// checked for it.
    #[serde(default)]
    pub evidence: Option<EvidenceConfig>,
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            about: String::new(),
            base: "main".into(),
            budget_microusd: None,
            notes: None,
            owes: Deliverable::default(),
            merge: MergeConfig::default(),
            ci: CiConfig::default(),
            protected: protected(),
            max_pushes: max_pushes(),
            max_repairs: max_repairs(),
            max_change_bytes: max_change_bytes(),
            deps: None,
            evidence: None,
        }
    }
}

/// `forge.repos.<repo>.deps`: the host builds, from the base branch's
/// lockfile and package manifests alone, a directory of installed dependencies,
/// and each Ticket's `run_code` mounts it read-only at the repository's
/// `node_modules` paths. The build runs in its own sandbox that reaches the
/// network (the only place on the host's behalf that does besides the
/// browser), with no home, token, vault or agent state, no lifecycle script
/// (so no code of the repository runs), and the default npm registry only: a
/// repository's `bunfig.toml` and `.npmrc` are never read.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepsConfig {
    pub kind: DepsKind,
    /// The package manager's executable, an absolute path. For `bun`, by
    /// default `$HOME/.bun/bin/bun`. It is bound read-only into the build
    /// sandbox and, as `bun` and as `node`, into every Ticket's.
    #[serde(default)]
    pub binary: Option<PathBuf>,
    /// The longest a build may run.
    #[serde(default = "deps_timeout_secs")]
    pub timeout_secs: u64,
    /// The most disk a build may use, scratch included; a bigger one is
    /// killed and the dependencies are unavailable.
    #[serde(default = "deps_max_bytes")]
    pub max_bytes: u64,
    /// The address space the repository's commands may use in a Ticket's
    /// `run_code`, in place of `executor.run_code.memory_bytes` where that is
    /// lower: bun (JavaScriptCore) reserves a great deal of it and aborts
    /// under about 8 GiB. A cap on address space, not on memory used, and
    /// only for Tickets on this repository.
    #[serde(default = "deps_memory_bytes")]
    pub memory_bytes: u64,
}

/// Which package manager installs a repository's dependencies. One variant
/// each: the key, the files it reads and the sandbox's tools are its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepsKind {
    /// `bun install --frozen-lockfile --ignore-scripts` from `bun.lock`.
    Bun,
}

impl DepsKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bun => "bun",
        }
    }
}

/// The range of `deps.timeout_secs`.
pub const DEPS_TIMEOUT_RANGE: std::ops::RangeInclusive<u64> = 10..=3_600;
/// The range of `deps.max_bytes`.
pub const DEPS_BYTES_RANGE: std::ops::RangeInclusive<u64> = 1 << 20..=64 << 30;

fn deps_timeout_secs() -> u64 {
    600
}

fn deps_max_bytes() -> u64 {
    8 << 30
}

fn deps_memory_bytes() -> u64 {
    16 << 30
}

impl DepsConfig {
    /// The package manager's executable: `binary`, or its usual place under
    /// `$HOME`.
    pub fn binary_path(&self) -> Option<PathBuf> {
        if let Some(binary) = &self.binary {
            return Some(binary.clone());
        }
        let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
        match self.kind {
            DepsKind::Bun => Some(PathBuf::from(home).join(".bun/bin/bun")),
        }
    }

    fn validate(&self) -> Result<()> {
        let binary = self.binary_path();
        ensure!(
            binary.as_ref().is_some_and(|binary| binary.is_absolute()),
            "binary must be an absolute path (it defaults to $HOME/.bun/bin/bun, and HOME is not set)"
        );
        ensure!(
            DEPS_TIMEOUT_RANGE.contains(&self.timeout_secs),
            "timeout_secs must be {}..={}",
            DEPS_TIMEOUT_RANGE.start(),
            DEPS_TIMEOUT_RANGE.end()
        );
        ensure!(
            DEPS_BYTES_RANGE.contains(&self.max_bytes),
            "max_bytes must be 1 MiB..=64 GiB"
        );
        ensure!(
            code::MEMORY_RANGE.contains(&self.memory_bytes),
            "memory_bytes must be 256 MiB..=16 GiB"
        );
        Ok(())
    }
}

/// `forge.repos.<repo>.evidence`: the repository's checks want proof of a
/// command in every pull request. The worker is told to run `command` in
/// `/work` once everything is committed and to put its JSON under `heading`
/// in its ready report, which is the pull request's description. The host
/// refuses a ready that opens a pull request without it: the heading, then a
/// ```json fence whose object says `ok: true` and does not say `dirty: true`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceConfig {
    /// A shell command, such as `bun run solos dev verify --scope unit --json`.
    pub command: String,
    #[serde(default = "evidence_heading")]
    pub heading: String,
}

fn evidence_heading() -> String {
    "## Evidence".into()
}

impl EvidenceConfig {
    fn validate(&self) -> Result<()> {
        let one_line = |text: &str| !text.trim().is_empty() && !text.chars().any(char::is_control);
        ensure!(one_line(&self.command), "command must be one line of text");
        ensure!(
            one_line(&self.heading) && self.heading.trim_start_matches('#').starts_with(' '),
            "heading must be a markdown heading on one line, such as \"## Evidence\""
        );
        Ok(())
    }
}

impl RepoConfig {
    /// What an accept on this repository grants about the forge, fixed in the
    /// Ticket's Mandate: config edits never change work in flight.
    pub fn terms(&self) -> ForgeTerms {
        ForgeTerms {
            merge: match self.merge {
                MergeConfig::Off => MergeMode::Off,
                MergeConfig::Ask => MergeMode::Ask,
                MergeConfig::Auto => MergeMode::Auto,
            },
            ci_expected: self.ci == CiConfig::Expected,
            max_pushes: self.max_pushes,
            max_repairs: self.max_repairs,
        }
    }
}

/// `forge.repos.<repo>.merge`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeConfig {
    /// The agent never merges; people do, on GitHub.
    Off,
    /// A person says yes, by a click or a review.
    #[default]
    Ask,
    /// Only for an approver's own request, once CI passed.
    Auto,
}

/// `forge.repos.<repo>.ci`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiConfig {
    #[default]
    Expected,
    /// The repository has no CI to wait for.
    None,
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

/// The most `max_change_bytes` may be.
const MAX_CHANGE_BYTES_LIMIT: u64 = 50_000_000;

/// pstack in every worker: its first prompt enters `entry`, its system
/// prompt carries the adapter, and it may `read` the tree.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PstackConfig {
    /// The skill every worker enters, a directory under
    /// `vendor/pstack/skills`.
    #[serde(default = "pstack_entry")]
    pub entry: String,
}

/// Whether `prepare` runs `git worktree add` into the worker's work dir,
/// whose `.git` would then point outside every sandbox.
fn worktree_into_work(prepare: &str) -> bool {
    let words: Vec<&str> = prepare.split_whitespace().collect();
    words.windows(2).any(|pair| pair == ["worktree", "add"]) && prepare.contains("SOLOS_WORK_DIR")
}

/// A branch name `git clone --branch` takes as one: no option, no range, no
/// path that climbs.
fn branch_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
}

fn pstack_entry() -> String {
    "poteto-mode".to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceConfig {
    /// Trigger JSON, one per line.
    Jsonl { path: PathBuf },
    /// Slack over Socket Mode; its Triggers name source `slack`. Only events
    /// from `channels` enter. Each token file is read once and deleted.
    Slack {
        /// Slack channel ids; a direct message's `D` id can be listed too.
        channels: BTreeSet<String>,
        /// The app-level token (`xapp-`, scope `connections:write`).
        app_token_file: PathBuf,
        /// The bot token (`xoxb-`), which the `slack` channel uses too.
        bot_token_file: PathBuf,
        #[serde(default = "slack_api_base")]
        api_base: String,
    },
    /// Discord over the Gateway; its Triggers name source `discord`. Only
    /// events from `guilds` and `channels` enter, and so do those in a thread
    /// of a listed channel. The token file is read once and deleted.
    Discord {
        /// Discord guild ids, as the developer-mode "Copy Server ID" gives.
        #[serde(default, deserialize_with = "snowflakes")]
        guilds: BTreeSet<String>,
        /// Discord channel ids; a direct message's channel id can be listed
        /// too.
        #[serde(default, deserialize_with = "snowflakes")]
        channels: BTreeSet<String>,
        /// The bot token, which the `discord` channel uses too.
        token_file: PathBuf,
        #[serde(default = "discord_api_base")]
        api_base: String,
    },
    /// GitHub over REST polling; its Triggers name source `github`. It
    /// takes no fields: it reads `forge.repos` as the `github:` section's App.
    Github {},
    /// Named schedules that fire as System Triggers of source `schedule`.
    Schedule { schedules: Vec<ScheduleConfig> },
    /// Requests that operators file with `solos-swarm mcp --state-dir`, as
    /// Triggers of source `mcp`, from an inbox in the state directory.
    Mcp {},
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Snowflake {
    Text(String),
    Number(u64),
}

impl From<Snowflake> for String {
    fn from(id: Snowflake) -> Self {
        match id {
            Snowflake::Text(text) => text,
            Snowflake::Number(number) => number.to_string(),
        }
    }
}

fn snowflakes<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeSet<String>, D::Error> {
    let ids = Vec::<Snowflake>::deserialize(deserializer)?;
    Ok(ids.into_iter().map(String::from).collect())
}

fn snowflake<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Snowflake::deserialize(deserializer).map(String::from)
}

fn optional_snowflake<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(Option::<Snowflake>::deserialize(deserializer)?.map(String::from))
}

fn slack_api_base() -> String {
    slack::DEFAULT_API_BASE.to_owned()
}

fn discord_api_base() -> String {
    discord::DEFAULT_API_BASE.to_owned()
}

fn github_api_base() -> String {
    github::DEFAULT_API_BASE.to_owned()
}

fn github_poll_secs() -> u64 {
    60
}

fn github_reconcile_secs() -> u64 {
    300
}

fn github_writes() -> bool {
    true
}

fn github_git_base() -> String {
    github::DEFAULT_GIT_BASE.to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChannelConfig {
    /// Appends each message to `path`. Serves replies whose `ChannelRef`
    /// names `source`.
    Jsonl { source: String, path: PathBuf },
    /// Posts in Slack threads with the bot token and API base of the
    /// `slack` Source. Serves replies to source `slack`.
    Slack {},
    /// Posts in Discord with the bot token and API base of the `discord`
    /// Source. Serves replies to source `discord`.
    Discord {},
    /// Comments on issues and pull requests as the `github:` section's App.
    /// Serves replies to source `github`.
    Github {},
    /// Appends replies to an outbox in the state directory, which
    /// `solos-swarm mcp --state-dir` reads. Serves replies to source `mcp`.
    Mcp {},
}

impl ChannelConfig {
    pub fn source(&self) -> &str {
        match self {
            Self::Jsonl { source, .. } => source,
            Self::Slack {} => slack::SOURCE,
            Self::Discord {} => discord::SOURCE,
            Self::Github {} => github::SOURCE,
            Self::Mcp {} => mcp::SOURCE,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouterConfig {
    Scripted {
        /// Ignore an explicit candidate on a Thread whose Ticket is still
        /// open, as a follow-up that Ticket already has.
        #[serde(default)]
        ignore_follow_ups: bool,
    },
    /// A long-lived Pi session, keyless and sandboxed, whose model calls go
    /// through the agent broker against the Router's daily cap.
    Pi {
        /// An admitted upstream, by the names `solos-swarm run --provider`
        /// takes: `deepseek` or `venice`.
        provider: String,
        model: String,
        prices: PriceTable,
        #[serde(default = "default_max_input_bytes")]
        max_input_bytes: usize,
        #[serde(default = "default_max_output_tokens")]
        max_output_tokens: u64,
        /// The most candidates one prompt holds, oldest first; the rest wait
        /// for the next turn. A reply decides each of them, so this many
        /// decisions (an accept runs to about 300 tokens) must fit in
        /// `max_output_tokens`.
        #[serde(default = "default_max_batch")]
        max_batch: usize,
        /// Replaces the built-in instructions; the budgets and rules are
        /// still added after them.
        #[serde(default)]
        instructions: Option<String>,
        /// What each enabled proactive rule (`policy.rules`) means, by name.
        #[serde(default)]
        rules: BTreeMap<String, String>,
    },
}

fn default_max_input_bytes() -> usize {
    1_048_576
}

fn default_max_output_tokens() -> u64 {
    8_192
}

/// DeepSeek's V4 model card recommends 1.0 and its API defaults to 1; its
/// older table gives 0.0 for code, whose exact tool arguments (a path, a line
/// range) a high temperature garbles. 0.6 is the low end of the 0.6 to 1.0
/// that agentic runs use: enough randomness to leave a loop, and low enough
/// for a command to come out as meant.
fn default_temperature() -> Temperature {
    Temperature::WORKER
}

fn default_max_batch() -> usize {
    6
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutorConfig {
    Scripted {},
    /// Work that only takes time, for `agent bench`: each Assignment starts
    /// when a slot is free and reports `ready` after its request's
    /// `meta.bench.work_ms`, scaled by `time_factor`.
    Simulated {
        #[serde(default = "default_simulated_workers")]
        max_workers: usize,
        #[serde(default = "default_time_factor")]
        time_factor: TimeFactor,
        /// The work of a Ticket whose origin names none, in workload ms.
        #[serde(default = "default_work_ms")]
        default_work_ms: u64,
        /// The Router takes no Candidate while work runs, as one session
        /// that runs its subagent in its own turn.
        #[serde(default)]
        router_waits_for_work: bool,
    },
    /// One keyless, sandboxed Pi worker session per Assignment, whose model
    /// calls go through the agent broker against its Ticket's budget.
    Pi {
        /// As the Router's; when both are Pi they name the same provider,
        /// since one credential serves both.
        provider: String,
        model: String,
        prices: PriceTable,
        #[serde(default = "default_max_input_bytes")]
        max_input_bytes: usize,
        #[serde(default = "default_max_output_tokens")]
        max_output_tokens: u64,
        /// What the workers' model calls are sampled at, 0 to 2. At 0 the
        /// model decodes greedily, and a greedy model goes round in circles
        /// when it is stuck (one worker made 1,107 calls in a five-call
        /// circle); some randomness lets it leave one. The Router judges at 0
        /// whatever this is, and so does swarm mode.
        #[serde(default = "default_temperature")]
        temperature: Temperature,
        /// Worker sessions that may live at once; a launch past it waits.
        #[serde(default = "default_max_workers")]
        max_workers: usize,
        /// A shell command run once per Ticket in its directory before its
        /// first worker, with `SOLOS_TICKET` and `SOLOS_WORK_DIR` (the
        /// worker's `/work`) set, such as a `git worktree add`.
        #[serde(default)]
        prepare: Option<String>,
        /// The workers' `run_code`. Without it they have none.
        #[serde(default)]
        run_code: Option<Box<RunCodeConfig>>,
        /// What the workers' `fetch_url` may reach.
        #[serde(default)]
        fetch: FetchConfig,
        /// How `Plan::Swarm` Tickets run. Without it they fail.
        #[serde(default)]
        swarm: Option<Box<SwarmConfig>>,
        /// The harness of a session Ticket whose Plan names none.
        #[serde(default)]
        harness: HarnessKind,
        /// Where Claude Code is. Without it no session runs on Claude Code.
        #[serde(default)]
        claude: Option<Box<ClaudeConfig>>,
        /// Where a session's commands run when its Plan names nowhere: a
        /// node in `nodes.allow`, or `local` (the default).
        #[serde(default)]
        environment: Option<String>,
    },
}

/// The harness a worker session runs on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessKind {
    #[default]
    Pi,
    ClaudeCode,
}

impl HarnessKind {
    /// The harness a Plan names.
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "pi" => Some(Self::Pi),
            "claude-code" => Some(Self::ClaudeCode),
            _ => None,
        }
    }
}

/// The `claude-code` harness's install. Every path is absolute.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeConfig {
    /// The `claude` executable, or with `node`, the npm package's `cli.js`.
    pub binary: PathBuf,
    /// The node that runs `binary`; without it `binary` runs on its own, as
    /// the native build does.
    #[serde(default)]
    pub node: Option<PathBuf>,
    /// The solos-swarm the sandbox runs for the session's tools and model
    /// bridge; by default the one serving.
    #[serde(default)]
    pub solos_swarm: Option<PathBuf>,
}

/// `Plan::Swarm` Tickets: each runs a governed swarm whose Goal is rendered
/// from a template, on the workers' provider and credential.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmConfig {
    /// Goal templates by name, each a YAML file relative to this file.
    pub templates: BTreeMap<String, PathBuf>,
    /// The template of a swarm whose Plan names none.
    #[serde(default)]
    pub default_template: Option<String>,
    /// Swarms that may run at once; a launch past it waits. Each swarm is
    /// many sessions, so this is apart from `max_workers`.
    #[serde(default = "default_max_swarms")]
    pub max_runs: usize,
    /// What a swarm accept that names no budget gets, before
    /// `budgets.session_microusd`. A swarm always has a budget, so with
    /// neither set, an accept must name one.
    #[serde(default)]
    pub default_budget_microusd: Option<MicroUsd>,
}

/// What a worker's `fetch_url` may reach. A worker reads pages, issues and
/// repositories whose text can tell it to fetch any URL, and this host may run
/// other services on loopback and private addresses, so by default it
/// reaches the public internet only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchConfig {
    /// Off (the default), `fetch_url` refuses a request, and every redirect
    /// hop, whose host resolves to a loopback, private (RFC 1918),
    /// link-local (including the cloud metadata address), shared (100.64/10,
    /// which Tailscale uses), unique-local IPv6, unspecified or multicast
    /// address. On, workers may reach those, such as a service on this
    /// network that they are meant to read. The browser has its own
    /// `browser.allow_private_networks`.
    #[serde(default)]
    pub allow_private_networks: bool,
}

/// What a worker's `run_code` sandbox mounts and how many run at once.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCodeConfig {
    /// Read-only binds, each `{target: /name, source: /absolute/host/path}`.
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// A uv Python store that a mounted venv's interpreter links into,
    /// bound read-only at its own path.
    #[serde(default)]
    pub uv_root: Option<PathBuf>,
    /// Sandboxes at once, across all workers.
    #[serde(default = "default_code_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_code_memory")]
    pub memory_bytes: u64,
    /// A `run_code` still running after this many seconds moves to the
    /// background as a job.
    #[serde(default = "default_background_after_secs")]
    pub background_after_secs: u64,
    /// Background jobs one Ticket may hold whose end has not reached its
    /// worker.
    #[serde(default = "default_max_jobs")]
    pub max_jobs: usize,
    /// The longest a `job_start` job may run, in seconds.
    #[serde(default = "default_job_max_secs")]
    pub job_max_secs: u64,
    /// What a `job_start` job keeps of each output stream until it is read.
    #[serde(default = "default_job_output_bytes")]
    pub job_output_bytes: usize,
}

impl RunCodeConfig {
    pub fn job_limits(&self) -> JobLimits {
        JobLimits {
            background_after: Duration::from_secs(self.background_after_secs),
            max_jobs: self.max_jobs,
            max_wall: Duration::from_secs(self.job_max_secs),
            output_bytes: self.job_output_bytes,
        }
    }
}

fn default_background_after_secs() -> u64 {
    jobs::DEFAULT_BACKGROUND_AFTER_SECS
}

fn default_max_jobs() -> usize {
    jobs::DEFAULT_MAX_JOBS
}

fn default_job_max_secs() -> u64 {
    jobs::DEFAULT_JOB_MAX_SECS
}

fn default_job_output_bytes() -> usize {
    jobs::DEFAULT_JOB_OUTPUT_BYTES
}

/// The jobs one Ticket may hold.
const MAX_JOBS: std::ops::RangeInclusive<usize> = 1..=8;
/// What a job may keep of a stream: the Pi extension reads at most 4 MiB
/// of a tool reply.
const JOB_OUTPUT_BYTES: std::ops::RangeInclusive<usize> = 1_024..=1_048_576;

fn default_max_workers() -> usize {
    2
}

fn default_simulated_workers() -> usize {
    MAX_WORKERS
}

fn default_time_factor() -> TimeFactor {
    TimeFactor::REAL_TIME
}

fn default_work_ms() -> u64 {
    600_000
}

fn default_max_swarms() -> usize {
    1
}

fn default_code_concurrency() -> usize {
    code::DEFAULT_CONCURRENCY
}

fn default_code_memory() -> u64 {
    code::DEFAULT_MEMORY_BYTES
}

/// The most workers one agent may run at once.
const MAX_WORKERS: usize = 64;
/// The longest simulated work.
const DAY_MS: u64 = 86_400_000;
/// The most swarms one agent may run at once.
const MAX_SWARMS: usize = 8;
/// The budget a Goal template is rendered with to check it, where config
/// names no swarm budget.
const SAMPLE_SWARM_BUDGET_MICROUSD: MicroUsd = 500_000;

/// What the shape from before the GitHub App said, which no longer exists.
/// Serde would answer "unknown field" and not say what replaced it, so the
/// raw YAML is read first.
fn refuse_token_files(yaml: &serde_yaml::Value) -> Result<()> {
    fn kind(value: &serde_yaml::Value) -> Option<&str> {
        value.get("kind").and_then(serde_yaml::Value::as_str)
    }
    let sources = yaml
        .get("sources")
        .and_then(serde_yaml::Value::as_sequence)
        .into_iter()
        .flatten();
    for source in sources.filter(|source| kind(source) == Some("github")) {
        ensure!(
            source.get("token_file").is_none(),
            "the github source's token_file is gone, since the agent acts as a GitHub App whose \
             key is github.key_file: see {GITHUB_APP_DOC}"
        );
        for field in ["repos", "api_base", "poll_secs"] {
            ensure!(
                source.get(field).is_none(),
                "the github source no longer takes {field}, since its repositories are forge.repos \
                 and its poll_secs and api_base are in the github: section: see {GITHUB_APP_DOC}"
            );
        }
    }
    if let Some(forge) = yaml
        .get("forge")
        .filter(|forge| kind(forge) == Some("github"))
    {
        for field in ["token_file", "identity"] {
            ensure!(
                forge.get(field).is_none(),
                "forge: github no longer takes {field}, since the GitHub App replaced token files \
                 and the clone's identity is the App's bot: see {GITHUB_APP_DOC}"
            );
        }
    }
    Ok(())
}

/// The agent's caps. Each is optional: where one is absent, what it would
/// cap is not capped, though its model calls are still held and settled in
/// the ledger, so the spend is recorded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    /// What the Router gives a `Plan::Session` Ticket. Absent: a session
    /// Ticket has no budget, so it needs no approval and its account has no
    /// cap. A swarm always has a budget (`executor.swarm.default_budget_microusd`
    /// or the Router's), which this is the fallback for.
    #[serde(default)]
    pub session_microusd: Option<MicroUsd>,
    /// The most a Pi Router's model calls may commit in one UTC day. Absent:
    /// no daily cap.
    #[serde(default)]
    pub router_daily_microusd: Option<MicroUsd>,
}

impl AgentConfig {
    /// Read and validate `path`. Relative paths in it are relative to its
    /// directory.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        if let Ok(raw) = serde_yaml::from_slice::<serde_yaml::Value>(&bytes) {
            refuse_token_files(&raw)?;
        }
        let mut config: Self = serde_yaml::from_slice(&bytes)
            .with_context(|| format!("invalid agent config {}", path.display()))?;
        config.validate()?;
        let base = path.parent().unwrap_or(Path::new("."));
        for source in &mut config.sources {
            match source {
                SourceConfig::Jsonl { path } => *path = base.join(&*path),
                SourceConfig::Slack {
                    app_token_file,
                    bot_token_file,
                    ..
                } => {
                    *app_token_file = base.join(&*app_token_file);
                    *bot_token_file = base.join(&*bot_token_file);
                }
                SourceConfig::Discord { token_file, .. } => *token_file = base.join(&*token_file),
                SourceConfig::Github {} | SourceConfig::Schedule { .. } | SourceConfig::Mcp {} => {}
            }
        }
        for channel in &mut config.channels {
            match channel {
                ChannelConfig::Jsonl { path, .. } => *path = base.join(&*path),
                ChannelConfig::Slack {}
                | ChannelConfig::Discord {}
                | ChannelConfig::Github {}
                | ChannelConfig::Mcp {} => {}
            }
        }
        if let Some(nodes) = &mut config.nodes {
            nodes.resolve(base);
        }
        if let Some(vault) = &mut config.vault {
            vault.resolve(base);
        }
        if let Some(browser) = &mut config.browser {
            browser.lightpanda = base.join(&browser.lightpanda);
            browser.audit_key = base.join(&browser.audit_key);
            browser.runtime_dir = browser.runtime_dir.as_ref().map(|dir| base.join(dir));
        }
        config.trail.path = config.trail.path.as_ref().map(|path| base.join(path));
        match &mut config.forge {
            Some(ForgeConfig::Local { remote, .. }) => *remote = base.join(&*remote),
            Some(ForgeConfig::Github { .. }) | None => {}
        }
        if let Some(github) = &mut config.github {
            github.key_file = base.join(&github.key_file);
        }
        if let Some(publish) = config.web.as_mut().and_then(|web| web.publish.as_mut()) {
            publish.token_file = base.join(&publish.token_file);
        }
        // GitHub logins are case-blind, and the Policy spells them in lower
        // case, as the Source does.
        let policy = &mut config.policy;
        for who in [&mut policy.people, &mut policy.approvers, &mut policy.bots] {
            *who = std::mem::take(who)
                .into_iter()
                .map(|entry| match entry.strip_prefix("github:") {
                    Some(login) => format!("github:{}", login.to_ascii_lowercase()),
                    None => entry,
                })
                .collect();
        }
        if let ExecutorConfig::Pi {
            provider,
            swarm: Some(swarm),
            ..
        } = &mut config.executor
        {
            for path in swarm.templates.values_mut() {
                *path = base.join(&*path);
            }
            let sample = swarm
                .default_budget_microusd
                .or(config.budgets.session_microusd)
                .unwrap_or(SAMPLE_SWARM_BUDGET_MICROUSD);
            GoalTemplates::load(swarm, provider)?.check(sample)?;
        }
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.sources.is_empty(), "the agent config names no source");
        let mut served = BTreeSet::new();
        for channel in &self.channels {
            ensure!(
                served.insert(channel.source()),
                "two channels serve source {}",
                channel.source()
            );
        }
        let mut slack_sources = 0;
        for source in &self.sources {
            if let SourceConfig::Slack { channels, .. } = source {
                slack_sources += 1;
                ensure!(
                    !channels.is_empty(),
                    "the slack source listens to no channel"
                );
            }
        }
        ensure!(
            slack_sources <= 1,
            "the agent config names two slack sources"
        );
        ensure!(
            !self.slack_channel() || slack_sources == 1,
            "the slack channel needs the slack source's bot token"
        );
        self.validate_discord()?;
        self.validate_github()?;
        if let Some(web) = &self.web {
            TicketLinks::new(&web.public_url)?;
            web.validate()?;
        }
        let mcp_sources = self
            .sources
            .iter()
            .filter(|source| matches!(source, SourceConfig::Mcp {}))
            .count();
        ensure!(mcp_sources <= 1, "the agent config names two mcp sources");
        ensure!(
            mcp_sources == 0
                || self
                    .channels
                    .iter()
                    .any(|channel| matches!(channel, ChannelConfig::Mcp {})),
            "the mcp source needs the mcp channel, or nobody hears back"
        );
        self.validate_schedules(&served)?;
        self.validate_watchdog(&served)?;
        self.validate_health()?;
        self.validate_nodes()?;
        ensure!(
            self.browser.is_none() || matches!(self.executor, ExecutorConfig::Pi { .. }),
            "browser needs executor: pi, whose workers have its tools"
        );
        ensure!(
            self.browser
                .as_ref()
                .is_none_or(|browser| browser.idle_secs > 0),
            "browser.idle_secs must be above zero"
        );
        ensure!(
            self.budgets.session_microusd != Some(0),
            "budgets.session_microusd must be above zero, or absent for no cap"
        );
        ensure!(
            self.budgets.router_daily_microusd != Some(0),
            "budgets.router_daily_microusd must be above zero, or absent for no cap"
        );
        if let Some(session) = self.budgets.session_microusd
            && session > self.policy.auto_approve_microusd
        {
            let gated = "budgets.session_microusd is above policy.auto_approve_microusd, so every \
                         Ticket waits for an approval";
            ensure!(
                self.slack_channel() || self.discord_channel(),
                "{gated}, and only the slack and discord channels' buttons can take one"
            );
            ensure!(
                self.takes_approvals(),
                "{gated}, and policy.approvers names nobody on a source whose channel has buttons"
            );
        }
        if let ExecutorConfig::Pi {
            swarm: Some(swarm), ..
        } = &self.executor
            && let Some(budget) = swarm.default_budget_microusd
        {
            ensure!(
                self.takes_approvals() || budget <= self.policy.auto_approve_microusd,
                "executor.swarm.default_budget_microusd {budget} is above \
                 policy.auto_approve_microusd, and nobody here can approve a budget"
            );
        }
        if let RouterConfig::Pi {
            provider,
            model,
            prices,
            max_input_bytes,
            max_output_tokens,
            max_batch,
            rules,
            ..
        } = &self.router
        {
            upstream_profile(provider, model).context("router")?;
            prices.validate().context("router.prices")?;
            ensure!(
                (1..=16_777_216).contains(max_input_bytes)
                    && (1..=393_216).contains(max_output_tokens),
                "router.max_input_bytes or router.max_output_tokens is out of range"
            );
            ensure!(
                (1..=64).contains(max_batch),
                "router.max_batch must be 1..=64"
            );
            for name in rules.keys() {
                ensure!(
                    self.policy.rules.contains(name),
                    "router rule {name} is not an enabled policy rule"
                );
            }
        }
        self.validate_pstack()?;
        self.validate_forge()?;
        self.validate_memory()?;
        self.validate_executor()
    }

    fn validate_memory(&self) -> Result<()> {
        let memory = &self.memory;
        ensure!(
            memory.daily_microusd != Some(0),
            "memory.daily_microusd must be above zero, or absent for no cap"
        );
        let ExecutorConfig::Pi {
            provider, model, ..
        } = &self.executor
        else {
            ensure!(
                memory.review == ReviewConfig::default() && memory.daily_microusd.is_none(),
                "memory.review and memory.daily_microusd are for workers on executor: pi"
            );
            return Ok(());
        };
        let review = &memory.review;
        if let Some(named) = &review.provider {
            ensure!(
                named == provider,
                "memory.review.provider must be the workers' provider, {provider}: one credential \
                 serves both"
            );
        }
        let model = review.model.as_deref().unwrap_or(model);
        upstream_profile(provider, model).context("memory.review")?;
        if let Some(prices) = &review.prices {
            prices.validate().context("memory.review.prices")?;
        }
        ensure!(
            review
                .max_output_tokens
                .is_none_or(|max| (1..=393_216).contains(&max)),
            "memory.review.max_output_tokens must be 1..=393216"
        );
        ensure!(
            memory
                .dream
                .max_output_tokens
                .is_none_or(|max| (1..=393_216).contains(&max)),
            "memory.dream.max_output_tokens must be 1..=393216"
        );
        Ok(())
    }

    /// When the daily dream is asked for: where workers run on Pi and
    /// propose memos, unless `memory.dream.enabled` is false. Its slot is
    /// remembered in `state_dir`, so a restart in the same day asks for
    /// nothing new.
    pub fn dream_schedule(&self, state_dir: &Path) -> Option<DreamSchedule> {
        let dreams = matches!(self.executor, ExecutorConfig::Pi { .. })
            && self.memory.proposals
            && self.memory.dream.enabled;
        dreams.then(|| {
            let mut schedule = DreamSchedule::daily(self.memory.dream.at.0);
            schedule.marker = Some(state_dir.join(dream::SLOT_FILE));
            schedule
        })
    }

    /// The review of workers' memos and its account, where workers run on
    /// Pi and propose memos: each of the review's fields the workers' unless
    /// `memory.review` sets it, with thinking on at `memory.review.thinking`.
    pub fn memory_parts(&self) -> Result<Option<MemoryParts>> {
        let ExecutorConfig::Pi {
            provider,
            model,
            prices,
            max_input_bytes,
            ..
        } = &self.executor
        else {
            return Ok(None);
        };
        if !self.memory.proposals {
            return Ok(None);
        }
        let review = &self.memory.review;
        let effort = review.thinking;
        let terms = ModelTerms {
            upstream: upstream_profile(provider, review.model.as_deref().unwrap_or(model))?,
            prices: review.prices.unwrap_or(*prices),
            max_input_bytes: *max_input_bytes,
            max_output_tokens: review
                .max_output_tokens
                .unwrap_or(effort.max_output_tokens()),
            call_limit: Some(REVIEW_CALL_LIMIT),
            temperature: None,
            thinking: None,
        };
        let dream = &self.memory.dream;
        let dream = dream.enabled.then(|| DreamTerms {
            effort: dream.thinking,
            max_output_tokens: dream
                .max_output_tokens
                .unwrap_or(dream.thinking.max_output_tokens()),
        });
        Ok(Some(MemoryParts {
            review: ReviewTerms { terms, effort },
            daily_microusd: self.memory.daily_microusd,
            dream,
        }))
    }

    fn validate_forge(&self) -> Result<()> {
        let Some(forge) = &self.forge else {
            return Ok(());
        };
        let ExecutorConfig::Pi {
            run_code, prepare, ..
        } = &self.executor
        else {
            bail!("forge needs executor: {{kind: pi}}, whose workers work in its clones");
        };
        ensure!(
            run_code.is_some(),
            "forge needs executor.run_code, where workers run git in their clones"
        );
        ensure!(
            !prepare.as_deref().is_some_and(worktree_into_work),
            "executor.prepare runs git worktree add into the work dir, and the forge clones each \
             repository ticket's work dir itself: drop the worktree add"
        );
        if let Some(Identity { name, email }) = forge.identity() {
            for (field, value) in [("name", name), ("email", email)] {
                ensure!(
                    !value.trim().is_empty() && !value.chars().any(char::is_control),
                    "forge.identity.{field} must be one line of text"
                );
            }
        }
        ensure!(!forge.repos().is_empty(), "forge names no repos");
        for (repo, terms) in forge.repos() {
            ensure!(
                !terms.about.trim().is_empty(),
                "forge.repos.{repo}.about is empty, and the Router reads it"
            );
            ensure!(
                branch_name(&terms.base),
                "forge.repos.{repo}.base {:?} is not a branch name",
                terms.base
            );
            if let Some(budget) = terms.budget_microusd {
                ensure!(budget > 0, "forge.repos.{repo}.budget_microusd is zero");
                ensure!(
                    self.takes_approvals() || budget <= self.policy.auto_approve_microusd,
                    "forge.repos.{repo}.budget_microusd {budget} is above \
                     policy.auto_approve_microusd, and nobody here can approve a budget"
                );
            }
            for glob in &terms.protected {
                if let Err(why) = super::forge::valid_glob(glob) {
                    bail!("forge.repos.{repo}.protected {glob:?} is not a path glob: {why}");
                }
            }
            if let Some(deps) = &terms.deps {
                deps.validate()
                    .with_context(|| format!("forge.repos.{repo}.deps"))?;
            }
            if let Some(evidence) = &terms.evidence {
                evidence
                    .validate()
                    .with_context(|| format!("forge.repos.{repo}.evidence"))?;
                ensure!(
                    terms.owes == Deliverable::PullRequest,
                    "forge.repos.{repo}.evidence is for a pull request's description, and owes is \
                     not pull_request"
                );
            }
            ensure!(
                terms.max_pushes >= 1,
                "forge.repos.{repo}.max_pushes must be at least 1"
            );
            ensure!(
                (1..=MAX_CHANGE_BYTES_LIMIT).contains(&terms.max_change_bytes),
                "forge.repos.{repo}.max_change_bytes must be 1..={MAX_CHANGE_BYTES_LIMIT}"
            );
            if terms.owes == Deliverable::PullRequest {
                ensure!(
                    matches!(forge, ForgeConfig::Github { .. }),
                    "forge.repos.{repo}.owes is pull_request, and a local forge has no pull \
                     requests: use forge: github, or owes: change to keep commits in the clone"
                );
                ensure!(
                    self.github.as_ref().is_none_or(|github| github.writes),
                    "forge.repos.{repo}.owes is pull_request, and github.writes is false, so \
                     nothing could be pushed: use owes: answer, or turn writes on"
                );
            }
            if terms.merge == MergeConfig::Auto {
                ensure!(
                    terms.ci == CiConfig::Expected,
                    "forge.repos.{repo}.merge is auto, which needs ci: expected: a repository \
                     with no CI never merges by itself"
                );
                ensure!(
                    !self.policy.approvers.is_empty(),
                    "forge.repos.{repo}.merge is auto, which acts only for an approver's own \
                     requests, and policy.approvers names nobody"
                );
            }
        }
        Ok(())
    }

    fn validate_pstack(&self) -> Result<()> {
        if self.pstack.is_none() {
            return Ok(());
        }
        let ExecutorConfig::Pi { run_code, .. } = &self.executor else {
            bail!("pstack needs executor: {{kind: pi}}, whose workers it enters");
        };
        let mounts = run_code.iter().flat_map(|run_code| &run_code.mounts);
        for mount in mounts {
            ensure!(
                mount.target != Path::new(pstack::MOUNT),
                "executor.run_code mounts {}, where pstack is while pstack is on",
                pstack::MOUNT
            );
        }
        Ok(())
    }

    fn validate_executor(&self) -> Result<()> {
        if let ExecutorConfig::Simulated {
            max_workers,
            default_work_ms,
            ..
        } = &self.executor
        {
            ensure!(
                (1..=MAX_WORKERS).contains(max_workers),
                "executor.max_workers must be 1..={MAX_WORKERS}"
            );
            ensure!(
                (1..=DAY_MS).contains(default_work_ms),
                "executor.default_work_ms must be 1..={DAY_MS}"
            );
        }
        let ExecutorConfig::Pi {
            provider,
            model,
            prices,
            max_input_bytes,
            max_output_tokens,
            max_workers,
            prepare,
            run_code,
            swarm,
            harness,
            claude,
            ..
        } = &self.executor
        else {
            return Ok(());
        };
        upstream_profile(provider, model).context("executor")?;
        prices.validate().context("executor.prices")?;
        ensure!(
            (1..=16_777_216).contains(max_input_bytes) && (1..=393_216).contains(max_output_tokens),
            "executor.max_input_bytes or executor.max_output_tokens is out of range"
        );
        ensure!(
            (1..=MAX_WORKERS).contains(max_workers),
            "executor.max_workers must be 1..={MAX_WORKERS}"
        );
        if let RouterConfig::Pi {
            provider: router, ..
        } = &self.router
        {
            ensure!(
                router == provider,
                "the router and the workers share one credential, so they name one provider, \
                 not {router} and {provider}"
            );
        }
        ensure!(
            prepare
                .as_ref()
                .is_none_or(|command| !command.trim().is_empty()),
            "executor.prepare is empty"
        );
        if let Some(run_code) = run_code {
            let mut targets = BTreeSet::new();
            for mount in &run_code.mounts {
                mount.validate().context("executor.run_code")?;
                ensure!(
                    targets.insert(&mount.target),
                    "executor.run_code mounts {} twice",
                    mount.target.display()
                );
            }
            ensure!(
                run_code
                    .uv_root
                    .as_ref()
                    .is_none_or(|root| root.is_absolute()),
                "executor.run_code.uv_root must be an absolute path"
            );
            ensure!(
                code::CONCURRENCY_RANGE.contains(&run_code.concurrency),
                "executor.run_code.concurrency must be 1..=8"
            );
            ensure!(
                code::MEMORY_RANGE.contains(&run_code.memory_bytes),
                "executor.run_code.memory_bytes must be 256 MiB..=16 GiB"
            );
            ensure!(
                (1..=code::MAX_TIMEOUT_SECONDS).contains(&run_code.background_after_secs),
                "executor.run_code.background_after_secs must be 1..={}",
                code::MAX_TIMEOUT_SECONDS
            );
            ensure!(
                MAX_JOBS.contains(&run_code.max_jobs),
                "executor.run_code.max_jobs must be 1..=8"
            );
            ensure!(
                JobLimits::MAX_WALL_SECS.contains(&run_code.job_max_secs),
                "executor.run_code.job_max_secs must be {}..={}, so a run_code that moves to the \
                 background keeps its own timeout",
                JobLimits::MAX_WALL_SECS.start(),
                JobLimits::MAX_WALL_SECS.end()
            );
            ensure!(
                JOB_OUTPUT_BYTES.contains(&run_code.job_output_bytes),
                "executor.run_code.job_output_bytes must be 1 KiB..=1 MiB"
            );
        }
        if let Some(swarm) = swarm {
            swarm.validate()?;
        }
        ensure!(
            *harness != HarnessKind::ClaudeCode || claude.is_some(),
            "executor.harness claude-code needs executor.claude"
        );
        if let Some(claude) = claude {
            let paths = [
                Some(&claude.binary),
                claude.node.as_ref(),
                claude.solos_swarm.as_ref(),
            ];
            ensure!(
                paths.into_iter().flatten().all(|path| path.is_absolute()),
                "executor.claude's paths must be absolute"
            );
        }
        Ok(())
    }

    /// The GitHub logins the Policy lists, lowercased and without the
    /// `github:` prefix: the accounts whose ids are pinned.
    pub fn github_logins(&self) -> BTreeSet<String> {
        let policy = &self.policy;
        policy
            .people
            .iter()
            .chain(&policy.approvers)
            .chain(&policy.bots)
            .filter_map(|who| who.strip_prefix("github:"))
            .map(str::to_ascii_lowercase)
            .collect()
    }

    /// The provider and model whose credential `--credential-file` holds:
    /// the Pi Router's, else the Pi workers'. `None` when nothing is Pi.
    pub fn pi_upstream(&self) -> Option<(&str, &str)> {
        match (&self.router, &self.executor) {
            (
                RouterConfig::Pi {
                    provider, model, ..
                },
                _,
            )
            | (
                _,
                ExecutorConfig::Pi {
                    provider, model, ..
                },
            ) => Some((provider, model)),
            _ => None,
        }
    }

    /// Whether a person can approve a budget above
    /// `policy.auto_approve_microusd`: only the slack and discord channels'
    /// buttons take approvals, from an approver on the same source.
    pub fn takes_approvals(&self) -> bool {
        !self.approval_sources().is_empty()
    }

    /// The Sources whose Channel here shows a person Allow and Deny and has
    /// an approver: a Ticket that answers on one of them takes approvals.
    pub fn approval_sources(&self) -> BTreeSet<String> {
        [
            (slack::SOURCE, self.slack_channel()),
            (discord::SOURCE, self.discord_channel()),
        ]
        .into_iter()
        .filter(|(source, channel)| *channel && self.approver_on(source))
        .map(|(source, _)| source.to_owned())
        .collect()
    }

    fn slack_channel(&self) -> bool {
        self.channels
            .iter()
            .any(|channel| matches!(channel, ChannelConfig::Slack {}))
    }

    pub(super) fn discord_channel(&self) -> bool {
        self.channels
            .iter()
            .any(|channel| matches!(channel, ChannelConfig::Discord {}))
    }

    fn approver_on(&self, source: &str) -> bool {
        self.policy
            .approvers
            .iter()
            .any(|who| who.split_once(':').is_some_and(|(on, _)| on == source))
    }
}

impl SwarmConfig {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.templates.is_empty(),
            "executor.swarm names no goal template"
        );
        for name in self.templates.keys() {
            ensure!(
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "goal template name {name:?} is not a short word"
            );
        }
        if let Some(name) = &self.default_template {
            ensure!(
                self.templates.contains_key(name),
                "executor.swarm.default_template {name} is not one of its templates"
            );
        }
        ensure!(
            (1..=MAX_SWARMS).contains(&self.max_runs),
            "executor.swarm.max_runs must be 1..={MAX_SWARMS}"
        );
        ensure!(
            self.default_budget_microusd != Some(0),
            "executor.swarm.default_budget_microusd must be above zero"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentConfig, Budgets, ChannelConfig, ClaudeConfig, DepsConfig, DepsKind, EvidenceConfig,
        ExecutorConfig, ForgeConfig, GithubConfig, HarnessKind, JobLimits, RepoConfig,
        RouterConfig, SourceConfig,
    };
    use solos_agent_core::change::{Deliverable, RepoName};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    const VALID: &str = "
policy:
  people: [slack:alice]
  bots: [github:ci]
sources:
  - kind: jsonl
    path: triggers.jsonl
channels:
  - kind: jsonl
    source: slack
    path: out/outbox.jsonl
router: {kind: scripted}
executor: {kind: scripted}
budgets: {session_microusd: 500000}
";

    fn parse(yaml: &str) -> anyhow::Result<AgentConfig> {
        let config: AgentConfig = serde_yaml::from_str(yaml)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn a_valid_config_loads_with_paths_beside_it() {
        let dir = std::env::temp_dir().join(format!("agent-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.yaml");
        std::fs::write(&path, VALID).unwrap();
        let config = AgentConfig::load(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(config.policy.people.contains("slack:alice"));
        assert_eq!(config.budgets.session_microusd, Some(500_000));
        assert_eq!(
            config.sources,
            [SourceConfig::Jsonl {
                path: dir.join("triggers.jsonl")
            }]
        );
        assert_eq!(
            config.channels,
            [ChannelConfig::Jsonl {
                source: "slack".into(),
                path: dir.join("out/outbox.jsonl")
            }]
        );
    }

    #[test]
    fn the_web_section_takes_an_https_public_url_and_defaults_the_rest() {
        assert!(parse(VALID).unwrap().web.is_none());
        let with = |section: &str| parse(&format!("{VALID}{section}\n"));
        let config = with("web: {public_url: 'https://agent.example.com'}").unwrap();
        let web = config.web.unwrap();
        assert_eq!(web.public_url, "https://agent.example.com");
        assert_eq!(web.publish, None);
        let full = with("web:\n  public_url: https://example.com/agent")
            .unwrap()
            .web
            .unwrap();
        assert_eq!(full.public_url, "https://example.com/agent");

        for (bad, why) in [
            ("web: {public_url: 'http://agent.example.com'}", "https"),
            (
                "web: {public_url: 'https://agent.example.com/'}",
                "end with a slash",
            ),
            ("web: {public_url: 'agent.example.com'}", "not a URL"),
            (
                "web: {public_url: 'https://agent.example.com?x=1'}",
                "query",
            ),
            ("web: {public_url: 'https://a.example/x)(y'}", "()"),
            ("web: {public_url: ''}", "1 to"),
            ("web: {}", "public_url"),
            ("web: {public_url: 'https://a.example', port: 1}", "port"),
        ] {
            let err = with(bad).unwrap_err();
            assert!(
                format!("{err:#}").contains(why),
                "{bad}: expected {why:?} in {err:#}"
            );
        }
    }

    #[test]
    fn a_browser_loads_with_its_paths_beside_the_config_and_needs_pi_workers() {
        let pi = VALID.replace(
            "executor: {kind: scripted}",
            "executor: {kind: pi, provider: deepseek, model: deepseek-flash, prices: \
             {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}}",
        );
        let browser = "browser: {lightpanda: bin/lightpanda, audit_key: keys/trail.key}\n\
                       trail: {path: blobs, ticket_max_bytes: 1000}\n";
        let dir = std::env::temp_dir().join(format!("agent-config-browser-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.yaml");
        std::fs::write(&path, format!("{pi}{browser}")).unwrap();
        let config = AgentConfig::load(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let loaded = config.browser.unwrap();
        assert_eq!(
            (loaded.lightpanda, loaded.audit_key, loaded.idle_secs),
            (
                dir.join("bin/lightpanda"),
                dir.join("keys/trail.key"),
                1_800
            )
        );
        assert_eq!(config.trail.path, Some(dir.join("blobs")));
        assert_eq!(
            (config.trail.max_bytes, config.trail.ticket_max_bytes),
            (8 << 30, 1_000)
        );

        let why = parse(&format!("{VALID}{browser}")).unwrap_err().to_string();
        assert!(why.contains("browser needs executor: pi"), "{why}");
        let idle = browser.replace("trail.key}", "trail.key, idle_secs: 0}");
        let why = parse(&format!("{pi}{idle}")).unwrap_err().to_string();
        assert!(
            why.contains("browser.idle_secs must be above zero"),
            "{why}"
        );
        assert!(parse(&pi).unwrap().browser.is_none());
    }

    #[test]
    fn fetch_url_reaches_public_addresses_only_unless_the_executor_says_otherwise() {
        let pi = |fetch: &str| {
            VALID.replace(
                "executor: {kind: scripted}",
                &format!(
                    "executor: {{kind: pi, provider: deepseek, model: deepseek-flash, prices: \
                     {{input_microusd_per_million: 300000, output_microusd_per_million: 1200000}}\
                     {fetch}}}"
                ),
            )
        };
        let allowed = |yaml: &str| match parse(yaml).unwrap().executor {
            ExecutorConfig::Pi { fetch, .. } => fetch.allow_private_networks,
            other => panic!("{other:?}"),
        };
        assert!(!allowed(&pi("")), "private networks are off by default");
        assert!(!allowed(&pi(", fetch: {}")));
        assert!(!allowed(&pi(", fetch: {allow_private_networks: false}")));
        assert!(allowed(&pi(", fetch: {allow_private_networks: true}")));
        let why = parse(&pi(", fetch: {allow_private_network: true}"))
            .unwrap_err()
            .to_string();
        assert!(
            why.contains("allow_private_network"),
            "a typo is refused, not read as off: {why}"
        );
    }

    #[test]
    fn unknown_fields_are_refused_at_every_level() {
        let cases = [
            (
                "top level",
                VALID.replace("router:", "routr: {kind: scripted}\nrouter:"),
            ),
            ("policy", VALID.replace("people:", "peeple: []\n  people:")),
            (
                "source",
                VALID.replace("path: triggers", "follow: true\n    path: triggers"),
            ),
            (
                "channel",
                VALID.replace("source: slack", "source: slack\n    format: x"),
            ),
            (
                "router",
                VALID.replace(
                    "{kind: scripted}\nexecutor",
                    "{kind: scripted, model: x}\nexecutor",
                ),
            ),
            (
                "executor",
                VALID.replace(
                    "executor: {kind: scripted}",
                    "executor: {kind: scripted, agents: 3}",
                ),
            ),
            (
                "budgets",
                VALID.replace("session_microusd:", "swarm_microusd: 1, session_microusd:"),
            ),
            (
                "kind",
                VALID.replace("router: {kind: scripted}", "router: {kind: llm}"),
            ),
        ];
        for (level, yaml) in cases {
            assert_ne!(yaml, VALID, "{level}: the case did not change the config");
            assert!(parse(&yaml).is_err(), "{level}: accepted\n{yaml}");
        }
        parse(VALID).unwrap();
    }

    const PI: &str = "router:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}
  rules: {ci-red: Fix a red CI run on main.}
";

    fn pi(edit: impl Fn(String) -> String) -> String {
        let yaml = VALID
            .replace("router: {kind: scripted}\n", PI)
            .replace("bots: [github:ci]", "bots: [github:ci]\n  rules: [ci-red]")
            .replace(
                "budgets: {session_microusd: 500000}",
                "budgets: {session_microusd: 500000, router_daily_microusd: 1000000}",
            );
        edit(yaml)
    }

    #[test]
    fn a_pi_router_loads_with_its_defaults() {
        let config = parse(&pi(|yaml| yaml)).unwrap();
        let RouterConfig::Pi {
            provider,
            max_input_bytes,
            max_output_tokens,
            max_batch,
            instructions,
            rules,
            ..
        } = &config.router
        else {
            panic!("{:?}", config.router)
        };
        assert_eq!(provider, "deepseek");
        assert_eq!((*max_input_bytes, *max_output_tokens), (1_048_576, 8_192));
        assert_eq!(*max_batch, 6);
        assert_eq!(*instructions, None);
        assert_eq!(rules["ci-red"], "Fix a red CI run on main.");
        assert_eq!(config.budgets.router_daily_microusd, Some(1_000_000));
    }

    #[test]
    fn a_config_without_budgets_loads_for_a_pi_router_and_caps_nothing() {
        let bare = |yaml: String| {
            yaml.replace(
                "budgets: {session_microusd: 500000, router_daily_microusd: 1000000}\n",
                "",
            )
        };
        let yaml = pi(bare);
        assert!(!yaml.contains("budgets"), "{yaml}");
        let config = parse(&yaml).unwrap();
        assert_eq!(config.budgets, Budgets::default());
        assert_eq!(config.budgets.session_microusd, None);
        assert_eq!(config.budgets.router_daily_microusd, None);
        // Each cap is its own choice.
        let daily_only = parse(&pi(|yaml| yaml.replace("session_microusd: 500000, ", ""))).unwrap();
        assert_eq!(daily_only.budgets.session_microusd, None);
        assert_eq!(daily_only.budgets.router_daily_microusd, Some(1_000_000));
        let session_only = parse(&pi(|yaml| {
            yaml.replace(", router_daily_microusd: 1000000", "")
        }))
        .unwrap();
        assert_eq!(session_only.budgets.session_microusd, Some(500_000));
        assert_eq!(session_only.budgets.router_daily_microusd, None);
        // An empty section is the same as none.
        let empty = parse(&pi(|yaml| {
            yaml.replace(
                "budgets: {session_microusd: 500000, router_daily_microusd: 1000000}",
                "budgets: {}",
            )
        }))
        .unwrap();
        assert_eq!(empty.budgets, Budgets::default());
    }

    #[test]
    fn a_pi_router_refuses_what_it_cannot_run_under() {
        for (case, yaml) in [
            (
                "zero daily cap",
                pi(|yaml| {
                    yaml.replace("router_daily_microusd: 1000000", "router_daily_microusd: 0")
                }),
            ),
            (
                "unknown provider",
                pi(|yaml| yaml.replace("provider: deepseek", "provider: openrouter")),
            ),
            (
                "prices below the admitted rates",
                pi(|yaml| {
                    yaml.replace(
                        "input_microusd_per_million: 300000",
                        "input_microusd_per_million: 1",
                    )
                }),
            ),
            (
                "a rule the policy does not enable",
                pi(|yaml| yaml.replace("  rules: [ci-red]", "  rules: []")),
            ),
            (
                "output limit out of range",
                pi(|yaml| {
                    yaml.replace(
                        "  model: deepseek-flash",
                        "  model: deepseek-flash\n  max_output_tokens: 0",
                    )
                }),
            ),
            (
                "no candidate in a batch",
                pi(|yaml| {
                    yaml.replace(
                        "  model: deepseek-flash",
                        "  model: deepseek-flash\n  max_batch: 0",
                    )
                }),
            ),
            (
                "a batch above the cap",
                pi(|yaml| {
                    yaml.replace(
                        "  model: deepseek-flash",
                        "  model: deepseek-flash\n  max_batch: 65",
                    )
                }),
            ),
            (
                "unknown field",
                pi(|yaml| {
                    yaml.replace(
                        "  model: deepseek-flash",
                        "  model: deepseek-flash\n  run_code: true",
                    )
                }),
            ),
        ] {
            assert_ne!(
                yaml,
                pi(|yaml| yaml),
                "{case}: the case did not change the config"
            );
            assert!(parse(&yaml).is_err(), "{case}: accepted\n{yaml}");
        }
    }

    #[test]
    fn a_pi_router_takes_the_batch_size_it_is_given() {
        let config = parse(&pi(|yaml| {
            yaml.replace(
                "  model: deepseek-flash",
                "  model: deepseek-flash\n  max_batch: 3",
            )
        }))
        .unwrap();
        let RouterConfig::Pi { max_batch, .. } = &config.router else {
            panic!("{:?}", config.router)
        };
        assert_eq!(*max_batch, 3);
    }

    const WORKERS: &str = "executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}
  max_workers: 3
  prepare: git -C /srv/repo worktree add \"$SOLOS_WORK_DIR\"
  run_code:
    mounts: [{target: /repo, source: /srv/repo}]
    concurrency: 2
";

    fn workers(edit: impl Fn(String) -> String) -> String {
        edit(pi(|yaml| {
            yaml.replace("executor: {kind: scripted}\n", WORKERS)
        }))
    }

    #[test]
    fn pstack_enters_poteto_mode_unless_named_and_needs_pi_workers_and_its_mount() {
        let on = |yaml: String, pstack: &str| format!("{yaml}pstack: {pstack}\n");
        let config = parse(&on(workers(|yaml| yaml), "{}")).unwrap();
        assert_eq!(config.pstack.unwrap().entry, "poteto-mode");
        let named = parse(&on(workers(|yaml| yaml), "{entry: how}")).unwrap();
        assert_eq!(named.pstack.unwrap().entry, "how");
        assert_eq!(parse(&workers(|yaml| yaml)).unwrap().pstack, None);
        for (yaml, why) in [
            (
                on(VALID.to_owned(), "{}"),
                "pstack needs executor: {kind: pi}",
            ),
            (
                on(
                    workers(|yaml| yaml.replace("target: /repo", "target: /pstack")),
                    "{}",
                ),
                "executor.run_code mounts /pstack",
            ),
            (
                on(workers(|yaml| yaml), "{entry: how, models: {}}"),
                "unknown field `models`",
            ),
        ] {
            let refused = format!("{:#}", parse(&yaml).unwrap_err());
            assert!(refused.contains(why), "{refused}");
        }
    }

    const APP: &str = "github: {app_id: 7, key_file: secrets/github-app.pem}\n";
    const FORGE: &str = "github: {app_id: 7, key_file: secrets/github-app.pem}
forge:
  kind: github
  repos:
    acme/widgets:
      about: Widgets, in Rust.
      base: main
      budget_microusd: 1500000
      notes: Tests run with ./check.
      owes: pull_request
    acme/docs: {about: The handbook., base: release/2026}
";
    const WORKTREE: &str = "  prepare: git -C /srv/repo worktree add \"$SOLOS_WORK_DIR\"\n";

    fn forged(edit: impl Fn(String) -> String) -> String {
        let workers = workers(|yaml| yaml.replace(WORKTREE, ""));
        format!("{workers}{}", edit(FORGE.to_owned()))
    }

    /// The forge on bare repositories on this disk instead, which has no App and
    /// no pull requests: its work owes a commit in the clone.
    fn local(yaml: String) -> String {
        let yaml = yaml.replace("owes: pull_request", "owes: change");
        yaml.replace(
            "github: {app_id: 7, key_file: secrets/github-app.pem}\nforge:\n  kind: github",
            "forge:\n  kind: local\n  remote: remotes\n  identity: {name: solos-bot, email: bot@example.com}",
        )
    }

    #[test]
    fn a_forge_loads_its_repositories_with_its_files_beside_the_config() {
        let dir = std::env::temp_dir().join(format!("agent-config-forge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.yaml");
        let load = |yaml: String| {
            std::fs::write(&path, yaml).unwrap();
            AgentConfig::load(&path).unwrap()
        };
        let app = load(forged(|yaml| yaml));
        let (github, settings) = (app.forge.unwrap(), app.github.unwrap());
        let on_disk = load(forged(local));
        assert_eq!(on_disk.github, None, "a local forge has no App");
        let on_disk = on_disk.forge.unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(matches!(github, ForgeConfig::Github { .. }), "{github:?}");
        assert_eq!(
            settings,
            GithubConfig {
                app_id: 7,
                key_file: dir.join("secrets/github-app.pem"),
                poll_secs: 60,
                reconcile_secs: 300,
                writes: true,
                api_base: "https://api.github.com/".into(),
                git_base: "https://github.com/".into(),
            }
        );
        assert_eq!(github.identity(), None, "the clone's identity is the App's");
        let ForgeConfig::Local { remote, .. } = &on_disk else {
            panic!("{on_disk:?}")
        };
        assert_eq!(*remote, dir.join("remotes"));
        assert_eq!(on_disk.identity().unwrap().email, "bot@example.com");
        let repo = |name: &str| RepoName::try_from(name.to_owned()).unwrap();
        assert_eq!(
            *github.repos(),
            BTreeMap::from([
                (
                    repo("acme/docs"),
                    RepoConfig {
                        about: "The handbook.".into(),
                        base: "release/2026".into(),
                        budget_microusd: None,
                        notes: None,
                        owes: Deliverable::Answer,
                        ..RepoConfig::default()
                    }
                ),
                (
                    repo("acme/widgets"),
                    RepoConfig {
                        about: "Widgets, in Rust.".into(),
                        base: "main".into(),
                        budget_microusd: Some(1_500_000),
                        notes: Some("Tests run with ./check.".into()),
                        owes: Deliverable::PullRequest,
                        ..RepoConfig::default()
                    }
                ),
            ])
        );
        let mut kept = github.repos().clone();
        kept.get_mut(&repo("acme/widgets")).unwrap().owes = Deliverable::Change;
        assert_eq!(
            *on_disk.repos(),
            kept,
            "the same terms, less the pull request"
        );
    }

    #[test]
    fn a_repository_owes_a_pull_request_only_where_one_can_be_made_and_its_limits_are_checked() {
        let refused = |yaml: String, said: &str| {
            let err = parse(&yaml).unwrap_err().to_string();
            assert!(err.contains(said), "{said}: {err}");
        };
        let widgets = |extra: &str| {
            forged(|yaml| {
                yaml.replace(
                    "      owes: pull_request\n",
                    &format!("      owes: pull_request\n{extra}"),
                )
            })
        };
        assert!(parse(&widgets("")).is_ok());
        assert!(parse(&widgets("      protected: [\".github/**\", \"vendor/**\"]\n      max_pushes: 3\n      merge: off\n      ci: none\n")).is_ok());
        refused(
            local(widgets("")).replace("owes: change", "owes: pull_request"),
            "a local forge has no pull requests",
        );
        refused(
            widgets("").replace("app_id: 7,", "app_id: 7, writes: false,"),
            "github.writes is false",
        );
        refused(
            widgets("      protected: [\"../x\"]\n"),
            "is not a path glob",
        );
        refused(widgets("      protected: [\"\"]\n"), "is not a path glob");
        refused(
            widgets("      max_pushes: 0\n"),
            "max_pushes must be at least 1",
        );
        refused(
            widgets("      max_change_bytes: 0\n"),
            "max_change_bytes must be 1..=",
        );
        refused(
            widgets("      max_change_bytes: 60000000\n"),
            "max_change_bytes must be 1..=",
        );
        refused(
            widgets("      merge: auto\n      ci: none\n"),
            "merge is auto, which needs ci: expected",
        );
        refused(
            widgets("      merge: auto\n"),
            "policy.approvers names nobody",
        );
        let auto = widgets("      merge: auto\n").replace(
            "people: [slack:alice]",
            "people: [slack:alice]\n  approvers: [slack:alice]",
        );
        assert!(parse(&auto).is_ok(), "{:?}", parse(&auto).err());
    }

    #[test]
    fn what_a_repository_grants_is_read_into_the_terms_an_accept_fixes() {
        use solos_agent_core::change::{ForgeTerms, MergeMode};
        let repo = RepoConfig::default();
        assert_eq!(
            repo.terms(),
            ForgeTerms {
                merge: MergeMode::Ask,
                ci_expected: true,
                max_pushes: 6,
                max_repairs: 2,
            }
        );
        assert_eq!(repo.protected, [".github/**"]);
        assert_eq!(repo.max_change_bytes, 5_000_000);
        let loose = RepoConfig {
            merge: super::MergeConfig::Auto,
            ci: super::CiConfig::None,
            max_pushes: 3,
            max_repairs: 0,
            ..RepoConfig::default()
        };
        let terms = loose.terms();
        assert_eq!(
            (
                terms.merge,
                terms.ci_expected,
                terms.max_pushes,
                terms.max_repairs
            ),
            (MergeMode::Auto, false, 3, 0)
        );
    }

    const DEPS_AND_EVIDENCE: &str = "owes: pull_request
      deps: {kind: bun, binary: /opt/bun/bin/bun}
      evidence: {command: \"bun run solos dev verify --scope unit --json\"}";

    #[test]
    fn a_repository_may_ask_for_installed_dependencies_and_evidence_in_its_pull_requests() {
        let repo = |name: &str| RepoName::try_from(name.to_owned()).unwrap();
        let config = parse(&forged(|yaml| {
            yaml.replace("owes: pull_request", DEPS_AND_EVIDENCE)
        }))
        .unwrap();
        let forge = config.forge.unwrap();
        let repos = forge.repos();

        let widgets = &repos[&repo("acme/widgets")];
        assert_eq!(
            widgets.deps,
            Some(DepsConfig {
                kind: DepsKind::Bun,
                binary: Some(PathBuf::from("/opt/bun/bin/bun")),
                timeout_secs: 600,
                max_bytes: 8 << 30,
                memory_bytes: 16 << 30,
            }),
            "ten minutes, 8 GiB of disk and 16 GiB of address space unless it says otherwise"
        );
        assert_eq!(
            widgets.evidence,
            Some(EvidenceConfig {
                command: "bun run solos dev verify --scope unit --json".into(),
                heading: "## Evidence".into(),
            })
        );
        let docs = &repos[&repo("acme/docs")];
        assert_eq!(
            (docs.deps.clone(), docs.evidence.clone()),
            (None, None),
            "a repository that asks for neither gets neither"
        );
        let found = widgets.deps.as_ref().unwrap();
        assert_eq!(found.binary_path(), Some(PathBuf::from("/opt/bun/bin/bun")));
        let usual = DepsConfig {
            binary: None,
            ..found.clone()
        };
        if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
            assert_eq!(
                usual.binary_path(),
                Some(PathBuf::from(home).join(".bun/bin/bun")),
                "bun's usual place"
            );
        }
    }

    #[test]
    fn a_dependency_cache_or_evidence_that_cannot_work_is_refused_where_it_is_written() {
        let on_widgets = |extra: &str| {
            forged(|yaml| {
                yaml.replace(
                    "owes: pull_request",
                    &format!("owes: pull_request\n      {extra}"),
                )
            })
        };
        for (yaml, why) in [
            (on_widgets("deps: {kind: npm}"), "unknown variant `npm`"),
            (
                on_widgets("deps: {kind: bun, caches: 1}"),
                "unknown field `caches`",
            ),
            (
                on_widgets("deps: {kind: bun, binary: bun}"),
                "forge.repos.acme/widgets.deps: binary must be an absolute path",
            ),
            (
                on_widgets("deps: {kind: bun, binary: /b, timeout_secs: 5}"),
                "forge.repos.acme/widgets.deps: timeout_secs must be 10..=3600",
            ),
            (
                on_widgets("deps: {kind: bun, binary: /b, timeout_secs: 3601}"),
                "timeout_secs must be 10..=3600",
            ),
            (
                on_widgets("deps: {kind: bun, binary: /b, max_bytes: 1000}"),
                "max_bytes must be 1 MiB..=64 GiB",
            ),
            (
                on_widgets("deps: {kind: bun, binary: /b, memory_bytes: 17179869185}"),
                "memory_bytes must be 256 MiB..=16 GiB",
            ),
            (
                on_widgets("evidence: {command: \"\"}"),
                "forge.repos.acme/widgets.evidence: command must be one line of text",
            ),
            (
                on_widgets("evidence: {command: \"a\\nb\"}"),
                "command must be one line of text",
            ),
            (
                on_widgets("evidence: {command: x, heading: Evidence}"),
                "heading must be a markdown heading",
            ),
            (
                on_widgets("evidence: {command: x, heading: \"##Evidence\"}"),
                "heading must be a markdown heading",
            ),
            (
                forged(|yaml| {
                    yaml.replace(
                        "acme/docs: {about: The handbook., base: release/2026}",
                        "acme/docs: {about: The handbook., base: release/2026, evidence: {command: x}}",
                    )
                }),
                "forge.repos.acme/docs.evidence is for a pull request's description, and owes is \
                 not pull_request",
            ),
        ] {
            let refused = format!("{:#}", parse(&yaml).unwrap_err());
            assert!(refused.contains(why), "{refused}");
        }
    }

    #[test]
    fn a_forge_refuses_a_worktree_prepare_and_needs_pi_workers_with_run_code() {
        let worktree = format!("{}{FORGE}", workers(|yaml| yaml));
        assert!(worktree.contains(WORKTREE));
        assert!(
            parse(&workers(|yaml| yaml)).is_ok(),
            "the same prepare loads without a forge"
        );
        let scripted = format!("{VALID}{FORGE}");
        let no_run_code = format!(
            "{}{FORGE}",
            workers(|yaml| {
                let bare = yaml.replace(WORKTREE, "");
                let at = bare.find("  run_code:").unwrap();
                let end = bare[at..].find("budgets:").unwrap();
                format!("{}{}", &bare[..at], &bare[at + end..])
            })
        );
        for (yaml, why) in [
            (
                worktree,
                "executor.prepare runs git worktree add into the work dir, and the forge clones \
                 each repository ticket's work dir itself",
            ),
            (scripted, "forge needs executor: {kind: pi}"),
            (no_run_code, "forge needs executor.run_code"),
            (
                forged(|yaml| {
                    let at = yaml.find("  repos:").unwrap();
                    format!("{}  repos: {{}}\n", &yaml[..at])
                }),
                "forge names no repos",
            ),
            (
                forged(|yaml| yaml.replace("acme/docs:", "acme/../docs:")),
                "\"acme/../docs\" is not a repository",
            ),
            (
                forged(|yaml| yaml.replace("base: release/2026", "base: --upload-pack=x")),
                "forge.repos.acme/docs.base \"--upload-pack=x\" is not a branch name",
            ),
            (
                forged(|yaml| yaml.replace("about: The handbook.", "about: \"  \"")),
                "forge.repos.acme/docs.about is empty",
            ),
            (
                forged(|yaml| yaml.replace("1500000", "2500000")),
                "forge.repos.acme/widgets.budget_microusd 2500000 is above \
                 policy.auto_approve_microusd, and nobody here can approve a budget",
            ),
            (
                forged(|yaml| local(yaml).replace("name: solos-bot", "name: \"\"")),
                "forge.identity.name must be one line of text",
            ),
            (
                forged(|yaml| yaml.replace("owes: pull_request", "owes: merge")),
                "unknown variant `merge`",
            ),
            (
                forged(|yaml| yaml.replace("kind: github", "kind: local")),
                "missing field `remote`",
            ),
        ] {
            let refused = format!("{:#}", parse(&yaml).unwrap_err());
            assert!(refused.contains(why), "{refused}");
        }
    }

    #[test]
    fn the_old_token_file_shape_is_refused_with_the_sentence_that_says_what_replaced_it() {
        let dir = std::env::temp_dir().join(format!("agent-config-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.yaml");
        let old_source = |fields: &str| {
            let yaml = format!(
                "{}sources:\n  - kind: github\n{fields}",
                VALID.split("sources:").next().unwrap(),
            );
            format!(
                "{yaml}channels: []\nrouter: {{kind: scripted}}\nexecutor: {{kind: scripted}}\nbudgets: {{session_microusd: 500000}}\n"
            )
        };
        let cases = [
            (
                old_source("    repos: [acme/widgets]\n    token_file: secrets/github\n"),
                "the github source's token_file is gone",
            ),
            (
                old_source("    token_file: secrets/github\n"),
                "the github source's token_file is gone",
            ),
            (
                old_source("    repos: [acme/widgets]\n"),
                "the github source no longer takes repos",
            ),
            (
                old_source("    api_base: https://api.github.com/\n"),
                "the github source no longer takes api_base",
            ),
            (
                old_source("    poll_secs: 30\n"),
                "the github source no longer takes poll_secs",
            ),
            (
                forged(|yaml| {
                    yaml.replace(
                        "kind: github\n",
                        "kind: github\n  token_file: secrets/forge-token\n",
                    )
                }),
                "forge: github no longer takes token_file",
            ),
            (
                forged(|yaml| {
                    yaml.replace(
                        "kind: github\n",
                        "kind: github\n  identity: {name: bot, email: bot@example.com}\n",
                    )
                }),
                "forge: github no longer takes identity",
            ),
        ];
        for (yaml, why) in cases {
            std::fs::write(&path, &yaml).unwrap();
            let refused = format!("{:#}", AgentConfig::load(&path).unwrap_err());
            assert!(refused.contains(why), "{why}: {refused}");
            assert!(
                refused.ends_with("see docs/agent-mode/how-to/github-app.md")
                    && refused.lines().count() == 1,
                "one sentence naming the migration doc: {refused}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_github_section_goes_with_what_uses_it_and_the_policys_logins_are_lowercased() {
        let dir = std::env::temp_dir().join(format!("agent-config-gh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.yaml");
        let lowered = forged(|yaml| yaml).replace(
            "policy:\n  people: [slack:alice]",
            "policy:\n  people: [slack:alice, \"github:Alice\"]\n  approvers: [github:BOB]",
        );
        assert_ne!(lowered, forged(|yaml| yaml));
        std::fs::write(&path, &lowered).unwrap();
        let config = AgentConfig::load(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(config.policy.people.contains("github:alice"));
        assert!(config.policy.people.contains("slack:alice"), "others stay");
        assert!(config.policy.approvers.contains("github:bob"));
        assert_eq!(
            config.github_logins(),
            ["alice".to_owned(), "bob".to_owned(), "ci".to_owned()].into()
        );
        for (yaml, why) in [
            (
                forged(|yaml| yaml.replace(APP, "")),
                "add a github: section",
            ),
            (format!("{VALID}{APP}"), "the github: section is unused"),
            (
                forged(|yaml| yaml.replace("app_id: 7", "app_id: 0")),
                "github.app_id is the App's numeric id",
            ),
            (
                forged(|yaml| yaml.replace("app_id: 7", "app_id: 7, poll_secs: 2")),
                "github.poll_secs must not poll more often than every 10s",
            ),
            (
                forged(|yaml| yaml.replace("app_id: 7", "app_id: 7, reconcile_secs: 30")),
                "github.reconcile_secs must be at least 60",
            ),
            (
                forged(|yaml| yaml.replace("app_id: 7", "app_id: 7, api_base: nowhere")),
                "invalid github.api_base nowhere",
            ),
            (
                forged(|yaml| yaml.replace("app_id: 7", "app_id: 7, git_base: ftp://x/")),
                "github.git_base ftp://x/ is not an https://, http:// or file:// URL",
            ),
            (
                forged(|yaml| yaml.replace("app_id: 7", "app_id: 7, token: x")),
                "unknown field `token`",
            ),
            (
                forged(|yaml| yaml).replace("people: [slack:alice]", "people: [\"github:a b\"]"),
                "policy lists github:a b, which is not a GitHub login",
            ),
        ] {
            let refused = format!("{:#}", parse(&yaml).unwrap_err());
            assert!(refused.contains(why), "{why}: {refused}");
        }
        // forge.repos is the one list: the github Source needs it.
        let needs_forge = format!("{VALID}{APP}").replace(
            "sources:\n  - kind: jsonl\n    path: triggers.jsonl",
            "sources:\n  - kind: github",
        );
        let refused = format!("{:#}", parse(&needs_forge).unwrap_err());
        assert!(refused.contains("add a forge: section"), "{refused}");
    }

    #[test]
    fn pi_workers_load_with_their_limits_and_mounts() {
        let config = parse(&workers(|yaml| yaml)).unwrap();
        let ExecutorConfig::Pi {
            provider,
            max_input_bytes,
            max_workers,
            prepare,
            run_code,
            ..
        } = &config.executor
        else {
            panic!("{:?}", config.executor)
        };
        assert_eq!(
            (provider.as_str(), *max_input_bytes, *max_workers),
            ("deepseek", 1_048_576, 3)
        );
        assert!(prepare.as_deref().unwrap().contains("worktree add"));
        let run_code = run_code.as_ref().unwrap();
        assert_eq!(run_code.mounts[0].target, PathBuf::from("/repo"));
        assert_eq!(
            (run_code.concurrency, run_code.memory_bytes),
            (2, 4 * 1024 * 1024 * 1024)
        );
        assert_eq!(config.pi_upstream(), Some(("deepseek", "deepseek-flash")));
        let alone = workers(|yaml| yaml.replace(PI, "router: {kind: scripted}\n"));
        let alone = parse(&alone).unwrap();
        assert_eq!(alone.pi_upstream(), Some(("deepseek", "deepseek-flash")));
        let bare = WORKERS.split("  max_workers").next().unwrap().to_owned();
        let bare = parse(&workers(|yaml| yaml.replace(WORKERS, &bare))).unwrap();
        let ExecutorConfig::Pi {
            max_workers,
            run_code,
            prepare,
            ..
        } = bare.executor
        else {
            panic!()
        };
        assert_eq!((max_workers, run_code, prepare), (2, None, None));
        assert_eq!(parse(VALID).unwrap().pi_upstream(), None);
    }

    #[test]
    fn workers_can_run_on_claude_code_where_it_is_configured() {
        let with = |harness: &str, claude: &str| {
            workers(|yaml| {
                yaml.replace(
                    "  max_workers: 3\n",
                    &format!("  max_workers: 3\n{harness}{claude}"),
                )
            })
        };
        let binary = "  claude: {binary: /opt/claude/2.1.231}\n";
        let config = parse(&with("  harness: claude-code\n", binary)).unwrap();
        let ExecutorConfig::Pi {
            harness,
            claude: Some(install),
            ..
        } = &config.executor
        else {
            panic!("{:?}", config.executor)
        };
        assert_eq!(*harness, HarnessKind::ClaudeCode);
        assert_eq!(
            **install,
            ClaudeConfig {
                binary: PathBuf::from("/opt/claude/2.1.231"),
                node: None,
                solos_swarm: None,
            }
        );
        let ExecutorConfig::Pi {
            harness, claude, ..
        } = parse(&workers(|yaml| yaml)).unwrap().executor
        else {
            panic!()
        };
        assert_eq!((harness, claude), (HarnessKind::Pi, None));
        assert!(parse(&with("", binary)).is_ok(), "a Plan may still name it");
        assert_eq!(
            HarnessKind::named("claude-code"),
            Some(HarnessKind::ClaudeCode)
        );
        assert_eq!(HarnessKind::named("codex"), None);
        for (case, yaml) in [
            ("no install", with("  harness: claude-code\n", "")),
            ("an unknown harness", with("  harness: codex\n", binary)),
            (
                "a relative binary",
                with("", "  claude: {binary: bin/claude}\n"),
            ),
            (
                "a relative node",
                with("", "  claude: {binary: /opt/cli.js, node: node}\n"),
            ),
            (
                "an unknown field",
                with("", "  claude: {binary: /opt/c, model: x}\n"),
            ),
        ] {
            assert!(parse(&yaml).is_err(), "{case}: accepted\n{yaml}");
        }
    }

    #[test]
    fn pi_workers_refuse_what_they_cannot_run_under() {
        for (case, yaml) in [
            (
                "another provider than the router's",
                workers(|yaml| {
                    yaml.replace("  provider: deepseek\n  model: deepseek-flash\n  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}\n  max_workers", "  provider: venice\n  model: deepseek-v4-1-flash-uncensored\n  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}\n  max_workers")
                }),
            ),
            (
                "no worker slot",
                workers(|yaml| yaml.replace("max_workers: 3", "max_workers: 0")),
            ),
            (
                "too many worker slots",
                workers(|yaml| yaml.replace("max_workers: 3", "max_workers: 65")),
            ),
            (
                "an empty prepare command",
                workers(|yaml| yaml.replace("prepare: git", "prepare: ' '\n  # git")),
            ),
            (
                "a mount over the work directory",
                workers(|yaml| yaml.replace("target: /repo", "target: /work")),
            ),
            (
                "a relative mount source",
                workers(|yaml| yaml.replace("source: /srv/repo}", "source: srv/repo}")),
            ),
            (
                "one target twice",
                workers(|yaml| {
                    yaml.replace(
                        "mounts: [{target: /repo, source: /srv/repo}]",
                        "mounts: [{target: /repo, source: /srv/a}, {target: /repo, source: /srv/b}]",
                    )
                }),
            ),
            (
                "too many sandboxes",
                workers(|yaml| yaml.replace("concurrency: 2", "concurrency: 9")),
            ),
            (
                "an output limit out of range",
                workers(|yaml| {
                    yaml.replace("max_workers: 3", "max_workers: 3\n  max_output_tokens: 0")
                }),
            ),
            (
                "a temperature above 2",
                workers(|yaml| {
                    yaml.replace("max_workers: 3", "max_workers: 3\n  temperature: 2.5")
                }),
            ),
            (
                "a negative temperature",
                workers(|yaml| {
                    yaml.replace("max_workers: 3", "max_workers: 3\n  temperature: -0.1")
                }),
            ),
            (
                "a temperature that is no number",
                workers(|yaml| {
                    yaml.replace("max_workers: 3", "max_workers: 3\n  temperature: .nan")
                }),
            ),
            (
                "an unknown field",
                workers(|yaml| yaml.replace("concurrency: 2", "concurrency: 2\n    network: true")),
            ),
        ] {
            assert_ne!(
                yaml,
                workers(|yaml| yaml),
                "{case}: the case did not change the config"
            );
            assert!(parse(&yaml).is_err(), "{case}: accepted\n{yaml}");
        }
    }

    #[test]
    fn the_workers_temperature_is_optional_and_has_a_default() {
        let temperature = |yaml: String| {
            let config = parse(&yaml).unwrap();
            let ExecutorConfig::Pi { temperature, .. } = config.executor else {
                panic!("{:?}", config.executor)
            };
            temperature.get()
        };
        assert_eq!(temperature(workers(|yaml| yaml)), 0.6);
        for (given, wanted) in [("0", 0.0), ("0.9", 0.9), ("1", 1.0), ("2", 2.0)] {
            let yaml = workers(|yaml| {
                yaml.replace(
                    "max_workers: 3",
                    &format!("max_workers: 3\n  temperature: {given}"),
                )
            });
            assert_eq!(temperature(yaml), wanted, "temperature: {given}");
        }
    }

    const SWARM: &str = "  swarm:
    templates: {research: goals/research.yaml}
    default_template: research
";

    #[test]
    fn a_swarm_loads_its_checked_templates_beside_the_config() {
        let dir = std::env::temp_dir().join(format!("agent-config-swarm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("goals")).unwrap();
        let template = include_str!("../../tests/fixtures/goal-template.yaml");
        std::fs::write(dir.join("goals/research.yaml"), template).unwrap();
        let load = |yaml: String| {
            std::fs::write(dir.join("agent.yaml"), yaml).unwrap();
            AgentConfig::load(&dir.join("agent.yaml"))
        };
        let config = load(workers(|yaml| {
            yaml.replace(
                "    concurrency: 2\n",
                &format!("    concurrency: 2\n{SWARM}"),
            )
        }))
        .unwrap();
        let ExecutorConfig::Pi {
            swarm: Some(swarm), ..
        } = &config.executor
        else {
            panic!("{:?}", config.executor)
        };
        assert_eq!(swarm.templates["research"], dir.join("goals/research.yaml"));
        assert_eq!(
            (swarm.default_template.as_deref(), swarm.max_runs),
            (Some("research"), 1)
        );

        let with = |edit: &dyn Fn(String) -> String| {
            workers(|yaml| {
                yaml.replace(
                    "    concurrency: 2\n",
                    &format!("    concurrency: 2\n{}", edit(SWARM.to_owned())),
                )
            })
        };
        for (case, yaml) in [
            (
                "no template",
                with(&|s| {
                    s.replace("{research: goals/research.yaml}", "{}")
                        .replace("    default_template: research\n", "")
                }),
            ),
            (
                "an unknown default",
                with(&|s| s.replace("default_template: research", "default_template: audit")),
            ),
            ("no run slot", with(&|s| format!("{s}    max_runs: 0\n"))),
            (
                "too many run slots",
                with(&|s| format!("{s}    max_runs: 9\n")),
            ),
            (
                "a template name that is not a word",
                with(&|s| {
                    s.replace("{research:", "{\"re search\":")
                        .replace("default_template: research", "default_template: re search")
                }),
            ),
            (
                "a missing template",
                with(&|s| s.replace("goals/research.yaml", "goals/gone.yaml")),
            ),
            ("an unknown field", with(&|s| format!("{s}    agents: 3\n"))),
            (
                "a zero default budget",
                with(&|s| format!("{s}    default_budget_microusd: 0\n")),
            ),
            (
                "a default budget above the threshold that nobody can approve",
                with(&|s| format!("{s}    default_budget_microusd: 9000000\n")),
            ),
        ] {
            assert!(load(yaml.clone()).is_err(), "{case}: accepted\n{yaml}");
        }
        let budgeted = load(with(&|s| {
            format!("{s}    default_budget_microusd: 1500000\n")
        }))
        .unwrap();
        let ExecutorConfig::Pi {
            swarm: Some(swarm), ..
        } = &budgeted.executor
        else {
            panic!("{:?}", budgeted.executor)
        };
        assert_eq!(swarm.default_budget_microusd, Some(1_500_000));
        assert_eq!(
            budgeted.budgets.session_microusd,
            Some(500_000),
            "a swarm's own default does not touch a session's"
        );
        let uncapped = load(with(&|s| s).replace(
            "budgets: {session_microusd: 500000, router_daily_microusd: 1000000}",
            "budgets: {}",
        ))
        .unwrap();
        assert_eq!(uncapped.budgets, Budgets::default());
        assert!(
            matches!(
                &uncapped.executor,
                ExecutorConfig::Pi { swarm: Some(_), .. }
            ),
            "swarm templates still check against a sample budget with no session budget"
        );
        std::fs::write(
            dir.join("goals/research.yaml"),
            template.replace("{% for n in range(agents) %}", "{% for n in range(2) %}"),
        )
        .unwrap();
        let err = load(with(&|s| s)).unwrap_err();
        assert!(
            format!("{err:#}").contains("checking goal template research with 1 agents"),
            "{err:#}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn background_jobs_load_with_their_defaults_and_refuse_limits_out_of_range() {
        let config = parse(&workers(|yaml| yaml)).unwrap();
        let ExecutorConfig::Pi {
            run_code: Some(run_code),
            ..
        } = &config.executor
        else {
            panic!("{:?}", config.executor)
        };
        assert_eq!(run_code.job_limits(), JobLimits::default());
        assert_eq!(
            JobLimits::default(),
            JobLimits {
                background_after: Duration::from_secs(5),
                max_jobs: 2,
                max_wall: Duration::from_secs(1_800),
                output_bytes: 65_536,
            }
        );
        let set = "concurrency: 2\n    background_after_secs: 10\n    max_jobs: 4\n    \
                   job_max_secs: 600\n    job_output_bytes: 4096";
        let config = parse(&workers(|yaml| yaml.replace("concurrency: 2", set))).unwrap();
        let ExecutorConfig::Pi {
            run_code: Some(run_code),
            ..
        } = &config.executor
        else {
            panic!("{:?}", config.executor)
        };
        assert_eq!(
            run_code.job_limits(),
            JobLimits {
                background_after: Duration::from_secs(10),
                max_jobs: 4,
                max_wall: Duration::from_secs(600),
                output_bytes: 4_096,
            }
        );
        for bad in [
            "background_after_secs: 0",
            "background_after_secs: 301",
            "max_jobs: 0",
            "max_jobs: 9",
            "job_max_secs: 299",
            "job_max_secs: 86401",
            "job_output_bytes: 1023",
            "job_output_bytes: 1048577",
            "jobs: 3",
        ] {
            let yaml = workers(|yaml| {
                yaml.replace("concurrency: 2", &format!("concurrency: 2\n    {bad}"))
            });
            assert!(parse(&yaml).is_err(), "{bad}: accepted");
        }
    }

    #[test]
    fn validation_refuses_what_cannot_run() {
        let no_source = VALID.replace(
            "sources:\n  - kind: jsonl\n    path: triggers.jsonl",
            "sources: []",
        );
        let twice = VALID.replace(
            "channels:\n",
            "channels:\n  - {kind: jsonl, source: slack, path: other.jsonl}\n",
        );
        let free = VALID.replace("session_microusd: 500000", "session_microusd: 0");
        let free_router = VALID.replace(
            "session_microusd: 500000",
            "session_microusd: 500000, router_daily_microusd: 0",
        );
        let gated = VALID.replace("session_microusd: 500000", "session_microusd: 50000000");
        for (case, yaml) in [
            ("no source", no_source),
            ("one source twice", twice),
            ("zero budget", free),
            ("zero router cap", free_router),
            ("budget above the approval threshold", gated),
        ] {
            assert_ne!(yaml, VALID, "{case}: the case did not change the config");
            assert!(parse(&yaml).is_err(), "{case}: accepted");
        }
    }

    #[test]
    fn the_bench_designs_load_and_their_limits_are_checked() {
        let design = |router: &str, executor: &str| {
            VALID
                .replace("router: {kind: scripted}", router)
                .replace("executor: {kind: scripted}", executor)
        };
        let serial = parse(&format!(
            "{}pass_through: false\n",
            design(
                "router: {kind: scripted, ignore_follow_ups: true}",
                "executor: {kind: simulated, max_workers: 1, time_factor: 0.01, \
                 router_waits_for_work: true}",
            )
        ))
        .unwrap();
        assert!(!serial.pass_through);
        assert_eq!(
            serial.router,
            RouterConfig::Scripted {
                ignore_follow_ups: true
            }
        );
        let ExecutorConfig::Simulated {
            max_workers,
            time_factor,
            default_work_ms,
            router_waits_for_work,
        } = serial.executor
        else {
            panic!("{:?}", serial.executor)
        };
        assert_eq!((max_workers, default_work_ms), (1, 600_000));
        assert_eq!(time_factor.wall(60_000), Duration::from_millis(600));
        assert!(router_waits_for_work);
        let defaults = parse(&design(
            "router: {kind: scripted}",
            "executor: {kind: simulated}",
        ))
        .unwrap();
        assert!(defaults.pass_through, "pass-through is on unless refused");
        for executor in [
            "executor: {kind: simulated, max_workers: 0}",
            "executor: {kind: simulated, max_workers: 65}",
            "executor: {kind: simulated, time_factor: 0}",
            "executor: {kind: simulated, time_factor: .nan}",
            "executor: {kind: simulated, default_work_ms: 0}",
            "executor: {kind: simulated, workers: 2}",
        ] {
            let yaml = design("router: {kind: scripted}", executor);
            assert!(parse(&yaml).is_err(), "{executor}: accepted");
        }
    }

    #[test]
    fn an_mcp_source_needs_the_mcp_channel_and_an_mcp_operator_needs_a_thread() {
        let mcp = VALID
            .replace(
                "  - kind: jsonl\n    path: triggers.jsonl",
                "  - kind: jsonl\n    path: triggers.jsonl\n  - kind: mcp",
            )
            .replace("channels:\n", "channels:\n  - kind: mcp\n");
        let config = parse(&mcp).unwrap();
        assert_eq!(config.sources[1], SourceConfig::Mcp {});
        assert_eq!(config.channels[0].source(), "mcp");
        let watched = format!(
            "{mcp}watchdog:\n  operator: {{source: mcp, address: {{operator: ops, thread: alerts}}}}\n"
        );
        parse(&watched).unwrap();
        for (case, yaml) in [
            (
                "no mcp channel",
                mcp.replace("  - kind: mcp\n  - kind: jsonl", "  - kind: jsonl"),
            ),
            (
                "a jsonl channel for mcp",
                mcp.replace(
                    "  - kind: mcp\n  - kind: jsonl",
                    "  - {kind: jsonl, source: mcp, path: o}\n  - kind: jsonl",
                ),
            ),
            (
                "two mcp sources",
                mcp.replace(
                    "  - kind: mcp\nchannels",
                    "  - kind: mcp\n  - kind: mcp\nchannels",
                ),
            ),
            (
                "an mcp operator with no thread",
                watched.replace(", thread: alerts", ""),
            ),
            (
                "an mcp operator with a colon",
                watched.replace("operator: ops", "operator: 'o:ps'"),
            ),
        ] {
            assert_ne!(yaml, mcp, "{case}: the case did not change the config");
            assert!(parse(&yaml).is_err(), "{case}: accepted\n{yaml}");
        }
    }

    const SLACK: &str = "
policy:
  people: [slack:U1]
  approvers: [slack:U1]
sources:
  - kind: slack
    channels: [C1]
    app_token_file: secrets/app
    bot_token_file: secrets/bot
channels:
  - kind: slack
router: {kind: scripted}
executor: {kind: scripted}
budgets: {session_microusd: 500000}
";

    #[test]
    fn a_slack_config_loads_with_token_files_beside_it_and_the_public_api() {
        let dir = std::env::temp_dir().join(format!("agent-config-slack-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.yaml");
        std::fs::write(&path, SLACK).unwrap();
        let config = AgentConfig::load(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            config.sources,
            [SourceConfig::Slack {
                channels: ["C1".to_owned()].into(),
                app_token_file: dir.join("secrets/app"),
                bot_token_file: dir.join("secrets/bot"),
                api_base: "https://slack.com/api/".into(),
            }]
        );
        assert_eq!(config.channels, [ChannelConfig::Slack {}]);
        assert_eq!(config.channels[0].source(), "slack");
    }

    #[test]
    fn slack_takes_approvals_so_a_gated_budget_needs_its_channel_and_an_approver() {
        let gated = SLACK.replace("session_microusd: 500000", "session_microusd: 50000000");
        parse(&gated).unwrap();
        let typo = SLACK.replace("channels: [C1]", "channels: [C1]\n    chanels: [C2]");
        let two = SLACK.replace(
            "sources:\n",
            "sources:\n  - {kind: slack, channels: [C2], app_token_file: a, bot_token_file: b}\n",
        );
        let deaf = SLACK.replace("channels: [C1]", "channels: []");
        let tokenless = SLACK.replace(
            "  - kind: slack\n    channels: [C1]\n    app_token_file: secrets/app\n    \
             bot_token_file: secrets/bot",
            "  - {kind: jsonl, path: triggers.jsonl}",
        );
        let unheard = gated.replace("channels:\n  - kind: slack\n", "channels: []\n");
        let unapproved = gated.replace("approvers: [slack:U1]", "approvers: [github:U1]");
        for (case, yaml) in [
            ("an unknown slack field", typo),
            ("two slack sources", two),
            ("no listened channel", deaf),
            ("a slack channel without a slack source", tokenless),
            ("a gated budget with no slack channel", unheard),
            ("a gated budget with no slack approver", unapproved),
        ] {
            assert!(parse(&yaml).is_err(), "{case}: accepted\n{yaml}");
        }
    }

    const DISCORD: &str = "
policy:
  people: [discord:1300000000000000011]
  approvers: [discord:1300000000000000011]
sources:
  - kind: discord
    guilds: [1100000000000000001]
    channels: [1200000000000000001]
    token_file: secrets/discord
channels:
  - kind: discord
router: {kind: scripted}
executor: {kind: scripted}
budgets: {session_microusd: 500000}
";

    #[test]
    fn a_discord_config_loads_with_ids_written_as_numbers_and_the_token_file_beside_it() {
        let dir = std::env::temp_dir().join(format!("agent-config-discord-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.yaml");
        std::fs::write(&path, DISCORD).unwrap();
        let config = AgentConfig::load(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            config.sources,
            [SourceConfig::Discord {
                guilds: ["1100000000000000001".to_owned()].into(),
                channels: ["1200000000000000001".to_owned()].into(),
                token_file: dir.join("secrets/discord"),
                api_base: "https://discord.com/api/v10/".into(),
            }]
        );
        assert_eq!(config.channels, [ChannelConfig::Discord {}]);
        assert_eq!(config.channels[0].source(), "discord");
        let quoted = DISCORD
            .replace("[1100000000000000001]", "[\"1100000000000000001\"]")
            .replace("[1200000000000000001]", "['1200000000000000001']");
        assert_eq!(parse(&quoted).unwrap(), parse(DISCORD).unwrap());
        let by_guild_only = DISCORD.replace("    channels: [1200000000000000001]\n", "");
        parse(&by_guild_only).unwrap();
    }

    #[test]
    fn discord_takes_approvals_so_a_gated_budget_needs_its_channel_and_an_approver() {
        let gated = DISCORD.replace("session_microusd: 500000", "session_microusd: 50000000");
        parse(&gated).unwrap();
        assert!(parse(&gated).unwrap().takes_approvals());
        let slack_approver = gated.replace(
            "approvers: [discord:1300000000000000011]",
            "approvers: [slack:U1]",
        );
        let no_channel = gated.replace("channels:\n  - kind: discord\n", "channels: []\n");
        let typo = DISCORD.replace("token_file:", "token: x\n    token_file:");
        let two = DISCORD.replace(
            "sources:\n",
            "sources:\n  - {kind: discord, channels: [1], token_file: t}\n",
        );
        let deaf = DISCORD.replace(
            "    guilds: [1100000000000000001]\n    channels: [1200000000000000001]\n",
            "",
        );
        let not_an_id = DISCORD.replace("[1200000000000000001]", "[general]");
        let tokenless = DISCORD.replace(
            "  - kind: discord\n    guilds: [1100000000000000001]\n    channels: \
             [1200000000000000001]\n    token_file: secrets/discord",
            "  - {kind: jsonl, path: triggers.jsonl}",
        );
        let not_http = DISCORD.replace(
            "token_file: secrets/discord",
            "token_file: t\n    api_base: wss://discord.com",
        );
        for (case, yaml) in [
            ("a gated budget with a slack approver only", slack_approver),
            ("a gated budget with no button channel", no_channel),
            ("an unknown discord field", typo),
            ("two discord sources", two),
            ("no guild or channel", deaf),
            ("a channel that is not an id", not_an_id),
            ("a discord channel without a discord source", tokenless),
            ("an api base that is not http", not_http),
        ] {
            assert_ne!(yaml, DISCORD, "{case}: the case did not change the config");
            assert!(parse(&yaml).is_err(), "{case}: accepted\n{yaml}");
        }
    }

    #[test]
    fn approvals_need_an_approver_on_the_source_of_a_channel_with_buttons() {
        let elsewhere = DISCORD.replace(
            "approvers: [discord:1300000000000000011]",
            "approvers: [slack:U1]",
        );
        assert!(!parse(&elsewhere).unwrap().takes_approvals());
        let slack_channel_only = SLACK.replace(
            "approvers: [slack:U1]",
            "approvers: [discord:1300000000000000011]",
        );
        assert!(!parse(&slack_channel_only).unwrap().takes_approvals());
    }

    #[test]
    fn only_a_slack_channel_with_a_slack_approver_takes_approvals() {
        assert!(parse(SLACK).unwrap().takes_approvals());
        assert_eq!(
            parse(SLACK).unwrap().approval_sources(),
            ["slack".to_owned()].into(),
            "a Ticket that answers on Slack takes approvals here, and only such a Ticket"
        );
        let elsewhere = SLACK.replace("approvers: [slack:U1]", "approvers: [github:U1]");
        assert!(!parse(&elsewhere).unwrap().takes_approvals());
        assert!(parse(&elsewhere).unwrap().approval_sources().is_empty());
        let unheard = VALID.replace(
            "bots: [github:ci]",
            "bots: [github:ci]\n  approvers: [slack:alice]",
        );
        assert!(!parse(&unheard).unwrap().takes_approvals());
    }
}
