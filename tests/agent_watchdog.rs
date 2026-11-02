//! The Watchdog inside `agent serve`. A Pi worker whose scripted model call
//! never answers stalls its Assignment: one alert reaches the operator's
//! channel, a restarted Watchdog tells nothing new, and with
//! `retry_stalled` the Assignment is retried once per Ticket. `agent status`
//! reads the same state as text and as JSON without writing to the Store.

use serde_json::{Value, json};
use solos_agent_core::model::{
    AssignmentState, MessageKind, OutboundMessage, Ticket, TicketOutcome,
};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::jsonl::JsonlChannel;
use solos_swarm::agent::watchdog::{Known, Snapshot, Watchdog, assess};
use solos_swarm::agent::{AgentConfig, HOST_RESTARTED, PiHost, STORE_FILE, serve_with};
use solos_swarm::broker::now_ms;
use solos_swarm::gateway::{Dispatch, Provider};
use solos_swarm::harness::scripted::{Call, ScriptedHarness};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(60);
/// Two Watchdog ticks and a margin: long enough for a repeat to show.
const SETTLE: Duration = Duration::from_millis(2_500);
const OPS: &str = "ops.jsonl";

/// The workers' model. Its first `hang` calls never answer; every later
/// one reports `ready`.
struct Model {
    calls: AtomicUsize,
    hang: usize,
}

impl Model {
    fn new(hang: usize) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            hang,
        })
    }
}

impl Provider for Model {
    fn dispatch(&self, _: Value) -> Dispatch {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let hang = n <= self.hang;
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
            let report = json!({"status":"ready","text":"Fixed the flaky retry test."});
            let call = json!({"id":"call-report","type":"function",
                "function":{"name":"worker_report","arguments":report.to_string()}});
            Ok(
                json!({"id":format!("model-{n}"),"created":1,"model":"deepseek-flash",
                "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[call]},
                            "finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":0,"completion_tokens":50}}),
            )
        })
    }
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
            "agent-watchdog-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("node"), "").unwrap();
        Self { root }
    }
    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    fn store(&self) -> SqliteStore {
        SqliteStore::open(self.state().join(STORE_FILE)).unwrap()
    }
    fn config_file(&self) -> PathBuf {
        self.root.join("agent.yaml")
    }
    /// What the agent told people, and what it told the operator.
    fn sent(&self, file: &str) -> Vec<OutboundMessage> {
        sent(&self.root.join(file))
    }
    fn replies(&self) -> Vec<(String, MessageKind)> {
        self.sent("outbox.jsonl")
            .iter()
            .map(|m| (m.channel.address["thread_ts"].clone(), m.kind))
            .collect()
    }
    fn ticket(&self) -> Ticket {
        let tickets = self.store().tickets().unwrap();
        let [ticket] = &tickets[..] else {
            panic!("{tickets:?}")
        };
        ticket.clone()
    }
    fn ask(&self, text: &str) {
        let line = json!({"id":"t-ask","source":"slack","at_ms":1_793_613_600_000u64,
            "actor":{"id":"alice","kind":"person"},"addressed":"mention","text":text,
            "threads":["T1"],"reply_to":[{"source":"slack","address":{"thread_ts":"T1"}}]});
        std::fs::write(self.root.join("triggers.jsonl"), format!("{line}\n")).unwrap();
    }
    /// Pi workers, a scripted Router, and a Watchdog that looks every
    /// second and calls a second of silence a stall.
    fn config(&self, retry_stalled: bool) -> AgentConfig {
        let yaml = format!(
            "policy:
  people: [slack:alice]
sources:
  - {{kind: jsonl, path: triggers.jsonl}}
channels:
  - {{kind: jsonl, source: slack, path: outbox.jsonl}}
  - {{kind: jsonl, source: ops, path: {OPS}}}
router: {{kind: scripted}}
executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {{input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}}
  max_output_tokens: 2048
budgets: {{session_microusd: 500000}}
watchdog:
  operator: {{source: ops, address: {{channel: D-OPS}}}}
  every: 1s
  retry_stalled: {retry_stalled}
  thresholds: {{stalled: 1s}}
"
        );
        std::fs::write(self.config_file(), yaml).unwrap();
        AgentConfig::load(&self.config_file()).unwrap()
    }
    fn host(&self, model: Arc<Model>) -> PiHost<ScriptedHarness> {
        PiHost {
            harness: ScriptedHarness::new(BTreeMap::new()).every_turn(vec![Call::Replied]),
            provider: model,
            pi_root: self.root.clone(),
            node: self.root.join("node"),
        }
    }
    async fn serve_until_idle(&self, config: AgentConfig, model: Arc<Model>) {
        let state = self.state();
        let serving = serve_with(config, &state, true, Some(self.host(model)));
        tokio::time::timeout(LIMIT, serving)
            .await
            .expect("serve did not go idle")
            .unwrap();
    }
    /// Serve in a runtime of its own until `operator` alerts have been out
    /// for [`SETTLE`], then drop it whole, as a crash would.
    fn serve_until_alerted(&self, config: AgentConfig, model: Arc<Model>, operator: usize) {
        let (state, host, ops) = (self.state(), self.host(model), self.root.join(OPS));
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let alerted = async {
                    let started = Instant::now();
                    let mut told = None;
                    loop {
                        assert!(started.elapsed() < LIMIT, "{operator} alert(s) never came");
                        if told.is_none() && sent(&ops).len() >= operator {
                            told = Some(Instant::now());
                        }
                        if told.is_some_and(|at| at.elapsed() >= SETTLE) {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                };
                tokio::select! {
                    ended = serve_with(config, &state, false, Some(host)) => {
                        panic!("serve ended: {ended:?}")
                    }
                    () = alerted => {}
                }
            });
            runtime.shutdown_timeout(Duration::from_secs(5));
        })
        .join()
        .unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("SOLOS_KEEP_SCRATCH").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// The whole lines of an outbox; one being written may not be whole yet.
fn sent(path: &Path) -> Vec<OutboundMessage> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn ack_and_result() -> Vec<(String, MessageKind)> {
    vec![
        ("T1".to_owned(), MessageKind::Acknowledgement),
        ("T1".to_owned(), MessageKind::Result),
    ]
}

/// The Store's file and its write-ahead log, byte for byte.
fn store_bytes(state: &Path) -> (Vec<u8>, Vec<u8>) {
    let db = state.join(STORE_FILE);
    let wal = db.with_file_name(format!("{STORE_FILE}-wal"));
    (
        std::fs::read(&db).unwrap(),
        std::fs::read(&wal).unwrap_or_default(),
    )
}

/// `agent status` as text and as JSON, checking that neither changed the
/// Store.
fn status(scratch: &Scratch) -> (String, Value) {
    let before = store_bytes(&scratch.state());
    let run = |json: bool| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_solos-swarm"));
        command
            .args(["agent", "status", "--state-dir"])
            .arg(scratch.state())
            .arg("--config")
            .arg(scratch.config_file());
        if json {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let text = String::from_utf8(run(false)).unwrap();
    let report: Value = serde_json::from_slice(&run(true)).unwrap();
    assert!(
        store_bytes(&scratch.state()) == before,
        "agent status changed the store"
    );
    (text, report)
}

#[tokio::test]
async fn a_stalled_worker_alerts_the_operator_once_and_a_restart_tells_nothing_new() {
    let scratch = Scratch::new("stall");
    scratch.ask("Fix the flaky retry test");
    let config = scratch.config(false);

    scratch.serve_until_alerted(config.clone(), Model::new(usize::MAX), 1);

    let alerts = scratch.sent(OPS);
    let [alert] = &alerts[..] else {
        panic!("one alert, told once: {alerts:?}")
    };
    let ticket = scratch.ticket();
    let stalled = ticket.running_assignment().expect("the worker still runs");
    assert_eq!(alert.kind, MessageKind::Attention);
    assert_eq!(alert.channel.source, "ops");
    assert_eq!(alert.channel.address["channel"], "D-OPS");
    assert_eq!(alert.ticket.as_ref(), Some(&ticket.id));
    assert!(alert.text.contains(stalled.id.as_str()), "{}", alert.text);
    assert!(alert.text.contains("has sent no report"), "{}", alert.text);
    assert!(!alert.text.contains("retries"), "no action is configured");
    assert_eq!(
        scratch.replies(),
        [("T1".to_owned(), MessageKind::Acknowledgement)]
    );
    let rows = scratch.store().alerts().unwrap();
    let [row] = &rows[..] else { panic!("{rows:?}") };
    assert_eq!(
        (row.id.as_str(), row.kind.as_str()),
        (alert.id.as_str(), "stalled")
    );
    assert!(row.sent_ms.is_some() && !row.retry);

    let (text, report) = status(&scratch);
    eprintln!("{text}");
    assert!(text.contains(&format!("{} working", ticket.id)), "{text}");
    assert!(
        text.contains("asked by slack:alice (person) on T1"),
        "{text}"
    );
    assert!(text.contains("alerts now: 1\n  [stalled]"), "{text}");
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["tickets"][0]["stage"], "working");
    assert_eq!(
        report["tickets"][0]["request"]["text"],
        "Fix the flaky retry test"
    );
    assert_eq!(report["tickets"][0]["plan"]["kind"], "session");
    assert_eq!(report["alerts"][0]["id"], alert.id.as_str());
    assert_eq!(report["thresholds"]["stalled_ms"], 1_000);
    assert!(report["decisions"][0]["kind"] == "accept");

    // The worker is still stalled, so a restarted Watchdog raises the same alert, and tells nobody.
    let settings = config.watchdog.clone().unwrap();
    let now = now_ms();
    let store = SqliteStore::open_read_only(scratch.state().join(STORE_FILE)).unwrap();
    let snapshot = Snapshot::read(
        &store,
        &scratch.state(),
        now,
        Known::of(&config),
        &settings.thresholds(),
    )
    .unwrap();
    let again = assess(&snapshot, now, &settings.thresholds());
    assert_eq!(again.len(), 1);
    assert_eq!(
        again[0].id,
        alert.id.as_str(),
        "the same problem in the same bucket"
    );
    let channel = Arc::new(JsonlChannel::open(scratch.root.join(OPS)).unwrap());
    let mut watchdog = Watchdog::open(
        &scratch.state(),
        &settings,
        Known::of(&config),
        channel,
        None,
    )
    .unwrap();
    assert_eq!(watchdog.tick(now).await.unwrap(), 0);
    assert_eq!(scratch.sent(OPS).len(), 1);

    // A restarted agent recovers the work, which now finishes, and tells nothing new.
    scratch.serve_until_idle(config, Model::new(0)).await;

    assert_eq!(scratch.sent(OPS).len(), 1);
    assert_eq!(scratch.replies(), ack_and_result());
    let ticket = scratch.ticket();
    let ends: Vec<_> = ticket
        .assignments
        .iter()
        .map(|a| a.ended_reason.as_deref())
        .collect();
    assert_eq!(ends, [Some(HOST_RESTARTED), Some("ready")]);
    assert_eq!(
        ticket.ended.map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    let (text, _) = status(&scratch);
    assert!(text.contains("open tickets: 0\n"), "{text}");
    assert!(text.contains("alerts now: 0\n"), "{text}");
}

#[tokio::test]
async fn the_retry_action_retries_a_stalled_assignment_once_per_ticket() {
    let scratch = Scratch::new("retry");
    scratch.ask("Fix the flaky retry test");
    let config = scratch.config(true);
    // The first two workers stall: the first is retried, the second only told.
    let model = Model::new(2);

    scratch.serve_until_alerted(config.clone(), model.clone(), 2);

    let ticket = scratch.ticket();
    let states: Vec<_> = ticket
        .assignments
        .iter()
        .map(|a| (a.state, a.ended_reason.as_deref()))
        .collect();
    assert_eq!(
        states,
        [
            (AssignmentState::Ended, Some("retried")),
            (AssignmentState::Running, None)
        ],
        "retried once, not twice"
    );
    let alerts = scratch.sent(OPS);
    let [first, second] = &alerts[..] else {
        panic!("{alerts:?}")
    };
    assert!(
        first.text.contains(ticket.assignments[0].id.as_str()),
        "{}",
        first.text
    );
    assert!(
        first.text.ends_with("The watchdog retries it once."),
        "{}",
        first.text
    );
    assert!(
        second.text.contains(ticket.assignments[1].id.as_str()),
        "{}",
        second.text
    );
    assert!(!second.text.contains("retries"), "{}", second.text);
    let retried: Vec<bool> = scratch
        .store()
        .alerts()
        .unwrap()
        .iter()
        .map(|row| row.retry)
        .collect();
    assert_eq!(retried, [true, false]);
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        2,
        "one model call per worker"
    );

    scratch.serve_until_idle(config, model).await;

    assert_eq!(scratch.sent(OPS).len(), 2, "a restart tells nothing new");
    assert_eq!(scratch.replies(), ack_and_result());
    let ticket = scratch.ticket();
    let ends: Vec<_> = ticket
        .assignments
        .iter()
        .map(|a| a.ended_reason.as_deref())
        .collect();
    assert_eq!(ends, [Some("retried"), Some(HOST_RESTARTED), Some("ready")]);
    assert_eq!(
        ticket.ended.map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
}
