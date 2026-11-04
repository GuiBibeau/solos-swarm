//! Remote execution nodes (P9): the agent's node listener and a
//! `solos-swarm node` daemon in one process, over mutually authenticated
//! TLS on loopback. A worker's `run_code` and background jobs on a Ticket
//! bound to a node run in the node's sandbox and come back with local
//! `run_code`'s receipts; a certificate that is not pinned is refused on
//! either side; a node that drops fails its calls at once; a Ticket bound to
//! an offline node launches once it connects; the node's secrets reach its
//! sandbox and never a receipt; oversized frames end the connection; a
//! Claude Code worker's tools, `solos-swarm mcp` over the broker, run on its
//! node too. Tests that run a sandbox skip without Linux, bwrap and prlimit.

use serde_json::{Value, json};
use solos_agent_core::AllowlistPolicy;
use solos_agent_core::model::{AssignmentId, Effect, Stage, TicketId, TicketOutcome};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::broker::{
    Account, AgentBroker, BrokerParts, DecideTerms, Grant, ModelTerms, Principal, SOCKET_FILE,
    WorkerParts, WorkerSeat,
};
use solos_swarm::agent::config::{AllowedNode, NodesConfig};
use solos_swarm::agent::environment::{ExecutionEnvironment, Nodes, Work};
use solos_swarm::agent::jobs::JobLimits;
use solos_swarm::agent::jsonl::{JsonlChannel, JsonlSource};
use solos_swarm::agent::scripted::{ScriptedExecutor, ScriptedRouter};
use solos_swarm::agent::{
    Adapters, AgentConfig, PiHost, RunOptions, Runtime, STORE_FILE, TICKETS_DIR, serve_with,
};
use solos_swarm::code::{CodeConfig, CodeExecutor, MAX_FILES_BYTES};
use solos_swarm::gateway::{Dispatch, PriceTable, Provider, Upstream};
use solos_swarm::harness::scripted::{Call, ScriptedHarness};
use solos_swarm::remote::daemon::{AgentEndpoint, SandboxConfig, SecretsConfig};
use solos_swarm::remote::tls::{self, Fingerprint, Identity};
use solos_swarm::remote::wire::{self, Hello, MAX_FRAME, PROTOCOL, Welcome};
use solos_swarm::remote::{Listener, Node, NodeConfig};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

const LIMIT: Duration = Duration::from_secs(60);
const LIMITS: JobLimits = JobLimits {
    background_after: Duration::from_secs(5),
    max_jobs: 4,
    max_wall: Duration::from_secs(300),
    output_bytes: 65_536,
};
const TOKEN: &str = "s3cr3t-token-value";
const FILE_SECRET: &str = "file-secret-0123456";

fn sandbox() -> bool {
    let found = cfg!(target_os = "linux")
        && Path::new("/usr/bin/bwrap").is_file()
        && Path::new("/usr/bin/prlimit").is_file();
    if !found {
        eprintln!("skipped: a node's sandbox needs Linux, /usr/bin/bwrap and /usr/bin/prlimit");
    }
    found
}

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "agent-nodes-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// The agent's and each node's certificates, and where nodes dial.
struct Pki {
    root: PathBuf,
    agent: Fingerprint,
    laptop: Fingerprint,
    desktop: Fingerprint,
}

impl Pki {
    fn new(root: &Path) -> Self {
        let made = |name: &str, subject: &str| tls::init(&root.join(name), subject).unwrap();
        Self {
            root: root.to_owned(),
            agent: made("agent-tls", tls::AGENT_NAME),
            laptop: made("laptop-tls", tls::NODE_NAME),
            desktop: made("desktop-tls", tls::NODE_NAME),
        }
    }

    fn nodes(&self, listen: &str) -> NodesConfig {
        let (cert, key) = tls::files(&self.root.join("agent-tls"));
        let allowed = |fingerprint| AllowedNode {
            fingerprint,
            about: None,
        };
        NodesConfig {
            listen: listen.parse().unwrap(),
            cert,
            key,
            allow: BTreeMap::from([
                ("laptop".to_owned(), allowed(self.laptop)),
                ("desktop".to_owned(), allowed(self.desktop)),
            ]),
        }
    }

    /// A node named `name` that presents the certificate in `tls_dir`,
    /// pins `agent` and dials `address`. Without a real sandbox, any file
    /// stands in for bwrap and prlimit, which nothing then runs.
    fn node(&self, name: &str, tls_dir: &str, agent: Fingerprint, address: String) -> NodeConfig {
        let (cert, key) = tls::files(&self.root.join(tls_dir));
        let mut sandbox_config = SandboxConfig::default();
        if !sandbox() {
            sandbox_config.bwrap = PathBuf::from("/bin/sh");
            sandbox_config.prlimit = PathBuf::from("/bin/sh");
        }
        NodeConfig {
            name: name.into(),
            agent: AgentEndpoint {
                address,
                fingerprint: agent,
            },
            cert,
            key,
            work_root: self.root.join(format!("{name}-work")),
            sandbox: sandbox_config,
            secrets: SecretsConfig::default(),
            vault: None,
        }
    }
}

async fn until_ready(node: &dyn ExecutionEnvironment) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while node.ready().is_err() {
        assert!(Instant::now() < deadline, "the node never connected");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn until_offline(node: &dyn ExecutionEnvironment) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while node.ready().is_ok() {
        assert!(Instant::now() < deadline, "the node stayed online");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn connect(config: &NodeConfig) -> JoinHandle<()> {
    let node = Node::new(config).unwrap();
    tokio::spawn(async move {
        let _ = node.session().await;
    })
}

struct Silent;

impl Provider for Silent {
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
        max_output_tokens: 1_024,
        call_limit: None,
    }
}

/// An agent broker whose workers run either here or on the node
/// `laptop`, with the node connected.
struct World {
    root: PathBuf,
    listener: Option<Listener>,
    laptop: Arc<dyn ExecutionEnvironment>,
    node: Option<JoinHandle<()>>,
    broker: AgentBroker,
    client: reqwest::Client,
    server: JoinHandle<()>,
    _runtime: Runtime,
}

impl World {
    async fn new(name: &str, secrets: SecretsConfig) -> Self {
        let root = scratch(name);
        let pki = Pki::new(&root);
        let listener = Listener::start(&pki.nodes("127.0.0.1:0")).await.unwrap();
        let laptop = listener.environments()["laptop"].clone();
        let mut config = pki.node(
            "laptop",
            "laptop-tls",
            pki.agent,
            listener.local_addr().to_string(),
        );
        config.secrets = secrets;
        let node = connect(&config);
        until_ready(laptop.as_ref()).await;

        let state = root.join("state");
        std::fs::create_dir_all(state.join(TICKETS_DIR)).unwrap();
        let store = || SqliteStore::open(state.join(STORE_FILE)).unwrap();
        let adapters = Adapters {
            sources: vec![Box::new(JsonlSource::new(
                root.join("triggers.jsonl"),
                false,
            ))],
            channels: BTreeMap::from([(
                "slack".to_owned(),
                Arc::new(JsonlChannel::open(root.join("outbox.jsonl")).unwrap())
                    as Arc<dyn solos_swarm::agent::ports::Channel>,
            )]),
            router: Arc::new(ScriptedRouter::new(500_000)),
            executor: Arc::new(ScriptedExecutor),
        };
        let runtime = Runtime::new(
            store(),
            AllowlistPolicy::default(),
            adapters,
            RunOptions::default(),
        );
        let local = if sandbox() {
            let mut code = CodeConfig::new(state.join(TICKETS_DIR), Vec::new());
            code.concurrency = 4;
            Some(Arc::new(CodeExecutor::from_config(code).unwrap()))
        } else {
            None
        };
        let parts = BrokerParts {
            dir: state.clone(),
            store: store(),
            provider: Arc::new(Silent),
            facts: runtime.facts(),
            decide: DecideTerms {
                default_budget_microusd: 500_000,
                max_budget_microusd: None,
                swarm: None,
                repos: BTreeMap::new(),
            },
            workers: Some(WorkerParts {
                terms: terms(),
                code: local,
                vault: None,
                browser: None,
            }),
        };
        let nodes = Nodes {
            default: None,
            nodes: listener.environments(),
        };
        let broker = AgentBroker::with_nodes(parts, LIMITS, nodes).unwrap();
        let (socket_listener, socket) = solos_swarm::agent::broker::bind(&state).unwrap();
        let app = broker.router().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(socket_listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .unix_socket(socket)
            .build()
            .unwrap();
        Self {
            root,
            listener: Some(listener),
            laptop,
            node: Some(node),
            broker,
            client,
            server,
            _runtime: runtime,
        }
    }

    /// A worker in a turn on `ticket`, whose commands run on `environment`.
    fn worker(&self, ticket: &str, environment: Option<&str>) -> String {
        let work = self
            .root
            .join("state")
            .join(TICKETS_DIR)
            .join(ticket)
            .join("work");
        std::fs::create_dir_all(&work).unwrap();
        let token = format!("worker-{ticket}-{}", environment.unwrap_or("here"));
        let seat = WorkerSeat::new(
            TicketId::new(ticket),
            AssignmentId::new(format!("as_{ticket}")),
            work,
            500_000,
        )
        .in_environment(environment.map(str::to_owned));
        let seat = self
            .broker
            .admit_worker(token.clone(), seat, "You are a worker.".into())
            .unwrap();
        seat.begin_turn();
        token
    }

    async fn post(&self, tool: &str, token: &str, body: Value) -> (u16, Value) {
        let response = self
            .client
            .post(format!("http://broker/tool/{tool}"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    fn node_work(&self, ticket: &str) -> PathBuf {
        self.root.join("laptop-work").join(ticket)
    }

    /// Take the node offline, as a machine that sleeps or loses its network.
    async fn drop_node(&mut self) {
        if let Some(node) = self.node.take() {
            node.abort();
            let _ = node.await;
        }
        until_offline(self.laptop.as_ref()).await;
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.server.abort();
        if let Some(node) = &self.node {
            node.abort();
        }
        drop(self.listener.take());
        let _ = std::fs::remove_dir_all(&self.root);
    }
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

async fn until_running(marker: &str) {
    for _ in 0..400 {
        if command_lines(marker)
            .iter()
            .any(|cmdline| cmdline.starts_with(marker))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{marker} never ran");
}

async fn until_gone(marker: &str) {
    for _ in 0..400 {
        if command_lines(marker).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "{marker} left processes behind: {:?}",
        command_lines(marker)
    );
}

fn new_marker() -> String {
    format!("node-job-{}", uuid::Uuid::new_v4().simple())
}

fn without_timings(mut receipt: Value) -> Value {
    receipt["duration_ms"] = json!(0);
    receipt["queue_wait_ms"] = json!(0);
    receipt
}

#[tokio::test]
async fn run_code_on_a_node_returns_the_receipt_local_run_code_gives() {
    if !sandbox() {
        return;
    }
    let w = World::new("parity", SecretsConfig::default()).await;
    let here = w.worker("tk_here", None);
    let there = w.worker("tk_there", Some("laptop"));
    for args in [
        json!({"code": "cat in.txt; printf 'é%.0s' $(seq 40000); echo oops >&2; exit 3", "files": {"in.txt": "x"}}),
        json!({"code": "read line; echo \"got $line\" | tee seen.txt", "stdin": "hello\n"}),
        json!({"code": "sleep 5", "timeout_seconds": 1}),
        json!({"code": "pwd; echo $HOME; ls /"}),
    ] {
        let (local_status, local) = w.post("run_code", &here, args.clone()).await;
        let (remote_status, remote) = w.post("run_code", &there, args.clone()).await;
        assert_eq!(
            (local_status, remote_status),
            (200, 200),
            "{args}: {local} {remote}"
        );
        assert_eq!(without_timings(remote), without_timings(local), "{args}");
    }
    for bad in [
        json!({"code": " "}),
        json!({"code": "true", "files": {"../x": "y"}}),
    ] {
        let local = w.post("run_code", &here, bad.clone()).await;
        let remote = w.post("run_code", &there, bad.clone()).await;
        assert_eq!(remote, local, "{bad}");
        assert_eq!(remote.0, 422, "{bad}");
    }
    assert_eq!(
        std::fs::read_to_string(w.node_work("tk_there").join("seen.txt")).unwrap(),
        "got hello\n",
        "the command ran in the node's /work"
    );
    let agent_side = w.root.join("state").join(TICKETS_DIR).join("tk_there/work");
    assert!(
        !agent_side.join("seen.txt").exists(),
        "nothing ran on the agent's host"
    );
}

#[tokio::test]
async fn remote_background_jobs_start_wait_and_stop() {
    if !sandbox() {
        return;
    }
    let w = World::new("jobs", SecretsConfig::default()).await;
    let worker = w.worker("tk_jobs", Some("laptop"));

    let (status, started) = w
        .post(
            "job_start",
            &worker,
            json!({"code": "echo first; sleep 1; echo second"}),
        )
        .await;
    assert_eq!(
        (status, &started["status"]),
        (200, &json!("started")),
        "{started}"
    );
    let (_, ended) = w
        .post(
            "job_wait",
            &worker,
            json!({"job": started["job"], "wait_s": 20}),
        )
        .await;
    assert_eq!(ended["status"], "exited", "{ended}");
    assert_eq!(ended["exit_code"], 0, "{ended}");
    assert_eq!(ended["stdout"], "first\nsecond\n", "{ended}");

    let marker = new_marker();
    let code = format!("echo up; exec -a {marker} sleep 300");
    let (_, started) = w.post("job_start", &worker, json!({"code": code})).await;
    let job = started["job"].clone();
    until_running(&marker).await;
    let (_, running) = w
        .post("job_wait", &worker, json!({"job": job, "wait_s": 3}))
        .await;
    assert_eq!(running["status"], "running", "{running}");
    assert_eq!(running["stdout"], "up\n", "{running}");
    let (_, stopped) = w.post("job_stop", &worker, json!({"job": job})).await;
    assert_eq!(stopped["status"], "stopped", "{stopped}");
    until_gone(&marker).await;

    let (_, moved) = w
        .post("run_code", &worker, json!({"code": "sleep 7; echo late"}))
        .await;
    assert_eq!(
        moved["status"], "running",
        "past background_after it is a job: {moved}"
    );
    let (_, late) = w
        .post(
            "job_wait",
            &worker,
            json!({"job": moved["job"], "wait_s": 20}),
        )
        .await;
    assert_eq!(
        (&late["status"], &late["stdout"]),
        (&json!("exited"), &json!("late\n")),
        "{late}"
    );
}

/// `solos-swarm mcp` for the worker `token`, as Claude Code starts it: the
/// answers to `tools/list` and then to each `tools/call` of `calls`.
async fn through_mcp(w: &World, token: &str, calls: &[(&str, Value)]) -> Vec<Value> {
    let token_file = w.root.join(format!("{token}.token"));
    std::fs::write(&token_file, token).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_solos-swarm"))
        .arg("mcp")
        .arg("--socket")
        .arg(w.root.join("state").join(SOCKET_FILE))
        .arg("--token-file")
        .arg(&token_file)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut requests = vec![
        json!({"method": "initialize", "params": {"protocolVersion": "2025-06-18",
               "capabilities": {}, "clientInfo": {"name": "test-host", "version": "1"}}}),
        json!({"method": "tools/list", "params": {}}),
    ];
    for (name, arguments) in calls {
        requests.push(json!({"method": "tools/call",
                             "params": {"name": name, "arguments": arguments}}));
    }
    let mut answers = Vec::new();
    for (id, mut request) in requests.into_iter().enumerate() {
        request["jsonrpc"] = json!("2.0");
        request["id"] = json!(id);
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let answer = tokio::time::timeout(LIMIT, async {
            loop {
                let line = lines.next_line().await.unwrap().expect("mcp exited");
                let reply: Value = serde_json::from_str(&line).unwrap();
                if reply["id"] == id {
                    return reply;
                }
            }
        })
        .await
        .expect("mcp did not answer");
        answers.push(answer["result"].clone());
    }
    answers.remove(0);
    answers
}

#[tokio::test]
async fn a_claude_code_workers_tools_list_and_run_on_its_node() {
    let mut w = World::new("mcp", SecretsConfig::default()).await;
    let there = w.worker("tk_mcp", Some("laptop"));
    let here = w.worker("tk_mcp_here", None);
    let names = |listed: &Value| -> Vec<String> {
        listed["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{listed}"))
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let on_the_node = [
        "worker_report",
        "worker_handback",
        "run_code",
        "job_start",
        "job_wait",
        "job_stop",
        "fetch_url",
    ];
    assert_eq!(w.broker.worker_tools(Some("laptop"), false), on_the_node);
    let listed = through_mcp(&w, &here, &[]).await;
    assert_eq!(
        names(&listed[0]),
        w.broker.worker_tools(None, false),
        "this host's"
    );
    let calls = if sandbox() {
        vec![
            ("run_code", json!({"code": "echo from-mcp | tee mcp.txt"})),
            ("job_start", json!({"code": "cat mcp.txt"})),
        ]
    } else {
        Vec::new()
    };
    let answers = through_mcp(&w, &there, &calls).await;
    assert_eq!(names(&answers[0]), on_the_node, "run_code on the node");
    if sandbox() {
        let text = |answer: &Value| -> Value {
            assert_eq!(answer["isError"], false, "{answer}");
            serde_json::from_str(answer["content"][0]["text"].as_str().unwrap()).unwrap()
        };
        let receipt = text(&answers[1]);
        assert_eq!(
            (&receipt["exit_code"], &receipt["stdout"]),
            (&json!(0), &json!("from-mcp\n")),
            "{receipt}"
        );
        assert!(w.node_work("tk_mcp").join("mcp.txt").is_file());
        let job = text(&answers[2]);
        let (_, ended) = w
            .post("job_wait", &there, json!({"job": job["job"], "wait_s": 20}))
            .await;
        assert_eq!(
            ended["stdout"], "from-mcp\n",
            "the job ran on the node: {ended}"
        );
    }
    w.drop_node().await;
    let answers = through_mcp(&w, &there, &[("run_code", json!({"code": "true"}))]).await;
    assert_eq!(answers[1]["isError"], true, "{}", answers[1]);
    let why = answers[1]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(why.contains("node laptop is offline"), "{why}");
}

#[tokio::test]
async fn a_node_going_offline_mid_request_fails_the_call_cleanly() {
    if !sandbox() {
        return;
    }
    let mut w = World::new("offline", SecretsConfig::default()).await;
    let worker = w.worker("tk_off", Some("laptop"));
    let job_marker = new_marker();
    let (_, job) = w
        .post(
            "job_start",
            &worker,
            json!({"code": format!("exec -a {job_marker} sleep 300")}),
        )
        .await;
    let marker = new_marker();
    let code = json!({"code": format!("exec -a {marker} sleep 300")});
    let call = {
        let (client, worker) = (w.client.clone(), worker.clone());
        tokio::spawn(async move {
            let response = client
                .post("http://broker/tool/run_code")
                .bearer_auth(worker)
                .json(&code)
                .send()
                .await
                .unwrap();
            let status = response.status().as_u16();
            (
                status,
                response.json::<Value>().await.unwrap_or(Value::Null),
            )
        })
    };
    until_running(&marker).await;
    until_running(&job_marker).await;
    let dropped = Instant::now();
    w.drop_node().await;
    let (status, failed) = tokio::time::timeout(Duration::from_secs(10), call)
        .await
        .expect("the call hung after its node dropped")
        .unwrap();
    assert!(dropped.elapsed() < Duration::from_secs(5));
    assert_eq!(status, 422, "{failed}");
    let why = failed["error"].as_str().unwrap_or_default();
    assert!(why.contains("node laptop went offline"), "{failed}");
    let (_, ended) = w
        .post(
            "job_wait",
            &worker,
            json!({"job": job["job"], "wait_s": 10}),
        )
        .await;
    assert_eq!(ended["status"], "failed", "{ended}");
    assert!(
        ended["error"]
            .as_str()
            .unwrap_or_default()
            .contains("went offline"),
        "{ended}"
    );
    until_gone(&marker).await;
    until_gone(&job_marker).await;

    let (status, offline) = w.post("run_code", &worker, json!({"code": "true"})).await;
    assert_eq!(status, 503, "{offline}");
    assert!(
        offline["error"]
            .as_str()
            .unwrap_or_default()
            .contains("node laptop is offline"),
        "{offline}"
    );
    let (status, _) = w.post("job_start", &worker, json!({"code": "true"})).await;
    assert_eq!(status, 503);
}

#[tokio::test]
async fn node_secrets_reach_the_sandbox_but_never_a_receipt() {
    if !sandbox() {
        return;
    }
    let root = scratch("secret-files");
    std::fs::write(root.join("token"), format!("{TOKEN}\n")).unwrap();
    std::fs::write(root.join("file"), format!("{FILE_SECRET}\n")).unwrap();
    let secrets = SecretsConfig {
        env: BTreeMap::from([("API_TOKEN".to_owned(), root.join("token"))]),
        files: BTreeMap::from([("deploy".to_owned(), root.join("file"))]),
    };
    let w = World::new("secrets", secrets).await;
    let worker = w.worker("tk_secret", Some("laptop"));
    let mut seen = String::new();

    let code = "echo \"$API_TOKEN\"; cat /secrets/deploy; printf %s \"$API_TOKEN\" | wc -c; \
                env | grep ^API_TOKEN=; echo \"$API_TOKEN\" >&2; echo \"$API_TOKEN\" > kept.txt";
    let (status, receipt) = w.post("run_code", &worker, json!({"code": code})).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(
        receipt["stdout"],
        "[redacted API_TOKEN]\n[redacted deploy]\n18\nAPI_TOKEN=[redacted API_TOKEN]\n",
        "{receipt}"
    );
    assert_eq!(receipt["stderr"], "[redacted API_TOKEN]\n", "{receipt}");
    seen.push_str(&receipt.to_string());
    assert_eq!(
        std::fs::read_to_string(w.node_work("tk_secret").join("kept.txt")).unwrap(),
        format!("{TOKEN}\n"),
        "the sandbox had the value itself"
    );

    let split = "printf %s \"${API_TOKEN:0:7}\"; sleep 4; printf '%s\\n' \"${API_TOKEN:7}\"";
    let (_, started) = w.post("job_start", &worker, json!({"code": split})).await;
    let job = started["job"].clone();
    let (_, early) = w
        .post("job_wait", &worker, json!({"job": job, "wait_s": 3}))
        .await;
    assert_eq!(early["status"], "running", "{early}");
    seen.push_str(&early.to_string());
    let (_, ended) = w
        .post("job_wait", &worker, json!({"job": job, "wait_s": 20}))
        .await;
    assert_eq!(ended["stdout"], "[redacted API_TOKEN]\n", "{ended}");
    seen.push_str(&ended.to_string());

    assert!(
        !seen.contains(TOKEN) && !seen.contains(FILE_SECRET) && !seen.contains("s3cr3t"),
        "a secret reached the agent: {seen}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn router_decide_names_only_this_host_or_a_configured_node() {
    let w = World::new("decide", SecretsConfig::default()).await;
    let router = Grant {
        principal: Principal::Router,
        account: Account::Router {
            daily_microusd: 1_000_000,
        },
        terms: terms(),
        system_prompt: None,
    };
    w.broker.admit("router-token".into(), router).unwrap();
    let accept = |environment: &str| {
        json!({"eval": "ev_1", "decision": "accept", "reason": "asked", "title": "T",
               "scope": "S", "environment": environment})
    };
    let (status, refused) = w
        .post("router_decide", "router-token", accept("cloud"))
        .await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(
        refused["error"],
        "there is no environment cloud; the environments are local, desktop, laptop"
    );
    let mut swarm = accept("laptop");
    swarm["plan"] = json!("swarm");
    swarm["agents"] = json!(2);
    let (status, refused) = w.post("router_decide", "router-token", swarm).await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["error"], "environment is for a session plan only");
    let mut on_a_repo = accept("laptop");
    on_a_repo["repo"] = json!("acme/widgets");
    let (status, refused) = w.post("router_decide", "router-token", on_a_repo).await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(
        refused["error"],
        "a ticket on a repository runs on this host only; name environment local"
    );
}

#[tokio::test]
async fn a_wrong_or_unpinned_certificate_is_refused_on_either_side() {
    let root = scratch("pins");
    let pki = Pki::new(&root);
    tls::init(&root.join("stranger-tls"), tls::NODE_NAME).unwrap();
    let listener = Listener::start(&pki.nodes("127.0.0.1:0")).await.unwrap();
    let address = listener.local_addr().to_string();
    let laptop = listener.environments()["laptop"].clone();
    let desktop = listener.environments()["desktop"].clone();

    for (case, config) in [
        (
            "a node certificate the agent does not pin",
            pki.node("laptop", "stranger-tls", pki.agent, address.clone()),
        ),
        (
            "an agent certificate the node does not pin",
            pki.node("laptop", "laptop-tls", pki.desktop, address.clone()),
        ),
        (
            "a pinned certificate that names another node",
            pki.node("desktop", "laptop-tls", pki.agent, address.clone()),
        ),
    ] {
        let refused = tokio::time::timeout(LIMIT, Node::new(&config).unwrap().session())
            .await
            .expect("the handshake hung");
        assert!(refused.is_err(), "{case}: connected");
        assert!(
            laptop.ready().is_err() && desktop.ready().is_err(),
            "{case}"
        );
    }
    let named = Node::new(&pki.node("desktop", "laptop-tls", pki.agent, address.clone()))
        .unwrap()
        .session()
        .await
        .unwrap_err();
    assert!(
        format!("{named:#}").contains("this certificate is node laptop's, not desktop's"),
        "{named:#}"
    );

    let node = connect(&pki.node("laptop", "laptop-tls", pki.agent, address));
    until_ready(laptop.as_ref()).await;
    assert!(desktop.ready().is_err());
    node.abort();
    until_offline(laptop.as_ref()).await;
    listener.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn oversized_frames_are_refused_on_either_side() {
    let root = scratch("frames");
    let pki = Pki::new(&root);
    let listener = Listener::start(&pki.nodes("127.0.0.1:0")).await.unwrap();
    let laptop = listener.environments()["laptop"].clone();

    // A node that, once welcomed, sends a frame above the limit.
    let (cert, key) = tls::files(&root.join("laptop-tls"));
    let identity = Identity::load(&cert, &key).unwrap();
    let connector =
        tokio_rustls::TlsConnector::from(tls::client_config(&identity, pki.agent).unwrap());
    let tcp = tokio::net::TcpStream::connect(listener.local_addr())
        .await
        .unwrap();
    let mut stream = connector
        .connect(tls::agent_name().unwrap(), tcp)
        .await
        .unwrap();
    let hello = Hello {
        name: "laptop".into(),
        protocol: PROTOCOL,
    };
    wire::send(&mut stream, &hello).await.unwrap();
    let welcome: Option<Welcome> = wire::receive(&mut stream).await.unwrap();
    assert_eq!(welcome, Some(Welcome::Accepted));
    until_ready(laptop.as_ref()).await;
    let header = u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes();
    stream.write_all(&header).await.unwrap();
    stream.flush().await.unwrap();
    until_offline(laptop.as_ref()).await;
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(LIMIT, stream.read_to_end(&mut rest)).await;

    // An agent that, once it welcomed the node, sends one.
    let agent = {
        let (cert, key) = tls::files(&root.join("agent-tls"));
        Identity::load(&cert, &key).unwrap()
    };
    let acceptor =
        tokio_rustls::TlsAcceptor::from(tls::server_config(&agent, vec![pki.laptop]).unwrap());
    let fake = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = fake.local_addr().unwrap().to_string();
    let serving = tokio::spawn(async move {
        let (tcp, _) = fake.accept().await.unwrap();
        let mut stream = acceptor.accept(tcp).await.unwrap();
        let _: Option<Hello> = wire::receive(&mut stream).await.unwrap();
        wire::send(&mut stream, &Welcome::Accepted).await.unwrap();
        stream.write_all(&header).await.unwrap();
        stream.flush().await.unwrap();
        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest).await;
    });
    let node = Node::new(&pki.node("laptop", "laptop-tls", pki.agent, address)).unwrap();
    let refused = tokio::time::timeout(LIMIT, node.session())
        .await
        .expect("the node kept an oversized frame's connection")
        .unwrap_err();
    assert!(format!("{refused:#}").contains("above the"), "{refused:#}");
    let _ = tokio::time::timeout(LIMIT, serving).await;

    // A request too large for one frame is refused before it is sent.
    let node = connect(&pki.node(
        "laptop",
        "laptop-tls",
        pki.agent,
        listener.local_addr().to_string(),
    ));
    until_ready(laptop.as_ref()).await;
    let ticket = TicketId::new("tk_big");
    let work = Work {
        ticket: &ticket,
        dir: &root,
    };
    let controls: String = "\u{1}".repeat(MAX_FILES_BYTES);
    let big = json!({"code": "true", "files": {"blob.txt": controls}});
    let refused = laptop
        .run_code(work, &big)
        .err()
        .expect("an oversized request was sent");
    assert!(
        refused.to_string().contains("a node takes at most"),
        "{refused}"
    );
    assert!(
        laptop.ready().is_ok(),
        "a refused request leaves the node connected"
    );
    node.abort();
    listener.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// The scripted model of the workers: the first prompt of each Ticket gets
/// `calls`, a tool result ends the turn.
struct Model {
    calls: Vec<(&'static str, Value)>,
}

impl Provider for Model {
    fn dispatch(&self, body: Value) -> Dispatch {
        let last = body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .cloned()
            .unwrap_or(Value::Null);
        let calls: Vec<Value> = if last["role"] == "user" {
            self.calls
                .iter()
                .map(|(name, arguments)| {
                    json!({"id": format!("call-{name}"), "type": "function",
                           "function": {"name": name, "arguments": arguments.to_string()}})
                })
                .collect()
        } else {
            Vec::new()
        };
        let (message, finish) = if calls.is_empty() {
            (json!({"role": "assistant", "content": "Done."}), "stop")
        } else {
            (
                json!({"role": "assistant", "content": null, "tool_calls": calls}),
                "tool_calls",
            )
        };
        Box::pin(async move {
            Ok(
                json!({"id": "model-1", "created": 1, "model": "deepseek-flash",
                      "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                      "usage": {"prompt_tokens": 100, "prompt_cache_hit_tokens": 0, "completion_tokens": 50}}),
            )
        })
    }
}

#[tokio::test]
async fn a_ticket_bound_to_an_offline_node_launches_once_it_connects() {
    let root = scratch("launch");
    let pki = Pki::new(&root);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let trigger = json!({"id": "t-ask", "source": "slack", "at_ms": 1_793_613_600_000u64,
        "actor": {"id": "alice", "kind": "person"}, "addressed": "mention",
        "text": "Count the lines on my laptop", "threads": ["T1"],
        "reply_to": [{"source": "slack", "address": {"thread_ts": "T1"}}]});
    std::fs::write(root.join("triggers.jsonl"), format!("{trigger}\n")).unwrap();
    let yaml = format!(
        "policy:\n  people: [slack:alice]\n\
         sources:\n  - {{kind: jsonl, path: triggers.jsonl}}\n\
         channels:\n  - {{kind: jsonl, source: slack, path: outbox.jsonl}}\n\
         router: {{kind: scripted}}\n\
         executor:\n  kind: pi\n  provider: deepseek\n  model: deepseek-flash\n  \
         prices: {{input_microusd_per_million: 300000, output_microusd_per_million: 1200000}}\n  \
         environment: laptop\n\
         budgets: {{session_microusd: 500000}}\n\
         nodes:\n  listen: 127.0.0.1:{port}\n  cert: agent-tls/cert.pem\n  key: agent-tls/key.pem\n  \
         allow:\n    laptop: {{fingerprint: \"{}\"}}\n",
        pki.laptop
    );
    std::fs::write(root.join("agent.yaml"), yaml).unwrap();
    let config = AgentConfig::load(&root.join("agent.yaml")).unwrap();
    let ran = sandbox();
    let mut calls = Vec::new();
    if ran {
        calls.push((
            "run_code",
            json!({"code": "echo from-laptop > here.txt; cat here.txt"}),
        ));
    }
    calls.push((
        "worker_report",
        json!({"status": "ready", "text": "Done on the laptop."}),
    ));
    std::fs::write(root.join("node"), "").unwrap();
    let host = PiHost {
        harness: ScriptedHarness::new(BTreeMap::new()).every_turn(vec![Call::Replied]),
        provider: Arc::new(Model { calls }) as Arc<dyn Provider>,
        pi_root: root.clone(),
        node: root.join("node"),
        browser: solos_swarm::browse::Launcher::Lightpanda,
    };
    let state = root.join("state");
    let serving = {
        let state = state.clone();
        tokio::spawn(async move { serve_with(config, &state, true, Some(host)).await })
    };

    // The launch fails while the node is offline, and retries.
    let deadline = Instant::now() + LIMIT;
    let waiting = loop {
        assert!(Instant::now() < deadline, "the launch never failed");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let Ok(store) = SqliteStore::open_read_only(state.join(STORE_FILE)) else {
            continue;
        };
        let failed = store
            .effects()
            .unwrap_or_default()
            .into_iter()
            .find(|queued| matches!(queued.effect, Effect::Launch { .. }) && queued.attempts > 0);
        if let Some(failed) = failed {
            break (failed, store.tickets().unwrap());
        }
    };
    let (failed, tickets) = waiting;
    assert!(!failed.done, "{failed:?}");
    assert!(
        failed
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("node laptop is offline"),
        "{failed:?}"
    );
    let [ticket] = &tickets[..] else {
        panic!("{tickets:?}")
    };
    assert_eq!(
        ticket.stage(),
        Stage::Queued,
        "nothing started without its node"
    );

    let node = Node::new(&pki.node(
        "laptop",
        "laptop-tls",
        pki.agent,
        format!("127.0.0.1:{port}"),
    ))
    .unwrap();
    let daemon = tokio::spawn(async move { node.serve(std::future::pending()).await });
    let summary = tokio::time::timeout(LIMIT, serving)
        .await
        .expect("serve did not go idle")
        .unwrap()
        .unwrap();
    daemon.abort();
    assert_eq!(summary.effects_pending, 0, "{summary:?}");
    assert!(summary.effect_failures >= 1, "{summary:?}");
    let store = SqliteStore::open(state.join(STORE_FILE)).unwrap();
    let ticket = store.tickets().unwrap().remove(0);
    assert_eq!(
        ticket.ended.as_ref().map(|ended| ended.outcome),
        Some(TicketOutcome::Completed)
    );
    if ran {
        let here = root
            .join("laptop-work")
            .join(ticket.id.as_str())
            .join("here.txt");
        assert_eq!(std::fs::read_to_string(here).unwrap(), "from-laptop\n");
        let agent_side = state
            .join(TICKETS_DIR)
            .join(ticket.id.as_str())
            .join("work/here.txt");
        assert!(!agent_side.exists(), "run_code ran on the agent's host");
    }
    let _ = std::fs::remove_dir_all(&root);
}
