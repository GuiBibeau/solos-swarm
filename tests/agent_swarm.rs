//! `agent serve` with swarm Tickets. The Pi Router's scripted model accepts
//! a request as a swarm; the swarm's agents play with `ScriptedHarness`
//! against the swarm's own broker, whose provider answers like
//! `smoke::Scripted`. The ignored test runs real `PiHarness` sessions (Pi,
//! node and bwrap) and needs `SOLOS_PI_ROOT` and `SOLOS_NODE`.

use serde_json::{Value, json};
use solos_agent_core::model::{
    AssignmentState, MessageKind, OutboundMessage, Plan, ReportStatus, Ticket, TicketOutcome,
};
use solos_agent_store::{SpendState, SqliteStore};
use solos_swarm::agent::swarm_executor::SWARM_DIR;
use solos_swarm::agent::{
    AgentConfig, HOST_RESTARTED, PiHost, STORE_FILE, Summary, TICKETS_DIR, serve_with,
};
use solos_swarm::gateway::{Dispatch, Provider};
use solos_swarm::harness::scripted::{Call, Script, ScriptedHarness};
use solos_swarm::harness::{AgentHarness, PiHarness};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

const LIMIT: Duration = Duration::from_secs(120);
const TEMPLATE: &str = include_str!("fixtures/goal-template.yaml");
const PRICES: &str = "{input_microusd_per_million: 300000, output_microusd_per_million: 1200000}";

/// The Router's model and the swarm's. A Router prompt gets a
/// `router_decide` per card that accepts it as a swarm of `agents`; any
/// other prompt, a swarm agent's, gets text. Every call costs 90 microusd.
struct Model {
    agents: u16,
    bodies: Mutex<Vec<Value>>,
    /// Swarm calls never answer, and tell this.
    hang: Option<Arc<Notify>>,
}

impl Model {
    fn new(agents: u16) -> Arc<Self> {
        Arc::new(Self {
            agents,
            bodies: Mutex::new(Vec::new()),
            hang: None,
        })
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

/// `(eval, first quoted line)` of each card in a Router prompt.
fn cards(prompt: &str) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    for line in prompt.lines() {
        if let Some(rest) = line.strip_prefix("eval ") {
            let eval = rest.split(" | ").next().unwrap_or_default();
            found.push((eval.into(), String::new()));
        } else if let (Some(text), Some(card)) = (line.strip_prefix("> "), found.last_mut())
            && card.1.is_empty()
        {
            card.1 = text.chars().take(60).collect();
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
        let last = body["messages"]
            .as_array()
            .and_then(|m| m.last())
            .cloned()
            .unwrap_or(Value::Null);
        let prompt = text(&last);
        let calls: Vec<Value> = if last["role"] == "user" && prompt.contains(" new candidate") {
            cards(&prompt)
                .into_iter()
                .map(|(eval, title)| {
                    call(
                        "router_decide",
                        &json!({"eval":eval,"decision":"accept","reason":"wide research",
                                "title":title,"scope":format!("Survey: {title}"),
                                "plan":"swarm","agents":self.agents}),
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        let hang = self.hang.clone().filter(|_| calls.is_empty());
        let (message, finish) = if calls.is_empty() {
            (json!({"role":"assistant","content":"ok"}), "stop")
        } else {
            (
                json!({"role":"assistant","content":null,"tool_calls":calls}),
                "tool_calls",
            )
        };
        Box::pin(async move {
            if let Some(hang) = hang {
                hang.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(
                json!({"id":format!("model-{n}"),"created":1,"model":"deepseek-flash",
                      "choices":[{"index":0,"message":message,"finish_reason":finish}],
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
            "agent-swarm-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::create_dir_all(root.join("extensions")).unwrap();
        // What `run_goal` digests into each swarm's manifest.
        std::fs::write(root.join("extensions/broker.ts"), "// stand-in").unwrap();
        std::fs::write(root.join("package-lock.json"), "{}").unwrap();
        std::fs::write(root.join("node"), "").unwrap();
        std::fs::write(root.join("goal.yaml"), TEMPLATE).unwrap();
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
    /// A Pi Router and Pi workers that run swarms from `goal.yaml`, with
    /// `swarm` added to the swarm settings.
    fn config(&self, swarm: &str) -> AgentConfig {
        let yaml = format!(
            "policy:\n  people: [slack:alice]\n\
             sources:\n  - {{kind: jsonl, path: triggers.jsonl}}\n\
             channels:\n  - {{kind: jsonl, source: slack, path: outbox.jsonl}}\n\
             router:\n  kind: pi\n  provider: deepseek\n  model: deepseek-flash\n  prices: {PRICES}\n\
             executor:\n  kind: pi\n  provider: deepseek\n  model: deepseek-flash\n  prices: {PRICES}\n\
             \x20 swarm:\n    templates: {{research: goal.yaml}}\n    default_template: research\n{swarm}\
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
    fn run_dir(&self, ticket: &Ticket, n: usize) -> PathBuf {
        self.state()
            .join(TICKETS_DIR)
            .join(ticket.id.as_str())
            .join(SWARM_DIR)
            .join(ticket.assignments[n].id.as_str())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("SOLOS_KEEP_SCRATCH").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn record(id: &str) -> Value {
    json!({"papers":[{"id":format!("{id}-paper"),"severity":"useful"}]})
}

/// Swarm agents that each file a record, the coordinator then the artifact;
/// every other session, the Router's, runs the tool calls its model asks for.
fn swarm(agents: usize) -> ScriptedHarness {
    let mut scripts: BTreeMap<String, Script> = BTreeMap::new();
    for n in 0..agents {
        let id = format!("agent-{n:02}");
        let mut turns = vec![vec![Call::ClaimTask, Call::SubmitFinding(record(&id))]];
        if n == 0 {
            turns.push(vec![Call::ReadFindings, Call::SubmitArtifact]);
        }
        scripts.insert(id, turns);
    }
    ScriptedHarness::new(scripts).every_turn(vec![Call::Replied])
}

fn read(path: PathBuf) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
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

#[tokio::test]
async fn a_swarm_ticket_runs_its_goal_and_answers_once_with_the_artifact() {
    let scratch = Scratch::new("e2e");
    scratch.ask(
        "t-ask",
        "T1",
        "Survey every paper that cites the swarm paper",
    );

    let summary = scratch
        .serve(scratch.config(""), scratch.host(swarm(2), Model::new(2)))
        .await;

    assert_eq!((summary.triggers_received, summary.effects_pending), (1, 0));
    let thread = "T1".to_owned();
    assert_eq!(
        scratch.replies(),
        [
            (thread.clone(), MessageKind::Acknowledgement),
            (thread, MessageKind::Result)
        ]
    );
    let tickets = scratch.tickets();
    let [ticket] = &tickets[..] else {
        panic!("{tickets:?}")
    };
    assert_eq!(
        ticket.plan,
        Plan::Swarm {
            agents: 2,
            goal_template: None
        }
    );
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    let run = scratch.run_dir(ticket, 0).canonicalize().unwrap();
    let artifact = run.join("artifacts/final.json");
    let result = &scratch.sent()[1].text;
    assert!(
        result.contains("filed 2 findings (part-0, part-1)")
            && result.contains(&artifact.display().to_string()),
        "{result}"
    );
    let papers = read(artifact)["papers"].as_array().cloned().unwrap();
    assert_eq!(papers.len(), 2);
    assert_eq!(read(run.join("cleanup.json"))["verdict"], "PASS");
    let config = std::fs::read_to_string(run.join("config.yaml")).unwrap();
    assert!(
        config.contains("intent: \"Survey every paper that cites the swarm paper\"\n"),
        "{config}"
    );
    let ready = ticket.latest_report().unwrap();
    assert_eq!(
        (ready.status, ready.spent_microusd),
        (ReportStatus::Ready, 270)
    );
    let store = scratch.store();
    let account = format!("ticket:{}", ticket.id);
    let rows: Vec<_> = store
        .spend(&account)
        .unwrap()
        .into_iter()
        .map(|r| (r.state, r.cost_microusd, r.request_id))
        .collect();
    assert_eq!(
        rows,
        [(
            SpendState::Settled,
            270,
            Some(format!("swarm:{}", ticket.assignments[0].id))
        )],
        "the Ticket's account holds the run's spend"
    );
    assert_eq!(store.committed(&account, 0).unwrap(), ready.spent_microusd);
    assert_settled(&store);
}

/// Swarm agents whose coordinator submits an artifact before it read any
/// finding, so its session fails and the swarm stops after some spend.
fn failing() -> ScriptedHarness {
    let scripts: BTreeMap<String, Script> = BTreeMap::from([
        (
            "agent-00".to_owned(),
            vec![vec![Call::ClaimTask, Call::SubmitArtifact]],
        ),
        (
            "agent-01".to_owned(),
            vec![vec![
                Call::ClaimTask,
                Call::SubmitFinding(record("agent-01")),
            ]],
        ),
    ]);
    ScriptedHarness::new(scripts).every_turn(vec![Call::Replied])
}

fn charges(store: &SqliteStore, ticket: &Ticket) -> Vec<(String, u64)> {
    store
        .spend(&format!("ticket:{}", ticket.id))
        .unwrap()
        .into_iter()
        .map(|r| (r.request_id.unwrap_or_default(), r.cost_microusd))
        .collect()
}

fn budget_line(run: &std::path::Path) -> String {
    let config = std::fs::read_to_string(run.join("config.yaml")).unwrap();
    config
        .lines()
        .find(|line| line.starts_with("budget_microusd: "))
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn the_next_assignment_of_a_swarm_ticket_gets_what_the_last_one_left() {
    let scratch = Scratch::new("again");
    scratch.ask("t-ask", "T1", "Survey the papers that cite the swarm paper");
    scratch
        .serve(scratch.config(""), scratch.host(failing(), Model::new(2)))
        .await;
    let replies = scratch.replies();
    assert_eq!(
        replies.iter().map(|(_, kind)| *kind).collect::<Vec<_>>(),
        [MessageKind::Acknowledgement, MessageKind::Attention]
    );
    assert!(
        scratch.sent()[1]
            .text
            .starts_with("The swarm stopped: agent-00 RPC failed"),
        "{}",
        scratch.sent()[1].text
    );
    let first = scratch.tickets().remove(0);
    let [(id, spent)] = &charges(&scratch.store(), &first)[..] else {
        panic!("{:?}", charges(&scratch.store(), &first))
    };
    assert_eq!(*id, format!("swarm:{}", first.assignments[0].id));
    assert!(*spent > 0, "the stopped run spent");
    assert_eq!(first.latest_report().unwrap().spent_microusd, *spent);

    scratch.ask("t-again", "T1", "Please try again");
    scratch
        .serve(scratch.config(""), scratch.host(swarm(2), Model::new(2)))
        .await;

    let ticket = scratch.tickets().remove(0);
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    assert_eq!(ticket.assignments.len(), 2);
    assert_eq!(
        budget_line(&scratch.run_dir(&ticket, 1)),
        format!("budget_microusd: {}", 500_000 - spent),
        "the second swarm's Goal has what the first left"
    );
    assert_eq!(
        scratch
            .store()
            .committed(&format!("ticket:{}", ticket.id), 0)
            .unwrap(),
        spent + 270
    );
    let kinds: Vec<_> = scratch
        .replies()
        .into_iter()
        .map(|(_, kind)| kind)
        .collect();
    assert_eq!(kinds.last(), Some(&MessageKind::Result));
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == MessageKind::Result)
            .count(),
        1
    );
    assert_settled(&scratch.store());
}

#[tokio::test]
async fn a_swarm_a_crash_cut_short_restarts_in_a_new_run_and_its_spend_is_kept() {
    let scratch = Scratch::new("crash");
    scratch.ask("t-ask", "T1", "Survey the papers that cite the swarm paper");
    let hung = Arc::new(Notify::new());
    let first = Arc::new(Model {
        agents: 2,
        bodies: Mutex::new(Vec::new()),
        hang: Some(hung.clone()),
    });
    // Its own runtime, dropped whole as a crash would: the swarm, its
    // broker and its sessions go with it.
    let crashed = {
        let (config, host, state) = (
            scratch.config(""),
            scratch.host(swarm(2), first),
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
                    panic!("the swarm never ran");
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
    let cut = scratch.tickets().remove(0);
    let old = scratch.run_dir(&cut, 0);
    let state = read(old.join("state.json"));
    let held = state["held_microusd"].as_u64().unwrap();
    assert!(held > 0, "the crash cut a call short: {state}");

    scratch
        .serve(scratch.config(""), scratch.host(swarm(2), Model::new(2)))
        .await;

    let thread = "T1".to_owned();
    assert_eq!(
        scratch.replies(),
        [
            (thread.clone(), MessageKind::Acknowledgement),
            (thread, MessageKind::Result)
        ],
        "no reply twice"
    );
    let ticket = scratch.tickets().remove(0);
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
    let new = scratch.run_dir(&ticket, 1);
    assert_ne!(old, new);
    assert!(
        old.join("events.jsonl").is_file() && !old.join("cleanup.json").exists(),
        "the run cut short stays as it was"
    );
    assert_eq!(read(new.join("cleanup.json"))["verdict"], "PASS");
    let cost = state["spent_microusd"].as_u64().unwrap() + held;
    assert_eq!(
        charges(&scratch.store(), &ticket),
        [
            (format!("swarm:{}", ticket.assignments[0].id), cost),
            (format!("swarm:{}", ticket.assignments[1].id), 270)
        ],
        "the run cut short is charged at its open holds"
    );
    assert_eq!(
        budget_line(&new),
        format!("budget_microusd: {}", 500_000 - cost)
    );
    assert_settled(&scratch.store());
}

/// The real sessions' model, read off each conversation. The Router accepts
/// every card as a swarm of two. A swarm agent claims its task and files one
/// record; the coordinator then reads the findings and submits their union.
#[derive(Default)]
struct Real {
    bodies: Mutex<Vec<Value>>,
}

/// The JSON a session got back from its latest call of `name`.
fn result_of(messages: &[Value], name: &str) -> Option<Value> {
    let id = messages
        .iter()
        .filter_map(|m| m["tool_calls"].as_array())
        .flatten()
        .filter(|call| call["function"]["name"] == name)
        .filter_map(|call| call["id"].as_str())
        .next_back()?;
    let answer = messages
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)?;
    serde_json::from_str(&text(answer)).ok()
}

fn called(messages: &[Value], name: &str) -> bool {
    messages
        .iter()
        .filter_map(|m| m["tool_calls"].as_array())
        .flatten()
        .any(|call| call["function"]["name"] == name)
}

impl Real {
    fn calls(messages: &[Value]) -> Vec<(&'static str, Value)> {
        let users: Vec<usize> = (0..messages.len())
            .filter(|&n| messages[n]["role"] == "user")
            .collect();
        let (Some(&first), Some(&latest)) = (users.first(), users.last()) else {
            return Vec::new();
        };
        let last_is_user = latest + 1 == messages.len();
        if text(&messages[first]).contains(" new candidate") {
            if !last_is_user {
                return Vec::new();
            }
            return cards(&text(&messages[latest]))
                .into_iter()
                .map(|(eval, title)| {
                    (
                        "router_decide",
                        json!({"eval":eval,"decision":"accept","reason":"wide research",
                               "title":title,"scope":format!("Survey: {title}"),
                               "plan":"swarm","agents":2}),
                    )
                })
                .collect();
        }
        let turn = &messages[latest..];
        if text(&messages[latest]).starts_with("Call read_findings") {
            if !called(turn, "read_findings") {
                return vec![("read_findings", json!({}))];
            }
            if called(turn, "submit_artifact") {
                return Vec::new();
            }
            let Some(read) = result_of(turn, "read_findings") else {
                return Vec::new();
            };
            let mut papers = Vec::new();
            for finding in read["findings"].as_array().into_iter().flatten() {
                papers.extend(
                    finding["result"]["papers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
            }
            return vec![("submit_artifact", json!({"artifact":{"papers":papers}}))];
        }
        if !called(messages, "claim_task") {
            return vec![("claim_task", json!({}))];
        }
        if called(messages, "submit_finding") {
            return Vec::new();
        }
        let Some(task) = result_of(messages, "claim_task") else {
            return Vec::new();
        };
        let id = task["id"].as_str().unwrap_or_default().to_owned();
        vec![(
            "submit_finding",
            json!({"task_id":id,"result":{"papers":[{"id":format!("{id}-paper"),"severity":"useful"}]}}),
        )]
    }
}

impl Provider for Real {
    fn dispatch(&self, body: Value) -> Dispatch {
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let n = {
            let mut bodies = self.bodies.lock().unwrap();
            bodies.push(body);
            bodies.len()
        };
        let calls: Vec<Value> = Real::calls(&messages)
            .into_iter()
            .map(|(name, arguments)| {
                let mut call = call(name, &arguments);
                call["id"] = json!(format!("call-{name}-{n}"));
                call
            })
            .collect();
        let (message, finish) = if calls.is_empty() {
            (json!({"role":"assistant","content":"Done."}), "stop")
        } else {
            (
                json!({"role":"assistant","content":null,"tool_calls":calls}),
                "tool_calls",
            )
        };
        Box::pin(async move {
            Ok(
                json!({"id":format!("real-{n}"),"created":1,"model":"deepseek-flash",
                      "choices":[{"index":0,"message":message,"finish_reason":finish}],
                      "usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":0,"completion_tokens":50}}),
            )
        })
    }
}

/// A swarm Ticket whose two agents are real Pi sessions in bubblewrap, and
/// whose Router is one too, all on a scripted model. On a Linux box with the
/// pinned Pi tree:
///
/// `SOLOS_PI_ROOT=<checkout with node_modules> SOLOS_NODE=<node binary>
/// cargo test --test agent_swarm -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs Linux, bwrap, node and the pinned Pi tree"]
async fn real_pi_swarm_agents_run_a_swarm_ticket_in_bubblewrap() {
    let pi_root = PathBuf::from(std::env::var_os("SOLOS_PI_ROOT").expect("SOLOS_PI_ROOT"));
    let node = PathBuf::from(std::env::var_os("SOLOS_NODE").expect("SOLOS_NODE"));
    let scratch = Scratch::new("pi");
    scratch.ask("t-ask", "T1", "Survey the papers that cite the swarm paper");
    let real = Arc::new(Real::default());
    let host = PiHost {
        harness: PiHarness,
        provider: real.clone(),
        pi_root,
        node,
    };

    let summary = scratch.serve(scratch.config(""), host).await;

    eprintln!("summary: {}", json!(summary));
    let thread = "T1".to_owned();
    assert_eq!(
        scratch.replies(),
        [
            (thread.clone(), MessageKind::Acknowledgement),
            (thread, MessageKind::Result)
        ]
    );
    let ticket = scratch.tickets().remove(0);
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    let run = scratch.run_dir(&ticket, 0);
    let papers = read(run.join("artifacts/final.json"))["papers"]
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(papers.len(), 2, "{papers:?}");
    assert_eq!(read(run.join("cleanup.json"))["verdict"], "PASS");
    assert_eq!(
        read(run.join("runtime-report.json"))["runtime_verdict"],
        "PASS"
    );
    for agent in ["agent-00", "agent-01"] {
        let transcript = run.join(format!("{agent}.rpc.jsonl"));
        assert!(
            transcript.is_file(),
            "{agent} wrote {}",
            transcript.display()
        );
        eprintln!("transcript: {}", transcript.display());
    }
    let ready = ticket.latest_report().unwrap();
    assert_eq!(
        charges(&scratch.store(), &ticket),
        [(
            format!("swarm:{}", ticket.assignments[0].id),
            ready.spent_microusd
        )]
    );
    let bodies = real.bodies.lock().unwrap().clone();
    let first_user = |body: &Value| {
        body["messages"]
            .as_array()
            .and_then(|m| m.iter().find(|m| m["role"] == "user"))
            .map(text)
            .unwrap_or_default()
    };
    let mut swarm_tools: Vec<&str> = bodies
        .iter()
        .find(|body| first_user(body).starts_with("You are agent-01"))
        .and_then(|body| body["tools"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    swarm_tools.sort_unstable();
    assert_eq!(
        swarm_tools,
        ["board_post", "board_read", "claim_task", "submit_finding"],
        "a worker gets the swarm's tools, not the agent's"
    );
    eprintln!(
        "model calls: {}; result: {}",
        bodies.len(),
        scratch.sent()[1].text
    );
    assert_settled(&scratch.store());
}
