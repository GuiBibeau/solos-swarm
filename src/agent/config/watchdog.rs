//! The `watchdog` section: where alerts go, how often the Watchdog looks,
//! its thresholds and its one action.

use super::{AgentConfig, Interval};
use crate::agent::watchdog::Thresholds;
use crate::agent::{github, mcp, slack};
use anyhow::{Result, ensure};
use serde::Deserialize;
use solos_agent_core::model::ChannelRef;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchdogConfig {
    /// Where alerts go, through the channel that serves its source.
    pub operator: OperatorConfig,
    /// How often it looks.
    #[serde(default = "default_every")]
    pub every: Interval,
    /// Retry a stalled Assignment, once per Ticket.
    #[serde(default)]
    pub retry_stalled: bool,
    #[serde(default)]
    pub thresholds: ThresholdsConfig,
}

/// A conversation the operator reads, as a reply address: for Slack
/// `{source: slack, address: {channel: D0123}}`, with `thread_ts` for a
/// thread; for GitHub `{source: github, address: {repo: o/r, number: "7"}}`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorConfig {
    pub source: String,
    pub address: BTreeMap<String, String>,
}

/// Each threshold left out keeps its default ([`Thresholds::default`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThresholdsConfig {
    pub waiting: Option<Interval>,
    pub stalled: Option<Interval>,
    pub failed_attempts: Option<u32>,
    pub unavailable: Option<u32>,
    pub unavailable_within: Option<Interval>,
    pub spend_percent: Option<u64>,
    pub paused: Option<Interval>,
    pub repeat: Option<Interval>,
}

fn default_every() -> Interval {
    Interval(60_000)
}

impl WatchdogConfig {
    pub fn thresholds(&self) -> Thresholds {
        let default = Thresholds::default();
        let given = &self.thresholds;
        let ms = |interval: Option<Interval>, default| interval.map_or(default, |i| i.0);
        Thresholds {
            waiting_ms: ms(given.waiting, default.waiting_ms),
            stalled_ms: ms(given.stalled, default.stalled_ms),
            failed_attempts: given.failed_attempts.unwrap_or(default.failed_attempts),
            unavailable: given.unavailable.unwrap_or(default.unavailable),
            unavailable_within_ms: ms(given.unavailable_within, default.unavailable_within_ms),
            spend_percent: given.spend_percent.unwrap_or(default.spend_percent),
            paused_ms: ms(given.paused, default.paused_ms),
            repeat_ms: ms(given.repeat, default.repeat_ms),
        }
    }

    pub fn operator(&self) -> ChannelRef {
        ChannelRef {
            source: self.operator.source.clone(),
            address: self.operator.address.clone(),
        }
    }

    pub fn every(&self) -> Duration {
        Duration::from_millis(self.every.0)
    }
}

impl AgentConfig {
    pub(super) fn validate_watchdog(&self, served: &BTreeSet<&str>) -> Result<()> {
        let Some(watchdog) = &self.watchdog else {
            return Ok(());
        };
        let OperatorConfig { source, address } = &watchdog.operator;
        ensure!(
            served.contains(source.as_str()),
            "watchdog.operator is on {source}, which no channel serves"
        );
        let named = |key: &str| address.get(key).is_some_and(|value| !value.is_empty());
        match source.as_str() {
            slack::SOURCE => ensure!(
                named("channel"),
                "watchdog.operator on slack names no channel"
            ),
            github::SOURCE => ensure!(
                address
                    .get("repo")
                    .is_some_and(|repo| github::valid_repo(repo))
                    && address
                        .get("number")
                        .and_then(|n| n.parse::<u64>().ok())
                        .is_some_and(|n| n > 0),
                "watchdog.operator on github needs a repo (owner/name) and an issue number"
            ),
            mcp::SOURCE => ensure!(
                address.get("operator").is_some_and(|o| mcp::valid_name(o))
                    && address.get("thread").is_some_and(|t| mcp::valid_name(t)),
                "watchdog.operator on mcp needs an operator and a thread"
            ),
            _ => ensure!(!address.is_empty(), "watchdog.operator has no address"),
        }
        let limits = watchdog.thresholds();
        ensure!(
            limits.failed_attempts > 0 && limits.unavailable > 0,
            "watchdog.thresholds.failed_attempts and unavailable must be at least 1"
        );
        ensure!(
            (1..=100).contains(&limits.spend_percent),
            "watchdog.thresholds.spend_percent must be 1..=100"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::AgentConfig;
    use crate::agent::watchdog::Thresholds;

    const BASE: &str = "
policy:
  people: [slack:alice]
sources:
  - kind: jsonl
    path: triggers.jsonl
channels:
  - kind: jsonl
    source: slack
    path: outbox.jsonl
  - kind: jsonl
    source: ops
    path: ops.jsonl
router: {kind: scripted}
executor: {kind: scripted}
budgets: {session_microusd: 500000}
";

    fn parse(yaml: &str) -> anyhow::Result<AgentConfig> {
        let config: AgentConfig = serde_yaml::from_str(yaml)?;
        config.validate()?;
        Ok(config)
    }

    fn with(watchdog: &str) -> String {
        format!("{BASE}watchdog:\n{watchdog}")
    }

    #[test]
    fn no_watchdog_section_means_no_watchdog() {
        assert_eq!(parse(BASE).unwrap().watchdog, None);
    }

    #[test]
    fn a_watchdog_loads_with_its_defaults_and_overrides() {
        let config = parse(&with("  operator: {source: ops, address: {channel: D1}}\n")).unwrap();
        let watchdog = config.watchdog.unwrap();
        assert_eq!(watchdog.thresholds(), Thresholds::default());
        assert_eq!(watchdog.every().as_secs(), 60);
        assert!(!watchdog.retry_stalled);
        assert_eq!(watchdog.operator().address["channel"], "D1");

        let tuned = parse(&with(
            "  operator: {source: ops, address: {channel: D1}}
  every: 5s
  retry_stalled: true
  thresholds: {stalled: 10m, failed_attempts: 5, spend_percent: 90, repeat: 1d}
",
        ))
        .unwrap()
        .watchdog
        .unwrap();
        let limits = tuned.thresholds();
        assert_eq!(
            (
                limits.stalled_ms,
                limits.failed_attempts,
                limits.spend_percent
            ),
            (600_000, 5, 90)
        );
        assert_eq!(limits.repeat_ms, 86_400_000);
        assert_eq!(limits.waiting_ms, Thresholds::default().waiting_ms);
        assert!(tuned.retry_stalled);
        assert_eq!(tuned.every().as_secs(), 5);
    }

    #[test]
    fn a_watchdog_refuses_what_cannot_reach_an_operator_or_never_fires() {
        let slack_base = BASE.replace(
            "  - kind: jsonl\n    source: slack\n    path: outbox.jsonl\n",
            "",
        );
        for (case, yaml) in [
            (
                "an operator no channel serves",
                with("  operator: {source: email, address: {to: ops}}\n"),
            ),
            (
                "an operator with no address",
                with("  operator: {source: ops, address: {}}\n"),
            ),
            (
                "an unknown field",
                with("  operator: {source: ops, address: {channel: D1}}\n  retry: true\n"),
            ),
            (
                "an unknown threshold",
                with(
                    "  operator: {source: ops, address: {channel: D1}}\n  thresholds: {stall: 1m}\n",
                ),
            ),
            (
                "a zero interval",
                with("  operator: {source: ops, address: {channel: D1}}\n  every: 0s\n"),
            ),
            (
                "zero failed attempts",
                with(
                    "  operator: {source: ops, address: {channel: D1}}\n  thresholds: {failed_attempts: 0}\n",
                ),
            ),
            (
                "a share above the cap",
                with(
                    "  operator: {source: ops, address: {channel: D1}}\n  thresholds: {spend_percent: 101}\n",
                ),
            ),
            (
                "a slack operator with no channel",
                format!(
                    "{}watchdog:\n  operator: {{source: slack, address: {{thread_ts: '1.2'}}}}\n",
                    slack_base.replace("source: ops", "source: slack")
                ),
            ),
        ] {
            assert!(parse(&yaml).is_err(), "{case}: accepted\n{yaml}");
        }
    }
}
