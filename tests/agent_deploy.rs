//! `scripts/agent-deploy`'s drain, run against a fake box: a HOME whose `systemctl` logs what it is asked and whose
//! `solos-swarm` prints the `agent status --json` a test queues, so the pause, the wait for a Report, the health
//! hold and the order of a switch are all checked without systemd or a build. The one thing real is the JSON: the
//! parser the drain uses is fed what `agent status --json` serializes, from the same structs.

use serde_json::{Value, json};
use solos_agent_core::model::{
    AssignmentId, AssignmentState, Decision, Plan, Report, ReportStatus, Stage, TicketId,
};
use solos_swarm::agent::status::{AssignmentStatus, RouterSpend, Status, TicketStatus};
use solos_swarm::agent::watchdog::Thresholds;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/agent-deploy");

fn running(ticket: &str, assignment: &str, reports: usize) -> TicketStatus {
    with(
        ticket,
        Stage::Working,
        Some((assignment, AssignmentState::Running, reports)),
    )
}

fn with(
    ticket: &str,
    stage: Stage,
    assignment: Option<(&str, AssignmentState, usize)>,
) -> TicketStatus {
    TicketStatus {
        id: TicketId::new(ticket),
        title: format!("ticket {ticket}"),
        stage,
        created_ms: 1_793_611_805_000,
        age_ms: 60_000,
        plan: Plan::Session {
            harness: None,
            environment: None,
            method: None,
        },
        budget_microusd: Some(500_000),
        committed_microusd: 1_000,
        attention: None,
        request: None,
        assignment: assignment.map(|(id, state, reports)| AssignmentStatus {
            id: AssignmentId::new(id),
            state,
            created_ms: 1_793_611_805_000,
            reports,
            last_report: (reports > 0).then(|| Report {
                seq: 1,
                status: ReportStatus::Progress,
                text: "still at it".into(),
                question: None,
                spent_microusd: 1_000,
                at_ms: 1_793_611_900_000,
            }),
            ended_reason: None,
        }),
    }
}

/// `agent status --json`, as the binary prints it.
fn status(tickets: Vec<TicketStatus>) -> String {
    let decision: Decision = serde_json::from_value(json!({
        "id": "dc_1", "thread": "slack:T1", "triggers": ["t1"], "kind": "accept",
        "reason": "alice asked", "ticket": "tk_decided", "would_accept": true,
        "policy_version": "v1", "at_ms": 1_793_611_805_000u64,
    }))
    .unwrap();
    let report = Status {
        at_ms: 1_793_611_900_000,
        state_dir: "/home/agent/agent/state".into(),
        schema_version: 3,
        pause: None,
        router: RouterSpend {
            committed_today_microusd: 0,
            daily_cap_microusd: None,
            cap_known: true,
        },
        tickets,
        decisions: vec![decision],
        effects: Vec::new(),
        thresholds: Thresholds::default(),
        alerts: Vec::new(),
    };
    serde_json::to_string_pretty(&report).unwrap()
}

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
            "agent-deploy-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn executable(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// What the box would print for `agent status --json`: the n-th call gets the n-th queued text, the last one
/// repeats.
const FAKE_SWARM: &str = "#!/bin/sh
[ \"$1 $2\" = \"agent status\" ] || { echo \"unexpected: $*\" >&2; exit 2; }
d=\"$HOME/fake\"
n=$(cat \"$d/count\" 2>/dev/null || echo 0)
n=$((n + 1))
echo $n > \"$d/count\"
f=\"$d/status.$n.json\"
[ -f \"$f\" ] || f=\"$d/status.last.json\"
cat \"$f\"
";

/// A `systemctl` that logs its arguments and keeps one bit of state: whether solos-agent is active.
const FAKE_SYSTEMCTL: &str = "#!/bin/sh
echo \"$*\" >> \"$HOME/fake/systemctl.log\"
echo \"$XDG_RUNTIME_DIR\" >> \"$HOME/fake/xdg.log\"
shift # --user
case \"$*\" in
\"is-active -q solos-agent\") [ -f \"$HOME/fake/active\" ] ;;
\"is-failed -q solos-agent\") exit 1 ;;
\"start solos-agent\") touch \"$HOME/fake/active\" ;;
\"stop solos-agent\") rm -f \"$HOME/fake/active\" ;;
\"restart solos-agent\") touch \"$HOME/fake/active\" ;;
\"show -p MainPID --value solos-agent\") echo 4242 ;;
*) exit 0 ;;
esac
";

struct Host {
    _scratch: Scratch,
    home: PathBuf,
    _socket: Option<UnixListener>,
}

impl Drop for Host {
    /// A built release is read-only, and the scratch directory cannot be removed until it is not.
    fn drop(&mut self) {
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&self.home)
            .status();
    }
}

impl Host {
    /// A box with an agent that is running its release `old`.
    fn new(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let home = scratch.root.join("home");
        let agent = home.join("agent");
        std::fs::create_dir_all(agent.join("state")).unwrap();
        std::fs::write(agent.join("state/agent.db"), b"db").unwrap();
        std::fs::write(agent.join("agent.yaml"), "policy: {}\n").unwrap();
        std::fs::create_dir_all(home.join(".config/systemd/user")).unwrap();
        std::fs::create_dir_all(home.join("fake")).unwrap();
        let old = agent.join("releases/old");
        executable(&old.join("bin/solos-swarm"), FAKE_SWARM);
        std::fs::write(old.join(".complete"), b"").unwrap();
        std::fs::create_dir_all(old.join("scripts/systemd")).unwrap();
        std::fs::write(old.join("scripts/systemd/solos-agent.service"), "# old\n").unwrap();
        std::os::unix::fs::symlink(&old, agent.join("current")).unwrap();
        executable(&home.join(".cargo/bin/systemctl"), FAKE_SYSTEMCTL);
        executable(&home.join(".cargo/bin/journalctl"), "#!/bin/sh\nexit 0\n");
        std::fs::write(home.join("fake/active"), b"").unwrap();
        Self {
            _scratch: scratch,
            home,
            _socket: None,
        }
    }

    fn agent(&self) -> PathBuf {
        self.home.join("agent")
    }

    fn state(&self, file: &str) -> PathBuf {
        self.agent().join("state").join(file)
    }

    /// Queue the statuses the running release prints, one per call.
    fn statuses(&self, queued: &[String]) {
        let dir = self.home.join("fake");
        let _ = std::fs::remove_file(dir.join("count"));
        for (n, text) in queued.iter().enumerate() {
            std::fs::write(dir.join(format!("status.{}.json", n + 1)), text).unwrap();
        }
        std::fs::write(dir.join("status.last.json"), queued.last().unwrap()).unwrap();
    }

    fn stop_the_unit(&self) {
        std::fs::remove_file(self.home.join("fake/active")).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.home.join("fake/systemctl.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// `agent-deploy --on-host ARGS`, as the box runs it, with the waits shortened.
    fn on_host(&self, args: &[&str], stdin: Option<&str>) -> (Output, Duration) {
        let started = Instant::now();
        let mut child = Command::new("bash")
            .arg(SCRIPT)
            .arg("--on-host")
            .args(args)
            .env("HOME", &self.home)
            .env("DRAIN_POLL", "0.1")
            .env("HEALTHY_SECS", "0")
            .env("HEALTHY_POLL", "0.05")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(text) = stdin {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        }
        let output = child.wait_with_output().unwrap();
        (output, started.elapsed())
    }
}

fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn the_drain_reads_running_assignments_out_of_the_real_status_json() {
    let boxed = Host::new("parse");
    let json = status(vec![
        running("tk_a", "as_1", 2),
        with(
            "tk_idle",
            Stage::Idle,
            Some(("as_2", AssignmentState::Ended, 3)),
        ),
        with("tk_new", Stage::Queued, None),
        with(
            "tk_wait",
            Stage::Queued,
            Some(("as_3", AssignmentState::Pending, 0)),
        ),
        running("tk_e", "as_5", 0),
    ]);
    let (output, _) = boxed.on_host(&["running-assignments"], Some(&json));
    assert!(output.status.success(), "{}", err(&output));
    assert_eq!(out(&output), "tk_a as_1 2\ntk_e as_5 0\n", "{json}");

    let none = status(Vec::new());
    let (output, _) = boxed.on_host(&["running-assignments"], Some(&none));
    assert_eq!(out(&output), "");
}

#[test]
fn a_drain_pauses_then_waits_until_every_running_assignment_has_reported() {
    let boxed = Host::new("report");
    boxed.statuses(&[
        status(vec![running("tk_a", "as_1", 1), running("tk_b", "as_2", 4)]),
        status(vec![running("tk_a", "as_1", 1), running("tk_b", "as_2", 4)]),
        status(vec![running("tk_a", "as_1", 2), running("tk_b", "as_2", 4)]),
        status(vec![running("tk_a", "as_1", 2), running("tk_b", "as_2", 5)]),
    ]);
    let (output, took) = boxed.on_host(&["drain", "20"], None);
    assert!(output.status.success(), "{}", err(&output));
    let said = out(&output);
    assert!(
        said.contains("drain: paused; waiting up to 20s for 2 running assignment(s)"),
        "{said}"
    );
    assert!(
        said.contains("every assignment reached a report or ended"),
        "{said}"
    );
    assert!(
        took < Duration::from_secs(10),
        "it did not wait out the clock: {took:?}"
    );
    let control: Value =
        serde_json::from_str(&std::fs::read_to_string(boxed.state("control.json")).unwrap())
            .unwrap();
    assert_eq!(
        (control["mode"].as_str(), control["by"].as_str()),
        (Some("pause"), Some("agent-deploy"))
    );
    // The file is the pause the agent, its broker and the Watchdog already read, not a private signal.
    let state = boxed.agent().join("state");
    assert!(solos_swarm::gateway::effective_control(&state, now_secs()).is_some());
    let held = solos_swarm::agent::watchdog::read_pause(&state, false, now_secs() * 1_000);
    assert_eq!(held.unwrap().by, "control.json (mode pause)");
}

#[test]
fn an_assignment_that_ended_counts_as_having_reached_its_end() {
    let boxed = Host::new("ended");
    boxed.statuses(&[
        status(vec![running("tk_a", "as_1", 1)]),
        status(vec![with(
            "tk_a",
            Stage::Idle,
            Some(("as_1", AssignmentState::Ended, 1)),
        )]),
    ]);
    let (output, _) = boxed.on_host(&["drain", "20"], None);
    assert!(
        out(&output).contains("every assignment reached a report or ended"),
        "{}",
        out(&output)
    );
}

#[test]
fn a_drain_that_sees_no_report_gives_up_on_time_and_does_not_fail_the_deploy() {
    let boxed = Host::new("stuck");
    boxed.statuses(&[status(vec![running("tk_a", "as_1", 1)])]);
    let (output, took) = boxed.on_host(&["drain", "2"], None);
    assert!(
        output.status.success(),
        "a drain that times out stops the agent anyway"
    );
    assert!(
        err(&output).contains("1 assignment(s) still mid-turn after 2s; stopping anyway"),
        "{}",
        err(&output)
    );
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(8),
        "{took:?}"
    );
    assert!(
        boxed.state("control.json").exists(),
        "still paused: the deploy lifts it"
    );
}

#[test]
fn nothing_running_pauses_and_does_not_wait() {
    let boxed = Host::new("idle");
    boxed.statuses(&[status(vec![with(
        "tk_a",
        Stage::Idle,
        Some(("as_1", AssignmentState::Ended, 1)),
    )])]);
    let (output, took) = boxed.on_host(&["drain", "30"], None);
    assert!(
        out(&output).contains("drain: paused; nothing is running"),
        "{}",
        out(&output)
    );
    assert!(took < Duration::from_secs(5));
    assert!(boxed.state("control.json").exists());
}

#[test]
fn there_is_no_drain_without_a_running_agent_a_wait_or_a_store() {
    let boxed = Host::new("skipped");
    boxed.statuses(&[status(vec![running("tk_a", "as_1", 1)])]);
    let (output, _) = boxed.on_host(&["drain", "0"], None);
    assert_eq!(out(&output), "");
    assert!(
        !boxed.state("control.json").exists(),
        "--no-drain pauses nothing"
    );

    boxed.stop_the_unit();
    let (output, _) = boxed.on_host(&["drain", "5"], None);
    assert_eq!(out(&output), "");
    assert!(
        !boxed.state("control.json").exists(),
        "a stopped agent has nothing to drain"
    );

    let boxed = Host::new("nostore");
    std::fs::remove_file(boxed.state("agent.db")).unwrap();
    let (output, _) = boxed.on_host(&["drain", "5"], None);
    assert_eq!(out(&output), "");
    assert!(!boxed.state("control.json").exists());
}

#[test]
fn a_status_that_fails_pauses_and_says_it_did_not_wait() {
    let boxed = Host::new("broken");
    executable(
        &boxed.agent().join("releases/old/bin/solos-swarm"),
        "#!/bin/sh\necho 'database is locked' >&2\nexit 1\n",
    );
    let (output, took) = boxed.on_host(&["drain", "30"], None);
    assert!(output.status.success());
    assert!(
        err(&output).contains("`agent status` failed, so not waiting"),
        "{}",
        err(&output)
    );
    assert!(took < Duration::from_secs(5));
    assert!(boxed.state("control.json").exists());
}

#[test]
fn a_pause_that_is_somebody_elses_is_never_replaced_or_lifted() {
    let boxed = Host::new("theirs");
    boxed.statuses(&[status(vec![running("tk_a", "as_1", 1)])]);
    let theirs = "{\"mode\":\"pause\",\"by\":\"alice\",\"reason\":\"looking at spend\"}\n";
    std::fs::write(boxed.state("control.json"), theirs).unwrap();
    boxed.on_host(&["drain", "1"], None);
    assert_eq!(
        std::fs::read_to_string(boxed.state("control.json")).unwrap(),
        theirs
    );
    let (output, _) = boxed.on_host(&["resume"], None);
    assert_eq!(out(&output), "");
    assert_eq!(
        std::fs::read_to_string(boxed.state("control.json")).unwrap(),
        theirs
    );

    std::fs::remove_file(boxed.state("control.json")).unwrap();
    boxed.on_host(&["drain", "1"], None);
    assert!(boxed.state("control.json").exists());
    let (output, _) = boxed.on_host(&["resume"], None);
    assert_eq!(out(&output), "unpaused\n");
    assert!(!boxed.state("control.json").exists());
}

#[test]
fn stop_drains_holds_the_health_check_off_for_a_while_and_leaves_the_agent_unpaused() {
    let boxed = Host::new("stop");
    boxed.statuses(&[
        status(vec![running("tk_a", "as_1", 1)]),
        status(vec![running("tk_a", "as_1", 2)]),
    ]);
    let (output, _) = boxed.on_host(&["stop"], None);
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    let said = out(&output);
    assert!(
        said.contains("drain: every assignment reached a report or ended"),
        "{said}"
    );
    assert!(said.contains("unpaused"), "{said}");
    let calls = boxed.calls();
    assert!(
        calls.iter().any(|call| call == "--user stop solos-agent"),
        "{calls:?}"
    );
    assert!(
        !boxed.state("control.json").exists(),
        "the next start must not come up paused"
    );
    let hold: u64 = std::fs::read_to_string(boxed.state("health-hold"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        hold > now_secs() + 25 * 60 && hold <= now_secs() + 31 * 60,
        "{hold}"
    );
    // The health check reads the very file the script wrote.
    let state = boxed.agent().join("state");
    assert_eq!(solos_swarm::agent::health::HOLD_FILE, "health-hold");
    assert!(solos_swarm::agent::health::hold_active(&state, now_secs()));
    assert!(!solos_swarm::agent::health::hold_active(
        &state,
        now_secs() + 3_600
    ));
}

/// A release with its units, as `git archive` and a build leave it.
fn release(boxed: &Host, name: &str, with_health: bool) {
    let dir = boxed.agent().join("releases").join(name);
    executable(&dir.join("bin/solos-swarm"), FAKE_SWARM);
    std::fs::write(dir.join(".complete"), b"").unwrap();
    let units = dir.join("scripts/systemd");
    std::fs::create_dir_all(&units).unwrap();
    std::fs::write(units.join("solos-agent.service"), format!("# {name}\n")).unwrap();
    if with_health {
        for file in [
            "solos-agent-health.service",
            "solos-agent-health.timer",
            "solos-agent-prune.service",
            "solos-agent-prune.timer",
        ] {
            std::fs::write(units.join(file), format!("# {name} {file}\n")).unwrap();
        }
    }
}

fn index_of(calls: &[String], wanted: &str) -> usize {
    calls
        .iter()
        .position(|call| call == wanted)
        .unwrap_or_else(|| panic!("{wanted:?} was never run: {calls:?}"))
}

#[test]
fn a_switch_drains_before_it_stops_installs_the_timers_and_lifts_the_pause_once_healthy() {
    let mut boxed = Host::new("switch");
    release(&boxed, "new", true);
    boxed._socket = Some(UnixListener::bind(boxed.state("broker.sock")).unwrap());
    boxed.statuses(&[
        status(vec![running("tk_a", "as_1", 1)]),
        status(vec![running("tk_a", "as_1", 2)]),
    ]);
    let (output, _) = boxed.on_host(&["switch", "new"], None);
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    let said = out(&output);
    assert!(said.contains("drain: paused; waiting up to"), "{said}");
    assert!(said.contains("release new is serving"), "{said}");

    let calls = boxed.calls();
    let stop = index_of(&calls, "--user stop solos-agent");
    let reload = index_of(&calls, "--user daemon-reload");
    let start = index_of(&calls, "--user start solos-agent");
    let timers = index_of(
        &calls,
        "--user start solos-agent-health.timer solos-agent-prune.timer",
    );
    assert!(
        stop < reload && reload < start && start < timers,
        "{calls:?}"
    );
    index_of(
        &calls,
        "--user enable -q solos-agent-health.timer solos-agent-prune.timer",
    );

    let user = boxed.home.join(".config/systemd/user");
    for file in [
        "solos-agent.service",
        "solos-agent-health.service",
        "solos-agent-health.timer",
        "solos-agent-prune.service",
        "solos-agent-prune.timer",
    ] {
        assert!(user.join(file).exists(), "{file} was not installed");
    }
    assert_eq!(
        std::fs::read_link(boxed.agent().join("current")).unwrap(),
        boxed.agent().join("releases/new")
    );
    assert!(
        !boxed.state("control.json").exists(),
        "healthy: the pause is lifted"
    );
    assert!(
        !boxed.state("health-hold").exists(),
        "and the health check is told it may alert again"
    );
    assert!(
        boxed.agent().join("backups").read_dir().unwrap().count() == 1,
        "agent.db was backed up while stopped"
    );
}

#[test]
fn a_switch_with_no_drain_stops_at_once() {
    let mut boxed = Host::new("nodrain");
    release(&boxed, "new", true);
    boxed._socket = Some(UnixListener::bind(boxed.state("broker.sock")).unwrap());
    boxed.statuses(&[status(vec![running("tk_a", "as_1", 1)])]);
    let started = Instant::now();
    let mut command = Command::new("bash");
    let output = command
        .arg(SCRIPT)
        .args(["--on-host", "switch", "new"])
        .env("HOME", &boxed.home)
        .env("AGENT_DRAIN_SECS", "0")
        .env("HEALTHY_SECS", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    assert!(!out(&output).contains("drain:"), "{}", out(&output));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!boxed.state("control.json").exists());
}

#[test]
fn a_release_that_predates_the_health_check_turns_its_timers_off_instead_of_failing_every_three_minutes()
 {
    let mut boxed = Host::new("old-release");
    release(&boxed, "ancient", false);
    boxed._socket = Some(UnixListener::bind(boxed.state("broker.sock")).unwrap());
    boxed.statuses(&[status(Vec::new())]);
    let (output, _) = boxed.on_host(&["switch", "ancient"], None);
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    assert!(
        err(&output).contains("has no health check: its timers are off"),
        "{}",
        err(&output)
    );
    let calls = boxed.calls();
    index_of(
        &calls,
        "--user disable -q --now solos-agent-health.timer solos-agent-prune.timer",
    );
    assert!(
        !calls
            .iter()
            .any(|call| call.starts_with("--user start solos-agent-health")),
        "{calls:?}"
    );
}

#[test]
fn a_release_that_does_not_come_up_leaves_the_pause_and_lifts_the_hold_so_the_health_check_speaks()
{
    let boxed = Host::new("unhealthy");
    release(&boxed, "bad", true);
    // No broker socket, ever: the new release never gets healthy, and neither does the one it falls back to.
    boxed.statuses(&[status(vec![running("tk_a", "as_1", 2)])]);
    let output = Command::new("bash")
        .arg(SCRIPT)
        .args(["--on-host", "switch", "bad"])
        .env("HOME", &boxed.home)
        .env("AGENT_DRAIN_SECS", "0")
        .env("HEALTHY_SECS", "0")
        .env("HEALTHY_POLL", "0.05")
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", boxed.home.join(".cargo/bin").display()),
        )
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        out(&output),
        err(&output)
    );
    assert!(
        !boxed.state("health-hold").exists(),
        "a failed deploy must be alertable"
    );
}

#[test]
fn the_flags_are_read_on_the_mac_and_reach_the_box_as_its_drain_time() {
    let scratch = Scratch::new("mac");
    let bin = scratch.root.join("bin");
    let log = scratch.root.join("ssh.log");
    executable(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\necho \"$*\" >> {}\ncat > /dev/null\n",
            log.display()
        ),
    );
    let run = |args: &[&str], env: Option<(&str, &str)>| {
        let mut command = Command::new("bash");
        command
            .arg(SCRIPT)
            .args(args)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env_remove("AGENT_DRAIN_SECS");
        if let Some((key, value)) = env {
            command.env(key, value);
        }
        command.output().unwrap()
    };
    let last = || {
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .last()
            .unwrap()
            .to_owned()
    };
    assert!(run(&["restart"], None).status.success());
    assert!(
        last().ends_with("AGENT_DRAIN_SECS=90 bash ~/agent/.agent-deploy --on-host restart"),
        "{}",
        last()
    );
    assert!(run(&["stop", "--drain", "7"], None).status.success());
    assert!(
        last().ends_with("AGENT_DRAIN_SECS=7 bash ~/agent/.agent-deploy --on-host stop"),
        "{}",
        last()
    );
    assert!(
        run(&["--no-drain"], None).status.code() == Some(2),
        "flags alone are no command"
    );
    assert!(run(&["rollback", "--no-drain"], None).status.success());
    assert!(
        last().ends_with("AGENT_DRAIN_SECS=0 bash ~/agent/.agent-deploy --on-host rollback"),
        "{}",
        last()
    );
    assert!(
        run(&["restart"], Some(("AGENT_DRAIN_SECS", "30")))
            .status
            .success()
    );
    assert!(
        last().ends_with("AGENT_DRAIN_SECS=30 bash ~/agent/.agent-deploy --on-host restart"),
        "{}",
        last()
    );
    assert!(run(&["logs", "-f", "--since", "1h"], None).status.success());
    assert!(
        last().ends_with("--on-host logs -f --since 1h"),
        "{}",
        last()
    );

    for (args, env) in [
        (vec!["restart", "--drain", "soon"], None),
        (vec!["restart", "--drain"], None),
        (vec!["restart"], Some(("AGENT_DRAIN_SECS", "a while"))),
    ] {
        let output = run(&args, env);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
    let usage = run(&["nonsense"], None);
    assert_eq!(usage.status.code(), Some(2));
    let text = String::from_utf8_lossy(&usage.stdout).into_owned()
        + &String::from_utf8_lossy(&usage.stderr);
    for word in [
        "--no-drain",
        "health-logs",
        "prune-logs",
        "solos-agent-health.timer",
        "prebuild",
    ] {
        assert!(text.contains(word), "the usage text lacks {word}: {text}");
    }
}

/// The `Key=value` settings of a shipped unit, comments and blank lines left out.
fn unit_settings(file: &str) -> Vec<(String, String)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/systemd");
    std::fs::read_to_string(path.join(file))
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with('#') && !line.starts_with('['))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

#[test]
fn every_unit_is_hardened_and_the_agents_leaves_out_what_breaks_bwrap() {
    let setting = |settings: &[(String, String)], key: &str| {
        settings
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };
    let all = [
        "solos-agent.service",
        "solos-agent-health.service",
        "solos-agent-prune.service",
        "solos-agent-publish.service",
        "solos-dashboard@.service",
    ];
    for file in all {
        let settings = unit_settings(file);
        for yes in [
            "NoNewPrivileges",
            "LockPersonality",
            "RestrictRealtime",
            "ProtectClock",
            "ProtectKernelModules",
            "ProtectControlGroups",
            "PrivateTmp",
        ] {
            assert_eq!(
                setting(&settings, yes).as_deref(),
                Some("yes"),
                "{file} {yes}"
            );
        }
        assert_eq!(
            setting(&settings, "ProtectSystem").as_deref(),
            Some("strict"),
            "{file}"
        );
        assert_eq!(
            setting(&settings, "ProtectHome").as_deref(),
            Some("read-only"),
            "{file}"
        );
        assert_eq!(
            setting(&settings, "SystemCallArchitectures").as_deref(),
            Some("native"),
            "{file}"
        );
        assert!(
            setting(&settings, "RestrictAddressFamilies")
                .unwrap_or_default()
                .contains("AF_UNIX"),
            "{file}"
        );
        assert_eq!(
            setting(&settings, "SystemCallErrorNumber").as_deref(),
            Some("EPERM"),
            "{file}: a blocked call fails, it does not kill the worker"
        );
        for (key, value) in &settings {
            if key == "ReadWritePaths" {
                assert_eq!(
                    value, "%h/agent/state",
                    "{file} may write the state directory only"
                );
            }
            assert!(
                !(key.starts_with("ExecStartPre") && value.contains("$HOME")),
                "{file}: %h is the home of the user the unit runs as, with no help from the environment"
            );
        }
    }

    // bwrap --unshare-all needs all seven namespaces, and bubblewrap 0.12 opens its bind sources with openat2, which
    // RestrictSUIDSGID turns into ENOSYS. ProtectKernelTunables remounts /proc/sys, so bwrap cannot mount a /proc.
    let agent = unit_settings("solos-agent.service");
    let namespaces = setting(&agent, "RestrictNamespaces").unwrap();
    for namespace in ["user", "mnt", "pid", "net", "ipc", "uts", "cgroup"] {
        assert!(
            namespaces.split(' ').any(|word| word == namespace),
            "{namespaces}"
        );
    }
    for broken in [
        "RestrictSUIDSGID",
        "ProtectKernelTunables",
        "MemoryDenyWriteExecute",
    ] {
        assert_eq!(
            setting(&agent, broken),
            None,
            "{broken} breaks the sandboxes"
        );
    }
    // Units that run no bwrap take them.
    for file in [
        "solos-agent-health.service",
        "solos-agent-prune.service",
        "solos-agent-publish.service",
        "solos-dashboard@.service",
    ] {
        let settings = unit_settings(file);
        assert_eq!(
            setting(&settings, "RestrictSUIDSGID").as_deref(),
            Some("yes")
        );
        assert_eq!(
            setting(&settings, "ProtectKernelTunables").as_deref(),
            Some("yes")
        );
        assert_eq!(
            setting(&settings, "RestrictNamespaces").as_deref(),
            Some("yes")
        );
    }
}

fn uid() -> u32 {
    let output = Command::new("id").arg("-u").output().unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn the_agent_user_is_the_login_name_of_every_ssh_command() {
    let scratch = Scratch::new("agent-user");
    let bin = scratch.root.join("bin");
    let log = scratch.root.join("ssh.log");
    executable(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\necho \"$*\" >> {}\ncat > /dev/null\n",
            log.display()
        ),
    );
    let run = |user: Option<&str>| {
        let mut command = Command::new("bash");
        command
            .arg(SCRIPT)
            .arg("restart")
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("AGENT_HOST", "host-a")
            .env_remove("AGENT_USER");
        if let Some(user) = user {
            command.env("AGENT_USER", user);
        }
        command.output().unwrap()
    };
    let lines = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };

    assert!(run(None).status.success());
    let plain = lines();
    assert_eq!(plain.len(), 2, "the script goes over, then the command");
    assert!(
        plain.iter().all(|line| !line.contains(" -l ")),
        "no user is named, so ssh's own config decides: {plain:?}"
    );

    let _ = std::fs::remove_file(&log);
    assert!(run(Some("solos-agent")).status.success());
    let named = lines();
    assert_eq!(named.len(), 2);
    for line in &named {
        assert!(
            line.contains("-o ServerAliveInterval=30 -l solos-agent host-a "),
            "{line}"
        );
    }

    let _ = std::fs::remove_file(&log);
    for bad in ["-oProxyCommand=x", "agent;reboot", "Agent", "a b"] {
        let output = run(Some(bad));
        assert_eq!(output.status.code(), Some(2), "{bad:?}");
        assert!(
            err(&output).contains("AGENT_USER must be a user name"),
            "{bad:?}"
        );
    }
    assert!(lines().is_empty(), "a bad name never reaches ssh");
}

#[test]
fn a_users_first_deploy_makes_the_unit_directory_and_the_state_directory_the_unit_needs() {
    let boxed = Host::new("first-deploy");
    release(&boxed, "new", true);
    std::fs::remove_dir_all(boxed.home.join(".config")).unwrap();
    std::fs::remove_dir_all(boxed.agent().join("state")).unwrap();
    // The agent's broker socket appears once its state directory does, as it would when the unit starts.
    let socket = boxed.state("broker.sock");
    let waiter = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if socket.parent().unwrap().is_dir() {
                return UnixListener::bind(&socket).ok();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    });
    let (output, _) = boxed.on_host(&["switch", "new"], None);
    let listener = waiter.join().unwrap();
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    assert!(listener.is_some(), "the state directory was never made");
    assert_eq!(
        std::fs::metadata(boxed.agent().join("state"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for file in ["solos-agent.service", "solos-agent-health.timer"] {
        assert!(
            boxed.home.join(".config/systemd/user").join(file).exists(),
            "{file} was not installed"
        );
    }
}

#[test]
fn a_switch_refuses_a_config_that_names_another_users_runtime_directory_before_it_stops_anything() {
    let mut boxed = Host::new("uid");
    release(&boxed, "new", true);
    boxed._socket = Some(UnixListener::bind(boxed.state("broker.sock")).unwrap());
    let mine = uid();
    let other = mine + 1;
    std::fs::write(
        boxed.agent().join("agent.yaml"),
        format!(
            "sources:\n  - token_file: /run/user/{other}/solos-agent/discord-token\n\
             github:\n  key_file: /run/user/{other}/solos-agent/github-app.pem\n"
        ),
    )
    .unwrap();
    let (output, _) = boxed.on_host(&["switch", "new"], None);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        out(&output),
        err(&output)
    );
    let said = err(&output);
    assert!(
        said.contains(&format!(
            "/run/user/{other} but this user (uid {mine}) has /run/user/{mine}"
        )),
        "{said}"
    );
    assert!(
        boxed.calls().iter().all(|call| !call.contains("stop")),
        "a working agent is not stopped for a config that cannot work: {:?}",
        boxed.calls()
    );
    assert_eq!(
        std::fs::read_link(boxed.agent().join("current")).unwrap(),
        boxed.agent().join("releases/old")
    );

    std::fs::write(
        boxed.agent().join("agent.yaml"),
        format!("sources:\n  - token_file: /run/user/{mine}/solos-agent/discord-token\n"),
    )
    .unwrap();
    boxed.statuses(&[status(Vec::new())]);
    let (output, _) = boxed.on_host(&["switch", "new"], None);
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
}

#[test]
fn the_box_commands_find_the_users_manager_without_a_login_environment() {
    let boxed = Host::new("xdg");
    let run = |xdg: Option<&str>| {
        let mut command = Command::new("bash");
        command
            .arg(SCRIPT)
            .args(["--on-host", "stop"])
            .env("HOME", &boxed.home)
            .env("AGENT_DRAIN_SECS", "0")
            .env_remove("XDG_RUNTIME_DIR");
        if let Some(xdg) = xdg {
            command.env("XDG_RUNTIME_DIR", xdg);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}{}", out(&output), err(&output));
        let seen: Vec<String> = std::fs::read_to_string(boxed.home.join("fake/xdg.log"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        let _ = std::fs::remove_file(boxed.home.join("fake/xdg.log"));
        seen
    };
    let bare = run(None);
    assert!(!bare.is_empty());
    assert!(
        bare.iter()
            .all(|dir| *dir == format!("/run/user/{}", uid())),
        "no login set it: {bare:?}"
    );
    let given = run(Some("/run/user/4242"));
    assert!(
        given.iter().all(|dir| dir == "/run/user/4242"),
        "one that is set is kept: {given:?}"
    );
}

#[test]
fn status_says_which_user_it_is_looking_at() {
    let boxed = Host::new("who");
    boxed.statuses(&[status(Vec::new())]);
    let (output, _) = boxed.on_host(&["status"], None);
    let me = Command::new("id").arg("-un").output().unwrap();
    let me = String::from_utf8(me.stdout).unwrap();
    assert!(
        out(&output).starts_with(&format!("user: {} (uid {})\n", me.trim(), uid())),
        "{}",
        out(&output)
    );
}

/// Run git in `dir` with an identity and, when given, a date for the commit.
fn git(dir: &Path, args: &[&str], date: Option<&str>) -> String {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args);
    if let Some(date) = date {
        command
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_DATE", date);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The repository on the Mac that a deploy sends from.
struct Source {
    dir: PathBuf,
}

impl Source {
    fn new(root: &Path) -> Self {
        let dir = root.join("source");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"], None);
        Self { dir }
    }

    /// Commit these files, as of `date`; the commit's id.
    fn commit(&self, files: &[(&str, &str)], date: &str) -> String {
        for (name, text) in files {
            let path = self.dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        git(&self.dir, &["add", "-A"], None);
        git(&self.dir, &["commit", "-qm", "a change"], Some(date));
        git(&self.dir, &["rev-parse", "HEAD"], None)
    }
}

/// Cargo as far as a deploy depends on it: it builds `src/lib.rs` into the binary, and, as cargo does, it builds
/// nothing when no source file is newer than the binary it made last time.
const FAKE_CARGO: &str = "#!/bin/sh
echo \"cargo $* in $(pwd) into $CARGO_TARGET_DIR\" >> \"$HOME/fake/cargo.log\"
bin=\"$CARGO_TARGET_DIR/release/solos-swarm\"
mkdir -p \"$CARGO_TARGET_DIR/release\"
if [ -f \"$bin\" ] && [ -z \"$(find . -type f -newer \"$bin\" -print -quit)\" ]; then exit 0; fi
printf '#!/bin/sh\\n[ \"$1\" = doctor ] && exit 0\\n# built from: %s\\n' \"$(cat src/lib.rs)\" > \"$bin\"
chmod +x \"$bin\"
";

const FAKE_NPM: &str = "#!/bin/sh
echo \"npm $*\" >> \"$HOME/fake/npm.log\"
mkdir -p node_modules && echo ok > node_modules/.package-lock.json
";

impl Host {
    fn build_tools(&self) {
        executable(&self.home.join(".cargo/bin/cargo"), FAKE_CARGO);
        executable(
            &self.home.join(".local/opt/node-v24.18.0-linux-x64/bin/npm"),
            FAKE_NPM,
        );
    }

    /// What the Mac's push does: the commit goes into the bare repository as `refs/deploy/<first 12>`.
    fn receive(&self, source: &Source, commit: &str) -> String {
        let short = commit[..12].to_owned();
        let bare = self.agent().join("src.git");
        if !bare.exists() {
            git(&self.agent(), &["init", "-q", "--bare", "src.git"], None);
        }
        let refspec = format!("{commit}:refs/deploy/{short}");
        git(
            &source.dir,
            &["push", "-q", "--force", bare.to_str().unwrap(), &refspec],
            None,
        );
        short
    }

    fn log(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.home.join("fake").join(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn release_binary(&self, name: &str) -> String {
        std::fs::read_to_string(
            self.agent()
                .join("releases")
                .join(name)
                .join("bin/solos-swarm"),
        )
        .unwrap()
    }
}

fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

#[test]
fn a_build_compiles_in_one_tree_with_one_cache_and_never_ships_a_binary_older_than_its_source() {
    let boxed = Host::new("build");
    boxed.build_tools();
    let source = Source::new(boxed.home.parent().unwrap());
    let manifest = [
        ("Cargo.toml", "[package]\n"),
        ("package.json", "{}\n"),
        ("scripts/systemd/solos-agent.service", "# unit\n"),
        ("src/other.rs", "never changes\n"),
    ];
    let tree_at = |lib: &'static str| {
        let mut files = manifest.to_vec();
        files.push(("src/lib.rs", lib));
        files
    };
    let a = source.commit(&tree_at("one"), "2026-01-01T00:00:00Z");
    let b = source.commit(&tree_at("two"), "2026-01-02T00:00:00Z");
    // Older than everything built before it, as a deploy of an old commit is: a rollback by hand, or a merge
    // commit made before the previous deploy finished building.
    let c = source.commit(&tree_at("one"), "2025-01-01T00:00:00Z");
    let (a, b, c) = (
        boxed.receive(&source, &a),
        boxed.receive(&source, &b),
        boxed.receive(&source, &c),
    );

    let (output, _) = boxed.on_host(&["build", &a], None);
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    let release = boxed.agent().join("releases").join(&a);
    assert!(release.join(".complete").exists());
    assert_eq!(
        std::fs::read_to_string(release.join("REVISION"))
            .unwrap()
            .trim(),
        a
    );
    assert!(release.join("node_modules/.package-lock.json").exists());
    assert!(
        release.join("src/other.rs").exists(),
        "the release holds the commit's files"
    );
    assert_eq!(
        boxed.log("npm.log"),
        ["npm ci --ignore-scripts --no-audit --no-fund"],
        "no install script runs on the box"
    );
    assert!(boxed.release_binary(&a).contains("built from: one"));
    let tree = boxed.agent().join("src");
    let cargo = boxed.log("cargo.log");
    assert_eq!(cargo.len(), 1, "{cargo:?}");
    assert!(
        cargo[0].starts_with("cargo build --locked --release --bin solos-swarm in ")
            && cargo[0].contains(&format!(
                "in {} into {}",
                tree.display(),
                boxed.agent().join("target").display()
            )),
        "cargo builds in the one tree, into the one cache: {cargo:?}"
    );
    assert!(out(&output).contains("built release"), "{}", out(&output));
    let untouched = mtime(&tree.join("src/other.rs"));

    std::thread::sleep(Duration::from_millis(50));
    let (output, _) = boxed.on_host(&["build", &b], None);
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    assert!(boxed.release_binary(&b).contains("built from: two"));
    assert_eq!(
        mtime(&tree.join("src/other.rs")),
        untouched,
        "a file the next commit does not change keeps its time, so cargo does not rebuild for it"
    );
    assert_eq!(boxed.log("cargo.log").len(), 2);

    std::thread::sleep(Duration::from_millis(50));
    let (output, _) = boxed.on_host(&["build", &c], None);
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    assert!(
        boxed.release_binary(&c).contains("built from: one"),
        "the commit is older than the last build, and its release must still hold its own code, not the binary of {b}: {}",
        boxed.release_binary(&c)
    );
    assert_eq!(boxed.log("cargo.log").len(), 3);

    let (output, _) = boxed.on_host(&["build", &a], None);
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    assert!(
        out(&output).contains("is already built"),
        "{}",
        out(&output)
    );
    assert_eq!(
        (boxed.log("cargo.log").len(), boxed.log("npm.log").len()),
        (3, 3),
        "a release that is built is not built again"
    );
}

#[test]
fn a_build_of_a_commit_the_box_was_not_sent_says_so_and_builds_nothing() {
    let boxed = Host::new("nocommit");
    boxed.build_tools();
    git(&boxed.agent(), &["init", "-q", "--bare", "src.git"], None);
    let (output, _) = boxed.on_host(&["build", "0123456789ab"], None);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        err(&output).contains("the box has no commit"),
        "{}",
        err(&output)
    );
    assert!(boxed.log("cargo.log").is_empty());
    assert!(!boxed.agent().join("releases/0123456789ab").exists());
}

#[test]
fn a_failed_compile_shows_cargos_own_words_and_leaves_no_release() {
    let boxed = Host::new("brokenbuild");
    boxed.build_tools();
    executable(
        &boxed.home.join(".cargo/bin/cargo"),
        "#!/bin/sh\necho 'error[E0425]: cannot find value `x`' ; echo 'error: could not compile' ; exit 101\n",
    );
    let source = Source::new(boxed.home.parent().unwrap());
    let a = source.commit(
        &[("src/lib.rs", "one"), ("Cargo.toml", "[package]\n")],
        "2026-01-01T00:00:00Z",
    );
    let a = boxed.receive(&source, &a);
    let (output, _) = boxed.on_host(&["build", &a], None);
    assert_eq!(output.status.code(), Some(1));
    assert!(err(&output).contains("error[E0425]"), "{}", err(&output));
    assert!(!boxed.agent().join("releases").join(&a).exists());
}

/// The Mac's side of `deploy` and `prebuild`, with a `git` and an `ssh` that log what they are asked.
fn mac(command: &[&str]) -> (Output, Vec<String>) {
    let scratch = Scratch::new("mac-deploy");
    let bin = scratch.root.join("bin");
    let log = scratch.root.join("calls.log");
    let full = "0123456789abcdef0123456789abcdef01234567";
    executable(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\necho \"ssh $*\" >> {}\ncat > /dev/null\n",
            log.display()
        ),
    );
    executable(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\necho \"git $*\" >> {log}
case \"$*\" in
*\"rev-parse --show-toplevel\") echo /fake/repo ;;
*\"^{{commit}}\") echo {full} ;;
*\" log \"*) echo a one-line fix ;;
esac
",
            log = log.display()
        ),
    );
    let output = Command::new("bash")
        .arg(SCRIPT)
        .args(command)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("AGENT_HOST", "testbox")
        .env_remove("AGENT_DRAIN_SECS")
        .output()
        .unwrap();
    let calls = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    (output, calls)
}

fn position(calls: &[String], part: &str) -> usize {
    calls
        .iter()
        .position(|call| call.contains(part))
        .unwrap_or_else(|| panic!("no call has {part:?}: {calls:#?}"))
}

#[test]
fn a_deploy_pushes_only_what_the_box_lacks_then_builds_then_switches() {
    let (output, calls) = mac(&["deploy", "origin/main", "--drain", "7"]);
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    let init = position(&calls, "git init -q --bare ~/agent/src.git");
    let push = position(
        &calls,
        "git -C /fake/repo push -q --force testbox:agent/src.git 0123456789abcdef0123456789abcdef01234567:refs/deploy/0123456789ab",
    );
    let build = position(&calls, "--on-host build 0123456789ab");
    let switch = position(&calls, "--on-host switch 0123456789ab");
    assert!(init < push && push < build && build < switch, "{calls:#?}");
    assert!(
        calls[build].contains("AGENT_DRAIN_SECS=7") && calls[switch].contains("AGENT_DRAIN_SECS=7"),
        "{calls:#?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("receive")),
        "the archive of the whole tree is no longer sent: {calls:#?}"
    );
    let said = out(&output);
    assert!(
        said.contains("deploy 0123456789ab to testbox: a one-line fix"),
        "{said}"
    );
    assert!(said.contains("sent 0123456789ab"), "{said}");
    assert!(said.contains("deployed 0123456789ab in "), "{said}");
}

#[test]
fn a_prebuild_builds_and_does_not_touch_the_running_agent() {
    let (output, calls) = mac(&["prebuild", "feature"]);
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    position(&calls, "--on-host build 0123456789ab");
    for call in &calls {
        assert!(
            !call.contains("switch") && !call.contains("drain") && !call.contains("restart"),
            "a prebuild only builds: {call}"
        );
    }
    assert!(
        out(&output).contains("prebuilt 0123456789ab in "),
        "{}",
        out(&output)
    );
}

const SHIP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/ship");
const HEAD: &str = "1111111111111111111111111111111111111111";
const MERGED: &str = "2222222222222222222222222222222222222222";

/// `scripts/ship` with a `gh`, a `git` and an `agent-deploy` that log what they are asked to one file and answer from
/// files the test writes. `polls` are what the poll of the PR's `check` job says, one per call, the last repeated.
struct Ship {
    scratch: Scratch,
}

impl Ship {
    fn new(polls: &[String]) -> Self {
        let scratch = Scratch::new("ship");
        let fake = scratch.root.join("fake");
        std::fs::create_dir_all(&fake).unwrap();
        std::fs::write(fake.join("info"), format!("OPEN {HEAD} main false false\n")).unwrap();
        std::fs::write(fake.join("fetch_head"), format!("{HEAD}\n")).unwrap();
        std::fs::write(fake.join("merged"), format!("{MERGED}\n")).unwrap();
        for (n, poll) in polls.iter().enumerate() {
            std::fs::write(fake.join(format!("poll.{}", n + 1)), format!("{poll}\n")).unwrap();
        }
        std::fs::write(
            fake.join("poll.last"),
            format!("{}\n", polls.last().unwrap()),
        )
        .unwrap();
        let scripts = scratch.root.join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::copy(SHIP, scripts.join("ship")).unwrap();
        executable(
            &scripts.join("agent-deploy"),
            "#!/bin/sh
echo \"agent-deploy $*\" >> \"$FAKE/log\"
if [ \"$1\" = prebuild ]; then
  sleep 0.3
  [ -f \"$FAKE/prebuild.fail\" ] && { echo 'error: could not compile'; exit 1; }
  echo \"prebuilt $2 in 1s\"
  echo \"agent-deploy prebuild done\" >> \"$FAKE/log\"
fi
",
        );
        executable(
            &scratch.root.join("bin/gh"),
            "#!/bin/sh
echo \"gh $*\" >> \"$FAKE/log\"
case \"$*\" in
*state,headRefOid,baseRefName,isDraft*) cat \"$FAKE/info\" ;;
*headRefOid,statusCheckRollup*)
  n=$(cat \"$FAKE/polls\" 2>/dev/null || echo 0); n=$((n + 1)); echo $n > \"$FAKE/polls\"
  f=\"$FAKE/poll.$n\"; [ -f \"$f\" ] || f=\"$FAKE/poll.last\"
  [ \"$(cat \"$f\")\" = FAIL ] && exit 1
  cat \"$f\" ;;
\"pr merge\"*) [ -f \"$FAKE/merge.fail\" ] && { echo 'not mergeable' >&2; exit 1; }; echo merged ;;
*mergeCommit*) cat \"$FAKE/merged\" ;;
*) echo \"unexpected: gh $*\" >&2; exit 2 ;;
esac
",
        );
        executable(
            &scratch.root.join("bin/git"),
            "#!/bin/sh
echo \"git $*\" >> \"$FAKE/log\"
case \"$*\" in
\"rev-parse FETCH_HEAD\") cat \"$FAKE/fetch_head\" ;;
esac
exit 0
",
        );
        Self { scratch }
    }

    fn fake(&self, file: &str) -> PathBuf {
        self.scratch.root.join("fake").join(file)
    }

    fn run(&self, args: &[&str]) -> (Output, Vec<String>) {
        let fake = self.scratch.root.join("fake");
        let output = Command::new("bash")
            .arg(self.scratch.root.join("scripts/ship"))
            .args(args)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.scratch.root.join("bin").display()),
            )
            .env("FAKE", &fake)
            .env("SHIP_POLL", "0.2")
            .env("TMPDIR", &self.scratch.root)
            .output()
            .unwrap();
        let calls = std::fs::read_to_string(fake.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        (output, calls)
    }
}

fn poll(ci: &str) -> String {
    format!("{HEAD} {ci}")
}

#[test]
fn ship_builds_on_the_box_while_ci_runs_then_merges_on_green_and_deploys_the_merge() {
    let ship = Ship::new(&[
        poll("NONE"),
        poll("IN_PROGRESS/"),
        poll("IN_PROGRESS/"),
        poll("COMPLETED/SUCCESS"),
    ]);
    let (output, calls) = ship.run(&["7"]);
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    let fetch = position(&calls, "git fetch -q origin pull/7/head");
    let prebuild = position(&calls, &format!("agent-deploy prebuild {HEAD}"));
    let first_poll = position(&calls, "headRefOid,statusCheckRollup");
    let merge = position(
        &calls,
        &format!("gh pr merge 7 --merge --match-head-commit {HEAD}"),
    );
    let prebuilt = position(&calls, "agent-deploy prebuild done");
    let deploy = position(&calls, &format!("agent-deploy deploy {MERGED}"));
    assert!(
        fetch < prebuild,
        "the head is fetched, then built: {calls:#?}"
    );
    assert!(
        prebuild < merge && first_poll < merge,
        "the box builds while CI is polled, and the merge comes last: {calls:#?}"
    );
    assert!(
        prebuilt < deploy && merge < deploy,
        "the deploy waits for the prebuild and follows the merge: {calls:#?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.contains("headRefOid,statusCheckRollup"))
            .count(),
        4,
        "it polled until the check job was green: {calls:#?}"
    );
    let said = out(&output);
    for line in ["CI is green", "merged as 222222222222", "#7 is live"] {
        assert!(said.contains(line), "{line}: {said}");
    }
}

#[test]
fn ship_merges_nothing_when_ci_is_red_cancelled_or_the_head_moved() {
    for (ci, wanted) in [
        (poll("COMPLETED/FAILURE"), "CI is COMPLETED/FAILURE"),
        (poll("COMPLETED/CANCELLED"), "CI is COMPLETED/CANCELLED"),
        (
            format!("{} IN_PROGRESS/", "3".repeat(40)),
            "the head of #7 moved",
        ),
    ] {
        let ship = Ship::new(&[poll("IN_PROGRESS/"), ci]);
        let (output, calls) = ship.run(&["7"]);
        assert_eq!(output.status.code(), Some(1), "{wanted}");
        assert!(err(&output).contains(wanted), "{wanted}: {}", err(&output));
        assert!(
            !calls
                .iter()
                .any(|call| call.contains("pr merge") || call.contains("agent-deploy deploy")),
            "{wanted}: nothing is merged or deployed: {calls:#?}"
        );
    }
}

#[test]
fn ship_refuses_a_pull_request_that_is_closed_a_draft_from_a_fork_or_not_against_main_before_it_builds_anything()
 {
    for info in [
        format!("MERGED {HEAD} main false false"),
        format!("OPEN {HEAD} release false false"),
        format!("OPEN {HEAD} main true false"),
        format!("OPEN {HEAD} main false true"),
    ] {
        let ship = Ship::new(&[poll("COMPLETED/SUCCESS")]);
        std::fs::write(ship.fake("info"), format!("{info}\n")).unwrap();
        let (output, calls) = ship.run(&["7"]);
        assert_eq!(output.status.code(), Some(1), "{info}");
        assert!(
            !calls.iter().any(|call| call.contains("agent-deploy")),
            "{info}: {calls:#?}"
        );
    }
}

#[test]
fn ship_passes_the_drain_to_the_deploy_and_can_skip_the_prebuild() {
    for (args, wanted) in [
        (
            vec!["7", "--drain", "30"],
            format!("agent-deploy deploy {MERGED} --drain 30"),
        ),
        (
            vec!["--no-drain", "7"],
            format!("agent-deploy deploy {MERGED} --no-drain"),
        ),
        (vec!["7"], format!("agent-deploy deploy {MERGED}")),
    ] {
        let ship = Ship::new(&[poll("COMPLETED/SUCCESS")]);
        let (output, calls) = ship.run(&args);
        assert!(
            output.status.success(),
            "{args:?}: {}{}",
            out(&output),
            err(&output)
        );
        assert_eq!(calls.last().unwrap(), &wanted, "{args:?}: {calls:#?}");
    }
    let ship = Ship::new(&[poll("COMPLETED/SUCCESS")]);
    let (output, calls) = ship.run(&["7", "--no-prebuild"]);
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    assert!(
        !calls.iter().any(|call| call.contains("prebuild")),
        "{calls:#?}"
    );
    for args in [
        vec!["--drain", "soon", "7"],
        vec!["seven"],
        vec![],
        vec!["7", "8"],
        vec!["--nope", "7"],
    ] {
        let (output, _) = Ship::new(&[poll("COMPLETED/SUCCESS")]).run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn a_failed_prebuild_does_not_stop_the_ship_and_neither_does_a_github_that_does_not_answer_once() {
    let ship = Ship::new(&["FAIL".into(), poll("COMPLETED/SUCCESS")]);
    std::fs::write(ship.fake("prebuild.fail"), b"").unwrap();
    let (output, calls) = ship.run(&["7"]);
    assert!(output.status.success(), "{}{}", out(&output), err(&output));
    assert!(
        out(&output).contains("GitHub did not answer; trying again"),
        "{}",
        out(&output)
    );
    assert!(
        out(&output).contains("the prebuild failed, so the deploy builds it again")
            || err(&output).contains("could not compile"),
        "{}{}",
        out(&output),
        err(&output)
    );
    position(&calls, &format!("agent-deploy deploy {MERGED}"));
}

#[test]
fn a_merge_that_github_refuses_is_not_deployed() {
    let ship = Ship::new(&[poll("COMPLETED/SUCCESS")]);
    std::fs::write(ship.fake("merge.fail"), b"").unwrap();
    let (output, calls) = ship.run(&["7"]);
    assert!(!output.status.success());
    assert!(err(&output).contains("not mergeable"), "{}", err(&output));
    assert!(
        !calls
            .iter()
            .any(|call| call.contains("agent-deploy deploy")),
        "{calls:#?}"
    );
}
