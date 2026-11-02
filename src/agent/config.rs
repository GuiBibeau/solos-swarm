//! `agent.yaml`. Unknown fields are errors, like a swarm Goal's, so a typo
//! cannot silently fall back to a default.

use super::slack;
use crate::gateway::{PriceTable, upstream_profile};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use solos_agent_core::AllowlistPolicy;
use solos_agent_core::model::MicroUsd;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub policy: AllowlistPolicy,
    pub sources: Vec<SourceConfig>,
    pub channels: Vec<ChannelConfig>,
    pub router: RouterConfig,
    pub executor: ExecutorConfig,
    pub budgets: Budgets,
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
}

fn slack_api_base() -> String {
    slack::DEFAULT_API_BASE.to_owned()
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
}

impl ChannelConfig {
    pub fn source(&self) -> &str {
        match self {
            Self::Jsonl { source, .. } => source,
            Self::Slack {} => slack::SOURCE,
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
}

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
            }
        }
        for channel in &mut config.channels {
            match channel {
                ChannelConfig::Jsonl { path, .. } => *path = base.join(&*path),
                ChannelConfig::Slack {} => {}
            }
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
        let slack_channel = self
            .channels
            .iter()
            .any(|channel| matches!(channel, ChannelConfig::Slack {}));
        ensure!(
            !slack_channel || slack_sources == 1,
            "the slack channel needs the slack source's bot token"
        );
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
                self.policy
                    .approvers
                    .iter()
                    .any(|who| who.starts_with("slack:")),
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentConfig, ChannelConfig, RouterConfig, SourceConfig};

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
}
