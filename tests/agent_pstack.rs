use serde_json::{Value, json};
use solos_agent_core::model::{Ticket, TicketOutcome};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::pi_executor::WORKER_INSTRUCTIONS;
use solos_swarm::agent::pstack::ADAPTER;
use solos_swarm::agent::{AgentConfig, PiHost, STORE_FILE, Summary, serve_with};
use solos_swarm::gateway::{Dispatch, Provider};
use solos_swarm::harness::scripted::{Call, ScriptedHarness, ScriptedSession};
use solos_swarm::harness::{AgentHarness, Observation, Session, SessionSpec};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

const LIMIT: Duration = Duration::from_secs(120);
const WORKERS: &str = "executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}
  max_output_tokens: 2048
";
const PLAYBOOK: &str = "/pstack/skills/poteto-mode/playbooks/feature.md";
const READY: &str = "agent-core forbids I/O so the Engine stays a pure function.";

fn checkout() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn call(name: &str, arguments: Value) -> Value {
    json!({"id":format!("call-{name}"),"type":"function",
           "function":{"name":name,"arguments":arguments.to_string()}})
}

struct Model(Vec<Value>);

impl Provider for Model {
    fn dispatch(&self, _: Value) -> Dispatch {
        let message = json!({"role":"assistant","content":null,"tool_calls":self.0});
        Box::pin(async move {
            Ok(json!({"id":"model-1","created":1,"model":"deepseek-flash",
                      "choices":[{"index":0,"message":message,"finish_reason":"tool_calls"}],
                      "usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":0,"completion_tokens":50}}))
        })
    }
}

#[derive(Default)]
struct Seen {
    grants: Vec<String>,
    system_prompt: String,
    first_prompt: String,
    reads: Vec<(&'static str, u16, Value)>,
}

struct Recording {
    inner: ScriptedHarness,
    seen: Arc<Mutex<Seen>>,
}

struct RecordedSession {
    inner: ScriptedSession,
    spec: SessionSpec,
    seen: Arc<Mutex<Seen>>,
    first: bool,
}

impl AgentHarness for Recording {
    type Session = RecordedSession;
    async fn start(
        &self,
        spec: &SessionSpec,
        events: mpsc::Sender<Observation>,
    ) -> anyhow::Result<RecordedSession> {
        Ok(RecordedSession {
            inner: self.inner.start(spec, events).await?,
            spec: spec.clone(),
            seen: self.seen.clone(),
            first: true,
        })
    }
}

impl RecordedSession {
    async fn post(&self, path: &str, body: Value) -> anyhow::Result<(u16, Value)> {
        let client = reqwest::Client::builder()
            .unix_socket(self.spec.broker_socket.clone())
            .build()?;
        let response = client
            .post(format!("http://broker/{path}"))
            .bearer_auth(&self.spec.token)
            .json(&body)
            .send()
            .await?;
        let status = response.status().as_u16();
        Ok((status, response.json().await.unwrap_or(Value::Null)))
    }
}

impl Session for RecordedSession {
    fn id(&self) -> &str {
        self.inner.id()
    }
    async fn prompt(&mut self, text: &str) -> anyhow::Result<()> {
        if std::mem::take(&mut self.first) {
            let (_, session) = self.post("session", json!({})).await?;
            let mut reads = Vec::new();
            for path in [
                PLAYBOOK,
                "/pstack/../../etc/passwd",
                "/etc/passwd",
                "/work/x",
            ] {
                let (status, reply) = self.post("tool/read", json!({"path": path})).await?;
                reads.push((path, status, reply));
            }
            *self.seen.lock().unwrap() = Seen {
                grants: self.spec.grants.clone(),
                system_prompt: session["system_prompt"].as_str().unwrap_or("").to_owned(),
                first_prompt: text.to_owned(),
                reads,
            };
        }
        self.inner.prompt(text).await
    }
    async fn stop(&mut self) -> anyhow::Result<bool> {
        self.inner.stop().await
    }
    async fn join(self) -> usize {
        self.inner.join().await
    }
}

fn recording() -> (Recording, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let inner = ScriptedHarness::new(BTreeMap::new()).every_turn(vec![Call::Replied]);
    (
        Recording {
            inner,
            seen: seen.clone(),
        },
        seen,
    )
}

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agent-pstack-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("node"), "").unwrap();
        let ask = json!({"id":"t-ask","source":"slack","at_ms":1_793_613_600_000u64,
            "actor":{"id":"alice","kind":"person"},"addressed":"mention",
            "text":"Explain why agent-core forbids I/O","threads":["T1"],
            "reply_to":[{"source":"slack","address":{"thread_ts":"T1"}}]});
        std::fs::write(root.join("triggers.jsonl"), format!("{ask}\n")).unwrap();
        Self { root }
    }
    fn config(&self, pstack: &str) -> AgentConfig {
        let yaml = format!(
            "policy:\n  people: [slack:alice]\n\
             sources:\n  - {{kind: jsonl, path: triggers.jsonl}}\n\
             channels:\n  - {{kind: jsonl, source: slack, path: outbox.jsonl}}\n\
             router: {{kind: scripted}}\n{WORKERS}{pstack}\
             budgets: {{session_microusd: 500000}}\n"
        );
        let path = self.root.join("agent.yaml");
        std::fs::write(&path, yaml).unwrap();
        AgentConfig::load(&path).unwrap()
    }
    async fn serve<H>(
        &self,
        config: AgentConfig,
        harness: H,
        model: Model,
        pi_root: &Path,
    ) -> anyhow::Result<Summary>
    where
        H: AgentHarness + Send + Sync + 'static,
        H::Session: Send + 'static,
    {
        let host = PiHost {
            harness,
            provider: Arc::new(model),
            pi_root: pi_root.to_owned(),
            node: self.root.join("node"),
            browser: solos_swarm::browse::Launcher::Lightpanda,
        };
        let state = self.state();
        let serving = serve_with(config, &state, true, Some(host));
        tokio::time::timeout(LIMIT, serving)
            .await
            .expect("serve did not go idle")
    }
    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    fn ticket(&self) -> Ticket {
        let store = SqliteStore::open(self.state().join(STORE_FILE)).unwrap();
        let tickets = store.tickets().unwrap();
        let [ticket] = &tickets[..] else {
            panic!("{tickets:?}")
        };
        ticket.clone()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stripped(lines: &Value) -> String {
    lines
        .as_str()
        .unwrap_or_else(|| panic!("{lines}"))
        .split_inclusive('\n')
        .map(|line| line.split_once('\t').unwrap().1)
        .collect()
}

#[tokio::test]
async fn a_worker_enters_poteto_mode_carries_the_adapter_and_reads_pstack_through_the_broker() {
    let scratch = Scratch::new("enter");
    let (harness, seen) = recording();
    let model = Model(vec![
        call("read", json!({"path": PLAYBOOK})),
        call("worker_report", json!({"status":"ready","text":READY})),
    ]);

    scratch
        .serve(scratch.config("pstack: {}\n"), harness, model, checkout())
        .await
        .unwrap();

    let ticket = scratch.ticket();
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed),
        "the scripted read was served: a refused one ends the session"
    );
    assert_eq!(ticket.latest_report().unwrap().text, READY);
    let seen = seen.lock().unwrap();
    assert!(
        seen.first_prompt.starts_with(
            "<skill name=\"poteto-mode\" location=\"/pstack/skills/poteto-mode/SKILL.md\">\n\
             References are relative to /pstack/skills/poteto-mode.\n\n# Poteto mode"
        ),
        "{}",
        seen.first_prompt
    );
    assert!(
        seen.first_prompt
            .contains(&format!("</skill>\n\nTicket {}: ", ticket.id)),
        "{}",
        seen.first_prompt
    );
    for told in [
        WORKER_INSTRUCTIONS,
        "pstack. You work the pstack way. pstack is Lauren Tan's set of agent skills, at /pstack \
         (pstack 0.15.36, read-only).",
        ADAPTER.trim(),
        "\njudgment and prose: deepseek-flash\n",
    ] {
        assert!(
            seen.system_prompt.contains(told),
            "{told:?} is not in {}",
            seen.system_prompt
        );
    }
    assert!(
        seen.grants.iter().any(|tool| tool == "read"),
        "{:?}",
        seen.grants
    );

    let [(_, status, playbook), refused @ ..] = &seen.reads[..] else {
        panic!("{:?}", seen.reads)
    };
    assert_eq!(*status, 200, "{playbook}");
    let file = std::fs::read_to_string(
        checkout().join("vendor/pstack/skills/poteto-mode/playbooks/feature.md"),
    )
    .unwrap();
    assert_eq!(stripped(&playbook["lines"]), file);
    assert_eq!(refused.len(), 3);
    for (path, status, reply) in refused {
        assert!((400..500).contains(status), "{path}: {status} {reply}");
    }
}

#[tokio::test]
async fn serve_refuses_a_vendored_tree_that_differs_from_its_manifest_and_names_the_file() {
    let scratch = Scratch::new("tampered");
    let pi_root = scratch.root.join("pi");
    copy_tree(
        &checkout().join("vendor/pstack"),
        &pi_root.join("vendor/pstack"),
    );
    let skill = pi_root.join("vendor/pstack/skills/poteto-mode/SKILL.md");
    let mut bytes = std::fs::read(&skill).unwrap();
    bytes[40] ^= 1;
    std::fs::write(&skill, bytes).unwrap();
    let (harness, _) = recording();

    let refused = scratch
        .serve(
            scratch.config("pstack: {}\n"),
            harness,
            Model(Vec::new()),
            &pi_root,
        )
        .await
        .expect_err("serve started on a changed tree");

    let why = format!("{refused:#}");
    assert!(
        why.contains("skills/poteto-mode/SKILL.md differs from MANIFEST"),
        "{why}"
    );
}

#[tokio::test]
async fn without_the_pstack_section_a_worker_starts_on_its_ticket_and_cannot_read() {
    let scratch = Scratch::new("off");
    let (harness, seen) = recording();
    let model = Model(vec![call(
        "worker_report",
        json!({"status":"ready","text":READY}),
    )]);

    scratch
        .serve(scratch.config(""), harness, model, checkout())
        .await
        .unwrap();

    let ticket = scratch.ticket();
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    let seen = seen.lock().unwrap();
    assert!(
        seen.first_prompt
            .starts_with(&format!("Ticket {}: ", ticket.id)),
        "{}",
        seen.first_prompt
    );
    assert!(
        !seen.grants.iter().any(|tool| tool == "read"),
        "{:?}",
        seen.grants
    );
    assert!(
        !seen.system_prompt.contains("pstack"),
        "{}",
        seen.system_prompt
    );
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
