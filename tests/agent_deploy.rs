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
    ] {
        assert!(text.contains(word), "the usage text lacks {word}: {text}");
    }
}
