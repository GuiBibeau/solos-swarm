//! `agent serve` with Pi workers whose model is scripted: each Ticket runs
//! on a worker session that calls the model and its tools through the agent
//! broker, on its Ticket's account. The sessions play with
//! `ScriptedHarness` (each turn runs the tool calls the scripted model
//! asks for); the ignored test runs the real `PiHarness` (Pi, node and
//! bwrap) and needs `SOLOS_PI_ROOT` and `SOLOS_NODE`.

use serde_json::{Value, json};
use solos_agent_core::model::{
    AssignmentState, DecisionKind, InputBody, MessageKind, OutboundMessage, ReportStatus, Ticket,
    TicketOutcome, TriggerId,
};
use solos_agent_store::{SpendState, SqliteStore};
use solos_swarm::agent::pi_executor::WORK_DIR;
use solos_swarm::agent::{
    AgentConfig, HOST_RESTARTED, PiHost, STORE_FILE, Summary, TICKETS_DIR, serve_with,
};
use solos_swarm::gateway::{Dispatch, Provider};
use solos_swarm::harness::scripted::{Call, ScriptedHarness, ScriptedSession};
use solos_swarm::harness::{AgentHarness, Observation, PiHarness, Session, SessionSpec};
use solos_swarm::vault::{SandboxView, Vault, VaultConfig};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

const LIMIT: Duration = Duration::from_secs(120);
const ROUTER: &str = "router:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}
  max_output_tokens: 2048
";
const WORKERS: &str = "executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}
  max_output_tokens: 2048
";

/// The tool calls a worker's model makes for one prompt.
type Worker = Box<dyn Fn(&str) -> Vec<(&'static str, Value)> + Send + Sync>;

/// The scripted model of the Router and the workers. A Router prompt (its
/// cards) gets a `router_decide` per card: explicit candidates are
/// accepted, with a tiny budget when they ask for one, and inferred ones
/// ignored. A worker prompt gets what `worker` asks for. After a tool result
/// either one ends its turn.
struct Model {
    worker: Worker,
    bodies: Mutex<Vec<Value>>,
    /// The first worker call never answers, and tells this.
    hang: Option<Arc<Notify>>,
    /// How long a worker call takes.
    delay: Duration,
}

impl Model {
    fn new(worker: Worker) -> Arc<Self> {
        Arc::new(Self {
            worker,
            bodies: Mutex::new(Vec::new()),
            hang: None,
            delay: Duration::ZERO,
        })
    }
    fn prompts(&self) -> Vec<String> {
        let bodies = self.bodies.lock().unwrap();
        bodies.iter().map(last_text).collect()
    }
    fn worker_prompts(&self) -> Vec<String> {
        self.prompts()
            .into_iter()
            .filter(|p| p.starts_with("Ticket ") || p.starts_with("New input"))
            .collect()
    }
}

fn text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        parts => parts
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|part| part["text"].as_str())
            .collect(),
    }
}

fn last_text(body: &Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .map(text)
        .unwrap_or_default()
}

/// `(eval, strength, first quoted line)` of each card in a Router prompt.
fn cards(prompt: &str) -> Vec<(String, String, String)> {
    let mut found: Vec<(String, String, String)> = Vec::new();
    for line in prompt.lines() {
        if let Some(rest) = line.strip_prefix("eval ") {
            let fields: Vec<&str> = rest.split(" | ").collect();
            found.push((fields[0].into(), fields[2].into(), String::new()));
        } else if let (Some(text), Some(card)) = (line.strip_prefix("> "), found.last_mut())
            && card.2.is_empty()
        {
            card.2 = text.chars().take(60).collect();
        }
    }
    found
}

fn call(name: &str, arguments: &Value) -> Value {
    json!({"id":format!("call-{name}"),"type":"function",
           "function":{"name":name,"arguments":arguments.to_string()}})
}

impl Provider for Model {
    fn dispatch(&self, body: Value) -> Dispatch {
        let n = {
            let mut bodies = self.bodies.lock().unwrap();
            bodies.push(body.clone());
            bodies.len()
        };
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let last = messages.last().cloned().unwrap_or(Value::Null);
        let prompt = text(&last);
        let router = prompt.contains(" new candidate");
        let calls: Vec<Value> = if last["role"] != "user" {
            Vec::new()
        } else if router {
            cards(&prompt)
                .into_iter()
                .map(|(eval, strength, title)| {
                    let mut decide = if strength == "explicit" {
                        json!({"eval":eval,"decision":"accept","reason":"a person asked for it",
                               "title":title,"scope":format!("Do what was asked: {title}")})
                    } else {
                        json!({"eval":eval,"decision":"ignore","reason":"nobody asked for work"})
                    };
                    if title.contains("tiny budget") {
                        decide["budget_microusd"] = json!(100);
                    }
                    call("router_decide", &decide)
                })
                .collect()
        } else {
            (self.worker)(&prompt)
                .into_iter()
                .map(|(name, arguments)| call(name, &arguments))
                .collect()
        };
        let message = if calls.is_empty() {
            json!({"role":"assistant","content":"Done."})
        } else {
            json!({"role":"assistant","content":null,"tool_calls":calls})
        };
        let finish = if calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        let hang = self.hang.clone().filter(|_| !router);
        let delay = if router { Duration::ZERO } else { self.delay };
        Box::pin(async move {
            if let Some(hang) = hang {
                hang.notify_one();
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(delay).await;
            Ok(
                json!({"id":format!("model-{n}"),"created":1,"model":"deepseek-flash",
                      "choices":[{"index":0,"message":message,"finish_reason":finish}],
                      "usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":0,"completion_tokens":50}}),
            )
        })
    }
}

fn report(status: &str, text: &str) -> (&'static str, Value) {
    ("worker_report", json!({"status":status,"text":text}))
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
            "agent-worker-{name}-{}-{}",
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
    fn sent(&self) -> Vec<OutboundMessage> {
        std::fs::read_to_string(self.root.join("outbox.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    /// `(thread, kind)` of every reply, in order.
    fn replies(&self) -> Vec<(String, MessageKind)> {
        self.sent()
            .iter()
            .map(|m| (m.channel.address["thread_ts"].clone(), m.kind))
            .collect()
    }
    fn ask(&self, id: &str, thread: &str, text: &str) {
        let line = json!({"id":id,"source":"slack","at_ms":1_793_613_600_000u64,
            "actor":{"id":"alice","kind":"person"},"addressed":"mention","text":text,
            "threads":[thread],"reply_to":[{"source":"slack","address":{"thread_ts":thread}}]});
        let mut lines =
            std::fs::read_to_string(self.root.join("triggers.jsonl")).unwrap_or_default();
        lines.push_str(&format!("{line}\n"));
        std::fs::write(self.root.join("triggers.jsonl"), lines).unwrap();
    }
    /// A config with a Pi Router when `router`, else a scripted one, and
    /// Pi workers with `executor` added to their settings.
    fn config(&self, router: bool, executor: &str) -> AgentConfig {
        let router = if router {
            ROUTER
        } else {
            "router: {kind: scripted}\n"
        };
        let yaml = format!(
            "policy:\n  people: [slack:alice]\n\
             sources:\n  - {{kind: jsonl, path: triggers.jsonl}}\n\
             channels:\n  - {{kind: jsonl, source: slack, path: outbox.jsonl}}\n\
             {router}{WORKERS}{executor}\
             budgets: {{session_microusd: 500000, router_daily_microusd: 1000000}}\n"
        );
        let path = self.root.join("agent.yaml");
        std::fs::write(&path, yaml).unwrap();
        AgentConfig::load(&path).unwrap()
    }
    fn host<H>(&self, harness: H, model: Arc<Model>) -> PiHost<H> {
        PiHost {
            harness,
            provider: model,
            pi_root: self.root.clone(),
            node: self.root.join("node"),
            browser: solos_swarm::browse::Launcher::Lightpanda,
        }
    }
    async fn serve<H>(&self, config: AgentConfig, host: PiHost<H>) -> Summary
    where
        H: AgentHarness + Send + Sync + 'static,
        H::Session: Send + 'static,
    {
        let state = self.state();
        let serving = serve_with(config, &state, true, Some(host));
        tokio::time::timeout(LIMIT, serving)
            .await
            .expect("serve did not go idle")
            .unwrap()
    }
    fn tickets(&self) -> Vec<Ticket> {
        let mut tickets = self.store().tickets().unwrap();
        tickets.sort_by_key(|t| t.created_ms);
        tickets
    }
    fn work_dir(&self, ticket: &Ticket) -> PathBuf {
        self.state()
            .join(TICKETS_DIR)
            .join(ticket.id.as_str())
            .join(WORK_DIR)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("SOLOS_KEEP_SCRATCH").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn scripted() -> ScriptedHarness {
    ScriptedHarness::new(BTreeMap::new()).every_turn(vec![Call::Replied])
}

fn ack_and_result(thread: &str) -> Vec<(String, MessageKind)> {
    vec![
        (thread.to_owned(), MessageKind::Acknowledgement),
        (thread.to_owned(), MessageKind::Result),
    ]
}

fn assert_settled(store: &SqliteStore) {
    let pending: Vec<_> = store
        .effects()
        .unwrap()
        .into_iter()
        .filter(|q| !q.done)
        .collect();
    assert!(pending.is_empty(), "effects not done: {pending:?}");
    store.verify_chain().unwrap();
}

fn sandbox() -> bool {
    let found = cfg!(target_os = "linux")
        && Path::new("/usr/bin/bwrap").is_file()
        && Path::new("/usr/bin/prlimit").is_file();
    if !found {
        eprintln!("skipped: run_code needs Linux, /usr/bin/bwrap and /usr/bin/prlimit");
    }
    found
}

#[tokio::test]
async fn a_request_is_accepted_launched_worked_with_run_code_and_answered_once() {
    if !sandbox() {
        return;
    }
    let scratch = Scratch::new("e2e");
    scratch.ask("t-ask", "T1", "Count the lines of the release notes");
    let model = Model::new(Box::new(|_| {
        vec![
            (
                "run_code",
                json!({"code":"cat prepared.txt > seen.txt && echo 3 > count.txt && cat count.txt"}),
            ),
            report("ready", "The release notes have 3 lines."),
        ]
    }));
    let executor =
        "  prepare: echo \"$SOLOS_TICKET\" > \"$SOLOS_WORK_DIR/prepared.txt\"\n  run_code: {}\n";
    let config = scratch.config(true, executor);

    let summary = scratch
        .serve(config, scratch.host(scripted(), model.clone()))
        .await;

    assert_eq!((summary.triggers_received, summary.effects_pending), (1, 0));
    assert_eq!(scratch.replies(), ack_and_result("T1"));
    assert_eq!(scratch.sent()[1].text, "The release notes have 3 lines.");
    let tickets = scratch.tickets();
    let [ticket] = &tickets[..] else {
        panic!("{tickets:?}")
    };
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    let work = scratch.work_dir(ticket);
    assert_eq!(
        std::fs::read_to_string(work.join("seen.txt")).unwrap(),
        format!("{}\n", ticket.id),
        "run_code ran where the prepare command wrote"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("count.txt")).unwrap(),
        "3\n"
    );
    let prompts = model.worker_prompts();
    let [assignment] = &prompts[..] else {
        panic!("{prompts:?}")
    };
    assert!(
        assignment.starts_with(&format!("Ticket {}: Count the lines", ticket.id)),
        "{assignment}"
    );
    assert!(assignment.contains("Scope:\nDo what was asked: Count the lines"));
    assert!(!assignment.contains("worked on before"));

    let store = scratch.store();
    let account = format!("ticket:{}", ticket.id);
    let worker_rows = store.spend(&account).unwrap();
    assert_eq!(worker_rows.len(), 1, "one worker model call");
    assert!(worker_rows.iter().all(|r| r.state == SpendState::Settled));
    let router_rows = store.spend("router").unwrap();
    assert!(!router_rows.is_empty());
    assert!(router_rows.iter().all(|r| r.state == SpendState::Settled));
    let ready = ticket.latest_report().unwrap();
    assert_eq!(ready.status, ReportStatus::Ready);
    assert_eq!(
        ready.spent_microusd,
        store.committed(&account, 0).unwrap(),
        "the report's spend is the ledger's"
    );
    assert_eq!(ready.spent_microusd, 90);
    assert_settled(&store);
}

#[tokio::test]
async fn a_worker_at_its_tickets_budget_is_refused_and_the_router_still_judges() {
    let scratch = Scratch::new("budget");
    scratch.ask("t-tiny", "T1", "Do this on a tiny budget please");
    let model = Model::new(Box::new(|_| vec![report("ready", "Done.")]));

    scratch
        .serve(
            scratch.config(true, ""),
            scratch.host(scripted(), model.clone()),
        )
        .await;
    let tickets = scratch.tickets();
    let [tiny] = &tickets[..] else {
        panic!("{tickets:?}")
    };
    assert_eq!(tiny.mandate.budget_microusd, 100);
    let sent = scratch.sent();
    assert_eq!(
        scratch.replies(),
        [
            ("T1".to_owned(), MessageKind::Acknowledgement),
            ("T1".to_owned(), MessageKind::Attention)
        ]
    );
    assert!(
        sent[1].text.contains("budget is reached"),
        "{}",
        sent[1].text
    );
    let store = scratch.store();
    let tiny_account = format!("ticket:{}", tiny.id);
    assert!(
        store.spend(&tiny_account).unwrap().is_empty(),
        "a refusal holds nothing"
    );
    let judged = store.spend("router").unwrap().len();

    scratch.ask("t-next", "T2", "Summarise the incident report");
    scratch
        .serve(
            scratch.config(true, ""),
            scratch.host(scripted(), model.clone()),
        )
        .await;

    let rows = scratch.store().spend("router").unwrap();
    assert!(
        rows.len() > judged,
        "the Router judged again after the refusal"
    );
    assert!(rows.iter().all(|r| r.state == SpendState::Settled));
    let replies = scratch.replies();
    assert_eq!(replies[2..], ack_and_result("T2")[..]);
    let tickets = scratch.tickets();
    assert_eq!(
        tickets[1].ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    assert!(
        tickets[0].ended.is_none(),
        "the refused Ticket waits for a person"
    );
    assert_settled(&scratch.store());
}

#[tokio::test]
async fn a_follow_up_the_worker_hands_back_reaches_a_fresh_eval() {
    let scratch = Scratch::new("handback");
    scratch.ask("t-ask", "T1", "Fix the flaky retry test");
    let model = Model::new(Box::new(|prompt| {
        if prompt.contains("\n- trigger t-more") {
            vec![
                (
                    "worker_handback",
                    json!({"trigger":"t-more","reason":"key rotation is separate work"}),
                ),
                report("waiting", "Handed the key rotation back."),
            ]
        } else if prompt.contains("Scope:\nAlso rotate") {
            vec![report("ready", "Rotated the staging keys.")]
        } else {
            vec![report("waiting", "Fixed locally; waiting for CI.")]
        }
    }));
    let config = || scratch.config(false, "");
    scratch
        .serve(config(), scratch.host(scripted(), model.clone()))
        .await;
    assert_eq!(
        scratch.replies(),
        [("T1".to_owned(), MessageKind::Acknowledgement)]
    );

    scratch.ask("t-more", "T1", "Also rotate the staging keys");
    scratch
        .serve(config(), scratch.host(scripted(), model.clone()))
        .await;

    let store = scratch.store();
    let more = TriggerId::new("t-more");
    let fresh: Vec<_> = store
        .decisions()
        .unwrap()
        .into_iter()
        .filter(|d| d.triggers == [more.clone()])
        .collect();
    let [fresh] = &fresh[..] else {
        panic!("{fresh:?}")
    };
    assert!(fresh.eval.is_some(), "the Router judged it in an Eval");
    assert_eq!(fresh.kind, DecisionKind::Accept);
    let tickets = scratch.tickets();
    let [first, second] = &tickets[..] else {
        panic!("{tickets:?}")
    };
    assert!(first.handed_back.contains(&more));
    assert!(first.ended.is_none());
    assert_eq!(second.origin, more);
    assert_eq!(
        second.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    assert_eq!(
        scratch.replies(),
        [
            ("T1".to_owned(), MessageKind::Acknowledgement),
            ("T1".to_owned(), MessageKind::Acknowledgement),
            ("T1".to_owned(), MessageKind::Result)
        ]
    );
    assert_settled(&store);
}

#[tokio::test]
async fn a_crash_during_a_running_assignment_restarts_it_as_a_recovery_without_repeating_replies() {
    let scratch = Scratch::new("crash");
    scratch.ask("t-ask", "T1", "Fix the flaky retry test");
    let hung = Arc::new(Notify::new());
    let first = Arc::new(Model {
        worker: Box::new(|_| vec![report("ready", "unreachable")]),
        bodies: Mutex::new(Vec::new()),
        hang: Some(hung.clone()),
        delay: Duration::ZERO,
    });
    // Its own runtime, dropped whole as a crash would: every task, the
    // broker and the worker session go with it.
    let crashed = {
        let (config, host, state) = (
            scratch.config(false, ""),
            scratch.host(scripted(), first.clone()),
            scratch.state(),
        );
        let store = scratch.state().join(STORE_FILE);
        let outbox = scratch.root.join("outbox.jsonl");
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let running = async {
                    hung.notified().await;
                    for _ in 0..1_000 {
                        let store = SqliteStore::open(&store).unwrap();
                        let ticket = store.tickets().unwrap().into_iter().next();
                        let acknowledged =
                            std::fs::read_to_string(&outbox).is_ok_and(|sent| !sent.is_empty());
                        if acknowledged && ticket.is_some_and(|t| t.running_assignment().is_some())
                        {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    panic!("the assignment never ran");
                };
                tokio::select! {
                    ended = serve_with(config, &state, false, Some(host)) => {
                        panic!("serve ended: {ended:?}")
                    }
                    () = running => {}
                }
            });
            runtime.shutdown_timeout(Duration::from_secs(5));
        })
    };
    crashed.join().unwrap();
    assert_eq!(
        scratch.replies(),
        [("T1".to_owned(), MessageKind::Acknowledgement)]
    );

    let second = Model::new(Box::new(|prompt| {
        if prompt.contains("This ticket was worked on before") {
            vec![report("ready", "Fixed after checking what was done.")]
        } else {
            vec![report("failed", "not told it was a recovery")]
        }
    }));
    scratch
        .serve(
            scratch.config(false, ""),
            scratch.host(scripted(), second.clone()),
        )
        .await;

    assert_eq!(scratch.replies(), ack_and_result("T1"), "no reply twice");
    let tickets = scratch.tickets();
    let ticket = &tickets[0];
    let assignments: Vec<_> = ticket
        .assignments
        .iter()
        .map(|a| (a.state, a.ended_reason.clone()))
        .collect();
    assert_eq!(
        assignments,
        [
            (AssignmentState::Ended, Some(HOST_RESTARTED.to_owned())),
            (AssignmentState::Ended, Some("ready".to_owned()))
        ]
    );
    assert_eq!(second.worker_prompts().len(), 1);
    let rows = scratch
        .store()
        .spend(&format!("ticket:{}", ticket.id))
        .unwrap();
    let states: Vec<_> = rows.iter().map(|r| r.state).collect();
    assert_eq!(
        states,
        [SpendState::Unknown, SpendState::Settled],
        "the call the crash cut short is charged at its hold"
    );
    assert_settled(&scratch.store());
}

fn notes(ticket: &Ticket) -> Vec<&str> {
    ticket
        .inputs
        .iter()
        .filter_map(|input| match &input.body {
            InputBody::Note { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The command lines on this host that carry `marker`.
fn command_lines(marker: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| std::fs::read(entry.path().join("cmdline")).ok())
        .map(|cmdline| String::from_utf8_lossy(&cmdline).into_owned())
        .filter(|cmdline| cmdline.contains(marker))
        .collect()
}

#[tokio::test]
async fn a_background_job_that_ends_between_turns_wakes_its_worker_once_with_its_result() {
    if !sandbox() {
        return;
    }
    let scratch = Scratch::new("job");
    scratch.ask("t-ask", "T1", "Build the release notes in the background");
    let model = Model::new(Box::new(|prompt| {
        if prompt.starts_with("Ticket ") {
            let build = "sleep 2 && echo notes built > notes.txt && cat notes.txt";
            vec![("job_start", json!({"code": build}))]
        } else if prompt.contains("- a note from the agent") && prompt.contains("notes built") {
            vec![report("ready", "The release notes are built.")]
        } else {
            vec![report("failed", "an input without the job's result")]
        }
    }));

    let summary = scratch
        .serve(
            scratch.config(false, "  run_code: {}\n"),
            scratch.host(scripted(), model.clone()),
        )
        .await;

    assert_eq!((summary.triggers_received, summary.effects_pending), (1, 0));
    assert_eq!(scratch.replies(), ack_and_result("T1"));
    assert_eq!(scratch.sent()[1].text, "The release notes are built.");
    let tickets = scratch.tickets();
    let [ticket] = &tickets[..] else {
        panic!("{tickets:?}")
    };
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    let [note] = notes(ticket)[..] else {
        panic!("one note: {:?}", ticket.inputs)
    };
    assert!(note.starts_with("Background job jb_"), "{note}");
    assert!(note.contains("exited with code 0"), "{note}");
    assert!(note.contains("stdout:\nnotes built"), "{note}");
    let prompts = model.worker_prompts();
    let [assignment, woken] = &prompts[..] else {
        panic!("the assignment and one wake-up: {prompts:?}")
    };
    assert!(assignment.starts_with("Ticket "), "{assignment}");
    assert!(woken.starts_with("New input on ticket"), "{woken}");
    assert_eq!(
        prompts.iter().filter(|p| p.contains("notes built")).count(),
        1,
        "the result reached the worker once"
    );
    let statuses: Vec<_> = ticket.assignments[0]
        .reports
        .iter()
        .map(|r| r.status)
        .collect();
    assert_eq!(
        statuses,
        [ReportStatus::Progress, ReportStatus::Ready],
        "a turn that ends with a job running is still work"
    );
    assert_eq!(
        std::fs::read_to_string(scratch.work_dir(ticket).join("notes.txt")).unwrap(),
        "notes built\n"
    );
    assert_settled(&scratch.store());
}

#[tokio::test]
async fn ending_a_ticket_kills_its_background_jobs_and_reports_none() {
    if !sandbox() {
        return;
    }
    let scratch = Scratch::new("job-end");
    scratch.ask("t-ask", "T1", "Start the long build and hand it over");
    let marker = format!("job-{}", uuid::Uuid::new_v4().simple());
    let code = format!("echo > started && exec -a {marker} sleep 300");
    let model = Model::new(Box::new(move |_| {
        vec![
            ("job_start", json!({"code": code})),
            ("run_code", json!({"code": "sleep 1"})),
            report("ready", "The build runs."),
        ]
    }));

    scratch
        .serve(
            scratch.config(false, "  run_code: {}\n"),
            scratch.host(scripted(), model),
        )
        .await;

    assert_eq!(scratch.replies(), ack_and_result("T1"));
    let mut left = command_lines(&marker);
    for _ in 0..200 {
        if left.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        left = command_lines(&marker);
    }
    assert!(left.is_empty(), "the job outlived its ticket: {left:?}");
    let tickets = scratch.tickets();
    assert!(
        scratch.work_dir(&tickets[0]).join("started").is_file(),
        "the job ran"
    );
    assert!(notes(&tickets[0]).is_empty(), "{:?}", tickets[0].inputs);
    assert_settled(&scratch.store());
}

/// A harness that counts the sessions alive at once.
struct Counting {
    inner: ScriptedHarness,
    live: Arc<AtomicUsize>,
    most: Arc<AtomicUsize>,
}

struct CountedSession {
    inner: ScriptedSession,
    live: Arc<AtomicUsize>,
    stopped: bool,
}

impl AgentHarness for Counting {
    type Session = CountedSession;
    async fn start(
        &self,
        spec: &SessionSpec,
        events: mpsc::Sender<Observation>,
    ) -> anyhow::Result<CountedSession> {
        let inner = self.inner.start(spec, events).await?;
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(live, Ordering::SeqCst);
        Ok(CountedSession {
            inner,
            live: self.live.clone(),
            stopped: false,
        })
    }
}

impl Session for CountedSession {
    fn id(&self) -> &str {
        self.inner.id()
    }
    async fn prompt(&mut self, text: &str) -> anyhow::Result<()> {
        self.inner.prompt(text).await
    }
    async fn stop(&mut self) -> anyhow::Result<bool> {
        if !std::mem::replace(&mut self.stopped, true) {
            self.live.fetch_sub(1, Ordering::SeqCst);
        }
        self.inner.stop().await
    }
    async fn join(self) -> usize {
        self.inner.join().await
    }
}

#[tokio::test]
async fn no_more_workers_live_than_the_limit_and_every_ticket_is_still_answered() {
    let scratch = Scratch::new("limit");
    for (id, thread) in [("t-1", "T1"), ("t-2", "T2"), ("t-3", "T3")] {
        scratch.ask(id, thread, &format!("Summarise incident {id}"));
    }
    let model = Arc::new(Model {
        worker: Box::new(|_| vec![report("ready", "Summarised.")]),
        bodies: Mutex::new(Vec::new()),
        hang: None,
        delay: Duration::from_millis(150),
    });
    let (live, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let harness = Counting {
        inner: scripted(),
        live: live.clone(),
        most: most.clone(),
    };

    scratch
        .serve(
            scratch.config(false, "  max_workers: 1\n"),
            scratch.host(harness, model),
        )
        .await;

    assert_eq!(most.load(Ordering::SeqCst), 1, "one worker at a time");
    assert_eq!(live.load(Ordering::SeqCst), 0);
    let tickets = scratch.tickets();
    assert_eq!(tickets.len(), 3);
    assert!(
        tickets
            .iter()
            .all(|t| t.ended.as_ref().map(|e| e.outcome) == Some(TicketOutcome::Completed))
    );
    for thread in ["T1", "T2", "T3"] {
        let on: Vec<_> = scratch
            .replies()
            .into_iter()
            .filter(|(t, _)| t == thread)
            .collect();
        assert_eq!(on, ack_and_result(thread), "{thread}");
    }
    assert_settled(&scratch.store());
}

/// A real Pi worker in bubblewrap whose scripted model runs one `run_code`
/// and reports `ready`. On a Linux box with the pinned Pi tree:
///
/// `SOLOS_PI_ROOT=<checkout with node_modules> SOLOS_NODE=<node binary>
/// cargo test --test agent_worker -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs Linux, bwrap, node and the pinned Pi tree"]
async fn a_real_pi_worker_runs_code_in_its_ticket_directory_and_reports_ready() {
    let pi_root = PathBuf::from(std::env::var_os("SOLOS_PI_ROOT").expect("SOLOS_PI_ROOT"));
    let node = PathBuf::from(std::env::var_os("SOLOS_NODE").expect("SOLOS_NODE"));
    let scratch = Scratch::new("pi");
    scratch.ask("t-ask", "T1", "Say hello from the sandbox");
    let real = Arc::new(RealWorker {
        bodies: Mutex::new(Vec::new()),
    });
    let host = PiHost {
        harness: PiHarness,
        provider: real.clone(),
        pi_root,
        node,
        browser: solos_swarm::browse::Launcher::Lightpanda,
    };

    let summary = scratch
        .serve(scratch.config(false, "  run_code: {}\n"), host)
        .await;

    eprintln!("summary: {}", json!(summary));
    assert_eq!(scratch.replies(), ack_and_result("T1"));
    assert_eq!(scratch.sent()[1].text, "Done: hello from the sandbox");
    let tickets = scratch.tickets();
    let ticket = &tickets[0];
    assert_eq!(
        std::fs::read_to_string(scratch.work_dir(ticket).join("proof.txt")).unwrap(),
        "hello from the sandbox\n"
    );
    let bodies = real.bodies.lock().unwrap().clone();
    let mut tools: Vec<&str> = bodies[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    tools.sort_unstable();
    assert_eq!(
        tools,
        [
            "fetch_url",
            "job_start",
            "job_stop",
            "job_wait",
            "run_code",
            "worker_handback",
            "worker_report"
        ]
    );
    assert_eq!(bodies[0]["messages"][0]["role"], "system");
    assert!(text(&bodies[0]["messages"][0]).starts_with("You are a worker"));
    let dir = scratch.state().join(TICKETS_DIR).join(ticket.id.as_str());
    let transcript = dir.join(format!("{}.rpc.jsonl", ticket.assignments[0].id));
    assert!(
        transcript.is_file(),
        "the session wrote {}",
        transcript.display()
    );
    eprintln!("model calls: {}", bodies.len());
    eprintln!("transcript: {}", transcript.display());
    assert_settled(&scratch.store());
}

/// The real worker's model: `run_code` first, then `worker_report`, then
/// the end of the turn, by how many tool results the conversation holds.
struct RealWorker {
    bodies: Mutex<Vec<Value>>,
}

impl Provider for RealWorker {
    fn dispatch(&self, body: Value) -> Dispatch {
        let results = body["messages"]
            .as_array()
            .map_or(0, |m| m.iter().filter(|m| m["role"] == "tool").count());
        self.bodies.lock().unwrap().push(body);
        let message = match results {
            0 => json!({"role":"assistant","content":null,"tool_calls":[call(
                "run_code",
                &json!({"code":"echo hello from the sandbox > proof.txt && cat proof.txt"}),
            )]}),
            1 => json!({"role":"assistant","content":null,"tool_calls":[call(
                "worker_report",
                &json!({"status":"ready","text":"Done: hello from the sandbox"}),
            )]}),
            _ => json!({"role":"assistant","content":"Reported."}),
        };
        let finish = if results < 2 { "tool_calls" } else { "stop" };
        Box::pin(async move {
            Ok(
                json!({"id":format!("real-{results}"),"created":1,"model":"deepseek-flash",
                      "choices":[{"index":0,"message":message,"finish_reason":finish}],
                      "usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":0,"completion_tokens":50}}),
            )
        })
    }
}

#[tokio::test]
async fn serve_refuses_a_vault_where_a_worker_sandbox_can_read_it() {
    let scratch = Scratch::new("vault-view");
    let pi_root = scratch.root.join("pi");
    let node_root = scratch.root.join("runtime");
    let node = node_root.join("bin").join("node");
    std::fs::create_dir_all(&pi_root).unwrap();
    std::fs::create_dir_all(node.parent().unwrap()).unwrap();
    std::fs::write(&node, "").unwrap();
    std::fs::write(scratch.root.join("triggers.jsonl"), "").unwrap();
    let tickets = scratch.state().join(TICKETS_DIR);
    let elsewhere = scratch.root.join("keys");
    for (place, dir, refused) in [
        ("every Ticket's directory", &tickets, true),
        ("Pi's checkout", &pi_root, true),
        ("node's install", &node_root, true),
        ("where no sandbox binds", &elsewhere, false),
    ] {
        let vault = VaultConfig {
            path: dir.join("vault.enc"),
            key_file: dir.join("vault.key"),
        };
        if !refused {
            Vault::init(&vault, &SandboxView::new([])).unwrap();
        }
        let mut config = scratch.config(false, "");
        config.vault = Some(vault);
        let host = PiHost {
            harness: scripted(),
            provider: Model::new(Box::new(|_| Vec::new())),
            pi_root: pi_root.clone(),
            node: node.clone(),
            browser: solos_swarm::browse::Launcher::Lightpanda,
        };
        let served = tokio::time::timeout(
            LIMIT,
            serve_with(config, &scratch.state(), true, Some(host)),
        )
        .await
        .expect("serve did not go idle");
        match served {
            Ok(_) => assert!(!refused, "a vault in {place} was opened"),
            Err(err) => {
                let why = format!("{err:#}");
                assert!(refused, "a vault {place} was refused: {why}");
                assert!(why.contains("which a sandbox can read"), "{place}: {why}");
            }
        }
    }
}
