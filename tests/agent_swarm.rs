//! `agent serve` with swarm Tickets. The Pi Router's scripted model accepts
//! a request as a swarm; the swarm's agents play with `ScriptedHarness`
//! against the swarm's own broker, whose provider answers like
//! `smoke::Scripted`.

use serde_json::{Value, json};
use solos_agent_core::model::{
    MessageKind, OutboundMessage, Plan, ReportStatus, Ticket, TicketOutcome,
};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::swarm_executor::SWARM_DIR;
use solos_swarm::agent::{AgentConfig, PiHost, STORE_FILE, Summary, TICKETS_DIR, serve_with};
use solos_swarm::gateway::{Dispatch, Provider};
use solos_swarm::harness::AgentHarness;
use solos_swarm::harness::scripted::{Call, Script, ScriptedHarness};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(120);
const TEMPLATE: &str = include_str!("fixtures/goal-template.yaml");
const PRICES: &str = "{input_microusd_per_million: 300000, output_microusd_per_million: 1200000}";

/// The Router's model and the swarm's. A Router prompt gets a
/// `router_decide` per card that accepts it as a swarm of `agents`; any
/// other prompt, a swarm agent's, gets text. Every call costs 90 microusd.
struct Model {
    agents: u16,
    bodies: Mutex<Vec<Value>>,
}

impl Model {
    fn new(agents: u16) -> Arc<Self> {
        Arc::new(Self {
            agents,
            bodies: Mutex::new(Vec::new()),
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
        let (message, finish) = if calls.is_empty() {
            (json!({"role":"assistant","content":"ok"}), "stop")
        } else {
            (
                json!({"role":"assistant","content":null,"tool_calls":calls}),
                "tool_calls",
            )
        };
        Box::pin(async move {
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
    assert_settled(&scratch.store());
}
