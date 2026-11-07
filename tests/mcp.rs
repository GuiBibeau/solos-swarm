//! `solos-swarm mcp` end to end, the binary driven on its stdin and stdout:
//! against a real agent broker on its Unix socket (the handshake, each
//! principal's tools, calls, refusals, unknown tools and methods, a pause,
//! the token file), and as an operator's host over a scripted `agent serve`.

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{Next, from_fn};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use solos_agent_core::AllowlistPolicy;
use solos_agent_core::model::{
    Actor, AssignmentId, DecisionKind, MessageKind, ThreadKey, TicketId, TriggerId,
};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::broker::registry::{self, PrincipalKind};
use solos_swarm::agent::broker::{
    self, Account, AgentBroker, BrokerParts, DecideTerms, Grant, ModelTerms, Principal,
    WorkerParts, WorkerSeat,
};
use solos_swarm::agent::scripted::{ScriptedExecutor, ScriptedRouter};
use solos_swarm::agent::{Adapters, RunOptions, Runtime, STORE_FILE, ServeOptions, serve};
use solos_swarm::gateway::{Dispatch, PriceTable, Provider, Upstream};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const LIMIT: Duration = Duration::from_secs(30);
const ROUTER_TOKEN: &str = "router-secret-token";
const WORKER_TOKEN: &str = "worker-secret-token";

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "mcp-{name}-{}-{}",
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
    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The `mcp` binary, spoken to as a host would.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next: u64,
}

impl Mcp {
    fn spawn(args: &[&dyn AsRef<std::ffi::OsStr>]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_solos-swarm"));
        command.arg("mcp");
        for arg in args {
            command.arg(arg);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            stdout,
            next: 0,
        }
    }

    fn broker(socket: &Path, token_file: &Path, extra: &[&str]) -> Self {
        let mut args: Vec<&dyn AsRef<std::ffi::OsStr>> =
            vec![&"--socket", &socket, &"--token-file", &token_file];
        for arg in extra {
            args.push(arg);
        }
        Self::spawn(&args)
    }

    async fn send(&mut self, message: &Value) {
        let mut line = serde_json::to_vec(message).unwrap();
        line.push(b'\n');
        self.stdin.write_all(&line).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// The whole response to one request.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        let reply = tokio::time::timeout(LIMIT, async {
            loop {
                let line = self.stdout.next_line().await.unwrap().expect("mcp exited");
                let reply: Value = serde_json::from_str(&line).unwrap();
                if reply["id"] == id {
                    return reply;
                }
            }
        })
        .await
        .expect("mcp did not answer");
        assert_eq!(reply["jsonrpc"], "2.0", "{reply}");
        reply
    }

    async fn initialize(&mut self) -> Value {
        let reply = self
            .request(
                "initialize",
                json!({"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "test-host", "version": "1"}}),
            )
            .await;
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        reply["result"].clone()
    }

    async fn names(&mut self) -> Vec<String> {
        let reply = self.request("tools/list", json!({})).await;
        reply["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{reply}"))
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// A `tools/call` result: its text and whether it is an error.
    async fn call(&mut self, name: &str, arguments: Value) -> (String, bool) {
        let reply = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        let result = &reply["result"];
        let content = result["content"]
            .as_array()
            .unwrap_or_else(|| panic!("{reply}"));
        assert_eq!(content.len(), 1, "{reply}");
        assert_eq!(content[0]["type"], "text");
        (
            content[0]["text"].as_str().unwrap().to_owned(),
            result["isError"].as_bool().unwrap(),
        )
    }

    /// End the session as a host does, by closing stdin; its stderr.
    async fn close(self) -> String {
        drop(self.stdin);
        let output = tokio::time::timeout(LIMIT, self.child.wait_with_output())
            .await
            .expect("mcp did not exit")
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stderr).unwrap()
    }
}

/// No model is called in these tests.
struct NoModel;

impl Provider for NoModel {
    fn dispatch(&self, _: Value) -> Dispatch {
        Box::pin(async { Err(anyhow::anyhow!("no model in this test")) })
    }
}

fn terms() -> ModelTerms {
    ModelTerms {
        upstream: Upstream::deepseek("deepseek-flash"),
        prices: PriceTable {
            input_microusd_per_million: 300_000,
            output_microusd_per_million: 1_200_000,
            cache_hit_microusd_per_million: None,
        },
        max_input_bytes: 65_536,
        max_output_tokens: 2_048,
        call_limit: None,
        temperature: None,
    }
}

fn scripted_config(scratch: &Scratch, io: &str, people: &str) -> PathBuf {
    scratch.write(
        "agent.yaml",
        &format!(
            "policy:\n  people: [{people}]\n{io}router: {{kind: scripted}}\n\
             executor: {{kind: scripted}}\nbudgets: {{session_microusd: 500000}}\n"
        ),
    )
}

async fn serve_until_idle(config: &Path, state: &Path) -> solos_swarm::agent::Summary {
    let options = ServeOptions {
        until_idle: true,
        ..ServeOptions::default()
    };
    tokio::time::timeout(LIMIT, serve(config, state, options))
        .await
        .expect("serve did not go idle")
        .unwrap()
}

/// A broker on the state directory's socket that admits a Router and one
/// worker (with no `run_code`), its routes wrapped by `layer`. The runtime
/// whose facts it takes never runs, so no call here may reach the Engine.
async fn broker_on(
    scratch: &Scratch,
    layer: impl FnOnce(axum::Router) -> axum::Router,
) -> (Runtime, tokio::task::JoinHandle<()>, PathBuf) {
    let adapters = Adapters {
        sources: Vec::new(),
        channels: BTreeMap::new(),
        router: Arc::new(ScriptedRouter::new(500_000)),
        executor: Arc::new(ScriptedExecutor),
    };
    let runtime = Runtime::new(
        scratch.store(),
        AllowlistPolicy::default(),
        adapters,
        RunOptions::default(),
    );
    let broker = AgentBroker::new(BrokerParts {
        dir: scratch.state(),
        store: scratch.store(),
        provider: Arc::new(NoModel),
        facts: runtime.facts(),
        decide: DecideTerms {
            default_budget_microusd: Some(500_000),
            max_budget_microusd: None,
            swarm: None,
            repos: BTreeMap::new(),
            ..DecideTerms::default()
        },
        workers: Some(WorkerParts {
            terms: terms(),
            code: None,
            vault: None,
            browser: None,
        }),
    })
    .unwrap();
    let router = Grant {
        principal: Principal::Router,
        account: Account::Router {
            daily_microusd: Some(1_000_000),
        },
        terms: terms(),
        system_prompt: None,
    };
    broker.admit(ROUTER_TOKEN.into(), router).unwrap();
    let seat = WorkerSeat::new(
        TicketId::new("tk_1"),
        AssignmentId::new("as_1"),
        scratch.root.join("work"),
        500_000,
    );
    broker
        .admit_worker(WORKER_TOKEN.into(), seat, "You are a worker.".into())
        .unwrap();
    let (listener, socket) = broker::bind(&scratch.state()).unwrap();
    let app = layer(broker.router().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (runtime, server, socket)
}

/// Store one Slack request, `t-ask`, through a scripted `agent serve`.
async fn store_a_request(scratch: &Scratch) {
    let trigger = json!({
        "id": "t-ask", "source": "slack", "at_ms": 1_000,
        "actor": {"id": "alice", "kind": "person"}, "addressed": "mention",
        "text": "Summarise the incident report", "threads": ["T1"],
        "reply_to": [{"source": "slack", "address": {"thread_ts": "T1"}}],
    });
    scratch.write("triggers.jsonl", &format!("{trigger}\n"));
    let io = "sources:\n  - {kind: jsonl, path: triggers.jsonl}\n\
              channels:\n  - {kind: jsonl, source: slack, path: outbox.jsonl}\n";
    let config = scripted_config(scratch, io, "slack:alice");
    serve_until_idle(&config, &scratch.state()).await;
}

fn schemas(kind: PrincipalKind) -> BTreeMap<String, Value> {
    registry::tools(kind)
        .into_iter()
        .map(|tool| (tool.name.to_owned(), tool.input_schema))
        .collect()
}

#[tokio::test]
async fn a_router_and_a_worker_each_list_and_call_only_their_own_tools() {
    let scratch = Scratch::new("principals");
    store_a_request(&scratch).await;
    let (_runtime, _server, socket) = broker_on(&scratch, |app| app).await;

    let token_file = scratch.write("router.token", &format!("{ROUTER_TOKEN}\n"));
    let mut router = Mcp::broker(&socket, &token_file, &[]);
    let init = router.initialize().await;
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert_eq!(
        init["capabilities"],
        json!({"tools": {"listChanged": false}})
    );
    assert_eq!(init["serverInfo"]["name"], "solos-swarm");
    assert!(
        !token_file.exists(),
        "the token file is read once and deleted"
    );
    assert_eq!(router.request("ping", json!({})).await["result"], json!({}));

    let listed = router.request("tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    let offered: BTreeMap<String, Value> = tools
        .iter()
        .map(|tool| {
            assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
            (
                tool["name"].as_str().unwrap().to_owned(),
                tool["inputSchema"].clone(),
            )
        })
        .collect();
    assert_eq!(
        offered,
        schemas(PrincipalKind::Router),
        "a Router sees Router tools only"
    );

    let (text, is_error) = router.call("router_read", json!({"id": "t-ask"})).await;
    assert!(!is_error, "{text}");
    let read: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        (&read["kind"], &read["id"]),
        (&json!("trigger"), &json!("t-ask"))
    );
    assert_eq!(
        router.call("router_read", json!({"id": "nope"})).await,
        ("no trigger, thread or ticket nope".to_owned(), true),
        "a refusal is a tool error with the broker's reason"
    );
    let (text, is_error) = router.call("router_read", json!({"name": "t-ask"})).await;
    assert!(is_error && text.starts_with("bad arguments"), "{text}");
    for forged in ["run_code", "worker_report", "claim_task"] {
        let reply = router
            .request("tools/call", json!({"name": forged, "arguments": {}}))
            .await;
        assert_eq!(reply["error"]["code"], -32_602, "{forged}: {reply}");
        assert_eq!(reply["error"]["message"], format!("Unknown tool: {forged}"));
    }
    let reply = router
        .request(
            "tools/call",
            json!({"name": "router_read", "arguments": [1]}),
        )
        .await;
    assert_eq!(reply["error"]["code"], -32_602, "{reply}");
    let reply = router.request("resources/list", json!({})).await;
    assert_eq!(reply["error"]["code"], -32_601, "{reply}");
    let stderr = router.close().await;
    assert!(
        !stderr.contains(ROUTER_TOKEN),
        "the token is never logged: {stderr}"
    );

    let token_file = scratch.write("worker.token", WORKER_TOKEN);
    let mut worker = Mcp::broker(&socket, &token_file, &[]);
    worker.initialize().await;
    assert!(!token_file.exists());
    // The seat of a host with no run_code, vault, browser or pstack.
    let mut expected: Vec<String> = ["worker_report", "worker_handback", "fetch_url"]
        .map(String::from)
        .to_vec();
    expected.sort();
    let mut names = worker.names().await;
    names.sort();
    assert_eq!(
        names, expected,
        "a worker sees worker tools only, and no run_code, job tools, vault_list, browser or \
         read on this host"
    );
    for absent in ["run_code", "job_start"] {
        let reply = worker
            .request(
                "tools/call",
                json!({"name": absent, "arguments": {"code": "true"}}),
            )
            .await;
        assert_eq!(reply["error"]["code"], -32_602, "{absent}: {reply}");
    }
    let (text, is_error) = worker
        .call(
            "worker_handback",
            json!({"trigger": "t-ask", "reason": " "}),
        )
        .await;
    assert_eq!(
        (text.as_str(), is_error),
        ("a handback needs a reason", true)
    );
    let reply = worker
        .request(
            "tools/call",
            json!({"name": "router_decide", "arguments": {}}),
        )
        .await;
    assert_eq!(reply["error"]["code"], -32_602, "{reply}");
    assert!(!worker.close().await.contains(WORKER_TOKEN));

    let forged = scratch.write("forged.token", "forged");
    let mut stranger = Mcp::broker(&socket, &forged, &[]);
    stranger.initialize().await;
    let reply = stranger.request("tools/list", json!({})).await;
    assert_eq!(reply["error"]["code"], -32_603, "{reply}");
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid identity"),
        "{reply}"
    );
    stranger.close().await;
}

/// Answers the next `pauses` tool calls (all of them when negative) as a
/// paused broker does, before they reach the broker.
fn pausing(pauses: Arc<AtomicI64>) -> impl FnOnce(axum::Router) -> axum::Router {
    move |app| {
        app.layer(from_fn(move |request: Request, next: Next| {
            let pauses = pauses.clone();
            async move {
                let left = pauses.load(Ordering::SeqCst);
                if request.uri().path().starts_with("/tool/") && left != 0 {
                    if left > 0 {
                        pauses.fetch_sub(1, Ordering::SeqCst);
                    }
                    let paused = json!({"paused": true, "retry_after_s": 0.2,
                                        "reason": "model calls are paused (operator pause or peak window)"});
                    return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(paused)).into_response();
                }
                next.run(request).await
            }
        }))
    }
}

#[tokio::test]
async fn a_paused_broker_is_waited_out_then_reported() {
    let scratch = Scratch::new("pause");
    let pauses = Arc::new(AtomicI64::new(2));
    let (_runtime, _server, socket) = broker_on(&scratch, pausing(pauses.clone())).await;
    let token_file = scratch.write("router.token", ROUTER_TOKEN);
    let mut router = Mcp::broker(&socket, &token_file, &["--max-pause-wait-secs", "2"]);
    router.initialize().await;

    let started = Instant::now();
    let answer = router.call("router_read", json!({"id": "nope"})).await;
    assert_eq!(
        answer,
        ("no trigger, thread or ticket nope".to_owned(), true),
        "the call reached the broker once the pause lifted"
    );
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert_eq!(pauses.load(Ordering::SeqCst), 0);

    pauses.store(-1, Ordering::SeqCst);
    let started = Instant::now();
    let (text, is_error) = router.call("router_read", json!({"id": "t1"})).await;
    let waited = started.elapsed();
    assert!(is_error, "{text}");
    assert!(
        text.contains("paused") && text.contains("did nothing"),
        "{text}"
    );
    assert!(
        waited >= Duration::from_secs(2) && waited < Duration::from_secs(10),
        "{waited:?}"
    );
    router.close().await;
}

#[tokio::test]
async fn an_operator_files_requests_as_mcp_people_and_reads_the_answers() {
    let scratch = Scratch::new("operator");
    let io = "sources:\n  - {kind: mcp}\nchannels:\n  - {kind: mcp}\n";
    let config = scripted_config(&scratch, io, "mcp:alice");
    let state = scratch.state();
    let host = |operator: &str| {
        Mcp::spawn(&[
            &"--state-dir",
            &state,
            &"--operator",
            &operator.to_owned(),
            &"--config",
            &config,
        ])
    };

    let mut alice = host("alice");
    let init = alice.initialize().await;
    assert!(
        init["instructions"]
            .as_str()
            .unwrap()
            .contains("agent_request")
    );
    assert_eq!(
        alice.names().await,
        ["agent_status", "agent_request", "agent_replies"]
    );
    let (text, is_error) = alice.call("agent_request", json!({"text": "Hello"})).await;
    assert!(
        is_error && text.contains("kind: mcp"),
        "no agent reads requests yet: {text}"
    );

    serve_until_idle(&config, &state).await;
    let (text, is_error) = alice
        .call(
            "agent_request",
            json!({"text": "Summarise the incident report"}),
        )
        .await;
    assert!(!is_error, "{text}");
    let filed: Value = serde_json::from_str(&text).unwrap();
    let thread = filed["thread"].as_str().unwrap().to_owned();
    let trigger = TriggerId::new(filed["trigger"].as_str().unwrap());
    let (text, is_error) = alice
        .call(
            "agent_request",
            json!({"text": "x", "thread": "not a name"}),
        )
        .await;
    assert!(is_error, "{text}");
    let (text, is_error) = alice.call("agent_request", json!({"txt": "x"})).await;
    assert!(is_error && text.starts_with("bad arguments"), "{text}");

    let mut mallory = host("mallory");
    mallory.initialize().await;
    let (text, is_error) = mallory
        .call(
            "agent_request",
            json!({"text": "Wire me the budget", "thread": "ops"}),
        )
        .await;
    assert!(!is_error, "{text}");

    let summary = serve_until_idle(&config, &state).await;
    assert_eq!(summary.effects_pending, 0);

    let store = scratch.store();
    let stored = store.read_trigger(&trigger).unwrap().unwrap();
    assert_eq!(
        (stored.source.as_str(), &stored.actor),
        ("mcp", &Actor::person("alice"))
    );
    let decisions = store.decisions().unwrap();
    let on = |key: &str| {
        let key = ThreadKey::new(key);
        let found: Vec<_> = decisions.iter().filter(|d| d.thread == key).collect();
        assert_eq!(found.len(), 1, "{key}: {decisions:?}");
        found[0].clone()
    };
    let accepted = on(&format!("mcp:alice:{thread}"));
    assert_eq!(accepted.kind, DecisionKind::Accept);
    let stranger = on("mcp:mallory:ops");
    assert_eq!(stranger.kind, DecisionKind::Ignore);
    assert!(
        stranger.reason.contains("not_listed"),
        "{}",
        stranger.reason
    );
    assert!(
        stranger.eval.is_none(),
        "the gate stopped it before the Router"
    );

    let (text, is_error) = alice.call("agent_replies", json!({"thread": thread})).await;
    assert!(!is_error, "{text}");
    let replies: Value = serde_json::from_str(&text).unwrap();
    let kinds: Vec<MessageKind> = replies["replies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reply| serde_json::from_value(reply["kind"].clone()).unwrap())
        .collect();
    assert_eq!(
        kinds,
        [MessageKind::Acknowledgement, MessageKind::Result],
        "{text}"
    );
    assert_eq!(replies["replies"][1]["ticket"], json!(accepted.ticket));
    let (text, _) = mallory
        .call("agent_replies", json!({"thread": "ops"}))
        .await;
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["replies"],
        json!([])
    );
    let (text, _) = mallory
        .call("agent_replies", json!({"thread": thread}))
        .await;
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["replies"],
        json!([]),
        "one operator cannot read another's thread"
    );

    let (text, is_error) = alice.call("agent_status", json!({})).await;
    assert!(!is_error, "{text}");
    let status: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(status["decisions"].as_array().unwrap().len(), 2, "{status}");
    assert_eq!(
        status["tickets"],
        json!([]),
        "the scripted ticket has ended"
    );
    alice.close().await;
    mallory.close().await;
}

#[tokio::test]
async fn an_operators_result_names_the_tickets_page_when_the_config_has_a_public_url() {
    let scratch = Scratch::new("operator-web");
    let io = "sources:\n  - {kind: mcp}\nchannels:\n  - {kind: mcp}\n\
              web: {public_url: 'https://agent.example.com'}\n";
    let config = scripted_config(&scratch, io, "mcp:alice");
    let state = scratch.state();
    serve_until_idle(&config, &state).await;
    let mut alice = Mcp::spawn(&[
        &"--state-dir",
        &state,
        &"--operator",
        &"alice",
        &"--config",
        &config,
    ]);
    alice.initialize().await;
    let (text, is_error) = alice
        .call("agent_request", json!({"text": "Summarise the incident"}))
        .await;
    assert!(!is_error, "{text}");
    let thread = serde_json::from_str::<Value>(&text).unwrap()["thread"]
        .as_str()
        .unwrap()
        .to_owned();
    serve_until_idle(&config, &state).await;

    let (text, is_error) = alice.call("agent_replies", json!({"thread": thread})).await;
    assert!(!is_error, "{text}");
    let replies: Value = serde_json::from_str(&text).unwrap();
    let texts: Vec<&str> = replies["replies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reply| reply["text"].as_str().unwrap())
        .collect();
    let ticket = replies["replies"][1]["ticket"].as_str().unwrap();
    assert_eq!(texts.len(), 2, "{text}");
    assert_eq!(
        texts[0], "Looking into it.",
        "an acknowledgement has no link"
    );
    assert!(
        texts[1].starts_with("Done: Summarise the incident."),
        "{text}"
    );
    assert!(
        texts[1].ends_with(&format!(
            "\n\nDetails: https://agent.example.com/tickets/{ticket}"
        )),
        "{text}"
    );
    alice.close().await;
}

#[tokio::test]
async fn mcp_refuses_a_mixed_or_missing_mode_and_still_deletes_the_token() {
    let scratch = Scratch::new("usage");
    let token_file = scratch.write("t.token", ROUTER_TOKEN);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_solos-swarm"))
        .arg("mcp")
        .arg("--token-file")
        .arg(&token_file)
        .arg("--state-dir")
        .arg(scratch.state())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--socket and --token-file"));
    assert!(!token_file.exists());
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_solos-swarm"))
        .args(["mcp", "--state-dir", "/nonexistent", "--operator", "a:b"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success(), "an operator name with a colon");
}
