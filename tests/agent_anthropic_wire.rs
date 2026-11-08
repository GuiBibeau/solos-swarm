//! The agent broker's Messages facade on the Anthropic wire, over HTTP on
//! its Unix socket: a Claude Code worker's calls forwarded untranslated, with
//! the broker's key, to a fake upstream that speaks Anthropic's Messages and
//! its server-sent events. Each call is held and settled on its Ticket's
//! account from the reply's usage, and streams back as it arrives; what the
//! contract refuses never reaches the upstream.

use axum::{
    Json,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use solos_agent_core::model::{AssignmentId, TicketId};
use solos_agent_store::{Spend, SpendState, SqliteStore, Tokens};
use solos_swarm::agent::broker::{
    self, Account, AgentBroker, BrokerParts, DecideTerms, Grant, ModelTerms, Principal,
    WorkerParts, WorkerSeat,
};
use solos_swarm::agent::scripted::{ScriptedExecutor, ScriptedRouter};
use solos_swarm::agent::{Adapters, RunOptions, Runtime, STORE_FILE};
use solos_swarm::gateway::forward::{Forward, WebSearch};
use solos_swarm::gateway::{AnthropicMessages, Dispatch, PriceTable, Provider, Upstream};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

const MODEL: &str = "deepseek-flash";
const MAX_OUTPUT: u64 = 2_048;
const PRICES: PriceTable = PriceTable {
    input_microusd_per_million: 300_000,
    output_microusd_per_million: 1_200_000,
    cache_hit_microusd_per_million: Some(6_000),
};
/// 156 uncached input tokens, 1,024 cache reads and 32 output tokens, at
/// `PRICES`: 156 at 0.3, 1,024 at 0.006 and 32 at 1.2 microusd.
const COST: u64 = 92;
const WORKER: &str = "worker-token-7f3a9c";
const ROUTER: &str = "router-token-51d0e2";
/// The provider key the broker holds.
const KEY: &str = "sk-deepseek-test-key";
const FIRST_TURN: &str = include_str!("fixtures/anthropic/claude-code-first-turn.json");
const SEARCH: WebSearch = WebSearch {
    max_uses: 2,
    microusd_per_search: 750,
    result_tokens: 1_000,
};

/// What the fake upstream answers a call with, in order.
enum Answer {
    /// Server-sent events, sent as they come down the channel.
    Stream(mpsc::Receiver<Result<Bytes, std::io::Error>>),
    /// One whole Message.
    Message(Value),
    /// An error status and its body.
    Status(u16, Value),
}

/// One call as the upstream received it.
#[derive(Clone, Debug)]
struct Received {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
    /// Every byte of the request: its path, headers and body.
    fn raw(&self) -> String {
        let headers: Vec<String> = self
            .headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}"))
            .collect();
        format!(
            "{}\n{}\n{}",
            self.path,
            headers.join("\n"),
            String::from_utf8_lossy(&self.body)
        )
    }
}

#[derive(Default)]
struct Fake {
    answers: Mutex<VecDeque<Answer>>,
    received: Mutex<Vec<Received>>,
}

impl Fake {
    fn answer(&self, answer: Answer) {
        self.answers.lock().unwrap().push_back(answer);
    }
    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
}

async fn upstream(
    State(fake): State<Arc<Fake>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    fake.received.lock().unwrap().push(Received {
        path: uri.to_string(),
        headers: headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect(),
        body: body.to_vec(),
    });
    let answer = fake.answers.lock().unwrap().pop_front();
    match answer {
        Some(Answer::Stream(chunks)) => {
            let body = Body::from_stream(futures_util::stream::unfold(
                chunks,
                |mut chunks| async move { chunks.recv().await.map(|chunk| (chunk, chunks)) },
            ));
            ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
        }
        Some(Answer::Message(message)) => Json(message).into_response(),
        Some(Answer::Status(status, body)) => {
            (StatusCode::from_u16(status).unwrap(), Json(body)).into_response()
        }
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The broker's provider on the Anthropic wire only.
struct Forwarding {
    messages: AnthropicMessages,
}

impl Provider for Forwarding {
    fn dispatch(&self, _: Value) -> Dispatch {
        Box::pin(async { anyhow::bail!("the chat wire is not used here") })
    }
    fn messages(&self) -> Option<&AnthropicMessages> {
        Some(&self.messages)
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
            "anthropic-wire-{name}-{}-{}",
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
    fn committed(&self) -> u64 {
        self.store().committed("ticket:tk_1", 0).unwrap()
    }
    fn spend(&self) -> Vec<Spend> {
        self.store().spend("ticket:tk_1").unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn terms() -> ModelTerms {
    ModelTerms {
        upstream: Upstream::deepseek(MODEL),
        prices: PRICES,
        max_input_bytes: 65_536,
        max_output_tokens: MAX_OUTPUT,
        call_limit: None,
        temperature: None,
        thinking: None,
    }
}

/// A broker on the state directory's socket with a worker on `tk_1`'s
/// account and a Router, forwarding to the fake upstream.
struct Served {
    /// Never runs: it only holds the end of the broker's fact channel.
    _runtime: Runtime,
    servers: Vec<tokio::task::JoinHandle<()>>,
    client: reqwest::Client,
    fake: Arc<Fake>,
}

impl Drop for Served {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
    }
}

async fn serve(scratch: &Scratch, budget_microusd: u64, web_search: Option<WebSearch>) -> Served {
    let fake = Arc::new(Fake::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new()
        .route("/anthropic/v1/messages", post(upstream))
        .with_state(fake.clone());
    let fake_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = Arc::new(Forwarding {
        messages: AnthropicMessages {
            endpoint: format!("http://127.0.0.1:{port}/anthropic/v1/messages"),
            client: reqwest::Client::new(),
            key: Zeroizing::new(KEY.into()),
            web_search,
        },
    });
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
    let broker = AgentBroker::new(BrokerParts {
        dir: scratch.state(),
        store: scratch.store(),
        provider,
        facts: runtime.facts(),
        decide: DecideTerms {
            default_budget_microusd: Some(500_000),
            ..DecideTerms::default()
        },
        workers: Some(WorkerParts {
            terms: terms(),
            code: None,
            vault: None,
            browser: None,
            allow_private_networks: false,
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
            daily_microusd: Some(1_000_000),
        },
        terms: terms(),
        system_prompt: None,
    };
    broker.admit(ROUTER.into(), router).unwrap();
    let (listener, socket) = broker::bind(&scratch.state()).unwrap();
    let app = broker.router().unwrap();
    let broker_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder()
        .unix_socket(socket)
        .build()
        .unwrap();
    Served {
        _runtime: runtime,
        servers: vec![fake_server, broker_server],
        client,
        fake,
    }
}

impl Served {
    async fn messages(&self, key: &str, body: &Value) -> reqwest::Response {
        self.client
            .post("http://broker/v1/messages?beta=true")
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", key)
            .json(body)
            .send()
            .await
            .unwrap()
    }
    async fn json(&self, key: &str, body: &Value) -> (u16, Value) {
        let response = self.messages(key, body).await;
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }
}

/// Claude Code's first worker turn, as recorded, on the broker's model.
fn asks(stream: bool) -> Value {
    let mut body: Value = serde_json::from_str(FIRST_TURN).unwrap();
    body["model"] = json!(MODEL);
    body["stream"] = json!(stream);
    body
}

/// The hold of `asks(stream)` on these terms.
fn hold(stream: bool, web_search: Option<&WebSearch>) -> u64 {
    let sent = Forward::read(&asks(stream))
        .unwrap()
        .upstream(MODEL, MAX_OUTPUT, None, web_search);
    let bytes = serde_json::to_vec(&sent).unwrap().len();
    match web_search {
        Some(search) => search.hold(&PRICES, bytes, MAX_OUTPUT),
        None => PRICES.request_hold(bytes, MAX_OUTPUT),
    }
}

fn usage(output: u64) -> Value {
    json!({"input_tokens": 156, "cache_creation_input_tokens": 0,
           "cache_read_input_tokens": 1024, "output_tokens": output})
}

/// A reply's events as DeepSeek frames them: a thinking block, a ping, a
/// text block, then the usage; `output` is its output tokens.
fn events(id: &str, output: u64) -> Vec<String> {
    let mut start = usage(0);
    start["input_tokens"] = json!(1180);
    start["cache_read_input_tokens"] = json!(0);
    [
        (
            "message_start",
            json!({"type": "message_start", "message": {"id": id, "type": "message",
            "role": "assistant", "model": MODEL, "content": [], "stop_reason": null,
            "stop_sequence": null, "usage": start}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        ),
        ("ping", json!({"type": "ping"})),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "thinking_delta", "thinking": "I should report."}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "signature_delta", "signature": id}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 1,
            "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "text_delta", "text": "Reported."}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 1}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": usage(output)}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ]
    .iter()
    .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
    .collect()
}

/// A stream the fake sends at once: `frames`, then its end.
fn sent(frames: &[String]) -> Answer {
    let (chunks, piped) = mpsc::channel(frames.len().max(1));
    for frame in frames {
        chunks.try_send(Ok(Bytes::from(frame.clone()))).unwrap();
    }
    Answer::Stream(piped)
}

fn message(id: &str, usage: Value) -> Value {
    json!({"id": id, "type": "message", "role": "assistant", "model": MODEL,
           "content": [{"type": "text", "text": "Reported."}],
           "stop_reason": "end_turn", "stop_sequence": null, "usage": usage})
}

/// Read `response` until it has at least `bytes`, or its end.
async fn read_at_least(response: &mut reqwest::Response, bytes: usize) -> Vec<u8> {
    let mut read = Vec::new();
    while read.len() < bytes {
        let chunk = tokio::time::timeout(Duration::from_secs(10), response.chunk())
            .await
            .expect("no bytes within 10 s")
            .unwrap();
        match chunk {
            Some(chunk) => read.extend_from_slice(&chunk),
            None => break,
        }
    }
    read
}

fn tokens(spend: &Spend) -> Option<Tokens> {
    spend.tokens
}

#[tokio::test]
async fn a_streamed_call_reaches_the_session_as_it_arrives_and_settles_from_its_usage() {
    let scratch = Scratch::new("stream");
    let served = serve(&scratch, 500_000, None).await;
    let (chunks, piped) = mpsc::channel(16);
    served.fake.answer(Answer::Stream(piped));
    let frames = events("msg_live", 32);
    chunks
        .send(Ok(Bytes::from(frames[0].clone())))
        .await
        .unwrap();

    let mut response = served.messages(WORKER, &asks(true)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut read = read_at_least(&mut response, frames[0].len()).await;
    assert_eq!(
        read,
        frames[0].as_bytes(),
        "the start came through before the upstream sent the rest"
    );
    let spend = scratch.spend();
    assert_eq!(spend.len(), 1);
    assert_eq!(spend[0].state, SpendState::Held, "held while it streams");
    assert_eq!(spend[0].hold_microusd, hold(true, None));

    for frame in &frames[1..] {
        chunks.send(Ok(Bytes::from(frame.clone()))).await.unwrap();
    }
    drop(chunks);
    while let Some(chunk) = response.chunk().await.unwrap() {
        read.extend_from_slice(&chunk);
    }
    assert_eq!(
        String::from_utf8(read).unwrap(),
        frames.concat(),
        "the session reads the provider's own events, byte for byte"
    );
    assert_eq!(scratch.committed(), COST, "settled by the stream's end");
    let spend = scratch.spend();
    assert_eq!(spend[0].state, SpendState::Settled);
    assert_eq!(spend[0].request_id.as_deref(), Some("msg_live"));
    assert_eq!(
        tokens(&spend[0]),
        Some(Tokens {
            input: 1180,
            cached_input: 1024,
            output: 32
        }),
        "cache reads are input, at the cache-hit rate"
    );

    let received = served.fake.received();
    assert_eq!(received.len(), 1);
    let call = &received[0];
    assert_eq!(call.path, "/anthropic/v1/messages");
    assert_eq!(call.header("x-api-key"), Some(KEY), "the broker's key");
    assert_eq!(call.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(call.header("authorization"), None);
    assert!(
        !call.raw().contains(WORKER),
        "the session's token never goes upstream:\n{}",
        call.raw()
    );
    let body = call.json();
    let asked = asks(true);
    assert_eq!(
        (&body["model"], &body["max_tokens"], &body["stream"]),
        (&json!(MODEL), &json!(MAX_OUTPUT), &json!(true)),
        "the broker's ceiling, below Claude Code's 32,000"
    );
    for field in ["messages", "system", "tools", "thinking", "output_config"] {
        assert_eq!(body[field], asked[field], "{field} as the session sent it");
    }
    assert!(body.get("metadata").is_none() && body.get("context_management").is_none());
}

#[tokio::test]
async fn a_session_that_hangs_up_mid_stream_still_settles_from_its_usage() {
    let scratch = Scratch::new("hangup");
    let served = serve(&scratch, 500_000, None).await;
    let (chunks, piped) = mpsc::channel(16);
    served.fake.answer(Answer::Stream(piped));
    let frames = events("msg_hangup", 32);
    chunks
        .send(Ok(Bytes::from(frames[0].clone())))
        .await
        .unwrap();

    let mut response = served.messages(WORKER, &asks(true)).await;
    assert_eq!(response.status(), 200);
    read_at_least(&mut response, frames[0].len()).await;
    drop(response);

    // The provider sends the rest after the session is gone, and the
    // broker reads it to the end for the call's usage.
    for frame in &frames[1..] {
        chunks.send(Ok(Bytes::from(frame.clone()))).await.unwrap();
    }
    drop(chunks);
    let settled = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if scratch
                .spend()
                .first()
                .is_some_and(|spend| spend.state == SpendState::Settled)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(settled.is_ok(), "the hung-up call settled");
    assert_eq!(scratch.committed(), COST);
    let spend = scratch.spend();
    assert_eq!(spend[0].request_id.as_deref(), Some("msg_hangup"));
    assert_eq!(
        tokens(&spend[0]),
        Some(Tokens {
            input: 1180,
            cached_input: 1024,
            output: 32
        })
    );
}

#[tokio::test]
async fn a_whole_message_comes_back_with_its_cost() {
    let scratch = Scratch::new("whole");
    let served = serve(&scratch, 500_000, None).await;
    let reply = message("msg_whole", usage(32));
    served.fake.answer(Answer::Message(reply.clone()));

    let (status, answer) = served.json(WORKER, &asks(false)).await;

    assert_eq!(status, 200, "{answer}");
    let mut expected = reply;
    expected["swarm_cost_microusd"] = json!(COST);
    assert_eq!(answer, expected);
    assert_eq!(scratch.committed(), COST);
    assert_eq!(served.fake.received()[0].json()["stream"], json!(false));

    served
        .fake
        .answer(Answer::Message(message("msg_long", usage(32))));
    let mut long = asks(false);
    long["model"] = json!(format!("{MODEL}[1m]"));
    let (status, _) = served.json(WORKER, &long).await;
    assert_eq!(status, 200, "the long-context name is the same model");
    assert_eq!(served.fake.received()[1].json()["model"], json!(MODEL));
}

#[tokio::test]
async fn what_the_contract_refuses_never_reaches_the_upstream() {
    let scratch = Scratch::new("contract");
    let served = serve(&scratch, 500_000, None).await;
    let refused = |status: u16, kind: &str, why: &str, answer: (u16, Value)| {
        assert_eq!(answer.0, status, "{why}: {}", answer.1);
        assert_eq!(answer.1["error"]["type"], kind, "{why}");
        let message = answer.1["error"]["message"].as_str().unwrap().to_owned();
        assert!(message.contains(why), "{why}: {message}");
    };
    let edited = |edit: &dyn Fn(&mut Value)| {
        let mut body = asks(true);
        edit(&mut body);
        body
    };

    for (why, body) in [
        (
            "outside contract",
            edited(&|b| b["model"] = json!("claude-opus-5")),
        ),
        (
            "unadmitted provider tool",
            edited(&|b| b["tools"][0]["name"] = json!("Bash")),
        ),
        (
            "unadmitted provider tool",
            edited(&|b| {
                b["tools"] =
                    json!([{"type": "web_search_20250305", "name": "web_search", "max_uses": 8}])
            }),
        ),
        (
            "exceeds the model limit",
            edited(&|b| b["messages"][0]["content"][1]["text"] = json!("x".repeat(70_000))),
        ),
    ] {
        refused(
            403,
            "permission_error",
            why,
            served.json(WORKER, &body).await,
        );
    }
    refused(
        403,
        "permission_error",
        "unadmitted provider tool",
        served.json(ROUTER, &asks(true)).await,
    );
    refused(
        400,
        "invalid_request_error",
        "max_tokens must be at least 1",
        served
            .json(WORKER, &edited(&|b| b["max_tokens"] = json!(0)))
            .await,
    );
    refused(
        403,
        "permission_error",
        "invalid identity",
        served.json("nobody", &asks(true)).await,
    );
    std::fs::write(scratch.state().join("PAUSE"), "").unwrap();
    refused(
        503,
        "overloaded_error",
        "paused",
        served.json(WORKER, &asks(true)).await,
    );
    std::fs::remove_file(scratch.state().join("PAUSE")).unwrap();

    assert!(
        served.fake.received().is_empty(),
        "nothing reached the upstream"
    );
    assert!(scratch.spend().is_empty(), "and nothing was held");
}

#[tokio::test]
async fn a_provider_refusal_releases_the_hold_and_says_why() {
    let scratch = Scratch::new("refusal");
    let served = serve(&scratch, 500_000, None).await;
    served.fake.answer(Answer::Status(
        400,
        json!({"error": {"type": "invalid_request_error", "message": "messages.1: bad tool_result"}}),
    ));

    let (status, answer) = served.json(WORKER, &asks(true)).await;

    assert_eq!(status, 403, "{answer}");
    let why = answer["error"]["message"].as_str().unwrap();
    assert!(
        why.contains("provider returned HTTP 400: messages.1: bad tool_result"),
        "{why}"
    );
    assert_eq!(scratch.spend()[0].state, SpendState::Released);
    assert_eq!(scratch.committed(), 0);
}

#[tokio::test]
async fn a_call_whose_usage_cannot_be_trusted_is_charged_at_its_hold() {
    let scratch = Scratch::new("unknown");
    let served = serve(&scratch, 500_000, None).await;

    served
        .fake
        .answer(Answer::Message(message("msg_over", usage(MAX_OUTPUT + 1))));
    let (status, answer) = served.json(WORKER, &asks(false)).await;
    assert_eq!(status, 403, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("provider usage exceeds contract"),
        "{answer}"
    );
    assert_eq!(scratch.spend()[0].state, SpendState::Unknown);
    assert_eq!(scratch.committed(), hold(false, None), "past the ceiling");

    let frames = events("msg_cut", 32);
    let cut = frames.len() - 2;
    served.fake.answer(sent(&frames[..cut]));
    let mut response = served.messages(WORKER, &asks(true)).await;
    assert_eq!(response.status(), 200);
    let mut read = Vec::new();
    let ended = loop {
        match response.chunk().await {
            Ok(Some(chunk)) => read.extend_from_slice(&chunk),
            Ok(None) => break Ok(()),
            Err(err) => break Err(err),
        }
    };
    assert!(ended.is_err(), "the session sees its stream fail");
    assert_eq!(String::from_utf8(read).unwrap(), frames[..cut].concat());
    assert_eq!(scratch.spend()[1].state, SpendState::Unknown);
    assert_eq!(
        scratch.committed(),
        hold(false, None) + hold(true, None),
        "a stream cut before its usage"
    );
}

#[tokio::test]
async fn web_search_is_a_tool_the_broker_adds_holds_for_and_bills_per_search() {
    let scratch = Scratch::new("search");
    let searching = hold(false, Some(&SEARCH));
    assert!(searching > hold(false, None));
    // One call fits; a second search hold on top of its cost does not,
    // though a plain hold would.
    let budget = searching + COST + 750 - 1;
    let served = serve(&scratch, budget, Some(SEARCH)).await;
    let mut searched = usage(32);
    searched["server_tool_use"] = json!({"web_search_requests": 2, "tool_search": 1});
    served
        .fake
        .answer(Answer::Message(message("msg_search", searched)));

    let (status, answer) = served.json(WORKER, &asks(false)).await;

    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer["swarm_cost_microusd"],
        json!(COST + 750),
        "the tokens and the one search that found results"
    );
    assert_eq!(scratch.spend()[0].hold_microusd, searching);
    assert_eq!(scratch.committed(), COST + 750);
    let tools = served.fake.received()[0].json()["tools"].clone();
    assert_eq!(
        tools,
        json!([asks(false)["tools"][0], {"type": "web_search_20250305", "name": "web_search", "max_uses": 2}])
    );

    let (status, refused) = served.json(WORKER, &asks(false)).await;
    assert_eq!(status, 403, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("tk_1's budget is reached"),
        "{refused}"
    );
    assert_eq!(served.fake.received().len(), 1);
}
