//! `agent.yaml`. Unknown fields are errors, like a swarm Goal's, so a typo
//! cannot silently fall back to a default.

mod sources;
mod watchdog;

pub use sources::{Interval, ReplyTo, ScheduleConfig, TimeOfDay};
pub use watchdog::{OperatorConfig, ThresholdsConfig, WatchdogConfig};

use super::goal_template::GoalTemplates;
use super::jobs::{self, JobLimits};
use super::{github, slack};
use crate::code::{self, Mount};
use crate::gateway::{PriceTable, upstream_profile};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use solos_agent_core::AllowlistPolicy;
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
    pub budgets: Budgets,
    /// Without it, no Watchdog runs.
    #[serde(default)]
    pub watchdog: Option<WatchdogConfig>,
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
    /// GitHub over REST polling; its Triggers name source `github`. Only
    /// the listed repositories are read. The token file is read once and
    /// deleted.
    Github {
        /// `owner/name`.
        repos: BTreeSet<String>,
        /// A personal access token of the agent's own account, which the
        /// `github` channel uses too.
        token_file: PathBuf,
        #[serde(default = "github_api_base")]
        api_base: String,
        #[serde(default = "github_poll_secs")]
        poll_secs: u64,
    },
    /// Named schedules that fire as System Triggers of source `schedule`.
    Schedule { schedules: Vec<ScheduleConfig> },
}

fn slack_api_base() -> String {
    slack::DEFAULT_API_BASE.to_owned()
}

fn github_api_base() -> String {
    github::DEFAULT_API_BASE.to_owned()
}

fn github_poll_secs() -> u64 {
    60
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
    /// Comments on issues and pull requests with the token and API base of
    /// the `github` Source. Serves replies to source `github`.
    Github {},
}

impl ChannelConfig {
    pub fn source(&self) -> &str {
        match self {
            Self::Jsonl { source, .. } => source,
            Self::Slack {} => slack::SOURCE,
            Self::Github {} => github::SOURCE,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouterConfig {
    Scripted {},
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

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutorConfig {
    Scripted {},
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
        /// How `Plan::Swarm` Tickets run. Without it they fail.
        #[serde(default)]
        swarm: Option<Box<SwarmConfig>>,
        /// The harness of a session Ticket whose Plan names none.
        #[serde(default)]
        harness: HarnessKind,
        /// Where Claude Code is. Without it no session runs on Claude Code.
        #[serde(default)]
        claude: Option<Box<ClaudeConfig>>,
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
/// The most swarms one agent may run at once.
const MAX_SWARMS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    /// What the Router gives a `Plan::Session` Ticket.
    pub session_microusd: MicroUsd,
    /// The most a Pi Router's model calls may commit in one UTC day.
    #[serde(default)]
    pub router_daily_microusd: Option<MicroUsd>,
}

impl AgentConfig {
    /// Read and validate `path`. Relative paths in it are relative to its
    /// directory.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
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
                SourceConfig::Github { token_file, .. } => *token_file = base.join(&*token_file),
                SourceConfig::Schedule { .. } => {}
            }
        }
        for channel in &mut config.channels {
            match channel {
                ChannelConfig::Jsonl { path, .. } => *path = base.join(&*path),
                ChannelConfig::Slack {} | ChannelConfig::Github {} => {}
            }
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
            GoalTemplates::load(swarm, provider)?.check(config.budgets.session_microusd)?;
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
        let slack_channel = self.slack_channel();
        ensure!(
            !slack_channel || slack_sources == 1,
            "the slack channel needs the slack source's bot token"
        );
        self.validate_github()?;
        self.validate_schedules(&served)?;
        self.validate_watchdog(&served)?;
        ensure!(
            self.budgets.session_microusd > 0,
            "budgets.session_microusd must be above zero"
        );
        if self.budgets.session_microusd > self.policy.auto_approve_microusd {
            let gated = "budgets.session_microusd is above policy.auto_approve_microusd, so every \
                         Ticket waits for an approval";
            ensure!(
                slack_channel,
                "{gated}, and only the slack channel's buttons can take one"
            );
            ensure!(
                self.slack_approver(),
                "{gated}, and policy.approvers names nobody on slack"
            );
        }
        if let RouterConfig::Pi {
            provider,
            model,
            prices,
            max_input_bytes,
            max_output_tokens,
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
                self.budgets
                    .router_daily_microusd
                    .is_some_and(|cap| cap > 0),
                "a pi router needs budgets.router_daily_microusd above zero"
            );
            for name in rules.keys() {
                ensure!(
                    self.policy.rules.contains(name),
                    "router rule {name} is not an enabled policy rule"
                );
            }
        }
        self.validate_executor()
    }

    fn validate_executor(&self) -> Result<()> {
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
    /// `policy.auto_approve_microusd`: only the slack channel's buttons take
    /// approvals, from a `slack:` approver.
    pub fn takes_approvals(&self) -> bool {
        self.slack_channel() && self.slack_approver()
    }

    fn slack_channel(&self) -> bool {
        self.channels
            .iter()
            .any(|channel| matches!(channel, ChannelConfig::Slack {}))
    }

    fn slack_approver(&self) -> bool {
        self.policy
            .approvers
            .iter()
            .any(|who| who.starts_with("slack:"))
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentConfig, ChannelConfig, ClaudeConfig, ExecutorConfig, HarnessKind, JobLimits,
        RouterConfig, SourceConfig,
    };
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
        assert_eq!(config.budgets.session_microusd, 500_000);
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
            instructions,
            rules,
            ..
        } = &config.router
        else {
            panic!("{:?}", config.router)
        };
        assert_eq!(provider, "deepseek");
        assert_eq!((*max_input_bytes, *max_output_tokens), (1_048_576, 8_192));
        assert_eq!(*instructions, None);
        assert_eq!(rules["ci-red"], "Fix a red CI run on main.");
        assert_eq!(config.budgets.router_daily_microusd, Some(1_000_000));
    }

    #[test]
    fn a_pi_router_refuses_what_it_cannot_run_under() {
        for (case, yaml) in [
            (
                "no daily cap",
                pi(|yaml| yaml.replace(", router_daily_microusd: 1000000", "")),
            ),
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
        ] {
            assert!(load(yaml.clone()).is_err(), "{case}: accepted\n{yaml}");
        }
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
        let gated = VALID.replace("session_microusd: 500000", "session_microusd: 50000000");
        for (case, yaml) in [
            ("no source", no_source),
            ("one source twice", twice),
            ("zero budget", free),
            ("budget above the approval threshold", gated),
        ] {
            assert_ne!(yaml, VALID, "{case}: the case did not change the config");
            assert!(parse(&yaml).is_err(), "{case}: accepted");
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

    #[test]
    fn only_a_slack_channel_with_a_slack_approver_takes_approvals() {
        assert!(parse(SLACK).unwrap().takes_approvals());
        let elsewhere = SLACK.replace("approvers: [slack:U1]", "approvers: [github:U1]");
        assert!(!parse(&elsewhere).unwrap().takes_approvals());
        let unheard = VALID.replace(
            "bots: [github:ci]",
            "bots: [github:ci]\n  approvers: [slack:alice]",
        );
        assert!(!parse(&unheard).unwrap().takes_approvals());
    }
}
