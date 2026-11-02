//! The agent broker over HTTP on its Unix socket, with a scripted provider:
//! the Router account's holds and settlements, its daily cap across a
//! restart, the pause gate, identities, the two Router tools against a
//! running runtime, and a worker's account and tools.

use anyhow::Result;
use serde_json::{Value, json};
use solos_agent_core::AllowlistPolicy;
use solos_agent_core::model::{
    Actor, Addressed, Approval, AssignmentId, Candidate, ChannelRef, DecisionKind, InputBody,
    MessageKind, OutboundMessage, ReportStatus, ThreadKey, TicketId, Trigger, TriggerId,
};
use solos_agent_store::{SpendState, SqliteStore};
use solos_swarm::agent::broker::{
    self, Account, AgentBroker, BrokerParts, DecideTerms, Grant, ModelTerms, Principal,
    WorkerParts, WorkerSeat,
};
use solos_swarm::agent::jsonl::{JsonlChannel, JsonlSource};
use solos_swarm::agent::ports::{
    BoxFuture, Channel, Delivery, Executor, Fact, FactSink, Launch, Router,
};
use solos_swarm::agent::scripted::{ScriptedExecutor, ScriptedRouter};
use solos_swarm::agent::{Adapters, AgentConfig, RunOptions, Runtime, STORE_FILE};
use solos_swarm::gateway::{Dispatch, PriceTable, Provider, Upstream, upstream_body};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
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
const TERMS: DecideTerms = DecideTerms {
    default_budget_microusd: 500_000,
    max_budget_microusd: Some(2_000_000),
    swarm: None,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Behaviour {
    Answer,
    Fail,
    NoReceipt,
}

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
            match behaviour {
                Behaviour::Answer => Ok(
                    json!({"id":"receipt-1","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":usage}),
                ),
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
             executor: {{kind: scripted}}\n\
             budgets: {{session_microusd: 500000, router_daily_microusd: 1000000}}\n"
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
    }
}

fn grant(daily_microusd: u64) -> Grant {
    Grant {
        principal: Principal::Router,
        account: Account::Router { daily_microusd },
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
    serve_deciding(scratch, provider, facts, daily, TERMS)
}

fn serve_deciding(
    scratch: &Scratch,
    provider: Arc<Scripted>,
    facts: FactSink,
    daily: u64,
    decide: DecideTerms,
) -> Served {
    let broker = AgentBroker::new(BrokerParts {
        dir: scratch.state(),
        store: scratch.store(),
        provider,
        facts,
        decide,
        workers: Some(WorkerParts {
            terms: terms(),
            code: None,
        }),
    })
    .unwrap();
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
        assert_eq!(ticket.mandate.budget_microusd, GATED);
        assert_eq!(ticket.approval, Approval::Waiting);
        let sent = scratch.sent();
        let [ask] = &sent[..] else { panic!("{sent:?}") };
        assert_eq!(ask.kind, MessageKind::Attention);
        assert!(ask.text.contains("needs approval first"), "{}", ask.text);
    }
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
    let whole = json!({"url": url, "max_bytes": 8 * mebibyte});
    let mut read = 98_304;
    while read < solos_swarm::broker::FETCH_TOTAL_BYTES {
        let (status, page) = served
            .post("tool/fetch_url", Some(WORKER), whole.clone())
            .await;
        assert_eq!(status, 200, "{page}");
        assert_eq!(page["body_bytes"], mebibyte, "no call reads past 1 MiB");
        read += mebibyte as u64;
    }
    let (status, refused) = served.post("tool/fetch_url", Some(WORKER), whole).await;
    assert_eq!(status, 403);
    let why = refused["error"].as_str().unwrap();
    assert!(why.contains("read budget exhausted after 32 MiB"), "{why}");
    server.abort();
}
