use anyhow::Result;
use axum::Json;
use axum::extract::{Path, State};
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
use solos_swarm::agent::discord::{DiscordApi, DiscordChannel, DiscordSource, INTENTS, Token};
use solos_swarm::agent::jsonl::JsonlSource;
use solos_swarm::agent::ports::{
    Answer, Applied, BoxFuture, Channel, Delivered, Delivery, Executor, Fact, FactSink, Launch,
    RetryAfter, SendOptions, SendOutcome, Source,
};
use solos_swarm::agent::scripted::{ScriptedExecutor, ScriptedRouter};
use solos_swarm::agent::{
    Adapters, AgentConfig, RunOptions, Runtime, STORE_FILE, ServeOptions, Summary, serve,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
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
use tokio_tungstenite::tungstenite::handshake::server::{Request as Upgrade, Response as Accepted};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

const LIMIT: Duration = Duration::from_secs(30);
const TOKEN: &str = "MTMwMDAwMDAwMDAwMDAwMDAwMQ.GabCdE.fake-bot-token";
const AUTHORIZATION: &str = "Bot MTMwMDAwMDAwMDAwMDAwMDAwMQ.GabCdE.fake-bot-token";
const BOT: &str = "1300000000000000001";
const GUILD: &str = "1100000000000000001";
const OTHER_GUILD: &str = "1100000000000000002";
const CHAN: &str = "1200000000000000001";
const SIBLING: &str = "1200000000000000003";
const DM: &str = "1200000000000000099";
const ELSEWHERE: &str = "1200000000000000002";
const BOT_ROLE: &str = "1400000000000000007";
const OTHER_ROLE: &str = "1400000000000000008";
const GATED: u64 = 5_000_000;
const BUDGET: u64 = 500_000;
const T0: u64 = 1_700_000_000_000;
const DISCORD_EPOCH: u64 = 1_420_070_400_000;
const MESSAGE_CONTENT: u64 = 1 << 15;

fn flake(ms: u64, n: u64) -> String {
    (((ms - DISCORD_EPOCH) << 22) | n).to_string()
}

fn mid(n: u64) -> String {
    flake(T0 + n * 1_000, n)
}

fn user(id: &str, name: &str) -> Value {
    json!({ "id": id, "username": name, "global_name": name.to_uppercase() })
}

fn alice() -> Value {
    user("1300000000000000011", "alice")
}
fn bob() -> Value {
    user("1300000000000000012", "bob")
}
fn eve() -> Value {
    user("1300000000000000013", "eve")
}
fn me() -> Value {
    json!({ "id": BOT, "username": "solos", "bot": true })
}
fn deploy() -> Value {
    json!({ "id": "1300000000000000021", "username": "deploy", "bot": true })
}

fn message(channel: &str, id: &str, author: &Value, content: &str) -> Value {
    json!({
        "id": id, "channel_id": channel, "guild_id": GUILD, "author": author,
        "content": content, "type": 0, "mentions": [], "timestamp": "2023-11-14T22:13:20.000000+00:00",
    })
}

fn mention(channel: &str, id: &str, author: &Value, text: &str) -> Value {
    let mut found = message(channel, id, author, &format!("<@{BOT}> {text}"));
    found["mentions"] = json!([me()]);
    found
}

fn reply(mut found: Value, to: &str) -> Value {
    found["type"] = json!(19);
    found["message_reference"] = json!({ "message_id": to, "channel_id": found["channel_id"] });
    found
}

fn thread_channel(id: &str, parent: &str) -> Value {
    json!({
        "id": id, "type": 11, "guild_id": GUILD, "parent_id": parent,
        "thread_metadata": { "archived": false },
    })
}

fn click(
    interaction: &str,
    action: &str,
    ticket: &str,
    who: &Value,
    channel: &str,
    asked: &Value,
) -> Value {
    json!({
        "id": interaction,
        "application_id": "1400000000000000001",
        "type": 3,
        "token": format!("token-of-{interaction}"),
        "version": 1,
        "guild_id": GUILD,
        "channel_id": channel,
        "channel": { "id": channel, "type": 0 },
        "member": { "user": who },
        "data": { "custom_id": format!("{action}:{ticket}"), "component_type": 2 },
        "message": {
            "id": asked["created"],
            "channel_id": channel,
            "message_reference": asked["body"]["message_reference"],
        },
    })
}

fn person_address(channel: &str, message: Option<&str>) -> ChannelRef {
    let mut address = BTreeMap::from([("channel".to_owned(), channel.to_owned())]);
    if let Some(message) = message {
        address.insert("message_id".to_owned(), message.to_owned());
    }
    ChannelRef {
        source: "discord".into(),
        address,
    }
}

enum Reply {
    RateLimit {
        json: Option<f64>,
        header: Option<&'static str>,
    },
    Status(u16),
    Refuse {
        status: u16,
        code: u32,
        message: &'static str,
    },
    KeepThenHang,
}

struct Post {
    at: Instant,
    channel: String,
    body: Value,
    authorization: String,
    user_agent: String,
    created: Option<String>,
}

struct Callback {
    interaction: String,
    token: String,
    body: Value,
    authorization: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Opening {
    Identify,
    Resume,
}

struct Handshake {
    opening: Opening,
    frame: Value,
    at: Instant,
}

struct Session {
    id: String,
    seq: u64,
    log: Vec<(u64, String, Value)>,
    live: Option<(usize, mpsc::UnboundedSender<Out>)>,
    valid: bool,
}

struct Discord {
    gateway_calls: u32,
    posts: Vec<Post>,
    messages: Vec<Value>,
    nonces: HashMap<String, usize>,
    callbacks: Vec<Callback>,
    script: VecDeque<Reply>,
    sessions: Vec<Session>,
    handshakes: Vec<Handshake>,
    beats: Vec<(usize, Instant, Value)>,
    connections: usize,
    hellos: Vec<Instant>,
    mute: Option<usize>,
    message_content: bool,
    identify_close: Option<u16>,
    resume_override: Option<String>,
    paths: Vec<String>,
    spent: u32,
    gateway_times: Vec<Instant>,
}

impl Default for Discord {
    fn default() -> Self {
        Self {
            gateway_calls: 0,
            posts: Vec::new(),
            messages: Vec::new(),
            nonces: HashMap::new(),
            callbacks: Vec::new(),
            script: VecDeque::new(),
            sessions: Vec::new(),
            handshakes: Vec::new(),
            beats: Vec::new(),
            connections: 0,
            hellos: Vec::new(),
            mute: None,
            message_content: true,
            identify_close: None,
            resume_override: None,
            paths: Vec::new(),
            spent: 0,
            gateway_times: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct Shared {
    discord: Arc<Mutex<Discord>>,
    socket: SocketAddr,
    interval_ms: u64,
}

impl Shared {
    fn discord(&self) -> std::sync::MutexGuard<'_, Discord> {
        self.discord.lock().unwrap()
    }
}

enum Out {
    Text(String),
    Close(u16),
    DropSocket,
}

struct Conn {
    index: usize,
    out: mpsc::UnboundedSender<Out>,
    gone: oneshot::Receiver<()>,
}

impl Conn {
    fn send(&self, frame: Value) {
        self.out.send(Out::Text(frame.to_string())).unwrap();
    }
    fn close(&self, code: u16) {
        self.out.send(Out::Close(code)).unwrap();
    }
    async fn cut(self) {
        self.out.send(Out::DropSocket).unwrap();
        let _ = self.gone.await;
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

fn authorization(headers: &HeaderMap) -> String {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "message": "401: Unauthorized", "code": 0 })),
    )
        .into_response()
}

async fn gateway_bot(State(shared): State<Shared>, headers: HeaderMap) -> Response {
    if authorization(&headers) != AUTHORIZATION {
        return unauthorized();
    }
    let spent = {
        let mut discord = shared.discord();
        discord.gateway_calls += 1;
        discord.gateway_times.push(Instant::now());
        let spent = discord.spent > 0;
        discord.spent = discord.spent.saturating_sub(1);
        spent
    };
    let (remaining, reset_after) = if spent { (0, 400) } else { (999, 1) };
    Json(json!({
        "url": format!("ws://{}", shared.socket),
        "shards": 1,
        "session_start_limit": {
            "total": 1000,
            "remaining": remaining,
            "reset_after": reset_after,
            "max_concurrency": 1,
        },
    }))
    .into_response()
}

async fn create_message(
    State(shared): State<Shared>,
    Path(channel): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let authorization = authorization(&headers);
    if authorization != AUTHORIZATION {
        return unauthorized();
    }
    let (reply, created) = {
        let mut discord = shared.discord();
        let user_agent = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        discord.posts.push(Post {
            at: Instant::now(),
            channel: channel.clone(),
            body: body.clone(),
            authorization,
            user_agent,
            created: None,
        });
        let at = discord.posts.len() - 1;
        let reply = discord.script.pop_front();
        let creates = matches!(reply, None | Some(Reply::KeepThenHang));
        let mut created = None;
        if creates {
            let nonce = body["nonce"].as_str().unwrap_or_default().to_owned();
            let held = (body["enforce_nonce"] == json!(true))
                .then(|| discord.nonces.get(&nonce).copied())
                .flatten();
            let index = held.unwrap_or_else(|| {
                let n = discord.messages.len() as u64 + 1;
                discord.messages.push(json!({
                    "id": flake(T0 + 100_000 + n * 1_000, n),
                    "channel_id": channel,
                    "content": body["content"],
                    "nonce": nonce,
                    "message_reference": body["message_reference"],
                    "components": body["components"],
                }));
                let index = discord.messages.len() - 1;
                discord.nonces.insert(nonce, index);
                index
            });
            let message = discord.messages[index].clone();
            discord.posts[at].created = message["id"].as_str().map(str::to_owned);
            created = Some(message);
        }
        (reply, created)
    };
    match reply {
        None => Json(created.unwrap()).into_response(),
        Some(Reply::KeepThenHang) => std::future::pending().await,
        Some(Reply::Status(status)) => {
            (StatusCode::from_u16(status).unwrap(), "busy").into_response()
        }
        Some(Reply::Refuse {
            status,
            code,
            message,
        }) => (
            StatusCode::from_u16(status).unwrap(),
            Json(json!({ "message": message, "code": code })),
        )
            .into_response(),
        Some(Reply::RateLimit { json: wait, header }) => {
            let mut body = json!({ "message": "You are being rate limited.", "global": false });
            if let Some(wait) = wait {
                body["retry_after"] = json!(wait);
            }
            let mut response = (StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
            if let Some(seconds) = header {
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, seconds.parse().unwrap());
            }
            response
        }
    }
}

async fn interaction_callback(
    State(shared): State<Shared>,
    Path((interaction, token)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    shared.discord().callbacks.push(Callback {
        interaction,
        token,
        body,
        authorization,
    });
    StatusCode::NO_CONTENT.into_response()
}

fn dispatch_frame(seq: u64, event: &str, d: &Value) -> String {
    json!({ "op": 0, "t": event, "s": seq, "d": d }).to_string()
}

fn answer(
    index: usize,
    out: &mpsc::UnboundedSender<Out>,
    shared: &Shared,
    frame: &Value,
) -> Vec<Out> {
    let mut discord = shared.discord();
    let now = Instant::now();
    let opening = |kind| Handshake {
        opening: kind,
        frame: frame.clone(),
        at: now,
    };
    match frame["op"].as_u64() {
        Some(1) => {
            discord.beats.push((index, now, frame["d"].clone()));
            if discord.mute == Some(index) {
                Vec::new()
            } else {
                vec![Out::Text(json!({ "op": 11 }).to_string())]
            }
        }
        Some(2) => {
            discord.handshakes.push(opening(Opening::Identify));
            let intents = frame["d"]["intents"].as_u64().unwrap_or_default();
            if frame["d"]["token"] != TOKEN {
                return vec![Out::Close(4004)];
            }
            if let Some(code) = discord.identify_close {
                return vec![Out::Close(code)];
            }
            if intents & MESSAGE_CONTENT != 0 && !discord.message_content {
                return vec![Out::Close(4014)];
            }
            let id = format!("session-{}", discord.sessions.len());
            let ready = json!({
                "v": 10,
                "user": me(),
                "session_id": id,
                "resume_gateway_url": discord
                    .resume_override
                    .take()
                    .unwrap_or_else(|| format!("ws://{}/resume", shared.socket)),
                "application": { "id": "1400000000000000001" },
                "guilds": [],
            });
            discord.sessions.push(Session {
                id,
                seq: 1,
                log: vec![(1, "READY".to_owned(), ready.clone())],
                live: Some((index, out.clone())),
                valid: true,
            });
            vec![Out::Text(dispatch_frame(1, "READY", &ready))]
        }
        Some(6) => {
            discord.handshakes.push(opening(Opening::Resume));
            if frame["d"]["token"] != TOKEN {
                return vec![Out::Close(4004)];
            }
            let wanted = frame["d"]["session_id"].as_str().unwrap_or_default();
            let asked = frame["d"]["seq"].as_u64().unwrap_or_default();
            let Some(session) = discord
                .sessions
                .iter_mut()
                .find(|s| s.id == wanted && s.valid)
            else {
                return vec![Out::Text(json!({ "op": 9, "d": false }).to_string())];
            };
            session.live = Some((index, out.clone()));
            let mut sent: Vec<Out> = session
                .log
                .iter()
                .filter(|(seq, ..)| *seq > asked)
                .map(|(seq, event, d)| Out::Text(dispatch_frame(*seq, event, d)))
                .collect();
            session.seq += 1;
            sent.push(Out::Text(dispatch_frame(
                session.seq,
                "RESUMED",
                &json!({}),
            )));
            sent
        }
        _ => Vec::new(),
    }
}

async fn serve_socket(
    index: usize,
    mut socket: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    mut orders: mpsc::UnboundedReceiver<Out>,
    out: mpsc::UnboundedSender<Out>,
    shared: Shared,
    _gone: oneshot::Sender<()>,
) {
    'serving: loop {
        let mut sends = Vec::new();
        tokio::select! {
            order = orders.recv() => match order {
                Some(order) => sends.push(order),
                None => break,
            },
            frame = socket.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    let frame: Value = serde_json::from_str(text.as_str()).unwrap();
                    sends = answer(index, &out, &shared, &frame);
                }
                Some(Ok(Message::Close(frame))) => {
                    let code = frame.map(|frame| frame.code);
                    if matches!(code, Some(CloseCode::Normal | CloseCode::Away)) {
                        for session in &mut shared.discord().sessions {
                            if session.live.as_ref().is_some_and(|(live, _)| *live == index) {
                                session.valid = false;
                            }
                        }
                    }
                    break;
                }
                Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
        for send in sends {
            match send {
                Out::Text(text) => {
                    if socket.send(Message::text(text)).await.is_err() {
                        break 'serving;
                    }
                }
                Out::Close(code) => {
                    let frame = CloseFrame {
                        code: CloseCode::from(code),
                        reason: "".into(),
                    };
                    let _ = socket.send(Message::Close(Some(frame))).await;
                    let _ = tokio::time::timeout(Duration::from_secs(2), async {
                        while let Some(Ok(_)) = socket.next().await {}
                    })
                    .await;
                    break 'serving;
                }
                Out::DropSocket => break 'serving,
            }
        }
    }
    for session in &mut shared.discord().sessions {
        if session
            .live
            .as_ref()
            .is_some_and(|(live, _)| *live == index)
        {
            session.live = None;
        }
    }
}

#[allow(clippy::result_large_err)]
async fn sockets(listener: TcpListener, conns: mpsc::UnboundedSender<Conn>, shared: Shared) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let requested = Arc::new(Mutex::new(String::new()));
        let seen = requested.clone();
        let accepted = tokio_tungstenite::accept_hdr_async(
            stream,
            move |request: &Upgrade, response: Accepted| {
                *seen.lock().unwrap() = request.uri().to_string();
                Ok(response)
            },
        )
        .await;
        let Ok(mut socket) = accepted else {
            continue;
        };
        let index = {
            let mut discord = shared.discord();
            discord.connections += 1;
            discord.hellos.push(Instant::now());
            discord.paths.push(requested.lock().unwrap().clone());
            discord.connections - 1
        };
        let hello = json!({ "op": 10, "d": { "heartbeat_interval": shared.interval_ms } });
        if socket.send(Message::text(hello.to_string())).await.is_err() {
            continue;
        }
        let (out, orders) = mpsc::unbounded_channel();
        let (gone_tx, gone) = oneshot::channel();
        let _ = conns.send(Conn {
            index,
            out: out.clone(),
            gone,
        });
        tokio::spawn(serve_socket(
            index,
            socket,
            orders,
            out,
            shared.clone(),
            gone_tx,
        ));
    }
}

impl Fake {
    async fn start() -> Self {
        Self::start_with(100).await
    }

    async fn start_with(interval_ms: u64) -> Self {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let shared = Shared {
            discord: Arc::default(),
            socket: socket.local_addr().unwrap(),
            interval_ms,
        };
        let api = format!("http://{}/api/v10/", http.local_addr().unwrap());
        let app = axum::Router::new()
            .route("/api/v10/gateway/bot", get(gateway_bot))
            .route("/api/v10/channels/{id}/messages", post(create_message))
            .route(
                "/api/v10/interactions/{id}/{token}/callback",
                post(interaction_callback),
            )
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
        self.discord().script.extend(replies);
    }

    fn discord(&self) -> std::sync::MutexGuard<'_, Discord> {
        self.shared.discord()
    }

    async fn connection(&self) -> Conn {
        let next = async { self.connections.lock().await.recv().await };
        tokio::time::timeout(LIMIT, next)
            .await
            .expect("the agent did not connect")
            .unwrap()
    }

    async fn until<T>(&self, what: &str, check: impl Fn(&Discord) -> Option<T>) -> T {
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Some(found) = check(&self.discord()) {
                return found;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn ready(&self) -> String {
        self.until("a ready session", |d| {
            let session = d.sessions.last()?;
            session.live.is_some().then(|| session.id.clone())
        })
        .await
    }

    fn dispatch(&self, event: &str, d: Value) {
        let mut discord = self.discord();
        let session = discord
            .sessions
            .last_mut()
            .expect("no session to dispatch on");
        session.seq += 1;
        let seq = session.seq;
        session.log.push((seq, event.to_owned(), d.clone()));
        if let Some((_, out)) = &session.live {
            let _ = out.send(Out::Text(dispatch_frame(seq, event, &d)));
        }
    }

    fn expire_sessions(&self) {
        for session in &mut self.discord().sessions {
            session.valid = false;
        }
    }

    fn openings(&self) -> Vec<Opening> {
        self.discord()
            .handshakes
            .iter()
            .map(|h| h.opening)
            .collect()
    }

    async fn until_openings(&self, n: usize) -> Vec<Opening> {
        self.until(&format!("{n} handshakes"), |d| {
            (d.handshakes.len() >= n).then(|| d.handshakes.iter().map(|h| h.opening).collect())
        })
        .await
    }

    async fn until_posts(&self, n: usize) -> Vec<Value> {
        self.until(&format!("{n} posts"), |d| {
            (d.posts.len() >= n).then(|| d.posts.iter().map(post_json).collect())
        })
        .await
    }

    async fn until_text(&self, prefix: &str) -> Value {
        self.until(&format!("a post starting {prefix:?}"), |d| {
            d.posts.iter().map(post_json).find(|post| {
                post["body"]["content"]
                    .as_str()
                    .unwrap()
                    .starts_with(prefix)
            })
        })
        .await
    }

    fn api(&self) -> Arc<DiscordApi> {
        Arc::new(DiscordApi::new(&self.api, Token::new(TOKEN)).unwrap())
    }

    fn source(&self, follow: bool) -> Box<dyn Source> {
        self.source_for(&[], &[CHAN, DM], follow)
    }

    fn source_for(&self, guilds: &[&str], channels: &[&str], follow: bool) -> Box<dyn Source> {
        let set = |ids: &[&str]| {
            ids.iter()
                .map(|id| (*id).to_owned())
                .collect::<BTreeSet<_>>()
        };
        let source = DiscordSource::new(self.api(), set(guilds), set(channels), follow)
            .with_backoff(Duration::from_millis(10), Duration::from_millis(100))
            .with_reidentify_wait(Duration::from_millis(200), Duration::from_millis(300));
        Box::new(source)
    }

    fn channel(&self) -> Arc<dyn Channel> {
        Arc::new(DiscordChannel::new(self.api()))
    }
}

fn post_json(post: &Post) -> Value {
    json!({
        "channel": post.channel,
        "body": post.body,
        "authorization": post.authorization,
        "user_agent": post.user_agent,
        "created": post.created,
    })
}

fn text_of(post: &Value) -> &str {
    post["body"]["content"].as_str().unwrap()
}

fn nonce_of(post: &Value) -> &str {
    post["body"]["nonce"].as_str().unwrap()
}

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agent-discord-{name}-{}-{}",
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
        people: [
            "discord:1300000000000000011".to_owned(),
            "discord:1300000000000000012".to_owned(),
        ]
        .into(),
        bots: ["discord:1300000000000000021".to_owned()].into(),
        approvers: ["discord:1300000000000000011".to_owned()].into(),
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
                .map(|channel| BTreeMap::from([("discord".to_owned(), channel)]))
                .unwrap_or_default(),
            router: Arc::new(ScriptedRouter::new(self.budget)),
            executor: self.executor,
        };
        let options = RunOptions {
            until_idle: self.until_idle,
            tick: Duration::from_millis(10),
            retry_base: Duration::from_millis(10),
            retry_max: Duration::from_millis(500),
        };
        Runtime::new(self.scratch.store(), policy(), adapters, options)
    }
}

fn serving<'a>(fake: &Fake, scratch: &'a Scratch, budget: u64) -> Agent<'a> {
    Agent {
        scratch,
        source: fake.source(true),
        channel: Some(fake.channel()),
        executor: Arc::new(ScriptedExecutor),
        budget,
        until_idle: false,
    }
}

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
        id: TriggerId::new(format!("discord:{CHAN}:{id}")),
        source: "discord".into(),
        at_ms: 1_000,
        actor: Actor::person("1300000000000000011"),
        addressed: Addressed::Mention,
        text: text.into(),
        threads: vec![ThreadKey::new(format!("discord:{thread}"))],
        reply_to: vec![person_address(CHAN, Some(thread))],
        meta: json!({}),
    }
}

fn outbound(id: &str, kind: MessageKind, channel: &str, text: &str) -> OutboundMessage {
    OutboundMessage {
        id: MessageId::new(id),
        channel: person_address(channel, Some("1000000000000000001")),
        kind,
        text: text.into(),
        ticket: Some(TicketId::new("tk_1")),
        ask: None,
    }
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

fn by_id<'a>(triggers: &'a [Trigger], id: &str) -> &'a Trigger {
    triggers
        .iter()
        .find(|t| t.id.as_str() == id)
        .unwrap_or_else(|| panic!("no trigger {id}"))
}

async fn until_settled(scratch: &Scratch) {
    until_some("every queued effect to be done", || {
        let effects = scratch.store().effects().unwrap();
        (!effects.is_empty() && effects.iter().all(|q| q.done)).then_some(())
    })
    .await;
}

async fn until_triggers(scratch: &Scratch, n: usize) {
    tokio::time::timeout(LIMIT, async {
        while scratch.store().triggers().unwrap().len() < n {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fewer than {n} messages arrived"));
}

fn trigger_ids(store: &SqliteStore) -> Vec<String> {
    store
        .triggers()
        .unwrap()
        .iter()
        .map(|t| t.id.to_string())
        .collect()
}

#[tokio::test]
async fn the_agent_identifies_with_its_intents_and_heartbeats_with_the_last_sequence_number() {
    let fake = Fake::start_with(150).await;
    let scratch = Scratch::new("handshake");
    let runtime = serving(&fake, &scratch, BUDGET).runtime();
    let driver = async {
        let conn = fake.connection().await;
        fake.ready().await;
        let hello = fake.discord().hellos[conn.index];
        fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(1), &alice(), "hi"));
        let first_beat = fake
            .until(
                "a heartbeat that carries the message's sequence number",
                |d| {
                    d.beats
                        .iter()
                        .any(|beat| beat.2 == json!(2))
                        .then(|| d.beats[0].1)
                },
            )
            .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        first_beat - hello
    };

    let (_, first_beat) = drive(runtime, driver).await;

    let discord = fake.discord();
    assert_eq!(discord.handshakes.len(), 1, "one identify and no reconnect");
    let identify = &discord.handshakes[0];
    assert_eq!(identify.opening, Opening::Identify);
    assert_eq!(identify.frame["d"]["token"], TOKEN);
    assert_eq!(
        identify.frame["d"]["intents"],
        1 | (1 << 9) | (1 << 12) | (1 << 15)
    );
    assert_eq!(identify.frame["d"]["intents"], INTENTS);
    assert!(identify.frame["d"]["properties"]["browser"].is_string());
    assert_eq!(discord.gateway_calls, 1);
    assert_eq!(
        discord.connections, 1,
        "every heartbeat was acknowledged, so the socket stayed"
    );
    assert!(
        first_beat < Duration::from_millis(150 + 500),
        "the first beat came {first_beat:?} after Hello, within an interval"
    );
    assert!(
        discord.beats.len() >= 3,
        "it keeps beating: {} beats",
        discord.beats.len()
    );
    for (_, _, carried) in &discord.beats {
        assert!(
            [json!(null), json!(1), json!(2)].contains(carried),
            "a beat carries the last sequence number seen, not {carried}"
        );
    }
}

#[tokio::test]
async fn a_heartbeat_request_from_the_server_is_answered_at_once() {
    let fake = Fake::start_with(600_000).await;
    let scratch = Scratch::new("beat-now");
    let runtime = serving(&fake, &scratch, BUDGET).runtime();
    let driver = async {
        let conn = fake.connection().await;
        fake.ready().await;
        fake.dispatch("GUILD_CREATE", json!({ "id": GUILD, "threads": [] }));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let asked = Instant::now();
        conn.send(json!({ "op": 1, "d": null }));
        fake.until("the answer to the request", |d| {
            d.beats
                .iter()
                .find(|b| b.1 >= asked)
                .map(|b| (b.1 - asked, b.2.clone()))
        })
        .await
    };

    let (_, (waited, carried)) = drive(runtime, driver).await;

    assert!(waited < Duration::from_secs(2), "answered after {waited:?}");
    assert_eq!(carried, json!(2), "it carries the last sequence number");
}

#[tokio::test]
async fn a_missing_heartbeat_ack_is_a_zombie_connection_and_the_agent_resumes() {
    let fake = Fake::start_with(100).await;
    let scratch = Scratch::new("zombie");
    let runtime = serving(&fake, &scratch, BUDGET).runtime();
    let driver = async {
        let first = fake.connection().await;
        let session = fake.ready().await;
        fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(1), &alice(), "one"));
        until_triggers(&scratch, 1).await;
        fake.discord().mute = Some(first.index);
        let _second = fake.connection().await;
        fake.until_openings(2).await;
        fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(2), &alice(), "two"));
        until_triggers(&scratch, 2).await;
        session
    };

    let (summary, session) = drive(runtime, driver).await;

    assert_eq!(fake.openings(), [Opening::Identify, Opening::Resume]);
    let discord = fake.discord();
    let resume = &discord.handshakes[1];
    assert_eq!(resume.frame["d"]["session_id"], session);
    assert_eq!(resume.frame["d"]["seq"], 2, "READY, then the message");
    assert!(
        discord.beats.iter().any(|beat| beat.0 == 0),
        "the silent socket had been beating"
    );
    assert_eq!(
        (summary.triggers_received, summary.triggers_duplicate),
        (2, 0)
    );
}

#[tokio::test]
async fn messages_become_triggers_and_everything_else_is_dropped() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("mapping");
    let runtime = Agent {
        scratch: &scratch,
        source: fake.source(false),
        channel: None,
        executor: Arc::new(ScriptedExecutor),
        budget: BUDGET,
        until_idle: true,
    }
    .runtime();
    let (m9, m10, m11) = (mid(9), mid(10), mid(11));
    let (m1, m2, m3, m4, m5, m6, m7, m8) = (
        mid(1),
        mid(2),
        mid(3),
        mid(4),
        mid(5),
        mid(6),
        mid(7),
        mid(8),
    );
    let b1 = mid(50);
    let driver = async {
        let conn = fake.connection().await;
        fake.ready().await;
        fake.dispatch("THREAD_CREATE", thread_channel("1500000000000000001", CHAN));
        fake.dispatch(
            "THREAD_CREATE",
            thread_channel("1500000000000000002", ELSEWHERE),
        );
        fake.dispatch("THREAD_CREATE", thread_channel(&m1, CHAN));
        fake.dispatch("THREAD_CREATE", thread_channel("1500000000000000005", CHAN));
        let mut archived = thread_channel("1500000000000000005", CHAN);
        archived["thread_metadata"]["archived"] = json!(true);
        fake.dispatch("THREAD_UPDATE", archived);
        fake.dispatch(
            "GUILD_CREATE",
            json!({
                "id": GUILD,
                "threads": [],
                "roles": [
                    { "id": BOT_ROLE, "name": "solos", "tags": { "bot_id": BOT } },
                    { "id": OTHER_ROLE, "name": "ops" },
                ],
            }),
        );
        let by_role = |id: &str, role: &str, text: &str| {
            let mut found = message(CHAN, id, &alice(), &format!("<@&{role}> {text}"));
            found["mention_roles"] = json!([role]);
            found
        };
        let events = [
            (
                "MESSAGE_CREATE",
                mention(CHAN, &m1, &alice(), "summarise the incident"),
            ),
            (
                "MESSAGE_CREATE",
                mention(CHAN, &m1, &alice(), "summarise the incident"),
            ),
            (
                "MESSAGE_CREATE",
                reply(message(CHAN, &b1, &me(), "On it."), &m1),
            ),
            (
                "MESSAGE_CREATE",
                reply(message(CHAN, &m2, &bob(), "also the timeline"), &b1),
            ),
            ("MESSAGE_CREATE", {
                let mut ping = reply(message(CHAN, &m3, &alice(), "and the cost"), &m2);
                ping["mentions"] = json!([me()]);
                ping
            }),
            (
                "MESSAGE_CREATE",
                message("1500000000000000001", &m4, &bob(), "in a thread"),
            ),
            (
                "MESSAGE_CREATE",
                message(&m1, &m5, &alice(), "in the thread of the request"),
            ),
            ("MESSAGE_CREATE", {
                let mut direct = message(DM, &m6, &alice(), "status?");
                direct.as_object_mut().unwrap().remove("guild_id");
                direct
            }),
            (
                "MESSAGE_CREATE",
                message(CHAN, &m7, &deploy(), "Deploy 42 finished"),
            ),
            (
                "MESSAGE_CREATE",
                message(CHAN, &m8, &alice(), &format!("<@!{BOT}> old style mention")),
            ),
            ("MESSAGE_CREATE", message(CHAN, &mid(60), &me(), "Done.")),
            ("MESSAGE_CREATE", {
                let mut other = message(
                    ELSEWHERE,
                    &mid(61),
                    &alice(),
                    &format!("<@{BOT}> elsewhere"),
                );
                other["guild_id"] = json!(OTHER_GUILD);
                other
            }),
            (
                "MESSAGE_CREATE",
                message(
                    "1500000000000000002",
                    &mid(62),
                    &alice(),
                    "in a stranger's thread",
                ),
            ),
            ("MESSAGE_CREATE", {
                let mut join = message(CHAN, &mid(63), &alice(), "");
                join["type"] = json!(7);
                join
            }),
            (
                "MESSAGE_CREATE",
                by_role(&m9, BOT_ROLE, "by the bot's role"),
            ),
            (
                "MESSAGE_CREATE",
                by_role(&m10, OTHER_ROLE, "by someone else's"),
            ),
            (
                "MESSAGE_CREATE",
                message("1500000000000000005", &m11, &bob(), "back in an old thread"),
            ),
            ("MESSAGE_UPDATE", message(CHAN, &m1, &alice(), "edited")),
            (
                "TYPING_START",
                json!({ "channel_id": CHAN, "user_id": "1300000000000000011" }),
            ),
        ];
        for (event, d) in events {
            fake.dispatch(event, d);
        }
        conn.close(1000);
    };

    let (summary, ()) = tokio::time::timeout(LIMIT, async {
        tokio::join!(runtime.run(std::future::pending()), driver)
    })
    .await
    .expect("the run did not go idle");

    let summary = summary.unwrap();
    assert_eq!(
        (summary.triggers_received, summary.triggers_duplicate),
        (11, 1),
        "a message Discord sends twice is stored once"
    );
    let store = scratch.store();
    let triggers = store.triggers().unwrap();
    let mut found = trigger_ids(&store);
    found.sort();
    let mut expected = vec![
        format!("discord:{CHAN}:{m1}"),
        format!("discord:{CHAN}:{m2}"),
        format!("discord:{CHAN}:{m3}"),
        format!("discord:1500000000000000001:{m4}"),
        format!("discord:{m1}:{m5}"),
        format!("discord:{DM}:{m6}"),
        format!("discord:{CHAN}:{m7}"),
        format!("discord:{CHAN}:{m8}"),
        format!("discord:{CHAN}:{m9}"),
        format!("discord:{CHAN}:{m10}"),
        format!("discord:1500000000000000005:{m11}"),
    ];
    expected.sort();
    assert_eq!(
        found, expected,
        "the bot's own messages, unlisted places, system messages, edits and other events stay out"
    );

    let asked = by_id(&triggers, &format!("discord:{CHAN}:{m1}"));
    assert_eq!(asked.source, "discord");
    assert_eq!(
        asked.actor,
        Actor {
            id: "1300000000000000011".into(),
            kind: ActorKind::Person,
            name: Some("alice".into())
        }
    );
    assert_eq!(asked.addressed, Addressed::Mention);
    assert_eq!(
        asked.text, "summarise the incident",
        "the bot's mention is dropped"
    );
    assert_eq!(asked.at_ms, T0 + 1_000, "the time is the snowflake's");
    assert_eq!(asked.threads, [ThreadKey::new(format!("discord:{m1}"))]);
    assert_eq!(asked.reply_to, [person_address(CHAN, Some(&m1))]);
    assert_eq!(asked.meta["channel"], CHAN);
    assert_eq!(asked.meta["guild"], GUILD);
    assert_eq!(asked.meta["message"], m1);
    assert_eq!(asked.meta["user"], "1300000000000000011");

    let followed = by_id(&triggers, &format!("discord:{CHAN}:{m2}"));
    assert_eq!(followed.actor.id, "1300000000000000012");
    assert_eq!(followed.addressed, Addressed::Unaddressed);
    assert_eq!(
        followed.threads,
        [ThreadKey::new(format!("discord:{m1}"))],
        "a reply to the agent's answer joins the request it answered"
    );
    assert_eq!(followed.reply_to, [person_address(CHAN, Some(&m1))]);
    assert_eq!(followed.meta["reference"], b1);

    let pinged = by_id(&triggers, &format!("discord:{CHAN}:{m3}"));
    assert_eq!(
        pinged.addressed,
        Addressed::Mention,
        "a reply that pings the bot"
    );
    assert_eq!(pinged.threads, [ThreadKey::new(format!("discord:{m1}"))]);
    assert_eq!(pinged.text, "and the cost");

    let threaded = by_id(&triggers, &format!("discord:1500000000000000001:{m4}"));
    assert_eq!(
        threaded.threads,
        [ThreadKey::new("discord:1500000000000000001")]
    );
    assert_eq!(
        threaded.reply_to,
        [person_address("1500000000000000001", None)],
        "a reply in a thread channel is posted there, not as a reply to a message"
    );
    assert_eq!(threaded.meta["parent"], CHAN);

    let grown = by_id(&triggers, &format!("discord:{m1}:{m5}"));
    assert_eq!(
        grown.threads,
        [ThreadKey::new(format!("discord:{m1}"))],
        "a thread started from a message is that message's Thread"
    );
    assert_eq!(grown.reply_to, [person_address(&m1, None)]);

    let direct = by_id(&triggers, &format!("discord:{DM}:{m6}"));
    assert_eq!(direct.addressed, Addressed::Direct);
    assert_eq!(direct.threads, [ThreadKey::new(format!("discord:{m6}"))]);
    assert_eq!(direct.reply_to, [person_address(DM, Some(&m6))]);

    let bot = by_id(&triggers, &format!("discord:{CHAN}:{m7}"));
    assert_eq!(bot.actor.kind, ActorKind::Bot);
    assert_eq!(bot.actor.id, "1300000000000000021");
    assert_eq!(bot.addressed, Addressed::Unaddressed);

    let old = by_id(&triggers, &format!("discord:{CHAN}:{m8}"));
    assert_eq!(
        (old.addressed, old.text.as_str()),
        (Addressed::Mention, "old style mention")
    );

    let through_role = by_id(&triggers, &format!("discord:{CHAN}:{m9}"));
    assert_eq!(
        (through_role.addressed, through_role.text.as_str()),
        (Addressed::Mention, "by the bot's role"),
        "the managed role Discord makes for a bot is the bot"
    );
    let other_role = by_id(&triggers, &format!("discord:{CHAN}:{m10}"));
    assert_eq!(other_role.addressed, Addressed::Unaddressed);
    assert!(other_role.text.starts_with("<@&"));
    let revived = by_id(&triggers, &format!("discord:1500000000000000005:{m11}"));
    assert_eq!(
        revived.threads,
        [ThreadKey::new("discord:1500000000000000005")],
        "a thread that archived is still a thread when someone writes in it"
    );

    for trigger in &triggers {
        let address = &trigger.reply_to[0].address;
        let root = address.get("message_id").unwrap_or(&address["channel"]);
        assert_eq!(
            trigger.threads[0],
            ThreadKey::new(format!("discord:{root}")),
            "{}: the Thread names the message or thread its reply address does",
            trigger.id
        );
    }
}

#[tokio::test]
async fn a_guild_or_a_channel_lists_a_place_and_a_thread_follows_its_parent() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("scope");
    let runtime = Agent {
        scratch: &scratch,
        source: fake.source_for(&[GUILD], &[SIBLING], false),
        channel: None,
        executor: Arc::new(ScriptedExecutor),
        budget: BUDGET,
        until_idle: true,
    }
    .runtime();
    let in_other = |channel: &str, id: &str| {
        let mut found = message(channel, id, &alice(), &format!("<@{BOT}> hi"));
        found["guild_id"] = json!(OTHER_GUILD);
        found
    };
    let driver = async {
        let conn = fake.connection().await;
        fake.ready().await;
        fake.dispatch("GUILD_CREATE", json!({
            "id": OTHER_GUILD,
            "threads": [thread_channel("1500000000000000003", SIBLING), thread_channel("1500000000000000004", ELSEWHERE)],
        }));
        for found in [
            message(
                ELSEWHERE,
                &mid(1),
                &alice(),
                "unlisted channel of a listed guild",
            ),
            in_other(SIBLING, &mid(2)),
            in_other(ELSEWHERE, &mid(3)),
            in_other("1500000000000000003", &mid(4)),
            in_other("1500000000000000004", &mid(5)),
            {
                let mut direct = message(DM, &mid(6), &alice(), "unlisted DM");
                direct.as_object_mut().unwrap().remove("guild_id");
                direct
            },
        ] {
            fake.dispatch("MESSAGE_CREATE", found);
        }
        conn.close(1000);
    };

    let (summary, ()) = tokio::time::timeout(LIMIT, async {
        tokio::join!(runtime.run(std::future::pending()), driver)
    })
    .await
    .expect("the run did not go idle");

    summary.unwrap();
    let mut found = trigger_ids(&scratch.store());
    found.sort();
    let mut expected = vec![
        format!("discord:{ELSEWHERE}:{}", mid(1)),
        format!("discord:{SIBLING}:{}", mid(2)),
        format!("discord:1500000000000000003:{}", mid(4)),
    ];
    expected.sort();
    assert_eq!(
        found, expected,
        "a listed guild, a listed channel and a thread of one; not the other guild, its thread or an unlisted DM"
    );
}

#[tokio::test]
async fn a_request_is_answered_as_a_reply_with_a_nonce_and_no_mentions() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("answer");
    let m1 = mid(1);
    let driver = async {
        fake.connection().await;
        fake.ready().await;
        fake.dispatch(
            "MESSAGE_CREATE",
            mention(CHAN, &m1, &alice(), "draft the release notes"),
        );
        let posts = fake.until_posts(2).await;
        // Discord has the second post when its request arrives; the agent marks the send done once the
        // response is in. Stopping the run before that leaves the effect queued, which is not what is asserted.
        until_settled(&scratch).await;
        posts
    };

    let (summary, posts) = drive(serving(&fake, &scratch, BUDGET).runtime(), driver).await;

    assert_eq!(summary.triggers_received, 1);
    for post in &posts {
        assert_eq!(post["channel"], CHAN);
        assert_eq!(post["authorization"], AUTHORIZATION);
        assert!(
            post["user_agent"]
                .as_str()
                .unwrap()
                .starts_with("DiscordBot (")
        );
        assert_eq!(
            post["body"]["message_reference"],
            json!({ "message_id": m1, "fail_if_not_exists": false })
        );
        assert_eq!(post["body"]["enforce_nonce"], true);
        assert_eq!(post["body"]["allowed_mentions"], json!({ "parse": [] }));
        assert!(nonce_of(post).len() <= 25, "{}", nonce_of(post));
        assert!(post["body"].get("components").is_none());
    }
    assert_eq!(text_of(&posts[0]), "Looking into it.");
    assert!(text_of(&posts[1]).starts_with("Done: draft the release notes."));
    assert_ne!(
        nonce_of(&posts[0]),
        nonce_of(&posts[1]),
        "one nonce per message"
    );
    let store = scratch.store();
    let sent = store
        .effects()
        .unwrap()
        .into_iter()
        .filter(|q| matches!(q.effect, Effect::Send(_)))
        .count();
    assert_eq!(sent, posts.len());
    assert_settled(&store);
}

#[tokio::test]
async fn a_request_in_a_thread_is_answered_in_that_thread() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("thread-answer");
    let driver = async {
        fake.connection().await;
        fake.ready().await;
        fake.dispatch("THREAD_CREATE", thread_channel("1500000000000000001", CHAN));
        fake.dispatch(
            "MESSAGE_CREATE",
            mention(
                "1500000000000000001",
                &mid(1),
                &alice(),
                "look into the flake",
            ),
        );
        fake.until_posts(2).await
    };

    let (_, posts) = drive(serving(&fake, &scratch, BUDGET).runtime(), driver).await;

    for post in &posts {
        assert_eq!(post["channel"], "1500000000000000001");
        assert!(post["body"].get("message_reference").is_none());
    }
    assert_eq!(text_of(&posts[0]), "Looking into it.");
}

#[tokio::test]
async fn an_op7_reconnect_a_close_code_and_a_dropped_socket_each_resume_without_a_duplicate() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("resume");
    let driver = async {
        let first = fake.connection().await;
        let session = fake.ready().await;
        fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(1), &alice(), "one"));
        first.send(json!({ "op": 7, "d": null }));

        let second = fake.connection().await;
        fake.until_openings(2).await;
        fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(2), &alice(), "two"));
        second.close(4000);

        let third = fake.connection().await;
        fake.until_openings(3).await;
        fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(3), &alice(), "three"));
        third.cut().await;
        fake.dispatch(
            "MESSAGE_CREATE",
            mention(CHAN, &mid(4), &alice(), "four, while the socket was down"),
        );

        let _fourth = fake.connection().await;
        fake.until_openings(4).await;
        fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(5), &alice(), "five"));
        until_triggers(&scratch, 5).await;
        session
    };

    let (summary, session) = drive(serving(&fake, &scratch, BUDGET).runtime(), driver).await;

    assert_eq!(summary.triggers_received, 5);
    assert_eq!(summary.triggers_duplicate, 0, "no message came twice");
    assert_eq!(
        fake.openings(),
        [
            Opening::Identify,
            Opening::Resume,
            Opening::Resume,
            Opening::Resume
        ]
    );
    assert_eq!(
        fake.discord().gateway_calls,
        1,
        "a resume dials the resume URL and asks Discord for nothing"
    );
    let discord = fake.discord();
    for resume in &discord.handshakes[1..] {
        assert_eq!(resume.frame["d"]["token"], TOKEN);
        assert_eq!(resume.frame["d"]["session_id"], session);
    }
    assert_eq!(discord.paths[0], "/?v=10&encoding=json");
    assert!(
        discord.paths[1..]
            .iter()
            .all(|path| path == "/resume?v=10&encoding=json"),
        "a resume dials the resume URL READY named: {:?}",
        discord.paths
    );
    assert_eq!(
        discord.handshakes[1].frame["d"]["seq"], 2,
        "READY, then message one"
    );
    assert_eq!(
        discord.handshakes[2].frame["d"]["seq"], 4,
        "the RESUMED event, then message two"
    );
}

#[tokio::test]
async fn a_message_discord_sends_again_is_stored_once() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("redelivery");
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
        fake.ready().await;
        for _ in 0..3 {
            fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(1), &alice(), "once"));
        }
        conn.close(1000);
    };

    let (summary, ()) = tokio::time::timeout(LIMIT, async {
        tokio::join!(runtime.run(std::future::pending()), driver)
    })
    .await
    .expect("the run did not go idle");

    let summary = summary.unwrap();
    assert_eq!(
        (summary.triggers_received, summary.triggers_duplicate),
        (1, 2)
    );
    assert_eq!(scratch.store().triggers().unwrap().len(), 1);
}

#[tokio::test]
async fn an_invalid_session_that_can_resume_resumes_and_one_that_cannot_identifies_again_after_a_wait()
 {
    let fake = Fake::start().await;
    let scratch = Scratch::new("invalid-session");
    let driver = async {
        let first = fake.connection().await;
        let session = fake.ready().await;
        fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(1), &alice(), "one"));
        first.send(json!({ "op": 9, "d": true }));
        let second = fake.connection().await;
        fake.until_openings(2).await;
        fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(2), &alice(), "two"));

        let invalidated = Instant::now();
        second.send(json!({ "op": 9, "d": false }));
        let third = fake.connection().await;
        fake.until_openings(3).await;
        let fresh = fake.ready().await;
        fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(3), &alice(), "three"));

        fake.expire_sessions();
        third.send(json!({ "op": 7, "d": null }));
        let _fourth = fake.connection().await;
        fake.until_openings(5).await;
        let refused = fake.ready().await;
        fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(4), &alice(), "four"));
        until_triggers(&scratch, 4).await;
        (session, fresh, refused, invalidated)
    };

    let (summary, (session, fresh, refused, invalidated)) =
        drive(serving(&fake, &scratch, BUDGET).runtime(), driver).await;

    assert_eq!(summary.triggers_received, 4);
    assert_eq!(
        fake.openings(),
        [
            Opening::Identify,
            Opening::Resume,
            Opening::Identify,
            Opening::Resume,
            Opening::Identify
        ],
        "d: true resumes; d: false, and a resume Discord refuses, identify"
    );
    let discord = fake.discord();
    assert_eq!(discord.handshakes[1].frame["d"]["session_id"], session);
    assert_ne!(fresh, session, "a new session");
    assert_ne!(refused, fresh);
    assert_eq!(discord.handshakes[3].frame["d"]["session_id"], fresh);
    let waited = discord.handshakes[2].at - invalidated;
    assert!(
        waited >= Duration::from_millis(190),
        "identified again after {waited:?}, not after the wait Discord asks"
    );
    assert_eq!(
        discord.gateway_calls, 3,
        "each fresh identify asks for the gateway"
    );
}

#[tokio::test]
async fn a_close_code_that_names_a_session_to_drop_identifies_again() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("session-timeout");
    let driver = async {
        let first = fake.connection().await;
        fake.ready().await;
        first.close(4009);
        let _second = fake.connection().await;
        fake.until_openings(2).await;
        fake.ready().await;
    };

    drive(serving(&fake, &scratch, BUDGET).runtime(), driver).await;

    assert_eq!(fake.openings(), [Opening::Identify, Opening::Identify]);
}

#[tokio::test]
async fn a_close_code_no_reconnect_survives_fails_the_run_and_names_the_fix() {
    let cases: [(&str, u16, &str); 3] = [
        ("intents", 4014, "Message Content Intent"),
        ("token", 4004, "Reset it"),
        ("version", 4012, "Update solos-swarm"),
    ];
    for (name, code, fix) in cases {
        let fake = Fake::start().await;
        let scratch = Scratch::new(name);
        fake.discord().identify_close = Some(code);
        let runtime = serving(&fake, &scratch, BUDGET).runtime();

        let ended = tokio::time::timeout(LIMIT, runtime.run(std::future::pending()))
            .await
            .expect("the run did not end");

        let err = format!("{:#}", ended.unwrap_err());
        assert!(err.contains(&code.to_string()), "{err}");
        assert!(err.contains(fix), "{err}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            fake.discord().connections,
            1,
            "{name}: it did not reconnect"
        );
    }
}

#[tokio::test]
async fn message_content_intent_off_in_the_portal_is_the_error_discord_gives() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("portal");
    fake.discord().message_content = false;
    let runtime = serving(&fake, &scratch, BUDGET).runtime();

    let ended = tokio::time::timeout(LIMIT, runtime.run(std::future::pending()))
        .await
        .expect("the run did not end");

    let err = format!("{:#}", ended.unwrap_err());
    assert!(
        err.contains("4014") && err.contains("Privileged Gateway Intents"),
        "{err}"
    );
}

#[tokio::test]
async fn a_refused_token_stops_the_run_before_any_socket_opens() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("bad-token");
    let api = Arc::new(DiscordApi::new(&fake.api, Token::new("not-the-token")).unwrap());
    let source = DiscordSource::new(api, BTreeSet::new(), [CHAN.to_owned()].into(), true);
    let runtime = Agent {
        scratch: &scratch,
        source: Box::new(source),
        channel: None,
        executor: Arc::new(ScriptedExecutor),
        budget: BUDGET,
        until_idle: false,
    }
    .runtime();

    let ended = tokio::time::timeout(LIMIT, runtime.run(std::future::pending()))
        .await
        .expect("the run did not end");

    let err = format!("{:#}", ended.unwrap_err());
    assert!(
        err.contains("refused the bot token") && err.contains("Bot page"),
        "{err}"
    );
    assert!(!err.contains("not-the-token"), "{err}");
    assert_eq!(fake.discord().connections, 0);
}

#[tokio::test]
async fn a_spent_limit_on_new_sessions_is_waited_out_before_the_agent_identifies() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("start-limit");
    fake.discord().spent = 1;
    let runtime = serving(&fake, &scratch, BUDGET).runtime();
    let driver = async {
        fake.connection().await;
        fake.ready().await;
    };

    drive(runtime, driver).await;

    let discord = fake.discord();
    assert_eq!(discord.gateway_calls, 2);
    let waited = discord.gateway_times[1] - discord.gateway_times[0];
    assert!(
        waited >= Duration::from_millis(380),
        "asked again after {waited:?}, not after the 400 ms Discord named"
    );
    assert_eq!(discord.handshakes.len(), 1, "one identify, after the wait");
}

#[tokio::test]
async fn a_resume_address_nobody_answers_is_given_up_for_a_fresh_session() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("dead-resume");
    let nobody = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    fake.discord().resume_override = Some(format!("ws://{nobody}/resume"));
    let runtime = serving(&fake, &scratch, BUDGET).runtime();
    let driver = async {
        let first = fake.connection().await;
        let session = fake.ready().await;
        first.cut().await;
        fake.until_openings(2).await;
        let fresh = fake.ready().await;
        fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(1), &alice(), "hello"));
        until_triggers(&scratch, 1).await;
        (session, fresh)
    };

    let (_, (session, fresh)) = drive(runtime, driver).await;

    assert_ne!(session, fresh);
    assert_eq!(fake.openings(), [Opening::Identify, Opening::Identify]);
    assert_eq!(fake.discord().gateway_calls, 2);
}

#[tokio::test]
async fn a_reply_is_cut_to_discords_limit_and_an_approval_request_has_two_buttons() {
    let fake = Fake::start().await;
    let channel = fake.channel();
    let long = format!("@everyone fixed & tested. {}", "word ".repeat(1_000));
    let approval = SendOptions {
        ask: Some(Ask::Budget),
        ..SendOptions::default()
    };
    let sends = [
        (
            outbound("ms_result", MessageKind::Result, CHAN, &long),
            SendOptions::default(),
        ),
        (
            outbound("ms_ack", MessageKind::Acknowledgement, CHAN, &long),
            SendOptions::default(),
        ),
        (
            outbound(
                "ms_ask",
                MessageKind::Attention,
                CHAN,
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
    let text = text_of(result);
    assert!(text.starts_with("@everyone fixed & tested."), "{text}");
    assert!(text.ends_with("of 5025 characters shown.]_"), "{text}");
    assert!(text.chars().count() <= 2_000, "{}", text.chars().count());
    assert!(text_of(ack).chars().count() <= 500);
    for post in [result, ack, ask] {
        assert_eq!(post["body"]["allowed_mentions"], json!({ "parse": [] }));
        assert_eq!(post["body"]["flags"], 4, "no link previews");
        assert_eq!(
            post["body"]["message_reference"]["message_id"],
            "1000000000000000001"
        );
    }
    assert!(result["body"].get("components").is_none() && ack["body"].get("components").is_none());
    assert_eq!(
        ask["body"]["components"],
        json!([{
            "type": 1,
            "components": [
                { "type": 2, "style": 3, "label": "Approve", "custom_id": "solos_approve:tk_1" },
                { "type": 2, "style": 4, "label": "Stop", "custom_id": "solos_stop:tk_1" },
            ],
        }])
    );
    assert_eq!(format!("{:?}", Token::new(TOKEN)), "Token(..)");
}

#[tokio::test]
async fn a_429_waits_as_long_as_discord_asks_a_5xx_retries_and_other_4xx_are_permanent() {
    let fake = Fake::start().await;
    let channel = fake.channel();
    let send = |id: &str, channel_id: &str| {
        channel.send(
            outbound(id, MessageKind::Result, channel_id, "done"),
            SendOptions::default(),
        )
    };
    fake.script([
        Reply::RateLimit {
            json: Some(1.5),
            header: Some("9"),
        },
        Reply::RateLimit {
            json: None,
            header: Some("7"),
        },
        Reply::Status(503),
        Reply::Status(500),
        Reply::Refuse {
            status: 403,
            code: 50001,
            message: "Missing Access",
        },
        Reply::Refuse {
            status: 404,
            code: 10003,
            message: "Unknown Channel",
        },
        Reply::Refuse {
            status: 400,
            code: 50035,
            message: "Invalid Form Body",
        },
        Reply::Status(401),
    ]);
    let retry = |err: &anyhow::Error| {
        err.chain()
            .find_map(|e| e.downcast_ref::<RetryAfter>())
            .copied()
    };

    let by_body = send("ms_1", CHAN).await.unwrap_err();
    assert_eq!(
        retry(&by_body),
        Some(RetryAfter(Duration::from_millis(1_500))),
        "the JSON retry_after, not the header: {by_body:#}"
    );
    let by_header = send("ms_1", CHAN).await.unwrap_err();
    assert_eq!(retry(&by_header), Some(RetryAfter(Duration::from_secs(7))));
    for status in ["503", "500"] {
        let down = send("ms_1", CHAN).await.unwrap_err();
        assert!(
            retry(&down).is_none() && format!("{down:#}").contains(status),
            "{down:#}"
        );
    }
    for id in ["ms_2", "ms_3", "ms_4"] {
        assert_eq!(
            send(id, CHAN).await.unwrap(),
            SendOutcome::Gone,
            "{id}: a 4xx repeating cannot fix"
        );
    }
    let revoked = send("ms_5", CHAN).await.unwrap_err();
    assert!(
        format!("{revoked:#}").contains("refused the bot token"),
        "a refused token is an error to retry once the operator fixes it: {revoked:#}"
    );
    assert_eq!(send("ms_6", "../1").await.unwrap(), SendOutcome::Gone);
    assert_eq!(
        fake.discord().posts.len(),
        8,
        "the address that is not a channel id got no request"
    );
    let discord = fake.discord();
    let retries: BTreeSet<&str> = discord.posts[..4].iter().map(nonce_of_post).collect();
    assert_eq!(retries.len(), 1, "four attempts at ms_1 carry one nonce");
}

fn nonce_of_post(post: &Post) -> &str {
    post.body["nonce"].as_str().unwrap()
}

#[tokio::test]
async fn a_rate_limited_post_is_retried_after_the_wait_with_the_same_nonce() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("retry-after");
    fake.script([Reply::RateLimit {
        json: Some(1.0),
        header: None,
    }]);
    let runtime = Agent {
        scratch: &scratch,
        source: scratch.triggers(&[request(&mid(1), &mid(1), "Fix the flaky test")]),
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
    let discord = fake.discord();
    let [limited, retried, result] = &discord.posts[..] else {
        panic!("{} posts", discord.posts.len())
    };
    assert_eq!(nonce_of_post(limited), nonce_of_post(retried));
    assert_ne!(nonce_of_post(retried), nonce_of_post(result));
    let waited = retried.at - limited.at;
    assert!(
        waited >= Duration::from_millis(950),
        "the retry waited {waited:?}, not the second Discord asked"
    );
    assert_eq!(
        discord.messages.len(),
        2,
        "the acknowledgement once, then the result"
    );
    assert_settled(&scratch.store());
}

#[tokio::test]
async fn a_crash_after_discord_took_a_post_does_not_post_it_twice() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("crash");
    let asked = [request(&mid(1), &mid(1), "Fix the flaky test")];
    fake.script([Reply::KeepThenHang]);
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
        posts = fake.until_posts(1) => assert_eq!(text_of(&posts[0]), "Looking into it."),
    }
    assert_eq!(fake.discord().messages.len(), 1, "Discord holds the post");
    let acknowledged = nonce_of_post(&fake.discord().posts[0]).to_owned();
    let pending = scratch
        .store()
        .effects()
        .unwrap()
        .into_iter()
        .any(|q| matches!(q.effect, Effect::Send(_)) && !q.done);
    assert!(pending, "the post was never marked sent");

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

    let discord = fake.discord();
    let same: Vec<&Post> = discord
        .posts
        .iter()
        .filter(|p| nonce_of_post(p) == acknowledged)
        .collect();
    assert_eq!(same.len(), 2, "the redone send reused the nonce");
    assert_eq!(
        discord.messages.len(),
        2,
        "Discord holds the acknowledgement once, then the result"
    );
    assert!(
        discord.messages[1]["content"]
            .as_str()
            .unwrap()
            .starts_with("Done:"),
        "{}",
        discord.messages[1]
    );
    assert_settled(&scratch.store());
}

async fn ask_for_gated_work(fake: &Fake) -> (String, Value) {
    fake.dispatch(
        "MESSAGE_CREATE",
        mention(CHAN, &mid(1), &alice(), "migrate the billing db"),
    );
    let ask = fake
        .until_text("migrate the billing db needs approval first")
        .await;
    assert_eq!(ask["channel"], CHAN);
    assert_eq!(ask["body"]["message_reference"]["message_id"], mid(1));
    let buttons = &ask["body"]["components"][0]["components"];
    let approve = buttons[0]["custom_id"].as_str().unwrap();
    let stop = buttons[1]["custom_id"].as_str().unwrap();
    let ticket = approve.strip_prefix("solos_approve:").unwrap();
    assert_eq!(stop, format!("solos_stop:{ticket}"));
    (ticket.to_owned(), ask)
}

fn launched(store: &SqliteStore) -> bool {
    store
        .effects()
        .unwrap()
        .iter()
        .any(|q| matches!(q.effect, Effect::Launch { .. }))
}

#[tokio::test]
async fn the_approve_button_launches_work_that_waited_for_approval_and_is_acknowledged() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("approve");
    let driver = async {
        fake.connection().await;
        fake.ready().await;
        let (ticket, ask) = ask_for_gated_work(&fake).await;
        assert!(
            !launched(&scratch.store()),
            "nothing runs before the approval"
        );
        fake.dispatch(
            "INTERACTION_CREATE",
            click(
                "1600000000000000001",
                "solos_approve",
                &ticket,
                &alice(),
                CHAN,
                &ask,
            ),
        );
        let approved = fake.until_text("Approved by alice.").await;
        let result = fake.until_text("Done: migrate the billing db.").await;
        (ticket, approved, result)
    };

    let (_, (ticket, approved, result)) =
        drive(serving(&fake, &scratch, GATED).runtime(), driver).await;

    for post in [&approved, &result] {
        assert_eq!(post["channel"], CHAN);
        assert_eq!(
            post["body"]["message_reference"]["message_id"],
            mid(1),
            "the answer is a reply to the request"
        );
    }
    let discord = fake.discord();
    let [acked] = &discord.callbacks[..] else {
        panic!("{} callbacks", discord.callbacks.len())
    };
    assert_eq!(acked.interaction, "1600000000000000001");
    assert_eq!(acked.token, "token-of-1600000000000000001");
    assert_eq!(acked.body, json!({ "type": 6 }));
    assert_eq!(
        acked.authorization, None,
        "the interaction's token is the credential, not the bot's"
    );
    drop(discord);
    let store = scratch.store();
    let found = store.ticket(&TicketId::new(&ticket)).unwrap();
    let Approval::Granted { by, .. } = &found.approval else {
        panic!("{:?}", found.approval)
    };
    assert_eq!(by.id, "1300000000000000011");
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
        fake.connection().await;
        fake.ready().await;
        let (ticket, ask) = ask_for_gated_work(&fake).await;
        fake.dispatch(
            "INTERACTION_CREATE",
            click(
                "1600000000000000001",
                "solos_stop",
                &ticket,
                &alice(),
                CHAN,
                &ask,
            ),
        );
        fake.until_text("Stopped by alice.").await;
        ticket
    };

    let (_, ticket) = drive(serving(&fake, &scratch, GATED).runtime(), driver).await;

    let store = scratch.store();
    let ended = store
        .ticket(&TicketId::new(&ticket))
        .unwrap()
        .ended
        .unwrap();
    assert_eq!(
        (ended.by, ended.outcome),
        (EndedBy::Person, TicketOutcome::Stopped)
    );
    assert!(!launched(&store), "the stopped work never launched");
    assert_eq!(fake.discord().callbacks.len(), 1);
}

#[tokio::test]
async fn a_click_from_someone_the_policy_does_not_allow_changes_nothing_but_is_acknowledged() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("refused");
    let driver = async {
        fake.connection().await;
        fake.ready().await;
        let (ticket, ask) = ask_for_gated_work(&fake).await;
        let press = |id: &str, action: &str, who: &Value, channel: &str| {
            click(id, action, &ticket, who, channel, &ask)
        };
        fake.dispatch(
            "INTERACTION_CREATE",
            press("1600000000000000001", "solos_approve", &bob(), CHAN),
        );
        fake.until_text("bob is not allowed to approve budgets.")
            .await;
        fake.dispatch(
            "INTERACTION_CREATE",
            press("1600000000000000002", "solos_stop", &eve(), CHAN),
        );
        fake.until_text("eve is not allowed to stop this work.")
            .await;
        let mut stranger = press("1600000000000000003", "solos_approve", &alice(), ELSEWHERE);
        stranger["guild_id"] = json!(OTHER_GUILD);
        fake.dispatch("INTERACTION_CREATE", stranger);
        let mut foreign = press("1600000000000000004", "solos_approve", &alice(), CHAN);
        foreign["data"]["custom_id"] = json!("something-else");
        fake.dispatch("INTERACTION_CREATE", foreign);
        fake.dispatch(
            "INTERACTION_CREATE",
            json!({ "id": "1600000000000000005", "token": "token-of-5", "type": 3, "data": "x" }),
        );
        fake.dispatch(
            "INTERACTION_CREATE",
            json!({ "id": "1600000000000000006", "token": "token-of-6", "type": 2, "data": {} }),
        );
        fake.until("every click acknowledged", |d| {
            (d.callbacks.len() >= 5).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        ticket
    };

    let (_, ticket) = drive(serving(&fake, &scratch, GATED).runtime(), driver).await;

    let store = scratch.store();
    let found = store.ticket(&TicketId::new(&ticket)).unwrap();
    assert_eq!(found.approval, Approval::Waiting);
    assert!(found.ended.is_none());
    assert!(!launched(&store));
    let discord = fake.discord();
    let mut acked: Vec<&str> = discord
        .callbacks
        .iter()
        .map(|c| c.interaction.as_str())
        .collect();
    acked.sort_unstable();
    assert_eq!(
        acked,
        [
            "1600000000000000001",
            "1600000000000000002",
            "1600000000000000003",
            "1600000000000000004",
            "1600000000000000005",
        ],
        "every click is acknowledged: one outside the listed places, one that is not the agent's button, one the agent cannot read; an interaction that is not a click is not"
    );
    assert!(
        discord
            .callbacks
            .iter()
            .all(|c| c.body == json!({ "type": 6 }))
    );
    assert_eq!(
        discord.posts.len(),
        3,
        "the request, then the two refusals; the click outside the listed places and the foreign button said nothing"
    );
}

#[tokio::test]
async fn a_click_in_a_direct_message_is_judged_by_the_user_who_made_it() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("dm-click");
    let driver = async {
        fake.connection().await;
        fake.ready().await;
        let mut direct = mention(DM, &mid(1), &alice(), "migrate the billing db");
        direct.as_object_mut().unwrap().remove("guild_id");
        fake.dispatch("MESSAGE_CREATE", direct);
        let ask = fake
            .until_text("migrate the billing db needs approval first")
            .await;
        let ticket = ask["body"]["components"][0]["components"][0]["custom_id"]
            .as_str()
            .unwrap()
            .strip_prefix("solos_approve:")
            .unwrap()
            .to_owned();
        let mut approve = click(
            "1600000000000000001",
            "solos_approve",
            &ticket,
            &alice(),
            DM,
            &ask,
        );
        let object = approve.as_object_mut().unwrap();
        object.remove("member");
        object.remove("guild_id");
        approve["user"] = alice();
        fake.dispatch("INTERACTION_CREATE", approve);
        fake.until_text("Approved by alice.").await
    };

    let (_, approved) = drive(serving(&fake, &scratch, GATED).runtime(), driver).await;

    assert_eq!(approved["channel"], DM);
    assert_eq!(approved["body"]["message_reference"]["message_id"], mid(1));
}

const REQUEST: &str = "Triage my notifications";
const LABEL: &str = "sign in to https://github.com as gui";
const ALICE_ID: &str = "discord:1300000000000000011";
const BOB_ID: &str = "discord:1300000000000000012";

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

async fn flow<T>(
    fake: &Fake,
    scratch: &Scratch,
    executor: &Arc<AsksForAKeyTwice>,
    then: impl AsyncFnOnce(Value) -> T,
) -> T {
    let driver = async {
        fake.connection().await;
        fake.ready().await;
        fake.dispatch("MESSAGE_CREATE", mention(CHAN, &mid(1), &alice(), REQUEST));
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
        then(ask).await
    };
    let agent = Agent {
        scratch,
        source: fake.source(true),
        channel: Some(fake.channel()),
        executor: executor.clone(),
        budget: BUDGET,
        until_idle: false,
    };
    drive(agent.runtime(), driver).await.1
}

fn press(fake: &Fake, interaction: &str, ask: &Value, allow: bool, who: &Value) {
    let buttons = &ask["body"]["components"][0]["components"];
    let mut pressed = click(interaction, "unused", "unused", who, CHAN, ask);
    pressed["data"]["custom_id"] = buttons[usize::from(!allow)]["custom_id"].clone();
    fake.dispatch("INTERACTION_CREATE", pressed);
}

fn grant(scratch: &Scratch) -> Option<SecretGrant> {
    scratch.store().tickets().unwrap()[0]
        .secrets
        .get(&secret())
        .cloned()
}

#[tokio::test]
async fn one_ask_posts_one_message_with_allow_and_deny() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-ask");
    let executor = AsksForAKeyTwice::new(&[ALICE_ID]);

    let ask = flow(&fake, &scratch, &executor, async |ask| ask).await;

    let ticket = scratch.store().tickets().unwrap()[0].id.to_string();
    assert_eq!(ask["channel"], CHAN);
    assert_eq!(ask["body"]["message_reference"]["message_id"], mid(1));
    assert_eq!(
        text_of(&ask),
        format!("{REQUEST} wants to {LABEL} for the first time.")
    );
    assert_eq!(
        ask["body"]["components"],
        json!([{
            "type": 1,
            "components": [
                {
                    "type": 2, "style": 3, "label": "Allow for this ticket",
                    "custom_id": format!("solos_secret_allow:{ticket}:local/github-gui"),
                },
                {
                    "type": 2, "style": 4, "label": "Deny",
                    "custom_id": format!("solos_secret_deny:{ticket}:local/github-gui"),
                },
            ],
        }])
    );
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
    assert_eq!(answerable_by, &[ALICE_ID]);
    assert_eq!(second, first, "the second ask finds the first");
    assert_eq!(
        fake.discord().posts.len(),
        2,
        "the acknowledgement and one ask, nothing more"
    );
}

#[tokio::test]
async fn allow_by_a_person_on_the_items_list_grants_it_and_tells_the_worker() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-allow");
    let executor = AsksForAKeyTwice::new(&[ALICE_ID]);

    let reply = flow(&fake, &scratch, &executor, async |ask| {
        press(&fake, "1600000000000000001", &ask, true, &alice());
        let reply = fake.until_text("Allowed by alice").await;
        until_some("the verdict handed to the worker", || {
            (!executor.verdicts().is_empty()).then_some(())
        })
        .await;
        reply
    })
    .await;

    assert_eq!(
        text_of(&reply),
        "Allowed by alice: local/github-gui for this ticket."
    );
    assert_eq!(reply["channel"], CHAN);
    assert_eq!(reply["body"]["message_reference"]["message_id"], mid(1));
    let Some(SecretGrant::Granted { by, .. }) = grant(&scratch) else {
        panic!("{:?}", grant(&scratch))
    };
    assert_eq!(
        (by.id.as_str(), by.name.as_deref()),
        ("1300000000000000011", Some("alice"))
    );
    assert_eq!(
        executor.verdicts(),
        [(true, "1300000000000000011".to_owned())]
    );
    let callbacks = fake.discord().callbacks.len();
    assert_eq!(callbacks, 1, "the click was acknowledged");
}

#[tokio::test]
async fn deny_records_denied_and_tells_the_worker() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-deny");
    let executor = AsksForAKeyTwice::new(&[ALICE_ID]);

    let reply = flow(&fake, &scratch, &executor, async |ask| {
        press(&fake, "1600000000000000001", &ask, false, &alice());
        let reply = fake.until_text("Denied by alice").await;
        until_some("the verdict handed to the worker", || {
            (!executor.verdicts().is_empty()).then_some(())
        })
        .await;
        reply
    })
    .await;

    assert_eq!(
        text_of(&reply),
        "Denied by alice: local/github-gui for this ticket."
    );
    assert!(matches!(grant(&scratch), Some(SecretGrant::Denied { .. })));
    assert_eq!(
        executor.verdicts(),
        [(false, "1300000000000000011".to_owned())]
    );
}

#[tokio::test]
async fn an_approver_outside_the_items_list_and_a_non_approver_with_no_list_are_refused() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-refused-listed");
    let listed = AsksForAKeyTwice::new(&[BOB_ID]);
    let refusal = flow(&fake, &scratch, &listed, async |ask| {
        press(&fake, "1600000000000000001", &ask, true, &alice());
        fake.until_text("Nothing changed").await
    })
    .await;
    assert_eq!(
        text_of(&refusal),
        "Nothing changed: alice may not answer for local/github-gui."
    );
    assert!(matches!(grant(&scratch), Some(SecretGrant::Waiting { .. })));
    assert!(listed.verdicts().is_empty(), "the worker was told nothing");

    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-refused-open");
    let open = AsksForAKeyTwice::new(&[]);
    let refusal = flow(&fake, &scratch, &open, async |ask| {
        press(&fake, "1600000000000000001", &ask, true, &bob());
        fake.until_text("Nothing changed").await
    })
    .await;
    assert_eq!(
        text_of(&refusal),
        "Nothing changed: bob may not answer for local/github-gui."
    );
    assert!(matches!(grant(&scratch), Some(SecretGrant::Waiting { .. })));
    assert!(open.verdicts().is_empty());
}

#[tokio::test]
async fn a_second_click_is_refused_and_changes_nothing() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-twice");
    let executor = AsksForAKeyTwice::new(&[ALICE_ID, BOB_ID]);

    flow(&fake, &scratch, &executor, async |ask| {
        press(&fake, "1600000000000000001", &ask, true, &alice());
        fake.until_text("Allowed by alice").await;
        press(&fake, "1600000000000000002", &ask, false, &bob());
        press(&fake, "1600000000000000003", &ask, true, &alice());
        fake.until("both refusals", |d| {
            let refusals = d
                .posts
                .iter()
                .filter(|p| {
                    post_json(p)["body"]["content"]
                        .as_str()
                        .unwrap()
                        .starts_with("Nothing changed")
                })
                .count();
            (refusals == 2).then_some(())
        })
        .await;
    })
    .await;

    let discord = fake.discord();
    let refusals: Vec<&str> = discord
        .posts
        .iter()
        .map(|p| p.body["content"].as_str().unwrap())
        .filter(|text| text.starts_with("Nothing changed"))
        .collect();
    assert_eq!(
        refusals,
        [
            "Nothing changed: local/github-gui does not wait for an answer.",
            "Nothing changed: local/github-gui does not wait for an answer.",
        ]
    );
    drop(discord);
    let Some(SecretGrant::Granted { by, .. }) = grant(&scratch) else {
        panic!("{:?}", grant(&scratch))
    };
    assert_eq!(by.id, "1300000000000000011", "the first answer stands");
    assert_eq!(
        executor.verdicts(),
        [(true, "1300000000000000011".to_owned())],
        "the worker heard one answer"
    );
}

#[tokio::test]
async fn a_custom_id_that_does_not_split_into_a_ticket_and_a_secret_is_dropped_but_acknowledged() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-bad-id");
    let executor = AsksForAKeyTwice::new(&[ALICE_ID]);

    flow(&fake, &scratch, &executor, async |ask| {
        for (n, id) in [
            "solos_secret_allow:tk_1",
            "solos_secret_allow:tk_1:no-slash",
            "solos_secret_allow:tk_1:local/a:b",
            "solos_secret_allow:tk_1:local/a/b",
            "solos_secret_deny:",
        ]
        .into_iter()
        .enumerate()
        {
            let mut pressed = click(
                &format!("160000000000000010{n}"),
                "unused",
                "unused",
                &alice(),
                CHAN,
                &ask,
            );
            pressed["data"]["custom_id"] = json!(id);
            fake.dispatch("INTERACTION_CREATE", pressed);
        }
        fake.until("every click acknowledged", |d| {
            (d.callbacks.len() == 5).then_some(())
        })
        .await;
        press(&fake, "1600000000000000001", &ask, true, &alice());
        fake.until_text("Allowed by alice").await;
    })
    .await;

    assert_eq!(
        fake.discord().posts.len(),
        3,
        "the acknowledgement, the ask and one answer"
    );
    assert!(matches!(grant(&scratch), Some(SecretGrant::Granted { .. })));
}

#[tokio::test]
async fn typed_allow_in_the_thread_grants_nothing_and_reaches_the_worker_as_a_trigger() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("secret-typed");
    let executor = AsksForAKeyTwice::new(&[ALICE_ID, BOB_ID]);

    flow(&fake, &scratch, &executor, async |_| {
        fake.dispatch(
            "MESSAGE_CREATE",
            reply(message(CHAN, &mid(2), &alice(), "allow"), &mid(1)),
        );
        fake.dispatch(
            "MESSAGE_CREATE",
            reply(
                message(CHAN, &mid(3), &bob(), "allow local/github-gui"),
                &mid(1),
            ),
        );
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
        fake.discord().posts.len(),
        2,
        "no answer was posted: the acknowledgement and the ask only"
    );
}

#[tokio::test]
async fn agent_serve_takes_a_request_from_discord_and_answers_in_its_thread() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("serve");
    let token_file = scratch.root.join("bot.token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    let config = scratch.root.join("agent.yaml");
    std::fs::write(
        &config,
        format!(
            "policy:
  people: [discord:1300000000000000011]
  approvers: [discord:1300000000000000011]
sources:
  - kind: discord
    channels: [{CHAN}]
    token_file: bot.token
    api_base: {}
channels:
  - kind: discord
router: {{kind: scripted}}
executor: {{kind: scripted}}
budgets: {{session_microusd: {BUDGET}}}
",
            fake.api
        ),
    )
    .unwrap();
    AgentConfig::load(&config).unwrap();
    let driver = async {
        let conn = fake.connection().await;
        assert!(
            !token_file.exists(),
            "the token file was read once and deleted"
        );
        fake.ready().await;
        fake.dispatch(
            "MESSAGE_CREATE",
            mention(CHAN, &mid(1), &alice(), "summarise the incident"),
        );
        let posts = fake.until_posts(2).await;
        conn.close(1000);
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
    assert_eq!(text_of(&posts[0]), "Looking into it.");
    assert!(text_of(&posts[1]).starts_with("Done: summarise the incident."));
    for post in &posts {
        assert_eq!(post["channel"], CHAN);
        assert_eq!(post["authorization"], AUTHORIZATION);
        assert_eq!(post["body"]["message_reference"]["message_id"], mid(1));
    }
    assert_settled(&scratch.store());
}

#[test]
fn a_token_file_is_read_once_deleted_and_checked() {
    let scratch = Scratch::new("token");
    let good = scratch.root.join("good");
    std::fs::write(&good, format!("  {TOKEN}\n")).unwrap();
    let token = Token::take(&good).unwrap();
    assert!(!good.exists());
    assert_eq!(format!("{token:?}"), "Token(..)");

    for (name, content) in [("empty", ""), ("spaced", "two words"), ("lines", "a\nb")] {
        let path = scratch.root.join(name);
        std::fs::write(&path, content).unwrap();
        let err = Token::take(&path).unwrap_err().to_string();
        assert!(!path.exists(), "{name}: deleted even though it was refused");
        assert!(!err.contains("words"), "{err}");
    }
    assert!(Token::take(&scratch.root.join("missing")).is_err());
}
