//! The `watchdog` section: where alerts go, how often the Watchdog looks,
//! its thresholds, its one action and the external jobs it watches.

use super::{AgentConfig, Interval};
use crate::agent::watchdog::Thresholds;
use crate::agent::{discord, github, mcp, slack};
use anyhow::{Result, ensure};
use serde::Deserialize;
use solos_agent_core::model::{ChannelRef, Ms};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Most external jobs one Watchdog looks at.
pub const MAX_JOBS: usize = 64;
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const GIB: u64 = 1 << 30;

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
    /// Work that runs outside agent mode and is not a Ticket, which the
    /// Watchdog checks on every look.
    #[serde(default)]
    pub jobs: Vec<JobConfig>,
}

/// A conversation the operator reads, as a reply address: for Slack
/// `{source: slack, address: {channel: D0123}}`, with `thread_ts` for a
/// thread; for Discord `{source: discord, address: {channel: "123"}}`, with
/// `message_id` to reply to a message; for GitHub
/// `{source: github, address: {repo: o/r, number: "7"}}`.
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

/// One external job. Exactly one of `unit`, `tmux`, `pid_file` and `command`
/// says how it is checked; `progress` and `disk` add checks to any of them.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawJob")]
pub struct JobConfig {
    /// What alerts and the dashboard call it.
    pub name: String,
    pub check: JobCheck,
    pub progress: Option<Progress>,
    pub disk: Option<DiskFloor>,
}

/// How a job is known to be running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobCheck {
    /// A systemd user unit, active.
    Unit(String),
    /// A tmux session that exists.
    Tmux(String),
    /// A pid file naming a live process.
    PidFile(PathBuf),
    /// A program the owner wrote, run with no shell: exit 0 is healthy, 2
    /// is stalled and anything else is down.
    Command {
        argv: Vec<String>,
        timeout: Duration,
    },
}

impl JobCheck {
    /// How the job is checked, in a few words. A command shows its program
    /// and not its arguments, which may hold a secret.
    pub fn label(&self) -> String {
        match self {
            Self::Unit(unit) => format!("systemd unit {unit}"),
            Self::Tmux(session) => format!("tmux session {session}"),
            Self::PidFile(path) => format!("pid file {}", path.display()),
            Self::Command { argv, .. } => format!("status command {}", argv[0]),
        }
    }
}

/// A running job that writes a file as it works is stalled once the file
/// has not changed for `stalled_after`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub file: PathBuf,
    pub stalled_after: Interval,
}

/// Free space on the disk that holds `path`, below which the job is low on
/// disk.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskFloor {
    pub path: PathBuf,
    /// In GB (GiB, as `df -BG` counts).
    pub min_free_gb: u64,
}

impl DiskFloor {
    pub fn min_free_bytes(&self) -> u64 {
        self.min_free_gb.saturating_mul(GIB)
    }
}

impl Progress {
    pub fn stalled_after_ms(&self) -> Ms {
        self.stalled_after.0
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJob {
    name: String,
    unit: Option<String>,
    tmux: Option<String>,
    pid_file: Option<PathBuf>,
    command: Option<Vec<String>>,
    timeout: Option<Interval>,
    progress: Option<Progress>,
    disk: Option<DiskFloor>,
}

fn word(text: &str, extra: &[char], longest: usize) -> bool {
    !text.is_empty()
        && text.len() <= longest
        && text.starts_with(|c: char| c.is_ascii_alphanumeric())
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-') || extra.contains(&c))
}

fn absolute(path: &Path, what: &str) -> Result<(), String> {
    if path.is_absolute() && !path.to_string_lossy().contains('\0') {
        Ok(())
    } else {
        Err(format!(
            "{what} must be an absolute path, not {}",
            path.display()
        ))
    }
}

impl TryFrom<RawJob> for JobConfig {
    type Error = String;

    fn try_from(raw: RawJob) -> Result<Self, String> {
        let name = raw.name;
        let fail = |why: &str| format!("watchdog.jobs {name:?}: {why}");
        if !word(&name, &['.'], 64) {
            return Err(fail(
                "the name is 1 to 64 letters, digits, `-`, `_` or `.`, starting with a letter or digit",
            ));
        }
        if raw.timeout.is_some() && raw.command.is_none() {
            return Err(fail("timeout belongs to a command"));
        }
        let check = match (raw.unit, raw.tmux, raw.pid_file, raw.command) {
            (Some(unit), None, None, None) => {
                if !word(&unit, &['.', ':', '@', '\\'], 256) {
                    return Err(fail("unit is not a systemd unit name"));
                }
                JobCheck::Unit(unit)
            }
            (None, Some(session), None, None) => {
                if !word(&session, &[], 64) {
                    return Err(fail(
                        "tmux is a session name of 1 to 64 letters, digits, `-` and `_`",
                    ));
                }
                JobCheck::Tmux(session)
            }
            (None, None, Some(path), None) => {
                absolute(&path, "pid_file").map_err(|why| fail(&why))?;
                JobCheck::PidFile(path)
            }
            (None, None, None, Some(argv)) => {
                let program = argv.first().map(PathBuf::from);
                if !program.is_some_and(|program| program.is_absolute()) {
                    return Err(fail("command starts with the absolute path of a program"));
                }
                if argv.iter().any(|arg| arg.contains('\0')) {
                    return Err(fail("command holds a NUL"));
                }
                let timeout = raw
                    .timeout
                    .map_or(DEFAULT_COMMAND_TIMEOUT, |t| Duration::from_millis(t.0));
                if timeout > MAX_COMMAND_TIMEOUT {
                    return Err(fail("timeout is at most 2m"));
                }
                JobCheck::Command { argv, timeout }
            }
            _ => return Err(fail("give exactly one of unit, tmux, pid_file and command")),
        };
        if let Some(progress) = &raw.progress {
            absolute(&progress.file, "progress.file").map_err(|why| fail(&why))?;
        }
        if let Some(disk) = &raw.disk {
            absolute(&disk.path, "disk.path").map_err(|why| fail(&why))?;
            if disk.min_free_gb == 0 {
                return Err(fail("disk.min_free_gb must be at least 1"));
            }
        }
        Ok(Self {
            name,
            check,
            progress: raw.progress,
            disk: raw.disk,
        })
    }
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
            discord::SOURCE => ensure!(
                address
                    .get("channel")
                    .is_some_and(|id| discord::valid_id(id)),
                "watchdog.operator on discord needs a channel id"
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
        ensure!(
            watchdog.jobs.len() <= MAX_JOBS,
            "watchdog.jobs lists {} jobs; the most is {MAX_JOBS}",
            watchdog.jobs.len()
        );
        let mut names = BTreeSet::new();
        for job in &watchdog.jobs {
            ensure!(
                names.insert(job.name.as_str()),
                "watchdog.jobs names {} twice",
                job.name
            );
        }
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
                "a discord operator whose channel is not an id",
                format!(
                    "{}watchdog:\n  operator: {{source: discord, address: {{channel: general}}}}\n",
                    slack_base.replace("source: ops", "source: discord")
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

    const OPERATOR: &str = "  operator: {source: ops, address: {channel: D1}}\n";

    fn jobs(list: &str) -> String {
        with(&format!("{OPERATOR}  jobs:\n{list}"))
    }

    #[test]
    fn jobs_are_checked_one_of_four_ways_with_optional_progress_and_disk_checks() {
        use super::{DiskFloor, JobCheck, Progress};
        use crate::agent::config::Interval;
        use std::path::PathBuf;
        use std::time::Duration;

        let watchdog = parse(&jobs(
            "    - name: l2book
      unit: l2book-download.service
      progress: {file: /data/l2book/progress.log, stalled_after: 20m}
      disk: {path: /data/l2book, min_free_gb: 50}
    - {name: build, tmux: data-build}
    - {name: wave3, pid_file: /home/g/jobs/wave3.pid}
    - {name: train, command: [/home/g/bin/train-status, --token, s3cret], timeout: 20s}
    - {name: quick, command: [/bin/true]}
",
        ))
        .unwrap()
        .watchdog
        .unwrap();
        let [l2book, build, wave, train, quick] = &watchdog.jobs[..] else {
            panic!("{:?}", watchdog.jobs)
        };
        assert_eq!(
            l2book.check,
            JobCheck::Unit("l2book-download.service".into())
        );
        assert_eq!(
            l2book.progress,
            Some(Progress {
                file: PathBuf::from("/data/l2book/progress.log"),
                stalled_after: Interval(20 * 60_000),
            })
        );
        assert_eq!(
            l2book.disk,
            Some(DiskFloor {
                path: PathBuf::from("/data/l2book"),
                min_free_gb: 50,
            })
        );
        assert_eq!(build.check, JobCheck::Tmux("data-build".into()));
        assert_eq!(
            wave.check,
            JobCheck::PidFile("/home/g/jobs/wave3.pid".into())
        );
        assert_eq!((&build.progress, &build.disk), (&None, &None));
        assert_eq!(
            train.check,
            JobCheck::Command {
                argv: ["/home/g/bin/train-status", "--token", "s3cret"]
                    .map(String::from)
                    .to_vec(),
                timeout: Duration::from_secs(20),
            }
        );
        assert!(matches!(
            &quick.check,
            JobCheck::Command { timeout, .. } if *timeout == Duration::from_secs(10)
        ));
        assert_eq!(
            [&l2book.check, &build.check, &wave.check, &train.check].map(|check| check.label()),
            [
                "systemd unit l2book-download.service",
                "tmux session data-build",
                "pid file /home/g/jobs/wave3.pid",
                "status command /home/g/bin/train-status",
            ],
            "a command's arguments, which may hold a secret, are not shown"
        );
        let none = parse(&with(OPERATOR)).unwrap().watchdog.unwrap();
        assert!(none.jobs.is_empty());
    }

    #[test]
    fn a_job_that_could_not_be_checked_safely_is_refused() {
        let many: String = (0..=64)
            .map(|n| format!("    - {{name: j{n}, tmux: s{n}}}\n"))
            .collect();
        let long = format!("    - {{name: {}, tmux: a}}\n", "a".repeat(65));
        for (case, yaml) in [
            ("no way to check", jobs("    - {name: a}\n")),
            (
                "two ways",
                jobs("    - {name: a, unit: a.service, tmux: a}\n"),
            ),
            ("no name", jobs("    - {unit: a.service}\n")),
            (
                "a name with a space",
                jobs("    - {name: 'a b', tmux: a}\n"),
            ),
            (
                "a name that starts with a dash",
                jobs("    - {name: -a, tmux: a}\n"),
            ),
            ("a long name", jobs(&long)),
            (
                "the same name twice",
                jobs("    - {name: a, tmux: a}\n    - {name: a, tmux: b}\n"),
            ),
            ("too many jobs", jobs(&many)),
            (
                "an unknown field",
                jobs("    - {name: a, tmux: a, restart: true}\n"),
            ),
            (
                "a unit that is an option",
                jobs("    - {name: a, unit: --version}\n"),
            ),
            (
                "a unit with a slash",
                jobs("    - {name: a, unit: x/a.service}\n"),
            ),
            (
                "a tmux target that names a window",
                jobs("    - {name: a, tmux: 'a:0'}\n"),
            ),
            (
                "a tmux target with a dot",
                jobs("    - {name: a, tmux: a.b}\n"),
            ),
            (
                "a relative pid file",
                jobs("    - {name: a, pid_file: jobs/a.pid}\n"),
            ),
            (
                "a command with no program",
                jobs("    - {name: a, command: []}\n"),
            ),
            (
                "a command found on the path",
                jobs("    - {name: a, command: [status]}\n"),
            ),
            (
                "a command that is a string, for a shell",
                jobs("    - {name: a, command: '/bin/status --all'}\n"),
            ),
            (
                "a timeout on a unit",
                jobs("    - {name: a, unit: a.service, timeout: 5s}\n"),
            ),
            (
                "a timeout over two minutes",
                jobs("    - {name: a, command: [/bin/true], timeout: 3m}\n"),
            ),
            (
                "a relative progress file",
                jobs("    - {name: a, tmux: a, progress: {file: a.log, stalled_after: 5m}}\n"),
            ),
            (
                "a progress file with no limit",
                jobs("    - {name: a, tmux: a, progress: {file: /a.log}}\n"),
            ),
            (
                "a disk floor of zero",
                jobs("    - {name: a, tmux: a, disk: {path: /data, min_free_gb: 0}}\n"),
            ),
            (
                "a relative disk path",
                jobs("    - {name: a, tmux: a, disk: {path: data, min_free_gb: 5}}\n"),
            ),
        ] {
            assert!(parse(&yaml).is_err(), "{case}: accepted\n{yaml}");
        }
    }
}
