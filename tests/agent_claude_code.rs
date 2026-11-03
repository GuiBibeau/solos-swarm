//! The agent broker's Anthropic Messages facade over HTTP on its Unix
//! socket, with a scripted provider: a worker's calls held and settled on
//! its Ticket's account, the pause, the budget and the contract, as
//! `/model` has them. The ignored tests run the installed Claude Code as a
//! worker through `ClaudeCodeHarness` in bwrap, in `agent serve`, here and
//! on a Ticket bound to a remote node, and need `SOLOS_CLAUDE`.

use serde_json::{Value, json};
use solos_agent_core::model::{AssignmentId, MessageKind, OutboundMessage, TicketId};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::broker::{
    self, Account, AgentBroker, BrokerParts, DecideTerms, Grant, ModelTerms, Principal,
    WorkerParts, WorkerSeat,
};
use solos_swarm::agent::scripted::{ScriptedExecutor, ScriptedRouter};
use solos_swarm::agent::{
    Adapters, AgentConfig, PiHost, RunOptions, Runtime, STORE_FILE, TICKETS_DIR, serve_with,
};
use solos_swarm::gateway::anthropic::Chat;
use solos_swarm::gateway::{Dispatch, PriceTable, Provider, Upstream};
use solos_swarm::harness::PiHarness;
use solos_swarm::remote::daemon::{AgentEndpoint, SandboxConfig, SecretsConfig};
use solos_swarm::remote::{Node, NodeConfig, tls};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MODEL: &str = "deepseek-flash";
const MAX_OUTPUT: u64 = 2_048;
const PRICES: PriceTable = PriceTable {
    input_microusd_per_million: 300_000,
    output_microusd_per_million: 1_200_000,
    cache_hit_microusd_per_million: Some(6_000),
};
/// 1,180 prompt tokens with 1,024 cache hits, and 32 completion tokens, at
/// `PRICES`: 156 at 0.3, 1,024 at 0.006 and 32 at 1.2 microusd.
const COST: u64 = 92;
const WORKER: &str = "worker-token";
const ROUTER: &str = "router-token";
const FIRST_TURN: &str = include_str!("fixtures/anthropic/claude-code-first-turn.json");

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reply {
    /// A `worker_report` call, as Claude Code names it.
    Report,
    /// A tool call whose arguments are not JSON.
    Garbled,
}

/// Answers every call with one completion, and keeps what it was sent.
struct Scripted {
    reply: Mutex<Reply>,
    bodies: Mutex<Vec<Value>>,
}

impl Scripted {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            reply: Mutex::new(Reply::Report),
            bodies: Mutex::new(Vec::new()),
        })
    }
    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

impl Provider for Scripted {
    fn dispatch(&self, body: Value) -> Dispatch {
        let n = {
            let mut bodies = self.bodies.lock().unwrap();
            bodies.push(body);
            bodies.len()
        };
        let arguments = match *self.reply.lock().unwrap() {
            Reply::Report => r#"{"status":"ready","text":"done"}"#,
            Reply::Garbled => r#"{"status":"ready","#,
        };
        Box::pin(async move {
            Ok(json!({
                "id": format!("receipt-{n}"),
                "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                    "role": "assistant", "content": "Reporting.",
                    "tool_calls": [{"id": format!("call-{n}"), "type": "function", "function": {
                        "name": "mcp__swarm__worker_report", "arguments": arguments}}],
                }}],
                "usage": {"prompt_tokens": 1180, "prompt_cache_hit_tokens": 1024, "completion_tokens": 32},
            }))
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
            "claude-code-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        Self { root }
    }
    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    fn store(&self) -> SqliteStore {
        SqliteStore::open(self.state().join(STORE_FILE)).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("SOLOS_KEEP_SCRATCH").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn terms() -> ModelTerms {
    ModelTerms {
        upstream: Upstream::deepseek(MODEL),
        prices: PRICES,
        max_input_bytes: 65_536,
        max_output_tokens: MAX_OUTPUT,
        call_limit: None,
    }
}

/// A broker serving on the state directory's socket with a worker on
/// `tk_1`'s account and a Router, and a client for it.
struct Served {
    /// Never runs: it only holds the end of the broker's fact channel.
    _runtime: Runtime,
    server: tokio::task::JoinHandle<()>,
    client: reqwest::Client,
    provider: Arc<Scripted>,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn serve(scratch: &Scratch, budget_microusd: u64) -> Served {
    let provider = Scripted::new();
    let adapters = Adapters {
        sources: Vec::new(),
        channels: BTreeMap::new(),
        router: Arc::new(ScriptedRouter::new(500_000)),
        executor: Arc::new(ScriptedExecutor),
    };
    let runtime = Runtime::new(
        scratch.store(),
        Default::default(),
        adapters,
        RunOptions::default(),
    );
    let facts = runtime.facts();
    let broker = AgentBroker::new(BrokerParts {
        dir: scratch.state(),
        store: scratch.store(),
        provider: provider.clone(),
        facts,
        decide: DecideTerms {
            default_budget_microusd: 500_000,
            max_budget_microusd: None,
            swarm: None,
        },
        workers: Some(WorkerParts {
            terms: terms(),
            code: None,
        }),
    })
    .unwrap();
    let seat = WorkerSeat::new(
        TicketId::new("tk_1"),
        AssignmentId::new("as_1"),
        scratch.state(),
        budget_microusd,
    );
    broker
        .admit_worker(WORKER.into(), seat, "You are a worker.".into())
        .unwrap();
    let router = Grant {
        principal: Principal::Router,
        account: Account::Router {
            daily_microusd: 1_000_000,
        },
        terms: terms(),
        system_prompt: None,
    };
    broker.admit(ROUTER.into(), router).unwrap();
    let (listener, socket) = broker::bind(&scratch.state()).unwrap();
    let app = broker.router().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::builder()
        .unix_socket(socket)
        .build()
        .unwrap();
    Served {
        _runtime: runtime,
        server,
        client,
        provider,
    }
}

/// Claude Code's first worker turn, as recorded, on the broker's model.
fn asks(stream: bool) -> Value {
    let mut body: Value = serde_json::from_str(FIRST_TURN).unwrap();
    body["model"] = json!(MODEL);
    body["stream"] = json!(stream);
    body
}

impl Served {
    async fn messages(&self, key: Option<&str>, body: &Value) -> reqwest::Response {
        let mut request = self
            .client
            .post("http://broker/v1/messages?beta=true")
            .header("anthropic-version", "2023-06-01")
            .json(body);
        if let Some(key) = key {
            request = request.header("x-api-key", key);
        }
        request.send().await.unwrap()
    }
    async fn json(&self, key: Option<&str>, body: &Value) -> (u16, Value) {
        let response = self.messages(key, body).await;
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }
}

fn hold() -> u64 {
    let upstream = Chat::read(&asks(false))
        .unwrap()
        .upstream(MODEL, MAX_OUTPUT);
    let bytes = serde_json::to_vec(&upstream).unwrap().len();
    PRICES.request_hold(bytes, MAX_OUTPUT)
}

/// The events of a stream, by name, with their data.
fn events(stream: &str) -> Vec<(String, Value)> {
    stream
        .trim_end()
        .split("\n\n")
        .map(|frame| {
            let (event, data) = frame.split_once('\n').unwrap();
            let data = serde_json::from_str(data.strip_prefix("data: ").unwrap()).unwrap();
            (event.strip_prefix("event: ").unwrap().to_owned(), data)
        })
        .collect()
}

#[tokio::test]
async fn a_messages_call_is_held_and_settled_on_the_tickets_account() {
    let scratch = Scratch::new("settle");
    let served = serve(&scratch, 500_000);
    let account = "ticket:tk_1";

    let (status, message) = served.json(Some(WORKER), &asks(false)).await;
    assert_eq!(status, 200, "{message}");
    assert_eq!(
        message["content"],
        json!([
            {"type": "text", "text": "Reporting."},
            {"type": "tool_use", "id": "call-1", "name": "mcp__swarm__worker_report",
             "input": {"status": "ready", "text": "done"}},
        ])
    );
    assert_eq!(
        (&message["stop_reason"], &message["model"]),
        (&json!("tool_use"), &json!(MODEL))
    );
    assert_eq!(
        message["usage"],
        json!({"input_tokens": 156, "cache_creation_input_tokens": 0,
               "cache_read_input_tokens": 1024, "output_tokens": 32})
    );
    assert_eq!(message["swarm_cost_microusd"], COST);
    assert_eq!(scratch.store().committed(account, 0).unwrap(), COST);
    assert_eq!(scratch.store().committed("router", 0).unwrap(), 0);

    let sent = &served.provider.bodies()[0];
    assert_eq!(sent["model"], MODEL);
    assert_eq!(sent["max_tokens"], MAX_OUTPUT, "below Claude Code's 32,000");
    assert_eq!(
        (&sent["stream"], &sent["temperature"]),
        (&json!(false), &json!(0))
    );
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(
        sent["tools"][0]["function"]["name"],
        "mcp__swarm__worker_report"
    );

    let response = served.messages(Some(WORKER), &asks(true)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "text/event-stream",
        "a streaming client gets events"
    );
    assert_eq!(
        scratch.store().committed(account, 0).unwrap(),
        2 * COST,
        "settled before the stream starts"
    );
    let stream = events(&response.text().await.unwrap());
    let names: Vec<&str> = stream.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names.first(), Some(&"message_start"));
    assert_eq!(names.last(), Some(&"message_stop"));
    assert_eq!(
        stream[5].1["delta"],
        json!({"type": "input_json_delta", "partial_json": r#"{"status":"ready","text":"done"}"#})
    );

    let bearer = served
        .client
        .post("http://broker/v1/messages")
        .bearer_auth(WORKER)
        .json(&asks(false))
        .send()
        .await
        .unwrap();
    assert_eq!(bearer.status(), 200, "a bearer token works too");
    assert_eq!(scratch.store().committed(account, 0).unwrap(), 3 * COST);
}

#[tokio::test]
async fn a_paused_broker_answers_503_with_when_to_ask_again_and_holds_nothing() {
    let scratch = Scratch::new("pause");
    let served = serve(&scratch, 500_000);
    std::fs::write(scratch.state().join("PAUSE"), "").unwrap();

    let response = served.messages(Some(WORKER), &asks(true)).await;
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers()["retry-after"], "60");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "overloaded_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("paused"),
        "{body}"
    );
    assert_eq!(
        (&body["paused"], &body["retry_after_s"]),
        (&json!(true), &json!(60))
    );
    assert!(served.provider.bodies().is_empty());
    assert_eq!(scratch.store().committed("ticket:tk_1", 0).unwrap(), 0);

    std::fs::remove_file(scratch.state().join("PAUSE")).unwrap();
    let (status, _) = served.json(Some(WORKER), &asks(false)).await;
    assert_eq!(status, 200, "the pause lifted");
}

#[tokio::test]
async fn past_its_budget_a_call_is_refused_and_never_reaches_the_provider() {
    let scratch = Scratch::new("budget");
    // One call fits; a second hold on top of the first call's cost does not.
    let budget = hold() + COST - 1;
    let served = serve(&scratch, budget);

    assert_eq!(served.json(Some(WORKER), &asks(false)).await.0, 200);
    let (status, refused) = served.json(Some(WORKER), &asks(true)).await;
    assert_eq!(status, 403, "{refused}");
    assert_eq!(refused["error"]["type"], "permission_error");
    let why = refused["error"]["message"].as_str().unwrap();
    assert!(why.contains("tk_1's budget is reached"), "{why}");
    assert!(why.contains(&format!("{COST} of {budget}")), "{why}");
    assert_eq!(served.provider.bodies().len(), 1);
    assert_eq!(scratch.store().committed("ticket:tk_1", 0).unwrap(), COST);
}

#[tokio::test]
async fn what_the_contract_refuses_is_refused_as_a_messages_client_reads_it() {
    let scratch = Scratch::new("contract");
    let served = serve(&scratch, 500_000);
    let refused = |status: u16, kind: &str, why: &str, answer: (u16, Value)| {
        assert_eq!(answer.0, status, "{why}: {}", answer.1);
        assert_eq!(answer.1["error"]["type"], kind, "{why}");
        let message = answer.1["error"]["message"].as_str().unwrap().to_owned();
        assert!(message.contains(why), "{why}: {message}");
    };

    refused(
        403,
        "permission_error",
        "missing identity",
        served.json(None, &asks(false)).await,
    );
    refused(
        403,
        "permission_error",
        "invalid identity",
        served.json(Some("nobody"), &asks(false)).await,
    );
    let mut other = asks(false);
    other["model"] = json!("claude-fable-5");
    refused(
        403,
        "permission_error",
        "outside contract",
        served.json(Some(WORKER), &other).await,
    );
    let mut bash = asks(false);
    bash["tools"][0]["name"] = json!("Bash");
    refused(
        403,
        "permission_error",
        "unadmitted provider tool",
        served.json(Some(WORKER), &bash).await,
    );
    refused(
        403,
        "permission_error",
        "unadmitted provider tool",
        served.json(Some(ROUTER), &asks(false)).await,
    );
    let mut image = asks(false);
    image["messages"][0]["content"][1] = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}});
    refused(
        400,
        "invalid_request_error",
        "unknown variant `image`",
        served.json(Some(WORKER), &image).await,
    );
    let mut huge = asks(false);
    huge["messages"][0]["content"][1]["text"] = json!("x".repeat(70_000));
    refused(
        403,
        "permission_error",
        "exceeds the model limit",
        served.json(Some(WORKER), &huge).await,
    );
    assert!(
        served.provider.bodies().is_empty(),
        "nothing refused reached the provider"
    );

    *served.provider.reply.lock().unwrap() = Reply::Garbled;
    let response = served.messages(Some(WORKER), &asks(true)).await;
    assert_eq!(response.status(), 502);
    assert_eq!(
        response.headers()["x-should-retry"],
        "false",
        "it was paid for"
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "api_error");
    assert_eq!(
        scratch.store().committed("ticket:tk_1", 0).unwrap(),
        COST,
        "an unreadable reply is still settled"
    );

    let unknown = served
        .client
        .post("http://broker/v1/messages/count_tokens")
        .json(&asks(false))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    let body: Value = unknown.json().await.unwrap();
    assert_eq!(body["error"]["type"], "not_found_error");
    let hello = served.client.head("http://broker/api/hello").send().await;
    assert_eq!(hello.unwrap().status(), 200, "Claude Code's preconnect");
}

/// The upstream of the real run: a prompt gets the call `first` (a broker
/// tool and its arguments), its tool result a `ready` report, and anything
/// after a closing line.
struct Reporter {
    first: (&'static str, Value),
    bodies: Mutex<Vec<Value>>,
}

impl Provider for Reporter {
    fn dispatch(&self, body: Value) -> Dispatch {
        let n = {
            let mut bodies = self.bodies.lock().unwrap();
            bodies.push(body.clone());
            bodies.len()
        };
        let results = body["messages"].as_array().map_or(0, |messages| {
            messages.iter().filter(|m| m["role"] == "tool").count()
        });
        let call = match results {
            0 => Some(self.first.clone()),
            1 => Some((
                "worker_report",
                json!({"status": "ready", "text": "Done: hello from Claude Code"}),
            )),
            _ => None,
        };
        let (message, finish) = if let Some((tool, arguments)) = call {
            let call = json!({"id": format!("call-{n}"), "type": "function", "function": {
                "name": format!("mcp__swarm__{tool}"), "arguments": arguments.to_string()}});
            (
                json!({"role": "assistant", "content": null, "tool_calls": [call]}),
                "tool_calls",
            )
        } else {
            (json!({"role": "assistant", "content": "Reported."}), "stop")
        };
        Box::pin(async move {
            Ok(json!({
                "id": format!("receipt-{n}"),
                "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                "usage": {"prompt_tokens": 1180, "prompt_cache_hit_tokens": 1024, "completion_tokens": 32},
            }))
        })
    }
}

/// `SOLOS_CLAUDE=$(readlink -f ~/.local/bin/claude) cargo test --test
/// agent_claude_code -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs Linux, bwrap and an installed Claude Code"]
async fn a_real_claude_code_worker_reports_ready_through_the_facade() {
    let claude = PathBuf::from(std::env::var_os("SOLOS_CLAUDE").expect("SOLOS_CLAUDE"));
    let scratch = Scratch::new("real");
    std::fs::write(scratch.root.join("node"), "").unwrap();
    let line = json!({"id": "t-ask", "source": "slack", "at_ms": 1_793_613_600_000u64,
        "actor": {"id": "alice", "kind": "person"}, "addressed": "mention",
        "text": "Say hello from Claude Code", "threads": ["T1"],
        "reply_to": [{"source": "slack", "address": {"thread_ts": "T1"}}]});
    std::fs::write(scratch.root.join("triggers.jsonl"), format!("{line}\n")).unwrap();
    let yaml = format!(
        "policy:\n  people: [slack:alice]\n\
         sources:\n  - {{kind: jsonl, path: triggers.jsonl}}\n\
         channels:\n  - {{kind: jsonl, source: slack, path: outbox.jsonl}}\n\
         router: {{kind: scripted}}\n\
         executor:\n  kind: pi\n  provider: deepseek\n  model: {MODEL}\n  \
         prices: {{input_microusd_per_million: 300000, output_microusd_per_million: 1200000, \
         cache_hit_microusd_per_million: 6000}}\n  max_output_tokens: {MAX_OUTPUT}\n  \
         harness: claude-code\n  claude: {{binary: {}, solos_swarm: {}}}\n\
         budgets: {{session_microusd: 500000}}\n",
        claude.display(),
        env!("CARGO_BIN_EXE_solos-swarm"),
    );
    let path = scratch.root.join("agent.yaml");
    std::fs::write(&path, yaml).unwrap();
    let config = AgentConfig::load(&path).unwrap();
    let reporter = Arc::new(Reporter {
        first: (
            "worker_report",
            json!({"status": "progress", "text": "Saying hello"}),
        ),
        bodies: Mutex::new(Vec::new()),
    });
    let host = PiHost {
        harness: PiHarness,
        provider: reporter.clone(),
        pi_root: scratch.root.clone(),
        node: scratch.root.join("node"),
    };

    let state = scratch.state();
    let serving = serve_with(config, &state, true, Some(host));
    let summary = tokio::time::timeout(Duration::from_secs(180), serving)
        .await
        .expect("serve did not go idle")
        .unwrap();

    eprintln!("summary: {}", json!(summary));
    let sent: Vec<OutboundMessage> = std::fs::read_to_string(scratch.root.join("outbox.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let kinds: Vec<MessageKind> = sent.iter().map(|m| m.kind).collect();
    assert_eq!(kinds, [MessageKind::Acknowledgement, MessageKind::Result]);
    assert_eq!(sent[1].text, "Done: hello from Claude Code");

    let store = scratch.store();
    let ticket = &store.tickets().unwrap()[0];
    let bodies = reporter.bodies.lock().unwrap().clone();
    assert_eq!(
        store
            .committed(&format!("ticket:{}", ticket.id), 0)
            .unwrap(),
        COST * bodies.len() as u64,
        "every model call settled on the Ticket's account"
    );
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
            "mcp__swarm__fetch_url",
            "mcp__swarm__worker_handback",
            "mcp__swarm__worker_report"
        ],
        "only the broker's tools"
    );
    let system = bodies[0]["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains("You are a worker of a long-running agent"),
        "{system}"
    );
    assert!(bodies.len() >= 2, "the progress report's result came back");
    let result = bodies[1]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(result["role"], "tool");
    let recorded: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        (&recorded["recorded"], &recorded["status"]),
        (&json!(true), &json!("progress")),
        "the broker's answer, through the MCP server and Claude Code"
    );

    let dir = scratch.state().join(TICKETS_DIR).join(ticket.id.as_str());
    let assignment = &ticket.assignments[0].id;
    let transcript =
        std::fs::read_to_string(dir.join(format!("{assignment}.claude.jsonl"))).unwrap();
    let frames: Vec<Value> = transcript
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let init = frames
        .iter()
        .find(|frame| frame["subtype"] == "init")
        .expect("no init");
    eprintln!(
        "Claude Code {} ran with tools {} and MCP servers {}",
        init["claude_code_version"], init["tools"], init["mcp_servers"]
    );
    let kinds: Vec<String> = frames
        .iter()
        .map(|frame| {
            let kind = frame["type"].as_str().unwrap_or("?");
            match frame["subtype"].as_str() {
                Some(subtype) => format!("{kind}/{subtype}"),
                None => kind.to_owned(),
            }
        })
        .collect();
    eprintln!("transcript: {kinds:?}");
    eprintln!("model calls: {}", bodies.len());
    assert!(
        !dir.join(format!("{assignment}.token")).exists(),
        "the token file is gone with the session"
    );
}

/// `SOLOS_CLAUDE=$(readlink -f ~/.local/bin/claude) cargo test --test
/// agent_claude_code -- --ignored --nocapture`: the same worker on a Ticket
/// bound to a node, whose `run_code` runs in the node's sandbox, not here.
#[tokio::test]
#[ignore = "needs Linux, bwrap and an installed Claude Code"]
async fn a_real_claude_code_worker_runs_its_code_on_its_tickets_node() {
    let claude = PathBuf::from(std::env::var_os("SOLOS_CLAUDE").expect("SOLOS_CLAUDE"));
    let scratch = Scratch::new("node");
    let root = &scratch.root;
    std::fs::write(root.join("node"), "").unwrap();
    let agent = tls::init(&root.join("agent-tls"), tls::AGENT_NAME).unwrap();
    let laptop = tls::init(&root.join("laptop-tls"), tls::NODE_NAME).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let line = json!({"id": "t-ask", "source": "slack", "at_ms": 1_793_613_600_000u64,
        "actor": {"id": "alice", "kind": "person"}, "addressed": "mention",
        "text": "Say hello from my laptop", "threads": ["T1"],
        "reply_to": [{"source": "slack", "address": {"thread_ts": "T1"}}]});
    std::fs::write(root.join("triggers.jsonl"), format!("{line}\n")).unwrap();
    let yaml = format!(
        "policy:\n  people: [slack:alice]\n\
         sources:\n  - {{kind: jsonl, path: triggers.jsonl}}\n\
         channels:\n  - {{kind: jsonl, source: slack, path: outbox.jsonl}}\n\
         router: {{kind: scripted}}\n\
         executor:\n  kind: pi\n  provider: deepseek\n  model: {MODEL}\n  \
         prices: {{input_microusd_per_million: 300000, output_microusd_per_million: 1200000, \
         cache_hit_microusd_per_million: 6000}}\n  max_output_tokens: {MAX_OUTPUT}\n  \
         harness: claude-code\n  claude: {{binary: {}, solos_swarm: {}}}\n  environment: laptop\n\
         budgets: {{session_microusd: 500000}}\n\
         nodes:\n  listen: 127.0.0.1:{port}\n  cert: agent-tls/cert.pem\n  key: agent-tls/key.pem\n  \
         allow:\n    laptop: {{fingerprint: \"{laptop}\"}}\n",
        claude.display(),
        env!("CARGO_BIN_EXE_solos-swarm"),
    );
    let path = root.join("agent.yaml");
    std::fs::write(&path, yaml).unwrap();
    let config = AgentConfig::load(&path).unwrap();
    let reporter = Arc::new(Reporter {
        first: (
            "run_code",
            json!({"code": "echo hello from the laptop | tee here.txt"}),
        ),
        bodies: Mutex::new(Vec::new()),
    });
    let host = PiHost {
        harness: PiHarness,
        provider: reporter.clone(),
        pi_root: root.clone(),
        node: root.join("node"),
    };
    let (cert, key) = tls::files(&root.join("laptop-tls"));
    let node = Node::new(&NodeConfig {
        name: "laptop".into(),
        agent: AgentEndpoint {
            address: format!("127.0.0.1:{port}"),
            fingerprint: agent,
        },
        cert,
        key,
        work_root: root.join("laptop-work"),
        sandbox: SandboxConfig::default(),
        secrets: SecretsConfig::default(),
        vault: None,
    })
    .unwrap();
    let daemon = tokio::spawn(async move { node.serve(std::future::pending()).await });

    let state = scratch.state();
    let serving = serve_with(config, &state, true, Some(host));
    let summary = tokio::time::timeout(Duration::from_secs(180), serving)
        .await
        .expect("serve did not go idle")
        .unwrap();
    daemon.abort();

    eprintln!("summary: {}", json!(summary));
    let sent: Vec<OutboundMessage> = std::fs::read_to_string(root.join("outbox.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let kinds: Vec<MessageKind> = sent.iter().map(|m| m.kind).collect();
    assert_eq!(kinds, [MessageKind::Acknowledgement, MessageKind::Result]);
    let ticket = &scratch.store().tickets().unwrap()[0];
    let bodies = reporter.bodies.lock().unwrap().clone();
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
            "mcp__swarm__fetch_url",
            "mcp__swarm__job_start",
            "mcp__swarm__job_stop",
            "mcp__swarm__job_wait",
            "mcp__swarm__run_code",
            "mcp__swarm__worker_handback",
            "mcp__swarm__worker_report"
        ],
        "run_code and the job tools, on the node, though this host has no run_code"
    );
    let result = bodies[1]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(result["role"], "tool");
    let receipt: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        (&receipt["exit_code"], &receipt["stdout"]),
        (&json!(0), &json!("hello from the laptop\n")),
        "the node's receipt, through the MCP server and Claude Code: {receipt}"
    );
    let on_node = root
        .join("laptop-work")
        .join(ticket.id.as_str())
        .join("here.txt");
    assert_eq!(
        std::fs::read_to_string(on_node).unwrap(),
        "hello from the laptop\n"
    );
    let here = state
        .join(TICKETS_DIR)
        .join(ticket.id.as_str())
        .join("work/here.txt");
    assert!(!here.exists(), "run_code ran on the agent's host");
}
