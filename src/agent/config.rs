//! `agent.yaml`. Unknown fields are errors, like a swarm Goal's, so a typo
//! cannot silently fall back to a default.

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
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChannelConfig {
    /// Appends each message to `path`. Serves replies whose `ChannelRef`
    /// names `source`.
    Jsonl { source: String, path: PathBuf },
}

impl ChannelConfig {
    pub fn source(&self) -> &str {
        match self {
            Self::Jsonl { source, .. } => source,
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
            }
        }
        for channel in &mut config.channels {
            match channel {
                ChannelConfig::Jsonl { path, .. } => *path = base.join(&*path),
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
        ensure!(
            self.budgets.session_microusd > 0,
            "budgets.session_microusd must be above zero"
        );
        ensure!(
            self.budgets.session_microusd <= self.policy.auto_approve_microusd,
            "budgets.session_microusd is above policy.auto_approve_microusd, so every Ticket \
             would wait for an approval that agent serve cannot take yet"
        );
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
}
