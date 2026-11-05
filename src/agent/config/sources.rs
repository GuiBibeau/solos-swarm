//! The settings and checks of the `discord`, `github` and `schedule` kinds.

use super::{AgentConfig, ChannelConfig, ForgeConfig, GITHUB_APP_DOC, SourceConfig};
use crate::agent::schedule::Schedule;
use crate::agent::{discord, github, slack};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use solos_agent_core::change::RepoName;
use solos_agent_core::model::{ChannelRef, Ms, ThreadKey};
use std::collections::BTreeSet;

const MIN_POLL_SECS: u64 = 10;
const MIN_RECONCILE_SECS: u64 = 60;
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
    /// A Discord channel id, or a Discord thread's id; with `message_id`, a
    /// reply to that message, whose replies are the Thread.
    Discord {
        #[serde(deserialize_with = "super::snowflake")]
        channel: String,
        #[serde(default, deserialize_with = "super::optional_snowflake")]
        message_id: Option<String>,
    },
    /// An issue or pull request.
    Github { repo: String, number: u64 },
}

impl ReplyTo {
    pub fn channel(&self) -> ChannelRef {
        match self {
            Self::Slack { channel, thread_ts } => slack::reply_to(channel, thread_ts.as_deref()),
            Self::Discord {
                channel,
                message_id,
            } => discord::reply_to(channel, message_id.as_deref()),
            Self::Github { repo, number } => github::reply_to(repo, *number),
        }
    }

    /// The conversation's Thread, where an answer to a question arrives. A
    /// new Slack or Discord message has none until it is posted.
    pub fn thread(&self) -> Option<ThreadKey> {
        match self {
            Self::Slack { channel, thread_ts } => {
                thread_ts.as_deref().map(|ts| slack::thread(channel, ts))
            }
            Self::Discord { message_id, .. } => message_id.as_deref().map(discord::thread),
            Self::Github { repo, number } => Some(github::thread(repo, *number)),
        }
    }

    fn source(&self) -> &str {
        match self {
            Self::Slack { .. } => slack::SOURCE,
            Self::Discord { .. } => discord::SOURCE,
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
    pub(super) fn validate_discord(&self) -> Result<()> {
        let mut discord_sources = 0;
        for source in &self.sources {
            let SourceConfig::Discord {
                guilds,
                channels,
                api_base,
                ..
            } = source
            else {
                continue;
            };
            discord_sources += 1;
            ensure!(
                !guilds.is_empty() || !channels.is_empty(),
                "the discord source listens to no guild or channel"
            );
            for id in guilds.iter().chain(channels) {
                ensure!(
                    discord::valid_id(id),
                    "discord id {id:?} is not a numeric id"
                );
            }
            let url = reqwest::Url::parse(api_base)
                .with_context(|| format!("invalid discord api_base {api_base}"))?;
            ensure!(
                matches!(url.scheme(), "https" | "http"),
                "discord api_base {api_base} is not an http(s) URL"
            );
        }
        ensure!(
            discord_sources <= 1,
            "the agent config names two discord sources"
        );
        ensure!(
            !self.discord_channel() || discord_sources == 1,
            "the discord channel needs the discord source's bot token"
        );
        Ok(())
    }

    pub(super) fn validate_github(&self) -> Result<()> {
        let github_sources = self
            .sources
            .iter()
            .filter(|source| matches!(source, SourceConfig::Github {}))
            .count();
        ensure!(
            github_sources <= 1,
            "the agent config names two github sources"
        );
        let github_channel = self
            .channels
            .iter()
            .any(|channel| matches!(channel, ChannelConfig::Github {}));
        let github_forge = matches!(self.forge, Some(ForgeConfig::Github { .. }));
        let used = github_sources == 1 || github_channel || github_forge;
        let Some(github) = &self.github else {
            ensure!(
                !used,
                "the github source, channel and forge act as a GitHub App: add a github: section \
                 ({GITHUB_APP_DOC})"
            );
            return self.validate_github_logins();
        };
        ensure!(
            used,
            "the github: section is unused: add a github source, a github channel or forge: \
             github, or remove it"
        );
        ensure!(github.app_id > 0, "github.app_id is the App's numeric id");
        ensure!(
            !github.key_file.as_os_str().is_empty(),
            "github.key_file is empty"
        );
        ensure!(
            github.poll_secs >= MIN_POLL_SECS,
            "github.poll_secs must not poll more often than every {MIN_POLL_SECS}s"
        );
        ensure!(
            github.reconcile_secs >= MIN_RECONCILE_SECS,
            "github.reconcile_secs must be at least {MIN_RECONCILE_SECS}"
        );
        let url = reqwest::Url::parse(&github.api_base)
            .with_context(|| format!("invalid github.api_base {}", github.api_base))?;
        ensure!(
            matches!(url.scheme(), "https" | "http"),
            "github.api_base {} is not an http(s) URL",
            github.api_base
        );
        let url = reqwest::Url::parse(&github.git_base)
            .with_context(|| format!("invalid github.git_base {}", github.git_base))?;
        ensure!(
            matches!(url.scheme(), "https" | "http" | "file"),
            "github.git_base {} is not an https://, http:// or file:// URL",
            github.git_base
        );
        // The App's tokens are minted for forge.repos alone, so every
        // repository the agent names on GitHub is one of them.
        let Some(forge) = &self.forge else {
            bail!(
                "the github: section acts on forge.repos, the one list of repositories: add a \
                 forge: section ({GITHUB_APP_DOC})"
            );
        };
        ensure!(
            github_sources == 0 || !forge.repos().is_empty(),
            "the github source reads forge.repos, which is empty"
        );
        let listed = |repo: &str| {
            RepoName::try_from(repo.to_owned()).is_ok_and(|repo| forge.repos().contains_key(&repo))
        };
        if github_channel {
            for source in &self.sources {
                let SourceConfig::Schedule { schedules } = source else {
                    continue;
                };
                for schedule in schedules {
                    if let Some(ReplyTo::Github { repo, .. }) = &schedule.reply_to
                        && github::valid_repo(repo)
                    {
                        ensure!(
                            listed(repo),
                            "schedule {} replies on {repo}, which is not in forge.repos, so the \
                             App holds no token for it",
                            schedule.name
                        );
                    }
                }
            }
            if let Some(watchdog) = &self.watchdog
                && watchdog.operator.source == github::SOURCE
                && let Some(repo) = watchdog.operator.address.get("repo")
                && github::valid_repo(repo)
            {
                ensure!(
                    listed(repo),
                    "watchdog.operator is on {repo}, which is not in forge.repos, so the App \
                     holds no token for it"
                );
            }
        }
        self.validate_github_logins()
    }

    /// The Policy's `github:` entries are logins, which are pinned to their
    /// accounts at start.
    fn validate_github_logins(&self) -> Result<()> {
        let policy = &self.policy;
        for who in policy
            .people
            .iter()
            .chain(&policy.approvers)
            .chain(&policy.bots)
        {
            let Some(login) = who.strip_prefix("github:") else {
                continue;
            };
            let name = login.strip_suffix("[bot]").unwrap_or(login);
            ensure!(
                !name.is_empty()
                    && name.len() <= 39
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
                "policy lists github:{login}, which is not a GitHub login"
            );
        }
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
                    ReplyTo::Discord {
                        channel,
                        message_id,
                    } => ensure!(
                        discord::valid_id(channel)
                            && message_id.as_deref().is_none_or(discord::valid_id),
                        "schedule {name} replies on discord with an id that is not numeric"
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
    use crate::agent::{discord, github, utc};

    fn parse(yaml: &str) -> anyhow::Result<AgentConfig> {
        let config: AgentConfig = serde_yaml::from_str(yaml)?;
        config.validate()?;
        Ok(config)
    }

    const GITHUB: &str = "
policy:
  people: [github:alice]
github:
  app_id: 123456
  key_file: secrets/github-app.pem
sources:
  - kind: github
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
executor: {kind: pi, provider: deepseek, model: deepseek-flash, prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}, run_code: {}}
forge:
  kind: github
  repos:
    acme/widgets: {about: \"Widgets.\", base: main}
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
        assert_eq!(config.sources[0], SourceConfig::Github {});
        let app = config.github.unwrap();
        assert_eq!(
            (app.app_id, app.key_file, app.poll_secs, app.writes),
            (123456, dir.join("secrets/github-app.pem"), 60, true)
        );
        let repos: Vec<String> = config
            .forge
            .unwrap()
            .repos()
            .keys()
            .map(ToString::to_string)
            .collect();
        assert_eq!(repos, ["acme/widgets"], "the source reads forge.repos");
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
    fn a_schedule_replies_on_discord_in_a_channel_or_to_a_message_and_checks_its_ids() {
        let discord = GITHUB
            .replace(
                "{kind: github, repo: acme/widgets, number: 1}",
                "{kind: discord, channel: 1200000000000000001, message_id: 1700000000000000001}",
            )
            .replace(
                "  - kind: github\nrouter",
                "  - kind: github\n  - {kind: jsonl, source: discord, path: o}\nrouter",
            );
        let config = parse(&discord).unwrap();
        let SourceConfig::Schedule { schedules } = &config.sources[1] else {
            panic!("{:?}", config.sources[1])
        };
        let triage = schedules[0].schedule().unwrap();
        assert_eq!(
            triage.reply_to,
            Some(discord::reply_to(
                "1200000000000000001",
                Some("1700000000000000001")
            ))
        );
        assert_eq!(triage.threads, [discord::thread("1700000000000000001")]);
        let in_a_channel = discord.replace(", message_id: 1700000000000000001", "");
        let config = parse(&in_a_channel).unwrap();
        let SourceConfig::Schedule { schedules } = &config.sources[1] else {
            panic!("{:?}", config.sources[1])
        };
        let triage = schedules[0].schedule().unwrap();
        assert_eq!(
            triage.reply_to,
            Some(discord::reply_to("1200000000000000001", None))
        );
        assert!(
            triage.threads.is_empty(),
            "a new message has no Thread until it is posted"
        );
        for (case, yaml) in [
            (
                "a channel that is not an id",
                discord.replace("channel: 1200000000000000001", "channel: ops"),
            ),
            (
                "a message that is not an id",
                discord.replace("message_id: 1700000000000000001", "message_id: last"),
            ),
            (
                "a reply no channel serves",
                discord.replace("source: discord", "source: other"),
            ),
        ] {
            assert_ne!(yaml, discord, "{case}: the case did not change the config");
            assert!(parse(&yaml).is_err(), "{case}: accepted\n{yaml}");
        }
    }

    #[test]
    fn github_and_schedule_configs_refuse_what_cannot_run() {
        let schedule = "      - name: triage\n        every: 6h\n";
        let cases = [
            (
                "an unknown github field",
                GITHUB.replace("app_id: 123456", "app_id: 123456\n  tokn: x"),
            ),
            (
                "no repository",
                GITHUB.replace(
                    "  repos:\n    acme/widgets: {about: \"Widgets.\", base: main}\n",
                    "  repos: {}\n",
                ),
            ),
            (
                "a repository path",
                GITHUB.replace("acme/widgets: {", "acme/w/x: {"),
            ),
            (
                "two github sources",
                GITHUB.replace("sources:\n", "sources:\n  - kind: github\n"),
            ),
            (
                "a github source with a field",
                GITHUB.replace("  - kind: github\n  - kind: schedule", "  - {kind: github, repos: [a/b]}\n  - kind: schedule"),
            ),
            (
                "a github channel without the github: section",
                GITHUB.replace(
                    "github:\n  app_id: 123456\n  key_file: secrets/github-app.pem\n",
                    "",
                ),
            ),
            (
                "a github source without the forge whose repos it reads",
                GITHUB.replace(
                    "forge:\n  kind: github\n  repos:\n    acme/widgets: {about: \"Widgets.\", base: main}\n",
                    "",
                ),
            ),
            (
                "an api base that is not a URL",
                GITHUB.replace(
                    "key_file: secrets/github-app.pem",
                    "key_file: secrets/github-app.pem\n  api_base: nowhere",
                ),
            ),
            (
                "a poll every second",
                GITHUB.replace(
                    "key_file: secrets/github-app.pem",
                    "key_file: secrets/github-app.pem\n  poll_secs: 1",
                ),
            ),
            (
                "a schedule that replies where the App holds no token",
                GITHUB.replace("repo: acme/widgets, number: 1", "repo: acme/other, number: 1"),
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
                "a watchdog that alerts where the App holds no token",
                format!(
                    "{GITHUB}watchdog:\n  operator: {{source: github, address: {{repo: acme/other, number: \"3\"}}}}\n"
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
