//! The Slack adapters against a fake Slack: an HTTP server for the Web API
//! methods they call, and a Socket Mode WebSocket that sends envelopes and
//! records acknowledgements. No real Slack is contacted.

use anyhow::Result;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use solos_agent_core::model::{
    Actor, ActorKind, Addressed, Approval, Ask, AssignmentId, ChannelRef, Effect, EndedBy,
    InputBody, MessageId, MessageKind, OutboundMessage, SecretGrant, SecretId, ThreadKey, TicketId,
    TicketOutcome, Trigger, TriggerId,
};
use solos_agent_core::{AllowlistPolicy, Repositories};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::broker::{
    self as agent_broker, AgentBroker, BrokerParts, DecideTerms, ModelTerms, VaultParts,
    WorkerParts, WorkerSeat,
};
use solos_swarm::agent::jsonl::JsonlSource;
use solos_swarm::agent::links::TicketLinks;
use solos_swarm::agent::ports::{
    Answer, Applied, BoxFuture, Channel, Delivered, Delivery, Executor, Fact, FactSink, Launch,
    RetryAfter, SendOptions, SendOutcome, Source,
};
use solos_swarm::agent::scripted::{ScriptedExecutor, ScriptedRouter};
use solos_swarm::agent::slack::{
    ALLOW_SECRET_ACTION, APPROVE_ACTION, DENY_SECRET_ACTION, METADATA_EVENT, STOP_ACTION, SlackApi,
    SlackChannel, SlackSource, Token,
};
use solos_swarm::agent::{Adapters, RunOptions, Runtime, STORE_FILE, ServeOptions, Summary, serve};
use solos_swarm::gateway::{Dispatch, PriceTable, Provider, Upstream};
use solos_swarm::vault::{
    Approval as ItemApproval, ItemKind, KeyScheme, NewItem, SandboxView, SecretValue, Vault,
    VaultConfig,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

const LIMIT: Duration = Duration::from_secs(30);
const APP_TOKEN: &str = "xapp-fake-app";
const BOT_TOKEN: &str = "xoxb-fake-bot";
const BOT_USER: &str = "UBOT";
const BOT_ID: &str = "BBOT";
/// Above the Policy's approval threshold.
const GATED: u64 = 5_000_000;
const BUDGET: u64 = 500_000;

// ---- The fake Slack ----

/// How the fake answers the next `chat.postMessage`.
enum Reply {
    Error(&'static str),
    Status(u16, Option<&'static str>),
    /// Keep the message but never answer, as if the agent crashed while
    /// Slack accepted it.
    Hang,
}

struct Post {
    at: Instant,
    body: Value,
}

#[derive(Default)]
struct Slack {
    opens: u32,
    posts: Vec<Post>,
    /// What Slack holds, for `conversations.replies`.
    held: Vec<Value>,
    replies_calls: u32,
    acks: Vec<String>,
    script: VecDeque<Reply>,
}

#[derive(Clone)]
struct Shared {
    slack: Arc<Mutex<Slack>>,
    socket: SocketAddr,
}

/// What the test tells one socket connection to do.
enum Out {
    Text(String),
    Close,
    /// Drop the TCP stream with no closing handshake.
    Cut,
}

struct Conn {
    out: mpsc::UnboundedSender<Out>,
}

impl Conn {
    fn send(&self, text: String) {
        self.out.send(Out::Text(text)).unwrap();
    }
    fn close(&self) {
        self.out.send(Out::Close).unwrap();
    }
    fn cut(&self) {
        self.out.send(Out::Cut).unwrap();
    }
}

struct Fake {
    shared: Shared,
    api: String,
    connections: tokio::sync::Mutex<mpsc::UnboundedReceiver<Conn>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
}

fn refused(code: &str) -> Response {
    Json(json!({ "ok": false, "error": code })).into_response()
}

async fn connections_open(State(shared): State<Shared>, headers: HeaderMap) -> Response {
    if bearer(&headers) != APP_TOKEN {
        return refused("invalid_auth");
    }
    let mut slack = shared.slack.lock().unwrap();
    slack.opens += 1;
    let url = format!("ws://{}/link/?ticket=t{}", shared.socket, slack.opens);
    Json(json!({ "ok": true, "url": url })).into_response()
}

async fn auth_test(headers: HeaderMap) -> Response {
    if bearer(&headers) != BOT_TOKEN {
        return refused("invalid_auth");
    }
    Json(json!({ "ok": true, "user_id": BOT_USER, "bot_id": BOT_ID, "team_id": "T1" }))
        .into_response()
}

async fn post_message(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if bearer(&headers) != BOT_TOKEN {
        return refused("invalid_auth");
    }
    let (reply, ts) = {
        let mut slack = shared.slack.lock().unwrap();
        slack.posts.push(Post {
            at: Instant::now(),
            body: body.clone(),
        });
        let reply = slack.script.pop_front();
        let keep = matches!(reply, None | Some(Reply::Hang));
        let ts = format!("900.{:06}", slack.posts.len());
        if keep {
            slack.held.push(json!({
                "type": "message",
                "channel": body["channel"],
                "thread_ts": body["thread_ts"],
                "ts": ts,
                "bot_id": BOT_ID,
                "text": body["text"],
                "metadata": body["metadata"],
            }));
        }
        (reply, ts)
    };
    match reply {
        None => Json(json!({ "ok": true, "channel": body["channel"], "ts": ts })).into_response(),
        Some(Reply::Error(code)) => refused(code),
        Some(Reply::Status(status, retry_after)) => {
            let mut response = (StatusCode::from_u16(status).unwrap(), "busy").into_response();
            if let Some(seconds) = retry_after {
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, seconds.parse().unwrap());
            }
            response
        }
        Some(Reply::Hang) => std::future::pending().await,
    }
}

/// One message per page, so a search has to follow the cursor.
async fn conversations_replies(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if bearer(&headers) != BOT_TOKEN {
        return refused("invalid_auth");
    }
    let mut slack = shared.slack.lock().unwrap();
    slack.replies_calls += 1;
    let channel = query.get("channel").cloned().unwrap_or_default();
    if !matches!(channel.as_str(), "C1" | "D1") {
        return refused("channel_not_found");
    }
    let thread = query.get("ts").cloned().unwrap_or_default();
    let mut found: Vec<Value> = slack
        .held
        .iter()
        .filter(|m| m["channel"] == channel.as_str() && m["thread_ts"] == thread.as_str())
        .cloned()
        .collect();
    if query.get("include_all_metadata").map(String::as_str) != Some("true") {
        for message in &mut found {
            message.as_object_mut().unwrap().remove("metadata");
        }
    }
    let at: usize = query
        .get("cursor")
        .map_or(0, |cursor| cursor.parse().unwrap());
    let page: Vec<Value> = found.iter().skip(at).take(1).cloned().collect();
    let next = if at + 1 < found.len() {
        (at + 1).to_string()
    } else {
        String::new()
    };
    Json(json!({
        "ok": true,
        "messages": page,
        "has_more": !next.is_empty(),
        "response_metadata": { "next_cursor": next },
    }))
    .into_response()
}

/// Accepts sockets; each greets with `hello`, records acks, and hands the
/// test a [`Conn`] to drive it.
async fn sockets(listener: TcpListener, conns: mpsc::UnboundedSender<Conn>, shared: Shared) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
            continue;
        };
        let hello = json!({ "type": "hello", "num_connections": 1 }).to_string();
        if socket.send(Message::text(hello)).await.is_err() {
            continue;
        }
        let (out, mut orders) = mpsc::unbounded_channel();
        let _ = conns.send(Conn { out });
        let slack = shared.slack.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    order = orders.recv() => match order {
                        Some(Out::Text(text)) => {
                            if socket.send(Message::text(text)).await.is_err() {
                                return;
                            }
                        }
                        Some(Out::Close) | None => {
                            let _ = socket.close(None).await;
                            while let Some(Ok(_)) = socket.next().await {}
                            return;
                        }
                        Some(Out::Cut) => return,
                    },
                    frame = socket.next() => match frame {
                        Some(Ok(Message::Text(text))) => {
                            let ack: Value = serde_json::from_str(text.as_str()).unwrap();
                            let id = ack["envelope_id"].as_str().unwrap().to_owned();
                            slack.lock().unwrap().acks.push(id);
                        }
                        Some(Ok(_)) => {}
                        _ => return,
                    },
                }
            }
        });
    }
}

impl Fake {
    async fn start() -> Self {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let shared = Shared {
            slack: Arc::default(),
            socket: socket.local_addr().unwrap(),
        };
        let api = format!("http://{}/api/", http.local_addr().unwrap());
        let app = axum::Router::new()
            .route("/api/apps.connections.open", post(connections_open))
            .route("/api/auth.test", post(auth_test))
            .route("/api/chat.postMessage", post(post_message))
            .route("/api/conversations.replies", get(conversations_replies))
            .with_state(shared.clone());
        let (conns, connections) = mpsc::unbounded_channel();
        let tasks = vec![
            tokio::spawn(async move { axum::serve(http, app).await.unwrap() }),
            tokio::spawn(sockets(socket, conns, shared.clone())),
        ];
        Self {
            shared,
            api,
            connections: tokio::sync::Mutex::new(connections),
            tasks,
        }
    }

    fn script(&self, replies: impl IntoIterator<Item = Reply>) {
        self.slack().script.extend(replies);
    }

    fn slack(&self) -> std::sync::MutexGuard<'_, Slack> {
        self.shared.slack.lock().unwrap()
    }

    /// The next socket the agent opens.
    async fn connection(&self) -> Conn {
        let next = async { self.connections.lock().await.recv().await };
        tokio::time::timeout(LIMIT, next)
            .await
            .expect("the agent did not connect")
            .unwrap()
    }

    async fn until<T>(&self, what: &str, check: impl Fn(&Slack) -> Option<T>) -> T {
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Some(found) = check(&self.slack()) {
                return found;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn until_posts(&self, n: usize) -> Vec<Value> {
        self.until(&format!("{n} posts"), |slack| {
            (slack.posts.len() >= n).then(|| slack.posts.iter().map(|p| p.body.clone()).collect())
        })
        .await
    }

    async fn until_acks(&self, n: usize) -> Vec<String> {
        self.until(&format!("{n} acks"), |slack| {
            (slack.acks.len() >= n).then(|| slack.acks.clone())
        })
        .await
    }

    async fn until_text(&self, prefix: &str) -> Value {
        self.until(&format!("a post starting {prefix:?}"), |slack| {
            slack
                .posts
                .iter()
                .map(|p| &p.body)
                .find(|body| body["text"].as_str().unwrap().starts_with(prefix))
                .cloned()
        })
        .await
    }

    fn api(&self) -> Arc<SlackApi> {
        Arc::new(SlackApi::new(&self.api, Token::new(BOT_TOKEN)).unwrap())
    }

    fn source(&self, follow: bool) -> Box<dyn Source> {
        let channels = ["C1".to_owned(), "D1".to_owned()].into();
        let source = SlackSource::new(self.api(), Token::new(APP_TOKEN), channels, follow)
            .with_backoff(Duration::from_millis(10), Duration::from_millis(100));
        Box::new(source)
    }

    fn channel(&self) -> Arc<dyn Channel> {
        Arc::new(SlackChannel::new(self.api()))
    }

    /// A Channel whose results and questions link to `{public_url}/tickets/{id}`.
    fn channel_linking_to(&self, public_url: &str) -> Arc<dyn Channel> {
        let links = TicketLinks::new(public_url).unwrap();
        Arc::new(SlackChannel::new(self.api()).with_links(links))
    }
}

// ---- Envelopes and messages ----

fn event(envelope: &str, event: Value) -> String {
    json!({
        "envelope_id": envelope,
        "type": "events_api",
        "accepts_response_payload": false,
        "retry_attempt": 0,
        "payload": {
            "type": "event_callback",
            "team_id": "T1",
            "event_id": format!("Ev-{envelope}"),
            "event": event,
        },
    })
    .to_string()
}

fn message(channel: &str, ts: &str, user: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "channel": channel,
        "channel_type": "channel",
        "user": user,
        "text": text,
        "ts": ts,
    })
}

fn mention(ts: &str, user: &str, text: &str) -> Value {
    json!({
        "type": "app_mention",
        "channel": "C1",
        "user": user,
        "text": format!("<@{BOT_USER}> {text}"),
        "ts": ts,
        "event_ts": ts,
    })
}

fn click(
    envelope: &str,
    action: &str,
    ticket: &str,
    (user, name): (&str, &str),
    thread: &str,
) -> String {
    json!({
        "envelope_id": envelope,
        "type": "interactive",
        "accepts_response_payload": false,
        "payload": {
            "type": "block_actions",
            "user": { "id": user, "username": name, "name": name, "team_id": "T1" },
            "container": {
                "type": "message",
                "message_ts": "900.000001",
                "channel_id": "C1",
                "is_ephemeral": false,
                "thread_ts": thread,
            },
            "channel": { "id": "C1", "name": "ops" },
            "message": { "ts": "900.000001", "thread_ts": thread, "bot_id": BOT_ID },
            "actions": [{
                "type": "button",
                "action_id": action,
                "block_id": "solos_approval",
                "value": ticket,
                "action_ts": format!("{envelope}.5"),
            }],
        },
    })
    .to_string()
}

const ALICE: (&str, &str) = ("UALICE", "alice");
const BOB: (&str, &str) = ("UBOB", "bob");
const EVE: (&str, &str) = ("UEVE", "eve");

// ---- The agent ----

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agent-slack-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        Self { root }
    }
    fn store(&self) -> SqliteStore {
        SqliteStore::open(self.root.join("state").join(STORE_FILE)).unwrap()
    }
    fn triggers(&self, triggers: &[Trigger]) -> Box<dyn Source> {
        let path = self.root.join("triggers.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        for trigger in triggers {
            writeln!(file, "{}", serde_json::to_string(trigger).unwrap()).unwrap();
        }
        Box::new(JsonlSource::new(path, false))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn policy() -> AllowlistPolicy {
    AllowlistPolicy {
        people: ["slack:UALICE".to_owned(), "slack:UBOB".to_owned()].into(),
        bots: ["slack:BDEPLOY".to_owned()].into(),
        approvers: ["slack:UALICE".to_owned()].into(),
        auto_approve_microusd: 1_000_000,
        ..AllowlistPolicy::default()
    }
}

struct Agent<'a> {
    scratch: &'a Scratch,
    source: Box<dyn Source>,
    channel: Option<Arc<dyn Channel>>,
    executor: Arc<dyn Executor>,
    budget: u64,
    until_idle: bool,
}

impl Agent<'_> {
    fn runtime(self) -> Runtime {
        let adapters = Adapters {
            sources: vec![self.source],
            channels: self
                .channel
                .map(|channel| BTreeMap::from([("slack".to_owned(), channel)]))
                .unwrap_or_default(),
            router: Arc::new(ScriptedRouter::new(self.budget)),
            executor: self.executor,
        };
        let options = RunOptions {
            until_idle: self.until_idle,
            tick: Duration::from_millis(10),
            retry_base: Duration::from_millis(10),
            retry_max: Duration::from_millis(500),
            remind_after: None,
        };
        Runtime::new(self.scratch.store(), policy(), adapters, options)
    }
}

/// Run until `driver` is done with the fake, then stop the runtime.
async fn drive<T>(runtime: Runtime, driver: impl Future<Output = T>) -> (Summary, T) {
    let (stop, stopped) = oneshot::channel::<()>();
    let driver = async {
        let value = driver.await;
        let _ = stop.send(());
        value
    };
    let run = runtime.run(async {
        let _ = stopped.await;
    });
    let (summary, value) = tokio::time::timeout(LIMIT, async { tokio::join!(run, driver) })
        .await
        .expect("the run did not end");
    (summary.unwrap(), value)
}

fn request(id: &str, thread: &str, text: &str) -> Trigger {
    Trigger {
        id: TriggerId::new(id),
        source: "slack".into(),
        at_ms: 1_000,
        actor: Actor::person("UALICE"),
        addressed: Addressed::Mention,
        text: text.into(),
        threads: vec![ThreadKey::new(format!("slack:C1:{thread}"))],
        reply_to: vec![address("C1", thread)],
        meta: json!({}),
    }
}

fn address(channel: &str, thread: &str) -> ChannelRef {
    ChannelRef {
        source: "slack".into(),
        address: BTreeMap::from([
            ("channel".to_owned(), channel.to_owned()),
            ("thread_ts".to_owned(), thread.to_owned()),
        ]),
    }
}

fn outbound(id: &str, kind: MessageKind, channel: &str, text: &str) -> OutboundMessage {
    OutboundMessage {
        id: MessageId::new(id),
        channel: address(channel, "100.000001"),
        kind,
        text: text.into(),
        ticket: Some(TicketId::new("tk_1")),
        ask: None,
    }
}

fn message_id(post: &Value) -> &str {
    assert_eq!(post["metadata"]["event_type"], METADATA_EVENT, "{post}");
    post["metadata"]["event_payload"]["message_id"]
        .as_str()
        .unwrap()
}

fn assert_settled(store: &SqliteStore) {
    let pending: Vec<_> = store
        .effects()
        .unwrap()
        .into_iter()
        .filter(|q| !q.done)
        .collect();
    assert!(pending.is_empty(), "effects not done: {pending:?}");
}

/// Never finishes a launch, so the run can be cut short mid-flight.
struct Hanging;

impl Executor for Hanging {
    fn launch(&self, _: Launch, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(std::future::pending())
    }
    fn deliver(&self, _: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

// ---- Socket Mode Source ----

#[tokio::test]
async fn events_become_triggers_and_everything_else_is_dropped_yet_acknowledged() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("mapping");
    let bot = json!({
        "type": "message", "subtype": "bot_message", "channel": "C1", "bot_id": "BDEPLOY",
        "username": "deploy", "text": "Deploy 42 finished", "ts": "300.000001",
    });
    let own = message(
        "C1",
        "400.000001",
        BOT_USER,
        "On it: summarise the incident.",
    );
    let own_bot = json!({
        "type": "message", "subtype": "bot_message", "channel": "C1", "bot_id": BOT_ID,
        "text": "Done.", "ts": "400.000002",
    });
    let edited = json!({
        "type": "message", "subtype": "message_changed", "channel": "C1", "ts": "400.000003",
        "message": { "user": "UALICE", "text": "edited" },
    });
    let mut direct = message("D1", "200.000001", "UALICE", "status?");
    direct["channel_type"] = json!("im");
    let mut reply = message("C1", "100.000002", "UBOB", "also the timeline");
    reply["thread_ts"] = json!("100.000001");
    let envelopes = [
        event(
            "e1",
            mention("100.000001", "UALICE", "summarise the incident"),
        ),
        // The same message through the `message.channels` subscription.
        event(
            "e2",
            message(
                "C1",
                "100.000001",
                "UALICE",
                &format!("<@{BOT_USER}> summarise the incident"),
            ),
        ),
        event("e3", reply),
        event("e4", direct),
        event("e5", bot),
        event("e6", own),
        event("e7", own_bot),
        event(
            "e8",
            message("C9", "500.000001", "UALICE", "<@UBOT> elsewhere"),
        ),
        event("e9", edited),
        event(
            "e10",
            json!({ "type": "reaction_added", "user": "UALICE", "reaction": "eyes" }),
        ),
        json!({ "envelope_id": "e11", "type": "slash_commands", "payload": {} }).to_string(),
    ];
    let runtime = Agent {
        scratch: &scratch,
        source: fake.source(false),
        channel: None,
        executor: Arc::new(ScriptedExecutor),
        budget: BUDGET,
        until_idle: true,
    }
    .runtime();
    let driver = async {
        let conn = fake.connection().await;
        for envelope in &envelopes {
            conn.send(envelope.clone());
        }
        let acks = fake.until_acks(envelopes.len()).await;
        conn.close();
        acks
    };

    let (summary, acks) = tokio::time::timeout(LIMIT, async {
        tokio::join!(runtime.run(std::future::pending()), driver)
    })
    .await
    .expect("the run did not go idle");

    let ids: Vec<String> = (1..=envelopes.len()).map(|n| format!("e{n}")).collect();
    assert_eq!(acks, ids, "every envelope is acknowledged, in order");
    let summary = summary.unwrap();
    assert_eq!(
        (summary.triggers_received, summary.triggers_duplicate),
        (4, 1)
    );
    let store = scratch.store();
    let triggers = store.triggers().unwrap();
    let found: Vec<&str> = triggers.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(
        found,
        [
            "slack:C1:100.000001",
            "slack:C1:100.000002",
            "slack:C1:300.000001",
            "slack:D1:200.000001",
        ],
        "own messages, other channels, edits and other events stay out"
    );
    let [asked, followed, deployed, direct] = &triggers[..] else {
        unreachable!()
    };
    assert_eq!(asked.source, "slack");
    assert_eq!(asked.actor, Actor::person("UALICE"));
    assert_eq!(asked.addressed, Addressed::Mention);
    assert_eq!(
        asked.text, "summarise the incident",
        "the bot's mention is dropped"
    );
    assert_eq!(asked.at_ms, 100_000);
    assert_eq!(asked.threads, [ThreadKey::new("slack:C1:100.000001")]);
    assert_eq!(asked.reply_to, [address("C1", "100.000001")]);
    assert_eq!(asked.meta["channel"], "C1");
    assert_eq!(asked.meta["ts"], "100.000001");
    assert_eq!(asked.meta["user"], "UALICE");
    assert_eq!(asked.meta["team"], "T1");
    assert_eq!(asked.meta["event_id"], "Ev-e1");

    assert_eq!(followed.actor, Actor::person("UBOB"));
    assert_eq!(followed.addressed, Addressed::Unaddressed);
    assert_eq!(
        followed.threads,
        [ThreadKey::new("slack:C1:100.000001")],
        "a reply joins its thread"
    );
    assert_eq!(followed.reply_to, [address("C1", "100.000001")]);
    assert_eq!(followed.meta["thread_ts"], "100.000001");

    assert_eq!(deployed.actor.kind, ActorKind::Bot);
    assert_eq!(deployed.actor.id, "BDEPLOY");
    assert_eq!(deployed.actor.name.as_deref(), Some("deploy"));
    assert_eq!(deployed.addressed, Addressed::Unaddressed);
    assert_eq!(deployed.meta["bot_id"], "BDEPLOY");

    assert_eq!(direct.addressed, Addressed::Direct);
    assert_eq!(direct.threads, [ThreadKey::new("slack:D1:200.000001")]);
    assert_eq!(direct.reply_to, [address("D1", "200.000001")]);
}

#[tokio::test]
async fn a_disconnect_or_a_cut_socket_reconnects_and_replies_go_in_the_thread() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("reconnect");
    let runtime = Agent {
        scratch: &scratch,
        source: fake.source(true),
        channel: Some(fake.channel()),
        executor: Arc::new(ScriptedExecutor),
        budget: BUDGET,
        until_idle: false,
    }
    .runtime();
    let driver = async {
        let first = fake.connection().await;
        first.send(json!({ "type": "disconnect", "reason": "refresh_requested" }).to_string());
        let second = fake.connection().await;
        second.cut();
        let third = fake.connection().await;
        third.send(event(
            "e1",
            mention("500.000001", "UALICE", "draft the release notes"),
        ));
        fake.until_posts(2).await
    };

    let (summary, posts) = drive(runtime, driver).await;

    assert_eq!(summary.triggers_received, 1);
    assert_eq!(fake.slack().opens, 3, "one socket per connection");
    assert_eq!(fake.slack().acks, ["e1"]);
    for post in &posts {
        assert_eq!(
            (&post["channel"], &post["thread_ts"]),
            (&json!("C1"), &json!("500.000001"))
        );
    }
    assert_eq!(posts[0]["text"], "Looking into it.");
    assert!(
        posts[1]["text"]
            .as_str()
            .unwrap()
            .starts_with("Done: draft the release notes.")
    );
    let store = scratch.store();
    let sent: Vec<String> = store
        .effects()
        .unwrap()
        .into_iter()
        .filter_map(|q| match q.effect {
            Effect::Send(message) => Some(message.id.to_string()),
            _ => None,
        })
        .collect();
    let posted: Vec<String> = posts.iter().map(|p| message_id(p).to_owned()).collect();
    assert_eq!(posted, sent, "each post carries its message id");
}

// ---- Web API Channel ----

#[tokio::test]
async fn a_reply_is_escaped_shaped_for_chat_and_an_approval_request_has_buttons() {
    let fake = Fake::start().await;
    let channel = fake.channel();
    let long = format!("<!channel> fixed & tested. {}", "word ".repeat(1_000));
    let approval = SendOptions {
        ask: Some(Ask::Budget),
        ..SendOptions::default()
    };
    let sends = [
        (
            outbound("ms_result", MessageKind::Result, "C1", &long),
            SendOptions::default(),
        ),
        (
            outbound("ms_ack", MessageKind::Acknowledgement, "C1", &long),
            SendOptions::default(),
        ),
        (
            outbound(
                "ms_ask",
                MessageKind::Attention,
                "C1",
                "Migrate needs approval first: one worker session with a budget of $5.00.",
            ),
            approval,
        ),
    ];
    for (message, options) in sends {
        assert_eq!(
            channel.send(message, options).await.unwrap(),
            SendOutcome::Sent
        );
    }

    let posts = fake.until_posts(3).await;
    let [result, ack, ask] = &posts[..] else {
        unreachable!()
    };
    for (post, id) in [(result, "ms_result"), (ack, "ms_ack"), (ask, "ms_ask")] {
        assert_eq!(message_id(post), id);
        assert_eq!(
            (&post["channel"], &post["thread_ts"]),
            (&json!("C1"), &json!("100.000001"))
        );
    }
    let text = result["text"].as_str().unwrap();
    assert!(
        text.starts_with("&lt;!channel&gt; fixed &amp; tested."),
        "{text}"
    );
    assert!(text.ends_with("of 5026 characters shown.]_"), "{text}");
    assert!(text.chars().count() < 3_100, "{}", text.chars().count());
    assert!(ack["text"].as_str().unwrap().chars().count() < 600);
    assert!(result.get("blocks").is_none() && ack.get("blocks").is_none());
    let buttons = &ask["blocks"][1]["elements"];
    assert_eq!(buttons[0]["action_id"], APPROVE_ACTION);
    assert_eq!(buttons[1]["action_id"], STOP_ACTION);
    assert_eq!(
        (&buttons[0]["value"], &buttons[1]["value"]),
        (&json!("tk_1"), &json!("tk_1"))
    );
    assert_eq!(ask["blocks"][0]["text"]["text"], ask["text"]);
    assert_eq!(format!("{:?}", Token::new(BOT_TOKEN)), "Token(..)");
}

#[tokio::test]
async fn markdown_is_posted_as_mrkdwn_and_a_result_links_the_tickets_page_once() {
    let fake = Fake::start().await;
    let channel = fake.channel_linking_to("https://agent.example.com");
    let page = "https://agent.example.com/tickets/tk_1";
    let answer = "## Merged\n\n| PR | What | When |\n|---|---|---|\n\
        | [#65](https://github.com/acme/widgets/pull/65) | **fix** agent answers | 17:54 |\n\n\
        See [the docs](https://example.com/a?b=1&c=2). <!channel>";
    let long = "A paragraph that goes on for a while.\n\n".repeat(150);
    let approval = SendOptions {
        ask: Some(Ask::Budget),
        ..SendOptions::default()
    };
    let result = || outbound("ms_result", MessageKind::Result, "C1", answer);
    let sends = [
        (
            outbound(
                "ms_ack",
                MessageKind::Acknowledgement,
                "C1",
                "Looking into it.",
            ),
            SendOptions::default(),
        ),
        (
            outbound(
                "ms_ask",
                MessageKind::Attention,
                "C1",
                "Approve the $5.00 budget?",
            ),
            approval,
        ),
        (result(), SendOptions::default()),
        (
            outbound("ms_long", MessageKind::Result, "C1", &long),
            SendOptions::default(),
        ),
    ];
    for (message, options) in sends {
        channel.send(message, options).await.unwrap();
    }
    let repeat = SendOptions {
        repeat: true,
        ..SendOptions::default()
    };
    channel.send(result(), repeat).await.unwrap();

    let posts = fake.until_posts(4).await;
    assert_eq!(
        posts.len(),
        4,
        "the repeat found its message and posted nothing"
    );
    let [ack, ask, result, long] = &posts[..] else {
        unreachable!()
    };
    assert_eq!(ack["text"], "Looking into it.");
    assert_eq!(
        ask["text"],
        format!("Approve the $5.00 budget?\n\n<{page}|Task details>")
    );
    assert_eq!(ask["blocks"][0]["text"]["text"], ask["text"]);
    assert_eq!(
        result["text"],
        format!(
            "*Merged*\n\n_PR · What · When_\n\
             *<https://github.com/acme/widgets/pull/65|#65>* *fix* agent answers · 17:54\n\n\
             See <https://example.com/a?b=1&amp;c=2|the docs>. &lt;!channel&gt;\n\n\
             <{page}|Task details>"
        )
    );
    let text = long["text"].as_str().unwrap();
    assert!(
        text.ends_with(&format!("\n\n…the rest is on the <{page}|task page>")),
        "{}",
        &text[text.len() - 120..]
    );
    assert!(text.chars().count() < 3_200);
    assert!(!text.contains("Cut short") && !text.contains("Task details"));
}

#[tokio::test]
async fn a_repeat_looks_in_the_thread_first_and_posts_only_what_is_missing() {
    let fake = Fake::start().await;
    let channel = fake.channel();
    let repeat = SendOptions {
        repeat: true,
        ..SendOptions::default()
    };
    let first = || outbound("ms_1", MessageKind::Result, "C1", "first");
    let second = || outbound("ms_2", MessageKind::Result, "C1", "second");

    channel.send(first(), SendOptions::default()).await.unwrap();
    assert_eq!(
        fake.slack().replies_calls,
        0,
        "a first attempt posts at once"
    );
    channel.send(second(), repeat.clone()).await.unwrap();
    let calls = fake.slack().replies_calls;
    assert_eq!(calls, 1, "a repeat searches, and ms_2 was not there");
    for message in [first(), second()] {
        assert_eq!(
            channel.send(message, repeat.clone()).await.unwrap(),
            SendOutcome::Sent
        );
    }

    let posts = fake.until_posts(2).await;
    let ids: Vec<&str> = posts.iter().map(message_id).collect();
    assert_eq!(ids, ["ms_1", "ms_2"], "each message was posted once");
    assert!(
        fake.slack().replies_calls >= calls + 3,
        "the search followed the cursor past ms_1 to find ms_2"
    );
}

#[tokio::test]
async fn gone_is_for_a_conversation_that_can_never_take_it_and_the_rest_is_retried() {
    let fake = Fake::start().await;
    let channel = fake.channel();
    let send = |id: &str| {
        channel.send(
            outbound(id, MessageKind::Result, "C1", "done"),
            SendOptions::default(),
        )
    };
    fake.script([
        Reply::Error("channel_not_found"),
        Reply::Error("is_archived"),
        Reply::Status(429, Some("7")),
        Reply::Status(503, None),
        Reply::Error("internal_error"),
    ]);

    assert_eq!(send("ms_1").await.unwrap(), SendOutcome::Gone);
    assert_eq!(send("ms_2").await.unwrap(), SendOutcome::Gone);
    let limited = send("ms_3").await.unwrap_err();
    let wait = limited.chain().find_map(|e| e.downcast_ref::<RetryAfter>());
    assert_eq!(
        wait,
        Some(&RetryAfter(Duration::from_secs(7))),
        "{limited:#}"
    );
    let unavailable = send("ms_4").await.unwrap_err();
    assert!(
        unavailable
            .chain()
            .all(|e| e.downcast_ref::<RetryAfter>().is_none())
    );
    assert!(
        format!("{unavailable:#}").contains("503"),
        "{unavailable:#}"
    );
    assert!(send("ms_5").await.is_err(), "an unknown refusal is retried");
    let lost = outbound("ms_6", MessageKind::Result, "C404", "done");
    let repeat = SendOptions {
        repeat: true,
        ..SendOptions::default()
    };
    assert_eq!(channel.send(lost, repeat).await.unwrap(), SendOutcome::Gone);
    assert_eq!(fake.slack().posts.len(), 5, "the lost channel got no post");
}

// ---- The runtime with Slack ----

#[tokio::test]
async fn a_crash_after_a_post_and_before_its_done_mark_does_not_post_twice() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("crash");
    let asked = [request("t1", "100.000001", "Fix the flaky test")];
    fake.script([Reply::Hang]);
    let first = Agent {
        scratch: &scratch,
        source: scratch.triggers(&asked),
        channel: Some(fake.channel()),
        executor: Arc::new(Hanging),
        budget: BUDGET,
        until_idle: false,
    }
    .runtime();
    tokio::select! {
        ended = first.run(std::future::pending()) => panic!("the runtime ended: {ended:?}"),
        posts = fake.until_posts(1) => assert_eq!(posts[0]["text"], "Looking into it."),
    }
    let acknowledged = message_id(&fake.slack().posts[0].body).to_owned();
    let pending =
        scratch.store().effects().unwrap().into_iter().any(|q| {
            matches!(&q.effect, Effect::Send(m) if m.id.as_str() == acknowledged) && !q.done
        });
    assert!(
        pending,
        "Slack holds the post, but it was never marked sent"
    );

    let restarted = Agent {
        scratch: &scratch,
        source: scratch.triggers(&asked),
        channel: Some(fake.channel()),
        executor: Arc::new(ScriptedExecutor),
        budget: BUDGET,
        until_idle: true,
    }
    .runtime();
    tokio::time::timeout(LIMIT, restarted.run(std::future::pending()))
        .await
        .expect("the restart did not go idle")
        .unwrap();

    let posts = fake.until_posts(2).await;
    let ids: Vec<&str> = posts.iter().map(message_id).collect();
    assert_eq!(ids.iter().filter(|id| **id == acknowledged).count(), 1);
    assert_eq!(posts.len(), 2, "the acknowledgement once, then the result");
    assert!(posts[1]["text"].as_str().unwrap().starts_with("Done:"));
    assert_eq!(
        fake.slack().replies_calls,
        1,
        "only the redone send searched the thread"
    );
    assert_settled(&scratch.store());
}

#[tokio::test]
async fn a_rate_limited_post_waits_as_long_as_slack_asks() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("retry-after");
    fake.script([Reply::Status(429, Some("1"))]);
    let runtime = Agent {
        scratch: &scratch,
        source: scratch.triggers(&[request("t1", "100.000001", "Fix the flaky test")]),
        channel: Some(fake.channel()),
        executor: Arc::new(ScriptedExecutor),
        budget: BUDGET,
        until_idle: true,
    }
    .runtime();

    let summary = tokio::time::timeout(LIMIT, runtime.run(std::future::pending()))
        .await
        .expect("the run did not go idle")
        .unwrap();

    assert_eq!(summary.effect_failures, 1);
    let slack = fake.slack();
    let [limited, retried, _result] = &slack.posts[..] else {
        panic!("{} posts", slack.posts.len())
    };
    assert_eq!(message_id(&limited.body), message_id(&retried.body));
    let waited = retried.at - limited.at;
    assert!(
        waited >= Duration::from_millis(950),
        "the retry waited {waited:?}, not Retry-After's second"
    );
    assert_settled(&scratch.store());
}

/// Alice asks for work above the approval threshold; returns the Ticket id
/// the approval request's buttons carry.
async fn ask_for_gated_work(fake: &Fake, conn: &Conn) -> String {
    conn.send(event(
        "e1",
        mention("700.000001", "UALICE", "migrate the billing db"),
    ));
    let ask = fake
        .until_text("migrate the billing db needs approval first")
        .await;
    assert_eq!(
        (&ask["channel"], &ask["thread_ts"]),
        (&json!("C1"), &json!("700.000001"))
    );
    let buttons = &ask["blocks"][1]["elements"];
    assert_eq!(buttons[0]["action_id"], APPROVE_ACTION);
    assert_eq!(buttons[1]["action_id"], STOP_ACTION);
    assert_eq!(buttons[0]["value"], buttons[1]["value"]);
    buttons[0]["value"].as_str().unwrap().to_owned()
}

fn gated<'a>(fake: &Fake, scratch: &'a Scratch) -> Agent<'a> {
    Agent {
        scratch,
        source: fake.source(true),
        channel: Some(fake.channel()),
        executor: Arc::new(ScriptedExecutor),
        budget: GATED,
        until_idle: false,
    }
}

fn launched(store: &SqliteStore) -> bool {
    store
        .effects()
        .unwrap()
        .iter()
        .any(|q| matches!(q.effect, Effect::Launch { .. }))
}

#[tokio::test]
async fn the_approve_button_launches_work_that_waited_for_approval() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("approve");
    let driver = async {
        let conn = fake.connection().await;
        let ticket = ask_for_gated_work(&fake, &conn).await;
        assert!(
            !launched(&scratch.store()),
            "nothing runs before the approval"
        );
        conn.send(click("e2", APPROVE_ACTION, &ticket, ALICE, "700.000001"));
        let approved = fake.until_text("Approved by alice.").await;
        let result = fake.until_text("Done: migrate the billing db.").await;
        (ticket, approved, result)
    };

    let (_, (ticket, approved, result)) = drive(gated(&fake, &scratch).runtime(), driver).await;

    for post in [&approved, &result] {
        assert_eq!(
            (&post["channel"], &post["thread_ts"]),
            (&json!("C1"), &json!("700.000001"))
        );
    }
    assert_eq!(fake.slack().acks, ["e1", "e2"]);
    let store = scratch.store();
    let found = store.ticket(&TicketId::new(&ticket)).unwrap();
    let Approval::Granted { by, .. } = &found.approval else {
        panic!("{:?}", found.approval)
    };
    assert_eq!(by.id, "UALICE");
    assert_eq!(
        found.ended.map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
}

#[tokio::test]
async fn the_stop_button_stops_work_that_waited_for_approval() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("stop");
    let driver = async {
        let conn = fake.connection().await;
        let ticket = ask_for_gated_work(&fake, &conn).await;
        conn.send(click("e2", STOP_ACTION, &ticket, ALICE, "700.000001"));
        fake.until_text("Stopped by alice.").await;
        ticket
    };

    let (_, ticket) = drive(gated(&fake, &scratch).runtime(), driver).await;

    let store = scratch.store();
    let found = store.ticket(&TicketId::new(&ticket)).unwrap();
    let ended = found.ended.unwrap();
    assert_eq!(
        (ended.by, ended.outcome),
        (EndedBy::Person, TicketOutcome::Stopped)
    );
    assert!(!launched(&store), "the stopped work never launched");
    assert_eq!(fake.slack().posts.len(), 2, "the request, then the stop");
}

#[tokio::test]
async fn a_click_from_someone_the_policy_does_not_allow_changes_nothing() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("refused");
    let driver = async {
        let conn = fake.connection().await;
        let ticket = ask_for_gated_work(&fake, &conn).await;
        conn.send(click("e2", APPROVE_ACTION, &ticket, BOB, "700.000001"));
        fake.until_text("bob is not allowed to approve budgets.")
            .await;
        conn.send(click("e3", STOP_ACTION, &ticket, EVE, "700.000001"));
        fake.until_text("eve is not allowed to stop this work.")
            .await;
        ticket
    };

    let (_, ticket) = drive(gated(&fake, &scratch).runtime(), driver).await;

    let store = scratch.store();
    let found = store.ticket(&TicketId::new(&ticket)).unwrap();
    assert_eq!(found.approval, Approval::Waiting);
    assert!(found.ended.is_none());
    assert!(!launched(&store));
}

const REQUEST: &str = "Triage my notifications";
const LABEL: &str = "sign in to https://github.com as gui";
const THREAD: &str = "700.000001";

fn secret() -> SecretId {
    SecretId::try_from("local/github-gui".to_owned()).unwrap()
}

struct AsksForAKeyTwice {
    answerable_by: Vec<String>,
    answers: Mutex<Vec<Answer>>,
    delivered: Mutex<Vec<Delivered>>,
}

impl AsksForAKeyTwice {
    fn new(answerable_by: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            answerable_by: answerable_by.iter().map(|s| (*s).to_owned()).collect(),
            answers: Mutex::default(),
            delivered: Mutex::default(),
        })
    }

    fn answers(&self) -> Vec<Answer> {
        self.answers.lock().unwrap().clone()
    }

    fn verdicts(&self) -> Vec<(bool, String)> {
        let handed = self.delivered.lock().unwrap();
        handed
            .iter()
            .filter_map(|d| match &d.input.body {
                InputBody::Secret {
                    secret: s,
                    granted,
                    by,
                } if *s == secret() => Some((*granted, by.id.clone())),
                _ => None,
            })
            .collect()
    }

    fn typed(&self) -> Vec<String> {
        let handed = self.delivered.lock().unwrap();
        handed
            .iter()
            .filter_map(|d| d.trigger.as_ref().map(|t| t.text.clone()))
            .collect()
    }
}

impl Executor for AsksForAKeyTwice {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let started = Fact::Started {
                ticket: launch.ticket.clone(),
                assignment: launch.assignment,
            };
            facts.send(started).await?;
            for _ in 0..2 {
                let ask = Fact::SecretAsk {
                    ticket: launch.ticket.clone(),
                    secret: secret(),
                    label: LABEL.into(),
                    answerable_by: self.answerable_by.clone(),
                };
                let answer = facts.ask(ask).await?;
                self.answers.lock().unwrap().push(answer);
            }
            Ok(())
        })
    }
    fn deliver(&self, delivery: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        self.delivered.lock().unwrap().extend(delivery.inputs);
        Box::pin(async { Ok(()) })
    }
    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

async fn until_some<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Some(found) = check() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn keyed<'a>(fake: &Fake, scratch: &'a Scratch, executor: &Arc<AsksForAKeyTwice>) -> Agent<'a> {
    Agent {
        scratch,
        source: fake.source(true),
        channel: Some(fake.channel()),
        executor: executor.clone(),
        budget: BUDGET,
        until_idle: false,
    }
}

async fn click_on(conn: &Conn, ask: &Value, allow: bool, who: (&str, &str), envelope: &str) {
    let buttons = &ask["blocks"][1]["elements"];
    let button = &buttons[usize::from(!allow)];
    conn.send(click(
        envelope,
        button["action_id"].as_str().unwrap(),
        button["value"].as_str().unwrap(),
        who,
        THREAD,
    ));
}

fn grant(scratch: &Scratch) -> Option<SecretGrant> {
    scratch.store().tickets().unwrap()[0]
        .secrets
        .get(&secret())
        .cloned()
}

async fn flow<T>(
    fake: &Fake,
    scratch: &Scratch,
    executor: &Arc<AsksForAKeyTwice>,
    then: impl AsyncFnOnce(&Conn, Value) -> T,
) -> T {
    let driver = async {
        let conn = fake.connection().await;
        conn.send(event("e1", mention(THREAD, "UALICE", REQUEST)));
        let ask = fake.until_text(&format!("{REQUEST} wants to")).await;
        until_some("two answers to the ask", || {
            (executor.answers().len() == 2).then_some(())
        })
        .await;
        until_some("every message sent", || {
            let pending = scratch.store().pending_effects().unwrap();
            pending
                .iter()
                .all(|q| !matches!(q.effect, Effect::Send(_)))
                .then_some(())
        })
        .await;
        then(&conn, ask).await
    };
    drive(keyed(fake, scratch, executor).runtime(), driver)
        .await
        .1
}

#[tokio::test]
async fn one_ask_posts_one_message_with_allow_and_deny_and_a_repeated_send_posts_nothing() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-ask");
    let executor = AsksForAKeyTwice::new(&["slack:UALICE"]);

    let (ask, outcome) = flow(&fake, &scratch, &executor, async |_, ask| {
        let store = scratch.store();
        let sent = store
            .effects()
            .unwrap()
            .into_iter()
            .find_map(|q| match q.effect {
                Effect::Send(m) if m.ask.is_some() => Some(m),
                _ => None,
            })
            .expect("the ask was committed");
        let again = SendOptions {
            repeat: true,
            ask: sent.ask.clone(),
        };
        let outcome = fake.channel().send(sent, again).await.unwrap();
        (ask, outcome)
    })
    .await;

    let ticket = scratch.store().tickets().unwrap()[0].id.to_string();
    assert_eq!(
        (&ask["channel"], &ask["thread_ts"]),
        (&json!("C1"), &json!(THREAD))
    );
    assert_eq!(
        ask["text"],
        format!("{REQUEST} wants to {LABEL} for the first time.")
    );
    assert_eq!(ask["blocks"][0]["text"]["text"], ask["text"]);
    let buttons = ask["blocks"][1]["elements"].as_array().unwrap();
    let shown: Vec<(&str, &str, &str)> = buttons
        .iter()
        .map(|b| {
            (
                b["action_id"].as_str().unwrap(),
                b["text"]["text"].as_str().unwrap(),
                b["style"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            (ALLOW_SECRET_ACTION, "Allow for this ticket", "primary"),
            (DENY_SECRET_ACTION, "Deny", "danger"),
        ]
    );
    for button in buttons {
        let value: Value = serde_json::from_str(button["value"].as_str().unwrap()).unwrap();
        assert_eq!(
            value,
            json!({ "ticket": ticket, "secret": "local/github-gui" })
        );
    }

    let answers = executor.answers();
    let [first, second] = &answers[..] else {
        panic!("{answers:?}")
    };
    let Ok(Applied::Secret(SecretGrant::Waiting {
        label,
        answerable_by,
        ..
    })) = first
    else {
        panic!("the first ask waits: {first:?}")
    };
    assert_eq!(label, LABEL);
    assert_eq!(answerable_by, &["slack:UALICE"]);
    assert_eq!(second, first, "the second ask finds the first");
    assert_eq!(outcome, SendOutcome::Sent);
    assert!(
        fake.slack().replies_calls >= 1,
        "the repeat looked in the thread"
    );
    let posts = fake.slack().posts.len();
    assert_eq!(posts, 2, "the acknowledgement and one ask, nothing more");
    let asks = scratch
        .store()
        .effects()
        .unwrap()
        .into_iter()
        .filter(|q| matches!(&q.effect, Effect::Send(m) if m.ask.is_some()))
        .count();
    assert_eq!(asks, 1, "the Engine committed one ask");
}

#[tokio::test]
async fn allow_by_a_person_on_the_items_list_grants_it_and_tells_the_worker() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-allow");
    let executor = AsksForAKeyTwice::new(&["slack:UALICE"]);

    let reply = flow(&fake, &scratch, &executor, async |conn, ask| {
        click_on(conn, &ask, true, ALICE, "e2").await;
        let reply = fake.until_text("Allowed by alice").await;
        until_some("the verdict handed to the worker", || {
            (!executor.verdicts().is_empty()).then_some(())
        })
        .await;
        reply
    })
    .await;

    assert_eq!(
        reply["text"],
        "Allowed by alice: local/github-gui for this ticket."
    );
    assert_eq!(
        (&reply["channel"], &reply["thread_ts"]),
        (&json!("C1"), &json!(THREAD))
    );
    let Some(SecretGrant::Granted { by, .. }) = grant(&scratch) else {
        panic!("{:?}", grant(&scratch))
    };
    assert_eq!(
        (by.id.as_str(), by.name.as_deref()),
        ("UALICE", Some("alice"))
    );
    assert_eq!(executor.verdicts(), [(true, "UALICE".to_owned())]);
}

#[tokio::test]
async fn deny_records_denied_and_tells_the_worker() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-deny");
    let executor = AsksForAKeyTwice::new(&["slack:UALICE"]);

    let reply = flow(&fake, &scratch, &executor, async |conn, ask| {
        click_on(conn, &ask, false, ALICE, "e2").await;
        let reply = fake.until_text("Denied by alice").await;
        until_some("the verdict handed to the worker", || {
            (!executor.verdicts().is_empty()).then_some(())
        })
        .await;
        reply
    })
    .await;

    assert_eq!(
        reply["text"],
        "Denied by alice: local/github-gui for this ticket."
    );
    assert!(matches!(grant(&scratch), Some(SecretGrant::Denied { .. })));
    assert_eq!(executor.verdicts(), [(false, "UALICE".to_owned())]);
}

#[tokio::test]
async fn an_approver_outside_the_items_list_and_a_non_approver_with_no_list_are_refused() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-refused-listed");
    let listed = AsksForAKeyTwice::new(&["slack:UBOB"]);
    let refusal = flow(&fake, &scratch, &listed, async |conn, ask| {
        click_on(conn, &ask, true, ALICE, "e2").await;
        fake.until_text("Nothing changed").await
    })
    .await;
    assert_eq!(
        refusal["text"],
        "Nothing changed: alice may not answer for local/github-gui."
    );
    assert!(matches!(grant(&scratch), Some(SecretGrant::Waiting { .. })));
    assert!(listed.verdicts().is_empty(), "the worker was told nothing");

    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-refused-open");
    let open = AsksForAKeyTwice::new(&[]);
    let refusal = flow(&fake, &scratch, &open, async |conn, ask| {
        click_on(conn, &ask, true, BOB, "e2").await;
        fake.until_text("Nothing changed").await
    })
    .await;
    assert_eq!(
        refusal["text"],
        "Nothing changed: bob may not answer for local/github-gui."
    );
    assert!(matches!(grant(&scratch), Some(SecretGrant::Waiting { .. })));
    assert!(open.verdicts().is_empty());
}

#[tokio::test]
async fn a_second_click_is_refused_and_changes_nothing() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-twice");
    let executor = AsksForAKeyTwice::new(&["slack:UALICE", "slack:UBOB"]);

    let (first, second, third) = flow(&fake, &scratch, &executor, async |conn, ask| {
        click_on(conn, &ask, true, ALICE, "e2").await;
        let first = fake.until_text("Allowed by alice").await;
        click_on(conn, &ask, false, BOB, "e3").await;
        let second = fake.until_text("Nothing changed").await;
        click_on(conn, &ask, true, ALICE, "e4").await;
        fake.until_acks(4).await;
        let third = fake
            .until("both refusals", |slack| {
                let refusals: Vec<Value> = slack
                    .posts
                    .iter()
                    .map(|p| p.body.clone())
                    .filter(|b| b["text"].as_str().unwrap().starts_with("Nothing changed"))
                    .collect();
                (refusals.len() == 2).then(|| refusals[1].clone())
            })
            .await;
        (first, second, third)
    })
    .await;

    assert_eq!(
        first["text"],
        "Allowed by alice: local/github-gui for this ticket."
    );
    for refusal in [&second, &third] {
        assert_eq!(
            refusal["text"],
            "Nothing changed: local/github-gui does not wait for an answer."
        );
    }
    let Some(SecretGrant::Granted { by, .. }) = grant(&scratch) else {
        panic!("{:?}", grant(&scratch))
    };
    assert_eq!(by.id, "UALICE", "the first answer stands");
    assert_eq!(
        executor.verdicts(),
        [(true, "UALICE".to_owned())],
        "the worker heard one answer"
    );
}

#[tokio::test]
async fn a_click_whose_value_does_not_parse_is_dropped_and_acknowledged() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-bad-value");
    let executor = AsksForAKeyTwice::new(&["slack:UALICE"]);

    flow(&fake, &scratch, &executor, async |conn, ask| {
        for (n, value) in [
            "not json".to_owned(),
            json!({ "ticket": "tk_1", "secret": "no-slash" }).to_string(),
            json!({ "ticket": "tk_1" }).to_string(),
            json!({ "ticket": "tk_1", "secret": "local/github-gui", "extra": 1 }).to_string(),
        ]
        .into_iter()
        .enumerate()
        {
            conn.send(click(
                &format!("bad{n}"),
                ALLOW_SECRET_ACTION,
                &value,
                ALICE,
                THREAD,
            ));
        }
        fake.until_acks(5).await;
        click_on(conn, &ask, true, ALICE, "good").await;
        fake.until_text("Allowed by alice").await;
    })
    .await;

    let posts = fake.slack().posts.len();
    assert_eq!(posts, 3, "the acknowledgement, the ask and one answer");
    assert!(matches!(grant(&scratch), Some(SecretGrant::Granted { .. })));
}

#[tokio::test]
async fn typed_allow_in_the_thread_grants_nothing_and_reaches_the_worker_as_a_trigger() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-typed");
    let executor = AsksForAKeyTwice::new(&["slack:UALICE", "slack:UBOB"]);

    flow(&fake, &scratch, &executor, async |conn, _| {
        let mut typed = message("C1", "700.000002", "UALICE", "allow");
        typed["thread_ts"] = json!(THREAD);
        conn.send(event("e2", typed));
        let mut also = message("C1", "700.000003", "UBOB", "allow local/github-gui");
        also["thread_ts"] = json!(THREAD);
        conn.send(event("e3", also));
        until_some("both messages handed to the worker", || {
            (executor.typed().len() == 2).then_some(())
        })
        .await;
    })
    .await;

    let mut typed = executor.typed();
    typed.sort();
    assert_eq!(
        typed,
        ["allow", "allow local/github-gui"],
        "typed text is only context for the worker"
    );
    assert!(matches!(grant(&scratch), Some(SecretGrant::Waiting { .. })));
    assert!(executor.verdicts().is_empty());
    assert_eq!(
        fake.slack().posts.len(),
        2,
        "no answer was posted: the acknowledgement and the ask only"
    );
}

// ---- agent serve ----

#[tokio::test]
async fn agent_serve_takes_a_request_from_slack_and_answers_in_its_thread() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("serve");
    let app_file = scratch.root.join("app.token");
    let bot_file = scratch.root.join("bot.token");
    std::fs::write(&app_file, format!("{APP_TOKEN}\n")).unwrap();
    std::fs::write(&bot_file, format!("{BOT_TOKEN}\n")).unwrap();
    let config = scratch.root.join("agent.yaml");
    std::fs::write(
        &config,
        format!(
            "policy:
  people: [slack:UALICE]
  approvers: [slack:UALICE]
sources:
  - kind: slack
    channels: [C1]
    app_token_file: app.token
    bot_token_file: bot.token
    api_base: {}
channels:
  - kind: slack
router: {{kind: scripted}}
executor: {{kind: scripted}}
budgets: {{session_microusd: {BUDGET}}}
",
            fake.api
        ),
    )
    .unwrap();
    let driver = async {
        let conn = fake.connection().await;
        assert!(
            !app_file.exists() && !bot_file.exists(),
            "each token file was read once and deleted"
        );
        conn.send(event(
            "e1",
            mention("800.000001", "UALICE", "summarise the incident"),
        ));
        let posts = fake.until_posts(2).await;
        conn.close();
        posts
    };

    let state = scratch.root.join("state");
    let options = ServeOptions {
        until_idle: true,
        ..ServeOptions::default()
    };
    let (summary, posts) = tokio::time::timeout(LIMIT, async {
        tokio::join!(serve(&config, &state, options), driver)
    })
    .await
    .expect("agent serve did not go idle");

    let summary = summary.unwrap();
    assert_eq!((summary.triggers_received, summary.effects_pending), (1, 0));
    let texts: Vec<&str> = posts.iter().map(|p| p["text"].as_str().unwrap()).collect();
    assert_eq!(texts[0], "Looking into it.");
    assert!(
        texts[1].starts_with("Done: summarise the incident."),
        "{}",
        texts[1]
    );
    for post in &posts {
        assert_eq!(
            (&post["channel"], &post["thread_ts"]),
            (&json!("C1"), &json!("800.000001"))
        );
    }
    assert_eq!(fake.slack().acks, ["e1"]);
    assert_settled(&scratch.store());
}

const PROBE_KEY: &str = "sk-Pr0be-k3y-value-42";

struct NoModel;

impl Provider for NoModel {
    fn dispatch(&self, _: Value) -> Dispatch {
        Box::pin(async { Err(anyhow::anyhow!("no model in this test")) })
    }
}

#[derive(Default)]
struct KeyedWorker {
    launched: Mutex<Option<(TicketId, AssignmentId)>>,
    delivered: Mutex<Vec<Delivered>>,
}

impl KeyedWorker {
    fn verdicts(&self) -> Vec<bool> {
        let handed = self.delivered.lock().unwrap();
        handed
            .iter()
            .filter_map(|d| match &d.input.body {
                InputBody::Secret { granted, .. } => Some(*granted),
                _ => None,
            })
            .collect()
    }

    fn typed(&self) -> Vec<String> {
        let handed = self.delivered.lock().unwrap();
        handed
            .iter()
            .filter_map(|d| d.trigger.as_ref().map(|t| t.text.clone()))
            .collect()
    }
}

impl Executor for KeyedWorker {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let started = Fact::Started {
                ticket: launch.ticket.clone(),
                assignment: launch.assignment.clone(),
            };
            facts.send(started).await?;
            *self.launched.lock().unwrap() = Some((launch.ticket, launch.assignment));
            Ok(())
        })
    }
    fn deliver(&self, delivery: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        self.delivered.lock().unwrap().extend(delivery.inputs);
        Box::pin(async { Ok(()) })
    }
    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

struct KeyedSite {
    host: String,
    seen: Arc<Mutex<Vec<Option<String>>>>,
    task: JoinHandle<()>,
}

impl KeyedSite {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let kept = seen.clone();
        let app = axum::Router::new().fallback(move |headers: HeaderMap| {
            let authorization = headers
                .get(header::AUTHORIZATION)
                .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
            kept.lock().unwrap().push(authorization);
            async { "ok" }
        });
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { host, seen, task }
    }

    fn seen(&self) -> Vec<Option<String>> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for KeyedSite {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct KeyedBroker {
    broker: AgentBroker,
    client: reqwest::Client,
    url: String,
    server: JoinHandle<()>,
    seat: Mutex<Option<Arc<WorkerSeat>>>,
}

impl KeyedBroker {
    fn serve(scratch: &Scratch, runtime: &Runtime, site: &KeyedSite) -> Self {
        let config = VaultConfig {
            path: scratch.root.join("keys").join("vault.enc"),
            key_file: scratch.root.join("keys").join("vault.key"),
        };
        let view = SandboxView::new([]);
        Vault::init(&config, &view).unwrap();
        let vault = Vault::open(&config, &view).unwrap();
        let value = scratch.root.join("probe.value");
        std::fs::write(&value, PROBE_KEY).unwrap();
        let probe = NewItem {
            name: "probe".parse().unwrap(),
            kind: ItemKind::Key {
                hosts: vec![site.host.parse().unwrap()],
                header: "authorization".parse().unwrap(),
                scheme: KeyScheme::Bearer,
                about: "the probe API".into(),
            },
            users: ["slack:UALICE".to_owned()].into(),
            approval: ItemApproval::FirstUse,
            value: SecretValue::read_once(&value).unwrap(),
        };
        vault.add(probe, false).unwrap();
        let state = scratch.root.join("state");
        let parts = BrokerParts {
            dir: state.clone(),
            store: scratch.store(),
            provider: Arc::new(NoModel),
            facts: runtime.facts(),
            decide: DecideTerms {
                default_budget_microusd: Some(BUDGET),
                max_budget_microusd: None,
                swarm: None,
                repos: BTreeMap::new(),
                ..DecideTerms::default()
            },
            workers: Some(WorkerParts {
                terms: ModelTerms {
                    upstream: Upstream::deepseek("deepseek-flash"),
                    prices: PriceTable {
                        input_microusd_per_million: 300_000,
                        output_microusd_per_million: 1_200_000,
                        cache_hit_microusd_per_million: None,
                    },
                    max_input_bytes: 65_536,
                    max_output_tokens: 1_024,
                    call_limit: None,
                    temperature: None,
                    thinking: None,
                },
                code: None,
                vault: Some(VaultParts {
                    vault: Arc::new(vault),
                    asks_on: ["slack".to_owned()].into(),
                }),
                browser: None,
                // The keyed sites these tests fetch from are on loopback.
                allow_private_networks: true,
            }),
        };
        let broker = AgentBroker::new(parts).unwrap();
        let (listener, socket) = agent_broker::bind(&state).unwrap();
        let app = broker.router().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .unix_socket(socket)
            .build()
            .unwrap();
        Self {
            broker,
            client,
            url: format!("http://{}/v1/models", site.host),
            server,
            seat: Mutex::new(None),
        }
    }

    async fn admit_worker(&self, worker: &KeyedWorker) -> String {
        let (ticket, assignment) = until_some("the ticket's launch", || {
            worker.launched.lock().unwrap().clone()
        })
        .await;
        let seat = WorkerSeat::new(ticket, assignment, PathBuf::from("/nowhere"), BUDGET);
        let seat = self
            .broker
            .admit_worker("worker".into(), seat, "You are a worker.".into())
            .unwrap();
        *self.seat.lock().unwrap() = Some(seat);
        "worker".into()
    }

    /// New input reached the worker, so it is prompted again: its next
    /// call is not a repeat of the last turn's. A worker told to wait for
    /// a person's answer ends its turn, and the answer starts the next.
    fn new_turn(&self) {
        if let Some(seat) = &*self.seat.lock().unwrap() {
            seat.begin_turn();
        }
    }

    async fn fetch(&self, token: &str) -> (u16, Value) {
        let response = self
            .client
            .post("http://broker/tool/fetch_url")
            .bearer_auth(token)
            .json(&json!({"url": self.url, "key": "probe"}))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap())
    }
}

impl Drop for KeyedBroker {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn keyed_agent<'a>(fake: &Fake, scratch: &'a Scratch, worker: &Arc<KeyedWorker>) -> Agent<'a> {
    Agent {
        scratch,
        source: fake.source(true),
        channel: Some(fake.channel()),
        executor: worker.clone(),
        budget: BUDGET,
        until_idle: false,
    }
}

fn error(reply: &Value) -> &str {
    reply["error"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn a_keyed_fetch_waits_for_a_click_on_allow_and_typed_allow_grants_nothing() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("keyed-allow");
    let site = KeyedSite::start().await;
    let worker = Arc::new(KeyedWorker::default());
    let runtime = keyed_agent(&fake, &scratch, &worker).runtime();
    let keyed = KeyedBroker::serve(&scratch, &runtime, &site);

    let driver = async {
        let conn = fake.connection().await;
        conn.send(event("e1", mention(THREAD, "UALICE", REQUEST)));
        let token = keyed.admit_worker(&worker).await;
        let (status, asked) = keyed.fetch(&token).await;
        assert_eq!(status, 409, "{asked}");
        assert_eq!(
            error(&asked),
            format!(
                "A person must allow this ticket to send the API key probe to {} first; they \
                 were asked in the thread. End your turn: their answer comes back as new input.",
                site.host
            )
        );
        let ask = fake.until_text(&format!("{REQUEST} wants to")).await;

        let mut typed = message("C1", "700.000002", "UALICE", "allow");
        typed["thread_ts"] = json!(THREAD);
        conn.send(event("e2", typed));
        until_some("the typed allow handed to the worker", || {
            (!worker.typed().is_empty()).then_some(())
        })
        .await;
        keyed.new_turn();
        let (status, still) = keyed.fetch(&token).await;
        assert_eq!(status, 409, "typed allow grants nothing: {still}");
        assert!(
            error(&still).starts_with("A person has not yet allowed this ticket"),
            "{still}"
        );
        assert!(site.seen().is_empty(), "nothing left before the click");

        click_on(&conn, &ask, true, ALICE, "e3").await;
        until_some("alice's answer handed to the worker", || {
            (!worker.verdicts().is_empty()).then_some(())
        })
        .await;
        keyed.new_turn();
        keyed.fetch(&token).await
    };
    let (_, (status, fetched)) = drive(runtime, driver).await;

    assert_eq!((status, &fetched["body"]), (200, &json!("ok")), "{fetched}");
    assert_eq!(worker.typed(), ["allow"]);
    assert_eq!(
        worker.verdicts(),
        [true],
        "the click reached the worker as an Input"
    );
    assert_eq!(
        site.seen(),
        [Some(format!("Bearer {PROBE_KEY}"))],
        "the key left once, after the click"
    );
    assert!(!fetched.to_string().contains(PROBE_KEY));
}

#[tokio::test]
async fn a_keyed_fetch_is_refused_once_its_first_use_is_denied() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("keyed-deny");
    let site = KeyedSite::start().await;
    let worker = Arc::new(KeyedWorker::default());
    let runtime = keyed_agent(&fake, &scratch, &worker).runtime();
    let keyed = KeyedBroker::serve(&scratch, &runtime, &site);

    let driver = async {
        let conn = fake.connection().await;
        conn.send(event("e1", mention(THREAD, "UALICE", REQUEST)));
        let token = keyed.admit_worker(&worker).await;
        let (status, asked) = keyed.fetch(&token).await;
        assert_eq!(status, 409, "{asked}");
        let ask = fake.until_text(&format!("{REQUEST} wants to")).await;
        click_on(&conn, &ask, false, ALICE, "e2").await;
        until_some("alice's answer handed to the worker", || {
            (!worker.verdicts().is_empty()).then_some(())
        })
        .await;
        keyed.fetch(&token).await
    };
    let (_, (status, refused)) = drive(runtime, driver).await;

    assert_eq!(
        (status, error(&refused)),
        (403, "a person denied probe for this ticket")
    );
    assert_eq!(worker.verdicts(), [false]);
    assert!(site.seen().is_empty(), "the key never left");
}
