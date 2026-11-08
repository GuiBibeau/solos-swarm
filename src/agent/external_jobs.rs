//! External jobs: work that runs outside agent mode and is not a Ticket, such
//! as a downloader running as a systemd user unit, a data build in tmux, or a
//! training run that a status script reports on. The owner lists them under
//! `watchdog.jobs`; the Watchdog checks each one on every look and the
//! dashboard shows them.
//!
//! A job is [`Status::Up`], or it is down (not running), stalled (running but
//! not progressing) or low on disk. [`Checker`] asks the host, as the agent's
//! user: `systemctl --user show`, `tmux has-session`, `/proc`, a status
//! command the owner wrote, a progress file's age and the free space of a
//! disk. Every program is run with no shell, with a timeout and with its
//! output capped, and gets a short list of environment variables, not the
//! agent's.
//!
//! What the Watchdog remembers is [`STATE_FILE`] in the state directory: one
//! [`Record`] per job, with when its status began. That time is the
//! incident's identity, so a restart tells nothing twice, and the publisher
//! uploads the same file to the dashboard.

use super::config::{DiskFloor, JobCheck, JobConfig, Progress};
use super::health::{free_bytes, gb, parse_unit, write_private};
use super::utc;
use super::watchdog::{clip, duration};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use solos_agent_core::model::Ms;
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt};

/// What the Watchdog remembers about the jobs, in the state directory.
pub const STATE_FILE: &str = "watchdog-jobs.json";
/// The version of [`State`], which the dashboard reads too.
pub const FORMAT: u32 = 1;
/// How long `systemctl` and `tmux` may take, unless told otherwise.
const PROGRAM_TIMEOUT: Duration = Duration::from_secs(10);
/// What is kept of a program's output.
const OUTPUT_BYTES: usize = 4_096;
/// A pid file is one number.
const PID_FILE_BYTES: u64 = 64;
/// What a program is told of the environment. Everything else, such as the
/// agent's credentials, stays out.
const PASSED_ENV: [&str; 9] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TMUX_TMPDIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Up,
    /// Not running, or not answering.
    Down,
    /// Running, but its progress file is old or its status command says so.
    Stalled,
    /// Running, with less free space than its floor on its disk.
    LowDisk,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Stalled => "stalled",
            Self::LowDisk => "low_disk",
        }
    }

    /// As an operator reads it: "it had been down for 12m".
    fn phrase(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Stalled => "stalled",
            Self::LowDisk => "low on disk",
        }
    }
}

/// What one look at a job found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub status: Status,
    /// One line: what was found, whatever the status.
    pub detail: String,
}

impl Verdict {
    fn new(status: Status, detail: impl AsRef<str>) -> Self {
        Self {
            status,
            detail: clip(&printable(detail.as_ref())),
        }
    }

    fn up(detail: impl AsRef<str>) -> Self {
        Self::new(Status::Up, detail)
    }

    fn down(detail: impl AsRef<str>) -> Self {
        Self::new(Status::Down, detail)
    }

    fn stalled(detail: impl AsRef<str>) -> Self {
        Self::new(Status::Stalled, detail)
    }

    fn low_disk(detail: impl AsRef<str>) -> Self {
        Self::new(Status::LowDisk, detail)
    }
}

/// A job's output is read by an operator and shown on a page: no terminal
/// escapes.
fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_whitespace() || !c.is_control())
        .collect()
}

/// One job as the Watchdog last found it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub name: String,
    /// How it is checked: `systemd unit x.service`.
    pub check: String,
    pub status: Status,
    pub detail: String,
    /// When the job began to have this status, as the Watchdog saw it.
    pub since_ms: Ms,
}

/// [`STATE_FILE`]'s content, and what the dashboard reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub v: u32,
    /// When the Watchdog last looked.
    pub checked_ms: Ms,
    pub jobs: Vec<Record>,
}

impl State {
    /// The file's content, or `None` when there is none or it is not a
    /// state this build reads.
    pub fn load(state_dir: &Path) -> Option<Self> {
        let bytes = std::fs::read(state_dir.join(STATE_FILE)).ok()?;
        serde_json::from_slice::<Self>(&bytes)
            .ok()
            .filter(|state| state.v == FORMAT)
    }

    pub fn save(&self, state_dir: &Path) -> Result<()> {
        write_private(
            &state_dir.join(STATE_FILE),
            &serde_json::to_vec_pretty(self)?,
        )
    }
}

/// A job that was in trouble and is up again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovered {
    /// What it was last: the incident that ended.
    pub was: Record,
}

impl Recovered {
    /// What the operator is told.
    pub fn text(&self, now: Ms) -> String {
        format!(
            "The job \"{}\" ({}) is back up. It had been {} for {}.",
            self.was.name,
            self.was.check,
            self.was.status.phrase(),
            duration(now.saturating_sub(self.was.since_ms))
        )
    }
}

/// The state after a look at `jobs` found `verdicts` (one each, in order) at
/// `now`, given the state before. A job keeps the time its status began until
/// the status changes. A job that is no longer listed is dropped.
pub fn fold(
    before: Option<&State>,
    jobs: &[JobConfig],
    verdicts: Vec<Verdict>,
    now: Ms,
) -> (State, Vec<Recovered>) {
    let mut recovered = Vec::new();
    let records = jobs
        .iter()
        .zip(verdicts)
        .map(|(job, verdict)| {
            let was = before
                .into_iter()
                .flat_map(|state| &state.jobs)
                .find(|record| record.name == job.name);
            if let Some(was) = was
                && was.status != Status::Up
                && verdict.status == Status::Up
            {
                recovered.push(Recovered { was: was.clone() });
            }
            let since_ms = match was {
                Some(was) if was.status == verdict.status => was.since_ms,
                _ => now,
            };
            Record {
                name: job.name.clone(),
                check: job.check.label(),
                status: verdict.status,
                detail: verdict.detail,
                since_ms,
            }
        })
        .collect();
    let state = State {
        v: FORMAT,
        checked_ms: now,
        jobs: records,
    };
    (state, recovered)
}

/// Looks at jobs on this host.
#[derive(Clone, Debug)]
pub struct Checker {
    systemctl: PathBuf,
    tmux: PathBuf,
    /// How long `systemctl` and `tmux` may take.
    timeout: Duration,
}

impl Default for Checker {
    fn default() -> Self {
        Self::with_programs("systemctl", "tmux")
    }
}

impl Checker {
    /// A checker that runs these programs for systemd and tmux, found on
    /// `PATH` when they have no directory.
    pub fn with_programs(systemctl: impl Into<PathBuf>, tmux: impl Into<PathBuf>) -> Self {
        Self {
            systemctl: systemctl.into(),
            tmux: tmux.into(),
            timeout: PROGRAM_TIMEOUT,
        }
    }

    /// Give `systemctl` and `tmux` this long instead of 10 seconds.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Look at `jobs` and remember what was found in `state_dir`, where the
    /// publisher and the next look read it. Returns the jobs that were in
    /// trouble and are up now. With no jobs, a state that lists some is
    /// emptied, so a registry that was emptied leaves nothing on the
    /// dashboard.
    pub async fn look(
        &self,
        jobs: &[JobConfig],
        state_dir: &Path,
        now: Ms,
    ) -> Result<Vec<Recovered>> {
        let before = State::load(state_dir);
        if jobs.is_empty() {
            if before.is_some_and(|state| !state.jobs.is_empty()) {
                State {
                    v: FORMAT,
                    checked_ms: now,
                    jobs: Vec::new(),
                }
                .save(state_dir)?;
            }
            return Ok(Vec::new());
        }
        let verdicts = self.check_all(jobs, now).await;
        let (state, recovered) = fold(before.as_ref(), jobs, verdicts, now);
        state.save(state_dir)?;
        Ok(recovered)
    }

    /// Look at every job at once, each with its own timeout. One verdict per
    /// job, in order.
    pub async fn check_all(&self, jobs: &[JobConfig], now: Ms) -> Vec<Verdict> {
        futures_util::future::join_all(jobs.iter().map(|job| self.check(job, now))).await
    }

    /// Whether the job runs, then whether it progresses, then whether its
    /// disk has room.
    pub async fn check(&self, job: &JobConfig, now: Ms) -> Verdict {
        let running = match &job.check {
            JobCheck::Unit(unit) => self.unit(unit).await,
            JobCheck::Tmux(session) => self.tmux(session).await,
            JobCheck::PidFile(path) => pid_file(path),
            JobCheck::Command { argv, timeout } => command(argv, *timeout).await,
        };
        if running.status != Status::Up {
            return running;
        }
        if let Some(stalled) = job.progress.as_ref().and_then(|p| progress(p, now)) {
            return stalled;
        }
        if let Some(low) = job.disk.as_ref().and_then(low_disk) {
            return low;
        }
        running
    }

    async fn unit(&self, unit: &str) -> Verdict {
        let args = [
            "--user",
            "show",
            "--no-pager",
            "-p",
            "LoadState",
            "-p",
            "ActiveState",
            "-p",
            "SubState",
            unit,
        ];
        match run(&self.systemctl, &args, self.timeout).await {
            Ok(ran) if ran.code == Some(0) => {
                let state = parse_unit(&ran.stdout, None);
                if state.load_state == "not-found" {
                    Verdict::down("the unit is not loaded: it ended, or never started")
                } else if matches!(state.active_state.as_str(), "active" | "reloading") {
                    Verdict::up(format!("{} ({})", state.active_state, state.sub_state))
                } else {
                    Verdict::down(format!("{} ({})", state.active_state, state.sub_state))
                }
            }
            Ok(ran) => Verdict::down(format!(
                "systemctl --user show failed ({}): {}",
                ran.exit(),
                ran.line()
            )),
            Err(why) => Verdict::down(format!("systemctl {why}")),
        }
    }

    async fn tmux(&self, session: &str) -> Verdict {
        // `=` makes the name exact, where tmux would match a prefix.
        let target = format!("={session}");
        let args = ["has-session", "-t", target.as_str()];
        match run(&self.tmux, &args, self.timeout).await {
            Ok(ran) if ran.code == Some(0) => Verdict::up("the session exists"),
            Ok(ran) if ran.line().is_empty() => Verdict::down("the session does not exist"),
            Ok(ran) => Verdict::down(ran.line()),
            Err(why) => Verdict::down(format!("tmux {why}")),
        }
    }
}

/// Down unless the file names a live process. A pid the system gave to
/// another process since is not noticed: pair a pid file with `progress`.
fn pid_file(path: &Path) -> Verdict {
    let mut text = String::new();
    match std::fs::File::open(path)
        .and_then(|file| file.take(PID_FILE_BYTES).read_to_string(&mut text))
    {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Verdict::down("the pid file does not exist");
        }
        Err(err) => return Verdict::down(format!("cannot read the pid file: {err}")),
    }
    let Some(pid) = text
        .split_whitespace()
        .next()
        .and_then(|pid| pid.parse::<u32>().ok())
    else {
        return Verdict::down("the pid file holds no process id");
    };
    match process_state(pid) {
        None => Verdict::down(format!("process {pid} is not running")),
        Some('Z' | 'X') => Verdict::down(format!("process {pid} has died and is not reaped")),
        Some(_) => Verdict::up(format!("process {pid} is running")),
    }
}

/// The state letter in `/proc/<pid>/stat`, when the process exists.
fn process_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The state follows the command name, which may hold spaces or parentheses.
    stat.rsplit_once(')')?.1.trim_start().chars().next()
}

async fn command(argv: &[String], timeout: Duration) -> Verdict {
    let Some((program, args)) = argv.split_first() else {
        return Verdict::down("no status command is configured");
    };
    match run(program, args, timeout).await {
        Ok(ran) => {
            let line = ran.line();
            match ran.code {
                Some(0) if line.is_empty() => Verdict::up("the status command exited 0"),
                Some(0) => Verdict::up(line),
                Some(2) if line.is_empty() => Verdict::stalled("the status command exited 2"),
                Some(2) => Verdict::stalled(line),
                _ if line.is_empty() => Verdict::down(format!("the status command {}", ran.exit())),
                _ => Verdict::down(format!("the status command {}: {line}", ran.exit())),
            }
        }
        Err(why) => Verdict::down(format!("the status command {why}")),
    }
}

fn progress(progress: &Progress, now: Ms) -> Option<Verdict> {
    let file = progress.file.display();
    let modified = match std::fs::metadata(&progress.file).and_then(|meta| meta.modified()) {
        Ok(modified) => modified,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Some(Verdict::stalled(format!(
                "the progress file {file} does not exist"
            )));
        }
        Err(err) => {
            return Some(Verdict::stalled(format!(
                "cannot read the progress file {file}: {err}"
            )));
        }
    };
    let modified_ms = modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| Ms::try_from(since.as_millis()).ok())
        .unwrap_or_default();
    // The time it changed, not how long ago: the detail stays the same for as
    // long as the job stays stalled.
    (now.saturating_sub(modified_ms) >= progress.stalled_after_ms()).then(|| {
        Verdict::stalled(format!(
            "{file} has not changed since {} (limit {})",
            utc::format(modified_ms),
            duration(progress.stalled_after_ms())
        ))
    })
}

fn low_disk(floor: &DiskFloor) -> Option<Verdict> {
    let path = floor.path.display();
    match free_bytes(&floor.path) {
        Ok(free) if free >= floor.min_free_bytes() => None,
        Ok(free) => Some(Verdict::low_disk(format!(
            "{} free on {path} (floor {})",
            gb(free),
            gb(floor.min_free_bytes())
        ))),
        Err(err) => Some(Verdict::low_disk(format!(
            "cannot read the free space on {path}: {err:#}"
        ))),
    }
}

/// What a program did.
struct Ran {
    /// `None`: a signal ended it.
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Ran {
    fn exit(&self) -> String {
        match self.code {
            Some(code) => format!("exited {code}"),
            None => "was killed by a signal".to_owned(),
        }
    }

    /// The first line of output, from stdout, else from stderr.
    fn line(&self) -> String {
        [&self.stdout, &self.stderr]
            .into_iter()
            .find_map(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
            .unwrap_or_default()
            .to_owned()
    }
}

/// Run `program` with no shell, no input, a short environment and its own
/// process group, for at most `limit`. Past that, the whole group is killed.
async fn run<S: AsRef<OsStr>>(
    program: impl AsRef<OsStr>,
    args: &[S],
    limit: Duration,
) -> Result<Ran, String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(passed_env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|err| format!("cannot be run: {err}"))?;
    let group = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok());
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err("has no output to read".to_owned());
    };
    let finished = tokio::time::timeout(limit, async {
        tokio::join!(capture(stdout), capture(stderr), child.wait())
    })
    .await;
    match finished {
        Ok((stdout, stderr, status)) => Ok(Ran {
            code: status
                .map_err(|err| format!("cannot be waited for: {err}"))?
                .code(),
            stdout,
            stderr,
        }),
        Err(_) => {
            if let Some(group) = group {
                // SAFETY: killpg(2) takes no pointers. The group is the one
                // `process_group(0)` made for our child, which tokio has not
                // reaped (it clears the id when it does), so no other process
                // has the number.
                unsafe { libc::killpg(group, libc::SIGKILL) };
            }
            let _ = child.start_kill();
            Err(format!("gave no answer in {}", duration(millis(limit))))
        }
    }
}

fn millis(limit: Duration) -> Ms {
    Ms::try_from(limit.as_millis()).unwrap_or(Ms::MAX)
}

/// Read to the end, so the program never blocks on a full pipe, and keep the
/// first [`OUTPUT_BYTES`].
async fn capture(mut stream: impl AsyncRead + Unpin) -> String {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = OUTPUT_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..read.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

fn passed_env() -> Vec<(&'static str, std::ffi::OsString)> {
    let mut env: Vec<_> = PASSED_ENV
        .into_iter()
        .filter_map(|name| Some((name, std::env::var_os(name)?)))
        .collect();
    // As `agent health` does: a user manager's socket is under /run/user.
    if std::env::var_os("XDG_RUNTIME_DIR").is_none()
        && let Ok(me) = std::fs::metadata("/proc/self")
    {
        env.push(("XDG_RUNTIME_DIR", format!("/run/user/{}", me.uid()).into()));
    }
    env
}
