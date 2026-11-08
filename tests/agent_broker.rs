//! The agent broker over HTTP on its Unix socket, with a scripted provider:
//! the Router account's holds and settlements, its daily cap across a
//! restart, the pause gate, identities, the two Router tools against a
//! running runtime, and a worker's account and tools.

use anyhow::Result;
use serde_json::{Value, json};
use solos_agent_core::memo::{MemoAudience, MemoAuthor, MemoIn, MemoScope, MemoSource};
use solos_agent_core::model::{
    Actor, Addressed, Approval, AssignmentId, Candidate, ChannelRef, DecisionKind, DomainEvent,
    InputBody, MessageKind, OutboundMessage, Plan, ReportStatus, ThreadKey, TicketId, Trigger,
    TriggerId,
};
use solos_agent_core::{AllowlistPolicy, Engine};
use solos_agent_store::{SpendState, SqliteStore};
use solos_swarm::agent::broker::browser::{BrowserParts, LocalBrowser};
use solos_swarm::agent::broker::memory::{DreamTerms, MemoryParts};
use solos_swarm::agent::broker::registry::{self, PrincipalKind};
use solos_swarm::agent::broker::review::{Effort, ReviewTerms};
use solos_swarm::agent::broker::{
    self, Account, AgentBroker, BrokerParts, DecideTerms, Grant, ModelFailure, ModelTerms,
    Principal, ROUTER_TOOLS, SwarmTerms, VaultParts, WORKER_TOOLS, WorkerParts, WorkerSeat,
};
use solos_swarm::agent::environment::{ExecutionEnvironment, Nodes, Offline, Work};
use solos_swarm::agent::jobs::JobLimits;
use solos_swarm::agent::jsonl::{JsonlChannel, JsonlSource};
use solos_swarm::agent::ports::{
    BoxFuture, Channel, Delivery, Executor, Fact, FactSink, Launch, Router,
};
use solos_swarm::agent::pstack::Pstack;
use solos_swarm::agent::scripted::{ScriptedExecutor, ScriptedRouter};
use solos_swarm::agent::trail::{BlobStore, TrailStoreConfig};
use solos_swarm::agent::watchdog::usd;
use solos_swarm::agent::{Adapters, AgentConfig, RunOptions, Runtime, STORE_FILE};
use solos_swarm::browse::fake::FakeCdp;
use solos_swarm::browse::{BrowserConfig, Browsers, Launcher, TrailSigner};
use solos_swarm::code::{CodeConfig, CodeExecutor, OutputBounds, Started};
use solos_swarm::gateway::{Dispatch, PriceTable, Provider, Temperature, Upstream, upstream_body};
use solos_swarm::vault::{SandboxView, Vault, VaultConfig};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, oneshot};

const TOKEN: &str = "router-token";
const MAX_OUTPUT: u64 = 2_048;
const PRICES: PriceTable = PriceTable {
    input_microusd_per_million: 300_000,
    output_microusd_per_million: 1_200_000,
    cache_hit_microusd_per_million: Some(6_000),
};
/// 100 prompt tokens and 50 completion tokens at `PRICES`.
const COST: u64 = 90;
const LIMIT: Duration = Duration::from_secs(30);
/// A broker's accept terms. The policy names no repository: these tests
/// accept none.
fn decide_terms() -> DecideTerms {
    DecideTerms {
        default_budget_microusd: Some(500_000),
        max_budget_microusd: Some(2_000_000),
        approval_sources: BTreeSet::new(),
        auto_approve_microusd: 0,
        answer_sources: BTreeSet::new(),
        swarm: None,
        repos: BTreeMap::new(),
        models: solos_swarm::agent::models::ModelPolicy::default(),
        router_provider: None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Behaviour {
    Answer,
    /// A receipt for far more than any hold.
    Overrun,
    Fail,
    NoReceipt,
    Hang,
    Slow,
}

const SLOW: Duration = Duration::from_millis(600);

/// Answers every call with a fixed completion, and keeps what it was sent.
struct Scripted {
    behaviour: Mutex<Behaviour>,
    bodies: Mutex<Vec<Value>>,
}

impl Scripted {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            behaviour: Mutex::new(Behaviour::Answer),
            bodies: Mutex::new(Vec::new()),
        })
    }
    fn set(&self, behaviour: Behaviour) {
        *self.behaviour.lock().unwrap() = behaviour;
    }
    fn calls(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }
}

impl Provider for Scripted {
    fn dispatch(&self, body: Value) -> Dispatch {
        self.bodies.lock().unwrap().push(body);
        let behaviour = *self.behaviour.lock().unwrap();
        Box::pin(async move {
            let usage =
                json!({"prompt_tokens":100,"prompt_cache_hit_tokens":0,"completion_tokens":50});
            let usage = if behaviour == Behaviour::Overrun {
                json!({"prompt_tokens":1_000_000,"prompt_cache_hit_tokens":0,"completion_tokens":50})
            } else {
                usage
            };
            match behaviour {
                Behaviour::Answer | Behaviour::Overrun | Behaviour::Slow => {
                    if behaviour == Behaviour::Slow {
                        tokio::time::sleep(SLOW).await;
                    }
                    Ok(
                        json!({"id":"receipt-1","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":usage}),
                    )
                }
                Behaviour::Hang => std::future::pending().await,
                Behaviour::Fail => Err(anyhow::anyhow!(
                    "provider transport failure; outcome unknown"
                )),
                Behaviour::NoReceipt => Ok(json!({"choices":[],"usage":usage})),
            }
        })
    }
}

/// Keeps each Candidate for the test to decide over HTTP.
#[derive(Default)]
struct Holding {
    candidates: Mutex<Vec<Candidate>>,
    arrived: Notify,
}

impl Router for Holding {
    fn assess(&self, candidate: Candidate, _: FactSink) -> BoxFuture<'_, Result<()>> {
        self.candidates.lock().unwrap().push(candidate);
        self.arrived.notify_one();
        Box::pin(async { Ok(()) })
    }
}

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agent-broker-{name}-{}-{}",
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
    fn triggers(&self) -> PathBuf {
        self.root.join("triggers.jsonl")
    }
    fn append(&self, trigger: &Trigger) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.triggers())
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(trigger).unwrap()).unwrap();
    }
    fn sent(&self) -> Vec<OutboundMessage> {
        std::fs::read_to_string(self.root.join("outbox.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn runtime(&self, router: Arc<dyn Router>, until_idle: bool) -> Runtime {
        let policy = AllowlistPolicy {
            people: ["slack:alice".to_owned()].into(),
            ..AllowlistPolicy::default()
        };
        self.runtime_under(policy, router, until_idle)
    }
    fn runtime_under(
        &self,
        policy: AllowlistPolicy,
        router: Arc<dyn Router>,
        until_idle: bool,
    ) -> Runtime {
        self.runtime_with(
            policy,
            router,
            Arc::new(ScriptedExecutor),
            until_idle,
            false,
        )
    }
    /// `follow`: the triggers file is read again as it grows.
    fn runtime_with(
        &self,
        policy: AllowlistPolicy,
        router: Arc<dyn Router>,
        executor: Arc<dyn Executor>,
        until_idle: bool,
        follow: bool,
    ) -> Runtime {
        let channel: Arc<dyn Channel> =
            Arc::new(JsonlChannel::open(self.root.join("outbox.jsonl")).unwrap());
        let adapters = Adapters {
            sources: vec![Box::new(JsonlSource::new(self.triggers(), follow))],
            channels: BTreeMap::from([("slack".to_owned(), channel)]),
            router,
            executor,
        };
        let options = RunOptions {
            until_idle,
            tick: Duration::from_millis(10),
            ..RunOptions::default()
        };
        Runtime::new(self.store(), policy, adapters, options).with_control_dir(self.state())
    }
    /// An agent config whose Pi Router decides through this broker: with a
    /// Slack channel and a `slack:` approver when `slack`, else a jsonl
    /// outbox that cannot take an approval. Only its Policy and terms are
    /// used; the runtime keeps this scratch's jsonl adapters.
    fn config(&self, slack: bool) -> AgentConfig {
        let budgets = "budgets: {session_microusd: 500000, router_daily_microusd: 1000000}\n";
        self.config_under(slack, budgets)
    }
    /// [`config`](Self::config) with `budgets` as its last lines, which may
    /// be none.
    fn config_under(&self, slack: bool, budgets: &str) -> AgentConfig {
        let io = if slack {
            "sources:\n  - {kind: slack, channels: [C1], app_token_file: app, bot_token_file: bot}\n\
             channels:\n  - {kind: slack}\n"
        } else {
            "sources:\n  - {kind: jsonl, path: triggers.jsonl}\n\
             channels:\n  - {kind: jsonl, source: slack, path: outbox.jsonl}\n"
        };
        let yaml = format!(
            "policy:\n  people: [slack:alice]\n  approvers: [slack:boss]\n{io}\
             router:\n  kind: pi\n  provider: deepseek\n  model: deepseek-flash\n  \
             prices: {{input_microusd_per_million: 300000, output_microusd_per_million: 1200000}}\n\
             executor: {{kind: scripted}}\n{budgets}"
        );
        let path = self.root.join("agent.yaml");
        std::fs::write(&path, yaml).unwrap();
        AgentConfig::load(&path).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn request(id: &str, thread: &str, text: &str) -> Trigger {
    Trigger {
        id: TriggerId::new(id),
        source: "slack".into(),
        at_ms: 1_000,
        actor: Actor::person("alice"),
        addressed: Addressed::Mention,
        text: text.into(),
        threads: vec![ThreadKey::new(thread)],
        reply_to: vec![ChannelRef {
            source: "slack".into(),
            address: BTreeMap::from([("thread_ts".to_owned(), thread.to_owned())]),
        }],
        meta: json!({}),
    }
}

fn terms() -> ModelTerms {
    ModelTerms {
        upstream: Upstream::deepseek("deepseek-flash"),
        prices: PRICES,
        max_input_bytes: 65_536,
        max_output_tokens: MAX_OUTPUT,
        call_limit: None,
        temperature: None,
        thinking: None,
    }
}

fn grant(daily_microusd: u64) -> Grant {
    Grant {
        principal: Principal::Router,
        account: Account::Router {
            daily_microusd: Some(daily_microusd),
        },
        terms: terms(),
        system_prompt: Some("You are the Router.".into()),
    }
}

/// A broker serving on the state directory's socket, and a client for it.
struct Served {
    server: tokio::task::JoinHandle<()>,
    client: reqwest::Client,
    broker: AgentBroker,
}

impl Served {
    async fn post(&self, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
        let mut request = self
            .client
            .post(format!("http://broker/{path}"))
            .json(&body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }
    async fn model(&self, text: &str) -> (u16, Value) {
        self.post("model", Some(TOKEN), model_request(text)).await
    }
    /// The names `GET /tools` lists for `token`.
    async fn listed(&self, token: &str) -> Vec<String> {
        let response = self
            .client
            .get("http://broker/tools")
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let listed: Value = response.json().await.unwrap();
        listed["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{listed}"))
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    }
    /// Stop serving and close the socket, as a process exit would.
    async fn stop(mut self) {
        self.server.abort();
        let _ = (&mut self.server).await;
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn model_request(text: &str) -> Value {
    json!({"model":"deepseek-flash","messages":[{"role":"user","content":text}],
           "tools":[{"type":"function","function":{"name":"router_decide","parameters":{"type":"object"}}}]})
}

fn serve(scratch: &Scratch, provider: Arc<Scripted>, facts: FactSink, daily: u64) -> Served {
    serve_deciding(scratch, provider, facts, daily, decide_terms())
}

fn serve_deciding(
    scratch: &Scratch,
    provider: Arc<Scripted>,
    facts: FactSink,
    daily: u64,
    decide: DecideTerms,
) -> Served {
    serve_on(
        scratch,
        provider,
        facts,
        daily,
        decide,
        Nodes::default(),
        workers(None, None),
        None,
    )
}

fn workers(vault: Option<VaultParts>, browser: Option<BrowserParts>) -> WorkerParts {
    WorkerParts {
        terms: terms(),
        code: None,
        vault,
        browser,
        // The pages these tests fetch are on loopback.
        allow_private_networks: true,
    }
}

#[allow(clippy::too_many_arguments)]
fn serve_on(
    scratch: &Scratch,
    provider: Arc<Scripted>,
    facts: FactSink,
    daily: u64,
    decide: DecideTerms,
    nodes: Nodes,
    workers: WorkerParts,
    pstack: Option<Arc<Pstack>>,
) -> Served {
    let parts = BrokerParts {
        dir: scratch.state(),
        store: scratch.store(),
        provider,
        facts,
        decide,
        workers: Some(workers),
    };
    let broker = AgentBroker::with_pstack(parts, JobLimits::default(), nodes, pstack).unwrap();
    broker.admit(TOKEN.into(), grant(daily)).unwrap();
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
        server,
        client,
        broker,
    }
}

/// A broker whose runtime never runs: enough for model calls.
fn model_only(scratch: &Scratch, provider: Arc<Scripted>, daily: u64) -> (Runtime, Served) {
    let runtime = scratch.runtime(Arc::new(Holding::default()), true);
    let served = serve(scratch, provider, runtime.facts(), daily);
    (runtime, served)
}

fn hold_for(text: &str) -> u64 {
    let bytes = serde_json::to_vec(&model_request(text)).unwrap().len();
    PRICES.request_hold(bytes, MAX_OUTPUT)
}

#[tokio::test]
async fn a_router_call_is_held_then_settled_against_the_router_account() {
    let scratch = Scratch::new("settle");
    let provider = Scripted::new();
    let (_runtime, served) = model_only(&scratch, provider.clone(), 1_000_000);

    let (status, reply) = served.model("judge eval ev_1").await;

    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["swarm_cost_microusd"], COST);
    assert_eq!(reply["id"], "receipt-1");
    let sent = provider.bodies.lock().unwrap().clone();
    assert_eq!(
        sent,
        [upstream_body(
            "deepseek-flash",
            &model_request("judge eval ev_1"),
            MAX_OUTPUT
        )]
    );
    let rows = scratch.store().spend("router").unwrap();
    let [row] = &rows[..] else { panic!("{rows:?}") };
    assert_eq!(row.state, SpendState::Settled);
    assert_eq!(row.hold_microusd, hold_for("judge eval ev_1"));
    assert_eq!(row.cost_microusd, COST);
    assert_eq!(row.provider, "deepseek");
    assert_eq!(row.request_id.as_deref(), Some("receipt-1"));
    assert_eq!(
        row.tokens.map(|t| (t.input, t.cached_input, t.output)),
        Some((100, 0, 50))
    );
}

#[tokio::test]
async fn the_daily_cap_refuses_and_still_refuses_after_a_restart() {
    let scratch = Scratch::new("cap");
    let provider = Scripted::new();
    // One call fits; a second hold on top of the first call's cost does not.
    let cap = hold_for("judge") + COST - 1;
    {
        let (_runtime, served) = model_only(&scratch, provider.clone(), cap);
        assert_eq!(served.model("judge").await.0, 200);
        let (status, reply) = served.model("judge").await;
        assert_eq!(status, 403);
        let why = reply["error"].as_str().unwrap();
        assert!(why.contains("daily spend cap"), "{why}");
        assert!(why.contains(&format!("{COST} of {cap}")), "{why}");
        served.stop().await;
    }

    let (_runtime, restarted) = model_only(&scratch, provider.clone(), cap);
    let (status, reply) = restarted.model("judge").await;

    assert_eq!(status, 403, "{reply}");
    assert_eq!(provider.calls(), 1, "no refused call reached the provider");
    let rows = scratch.store().spend("router").unwrap();
    assert_eq!(rows.len(), 1, "a refusal holds nothing");
    assert_eq!(scratch.store().committed("router", 0).unwrap(), COST);
}

#[tokio::test]
async fn a_pause_in_the_state_dir_refuses_model_calls_with_the_swarm_reply() {
    let scratch = Scratch::new("pause");
    let provider = Scripted::new();
    let (_runtime, served) = model_only(&scratch, provider.clone(), 1_000_000);

    std::fs::write(scratch.state().join("PAUSE"), "").unwrap();
    let (status, reply) = served.model("judge").await;
    assert_eq!(status, 503);
    assert_eq!(
        reply,
        json!({"paused":true,"retry_after_s":60,"reason":"model calls are paused (operator pause or peak window)"})
    );
    std::fs::remove_file(scratch.state().join("PAUSE")).unwrap();
    std::fs::write(
        scratch.state().join("control.json"),
        r#"{"mode":"pause","reason":"test"}"#,
    )
    .unwrap();
    let (status, reply) = served.model("judge").await;
    assert_eq!((status, reply["retry_after_s"].clone()), (503, json!(30)));
    assert_eq!(provider.calls(), 0);
    assert!(scratch.store().spend("router").unwrap().is_empty());

    std::fs::remove_file(scratch.state().join("control.json")).unwrap();
    assert_eq!(served.model("judge").await.0, 200);
}

#[tokio::test]
async fn an_unknown_or_missing_token_is_refused_everywhere() {
    let scratch = Scratch::new("tokens");
    let provider = Scripted::new();
    let (_runtime, served) = model_only(&scratch, provider.clone(), 1_000_000);
    let read = json!({"id":"t1"});
    for (path, body) in [
        ("model", model_request("judge")),
        ("tool/router_read", read.clone()),
        ("session", json!({})),
    ] {
        for token in [None, Some("forged-token")] {
            let (status, reply) = served.post(path, token, body.clone()).await;
            assert_eq!(status, 403, "{path} {token:?}: {reply}");
        }
    }
    assert_eq!(provider.calls(), 0);
    let (status, reply) = served.post("session", Some(TOKEN), json!({})).await;
    assert_eq!(
        (status, reply),
        (200, json!({"system_prompt":"You are the Router."}))
    );
}

#[tokio::test]
async fn the_router_may_offer_and_call_only_its_two_tools() {
    let scratch = Scratch::new("grants");
    let provider = Scripted::new();
    let (_runtime, served) = model_only(&scratch, provider.clone(), 1_000_000);
    for tool in ["run_code", "fetch_url", "claim_task"] {
        let (status, _) = served
            .post(&format!("tool/{tool}"), Some(TOKEN), json!({}))
            .await;
        assert_eq!(status, 403, "{tool}");
        let mut offered = model_request("judge");
        offered["tools"][0]["function"]["name"] = json!(tool);
        let (status, reply) = served.post("model", Some(TOKEN), offered).await;
        assert_eq!(status, 403, "{tool}");
        assert_eq!(reply["error"], "unadmitted provider tool");
    }
    let mut wrong = model_request("judge");
    wrong["model"] = json!("deepseek-pro");
    assert_eq!(served.post("model", Some(TOKEN), wrong).await.0, 403);
    assert_eq!(provider.calls(), 0);
}

#[tokio::test]
async fn a_failed_call_releases_its_hold_and_an_unreadable_receipt_is_charged_at_it() {
    let scratch = Scratch::new("failures");
    let provider = Scripted::new();
    let (_runtime, served) = model_only(&scratch, provider.clone(), 1_000_000);
    provider.set(Behaviour::Fail);
    assert_eq!(served.model("judge").await.0, 403);
    provider.set(Behaviour::NoReceipt);
    assert_eq!(served.model("judge").await.0, 403);

    let rows = scratch.store().spend("router").unwrap();
    let closed: Vec<_> = rows.iter().map(|r| (r.state, r.cost_microusd)).collect();
    assert_eq!(
        closed,
        [
            (SpendState::Released, 0),
            (SpendState::Unknown, hold_for("judge"))
        ]
    );
}

#[tokio::test]
async fn a_call_past_its_limit_is_dropped_and_charged_its_hold_while_an_unlimited_token_waits() {
    let scratch = Scratch::new("limit");
    let provider = Scripted::new();
    let (_runtime, served) = model_only(&scratch, provider.clone(), 1_000_000);
    let limited = Grant {
        terms: ModelTerms {
            call_limit: Some(SLOW / 3),
            temperature: None,
            ..terms()
        },
        ..grant(1_000_000)
    };
    served.broker.admit("limited".into(), limited).unwrap();
    let on = &served;
    let ask = |token: &'static str| async move {
        let call = on.post("model", Some(token), model_request("judge"));
        tokio::time::timeout(SLOW * 10, call)
            .await
            .expect("the call was never dropped")
    };

    provider.set(Behaviour::Hang);
    let started = std::time::Instant::now();
    let (status, reply) = ask("limited").await;
    assert_eq!(status, 403, "{reply}");
    assert!(
        reply["error"].as_str().unwrap().contains("took longer"),
        "{reply}"
    );
    assert!(started.elapsed() < SLOW * 5, "{:?}", started.elapsed());
    assert_eq!(
        scratch.store().committed("router", 0).unwrap(),
        hold_for("judge"),
        "the provider may still bill a dropped call"
    );

    provider.set(Behaviour::Slow);
    assert_eq!(ask(TOKEN).await.0, 200, "a token without a limit waits");
    assert_eq!(ask("limited").await.0, 403, "a slow call is dropped");
    provider.set(Behaviour::Answer);
    assert_eq!(ask("limited").await.0, 200, "a prompt call goes through");

    let rows = scratch.store().spend("router").unwrap();
    let closed: Vec<_> = rows.iter().map(|r| (r.state, r.cost_microusd)).collect();
    assert_eq!(
        closed,
        [
            (SpendState::Unknown, hold_for("judge")),
            (SpendState::Settled, COST),
            (SpendState::Unknown, hold_for("judge")),
            (SpendState::Settled, COST)
        ]
    );
}

/// Run `body` against a live runtime, then stop it.
async fn with_runtime<F: Future<Output = ()>>(runtime: Runtime, body: F) {
    let (stop, stopped) = oneshot::channel::<()>();
    let run = runtime.run(async {
        let _ = stopped.await;
    });
    let test = async {
        body.await;
        let _ = stop.send(());
    };
    let (summary, ()) = tokio::time::timeout(LIMIT, async { tokio::join!(run, test) })
        .await
        .expect("the test did not finish");
    summary.unwrap();
}

#[tokio::test]
async fn router_decide_becomes_the_engines_decision_and_a_refused_one_is_a_tool_error() {
    let scratch = Scratch::new("decide");
    scratch.append(&request("t-ask", "T1", "Summarise the incident report"));
    let router = Arc::new(Holding::default());
    let runtime = scratch.runtime(router.clone(), false);
    let served = serve(&scratch, Scripted::new(), runtime.facts(), 1_000_000);

    with_runtime(runtime, async {
        router.arrived.notified().await;
        let eval = router.candidates.lock().unwrap()[0].eval.clone();
        let accept = json!({"eval":eval,"decision":"accept","reason":"alice asked",
                            "title":"Summarise the incident","scope":"Summarise the incident report"});

        let (status, answer) = served.post("tool/router_decide", Some(TOKEN), accept.clone()).await;

        assert_eq!(status, 200, "{answer}");
        assert_eq!(answer["kind"], "accept");
        let ticket = answer["ticket"].as_str().unwrap().to_owned();
        assert!(ticket.starts_with("tk"), "{answer}");

        let (status, refused) = served.post("tool/router_decide", Some(TOKEN), accept).await;
        assert_eq!(status, 409, "{refused}");
        let why = refused["error"].as_str().unwrap();
        assert!(why.contains(eval.as_str()), "{why}");
        assert!(why.contains("closed"), "{why}");
        assert!(why.contains("No eval is open now"), "{why}");

        let too_big = json!({"eval":eval,"decision":"accept","reason":"r","title":"T","scope":"S","budget_microusd":2_000_001});
        let (status, refused) = served.post("tool/router_decide", Some(TOKEN), too_big).await;
        assert_eq!(status, 422);
        assert!(refused["error"].as_str().unwrap().contains("approval"));

        // The accept launched work; wait until the scripted worker's result went out.
        for _ in 0..500 {
            if scratch.sent().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let kinds: Vec<_> = scratch.sent().iter().map(|m| m.kind).collect();
        assert_eq!(kinds, [MessageKind::Acknowledgement, MessageKind::Result]);
    })
    .await;

    let store = scratch.store();
    let decisions = store.decisions().unwrap();
    let [decision] = &decisions[..] else {
        panic!("{decisions:?}")
    };
    assert_eq!(decision.kind, DecisionKind::Accept);
    assert_eq!(decision.reason, "alice asked");
    assert_eq!(store.tickets().unwrap()[0].title, "Summarise the incident");
}

#[tokio::test]
async fn a_router_session_starts_with_the_global_memos_and_the_chain_records_it() {
    let scratch = Scratch::new("router-memos");
    let policy = AllowlistPolicy {
        people: ["slack:alice".to_owned()].into(),
        ..AllowlistPolicy::default()
    };
    let remember = |engine: &mut Engine<SqliteStore, AllowlistPolicy, fn() -> u64>,
                    scope: &str,
                    text: &str| {
        engine
            .propose_memo(MemoIn {
                scope: MemoScope::try_from(scope.to_owned()).unwrap(),
                text: text.into(),
                sources: vec![MemoSource::Operator],
                author: MemoAuthor::Operator {
                    source: "slack".into(),
                    by: Actor::person("alice"),
                },
                supersedes: None,
            })
            .unwrap()
    };
    let clock: fn() -> u64 = || 1_000;
    let mut engine = Engine::new(scratch.store(), policy, clock);
    let global = remember(&mut engine, "global", "Answer in French.");
    remember(&mut engine, "repo:acme/widgets", "Tests run with ./check.");
    drop(engine);

    // The runtime's Source reads the triggers file, which holds nothing.
    std::fs::write(scratch.triggers(), "").unwrap();
    let runtime = scratch.runtime(Arc::new(Holding::default()), false);
    let served = serve(&scratch, Scripted::new(), runtime.facts(), 1_000_000);
    with_runtime(runtime, async {
        let (status, session) = served.post("session", Some(TOKEN), json!({})).await;
        assert_eq!(status, 200, "{session}");
        let prompt = session["system_prompt"].as_str().unwrap();
        assert!(
            prompt.starts_with("You are the Router.\n\nMemos: "),
            "{prompt}"
        );
        assert!(
            prompt.ends_with(&format!("> [{global} · person] Answer in French.")),
            "{prompt}"
        );
        assert!(
            !prompt.contains("./check"),
            "a repository's memo is a worker's"
        );
    })
    .await;
    let shown: Vec<_> = scratch
        .store()
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.event {
            DomainEvent::MemosShown { to, memos, .. } => Some((to, memos)),
            _ => None,
        })
        .collect();
    assert_eq!(shown, [(MemoAudience::Router, vec![global])]);
}

async fn candidates(router: &Holding, count: usize) -> Vec<Candidate> {
    loop {
        let sent = router.candidates.lock().unwrap().clone();
        if sent.len() >= count {
            return sent;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn ignore(eval: &impl serde::Serialize) -> Value {
    json!({"eval":eval,"decision":"ignore","reason":"chatter"})
}

#[tokio::test]
async fn a_router_that_decides_a_decided_eval_again_is_told_which_evals_are_open_by_thread() {
    let scratch = Scratch::new("closed");
    scratch.append(&request("t-a", "T1", "Summarise the incident report"));
    scratch.append(&request("t-b", "T2", "Fix the flaky test"));
    let router = Arc::new(Holding::default());
    let runtime = scratch.runtime(router.clone(), false);
    let served = serve(&scratch, Scripted::new(), runtime.facts(), 1_000_000);

    with_runtime(runtime, async {
        let sent = candidates(&router, 2).await;
        let (first, second) = (&sent[0], &sent[1]);
        let decide = |eval| served.post("tool/router_decide", Some(TOKEN), ignore(eval));

        assert_eq!(decide(&first.eval).await.0, 200);
        let (status, refused) = decide(&first.eval).await;

        assert_eq!(status, 409, "{refused}");
        let why = refused["error"].as_str().unwrap();
        assert!(
            why.starts_with(&format!("refused: {} is closed", first.eval)),
            "{why}"
        );
        assert!(why.contains("Do not retry it"), "{why}");
        assert!(
            why.contains(&format!("{} (thread {})", second.eval, second.thread)),
            "{why}"
        );
        assert!(!why.contains(&format!("{} (thread", first.eval)), "{why}");

        assert_eq!(decide(&second.eval).await.0, 200);
        let (_, refused) = decide(&second.eval).await;
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("No eval is open now"),
            "{refused}"
        );
    })
    .await;
}

#[tokio::test]
async fn a_router_that_replays_an_expired_eval_finds_the_open_one_for_its_thread_in_the_refusal() {
    let scratch = Scratch::new("expired");
    scratch.append(&request("t-ask", "T1", "Summarise the incident report"));
    let router = Arc::new(Holding::default());
    let policy = AllowlistPolicy {
        people: ["slack:alice".to_owned()].into(),
        eval_timeout_ms: 500,
        ..AllowlistPolicy::default()
    };
    let runtime = scratch.runtime_under(policy, router.clone(), false);
    let served = serve(&scratch, Scripted::new(), runtime.facts(), 1_000_000);

    with_runtime(runtime, async {
        let sent = candidates(&router, 2).await;
        let (stale, current) = (&sent[0], &sent[1]);
        assert_ne!(stale.eval, current.eval);
        assert_eq!(stale.thread, current.thread, "the same triggers, a new id");

        let mut asked = 0;
        let mut eval = stale.eval.to_string();
        let decided = loop {
            asked += 1;
            let (status, answer) = served
                .post("tool/router_decide", Some(TOKEN), ignore(&eval))
                .await;
            if status == 200 {
                break answer;
            }
            assert_eq!(status, 409, "{answer}");
            let why = answer["error"].as_str().unwrap();
            eval = open_eval_on_thread(why, stale.thread.as_str())
                .unwrap_or_else(|| panic!("the refusal names no open eval on the thread: {why}"));
            assert!(asked < 3, "still refused after {asked} calls: {why}");
        };

        assert_eq!(asked, 2, "one refusal, then the eval it named");
        assert_eq!(decided["kind"], "ignore");
        assert_eq!(eval, current.eval.as_str());
    })
    .await;
}

fn open_eval_on_thread(refusal: &str, thread: &str) -> Option<String> {
    let listed = refusal.split("Evals open now: ").nth(1)?;
    let listed = listed.split(". ").next()?;
    listed.split(", ").find_map(|entry| {
        let (id, key) = entry.split_once(" (thread ")?;
        (key.trim_end_matches(')') == thread).then(|| id.to_owned())
    })
}

/// Above `auto_approve_microusd` a Ticket waits for a person's approval,
/// which only Slack's buttons take. The terms come from the config: without
/// Slack approvals the Router's accept above the threshold is refused; with
/// them it stands, and its Ticket waits.
#[tokio::test]
async fn an_accept_above_the_threshold_waits_for_approval_only_where_slack_can_give_it() {
    const GATED: u64 = 3_000_000;
    for slack in [false, true] {
        let scratch = Scratch::new("gated");
        let config = scratch.config(slack);
        assert!(GATED > config.policy.auto_approve_microusd);
        scratch.append(&request("t-ask", "T1", "Rerun the load test"));
        let router = Arc::new(Holding::default());
        let runtime = scratch.runtime_under(config.policy.clone(), router.clone(), false);
        let decide = DecideTerms::for_config(&config);
        let served = serve_deciding(
            &scratch,
            Scripted::new(),
            runtime.facts(),
            1_000_000,
            decide,
        );

        with_runtime(runtime, async {
            router.arrived.notified().await;
            let eval = router.candidates.lock().unwrap()[0].eval.clone();
            let gated = json!({"eval":eval,"decision":"accept","reason":"alice asked",
                               "title":"Load test","scope":"Rerun the load test",
                               "budget_microusd":GATED});

            let (status, answer) = served.post("tool/router_decide", Some(TOKEN), gated).await;

            if !slack {
                assert_eq!(status, 422, "{answer}");
                let why = answer["error"].as_str().unwrap();
                assert!(why.contains("without a person's approval"), "{why}");
                return;
            }
            assert_eq!(status, 200, "{answer}");
            assert_eq!(answer["kind"], "accept");
            for _ in 0..500 {
                if !scratch.sent().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;

        let tickets = scratch.store().tickets().unwrap();
        if !slack {
            assert!(tickets.is_empty(), "{tickets:?}");
            assert!(scratch.sent().is_empty());
            continue;
        }
        let [ticket] = &tickets[..] else {
            panic!("{tickets:?}")
        };
        assert_eq!(ticket.mandate.budget_microusd, Some(GATED));
        assert_eq!(ticket.approval, Approval::Waiting);
        let sent = scratch.sent();
        let [ask] = &sent[..] else { panic!("{sent:?}") };
        assert_eq!(ask.kind, MessageKind::Attention);
        assert!(ask.text.contains("needs approval first"), "{}", ask.text);
    }
}

/// With no `budgets`, a session has no budget and no approval, while a swarm
/// still needs a budget, and above the threshold it still waits for a person.
#[tokio::test]
async fn with_no_budgets_a_session_goes_uncapped_and_a_swarm_still_needs_a_budget_and_an_approval()
{
    let scratch = Scratch::new("uncapped-decide");
    let config = scratch.config_under(true, "");
    assert_eq!(config.budgets.session_microusd, None);
    scratch.append(&request("t-session", "T1", "Rerun the load test"));
    scratch.append(&request("t-swarm", "T2", "Survey every paper on swarms"));
    let router = Arc::new(Holding::default());
    let runtime = scratch.runtime_under(config.policy.clone(), router.clone(), false);
    let decide = DecideTerms {
        swarm: Some(SwarmTerms {
            templates: vec!["research".into()],
            default_template: Some("research".into()),
            default_budget_microusd: None,
        }),
        ..DecideTerms::for_config(&config)
    };
    assert_eq!(decide.default_budget_microusd, None);
    let served = serve_deciding(
        &scratch,
        Scripted::new(),
        runtime.facts(),
        1_000_000,
        decide,
    );
    const BIG: u64 = 3_000_000;

    with_runtime(runtime, async {
        let evals = candidates(&router, 2).await;
        let decide = |n: usize, extra: Value| {
            let mut args = json!({"eval":evals[n].eval,"decision":"accept","reason":"alice asked",
                                  "title":"Work","scope":"Do it"});
            for (key, value) in extra.as_object().unwrap() {
                args[key] = value.clone();
            }
            served.post("tool/router_decide", Some(TOKEN), args)
        };
        let swarm = json!({"plan":"swarm","agents":3});

        let (status, refused) = decide(1, swarm.clone()).await;
        assert_eq!(status, 422, "{refused}");
        let why = refused["error"].as_str().unwrap();
        assert!(why.contains("a swarm plan needs budget_microusd"), "{why}");

        let (status, answer) = decide(0, json!({"budget_microusd": BIG})).await;
        assert_eq!(status, 200, "a budget a session does not take: {answer}");

        let mut big = swarm;
        big["budget_microusd"] = json!(BIG);
        let (status, answer) = decide(1, big).await;
        assert_eq!(status, 200, "{answer}");
        for _ in 0..500 {
            if scratch.sent().len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    let tickets = scratch.store().tickets().unwrap();
    let (swarms, sessions): (Vec<_>, Vec<_>) = tickets
        .iter()
        .partition(|ticket| matches!(ticket.plan, Plan::Swarm { .. }));
    let ([session], [swarm]) = (&sessions[..], &swarms[..]) else {
        panic!("{tickets:?}")
    };
    assert_eq!(session.mandate.budget_microusd, None);
    assert_eq!(session.approval, Approval::NotNeeded);
    assert_eq!(swarm.mandate.budget_microusd, Some(BIG));
    assert_eq!(swarm.approval, Approval::Waiting, "above the threshold");
    let sent = scratch.sent();
    let asked: Vec<_> = sent
        .iter()
        .filter(|message| message.kind == MessageKind::Attention)
        .collect();
    let [ask] = &asked[..] else {
        panic!("{sent:?}")
    };
    assert!(
        ask.text.contains("needs approval first") && ask.text.contains("a swarm of 3 agents"),
        "only the swarm asks: {}",
        ask.text
    );
}

#[tokio::test]
async fn router_read_follows_ids_from_a_trigger_to_its_thread_and_ticket() {
    let scratch = Scratch::new("read");
    scratch.append(&request(
        "t-ask",
        "T1",
        &"Fix the flaky retry test. ".repeat(200),
    ));
    // The scripted Router accepts at once, so the Ticket exists to be read.
    let runtime = scratch.runtime(Arc::new(ScriptedRouter::new(500_000)), true);
    let facts = runtime.facts();
    runtime.run(std::future::pending()).await.unwrap();
    let served = serve(&scratch, Scripted::new(), facts, 1_000_000);
    let read = |args: Value| served.post("tool/router_read", Some(TOKEN), args);

    let (status, trigger) = read(json!({"id":"t-ask"})).await;
    assert_eq!(status, 200, "{trigger}");
    assert_eq!(trigger["kind"], "trigger");
    assert_eq!(trigger["actor"]["id"], "alice");
    assert_eq!(trigger["text_clipped"], true);
    assert!(trigger["text"].as_str().unwrap().chars().count() <= 2_003);
    assert_eq!(trigger["threads"], json!(["T1"]));
    let ticket_id = trigger["tickets"][0].as_str().unwrap().to_owned();

    let (_, thread) = read(json!({"id":"T1"})).await;
    assert_eq!(thread["kind"], "thread");
    assert_eq!(thread["triggers"], json!(["t-ask"]));
    assert_eq!(thread["tickets"], json!([ticket_id]));
    assert_eq!(thread["decisions"][0]["kind"], "accept");
    assert_eq!(thread["decisions"][0]["ticket"], json!(ticket_id));

    let (_, ticket) = read(json!({"id":ticket_id})).await;
    assert_eq!(ticket["kind"], "ticket");
    assert_eq!(ticket["origin"], "t-ask");
    assert_eq!(ticket["threads"], json!(["T1"]));
    assert_eq!(ticket["stage"], "ended");
    assert_eq!(ticket["latest_report"]["status"], "ready");

    let (status, _) = read(json!({"id":"t-ask","kind":"thread"})).await;
    assert_eq!(status, 404, "the kind narrows the lookup");
    let (status, missing) = read(json!({"id":"nothing-here"})).await;
    assert_eq!(status, 404);
    assert!(missing["error"].as_str().unwrap().contains("nothing-here"));
    assert_eq!(read(json!({"ids":["t-ask"]})).await.0, 422);
}

#[tokio::test]
async fn a_pause_file_in_the_state_dir_starts_no_eval_until_it_lifts() {
    let scratch = Scratch::new("runtime-pause");
    scratch.append(&request("t-ask", "T1", "Rotate the staging keys"));
    std::fs::write(scratch.state().join("PAUSE"), "").unwrap();
    let router = || Arc::new(ScriptedRouter::new(500_000));

    let paused = scratch.runtime(router(), true).run(std::future::pending());
    let summary = tokio::time::timeout(LIMIT, paused).await.unwrap().unwrap();
    assert_eq!((summary.triggers_received, summary.effects_done), (1, 0));
    assert!(scratch.store().decisions().unwrap().is_empty());
    assert!(scratch.sent().is_empty());

    std::fs::remove_file(scratch.state().join("PAUSE")).unwrap();
    let lifted = scratch.runtime(router(), true).run(std::future::pending());
    tokio::time::timeout(LIMIT, lifted).await.unwrap().unwrap();
    let kinds: Vec<_> = scratch.sent().iter().map(|m| m.kind).collect();
    assert_eq!(kinds, [MessageKind::Acknowledgement, MessageKind::Result]);
}

const WORKER: &str = "worker-token";

/// Starts every Assignment and leaves it running, so the test can play its
/// worker over HTTP.
#[derive(Default)]
struct Starting {
    launches: Mutex<Vec<Launch>>,
}

impl Executor for Starting {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        self.launches.lock().unwrap().push(launch.clone());
        Box::pin(async move {
            facts
                .send(Fact::Started {
                    ticket: launch.ticket,
                    assignment: launch.assignment,
                })
                .await
        })
    }
    fn deliver(&self, _: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// Poll the store until `ready` finds something.
async fn until<T>(scratch: &Scratch, what: &str, ready: impl Fn(&SqliteStore) -> Option<T>) -> T {
    for _ in 0..1_000 {
        if let Some(found) = ready(&scratch.store()) {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what} never happened")
}

/// The first Ticket, once its latest Assignment runs, and a seat for it.
async fn running(scratch: &Scratch) -> (TicketId, WorkerSeat) {
    until(scratch, "the assignment started", |store| {
        let ticket = store.tickets().ok()?.into_iter().next()?;
        let running = ticket.running_assignment()?.id.clone();
        let work = scratch.state().join("work");
        let seat = WorkerSeat::new(ticket.id.clone(), running, work, 500_000);
        Some((ticket.id, seat))
    })
    .await
}

fn alice_policy() -> AllowlistPolicy {
    AllowlistPolicy {
        people: ["slack:alice".to_owned()].into(),
        ..AllowlistPolicy::default()
    }
}

fn model_request_for(tool: &str, text: &str) -> Value {
    let mut request = model_request(text);
    request["tools"][0]["function"]["name"] = json!(tool);
    request
}

#[tokio::test]
async fn a_worker_reports_with_the_ledgers_spend_and_calls_only_its_own_tools() {
    let scratch = Scratch::new("worker");
    scratch.append(&request("t-ask", "T1", "Fix the flaky retry test"));
    let runtime = scratch.runtime_with(
        alice_policy(),
        Arc::new(ScriptedRouter::new(500_000)),
        Arc::new(Starting::default()),
        false,
        false,
    );
    let served = serve(&scratch, Scripted::new(), runtime.facts(), 1_000_000);

    with_runtime(runtime, async {
        let (ticket, seat) = running(&scratch).await;
        let seat = served
            .broker
            .admit_worker(WORKER.into(), seat, "You are a worker.".into())
            .unwrap();
        let (status, session) = served.post("session", Some(WORKER), json!({})).await;
        assert_eq!(
            (status, session),
            (200, json!({"system_prompt":"You are a worker."}))
        );

        let asks = model_request_for("worker_report", "fix it");
        let (status, reply) = served.post("model", Some(WORKER), asks).await;
        assert_eq!(status, 200, "{reply}");
        let account = format!("ticket:{ticket}");
        assert_eq!(scratch.store().committed(&account, 0).unwrap(), COST);
        assert_eq!(scratch.store().committed("router", 0).unwrap(), 0);

        let listed = served.listed(WORKER).await;
        assert_eq!(
            listed,
            ["worker_report", "worker_handback", "fetch_url"],
            "no run_code and no job tools on a host without run_code"
        );
        assert_eq!(
            served.broker.worker_tools(None, false),
            listed,
            "its grants"
        );
        for tool in ROUTER_TOOLS.iter().chain(&WORKER_TOOLS) {
            let (status, reply) = served
                .post(&format!("tool/{tool}"), Some(WORKER), json!({}))
                .await;
            assert_eq!(
                status != 403,
                listed.iter().any(|name| name == tool),
                "GET /tools lists exactly what the worker may call: {tool} {status} {reply}"
            );
        }
        for (path, token) in [
            ("tool/router_decide", WORKER),
            ("tool/router_read", WORKER),
            ("tool/claim_task", WORKER),
            ("tool/worker_report", TOKEN),
            ("tool/run_code", TOKEN),
        ] {
            let (status, _) = served.post(path, Some(token), json!({})).await;
            assert_eq!(status, 403, "{path} with {token}");
        }
        let (status, reply) = served.post("model", Some(WORKER), model_request("x")).await;
        assert_eq!(
            status, 403,
            "a worker may not offer the Router's tools: {reply}"
        );
        let (status, refused) = served
            .post("tool/run_code", Some(WORKER), json!({"code":"true"}))
            .await;
        assert_eq!(status, 403);
        assert!(refused["error"].as_str().unwrap().contains("not available"));
        for bad in [
            json!({"status":"progress","text":"   "}),
            json!({"status":"done","text":"t"}),
            json!({"status":"progress","text":"t","spent_microusd":0}),
        ] {
            let (status, _) = served
                .post("tool/worker_report", Some(WORKER), bad.clone())
                .await;
            assert_eq!(status, 422, "{bad}");
        }

        seat.begin_turn();
        let progress = json!({"status":"progress","text":"x".repeat(9_000)});
        let (status, reply) = served
            .post("tool/worker_report", Some(WORKER), progress)
            .await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["spent_microusd"], COST, "the spend is the ledger's");
        assert!(reply["shortened"].is_string(), "{reply}");
        assert!(seat.quiet_turn(), "a turn with only progress is quiet");
        let ready = json!({"status":"ready","text":"Fixed: the retry waited on a dropped timer."});
        let (status, reply) = served
            .post("tool/worker_report", Some(WORKER), ready.clone())
            .await;
        assert_eq!(status, 200, "{reply}");
        assert!(reply["next"].as_str().unwrap().contains("Stop here"));
        assert!(!seat.quiet_turn(), "a ready turn is not quiet");
        let (status, refused) = served.post("tool/worker_report", Some(WORKER), ready).await;
        assert_eq!(status, 409, "the ticket has ended: {refused}");

        served.broker.revoke(WORKER);
        let (status, _) = served.post("session", Some(WORKER), json!({})).await;
        assert_eq!(status, 403, "a revoked token is refused");
    })
    .await;

    let store = scratch.store();
    let ticket = &store.tickets().unwrap()[0];
    let kept: Vec<_> = ticket.assignments[0]
        .reports
        .iter()
        .map(|r| (r.status, r.spent_microusd, r.text.chars().count()))
        .collect();
    assert_eq!(
        kept,
        [
            (ReportStatus::Progress, COST, 8_003),
            (ReportStatus::Ready, COST, 43)
        ]
    );
    let sent = scratch.sent();
    let kinds: Vec<_> = sent.iter().map(|m| m.kind).collect();
    assert_eq!(kinds, [MessageKind::Acknowledgement, MessageKind::Result]);
    assert!(sent[1].text.starts_with("Fixed: the retry"));
}

#[tokio::test]
async fn a_worker_past_its_ticket_budget_is_refused_and_the_router_still_calls() {
    let scratch = Scratch::new("worker-budget");
    let provider = Scripted::new();
    let (_runtime, served) = model_only(&scratch, provider.clone(), 1_000_000);
    // One call fits; a second hold on top of the first call's cost does not.
    let budget = hold_for("fix it") + COST - 1;
    let seat = WorkerSeat::new(
        TicketId::new("tk_tiny"),
        AssignmentId::new("as_1"),
        scratch.state(),
        budget,
    );
    served
        .broker
        .admit_worker(WORKER.into(), seat, "You are a worker.".into())
        .unwrap();
    let worker = || {
        served.post(
            "model",
            Some(WORKER),
            model_request_for("worker_report", "fix it"),
        )
    };

    assert_eq!(worker().await.0, 200);
    let (status, refused) = worker().await;
    assert_eq!(status, 403, "{refused}");
    let why = refused["error"].as_str().unwrap();
    assert!(why.contains("tk_tiny's budget is reached"), "{why}");
    assert!(why.contains(&format!("{COST} of {budget}")), "{why}");

    let (status, reply) = served.model("judge").await;
    assert_eq!(status, 200, "the Router has its own account: {reply}");
    assert_eq!(
        provider.calls(),
        2,
        "the refused call never reached the provider"
    );
    let store = scratch.store();
    assert_eq!(store.committed("ticket:tk_tiny", 0).unwrap(), COST);
    assert_eq!(store.committed("router", 0).unwrap(), COST);
}

/// What `COST` is at an overrun: a million prompt tokens and 50 completion
/// tokens at `PRICES`.
const OVERRUN: u64 = 300_060;

fn admit_seat(served: &Served, scratch: &Scratch, token: &str, ticket: &str, budget: Option<u64>) {
    let seat = WorkerSeat::new(
        TicketId::new(ticket),
        AssignmentId::new("as_1"),
        scratch.state(),
        budget,
    );
    served
        .broker
        .admit_worker(token.into(), seat, "You are a worker.".into())
        .unwrap();
}

#[tokio::test]
async fn a_ticket_with_no_budget_is_held_and_settled_but_never_refused_for_spend() {
    let scratch = Scratch::new("worker-uncapped");
    let provider = Scripted::new();
    let (_runtime, served) = model_only(&scratch, provider.clone(), 1_000_000);
    // What a cap of one call would be.
    let budget = hold_for("fix it") + COST - 1;
    admit_seat(
        &served,
        &scratch,
        "worker-capped",
        "tk_capped",
        Some(budget),
    );
    admit_seat(&served, &scratch, "worker-open", "tk_open", None);
    let asks = |token: &'static str| {
        served.post(
            "model",
            Some(token),
            model_request_for("worker_report", "fix it"),
        )
    };

    assert_eq!(asks("worker-capped").await.0, 200);
    assert_eq!(asks("worker-capped").await.0, 403, "the cap holds");
    for _ in 0..5 {
        let (status, reply) = asks("worker-open").await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["swarm_cost_microusd"], COST);
    }

    let store = scratch.store();
    let rows = store.spend("ticket:tk_open").unwrap();
    assert_eq!(rows.len(), 5, "every call was held");
    assert!(rows.iter().all(|r| r.state == SpendState::Settled
        && r.hold_microusd == hold_for("fix it")
        && r.cost_microusd == COST));
    assert_eq!(
        store.committed("ticket:tk_open", 0).unwrap(),
        5 * COST,
        "and its spend is recorded"
    );
    assert_eq!(provider.calls(), 6, "only the refused call never went out");

    std::fs::write(scratch.state().join("PAUSE"), "").unwrap();
    assert_eq!(
        asks("worker-open").await.0,
        503,
        "the kill switch still stops it"
    );
    assert_eq!(store.spend("ticket:tk_open").unwrap().len(), 5);
}

#[tokio::test]
async fn a_router_with_no_daily_cap_is_never_refused_for_spend_and_still_counts_its_account() {
    let scratch = Scratch::new("router-uncapped");
    let provider = Scripted::new();
    // A cap of one call.
    let cap = hold_for("judge") + COST - 1;
    let (_runtime, served) = model_only(&scratch, provider.clone(), cap);
    let open = Grant {
        account: Account::Router {
            daily_microusd: None,
        },
        ..grant(cap)
    };
    served.broker.admit("open-router".into(), open).unwrap();

    assert_eq!(served.model("judge").await.0, 200);
    assert_eq!(served.model("judge").await.0, 403, "the cap holds");
    for _ in 0..4 {
        let (status, reply) = served
            .post("model", Some("open-router"), model_request("judge"))
            .await;
        assert_eq!(status, 200, "{reply}");
    }

    let store = scratch.store();
    assert_eq!(store.spend("router").unwrap().len(), 5);
    assert_eq!(store.committed("router", 0).unwrap(), 5 * COST);
}

#[tokio::test]
async fn an_overrun_is_only_recorded_with_no_cap_and_stops_admission_where_there_is_one() {
    let scratch = Scratch::new("overrun");
    let provider = Scripted::new();
    let (_runtime, served) = model_only(&scratch, provider.clone(), 1_000_000_000);
    admit_seat(&served, &scratch, "worker-open", "tk_open", None);
    let open = || {
        served.post(
            "model",
            Some("worker-open"),
            model_request_for("worker_report", "fix it"),
        )
    };

    provider.set(Behaviour::Overrun);
    let (status, reply) = open().await;
    assert_eq!(status, 200, "no cap, so nothing to refuse: {reply}");
    assert_eq!(reply["swarm_cost_microusd"], OVERRUN);
    let store = scratch.store();
    let rows = store.spend("ticket:tk_open").unwrap();
    let [row] = &rows[..] else { panic!("{rows:?}") };
    assert_eq!(
        (row.state, row.cost_microusd),
        (SpendState::Settled, OVERRUN),
        "the cost the provider billed is the record"
    );
    assert!(row.cost_microusd > row.hold_microusd);
    provider.set(Behaviour::Answer);
    assert_eq!(
        served.model("judge").await.0,
        200,
        "an overrun with no cap stops no one that has one"
    );
    assert_eq!(open().await.0, 200);

    provider.set(Behaviour::Overrun);
    let (status, reply) = served.model("judge").await;
    assert_eq!(status, 403, "{reply}");
    assert!(
        reply["error"]
            .as_str()
            .unwrap()
            .contains("provider exceeded reservation"),
        "{reply}"
    );
    provider.set(Behaviour::Answer);
    let (status, reply) = served.model("judge").await;
    assert_eq!(status, 403, "{reply}");
    assert!(
        reply["error"]
            .as_str()
            .unwrap()
            .contains("cost more than its hold"),
        "{reply}"
    );
    assert_eq!(open().await.0, 200, "but an account with no cap goes on");
    assert_eq!(
        store.committed("ticket:tk_open", 0).unwrap(),
        OVERRUN + 2 * COST
    );
}

#[tokio::test]
async fn a_seat_keeps_why_its_latest_model_call_failed_and_tells_it_once() {
    let scratch = Scratch::new("worker-failure");
    let provider = Scripted::new();
    let (_runtime, served) = model_only(&scratch, provider.clone(), 1_000_000);
    let admit = |token: &str, ticket: &str, budget: u64| {
        let seat = WorkerSeat::new(
            TicketId::new(ticket),
            AssignmentId::new("as_1"),
            scratch.state(),
            budget,
        );
        let prompt = "You are a worker.".into();
        served.broker.admit_worker(token.into(), seat, prompt)
    };
    let asks = |token: &'static str| {
        served.post(
            "model",
            Some(token),
            model_request_for("worker_report", "fix it"),
        )
    };
    let roomy = admit(WORKER, "tk_roomy", 500_000).unwrap();
    // One call fits; a second hold on top of the first call's cost does not.
    let tight = hold_for("fix it") + COST - 1;
    let tiny = admit("worker-tiny", "tk_tiny", tight).unwrap();

    roomy.begin_turn();
    assert_eq!(asks(WORKER).await.0, 200);
    assert_eq!(roomy.take_model_failure(), None, "a call that went through");

    provider.set(Behaviour::Fail);
    let (status, refused) = asks(WORKER).await;
    assert_eq!(status, 403, "{refused}");
    let why = "provider transport failure; outcome unknown".to_owned();
    assert_eq!(
        roomy.take_model_failure(),
        Some(ModelFailure::Failed(why.clone()))
    );
    assert_eq!(roomy.take_model_failure(), None, "told once");

    assert_eq!(asks(WORKER).await.0, 403);
    provider.set(Behaviour::Answer);
    assert_eq!(asks(WORKER).await.0, 200);
    assert_eq!(
        roomy.take_model_failure(),
        None,
        "a later call that went through is what the turn ended on"
    );

    std::fs::write(scratch.state().join("PAUSE"), "").unwrap();
    assert_eq!(asks(WORKER).await.0, 503);
    std::fs::remove_file(scratch.state().join("PAUSE")).unwrap();
    assert_eq!(
        roomy.take_model_failure(),
        None,
        "a pause is waited out, it is not a failure"
    );

    provider.set(Behaviour::Fail);
    assert_eq!(asks(WORKER).await.0, 403);
    roomy.begin_turn();
    assert_eq!(roomy.take_model_failure(), None, "a new turn starts clean");

    tiny.begin_turn();
    assert_eq!(asks("worker-tiny").await.0, 403, "the provider still fails");
    assert_eq!(
        tiny.take_model_failure(),
        Some(ModelFailure::Failed(why)),
        "a call that fails at the provider is no budget failure"
    );
    provider.set(Behaviour::Answer);
    assert_eq!(asks("worker-tiny").await.0, 200);
    let (status, refused) = asks("worker-tiny").await;
    assert_eq!(status, 403, "{refused}");
    let failure = tiny.take_model_failure().unwrap();
    assert_eq!(
        failure,
        ModelFailure::BudgetReached {
            committed: COST,
            budget: tight
        },
        "the budget refusal names the numbers"
    );
    let (status, text) = failure.report();
    assert_eq!(status, ReportStatus::Blocked);
    let cap = usd(tight);
    for says in [
        cap.as_str(),
        "cannot cover another model call",
        "new request",
    ] {
        assert!(text.contains(says), "{says:?} is not in {text}");
    }
}

#[tokio::test]
async fn a_worker_that_sends_the_same_call_over_and_over_is_refused_and_told_what_to_do() {
    let scratch = Scratch::new("worker-repeat");
    let (_runtime, served) = model_only(&scratch, Scripted::new(), 1_000_000);
    let seat = WorkerSeat::new(
        TicketId::new("tk_loop"),
        AssignmentId::new("as_1"),
        scratch.state(),
        500_000,
    );
    let seat = served
        .broker
        .admit_worker(WORKER.into(), seat, "You are a worker.".into())
        .unwrap();
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = hits.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let app = axum::Router::new().fallback(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        async { "page" }
    });
    let web = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let fetch = |path: &str| {
        let page = json!({"url": format!("{url}{path}")});
        served.post("tool/fetch_url", Some(WORKER), page)
    };

    for call in 1..=2 {
        let (status, page) = fetch("status").await;
        assert_eq!(status, 200, "call {call}: {page}");
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let (status, refused) = fetch("status").await;
    assert_eq!(status, 429, "{refused}");
    let why = refused["error"].as_str().unwrap();
    for says in [
        "fetch_url refused",
        "3 times in a row",
        "Change your approach",
        "worker_report",
    ] {
        assert!(why.contains(says), "{says:?} is not in {why}");
    }
    assert_eq!(fetch("status").await.0, 429, "it stays refused");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "a refused call is never sent"
    );

    assert_eq!(fetch("other").await.0, 200, "another call is not refused");
    assert_eq!(
        fetch("status").await.0,
        200,
        "the run of the same call ended"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 4);

    // A new turn starts the count again, even in the middle of a run.
    for _ in 0..2 {
        assert_eq!(fetch("again").await.0, 200);
    }
    seat.begin_turn();
    for call in 1..=2 {
        assert_eq!(fetch("again").await.0, 200, "a new turn, call {call}");
    }
    web.abort();
}

/// A page on a loopback port that counts its hits, and its address.
async fn counting_page() -> (
    String,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = hits.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let app = axum::Router::new().fallback(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        async { "page" }
    });
    let web = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, hits, web)
}

/// A broker whose host lets `fetch_url` reach private networks or not, with
/// one worker admitted.
fn fetching_worker(
    scratch: &Scratch,
    allow_private_networks: bool,
) -> (Runtime, Served, Arc<WorkerSeat>) {
    let runtime = scratch.runtime(Arc::new(Holding::default()), true);
    let served = serve_on(
        scratch,
        Scripted::new(),
        runtime.facts(),
        1_000_000,
        decide_terms(),
        Nodes::default(),
        WorkerParts {
            allow_private_networks,
            ..workers(None, None)
        },
        None,
    );
    let seat = WorkerSeat::new(
        TicketId::new("tk_1"),
        AssignmentId::new("as_1"),
        scratch.state(),
        500_000,
    );
    let seat = served
        .broker
        .admit_worker(WORKER.into(), seat, "You are a worker.".into())
        .unwrap();
    (runtime, served, seat)
}

#[tokio::test]
async fn fetch_url_refuses_loopback_and_private_addresses_unless_the_host_allows_them() {
    let (url, hits, web) = counting_page().await;
    let port = url.rsplit(':').next().unwrap().trim_end_matches('/');
    let scratch = Scratch::new("fetch-private");
    let (_runtime, served, seat) = fetching_worker(&scratch, false);

    // Each address a worker steered by a page might try, by number or by a
    // name that resolves to this host: the first is the page on loopback.
    let targets = [
        url.clone(),
        format!("http://localhost:{port}/"),
        format!("http://[::1]:{port}/"),
        format!("http://[fd00::1]:{port}/"),
        format!("http://[::ffff:127.0.0.1]:{port}/"),
        format!("http://0.0.0.0:{port}/"),
        "http://169.254.169.254/latest/meta-data/".to_owned(),
        "http://10.0.0.1/".to_owned(),
        "http://172.16.0.1/".to_owned(),
        "http://192.168.1.1/".to_owned(),
        "http://100.64.0.1/".to_owned(),
        "http://224.0.0.1/".to_owned(),
    ];
    for target in &targets {
        seat.begin_turn();
        let (status, reply) = served
            .post("tool/fetch_url", Some(WORKER), json!({"url": target}))
            .await;
        assert_eq!(status, 403, "{target}: {reply}");
        assert_eq!(reply["refused"], true, "{target}: {reply}");
        assert_eq!(reply["reason"], "private_address", "{target}: {reply}");
        let why = reply["error"].as_str().unwrap();
        assert!(
            why.starts_with("fetch_url refused: ")
                && why.contains("resolves to a private address")
                && why.contains("Nothing was sent"),
            "{target}: {why}"
        );
    }
    let (_, first) = served
        .post("tool/fetch_url", Some(WORKER), json!({"url": url}))
        .await;
    assert!(
        first["error"]
            .as_str()
            .unwrap()
            .starts_with("fetch_url refused: 127.0.0.1 resolves to a private address"),
        "{first}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0, "no request was sent");

    // The host's say-so reaches the same page.
    let scratch = Scratch::new("fetch-private-allowed");
    let (_runtime, allowed, _seat) = fetching_worker(&scratch, true);
    let (status, page) = allowed
        .post("tool/fetch_url", Some(WORKER), json!({"url": url}))
        .await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["body"], "page");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    web.abort();
}

#[tokio::test]
async fn a_worker_that_ignores_its_refusals_has_its_turn_stopped_and_pays_for_no_model_call_after()
{
    let scratch = Scratch::new("worker-stopped");
    let provider = Scripted::new();
    let (_runtime, served) = model_only(&scratch, provider.clone(), 1_000_000);
    let seat = WorkerSeat::new(
        TicketId::new("tk_loop"),
        AssignmentId::new("as_1"),
        scratch.state(),
        500_000,
    );
    let seat = served
        .broker
        .admit_worker(WORKER.into(), seat, "You are a worker.".into())
        .unwrap();
    let (url, hits, web) = counting_page().await;
    let fetch = |path: &str| {
        let page = json!({"url": format!("{url}{path}")});
        served.post("tool/fetch_url", Some(WORKER), page)
    };
    let ask = || {
        let request = model_request_for("worker_report", "fix it");
        served.post("model", Some(WORKER), request)
    };
    let spent = || served.broker.spent(&seat);
    seat.begin_turn();
    assert_eq!(ask().await.0, 200);

    for call in 1..=2 {
        assert_eq!(fetch("status").await.0, 200, "call {call}");
        assert_eq!(ask().await.0, 200, "the model is asked after each result");
    }
    for refusal in 1..=2 {
        let (status, refused) = fetch("status").await;
        assert_eq!(status, 429, "refusal {refusal}: {refused}");
        let why = refused["error"].as_str().unwrap();
        for says in [
            "What it answered before: {",
            "page",
            "You already have that answer.",
            "your turn is stopped",
        ] {
            assert!(why.contains(says), "{says:?} is not in {why}");
        }
        assert!(!why.contains("Your turn is stopped"), "not yet: {why}");
        assert_eq!(ask().await.0, 200, "still asked after refusal {refusal}");
        assert_eq!(seat.take_model_failure(), None);
    }

    let (status, last) = fetch("status").await;
    assert_eq!(status, 429);
    let why = last["error"].as_str().unwrap();
    assert!(why.contains("Your turn is stopped"), "{why}");
    assert!(why.contains("Do not call any more tools"), "{why}");
    let paid = spent().await.unwrap();
    let asked = provider.calls();

    let (status, refused) = ask().await;
    assert_eq!(status, 403, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("turn was stopped"),
        "{refused}"
    );
    assert_eq!(provider.calls(), asked, "the provider was not asked");
    assert_eq!(
        spent().await.unwrap(),
        paid,
        "and nothing was held or charged"
    );
    let failure = seat.take_model_failure();
    assert_eq!(failure, Some(ModelFailure::Stuck(FETCH_LOOP.into())));
    assert_eq!(seat.take_model_failure(), None, "told once");
    let failure = failure.unwrap();
    let (status, text) = failure.report();
    assert_eq!(status, ReportStatus::Failed);
    assert_eq!(text, FETCH_LOOP, "the question is the report");
    assert_eq!(failure.question().as_deref(), Some(FETCH_LOOP));

    for call in ["other", "status"] {
        let (status, refused) = fetch(call).await;
        assert_eq!(status, 429, "{call}: {refused}");
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("Your turn is stopped")
        );
    }
    let report = json!({"status": "ready", "text": "done"});
    let (status, refused) = served
        .post("tool/worker_report", Some(WORKER), report)
        .await;
    assert_eq!(status, 429, "a stopped turn runs no tool: {refused}");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "nothing ran after the stop");
    assert_eq!(ask().await.0, 403, "it stays stopped");

    seat.begin_turn();
    assert_eq!(ask().await.0, 200, "a new turn may ask the model");
    assert_eq!(fetch("other").await.0, 200, "and run a tool");
    assert_eq!(fetch("status").await.0, 200, "the count starts again");
    assert_eq!(hits.load(Ordering::SeqCst), 4);
    web.abort();
}

#[tokio::test]
async fn refusals_that_are_spread_over_a_turn_are_stopped_at_the_ceiling_too() {
    let scratch = Scratch::new("worker-ceiling");
    let provider = Scripted::new();
    let (_runtime, served) = model_only(&scratch, provider.clone(), 1_000_000);
    let seat = WorkerSeat::new(
        TicketId::new("tk_spread"),
        AssignmentId::new("as_1"),
        scratch.state(),
        500_000,
    );
    let seat = served
        .broker
        .admit_worker(WORKER.into(), seat, "You are a worker.".into())
        .unwrap();
    let (url, _hits, web) = counting_page().await;
    let fetch = |path: String| {
        let page = json!({"url": format!("{url}{path}")});
        served.post("tool/fetch_url", Some(WORKER), page)
    };
    let ask = || {
        let request = model_request_for("worker_report", "fix it");
        served.post("model", Some(WORKER), request)
    };
    seat.begin_turn();

    // Each round sends one call three times, and so is refused once, then
    // sends another call that goes through: never three refusals in a row.
    let mut refusals = 0;
    for round in 0..10 {
        assert_eq!(ask().await.0, 200, "round {round}: not stopped yet");
        for _ in 0..3 {
            refusals += usize::from(fetch("loop".to_owned()).await.0 == 429);
        }
        if round < 9 {
            assert_eq!(fetch(format!("other{round}")).await.0, 200);
        }
    }
    assert_eq!(refusals, 10);
    let (status, refused) = ask().await;
    assert_eq!(status, 403, "ten refusals in a turn stop it: {refused}");
    assert_eq!(
        seat.take_model_failure(),
        Some(ModelFailure::Stuck(FETCH_LOOP.into()))
    );
    web.abort();
}

#[tokio::test]
async fn a_worker_refused_the_same_way_three_times_has_its_turn_stopped_with_a_question() {
    let (url, hits, web) = counting_page().await;
    let scratch = Scratch::new("refused-three");
    let (_runtime, served, seat) = fetching_worker(&scratch, false);
    let fetch = || served.post("tool/fetch_url", Some(WORKER), json!({"url": url}));
    seat.begin_turn();

    for call in 1..=2 {
        let (status, reply) = fetch().await;
        assert_eq!(status, 403, "call {call}: {reply}");
        let request = model_request_for("worker_report", "fix it");
        assert_eq!(served.post("model", Some(WORKER), request).await.0, 200);
    }
    let (status, third) = fetch().await;
    assert_eq!(status, 429, "the third is stopped: {third}");
    let why = third["error"].as_str().unwrap();
    assert!(why.contains("Your turn is stopped"), "{why}");
    assert!(why.contains("failed the same way each time"), "{why}");

    let request = model_request_for("worker_report", "fix it");
    let (status, refused) = served.post("model", Some(WORKER), request).await;
    assert_eq!(status, 403, "no model call after the stop: {refused}");
    let failure = seat.take_model_failure().unwrap();
    let said = "I tried 3 times to call fetch_url, and it failed the same way each time, so I \
                stopped. Is there another way you want me to handle this, or should I stop here?";
    assert_eq!(failure, ModelFailure::Stuck(said.into()));
    assert_eq!(failure.question().as_deref(), Some(said));
    assert_eq!(hits.load(Ordering::SeqCst), 0, "nothing was fetched");
    web.abort();
}

/// What the host tells the person when a worker kept calling `fetch_url`.
const FETCH_LOOP: &str = "I kept calling fetch_url with the same arguments without making \
                          progress, so I stopped. Is there another way you want me to handle \
                          this, or should I stop here?";

/// A worker on a seat of its own, whose `fetch_url` reaches a loopback page
/// that counts its hits, and whose model calls reach `provider`.
struct Looper {
    served: Served,
    _runtime: Runtime,
    seat: Arc<WorkerSeat>,
    provider: Arc<Scripted>,
    url: String,
    hits: Arc<std::sync::atomic::AtomicUsize>,
    web: tokio::task::JoinHandle<()>,
}

impl Looper {
    async fn new(scratch: &Scratch, temperature: Option<Temperature>) -> Self {
        let provider = Scripted::new();
        let runtime = scratch.runtime(Arc::new(Holding::default()), true);
        let worker_terms = ModelTerms {
            temperature,
            ..terms()
        };
        let served = serve_on(
            scratch,
            provider.clone(),
            runtime.facts(),
            1_000_000,
            decide_terms(),
            Nodes::default(),
            WorkerParts {
                terms: worker_terms,
                ..workers(None, None)
            },
            None,
        );
        let seat = WorkerSeat::new(
            TicketId::new("tk_loop"),
            AssignmentId::new("as_1"),
            scratch.state(),
            500_000,
        );
        let seat = served
            .broker
            .admit_worker(WORKER.into(), seat, "You are a worker.".into())
            .unwrap();
        let (url, hits, web) = counting_page().await;
        seat.begin_turn();
        Self {
            served,
            _runtime: runtime,
            seat,
            provider,
            url,
            hits,
            web,
        }
    }
    /// `fetch_url` of the page named `page`.
    async fn fetch(&self, page: impl std::fmt::Display) -> (u16, Value) {
        let args = json!({"url": format!("{}{page}", self.url)});
        self.served.post("tool/fetch_url", Some(WORKER), args).await
    }
    /// A model call, as Pi makes one after each tool result.
    async fn ask(&self) -> (u16, Value) {
        let request = model_request_for("worker_report", "fix it");
        self.served.post("model", Some(WORKER), request).await
    }
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
    /// Every call of `pages` in turn, each followed by a model call, until
    /// a tool call is refused: how many were sent, and the refusal.
    async fn go_round(&self, pages: usize, most: usize) -> (usize, Value) {
        for sent in 1..=most {
            let (status, answer) = self.fetch((sent - 1) % pages).await;
            if status == 429 {
                return (sent, answer);
            }
            assert_eq!(status, 200, "call {sent}: {answer}");
            assert_eq!(self.ask().await.0, 200, "model call after call {sent}");
        }
        panic!("{most} calls and none was refused")
    }
}

impl Drop for Looper {
    fn drop(&mut self) {
        self.web.abort();
    }
}

#[tokio::test]
async fn a_five_call_cycle_is_stopped_at_the_seventeenth_call_and_says_what_it_was_doing() {
    let scratch = Scratch::new("cycle-of-five");
    let worker = Looper::new(&scratch, None).await;

    let (sent, refused) = worker.go_round(5, 100).await;

    assert_eq!(sent, 15, "the third round is refused at its last call");
    let why = refused["error"].as_str().unwrap();
    assert!(why.contains("these 5 calls in turn, 3 times over"), "{why}");
    assert!(!why.contains("Your turn is stopped"), "not yet: {why}");
    assert_eq!(worker.ask().await.0, 200, "still asked after the refusal");
    for call in 16..=17 {
        let (status, refused) = worker.fetch((call - 1) % 5).await;
        assert_eq!(status, 429, "call {call}: {refused}");
        assert_eq!(worker.seat.take_model_failure(), None);
        if call == 17 {
            let why = refused["error"].as_str().unwrap();
            assert!(
                why.contains(
                    "Your turn is stopped because you sent 3 refused calls in a row, and the \
                     person is told you got stuck. Do not call any more tools."
                ),
                "{why}"
            );
        } else {
            assert_eq!(worker.ask().await.0, 200);
        }
    }
    assert_eq!(worker.hits(), 14, "a refused call is never run");
    let calls = worker.provider.calls();
    let (status, refused) = worker.ask().await;
    assert_eq!(status, 403, "{refused}");
    assert_eq!(worker.provider.calls(), calls, "the provider was not asked");
    assert_eq!(
        worker.seat.take_model_failure(),
        Some(ModelFailure::Stuck(FETCH_LOOP.into()))
    );
}

#[tokio::test]
async fn calls_that_find_nothing_new_are_stopped_after_thirty_and_a_new_turn_may_go_on() {
    let scratch = Scratch::new("nothing-new");
    let worker = Looper::new(&scratch, None).await;

    // Seventeen pages in turn: too long a round to be a circle, and the page
    // says the same every time.
    let (sent, refused) = worker.go_round(17, 100).await;

    assert_eq!(
        sent,
        17 + 30,
        "seventeen new calls, then thirty that bring nothing"
    );
    assert_eq!(worker.hits(), 46, "the refused call is never run");
    let why = refused["error"].as_str().unwrap();
    assert!(
        why.contains(
            "Your turn is stopped because you made 30 calls in a row that found nothing new"
        ),
        "{why}"
    );
    let calls = worker.provider.calls();
    let (status, refused) = worker.ask().await;
    assert_eq!(status, 403, "{refused}");
    assert!(
        refused["error"].as_str().unwrap().contains(
            "the turn was stopped because the worker made 30 calls in a row that found nothing new"
        ),
        "{refused}"
    );
    assert_eq!(worker.provider.calls(), calls);
    assert_eq!(
        worker.seat.take_model_failure(),
        Some(ModelFailure::Stuck(FETCH_LOOP.into()))
    );

    worker.seat.begin_turn();
    assert_eq!(worker.ask().await.0, 200, "a new turn may ask the model");
    assert_eq!(worker.fetch(3).await.0, 200, "and run a tool");
}

#[tokio::test]
async fn a_turn_is_stopped_at_its_ceiling_of_tool_calls() {
    let scratch = Scratch::new("tool-ceiling");
    let worker = Looper::new(&scratch, None).await;

    // Every call is a different one, so nothing but the ceiling stops them.
    for call in 0..200 {
        assert_eq!(
            worker.fetch(format!("page{call}")).await.0,
            200,
            "call {call}"
        );
    }
    let (status, refused) = worker.fetch("page200").await;

    assert_eq!(status, 429, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("Your turn is stopped because you made 200 tool calls in this turn"),
        "{refused}"
    );
    assert_eq!(worker.hits(), 200);
    assert_eq!(worker.ask().await.0, 403);
    assert_eq!(
        worker.seat.take_model_failure(),
        Some(ModelFailure::Stuck(
            "I made 200 tool calls without finishing, so I stopped. Should I keep going from \
             where I am?"
                .into()
        ))
    );
}

#[tokio::test]
async fn a_turn_is_stopped_at_its_ceiling_of_model_calls_and_runs_no_tool_after() {
    let scratch = Scratch::new("model-ceiling");
    let worker = Looper::new(&scratch, None).await;

    for call in 1..=250 {
        assert_eq!(worker.ask().await.0, 200, "model call {call}");
    }
    assert_eq!(
        worker.fetch("page").await.0,
        200,
        "the tools are not stopped yet"
    );
    let (status, refused) = worker.ask().await;

    assert_eq!(status, 403, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("the turn was stopped because the worker made 250 model calls in this turn"),
        "{refused}"
    );
    assert_eq!(worker.provider.calls(), 250, "the 251st was not sent");
    assert_eq!(
        worker.seat.take_model_failure(),
        Some(ModelFailure::Stuck(
            "I made 250 model calls without finishing, so I stopped. Should I keep going from \
             where I am?"
                .into()
        ))
    );
    assert_eq!(
        worker.fetch("page").await.0,
        429,
        "a stopped turn runs no tool"
    );
    assert_eq!(worker.hits(), 1);
}

#[tokio::test]
async fn a_long_turn_of_different_calls_is_not_stopped() {
    let scratch = Scratch::new("long-varied");
    let worker = Looper::new(&scratch, None).await;

    // Eighty different calls, a model call after each, and the same few
    // pages read again between them: the way a long ticket goes.
    for call in 0..80 {
        assert_eq!(
            worker.fetch(format!("part{call}")).await.0,
            200,
            "call {call}"
        );
        assert_eq!(worker.ask().await.0, 200, "model call {call}");
        if call % 10 == 9 {
            assert_eq!(worker.fetch("index").await.0, 200, "call {call}");
        }
    }
    assert_eq!(worker.seat.take_model_failure(), None);
    assert_eq!(worker.hits(), 88);
}

#[tokio::test]
async fn waiting_for_a_job_over_and_over_is_not_a_loop() {
    let scratch = Scratch::new("job-polling");
    let (_runtime, served, _fake) = serve_with_a_node_a_vault_and_a_browser(&scratch);
    let run = |code: String| {
        let args = json!({"code": code});
        served.post("tool/run_code", Some(THERE), args)
    };
    let wait = || served.post("tool/job_wait", Some(THERE), json!({"job": "job_1"}));
    let ask = || {
        let request = model_request_for("worker_report", "fix it");
        served.post("model", Some(THERE), request)
    };

    // More waits than the turn has tool calls, between different commands.
    for call in 0..150 {
        assert_ne!(
            run(format!("tail -n {call} build.log")).await.0,
            429,
            "run {call}"
        );
        for _ in 0..3 {
            assert_ne!(wait().await.0, 429, "a repeat of a wait is normal");
        }
    }
    assert_eq!(ask().await.0, 200, "the turn goes on");
    assert_ne!(
        run("one more".into()).await.0,
        429,
        "450 waits are not tool calls of the turn's ceiling"
    );
}

#[tokio::test]
async fn workers_sample_at_the_configured_temperature_and_the_router_at_zero() {
    let scratch = Scratch::new("temperature");
    let sampled = Temperature::try_from(0.6).unwrap();
    let worker = Looper::new(&scratch, Some(sampled)).await;

    assert_eq!(worker.ask().await.0, 200);
    let (status, reply) = worker.served.model("judge eval ev_1").await;
    assert_eq!(status, 200, "{reply}");

    let sent = worker.provider.bodies.lock().unwrap().clone();
    let [for_worker, for_router] = &sent[..] else {
        panic!("{sent:?}")
    };
    assert_eq!(for_worker["temperature"], json!(0.6));
    let mut unsampled = upstream_body(
        "deepseek-flash",
        &model_request_for("worker_report", "fix it"),
        MAX_OUTPUT,
    );
    unsampled["temperature"] = json!(0.6);
    assert_eq!(
        for_worker, &unsampled,
        "only the temperature differs from the shared body"
    );
    assert_eq!(
        for_router,
        &upstream_body(
            "deepseek-flash",
            &model_request("judge eval ev_1"),
            MAX_OUTPUT
        ),
        "the Router's call is the shared body, byte for byte"
    );
    assert_eq!(for_router["temperature"], json!(0));

    // A worker whose terms name none keeps the shared body's 0.
    let other = Scratch::new("temperature-none");
    let worker = Looper::new(&other, None).await;
    assert_eq!(worker.ask().await.0, 200);
    assert_eq!(
        worker.provider.bodies.lock().unwrap()[0]["temperature"],
        json!(0)
    );
}

#[tokio::test]
async fn a_wait_between_refusals_is_a_call_that_went_through_and_the_turn_goes_on() {
    let scratch = Scratch::new("wait-between");
    let (_runtime, served, _fake) = serve_with_a_node_a_vault_and_a_browser(&scratch);
    let run = |code: &str| {
        let args = json!({"code": code});
        served.post("tool/run_code", Some(THERE), args)
    };
    let wait = || served.post("tool/job_wait", Some(THERE), json!({"job": "job_1"}));
    let ask = || {
        let request = model_request_for("worker_report", "fix it");
        served.post("model", Some(THERE), request)
    };

    // Two refusals of one shape are a retry; a wait is a call that went
    // through, so neither the circle nor the count of failures carries past
    // it. Three refusals without one are stopped by the failing-shape guard.
    for round in 1..=2 {
        for call in 1..=2 {
            assert_eq!(
                run("rg -n for_config src").await.0,
                422,
                "round {round}, call {call}"
            );
        }
        for _ in 0..5 {
            assert_ne!(wait().await.0, 429, "a repeat of a wait is normal");
        }
    }
    let (status, reply) = ask().await;
    assert_eq!(
        status, 200,
        "two refusals, a wait, two more, and a wait do not stop it: {reply}"
    );
}

#[tokio::test]
async fn a_repeat_is_normal_for_a_job_a_report_and_a_page_that_moves_but_not_for_the_rest() {
    let scratch = Scratch::new("repeatable");
    let (_runtime, served, _fake) = serve_with_a_node_a_vault_and_a_browser(&scratch);
    let report = json!({"status": "progress", "text": " "});

    for (tool, token, args) in [
        ("job_wait", THERE, json!({"job": "job_1"})),
        ("worker_report", HERE, report),
        ("browser_read", HERE, json!({})),
        (
            "browser_act",
            HERE,
            json!({"action": "scroll", "text": "down"}),
        ),
    ] {
        for call in 1..=8 {
            let (status, reply) = served
                .post(&format!("tool/{tool}"), Some(token), args.clone())
                .await;
            assert_ne!(status, 429, "{tool}, call {call}: {reply}");
        }
    }
    for (tool, token, args) in [
        ("vault_list", HERE, json!({})),
        ("run_code", THERE, json!({"code": "rg -n for_config src"})),
    ] {
        for call in 1..=2 {
            let (status, reply) = served
                .post(&format!("tool/{tool}"), Some(token), args.clone())
                .await;
            assert_ne!(status, 429, "{tool}, call {call}: {reply}");
        }
        let (status, refused) = served
            .post(&format!("tool/{tool}"), Some(token), args)
            .await;
        assert_eq!(status, 429, "{tool}: {refused}");
        assert!(refused["error"].as_str().unwrap().contains(tool));
    }
}

#[tokio::test]
async fn a_handed_back_follow_up_reaches_a_fresh_eval_once() {
    let scratch = Scratch::new("handback");
    scratch.append(&request("t-ask", "T1", "Fix the flaky retry test"));
    let runtime = scratch.runtime_with(
        alice_policy(),
        Arc::new(ScriptedRouter::new(500_000)),
        Arc::new(Starting::default()),
        false,
        true,
    );
    let served = serve(&scratch, Scripted::new(), runtime.facts(), 1_000_000);

    with_runtime(runtime, async {
        let (ticket, seat) = running(&scratch).await;
        served
            .broker
            .admit_worker(WORKER.into(), seat, "You are a worker.".into())
            .unwrap();
        scratch.append(&request("t-more", "T1", "Also rotate the staging keys"));
        let more = TriggerId::new("t-more");
        until(&scratch, "the follow-up reached the ticket", |store| {
            let found = store.read_ticket(&ticket).ok()??;
            let reached = found
                .inputs
                .iter()
                .any(|i| matches!(&i.body, InputBody::Trigger { trigger } if *trigger == more));
            reached.then_some(())
        })
        .await;

        let back = json!({"trigger":"t-more","reason":"a separate request"});
        let (status, reply) = served
            .post("tool/worker_handback", Some(WORKER), back.clone())
            .await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["queued"], true);
        let fresh = until(&scratch, "a fresh eval decided the follow-up", |store| {
            store
                .decisions()
                .ok()?
                .into_iter()
                .find(|d| d.triggers == [more.clone()] && d.eval.is_some())
        })
        .await;
        assert_eq!(fresh.kind, DecisionKind::Accept, "{fresh:?}");

        let (status, again) = served
            .post("tool/worker_handback", Some(WORKER), back)
            .await;
        assert_eq!((status, again["queued"].clone()), (200, json!(false)));
        let elsewhere = json!({"trigger":"t-nowhere","reason":"not mine"});
        let (status, _) = served
            .post("tool/worker_handback", Some(WORKER), elsewhere)
            .await;
        assert_eq!(status, 409, "a trigger that never reached the ticket");
        let unreasoned = json!({"trigger":"t-more","reason":" "});
        let (status, _) = served
            .post("tool/worker_handback", Some(WORKER), unreasoned)
            .await;
        assert_eq!(status, 422);
    })
    .await;

    let decisions = scratch.store().decisions().unwrap();
    let judged = decisions
        .iter()
        .filter(|d| d.triggers == [TriggerId::new("t-more")])
        .count();
    assert_eq!(judged, 1, "the follow-up was judged once: {decisions:?}");
}

/// Serves `body` to every request on a loopback port.
async fn web(body: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let app = axum::Router::new().fallback(move || {
        let body = body.clone();
        async move { body }
    });
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, server)
}

#[tokio::test]
async fn fetch_url_keeps_swarm_modes_limits_for_a_worker() {
    let scratch = Scratch::new("fetch");
    let (_runtime, served) = model_only(&scratch, Scripted::new(), 1_000_000);
    let seat = WorkerSeat::new(
        TicketId::new("tk_1"),
        AssignmentId::new("as_1"),
        scratch.state(),
        500_000,
    );
    served
        .broker
        .admit_worker(WORKER.into(), seat, "You are a worker.".into())
        .unwrap();
    let mebibyte = 1_048_576;
    let (url, server) = web(vec![b'x'; mebibyte]).await;

    let (status, page) = served
        .post("tool/fetch_url", Some(WORKER), json!({"url": url}))
        .await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["status"], 200);
    assert_eq!(page["body_bytes"], 98_304, "the per-call default bound");
    assert_eq!(page["truncated"], true);
    // Each page is its own URL: the same call over and over is a loop the broker refuses.
    let whole = |page: u32| json!({"url": format!("{url}?page={page}"), "max_bytes": 8 * mebibyte});
    let mut read = 98_304;
    let mut page = 0;
    while read < solos_swarm::broker::FETCH_TOTAL_BYTES {
        page += 1;
        let (status, body) = served
            .post("tool/fetch_url", Some(WORKER), whole(page))
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["body_bytes"], mebibyte, "no call reads past 1 MiB");
        read += mebibyte as u64;
    }
    let (status, refused) = served
        .post("tool/fetch_url", Some(WORKER), whole(page + 1))
        .await;
    assert_eq!(status, 403);
    let why = refused["error"].as_str().unwrap();
    assert!(why.contains("read budget exhausted after 32 MiB"), "{why}");
    server.abort();
}

struct Laptop;

impl ExecutionEnvironment for Laptop {
    fn ready(&self) -> Result<(), Offline> {
        Ok(())
    }
    fn run_code(&self, _: Work<'_>, _: &Value) -> Result<Started> {
        anyhow::bail!("not in this test")
    }
    fn job(&self, _: Work<'_>, _: &str, _: Duration, _: OutputBounds) -> Result<Started> {
        anyhow::bail!("not in this test")
    }
}

const HERE: &str = "worker-here";
const THERE: &str = "worker-there";
const DREAM: &str = "worker-dream";

fn local_browser(scratch: &Scratch) -> (FakeCdp, BrowserParts) {
    let view = SandboxView::new([]);
    let key = scratch.root.join("trail.key");
    TrailSigner::init(&key, &view).unwrap();
    let fake = FakeCdp::start(&scratch.root.join("cdp.sock"), Vec::new()).unwrap();
    let config: BrowserConfig =
        serde_json::from_value(json!({"lightpanda": "/nowhere", "audit_key": key})).unwrap();
    let browsers = Browsers::new(
        &config,
        "local",
        &scratch.root.join("run"),
        None,
        TrailSigner::load(&key, &view).unwrap(),
        Launcher::Socket(fake.socket().to_owned()),
    )
    .unwrap();
    let blobs = BlobStore::open(&scratch.state().join("trail"), &TrailStoreConfig::default());
    let place = Arc::new(LocalBrowser::new(Arc::new(browsers)));
    let parts = BrowserParts::new(place, blobs.unwrap(), Duration::from_secs(1_800));
    (fake, parts)
}

fn serve_with_a_node_a_vault_and_a_browser(scratch: &Scratch) -> (Runtime, Served, FakeCdp) {
    let runtime = scratch.runtime(Arc::new(Holding::default()), true);
    let nodes = Nodes {
        default: None,
        nodes: BTreeMap::from([(
            "laptop".to_owned(),
            Arc::new(Laptop) as Arc<dyn ExecutionEnvironment>,
        )]),
    };
    let config = VaultConfig {
        path: scratch.root.join("vault.enc"),
        key_file: scratch.root.join("vault.key"),
    };
    let view = SandboxView::new([]);
    Vault::init(&config, &view).unwrap();
    let vault = VaultParts {
        vault: Arc::new(Vault::open(&config, &view).unwrap()),
        asks_on: BTreeSet::new(),
    };
    let (fake, browser) = local_browser(scratch);
    let pstack = Pstack::open(Path::new(env!("CARGO_MANIFEST_DIR")), "poteto-mode").unwrap();
    let served = serve_on(
        scratch,
        Scripted::new(),
        runtime.facts(),
        1_000_000,
        decide_terms(),
        nodes,
        workers(Some(vault), Some(browser)),
        Some(Arc::new(pstack)),
    );
    served.broker.set_memory(MemoryParts {
        review: ReviewTerms {
            terms: terms(),
            effort: Effort::Max,
        },
        daily_microusd: None,
        dream: Some(DreamTerms {
            effort: Effort::Max,
            max_output_tokens: Effort::Max.max_output_tokens(),
        }),
    });
    let dream = WorkerSeat::new(
        TicketId::new("tk_dream"),
        AssignmentId::new("as_1"),
        scratch.state(),
        None,
    )
    .in_environment(Some("local".to_owned()))
    .dreaming(scratch.state().join("tickets"));
    served
        .broker
        .admit_worker(DREAM.into(), dream, "You are the dream.".into())
        .unwrap();
    for (token, ticket, environment) in
        [(HERE, "tk_here", None), (THERE, "tk_there", Some("laptop"))]
    {
        let seat = WorkerSeat::new(
            TicketId::new(ticket),
            AssignmentId::new("as_1"),
            scratch.state(),
            500_000,
        )
        .in_environment(environment.map(str::to_owned));
        served
            .broker
            .admit_worker(token.into(), seat, "You are a worker.".into())
            .unwrap();
    }
    (runtime, served, fake)
}

#[tokio::test]
async fn a_seat_may_call_exactly_the_tools_it_is_offered() {
    let scratch = Scratch::new("offered");
    let (_runtime, served, _fake) = serve_with_a_node_a_vault_and_a_browser(&scratch);

    for (token, environment) in [(HERE, None), (THERE, Some("laptop"))] {
        let offered = served.listed(token).await;
        assert_eq!(
            served.broker.worker_tools(environment, false),
            offered,
            "its grants"
        );
        for tool in WORKER_TOOLS {
            let (status, reply) = served
                .post(&format!("tool/{tool}"), Some(token), json!({}))
                .await;
            let refused = status == 403
                && [
                    Some("run_code is not available on this host"),
                    Some(
                        "this ticket's /work is on the machine its run_code runs on: change \
                         files there with run_code",
                    ),
                    Some("no browser where this ticket runs"),
                    Some("only the dream retires a memo"),
                ]
                .contains(&reply["error"].as_str());
            assert_eq!(
                offered.iter().any(|name| name == tool),
                !refused,
                "{tool} for {environment:?}: {status} {reply}"
            );
        }
    }
    assert_eq!(
        served.listed(THERE).await,
        [
            "worker_report",
            "worker_handback",
            "run_code",
            "job_start",
            "job_wait",
            "job_stop",
            "fetch_url",
            "vault_list",
            "read",
            "memo_propose"
        ],
        "a node's worker has run_code there and no browser"
    );
    assert_eq!(
        served.listed(HERE).await,
        [
            "worker_report",
            "worker_handback",
            "fetch_url",
            "vault_list",
            "browser_open",
            "browser_act",
            "browser_login",
            "browser_read",
            "read",
            "memo_propose"
        ],
        "this host has a browser and no run_code"
    );
}

#[tokio::test]
async fn every_tool_has_a_handler() {
    let scratch = Scratch::new("handlers");
    let (_runtime, served, _fake) = serve_with_a_node_a_vault_and_a_browser(&scratch);
    let (here, there) = (served.listed(HERE).await, served.listed(THERE).await);
    // The file tools need run_code on this host, which that one lacks.
    let coding = Scratch::new("handlers_code");
    let (_coding_runtime, with_code, _) = serve_with_run_code(&coding);
    let coder = with_code.listed(CODER).await;
    let offers = |listed: &[String], tool: &str| listed.iter().any(|name| name == tool);

    for kind in [PrincipalKind::Router, PrincipalKind::Worker] {
        for tool in registry::tools(kind) {
            let (served, token) = match kind {
                PrincipalKind::Router => (&served, TOKEN),
                PrincipalKind::Worker if tool.name == "memo_retire" => (&served, DREAM),
                PrincipalKind::Worker if offers(&there, tool.name) => (&served, THERE),
                PrincipalKind::Worker if offers(&here, tool.name) => (&served, HERE),
                PrincipalKind::Worker => {
                    assert!(
                        offers(&coder, tool.name),
                        "{} is offered nowhere",
                        tool.name
                    );
                    (&with_code, CODER)
                }
            };
            let (status, reply) = served
                .post(&format!("tool/{}", tool.name), Some(token), json!({}))
                .await;
            assert_ne!(
                status, 403,
                "{kind:?} {}: a refusal before any handler ran: {reply}",
                tool.name
            );
        }
    }
}

const CODER: &str = "worker-coder";

/// A broker whose host has run_code, and `CODER`, a worker on it, with its
/// `/work`. No sandbox starts: `/usr/bin/true` stands in for prlimit and
/// bwrap, so a command does nothing and exits 0, after its files are written.
fn serve_with_run_code(scratch: &Scratch) -> (Runtime, Served, PathBuf) {
    let runtime = scratch.runtime(Arc::new(Holding::default()), true);
    let tickets = scratch.state().join("tickets");
    let mut code = CodeConfig::new(tickets.clone(), Vec::new());
    code.bwrap = PathBuf::from("/usr/bin/true");
    code.prlimit = PathBuf::from("/usr/bin/true");
    let mut parts = workers(None, None);
    parts.code = Some(Arc::new(CodeExecutor::from_config(code).unwrap()));
    let served = serve_on(
        scratch,
        Scripted::new(),
        runtime.facts(),
        1_000_000,
        decide_terms(),
        Nodes::default(),
        parts,
        None,
    );
    let work = tickets.join("tk_files").join("work");
    std::fs::create_dir_all(&work).unwrap();
    let seat = WorkerSeat::new(
        TicketId::new("tk_files"),
        AssignmentId::new("as_1"),
        work.clone(),
        500_000,
    );
    served
        .broker
        .admit_worker(CODER.into(), seat, "You are a worker.".into())
        .unwrap();
    (runtime, served, work)
}

#[tokio::test]
async fn a_worker_writes_and_edits_a_file_in_its_work_with_no_shell_to_quote_it() {
    let scratch = Scratch::new("files");
    let (_runtime, served, work) = serve_with_run_code(&scratch);
    let call = async |tool: &str, args: Value| {
        served
            .post(&format!("tool/{tool}"), Some(CODER), args)
            .await
    };
    assert_eq!(
        served.listed(CODER).await,
        [
            "worker_report",
            "worker_handback",
            "run_code",
            "job_start",
            "job_wait",
            "job_stop",
            "fetch_url",
            "write_file",
            "edit_file"
        ]
    );

    // What a heredoc or a python one-liner would have to quote.
    let source =
        "fn main() {\n    let s = \"it's $HOME `pwd` \\\\n\";\n    println!(\"{s}\");\n}\nEOF\n";
    let (status, reply) = call(
        "write_file",
        json!({"path": "src/bin/main.rs", "content": source}),
    )
    .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(
        reply,
        json!({"path": "/work/src/bin/main.rs", "bytes": source.len(), "created": true})
    );
    let file = work.join("src/bin/main.rs");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), source);

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (status, reply) = call(
        "edit_file",
        json!({
            "path": "/work/src/bin/main.rs",
            "old": "    println!(\"{s}\");\n",
            "new": "    eprintln!(\"{s}\");\n",
        }),
    )
    .await;
    assert_eq!(status, 200, "{reply}");
    let edited = source.replace("println!", "eprintln!");
    assert_eq!(
        reply,
        json!({"path": "/work/src/bin/main.rs", "bytes": edited.len(), "line": 3})
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), edited);
    let mode = std::fs::metadata(&file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755, "an edit keeps the file's mode");

    let (status, reply) = call(
        "write_file",
        json!({"path": "/work/src/bin/main.rs", "content": "fn main() {}\n"}),
    )
    .await;
    assert_eq!((status, &reply["created"]), (200, &json!(false)), "{reply}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn main() {}\n");
    let names: Vec<String> = std::fs::read_dir(file.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["main.rs"], "no temporary file is left");

    // run_code's files map goes through the same writer.
    let (status, reply) = call(
        "run_code",
        json!({"code": "true", "files": {"notes/plan.md": "ship it\n"}}),
    )
    .await;
    assert_eq!((status, &reply["exit_code"]), (200, &json!(0)), "{reply}");
    assert_eq!(
        std::fs::read_to_string(work.join("notes/plan.md")).unwrap(),
        "ship it\n"
    );
}

#[tokio::test]
async fn edit_file_refuses_old_that_is_missing_or_there_twice_and_leaves_the_file() {
    let scratch = Scratch::new("edit_refused");
    let (_runtime, served, work) = serve_with_run_code(&scratch);
    let text = "a = 1\nb = 1\n";
    std::fs::write(work.join("conf.toml"), text).unwrap();
    let edit = |old: &str| {
        let args = json!({"path": "conf.toml", "old": old, "new": "c = 2"});
        served.post("tool/edit_file", Some(CODER), args)
    };
    for (old, says) in [
        ("c = 1", "/work/conf.toml: old is not in it."),
        (" = 1", "/work/conf.toml: old is in it 2 times."),
        ("", "old is empty"),
    ] {
        let (status, reply) = edit(old).await;
        assert_eq!(status, 422, "{old:?}: {reply}");
        let error = reply["error"].as_str().unwrap();
        assert!(error.starts_with(says), "{old:?}: {error}");
    }
    assert_eq!(
        std::fs::read_to_string(work.join("conf.toml")).unwrap(),
        text
    );

    let args = json!({"path": "missing.toml", "old": "a", "new": "b"});
    let (status, reply) = served.post("tool/edit_file", Some(CODER), args).await;
    assert_eq!(status, 404, "{reply}");
    assert!(!work.join("missing.toml").exists());
}

/// The worker can make links in its `/work`, and the broker writes as the
/// host: no link there, on the way or at the end, takes a write outside.
#[tokio::test]
async fn a_path_through_or_to_a_link_in_work_is_refused_and_nothing_is_written_outside() {
    let scratch = Scratch::new("files_links");
    let (_runtime, served, work) = serve_with_run_code(&scratch);
    let outside = scratch.root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret"), "keep").unwrap();
    std::os::unix::fs::symlink(&outside, work.join("escape")).unwrap();
    // `work` is `<root>/state/tickets/tk_files/work`.
    std::os::unix::fs::symlink("../../../..", work.join("up")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret"), work.join("secret")).unwrap();

    for path in [
        "escape/secret",
        "escape/new",
        "escape/deeper/new",
        "/work/up/outside/secret",
        "up/outside/new",
        "secret",
    ] {
        for (tool, args) in [
            ("write_file", json!({"path": path, "content": "pwned"})),
            (
                "edit_file",
                json!({"path": path, "old": "keep", "new": "pwned"}),
            ),
        ] {
            let (status, reply) = served
                .post(&format!("tool/{tool}"), Some(CODER), args)
                .await;
            assert_eq!(status, 403, "{tool} {path}: {reply}");
            let error = reply["error"].as_str().unwrap();
            assert!(error.contains(" is a link"), "{tool} {path}: {error}");
        }
    }
    for path in [
        "../outside/secret",
        "/work/../outside/secret",
        "/etc/hostname",
        "/work",
        "/work/",
        "a\u{0}b",
    ] {
        for (tool, args) in [
            ("write_file", json!({"path": path, "content": "pwned"})),
            (
                "edit_file",
                json!({"path": path, "old": "keep", "new": "pwned"}),
            ),
        ] {
            let (status, reply) = served
                .post(&format!("tool/{tool}"), Some(CODER), args)
                .await;
            assert_eq!(status, 422, "{tool} {path:?}: {reply}");
        }
    }
    let args = json!({"code": "true", "files": {"escape/secret": "pwned"}});
    let (status, reply) = served.post("tool/run_code", Some(CODER), args).await;
    assert_eq!(status, 422, "run_code's files: {reply}");
    assert!(
        reply["error"]
            .as_str()
            .unwrap()
            .contains("/work/escape is a link"),
        "{reply}"
    );

    assert_eq!(
        std::fs::read_to_string(outside.join("secret")).unwrap(),
        "keep"
    );
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
}

/// On a host with only a pstack, a seat holds `read` and the tools that need
/// no resource, and each tool it lacks is refused with the reason.
#[tokio::test]
async fn a_host_with_only_a_pstack_gives_a_seat_read_and_refuses_the_rest_with_why() {
    let scratch = Scratch::new("pstack_only");
    let runtime = scratch.runtime(Arc::new(Holding::default()), true);
    let pstack = Pstack::open(Path::new(env!("CARGO_MANIFEST_DIR")), "poteto-mode").unwrap();
    let served = serve_on(
        &scratch,
        Scripted::new(),
        runtime.facts(),
        1_000_000,
        decide_terms(),
        Nodes::default(),
        workers(None, None),
        Some(Arc::new(pstack)),
    );
    let seat = WorkerSeat::new(
        TicketId::new("tk_pstack"),
        AssignmentId::new("as_1"),
        scratch.state(),
        500_000,
    );
    served
        .broker
        .admit_worker(WORKER.into(), seat, "You are a worker.".into())
        .unwrap();

    let held = ["worker_report", "worker_handback", "fetch_url", "read"];
    assert_eq!(served.broker.worker_tools(None, false), held, "its grants");
    assert_eq!(served.listed(WORKER).await, held, "GET /tools");
    for (tool, why) in [
        ("run_code", "run_code is not available on this host"),
        ("vault_list", "no vault on this host"),
        ("browser_login", "no browser where this ticket runs"),
    ] {
        let (status, reply) = served
            .post(&format!("tool/{tool}"), Some(WORKER), json!({}))
            .await;
        assert_eq!(
            (status, reply["error"].as_str()),
            (403, Some(why)),
            "{tool}: {reply}"
        );
    }
    let (status, reply) = served
        .post(
            "tool/read",
            Some(WORKER),
            json!({"path": "/pstack/SKILL.md"}),
        )
        .await;
    assert_ne!(status, 403, "read is held: {reply}");
}

/// `read` is held per seat. With no pstack on this host, a seat on a
/// repository holds it, bound to its own clone and host notes, and a seat on
/// no repository is refused it with the reason.
#[tokio::test]
async fn without_a_pstack_only_a_seat_on_a_repository_holds_read_of_its_own_clone() {
    const CLONED: &str = "worker-cloned";
    const BARE: &str = "worker-bare";
    let scratch = Scratch::new("repository_read");
    let runtime = scratch.runtime(Arc::new(Holding::default()), true);
    let served = serve_on(
        &scratch,
        Scripted::new(),
        runtime.facts(),
        1_000_000,
        decide_terms(),
        Nodes::default(),
        workers(None, None),
        None,
    );
    let (work, host) = (scratch.root.join("work"), scratch.root.join("host"));
    for dir in [&work, &host] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(work.join("README.md"), "Widgets\n").unwrap();
    let seat = |ticket: &str, work: PathBuf| {
        WorkerSeat::new(
            TicketId::new(ticket),
            AssignmentId::new("as_1"),
            work,
            500_000,
        )
    };
    let cloned = seat("tk_cloned", work).on_repository(host);
    let named = served.broker.seat_tools(&cloned);
    for (token, seat) in [(CLONED, cloned), (BARE, seat("tk_bare", scratch.state()))] {
        served
            .broker
            .admit_worker(token.into(), seat, "You are a worker.".into())
            .unwrap();
    }

    let with_read = ["worker_report", "worker_handback", "fetch_url", "read"];
    let without = &with_read[..3];
    assert_eq!(named, with_read, "its grants, named before it is admitted");
    assert_eq!(served.listed(CLONED).await, with_read, "GET /tools");
    assert_eq!(served.listed(BARE).await, without, "GET /tools");
    assert_eq!(served.broker.worker_tools(None, true), with_read);
    assert_eq!(served.broker.worker_tools(None, false), without);

    let read = |token: &'static str, path: &'static str| {
        served.post("tool/read", Some(token), json!({"path": path}))
    };
    let (status, reply) = read(BARE, "/work/README.md").await;
    assert_eq!(
        (status, reply["error"].as_str()),
        (
            403,
            Some("this ticket has neither pstack nor a repository to read")
        ),
        "{reply}"
    );
    let (status, reply) = read(CLONED, "/work/README.md").await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["lines"].as_str().map(str::trim), Some("1\tWidgets"));
    let (status, reply) = read(CLONED, "/work/../../etc/passwd").await;
    assert_eq!(status, 403, "{reply}");
    let (status, reply) = read(CLONED, "/pstack/skills").await;
    assert!(
        (400..500).contains(&status),
        "no pstack here: {status} {reply}"
    );
}
