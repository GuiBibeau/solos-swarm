//! `solos-swarm agent health`: a check that runs outside the agent, so
//! someone hears when `agent serve` itself is down. The Watchdog runs inside
//! serve and cannot say that.
//!
//! systemd runs it from a timer every few minutes
//! (`scripts/systemd/solos-agent-health.timer`) and, through `OnFailure=`, at
//! once when the agent's unit gives up. Each run asks these questions, none
//! of which needs the agent to answer for itself:
//!
//! - is the unit active, and is it restart-looping (systemd's `NRestarts`
//!   moved by `restart_burst` since the last run)?
//! - does the broker socket answer `GET /api/hello`?
//! - is there free space above the floor on the state directory's disk? If
//!   not, it pauses new work by writing `PAUSE` (which `agent serve` and its
//!   broker already honour) and lifts that pause again, only if it made it,
//!   once space returns.
//! - can an alert be delivered: can the bot token read the Watchdog's Discord
//!   channel?
//! - where `executor.claude` names a `claude` that is installed, is the host's
//!   Claude Code still signed in? `claude auth status` answers from what is
//!   stored on disk: it starts no session and spends no tokens, and this
//!   check reads only its `loggedIn` and `loginExpiresAt`.
//!
//! A failure posts once per incident to that channel with the bot token, with
//! no help from serve, and says so again only for a problem of a new kind; the
//! return to health posts once more. What it remembers is
//! `health-state.json` in the state directory. `agent-deploy` writes
//! `health-hold` (an expiry time) while it stops and starts the agent, so a
//! deploy does not alert; the hold expires by itself, so a deploy that went
//! wrong does.
//!
//! The probes and the Discord channel are traits, so the logic is tested
//! with a fake unit, a fake socket and a fake Discord.

use super::config::{AgentConfig, ExecutorConfig, HarnessKind, HealthConfig, SourceConfig};
use super::discord::{self, DEFAULT_API_BASE, Reply, Token};
use super::ports::BoxFuture;
use super::{utc, watchdog};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use solos_agent_core::model::Ms;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What the check remembers between runs, in the state directory.
pub const STATE_FILE: &str = "health-state.json";
/// While its expiry (Unix seconds, as text) is in the future, problems are
/// recorded and not told: written by `agent-deploy` around a restart.
pub const HOLD_FILE: &str = "health-hold";
/// The pause gate `agent serve` and its broker already read.
pub const PAUSE_FILE: &str = "PAUSE";
/// What a PAUSE this check wrote starts with, so it never lifts anyone else's.
const PAUSE_MARK: &str = "paused by solos-agent-health";
/// A unit this young is still starting: its socket may not be up yet.
pub const STARTUP_GRACE_SECS: u64 = 60;
const SOCKET_WAIT: Duration = Duration::from_secs(5);
/// `claude auth status` takes a fraction of a second. A hang must end well
/// inside the unit's `TimeoutStartSec`, so it is reported and does not get
/// the whole check killed.
const CLAUDE_WAIT: Duration = Duration::from_secs(20);
/// Discord refuses a message past 2000 characters.
const MESSAGE_CHARS: usize = 1_900;
const LINE_CHARS: usize = 300;

/// What `systemctl --user show` says of the agent's unit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnitState {
    /// `loaded`, or `not-found` when the unit file is gone.
    pub load_state: String,
    /// `active`, `inactive`, `failed`, `activating`, ...
    pub active_state: String,
    /// `running`, `dead`, `failed`, `auto-restart`, ...
    pub sub_state: String,
    /// Automatic restarts so far; systemd resets it when the unit is
    /// stopped or restarted by hand.
    pub restarts: Option<u64>,
    /// How long it has been active, when systemd says.
    pub uptime_secs: Option<u64>,
}

/// What `claude auth status` says of the login Claude Code holds on this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaudeLogin {
    pub logged_in: bool,
    /// When the stored sign-in ends, Unix ms: Claude Code 2.1.318 and later
    /// say. After it the login cannot be renewed, and every run fails with
    /// "OAuth session expired and could not be refreshed".
    pub ends_ms: Option<Ms>,
}

/// What the check asks the machine.
pub trait Probe {
    fn unit(&self) -> Result<UnitState>;
    /// `GET /api/hello` on the broker's Unix socket answered 200.
    fn broker(&self) -> Result<()>;
    /// Bytes free to an ordinary process on the state directory's disk.
    fn free_bytes(&self) -> Result<u64>;
    /// The host's Claude Code login. `None`: there is none to check, because
    /// the config names no `claude`, or an optional one is not installed.
    fn claude_login(&self) -> Option<Result<ClaudeLogin>> {
        None
    }
}

/// The alert channel: Discord, with the bot token.
pub trait Alerts: Send + Sync {
    /// Whether a post can be delivered: the network, the token and the
    /// channel are all as they should be. Sends nothing.
    fn check(&self) -> BoxFuture<'_, Result<()>>;
    fn post(&self, text: &str) -> BoxFuture<'_, Result<()>>;
}

#[derive(Clone, Debug)]
pub struct Settings {
    /// `solos-agent`.
    pub unit: String,
    /// The machine's name, for the message.
    pub host: String,
    /// Below this the disk guard pauses new work. `0`: no guard.
    pub min_free_bytes: u64,
    /// At or above this a pause the guard made is lifted.
    pub resume_free_bytes: u64,
    pub restart_burst: u64,
}

/// What kind of trouble: an incident is told once per kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Unit,
    CrashLoop,
    Socket,
    Disk,
    Discord,
    /// agent.yaml no longer loads, so the agent cannot start with it.
    Config,
    /// Claude Code on this host is signed out, or its sign-in has ended.
    Claude,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Failure {
    pub kind: Kind,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// A stretch of trouble, from the first failing run to the first clean one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Incident {
    pub since_ms: Ms,
    /// The kinds a post has already said.
    pub announced: BTreeSet<Kind>,
}

/// Where alerts go: remembered from the last config that loaded, so a config
/// that no longer loads (the likeliest reason for a crash loop right after an
/// edit) can still be told.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub channel: String,
    pub api_base: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub target: Option<Target>,
    pub incident: Option<Incident>,
    /// `NRestarts` at the last run, to tell a loop from one old restart.
    pub restarts_seen: Option<u64>,
    /// This check wrote the PAUSE file, and so lifts it.
    pub disk_paused: bool,
}

impl State {
    /// The state file's content, or the default when it is missing or damaged
    /// (a lost state costs at most one repeated alert).
    pub fn load(state_dir: &Path) -> Self {
        std::fs::read(state_dir.join(STATE_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, state_dir: &Path) -> Result<()> {
        write_private(
            &state_dir.join(STATE_FILE),
            &serde_json::to_vec_pretty(self)?,
        )
    }
}

/// What a run did.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub at_ms: Ms,
    pub dry_run: bool,
    pub ok: bool,
    pub checks: Vec<Check>,
    /// What was posted: `down`, `recovered`, or nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub posted: Option<&'static str>,
    /// Why a post that was due did not go.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_error: Option<String>,
    /// Why no post was due though something failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quiet: Option<&'static str>,
    /// This run wrote or lifted the PAUSE file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause: Option<&'static str>,
}

pub struct Run<'a> {
    pub state_dir: &'a Path,
    pub settings: &'a Settings,
    pub probe: &'a dyn Probe,
    /// `None`: no token, so no post (and the check reports as skipped).
    pub alerts: Option<&'a dyn Alerts>,
    /// The target the config that loaded names, to remember.
    pub target: Option<&'a Target>,
    /// Why agent.yaml did not load, when it did not.
    pub config_error: Option<&'a str>,
    /// Check and report, and change nothing: no post, no pause, no state.
    pub dry_run: bool,
    pub now_ms: Ms,
}

/// One run of the check.
pub async fn run(ctx: &Run<'_>) -> Result<Report> {
    let Run {
        state_dir,
        settings,
        probe,
        alerts,
        target,
        config_error,
        dry_run,
        now_ms,
    } = *ctx;
    let mut state = State::load(state_dir);
    if let Some(target) = target {
        state.target = Some(target.clone());
    }
    let mut checks = Vec::new();
    let mut failing = Vec::new();
    let loads = match config_error {
        None => Ok("agent.yaml loads".to_owned()),
        Some(why) => Err(format!(
            "agent.yaml does not load, so the agent cannot start with it: {why}"
        )),
    };
    record(&mut checks, &mut failing, "config", Kind::Config, loads);

    let unit = probe.unit();
    let active = unit
        .as_ref()
        .is_ok_and(|unit| unit.active_state == "active");
    let found = unit_check(&settings.unit, &unit);
    record(&mut checks, &mut failing, "unit", Kind::Unit, found);
    if let Ok(unit) = &unit {
        let (looping, seen) = restart_check(settings, unit, state.restarts_seen);
        state.restarts_seen = seen;
        if let Some(text) = looping {
            record(
                &mut checks,
                &mut failing,
                "restarts",
                Kind::CrashLoop,
                Err(text),
            );
        }
    }
    let starting = unit
        .as_ref()
        .ok()
        .and_then(|unit| unit.uptime_secs)
        .is_some_and(|up| up < STARTUP_GRACE_SECS);
    if active && !starting {
        let found = probe
            .broker()
            .map(|()| "GET /api/hello answered 200".to_owned())
            .map_err(|err| format!("the broker socket does not answer: {err:#}"));
        record(&mut checks, &mut failing, "broker", Kind::Socket, found);
    } else if active {
        checks.push(Check {
            name: "broker",
            ok: true,
            detail: "the unit is starting; not asked yet".into(),
        });
    }

    let free = probe.free_bytes();
    let disk = disk_check(settings, &free, state.disk_paused);
    let low = disk.is_err() && free.is_ok();
    record(&mut checks, &mut failing, "disk", Kind::Disk, disk);
    let mut pause = None;
    if let (true, Ok(free), false) = (low, &free, dry_run) {
        pause = guard(state_dir, &mut state, settings, *free)?;
        if let Some(failure) = failing.iter_mut().find(|f| f.kind == Kind::Disk) {
            failure.text.push_str(&format!(
                " New work is paused ({PAUSE_FILE} in the state directory); it resumes at {} free.",
                gb(settings.resume_free_bytes)
            ));
        }
    } else if !low && state.disk_paused && free.is_ok() && !dry_run {
        pause = lift(state_dir, &mut state)?;
    }

    if let Some(login) = probe.claude_login() {
        let found = claude_check(&login, now_ms);
        record(&mut checks, &mut failing, "claude", Kind::Claude, found);
    }

    match alerts {
        Some(alerts) => {
            let found = alerts
                .check()
                .await
                .map(|()| "the bot can read the alert channel".to_owned())
                .map_err(|err| format!("alerts cannot be delivered to Discord: {err:#}"));
            record(&mut checks, &mut failing, "discord", Kind::Discord, found);
        }
        None => checks.push(Check {
            name: "discord",
            ok: true,
            detail: "skipped: no token file".into(),
        }),
    }

    let held = hold_active(state_dir, now_ms / 1_000);
    let notice = decide(&mut state, &failing, held, now_ms, settings);
    let mut report = Report {
        at_ms: now_ms,
        dry_run,
        ok: failing.is_empty(),
        checks,
        posted: None,
        post_error: None,
        quiet: None,
        pause,
    };
    if !failing.is_empty() && notice.is_none() {
        report.quiet = Some(if held {
            "held: a deploy or a stop is in progress"
        } else {
            "already told"
        });
    }
    if let (Some(notice), false) = (&notice, dry_run) {
        match (alerts, notice) {
            (None, _) => report.post_error = Some("no Discord token, so nothing was posted".into()),
            (Some(_), Notice::Down { kinds, .. }) if *kinds == BTreeSet::from([Kind::Discord]) => {
                report.post_error = Some("only Discord itself is failing: nowhere to tell".into());
            }
            (Some(alerts), notice) => match alerts.post(notice.text()).await {
                Ok(()) => {
                    report.posted = Some(notice.label());
                    if let (Notice::Down { kinds, .. }, Some(incident)) =
                        (notice, &mut state.incident)
                    {
                        incident.announced.extend(kinds.iter().copied());
                    }
                }
                Err(err) => report.post_error = Some(format!("{err:#}")),
            },
        }
    }
    if !dry_run {
        state.save(state_dir)?;
    }
    Ok(report)
}

fn record(
    checks: &mut Vec<Check>,
    failing: &mut Vec<Failure>,
    name: &'static str,
    kind: Kind,
    found: Result<String, String>,
) {
    match found {
        Ok(detail) => checks.push(Check {
            name,
            ok: true,
            detail,
        }),
        Err(text) => {
            checks.push(Check {
                name,
                ok: false,
                detail: text.clone(),
            });
            failing.push(Failure { kind, text });
        }
    }
}

fn unit_check(name: &str, unit: &Result<UnitState>) -> Result<String, String> {
    match unit {
        Err(err) => Err(format!("cannot ask systemd about {name}: {err:#}")),
        Ok(unit) if unit.load_state == "not-found" => {
            Err(format!("the unit {name} is not installed"))
        }
        Ok(unit) if unit.active_state == "active" => {
            Ok(format!("{name} is active ({})", unit.sub_state))
        }
        Ok(unit) if unit.sub_state == "auto-restart" => Err(format!(
            "{name} exited and systemd is restarting it ({} / {})",
            unit.active_state, unit.sub_state
        )),
        Ok(unit) => Err(format!(
            "{name} is {} ({})",
            unit.active_state, unit.sub_state
        )),
    }
}

/// A crash loop, if `unit` restarted itself `restart_burst` times since the
/// count was last read; and the count to remember. The first run has nothing
/// to compare with and says nothing; a count that went down was reset by a
/// manual restart, so what it shows now is all since then.
pub fn restart_check(
    settings: &Settings,
    unit: &UnitState,
    seen: Option<u64>,
) -> (Option<String>, Option<u64>) {
    let Some(now) = unit.restarts else {
        return (None, seen);
    };
    let since = match seen {
        None => 0,
        Some(before) if now >= before => now - before,
        Some(_) => now,
    };
    let text = (since >= settings.restart_burst).then(|| {
        format!(
            "{} restarted itself {since} times since the last check: it is crash-looping",
            settings.unit
        )
    });
    (text, Some(now))
}

fn disk_check(settings: &Settings, free: &Result<u64>, paused: bool) -> Result<String, String> {
    let free = match free {
        Ok(free) => *free,
        Err(err) => return Err(format!("cannot read the free space: {err:#}")),
    };
    let left = gb(free);
    if settings.min_free_bytes == 0 {
        return Ok(format!("{left} free (no floor set)"));
    }
    if free < settings.min_free_bytes {
        return Err(format!(
            "only {left} free on the state directory's disk (floor {}).",
            gb(settings.min_free_bytes)
        ));
    }
    if paused && free < settings.resume_free_bytes {
        return Err(format!(
            "{left} free: above the floor but below {}, where the pause it made lifts.",
            gb(settings.resume_free_bytes)
        ));
    }
    Ok(format!("{left} free"))
}

/// Whether the login is good at `now_ms`. Signed out, and past its end date,
/// are the two failures `claude auth status` shows. A session its server
/// revoked early, with the date still ahead, it cannot show.
fn claude_check(login: &Result<ClaudeLogin>, now_ms: Ms) -> Result<String, String> {
    const RELOGIN: &str = "Sign in again as the agent's user with `claude auth login`.";
    let login = match login {
        Ok(login) => login,
        Err(err) => return Err(format!("cannot ask Claude Code about its login: {err:#}")),
    };
    if !login.logged_in {
        return Err(format!(
            "Claude Code is not signed in on this host. {RELOGIN}"
        ));
    }
    match login.ends_ms {
        Some(ends) if ends <= now_ms => Err(format!(
            "the Claude Code sign-in on this host ended at {}, so its runs fail with \"OAuth \
             session expired\". {RELOGIN}",
            utc::format(ends)
        )),
        Some(ends) => Ok(format!("signed in until {}", utc::format(ends))),
        None => Ok("signed in".to_owned()),
    }
}

pub(super) fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / f64::from(1u32 << 30))
}

/// Pause new work: write `PAUSE`, unless something is already there. Returns
/// what it did.
fn guard(
    state_dir: &Path,
    state: &mut State,
    settings: &Settings,
    free: u64,
) -> Result<Option<&'static str>> {
    let path = state_dir.join(PAUSE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) if text.starts_with(PAUSE_MARK) => {
            state.disk_paused = true;
            Ok(None)
        }
        // A person's own pause: it stays theirs.
        Ok(_) => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let note = format!(
                "{PAUSE_MARK}: {} free on the state directory's disk, floor {}, at {}\n",
                gb(free),
                gb(settings.min_free_bytes),
                utc::format(crate::broker::now_ms())
            );
            write_private(&path, note.as_bytes())?;
            state.disk_paused = true;
            Ok(Some("paused"))
        }
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Lift a pause this check made, and only that.
fn lift(state_dir: &Path, state: &mut State) -> Result<Option<&'static str>> {
    let path = state_dir.join(PAUSE_FILE);
    state.disk_paused = false;
    match std::fs::read_to_string(&path) {
        Ok(text) if text.starts_with(PAUSE_MARK) => {
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
            Ok(Some("resumed"))
        }
        _ => Ok(None),
    }
}

/// What a run owes the channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    Down { text: String, kinds: BTreeSet<Kind> },
    Recovered { text: String },
}

impl Notice {
    pub fn text(&self) -> &str {
        match self {
            Self::Down { text, .. } | Self::Recovered { text } => text,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Down { .. } => "down",
            Self::Recovered { .. } => "recovered",
        }
    }
}

/// Open, extend or close the incident and say what to post: once for an
/// incident, again only for a kind of trouble it has not yet told, and once
/// at the end if it told anything. `held`: record, tell nothing. The caller
/// marks kinds told once the post is delivered, so a post that fails is
/// tried again next run.
pub fn decide(
    state: &mut State,
    failing: &[Failure],
    held: bool,
    now_ms: Ms,
    settings: &Settings,
) -> Option<Notice> {
    if failing.is_empty() {
        let incident = state.incident.take()?;
        if incident.announced.is_empty() {
            return None;
        }
        return Some(Notice::Recovered {
            text: format!(
                "{} on {} is healthy again after {}.",
                settings.unit,
                settings.host,
                watchdog::duration(now_ms.saturating_sub(incident.since_ms))
            ),
        });
    }
    let incident = state.incident.get_or_insert_with(|| Incident {
        since_ms: now_ms,
        announced: BTreeSet::new(),
    });
    if held {
        return None;
    }
    if failing
        .iter()
        .all(|failure| incident.announced.contains(&failure.kind))
    {
        return None;
    }
    let mut text = format!(
        "{} health check failed on {}:\n",
        settings.unit, settings.host
    );
    for failure in failing {
        let line: String = failure.text.chars().take(LINE_CHARS).collect();
        text.push_str(&format!("- {line}\n"));
    }
    text.push_str(&format!(
        "Since {}. This check runs outside the agent, so the agent's own Watchdog cannot report it. \
         Look at: scripts/agent-deploy status, then scripts/agent-deploy logs.",
        utc::format(incident.since_ms)
    ));
    let text: String = text.chars().take(MESSAGE_CHARS).collect();
    Some(Notice::Down {
        text,
        kinds: failing.iter().map(|failure| failure.kind).collect(),
    })
}

/// Whether `agent-deploy`'s hold is in force at `now_secs`.
pub fn hold_active(state_dir: &Path, now_secs: u64) -> bool {
    std::fs::read_to_string(state_dir.join(HOLD_FILE))
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .is_some_and(|until| until > now_secs)
}

pub(super) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("cannot write {}", temp.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("cannot write {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("cannot replace {}", path.display()))
}

/// The probes on this machine: systemd's answer about `unit`, the broker's
/// socket, `statvfs` on the state directory, and Claude Code's own account of
/// its login.
pub struct SystemProbe {
    pub unit: String,
    pub socket: PathBuf,
    pub state_dir: PathBuf,
    /// `None`: the config names no `claude`.
    pub claude: Option<ClaudeInstall>,
}

impl Probe for SystemProbe {
    fn unit(&self) -> Result<UnitState> {
        let mut systemctl = Command::new("systemctl");
        systemctl
            .args(["--user", "show", "--no-pager", &self.unit])
            .args([
                "-p",
                "LoadState",
                "-p",
                "ActiveState",
                "-p",
                "SubState",
                "-p",
                "NRestarts",
                "-p",
                "ActiveEnterTimestampMonotonic",
            ]);
        if std::env::var_os("XDG_RUNTIME_DIR").is_none()
            && let Ok(me) = std::fs::metadata("/proc/self")
        {
            systemctl.env("XDG_RUNTIME_DIR", format!("/run/user/{}", me.uid()));
        }
        let output = systemctl.output().context("cannot run systemctl --user")?;
        ensure!(
            output.status.success(),
            "systemctl --user show failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(parse_unit(
            &String::from_utf8_lossy(&output.stdout),
            boot_secs(),
        ))
    }

    fn broker(&self) -> Result<()> {
        hello(&self.socket)
    }

    fn free_bytes(&self) -> Result<u64> {
        free_bytes(&self.state_dir)
    }

    fn claude_login(&self) -> Option<Result<ClaudeLogin>> {
        self.claude.as_ref()?.ask()
    }
}

/// The `claude` that `executor.claude` names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeInstall {
    /// The `claude` executable, or with `node`, the npm package's `cli.js`.
    pub binary: PathBuf,
    pub node: Option<PathBuf>,
    /// `executor.harness` is `claude-code`, so a missing install is trouble.
    /// Otherwise only a Plan may name it, and a missing one is skipped.
    pub required: bool,
}

/// Where the config says Claude Code is, if it does.
pub fn claude_install(config: &AgentConfig) -> Option<ClaudeInstall> {
    let ExecutorConfig::Pi {
        harness,
        claude: Some(claude),
        ..
    } = &config.executor
    else {
        return None;
    };
    Some(ClaudeInstall {
        binary: claude.binary.clone(),
        node: claude.node.clone(),
        required: *harness == HarnessKind::ClaudeCode,
    })
}

impl ClaudeInstall {
    /// The login, or `None` when this install is optional and not there.
    pub fn ask(&self) -> Option<Result<ClaudeLogin>> {
        let missing = [Some(&self.binary), self.node.as_ref()]
            .into_iter()
            .flatten()
            .find(|path| !path.exists());
        match missing {
            None => Some(self.login()),
            Some(path) if self.required => Some(Err(anyhow::anyhow!(
                "{} does not exist, and executor.harness is claude-code",
                path.display()
            ))),
            Some(_) => None,
        }
    }

    /// `claude auth status --json`. The updater and telemetry are off so a
    /// check that runs every few minutes never changes the install or phones
    /// home.
    fn status_command(&self) -> Command {
        let mut command = match &self.node {
            Some(node) => {
                let mut command = Command::new(node);
                command.arg(&self.binary);
                command
            }
            None => Command::new(&self.binary),
        };
        command
            .args(["auth", "status", "--json"])
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("DISABLE_AUTOUPDATER", "1");
        command
    }

    /// Its status prints JSON and exits 1 when signed out, so the answer is
    /// read from the output whatever the exit code.
    fn login(&self) -> Result<ClaudeLogin> {
        let output = output_within(self.status_command(), CLAUDE_WAIT)
            .with_context(|| format!("{} auth status", self.binary.display()))?;
        parse_claude_status(&String::from_utf8_lossy(&output.stdout))
            .with_context(|| format!("`claude auth status` ({})", output.status))
    }
}

/// `claude auth status --json` as a [`ClaudeLogin`]. It also prints an email
/// and an organisation; only `loggedIn` and `loginExpiresAt` are read, so
/// nothing else is kept or put in an error.
pub fn parse_claude_status(text: &str) -> Result<ClaudeLogin> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Status {
        logged_in: bool,
        #[serde(default)]
        login_expires_at: Option<String>,
    }
    let status: Status =
        serde_json::from_str(text).context("did not print JSON with a `loggedIn` field")?;
    let ends_ms = status
        .login_expires_at
        .map(|at| {
            utc::parse(&at).with_context(|| format!("gave a `loginExpiresAt` it cannot read: {at}"))
        })
        .transpose()?;
    Ok(ClaudeLogin {
        logged_in: status.logged_in,
        ends_ms,
    })
}

/// `command`'s output, or an error if it has not ended in `limit`, in which
/// case it is killed. The output is small, so it is read after the exit.
fn output_within(mut command: Command, limit: Duration) -> Result<std::process::Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot start")?;
    let deadline = Instant::now() + limit;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("no answer in {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(child.wait_with_output()?)
}

/// `systemctl show`'s `Key=value` lines as a [`UnitState`]. `boot_secs` is
/// how long the machine has been up, to turn the unit's monotonic start time
/// into an age.
pub fn parse_unit(text: &str, boot_secs: Option<u64>) -> UnitState {
    let mut unit = UnitState::default();
    let mut entered_us = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "LoadState" => unit.load_state = value.to_owned(),
            "ActiveState" => unit.active_state = value.to_owned(),
            "SubState" => unit.sub_state = value.to_owned(),
            "NRestarts" => unit.restarts = value.parse().ok(),
            "ActiveEnterTimestampMonotonic" => entered_us = value.parse::<u64>().ok(),
            _ => {}
        }
    }
    unit.uptime_secs = match (entered_us, boot_secs) {
        (Some(entered), Some(boot)) if entered > 0 => {
            Some(boot.saturating_sub(entered / 1_000_000))
        }
        _ => None,
    };
    unit
}

fn boot_secs() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    text.split_whitespace()
        .next()?
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// `GET /api/hello` on a Unix socket, answered `200`.
pub fn hello(socket: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("cannot connect to {}", socket.display()))?;
    stream.set_read_timeout(Some(SOCKET_WAIT))?;
    stream.set_write_timeout(Some(SOCKET_WAIT))?;
    stream.write_all(b"GET /api/hello HTTP/1.1\r\nHost: broker\r\nConnection: close\r\n\r\n")?;
    let mut status = String::new();
    BufReader::new(stream.take(4_096))
        .read_line(&mut status)
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                anyhow::anyhow!("no answer in {} s", SOCKET_WAIT.as_secs())
            }
            _ => anyhow::Error::new(err),
        })?;
    let status = status.trim();
    ensure!(
        status.split_whitespace().nth(1) == Some("200"),
        "GET /api/hello answered {status:?}"
    );
    Ok(())
}

/// Bytes free to an ordinary process on `path`'s filesystem.
#[allow(clippy::unnecessary_cast)]
pub fn free_bytes(path: &Path) -> Result<u64> {
    let path = CString::new(path.as_os_str().as_bytes()).context("a path with a NUL")?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string that lives through the call,
    // and statvfs writes only into `stat`.
    let result = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("statvfs failed");
    }
    // SAFETY: statvfs returned 0, so it filled `stat` whole.
    let stat = unsafe { stat.assume_init() };
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

/// Discord, through the bot token: the Watchdog's channel.
pub struct DiscordAlerts {
    api: discord::DiscordApi,
    channel: String,
}

impl DiscordAlerts {
    pub fn new(api_base: &str, token: Token, channel: &str) -> Result<Self> {
        ensure!(
            discord::valid_id(channel),
            "{channel:?} is not a Discord channel id"
        );
        Ok(Self {
            api: discord::DiscordApi::new(api_base, token)?,
            channel: channel.to_owned(),
        })
    }
}

impl Alerts for DiscordAlerts {
    fn check(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            match self.api.get_channel(&self.channel).await? {
                Reply::Ok(_) => Ok(()),
                Reply::Refused(refusal) => {
                    bail!("Discord will not show channel {}: {refusal}", self.channel)
                }
            }
        })
    }

    fn post(&self, text: &str) -> BoxFuture<'_, Result<()>> {
        let body = json!({"content": text, "allowed_mentions": {"parse": []}});
        Box::pin(async move {
            match self.api.create_message(&self.channel, &body).await? {
                Reply::Ok(_) => Ok(()),
                Reply::Refused(refusal) => {
                    bail!("Discord refused the message to {}: {refusal}", self.channel)
                }
            }
        })
    }
}

/// The bot token in `path`, read and kept: the file is the unit's own copy in
/// its runtime directory, which goes when the run ends.
pub fn read_token(path: &Path) -> Result<Token> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the Discord token file {}", path.display()))?;
    let token = raw.trim();
    ensure!(
        !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()),
        "the Discord token in {} is empty or holds a space or a control character",
        path.display()
    );
    Ok(Token::new(token))
}

/// Where the config sends alerts: the Watchdog's Discord channel, and the
/// API root its Discord source uses.
pub fn alert_target(config: &AgentConfig) -> Result<(String, String)> {
    let watchdog = config.watchdog.as_ref().context(
        "agent.yaml has no watchdog section: there is no channel to tell. Add \
         `watchdog: {operator: {source: discord, address: {channel: <id>}}}`",
    )?;
    let operator = watchdog.operator();
    ensure!(
        operator.source == discord::SOURCE,
        "watchdog.operator is on {}: this check posts to Discord only",
        operator.source
    );
    let channel = operator
        .address
        .get("channel")
        .context("watchdog.operator on discord names no channel")?
        .clone();
    let base = config
        .sources
        .iter()
        .find_map(|source| match source {
            SourceConfig::Discord { api_base, .. } => Some(api_base.clone()),
            _ => None,
        })
        .unwrap_or_else(|| DEFAULT_API_BASE.to_owned());
    Ok((channel, base))
}

/// An alert channel that cannot be reached, for the reason given: the check
/// fails on it, so the unit shows failed and the journal says why.
pub struct Unavailable(pub String);

impl Alerts for Unavailable {
    fn check(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { bail!("{}", self.0) })
    }

    fn post(&self, _: &str) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { bail!("{}", self.0) })
    }
}

/// What `solos-swarm agent health` is given.
#[derive(Clone, Debug)]
pub struct Args {
    pub config: PathBuf,
    pub state_dir: PathBuf,
    pub unit: String,
    pub token_file: Option<PathBuf>,
    pub dry_run: bool,
}

/// One run on this machine: agent.yaml and the token file say where to tell,
/// systemd, the socket and the disk say how the agent is.
///
/// A config that does not load is itself a failure, told to the channel the
/// last good config named. A token or a channel that is missing makes
/// "alerts cannot be delivered" the failure, since a check that cannot tell
/// anyone has to at least fail loudly in the journal.
pub async fn check(args: &Args, now_ms: Ms) -> Result<Report> {
    let loaded = AgentConfig::load(&args.config);
    let remembered = State::load(&args.state_dir).target;
    let (health, fresh, config_error) = match &loaded {
        Ok(config) => (
            config.health.clone().unwrap_or_default(),
            alert_target(config).map(|(channel, api_base)| Target { channel, api_base }),
            None,
        ),
        Err(err) => (
            HealthConfig::default(),
            Err(anyhow::anyhow!("agent.yaml does not load")),
            Some(format!("{err:#}")),
        ),
    };
    let target = match (&fresh, remembered) {
        (Ok(fresh), _) => Ok(fresh.clone()),
        (Err(_), Some(kept)) => Ok(kept),
        (Err(why), None) => Err(format!("{why:#}")),
    };
    let alerts: Option<Box<dyn Alerts>> = args.token_file.as_deref().map(|path| {
        let made = match &target {
            Ok(target) => read_token(path)
                .and_then(|token| DiscordAlerts::new(&target.api_base, token, &target.channel))
                .map(|alerts| Box::new(alerts) as Box<dyn Alerts>)
                .map_err(|err| format!("{err:#}")),
            Err(why) => Err(why.clone()),
        };
        made.unwrap_or_else(|why| Box::new(Unavailable(why)))
    });
    let settings = Settings {
        unit: args.unit.clone(),
        host: host_name(),
        min_free_bytes: health.min_free_bytes(),
        resume_free_bytes: health.resume_free_bytes(),
        restart_burst: health.restart_burst,
    };
    let probe = SystemProbe {
        unit: args.unit.clone(),
        socket: args.state_dir.join(super::broker::SOCKET_FILE),
        state_dir: args.state_dir.clone(),
        claude: loaded.as_ref().ok().and_then(claude_install),
    };
    run(&Run {
        state_dir: &args.state_dir,
        settings: &settings,
        probe: &probe,
        alerts: alerts.as_deref(),
        target: fresh.as_ref().ok(),
        config_error: config_error.as_deref(),
        dry_run: args.dry_run,
        now_ms,
    })
    .await
}

/// The machine's name for a message.
pub fn host_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_owned())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this box".to_owned())
}

/// The report as text.
pub fn render(report: &Report) -> String {
    let mut out = format!(
        "agent health at {}{}: {}\n",
        utc::format(report.at_ms),
        if report.dry_run { " (dry run)" } else { "" },
        if report.ok { "healthy" } else { "FAILING" }
    );
    for check in &report.checks {
        out.push_str(&format!(
            "  {} {}: {}\n",
            if check.ok { "ok  " } else { "FAIL" },
            check.name,
            check.detail
        ));
    }
    if let Some(pause) = report.pause {
        out.push_str(&format!("  disk guard: {pause} new work\n"));
    }
    if let Some(what) = report.posted {
        out.push_str(&format!("  posted: {what}\n"));
    } else if let Some(why) = &report.post_error {
        out.push_str(&format!("  not posted: {why}\n"));
    } else if let Some(why) = report.quiet {
        out.push_str(&format!("  not posted: {why}\n"));
    }
    out
}

#[cfg(test)]
mod tests;
