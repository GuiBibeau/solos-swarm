//! The settings and checks of the `github` and `schedule` kinds.

use super::{AgentConfig, ChannelConfig, SourceConfig};
use crate::agent::schedule::Schedule;
use crate::agent::{github, slack};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use solos_agent_core::model::{ChannelRef, Ms, ThreadKey};
use std::collections::BTreeSet;

const MIN_POLL_SECS: u64 = 10;
const MIN_PERIOD_MS: Ms = 60_000;
const DAY_MS: Ms = 86_400_000;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    /// Names the schedule's Thread and Trigger ids: letters, digits, `-`,
    /// `_` and `.`.
    pub name: String,
    /// A fixed interval such as `30m`, `6h` or `1d`. Slots start at whole
    /// multiples of it since 1970-01-01T00:00Z, so `6h` fires at 00:00,
    /// 06:00, 12:00 and 18:00 UTC.
    #[serde(default)]
    pub every: Option<Interval>,
    /// A time of day in UTC, `HH:MM`.
    #[serde(default)]
    pub daily_at: Option<TimeOfDay>,
    /// What the agent is asked to do.
    pub text: String,
    /// Where its Tickets answer and ask. Without it nobody hears from them.
    #[serde(default)]
    pub reply_to: Option<ReplyTo>,
}

/// Milliseconds, written `<n>s`, `<n>m`, `<n>h` or `<n>d`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Interval(pub Ms);

impl TryFrom<String> for Interval {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        let invalid = || format!("invalid interval {text:?}: write it as 90s, 30m, 6h or 1d");
        let trimmed = text.trim();
        let unit = trimmed.chars().next_back().ok_or_else(invalid)?;
        let count = &trimmed[..trimmed.len() - unit.len_utf8()];
        let unit: Ms = match unit {
            's' => 1_000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => DAY_MS,
            _ => return Err(invalid()),
        };
        let count: Ms = count.parse().map_err(|_| invalid())?;
        count
            .checked_mul(unit)
            .filter(|ms| *ms > 0)
            .map(Self)
            .ok_or_else(invalid)
    }
}

/// Milliseconds after midnight UTC, written `HH:MM`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct TimeOfDay(pub Ms);

impl TryFrom<String> for TimeOfDay {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        let invalid = || format!("invalid time of day {text:?}: write it as HH:MM, in UTC");
        let (hour, minute) = text.trim().split_once(':').ok_or_else(invalid)?;
        let two = |part: &str| (part.len() == 2).then(|| part.parse::<Ms>().ok()).flatten();
        match (two(hour), two(minute)) {
            (Some(hour), Some(minute)) if hour < 24 && minute < 60 => {
                Ok(Self(hour * 3_600_000 + minute * 60_000))
            }
            _ => Err(invalid()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplyTo {
    /// A Slack channel id; with `thread_ts`, a thread in it.
    Slack {
        channel: String,
        #[serde(default)]
        thread_ts: Option<String>,
    },
    /// An issue or pull request.
    Github { repo: String, number: u64 },
}

impl ReplyTo {
    pub fn channel(&self) -> ChannelRef {
        match self {
            Self::Slack { channel, thread_ts } => slack::reply_to(channel, thread_ts.as_deref()),
            Self::Github { repo, number } => github::reply_to(repo, *number),
        }
    }

    /// The conversation's Thread, where an answer to a question arrives. A
    /// new Slack message has none until it is posted.
    pub fn thread(&self) -> Option<ThreadKey> {
        match self {
            Self::Slack { channel, thread_ts } => {
                thread_ts.as_deref().map(|ts| slack::thread(channel, ts))
            }
            Self::Github { repo, number } => Some(github::thread(repo, *number)),
        }
    }

    fn source(&self) -> &str {
        match self {
            Self::Slack { .. } => slack::SOURCE,
            Self::Github { .. } => github::SOURCE,
        }
    }
}

impl ScheduleConfig {
    pub fn schedule(&self) -> Result<Schedule> {
        let (period_ms, offset_ms) = match (self.every, self.daily_at) {
            (Some(Interval(every)), None) => (every, 0),
            (None, Some(TimeOfDay(at))) => (DAY_MS, at),
            _ => bail!("schedule {} needs one of every and daily_at", self.name),
        };
        Ok(Schedule {
            name: self.name.clone(),
            period_ms,
            offset_ms,
            text: self.text.clone(),
            reply_to: self.reply_to.as_ref().map(ReplyTo::channel),
            threads: self.reply_to.iter().filter_map(ReplyTo::thread).collect(),
        })
    }
}

impl AgentConfig {
    pub(super) fn validate_github(&self) -> Result<()> {
        let mut github_sources = 0;
        for source in &self.sources {
            let SourceConfig::Github {
                repos,
                api_base,
                poll_secs,
                ..
            } = source
            else {
                continue;
            };
            github_sources += 1;
            ensure!(!repos.is_empty(), "the github source reads no repository");
            for repo in repos {
                ensure!(
                    github::valid_repo(repo),
                    "github repository {repo:?} is not owner/name"
                );
            }
            let url = reqwest::Url::parse(api_base)
                .with_context(|| format!("invalid github api_base {api_base}"))?;
            ensure!(
                matches!(url.scheme(), "https" | "http"),
                "github api_base {api_base} is not an http(s) URL"
            );
            ensure!(
                *poll_secs >= MIN_POLL_SECS,
                "the github source must not poll more often than every {MIN_POLL_SECS}s"
            );
        }
        ensure!(
            github_sources <= 1,
            "the agent config names two github sources"
        );
        let github_channel = self
            .channels
            .iter()
            .any(|channel| matches!(channel, ChannelConfig::Github {}));
        ensure!(
            !github_channel || github_sources == 1,
            "the github channel needs the github source's token"
        );
        Ok(())
    }

    pub(super) fn validate_schedules(&self, served: &BTreeSet<&str>) -> Result<()> {
        let mut names = BTreeSet::new();
        for source in &self.sources {
            let SourceConfig::Schedule { schedules } = source else {
                continue;
            };
            ensure!(
                !schedules.is_empty(),
                "the schedule source names no schedule"
            );
            for config in schedules {
                let name = &config.name;
                let schedule = config.schedule()?;
                let named = !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
                ensure!(
                    named,
                    "schedule name {name:?} may hold only letters, digits, -, _ and ."
                );
                ensure!(names.insert(name), "two schedules are named {name}");
                ensure!(
                    schedule.period_ms >= MIN_PERIOD_MS,
                    "schedule {name} must not fire more often than once a minute"
                );
                ensure!(
                    !config.text.trim().is_empty(),
                    "schedule {name} has no text"
                );
                let Some(reply) = &config.reply_to else {
                    continue;
                };
                match reply {
                    ReplyTo::Github { repo, number } => ensure!(
                        github::valid_repo(repo) && *number > 0,
                        "schedule {name} replies on {repo}#{number}, which is not an issue"
                    ),
                    ReplyTo::Slack { channel, .. } => ensure!(
                        !channel.is_empty(),
                        "schedule {name} replies on slack but names no channel"
                    ),
                }
                ensure!(
                    served.contains(reply.source()),
                    "schedule {name} replies on {}, which no channel serves",
                    reply.source()
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{AgentConfig, ChannelConfig, SourceConfig};
    use crate::agent::{github, utc};

    fn parse(yaml: &str) -> anyhow::Result<AgentConfig> {
        let config: AgentConfig = serde_yaml::from_str(yaml)?;
        config.validate()?;
        Ok(config)
    }

    const GITHUB: &str = "
policy:
  people: [github:alice]
sources:
  - kind: github
    repos: [acme/widgets]
    token_file: secrets/github
  - kind: schedule
    schedules:
      - name: triage
        every: 6h
        text: Triage the new issues.
        reply_to: {kind: github, repo: acme/widgets, number: 1}
      - name: standup
        daily_at: \"09:30\"
        text: Summarise yesterday.
channels:
  - kind: github
router: {kind: scripted}
executor: {kind: scripted}
budgets: {session_microusd: 500000}
";

    #[test]
    fn a_github_and_schedule_config_loads_with_its_slots_parsed() {
        let dir = std::env::temp_dir().join(format!("agent-config-github-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.yaml");
        std::fs::write(&path, GITHUB).unwrap();
        let config = AgentConfig::load(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            config.sources[0],
            SourceConfig::Github {
                repos: ["acme/widgets".to_owned()].into(),
                token_file: dir.join("secrets/github"),
                api_base: "https://api.github.com/".into(),
                poll_secs: 60,
            }
        );
        let SourceConfig::Schedule { schedules } = &config.sources[1] else {
            panic!("{:?}", config.sources[1])
        };
        let triage = schedules[0].schedule().unwrap();
        assert_eq!((triage.period_ms, triage.offset_ms), (6 * 3_600_000, 0));
        assert_eq!(triage.reply_to, Some(github::reply_to("acme/widgets", 1)));
        assert_eq!(triage.threads, [github::thread("acme/widgets", 1)]);
        let standup = schedules[1].schedule().unwrap();
        assert_eq!(
            (standup.period_ms, standup.offset_ms),
            (86_400_000, utc::parse("1970-01-01T09:30:00Z").unwrap())
        );
        assert_eq!((standup.reply_to, standup.threads.len()), (None, 0));
        assert_eq!(config.channels, [ChannelConfig::Github {}]);
        assert_eq!(config.channels[0].source(), "github");
    }

    #[test]
    fn github_and_schedule_configs_refuse_what_cannot_run() {
        let schedule = "      - name: triage\n        every: 6h\n";
        let cases = [
            (
                "an unknown github field",
                GITHUB.replace("token_file:", "tokn: x\n    token_file:"),
            ),
            (
                "no repository",
                GITHUB.replace("repos: [acme/widgets]", "repos: []"),
            ),
            (
                "a repository path",
                GITHUB.replace("repos: [acme/widgets]", "repos: [acme/w/x]"),
            ),
            (
                "two github sources",
                GITHUB.replace(
                    "sources:\n",
                    "sources:\n  - {kind: github, repos: [a/b], token_file: t}\n",
                ),
            ),
            (
                "a github channel without a github source",
                GITHUB.replace(
                    "  - kind: github\n    repos: [acme/widgets]\n    token_file: secrets/github\n",
                    "",
                ),
            ),
            (
                "an api base that is not a URL",
                GITHUB.replace(
                    "token_file: secrets/github",
                    "token_file: t\n    api_base: nowhere",
                ),
            ),
            (
                "a poll every second",
                GITHUB.replace(
                    "token_file: secrets/github",
                    "token_file: t\n    poll_secs: 1",
                ),
            ),
            (
                "both every and daily_at",
                GITHUB.replace(
                    schedule,
                    &format!("{schedule}        daily_at: \"01:00\"\n"),
                ),
            ),
            (
                "neither every nor daily_at",
                GITHUB.replace("        every: 6h\n", ""),
            ),
            (
                "an interval under a minute",
                GITHUB.replace("every: 6h", "every: 30s"),
            ),
            (
                "an interval with no unit",
                GITHUB.replace("every: 6h", "every: 6"),
            ),
            ("a zero interval", GITHUB.replace("every: 6h", "every: 0m")),
            (
                "an hour past midnight",
                GITHUB.replace("\"09:30\"", "\"24:00\""),
            ),
            (
                "a time without minutes",
                GITHUB.replace("\"09:30\"", "\"9\""),
            ),
            (
                "two schedules of one name",
                GITHUB.replace("name: standup", "name: triage"),
            ),
            (
                "a name with a space",
                GITHUB.replace("name: standup", "name: stand up"),
            ),
            (
                "no text",
                GITHUB.replace("text: Summarise yesterday.", "text: \"  \""),
            ),
            (
                "an unknown schedule field",
                GITHUB.replace("every: 6h", "every: 6h\n        cron: x"),
            ),
            ("no schedule", {
                let start = GITHUB.find("    schedules:").unwrap();
                let end = GITHUB.find("channels:").unwrap();
                format!("{}    schedules: []\n{}", &GITHUB[..start], &GITHUB[end..])
            }),
            (
                "a reply on slack, which no channel serves",
                GITHUB.replace(
                    "{kind: github, repo: acme/widgets, number: 1}",
                    "{kind: slack, channel: C1}",
                ),
            ),
            (
                "a reply on issue zero",
                GITHUB.replace("number: 1}", "number: 0}"),
            ),
            (
                "an unknown reply kind",
                GITHUB.replace("{kind: github, repo:", "{kind: email, repo:"),
            ),
        ];
        for (case, yaml) in cases {
            assert_ne!(yaml, GITHUB, "{case}: the case did not change the config");
            assert!(parse(&yaml).is_err(), "{case}: accepted\n{yaml}");
        }
        parse(GITHUB).unwrap();
        let with_slack = GITHUB
            .replace(
                "{kind: github, repo: acme/widgets, number: 1}",
                "{kind: slack, channel: C1}",
            )
            .replace(
                "  - kind: github\nrouter",
                "  - kind: github\n  - {kind: jsonl, source: slack, path: o}\nrouter",
            );
        parse(&with_slack).unwrap();
    }
}
