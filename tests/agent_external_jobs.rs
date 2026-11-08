//! External jobs: work outside agent mode that the Watchdog checks. The
//! checks run real processes (a fake `systemctl` and `tmux` that read a
//! directory the test controls, `/bin/sh` as a status command, a pid file, a
//! progress file and a disk). The last test is the acceptance scenario: a
//! systemd unit and a tmux session are registered, stopping one posts one
//! alert, and it clears when it comes back.

use serde_json::{Value, json};
use solos_agent_core::model::{MessageKind, OutboundMessage};
use solos_swarm::agent::config::JobConfig;
use solos_swarm::agent::external_jobs::{Checker, FORMAT, Status, Verdict, fold};
use solos_swarm::agent::external_jobs::{STATE_FILE, State};
use solos_swarm::agent::jsonl::JsonlChannel;
use solos_swarm::agent::watchdog::{AlertKind, Known, Watchdog};
use solos_swarm::agent::{AgentConfig, status};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

const T0: u64 = 1_793_600_000_000;
const MIN: u64 = 60_000;
const HOUR: u64 = 60 * MIN;
const ACTIVE: &str = "LoadState=loaded\nActiveState=active\nSubState=running\n";
const INACTIVE: &str = "LoadState=loaded\nActiveState=inactive\nSubState=dead\n";

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::var_os("SOLOS_SCRATCH")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let root = base.join(format!(
            "agent-jobs-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        let scratch = Self { root };
        // What the fakes report: `systemctl.out` is what `systemctl show` prints,
        // and each file `session-<name>` is a live tmux session.
        scratch.script(
            "systemctl",
            "d=$(dirname \"$0\")
echo \"$@\" > \"$d/systemctl.args\"
[ -e \"$d/systemctl.hang\" ] && sleep 30
cat \"$d/systemctl.out\" 2>/dev/null",
        );
        scratch.script(
            "tmux",
            "d=$(dirname \"$0\")
echo \"$@\" > \"$d/tmux.args\"
name=${3#=}
[ \"$1\" = has-session ] && [ -e \"$d/session-$name\" ] && exit 0
echo \"can't find session: $name\" >&2
exit 1",
        );
        scratch.set_unit(ACTIVE);
        scratch
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    fn script(&self, name: &str, body: &str) {
        let path = self.path(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A file that was just written cannot be run while another thread's
        // fork still holds it open for writing. Run it once to be past that.
        for _ in 0..100 {
            let ran = Command::new(&path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match ran {
                Err(err) if err.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => break,
            }
        }
    }

    fn set_unit(&self, show: &str) {
        std::fs::write(self.path("systemctl.out"), show).unwrap();
    }

    fn set_session(&self, name: &str, alive: bool) {
        let file = self.path(&format!("session-{name}"));
        if alive {
            std::fs::write(file, "").unwrap();
        } else {
            let _ = std::fs::remove_file(file);
        }
    }

    fn checker(&self) -> Checker {
        Checker::with_programs(self.path("systemctl"), self.path("tmux"))
            .with_timeout(Duration::from_millis(700))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("SOLOS_KEEP_SCRATCH").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn job(spec: Value) -> JobConfig {
    serde_json::from_value(spec).unwrap()
}

fn unit_job() -> JobConfig {
    job(json!({"name": "l2book", "unit": "l2book-download.service"}))
}

/// A status command that runs `script` under `/bin/sh`, with a short timeout.
fn sh(script: &str) -> JobConfig {
    job(json!({"name": "wave", "command": ["/bin/sh", "-c", script], "timeout": "1s"}))
}

async fn verdict(checker: &Checker, job: &JobConfig) -> Verdict {
    checker.check(job, T0).await
}

#[tokio::test]
async fn a_systemd_unit_is_up_only_while_it_is_active() {
    let scratch = Scratch::new("unit");
    let checker = scratch.checker();
    let job = unit_job();
    for (show, status, says) in [
        (ACTIVE, Status::Up, "active (running)"),
        (INACTIVE, Status::Down, "inactive (dead)"),
        (
            "LoadState=loaded\nActiveState=failed\nSubState=failed\n",
            Status::Down,
            "failed (failed)",
        ),
        (
            "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\n",
            Status::Down,
            "activating (auto-restart)",
        ),
        (
            "LoadState=not-found\nActiveState=inactive\nSubState=dead\n",
            Status::Down,
            "not loaded",
        ),
    ] {
        scratch.set_unit(show);
        let found = verdict(&checker, &job).await;
        assert_eq!(found.status, status, "{show}: {found:?}");
        assert!(found.detail.contains(says), "{show}: {found:?}");
    }
    assert_eq!(
        std::fs::read_to_string(scratch.path("systemctl.args"))
            .unwrap()
            .trim(),
        "--user show --no-pager -p LoadState -p ActiveState -p SubState l2book-download.service",
        "it asks the user's systemd about this unit and nothing else"
    );
}

#[tokio::test]
async fn systemctl_that_hangs_or_is_missing_is_a_down_job_not_a_stuck_watchdog() {
    let scratch = Scratch::new("systemctl-gone");
    std::fs::write(scratch.path("systemctl.hang"), "").unwrap();
    let started = Instant::now();
    let found = verdict(&scratch.checker(), &unit_job()).await;
    assert_eq!(found.status, Status::Down);
    assert!(found.detail.contains("gave no answer"), "{found:?}");
    assert!(started.elapsed() < Duration::from_secs(10));

    let missing = Checker::with_programs("/nonexistent/systemctl", "/nonexistent/tmux");
    let found = verdict(&missing, &unit_job()).await;
    assert_eq!(found.status, Status::Down);
    assert!(found.detail.contains("cannot be run"), "{found:?}");
    let found = verdict(&missing, &job(json!({"name": "b", "tmux": "build"}))).await;
    assert!(found.detail.contains("cannot be run"), "{found:?}");
}

#[tokio::test]
async fn a_tmux_session_is_up_while_it_exists_by_its_exact_name() {
    let scratch = Scratch::new("tmux");
    let checker = scratch.checker();
    let build = job(json!({"name": "build", "tmux": "data-build"}));
    scratch.set_session("data-build", true);
    let found = verdict(&checker, &build).await;
    assert_eq!(found.status, Status::Up, "{found:?}");
    assert_eq!(
        std::fs::read_to_string(scratch.path("tmux.args"))
            .unwrap()
            .trim(),
        "has-session -t =data-build",
        "`=` asks for this name and not a prefix of it"
    );
    scratch.set_session("data-build", false);
    let found = verdict(&checker, &build).await;
    assert_eq!(found.status, Status::Down);
    assert!(
        found.detail.contains("can't find session: data-build"),
        "{found:?}"
    );
}

#[tokio::test]
async fn a_pid_file_is_up_while_it_names_a_live_process() {
    let scratch = Scratch::new("pid");
    let checker = scratch.checker();
    let file = scratch.path("job.pid");
    let pid_job = job(json!({"name": "wave", "pid_file": file}));

    let found = verdict(&checker, &pid_job).await;
    assert_eq!(found.status, Status::Down);
    assert!(found.detail.contains("does not exist"), "{found:?}");

    std::fs::write(&file, "not a pid\n").unwrap();
    let found = verdict(&checker, &pid_job).await;
    assert!(found.detail.contains("no process id"), "{found:?}");

    let me = std::process::id();
    std::fs::write(&file, format!("{me}\n")).unwrap();
    let found = verdict(&checker, &pid_job).await;
    assert_eq!(found.status, Status::Up, "{found:?}");
    assert!(found.detail.contains(&format!("process {me} is running")));

    // A process that exited and was not waited for is a zombie, which is dead.
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pid = child.id();
    let stat = format!("/proc/{pid}/stat");
    let started = Instant::now();
    while !std::fs::read_to_string(&stat).is_ok_and(|s| s.contains(") Z")) {
        assert!(started.elapsed() < Duration::from_secs(5), "no zombie");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(&file, format!("{pid}\n")).unwrap();
    let found = verdict(&checker, &pid_job).await;
    assert_eq!(found.status, Status::Down, "{found:?}");
    assert!(found.detail.contains("not reaped"), "{found:?}");
    child.wait().unwrap();
    let found = verdict(&checker, &pid_job).await;
    assert!(found.detail.contains("is not running"), "{found:?}");
}

#[tokio::test]
async fn a_status_command_says_healthy_stalled_or_down_by_its_exit_code() {
    let scratch = Scratch::new("command");
    let checker = scratch.checker();
    for (script, status, says) in [
        ("echo 'wave 3 of 8, 41%'", Status::Up, "wave 3 of 8, 41%"),
        ("exit 0", Status::Up, "exited 0"),
        (
            "echo 'no new data for 40m'; exit 2",
            Status::Stalled,
            "no new data for 40m",
        ),
        ("exit 2", Status::Stalled, "exited 2"),
        (
            "echo 'modal app is gone'; exit 1",
            Status::Down,
            "exited 1: modal app is gone",
        ),
        ("echo oops >&2; exit 3", Status::Down, "exited 3: oops"),
        ("kill -9 $$", Status::Down, "killed by a signal"),
        ("sleep 30", Status::Down, "gave no answer in 1s"),
    ] {
        let found = verdict(&checker, &sh(script)).await;
        assert_eq!(found.status, status, "{script}: {found:?}");
        assert!(found.detail.contains(says), "{script}: {found:?}");
    }
    let missing = job(json!({"name": "w", "command": ["/nonexistent/status"]}));
    let found = verdict(&checker, &missing).await;
    assert_eq!(found.status, Status::Down);
    assert!(found.detail.contains("cannot be run"), "{found:?}");
}

#[tokio::test]
async fn a_status_command_is_never_a_shell_line_and_sees_no_secrets() {
    let scratch = Scratch::new("command-safety");
    let checker = scratch.checker();
    // The arguments reach the program as they are, with no shell to read them.
    let hostile = "$(touch pwned) `touch pwned` ; touch pwned";
    let echo =
        job(json!({"name": "w", "command": ["/bin/sh", "-c", "printf %s \"$1\"", "sh", hostile]}));
    let found = verdict(&checker, &echo).await;
    assert_eq!(found.detail, hostile);
    assert!(!Path::new("pwned").exists());

    // The program gets a short environment: not the agent's.
    assert!(
        std::env::var_os("CARGO_MANIFEST_DIR").is_some(),
        "the test needs a variable that is set here and not passed on"
    );
    let found = verdict(
        &checker,
        &sh("printf '%s %s' \"${CARGO_MANIFEST_DIR-unset}\" \"${PATH:+path}\""),
    )
    .await;
    assert_eq!(found.detail, "unset path");

    // No input: a program that reads stdin sees the end at once.
    let found = verdict(&checker, &sh("cat; echo done")).await;
    assert_eq!(found.detail, "done");
}

#[tokio::test]
async fn a_status_command_that_floods_or_escapes_is_cut_and_cleaned() {
    let scratch = Scratch::new("command-output");
    let checker = scratch.checker();
    let started = Instant::now();
    let found = verdict(&checker, &sh("head -c 20000000 /dev/zero | tr '\\0' x")).await;
    assert_eq!(found.status, Status::Up, "{found:?}");
    assert!(
        found.detail.len() <= 210 && found.detail.starts_with("xxxx"),
        "{}",
        found.detail.len()
    );
    assert!(started.elapsed() < Duration::from_secs(5));

    let found = verdict(&checker, &sh("printf '\\033[31mred\\033[0m\\n'; exit 1")).await;
    assert!(!found.detail.contains('\u{1b}'), "{:?}", found.detail);
    assert!(found.detail.contains("red"));
}

#[tokio::test]
async fn a_status_command_that_runs_over_is_killed_with_what_it_started() {
    let scratch = Scratch::new("command-group");
    let checker = scratch.checker();
    let pid_file = scratch.path("child.pid");
    let script = format!("sleep 30 & echo $! > {}; wait", pid_file.display());
    let found = verdict(&checker, &sh(&script)).await;
    assert!(found.detail.contains("gave no answer"), "{found:?}");
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let stat = format!("/proc/{pid}/stat");
    let started = Instant::now();
    // Gone, or a zombie that is waiting for init to collect it.
    while std::fs::read_to_string(&stat).is_ok_and(|s| !s.contains(") Z")) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the command's child outlived it"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test]
async fn a_running_job_stalls_when_its_progress_file_stops_changing() {
    let scratch = Scratch::new("progress");
    let checker = scratch.checker();
    let log = scratch.path("progress.log");
    let watched = job(json!({
        "name": "l2book", "unit": "l2book-download.service",
        "progress": {"file": log, "stalled_after": "30m"},
    }));

    let found = checker.check(&watched, T0).await;
    assert_eq!(found.status, Status::Stalled);
    assert!(found.detail.contains("does not exist"), "{found:?}");

    std::fs::write(&log, "chunk 1\n").unwrap();
    let modified = std::fs::metadata(&log)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let found = checker.check(&watched, modified + 29 * MIN).await;
    assert_eq!(found.status, Status::Up, "{found:?}");
    let first = checker.check(&watched, modified + 30 * MIN).await;
    assert_eq!(first.status, Status::Stalled);
    assert!(first.detail.contains("has not changed since"), "{first:?}");
    assert!(first.detail.contains("limit 30m"), "{first:?}");
    let later = checker.check(&watched, modified + 3 * HOUR).await;
    assert_eq!(
        later.detail, first.detail,
        "the same words for as long as it lasts, so a page does not churn"
    );

    // A job that is down is down, whatever its file says.
    scratch.set_unit(INACTIVE);
    let found = checker.check(&watched, modified + 3 * HOUR).await;
    assert_eq!(found.status, Status::Down);
}

#[tokio::test]
async fn a_status_command_job_can_have_a_progress_file_too() {
    let scratch = Scratch::new("progress-command");
    let log = scratch.path("wave.log");
    let watched = job(json!({
        "name": "wave", "command": ["/bin/sh", "-c", "exit 0"],
        "progress": {"file": log, "stalled_after": "10m"},
    }));
    let found = scratch.checker().check(&watched, T0).await;
    assert_eq!(found.status, Status::Stalled, "{found:?}");
}

#[tokio::test]
async fn a_running_job_is_low_on_disk_below_its_floor() {
    let scratch = Scratch::new("disk");
    let checker = scratch.checker();
    let on = |min_free_gb: u64, path: &Path| {
        job(json!({
            "name": "l2book", "unit": "l2book-download.service",
            "disk": {"path": path, "min_free_gb": min_free_gb},
        }))
    };
    let found = checker.check(&on(1, &scratch.root), T0).await;
    assert_eq!(found.status, Status::Up, "{found:?}");

    let found = checker.check(&on(1_000_000_000, &scratch.root), T0).await;
    assert_eq!(found.status, Status::LowDisk);
    assert!(found.detail.contains("GB free on"), "{found:?}");
    assert!(found.detail.contains("floor"), "{found:?}");

    let found = checker
        .check(&on(1, &scratch.path("no/such/dir")), T0)
        .await;
    assert_eq!(found.status, Status::LowDisk);
    assert!(
        found.detail.contains("cannot read the free space"),
        "{found:?}"
    );

    // Down outranks low disk, and a job that is not running is not asked.
    scratch.set_unit(INACTIVE);
    let found = checker.check(&on(1_000_000_000, &scratch.root), T0).await;
    assert_eq!(found.status, Status::Down);
}

#[tokio::test]
async fn every_job_gets_a_verdict_in_order_and_a_hung_one_does_not_hold_up_the_rest() {
    let scratch = Scratch::new("many");
    scratch.set_session("build", true);
    let jobs = vec![
        sh("sleep 30"),
        unit_job(),
        job(json!({"name": "build", "tmux": "build"})),
    ];
    let found = scratch.checker().check_all(&jobs, T0).await;
    let statuses: Vec<Status> = found.iter().map(|v| v.status).collect();
    assert_eq!(statuses, [Status::Down, Status::Up, Status::Up]);
}

#[test]
fn a_job_keeps_the_time_its_status_began_and_a_return_to_up_is_told_once() {
    let jobs = [unit_job(), job(json!({"name": "build", "tmux": "build"}))];
    let up = || Verdict {
        status: Status::Up,
        detail: "fine".into(),
    };
    let down = |why: &str| Verdict {
        status: Status::Down,
        detail: why.into(),
    };

    let (first, told) = fold(None, &jobs, vec![up(), up()], T0);
    assert!(told.is_empty());
    assert_eq!((first.v, first.checked_ms), (FORMAT, T0));
    assert!(first.jobs.iter().all(|record| record.since_ms == T0));
    assert_eq!(first.jobs[0].check, "systemd unit l2book-download.service");
    assert_eq!(first.jobs[1].check, "tmux session build");

    let (second, told) = fold(Some(&first), &jobs, vec![down("dead"), up()], T0 + MIN);
    assert!(told.is_empty());
    assert_eq!(
        second.jobs[0].since_ms,
        T0 + MIN,
        "a new status is a new start"
    );
    assert_eq!(
        second.jobs[1].since_ms, T0,
        "an unchanged one keeps its own"
    );

    let (third, told) = fold(
        Some(&second),
        &jobs,
        vec![down("still dead"), up()],
        T0 + HOUR,
    );
    assert!(told.is_empty());
    assert_eq!(third.jobs[0].since_ms, T0 + MIN);
    assert_eq!(third.jobs[0].detail, "still dead");

    let (fourth, told) = fold(Some(&third), &jobs, vec![up(), up()], T0 + 2 * HOUR);
    let [back] = &told[..] else {
        panic!("{told:?}")
    };
    assert_eq!(back.was.name, "l2book");
    assert_eq!(
        back.text(T0 + 2 * HOUR),
        "The job \"l2book\" (systemd unit l2book-download.service) is back up. \
         It had been down for 1h 59m."
    );
    assert_eq!(fourth.jobs[0].since_ms, T0 + 2 * HOUR);

    let (fifth, told) = fold(Some(&fourth), &jobs[..1], vec![up()], T0 + 3 * HOUR);
    assert!(told.is_empty());
    assert_eq!(
        fifth.jobs.len(),
        1,
        "a job that is no longer listed is dropped"
    );
}

fn sent(path: &Path) -> Vec<OutboundMessage> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn config(scratch: &Scratch, jobs: &str) -> AgentConfig {
    let yaml = format!(
        "policy:
  people: [slack:alice]
sources:
  - kind: jsonl
    path: {root}/triggers.jsonl
channels:
  - kind: jsonl
    source: ops
    path: {root}/ops.jsonl
router: {{kind: scripted}}
executor: {{kind: scripted}}
budgets: {{session_microusd: 500000}}
watchdog:
  operator: {{source: ops, address: {{channel: D-OPS}}}}
{jobs}",
        root = scratch.root.display()
    );
    let file = scratch.path("agent.yaml");
    std::fs::write(&file, yaml).unwrap();
    AgentConfig::load(&file).unwrap()
}

fn watchdog(scratch: &Scratch, config: &AgentConfig) -> Watchdog {
    let channel = Arc::new(JsonlChannel::open(scratch.path("ops.jsonl")).unwrap());
    let settings = config.watchdog.as_ref().unwrap();
    Watchdog::open(&scratch.state(), settings, Known::of(config), channel, None)
        .unwrap()
        .with_checker(scratch.checker())
}

fn jobs_yaml() -> &'static str {
    "  jobs:
    - {name: l2book, unit: l2book-download.service}
    - {name: build, tmux: data-build}
"
}

fn record(scratch: &Scratch, name: &str) -> (Status, u64) {
    let state = State::load(&scratch.state()).expect("the Watchdog wrote its state");
    let found = state.jobs.iter().find(|r| r.name == name).unwrap();
    (found.status, found.since_ms)
}

#[tokio::test]
async fn stopping_a_job_posts_one_alert_and_it_clears_when_the_job_comes_back() {
    let scratch = Scratch::new("acceptance");
    let ops = scratch.path("ops.jsonl");
    let config = config(&scratch, jobs_yaml());
    scratch.set_session("data-build", true);
    let mut dog = watchdog(&scratch, &config);

    // Both are registered and up: they are listed, and nobody is told anything.
    assert_eq!(dog.tick(T0).await.unwrap(), 0);
    let state = State::load(&scratch.state()).unwrap();
    let listed: Vec<_> = state
        .jobs
        .iter()
        .map(|r| (r.name.as_str(), r.check.as_str(), r.status))
        .collect();
    assert_eq!(
        listed,
        [
            ("l2book", "systemd unit l2book-download.service", Status::Up),
            ("build", "tmux session data-build", Status::Up),
        ]
    );
    assert_eq!(state.checked_ms, T0);
    assert!(sent(&ops).is_empty());

    // The unit stops: one alert, however many looks and however long it stays down.
    scratch.set_unit(INACTIVE);
    assert_eq!(dog.tick(T0 + MIN).await.unwrap(), 1);
    for later in [2 * MIN, HOUR, 30 * HOUR] {
        assert_eq!(dog.tick(T0 + later).await.unwrap(), 0, "{later}");
    }
    let told = sent(&ops);
    let [alert] = &told[..] else {
        panic!("{told:?}")
    };
    assert_eq!(alert.kind, MessageKind::Attention);
    assert_eq!(
        alert.text,
        "The job \"l2book\" (systemd unit l2book-download.service) is down: inactive (dead)."
    );
    assert_eq!(alert.channel.address["channel"], "D-OPS");
    assert_eq!(record(&scratch, "l2book"), (Status::Down, T0 + MIN));
    assert_eq!(record(&scratch, "build").0, Status::Up);

    // `agent status` reads what the Watchdog found, with no look of its own.
    let now = T0 + 31 * HOUR;
    let report = status::read(&scratch.state(), Some(&config), now).unwrap();
    let jobs: Vec<_> = report
        .alerts
        .iter()
        .filter(|a| a.kind == AlertKind::Job)
        .collect();
    let [now_alert] = &jobs[..] else {
        panic!("{jobs:?}")
    };
    assert_eq!(now_alert.id, alert.id.0, "the same incident, the same id");

    // The tmux session ends too: its own alert, and the unit's is not repeated.
    scratch.set_session("data-build", false);
    assert_eq!(dog.tick(T0 + 31 * HOUR).await.unwrap(), 1);
    assert_eq!(sent(&ops).len(), 2);
    assert!(
        sent(&ops)[1]
            .text
            .contains("\"build\" (tmux session data-build) is down")
    );

    // A restarted Watchdog knows what it told: nothing is said twice.
    drop(dog);
    let mut dog = watchdog(&scratch, &config);
    assert_eq!(dog.tick(T0 + 32 * HOUR).await.unwrap(), 0);
    assert_eq!(sent(&ops).len(), 2);
    assert_eq!(record(&scratch, "l2book"), (Status::Down, T0 + MIN));

    // The unit comes back: it clears, once, and the other job stays down.
    scratch.set_unit(ACTIVE);
    assert_eq!(dog.tick(T0 + 33 * HOUR).await.unwrap(), 1);
    let told = sent(&ops);
    assert_eq!(told.len(), 3);
    assert_eq!(told[2].kind, MessageKind::Result);
    assert_eq!(
        told[2].text,
        "The job \"l2book\" (systemd unit l2book-download.service) is back up. \
         It had been down for 1d 08h."
    );
    for later in [34 * HOUR, 50 * HOUR] {
        assert_eq!(dog.tick(T0 + later).await.unwrap(), 0);
    }
    assert_eq!(sent(&ops).len(), 3);
    assert_eq!(record(&scratch, "l2book"), (Status::Up, T0 + 33 * HOUR));
    assert_eq!(record(&scratch, "build").0, Status::Down);

    // It dies again: that is a new incident and a new alert.
    scratch.set_unit(INACTIVE);
    assert_eq!(dog.tick(T0 + 51 * HOUR).await.unwrap(), 1);
    assert_eq!(sent(&ops).len(), 4);

    // The session comes back, from a Watchdog that started after it went down.
    scratch.set_session("data-build", true);
    assert_eq!(dog.tick(T0 + 52 * HOUR).await.unwrap(), 1);
    assert!(sent(&ops)[4].text.contains("\"build\""));
    assert!(sent(&ops)[4].text.contains("is back up"));
}

#[tokio::test]
async fn a_job_that_stalls_or_runs_low_on_disk_is_told_once_each() {
    let scratch = Scratch::new("stalled");
    let ops = scratch.path("ops.jsonl");
    let log = scratch.path("progress.log");
    std::fs::write(&log, "chunk\n").unwrap();
    let modified = std::fs::metadata(&log)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let jobs = format!(
        "  jobs:
    - name: l2book
      unit: l2book-download.service
      progress: {{file: {log}, stalled_after: 20m}}
      disk: {{path: {root}, min_free_gb: 1000000000}}
",
        log = log.display(),
        root = scratch.root.display()
    );
    let config = config(&scratch, &jobs);
    let mut dog = watchdog(&scratch, &config);

    // Running, but its disk is below the floor.
    assert_eq!(dog.tick(modified + MIN).await.unwrap(), 1);
    let told = sent(&ops);
    assert!(
        told[0].text.contains("is low on disk: "),
        "{}",
        told[0].text
    );
    assert_eq!(dog.tick(modified + 2 * MIN).await.unwrap(), 0);

    // Its file goes quiet: a stall is a new incident, told once.
    assert_eq!(dog.tick(modified + 21 * MIN).await.unwrap(), 1);
    assert_eq!(dog.tick(modified + 25 * MIN).await.unwrap(), 0);
    let told = sent(&ops);
    assert_eq!(told.len(), 2);
    assert!(told[1].text.contains("has stalled: "), "{}", told[1].text);
    assert!(
        told[1].text.contains("has not changed since"),
        "{}",
        told[1].text
    );
    assert_eq!(record(&scratch, "l2book").0, Status::Stalled);
}

#[tokio::test]
async fn emptying_the_registry_empties_what_the_dashboard_shows() {
    let scratch = Scratch::new("emptied");
    let config_with = config(&scratch, jobs_yaml());
    scratch.set_session("data-build", true);
    let mut dog = watchdog(&scratch, &config_with);
    dog.tick(T0).await.unwrap();
    assert_eq!(State::load(&scratch.state()).unwrap().jobs.len(), 2);

    let config_without = config(&scratch, "");
    let mut dog = watchdog(&scratch, &config_without);
    dog.tick(T0 + MIN).await.unwrap();
    let state = State::load(&scratch.state()).unwrap();
    assert!(state.jobs.is_empty());
    assert_eq!(state.checked_ms, T0 + MIN);
}

#[tokio::test]
async fn a_watchdog_without_jobs_writes_nothing() {
    let scratch = Scratch::new("none");
    let mut dog = watchdog(&scratch, &config(&scratch, ""));
    dog.tick(T0).await.unwrap();
    assert!(!scratch.state().join(STATE_FILE).exists());
}

#[test]
fn a_dashboard_never_reads_a_state_it_does_not_know() {
    let scratch = Scratch::new("format");
    assert!(State::load(&scratch.state()).is_none());
    std::fs::write(scratch.state().join(STATE_FILE), "{not json").unwrap();
    assert!(State::load(&scratch.state()).is_none());
    let other = json!({"v": FORMAT + 1, "checked_ms": 1, "jobs": []});
    std::fs::write(scratch.state().join(STATE_FILE), other.to_string()).unwrap();
    assert!(State::load(&scratch.state()).is_none());
}
