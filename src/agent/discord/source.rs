use super::gateway::{self, Closing, Fatal, Frame, op};
use super::{APPROVE_ACTION, DiscordApi, SOURCE, STOP_ACTION, TokenRefused, reply_to, thread};
use crate::agent::ports::{
    BoxFuture, Command, Fact, FactSink, Order, RetryAfter, Source, TriggerSink,
};
use crate::broker::now_ms;
use anyhow::{Context, Result, bail};
use futures_util::{FutureExt, SinkExt, StreamExt};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use solos_agent_core::model::{
    Actor, ActorKind, Addressed, ChannelRef, Ms, TicketId, Trigger, TriggerId,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const HELLO_WAIT: Duration = Duration::from_secs(30);
const MAX_HEARTBEAT: Duration = Duration::from_secs(600);
const MAX_ROOTS: usize = 10_000;
const MAX_THREADS: usize = 50_000;
/// Connections in a row that never became ready before the session is let go.
const RESUME_ATTEMPTS: u32 = 5;
const DISCORD_EPOCH_MS: Ms = 1_420_070_400_000;
const DEFAULT_MESSAGE: u8 = 0;
const REPLY_MESSAGE: u8 = 19;
const COMPONENT_CLICK: u64 = 3;
/// Channel types of a thread: announcement, public, private.
const THREAD_TYPES: [u8; 3] = [10, 11, 12];
const DEFAULT_REFERENCE: u8 = 0;

/// Listens to the configured guilds and channels only; everything else is
/// dropped before it reaches the Engine, and so are the bot's own messages.
pub struct DiscordSource {
    api: Arc<DiscordApi>,
    scope: Scope,
    follow: bool,
    retry_base: Duration,
    retry_max: Duration,
    reidentify: (Duration, Duration),
}

impl DiscordSource {
    /// `guilds` and `channels` are Discord ids. A message enters when its
    /// guild or its channel is listed, or when it is in a thread of a listed
    /// channel; a direct message enters when its channel is listed. With
    /// `follow` it reconnects whenever a connection ends; without, it serves
    /// one connection and ends with it, for tests and dry runs.
    pub fn new(
        api: Arc<DiscordApi>,
        guilds: BTreeSet<String>,
        channels: BTreeSet<String>,
        follow: bool,
    ) -> Self {
        Self {
            api,
            scope: Scope { guilds, channels },
            follow,
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            reidentify: (Duration::from_secs(1), Duration::from_secs(5)),
        }
    }

    /// Reconnect after `base`, twice as long after each failure in a row, up
    /// to `max`.
    #[must_use]
    pub fn with_backoff(mut self, base: Duration, max: Duration) -> Self {
        self.retry_base = base;
        self.retry_max = max;
        self
    }

    /// After an invalid session, wait a random time between `min` and `max`
    /// before identifying again, as Discord asks (1 to 5 seconds).
    #[must_use]
    pub fn with_reidentify_wait(mut self, min: Duration, max: Duration) -> Self {
        self.reidentify = (min, max.max(min));
        self
    }
}

impl Source for DiscordSource {
    fn run(self: Box<Self>, sink: TriggerSink, facts: FactSink) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { self.serve(&sink, &facts).await })
    }
}

#[derive(Clone, Debug, Default)]
struct Scope {
    guilds: BTreeSet<String>,
    channels: BTreeSet<String>,
}

impl Scope {
    fn admits(&self, guild: Option<&str>, channel: &str, parent: Option<&str>) -> bool {
        guild.is_some_and(|guild| self.guilds.contains(guild))
            || self.channels.contains(channel)
            || parent.is_some_and(|parent| self.channels.contains(parent))
    }
}

enum Ended {
    Resume,
    Reidentify,
    Stopped,
}

#[derive(Default)]
struct Link {
    session: Option<Session>,
    seq: Option<u64>,
    unready: u32,
    world: World,
}

impl Link {
    fn forget(&mut self) {
        self.session = None;
        self.seq = None;
    }
}

struct Session {
    id: String,
    resume_url: String,
    me: String,
}

#[derive(Default)]
struct World {
    thread_parents: BTreeMap<u64, u64>,
    message_roots: BTreeMap<u64, u64>,
    bot_roles: BTreeSet<String>,
}

struct Placed {
    root: String,
    reply: ChannelRef,
    parent: Option<String>,
}

impl World {
    fn learn(&mut self, channel: &ChannelInfo) {
        if !THREAD_TYPES.contains(&channel.kind) {
            return;
        }
        let parent = channel.parent_id.as_deref();
        if let (Ok(thread), Some(Ok(parent))) = (channel.id.parse(), parent.map(str::parse)) {
            self.thread_parents.insert(thread, parent);
            if self.thread_parents.len() > MAX_THREADS {
                self.thread_parents.pop_first();
            }
        }
    }

    fn forget_thread(&mut self, id: &str) {
        if let Ok(id) = id.parse() {
            self.thread_parents.remove(&id);
        }
    }

    fn learn_role(&mut self, me: &str, role: &Role) {
        if role.tags.as_ref().and_then(|tags| tags.bot_id.as_deref()) == Some(me) {
            self.bot_roles.insert(role.id.clone());
        }
    }

    fn parent(&self, channel: &str) -> Option<String> {
        let parent = self.thread_parents.get(&channel.parse().ok()?)?;
        Some(parent.to_string())
    }

    fn remember(&mut self, message: &str, root: &str) {
        if let (Ok(message), Ok(root)) = (message.parse(), root.parse()) {
            self.message_roots.insert(message, root);
            if self.message_roots.len() > MAX_ROOTS {
                self.message_roots.pop_first();
            }
        }
    }

    fn known_root(&self, message: &str) -> Option<String> {
        let root = self.message_roots.get(&message.parse().ok()?)?;
        Some(root.to_string())
    }

    /// After a restart the chain is unknown; Discord attaches the message
    /// replied to, so the agent reads one level up from it.
    fn root(&mut self, message: &MessageCreate) -> String {
        let Some(replied) = message
            .message_reference
            .as_ref()
            .and_then(Reference::same_channel_message(&message.channel_id))
        else {
            self.remember(&message.id, &message.id);
            return message.id.clone();
        };
        let above = message
            .referenced_message
            .as_ref()
            .and_then(|above| above.message_reference.as_ref())
            .and_then(Reference::same_channel_message(&message.channel_id));
        let root = self
            .known_root(replied)
            .or_else(|| {
                above.map(|above| self.known_root(above).unwrap_or_else(|| above.to_owned()))
            })
            .unwrap_or_else(|| replied.to_owned());
        self.remember(replied, &root);
        self.remember(&message.id, &root);
        root
    }

    fn place(&mut self, scope: &Scope, message: &MessageCreate) -> Option<Placed> {
        let parent = self.parent(&message.channel_id);
        if !scope.admits(
            message.guild_id.as_deref(),
            &message.channel_id,
            parent.as_deref(),
        ) || !matches!(message.kind, DEFAULT_MESSAGE | REPLY_MESSAGE)
        {
            return None;
        }
        let (root, reply) = if parent.is_some() {
            (
                message.channel_id.clone(),
                reply_to(&message.channel_id, None),
            )
        } else {
            let root = self.root(message);
            let reply = reply_to(&message.channel_id, Some(&root));
            (root, reply)
        };
        Some(Placed {
            root,
            reply,
            parent,
        })
    }

    fn trigger(&mut self, scope: &Scope, me: &str, message: MessageCreate) -> Option<Trigger> {
        let Placed {
            root,
            reply,
            parent,
        } = self.place(scope, &message)?;
        if message.author.id == me {
            return None;
        }
        let mut mentions = vec![format!("<@{me}>"), format!("<@!{me}>")];
        mentions.extend(self.bot_roles.iter().map(|role| format!("<@&{role}>")));
        let content = message.content.trim_start();
        let text = mentions
            .iter()
            .find_map(|mention| content.strip_prefix(mention.as_str()))
            .unwrap_or(content)
            .trim()
            .to_owned();
        let mentioned = mentions
            .iter()
            .any(|mention| message.content.contains(mention.as_str()))
            || message.mentions.iter().any(|user| user.id == me)
            || message
                .mention_roles
                .iter()
                .any(|role| self.bot_roles.contains(role));
        let addressed = if mentioned {
            Addressed::Mention
        } else if message.guild_id.is_none() {
            Addressed::Direct
        } else {
            Addressed::Unaddressed
        };
        let kind = if message.author.bot || message.webhook_id.is_some() {
            ActorKind::Bot
        } else {
            ActorKind::Person
        };
        let actor = Actor {
            id: message.author.id.clone(),
            kind,
            name: message.author.name(),
        };
        let reference = message
            .message_reference
            .as_ref()
            .and_then(|reference| reference.message_id.clone());
        let meta: Map<String, Value> = [
            ("event_type", Some("message_create".to_owned())),
            ("guild", message.guild_id.clone()),
            ("channel", Some(message.channel_id.clone())),
            ("parent", parent),
            ("message", Some(message.id.clone())),
            ("user", Some(message.author.id.clone())),
            ("reference", reference),
        ]
        .into_iter()
        .filter_map(|(key, value)| Some((key.to_owned(), Value::String(value?))))
        .collect();
        Some(Trigger {
            id: TriggerId::new(format!("{SOURCE}:{}:{}", message.channel_id, message.id)),
            source: SOURCE.into(),
            at_ms: snowflake_ms(&message.id).unwrap_or_else(now_ms),
            actor,
            addressed,
            text,
            threads: vec![thread(&root)],
            reply_to: vec![reply],
            meta: Value::Object(meta),
        })
    }

    fn command(&mut self, scope: &Scope, click: &Interaction) -> Option<Command> {
        let channel = click
            .channel_id
            .clone()
            .or_else(|| click.channel.as_ref().map(|channel| channel.id.clone()))?;
        if let Some(info) = &click.channel {
            self.learn(info);
        }
        let parent = self.parent(&channel);
        if !scope.admits(click.guild_id.as_deref(), &channel, parent.as_deref()) {
            return None;
        }
        let custom_id = click.data.as_ref()?.custom_id.as_deref()?;
        let (action, ticket) = custom_id.split_once(':')?;
        let order = match action {
            APPROVE_ACTION => Order::Approve,
            STOP_ACTION => Order::Stop,
            _ => return None,
        };
        if ticket.is_empty() {
            return None;
        }
        let user = click
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref())
            .or(click.user.as_ref())?;
        let reply = if parent.is_some() {
            reply_to(&channel, None)
        } else {
            let clicked = click.message.as_ref()?;
            let root = clicked
                .message_reference
                .as_ref()
                .and_then(Reference::same_channel_message(&channel))
                .unwrap_or(&clicked.id);
            reply_to(&channel, Some(root))
        };
        Some(Command {
            id: format!("{SOURCE}:{}", click.id),
            order,
            ticket: TicketId::new(ticket),
            source: SOURCE.into(),
            by: Actor {
                id: user.id.clone(),
                kind: ActorKind::Person,
                name: user.name(),
            },
            reply_to: reply,
        })
    }
}

fn snowflake_ms(id: &str) -> Option<Ms> {
    let id: u64 = id.parse().ok()?;
    Some((id >> 22) + DISCORD_EPOCH_MS)
}

#[derive(Deserialize)]
struct User {
    id: String,
    username: Option<String>,
    global_name: Option<String>,
    #[serde(default)]
    bot: bool,
}

impl User {
    fn name(&self) -> Option<String> {
        self.username.clone().or_else(|| self.global_name.clone())
    }
}

#[derive(Deserialize)]
struct Reference {
    message_id: Option<String>,
    channel_id: Option<String>,
    #[serde(rename = "type", default)]
    kind: u8,
}

impl Reference {
    /// The message a reference names, when it is a reply in `channel` and not
    /// a forward or a crosspost from elsewhere.
    fn same_channel_message(channel: &str) -> impl Fn(&Reference) -> Option<&str> + '_ {
        move |reference| {
            let here = reference
                .channel_id
                .as_deref()
                .is_none_or(|id| id == channel);
            (reference.kind == DEFAULT_REFERENCE && here)
                .then_some(reference.message_id.as_deref())
                .flatten()
        }
    }
}

#[derive(Deserialize)]
struct Mentioned {
    id: String,
}

#[derive(Deserialize)]
struct Above {
    message_reference: Option<Reference>,
}

#[derive(Deserialize)]
struct MessageCreate {
    id: String,
    channel_id: String,
    guild_id: Option<String>,
    author: User,
    #[serde(default)]
    content: String,
    #[serde(rename = "type", default)]
    kind: u8,
    #[serde(default)]
    mentions: Vec<Mentioned>,
    #[serde(default)]
    mention_roles: Vec<String>,
    webhook_id: Option<String>,
    message_reference: Option<Reference>,
    referenced_message: Option<Box<Above>>,
}

#[derive(Deserialize)]
struct ChannelInfo {
    id: String,
    #[serde(rename = "type", default)]
    kind: u8,
    parent_id: Option<String>,
}

#[derive(Deserialize)]
struct RoleTags {
    bot_id: Option<String>,
}

#[derive(Deserialize)]
struct Role {
    id: String,
    tags: Option<RoleTags>,
}

#[derive(Deserialize)]
struct Component {
    custom_id: Option<String>,
}

#[derive(Deserialize)]
struct Member {
    user: Option<User>,
}

#[derive(Deserialize)]
struct Clicked {
    id: String,
    message_reference: Option<Reference>,
}

#[derive(Deserialize)]
struct Interaction {
    id: String,
    data: Option<Component>,
    guild_id: Option<String>,
    channel_id: Option<String>,
    channel: Option<ChannelInfo>,
    member: Option<Member>,
    user: Option<User>,
    message: Option<Clicked>,
}

fn parse<T: DeserializeOwned>(event: &str, d: Value) -> Option<T> {
    match serde_json::from_value(d) {
        Ok(parsed) => Some(parsed),
        Err(_) => {
            eprintln!("agent: discord: an unreadable {event} event was skipped");
            None
        }
    }
}

impl DiscordSource {
    async fn serve(&self, sink: &TriggerSink, facts: &FactSink) -> Result<()> {
        // tokio-tungstenite builds its TLS config from the process default.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut link = Link::default();
        loop {
            let wait = match self.connection(&mut link, sink, facts).await {
                Ok(Ended::Stopped) => return Ok(()),
                Ok(_) if !self.follow => return Ok(()),
                Ok(Ended::Resume) => self.backoff(link.unready, None),
                Ok(Ended::Reidentify) => self.reidentify_wait(link.unready),
                Err(err) if !self.follow || is_fatal(&err) => return Err(err),
                Err(err) => {
                    eprintln!("agent: discord: {err:#}");
                    self.backoff(link.unready, Some(&err))
                }
            };
            link.unready = link.unready.saturating_add(1);
            if link.unready >= RESUME_ATTEMPTS {
                link.forget();
            }
            eprintln!("agent: discord: reconnecting in {wait:?}");
            tokio::time::sleep(wait).await;
        }
    }

    fn backoff(&self, unready: u32, err: Option<&anyhow::Error>) -> Duration {
        let asked = err
            .and_then(|err| err.chain().find_map(|e| e.downcast_ref::<RetryAfter>()))
            .map(|after| after.0)
            .unwrap_or_default();
        self.retry_base
            .saturating_mul(2u32.saturating_pow(unready))
            .min(self.retry_max)
            .max(asked)
    }

    fn reidentify_wait(&self, unready: u32) -> Duration {
        let (min, max) = self.reidentify;
        let wait = min + max.saturating_sub(min).mul_f64(rand::random::<f64>());
        wait.max(self.backoff(unready, None))
    }

    async fn connection(
        &self,
        link: &mut Link,
        sink: &TriggerSink,
        facts: &FactSink,
    ) -> Result<Ended> {
        let url = match &link.session {
            Some(session) => session.resume_url.clone(),
            None => {
                let gateway = self.api.gateway().await?;
                if gateway.remaining == Some(0) {
                    let spent = anyhow::Error::new(RetryAfter(gateway.reset_after));
                    return Err(spent.context("Discord's limit on new sessions is spent"));
                }
                gateway.url
            }
        };
        let endpoint = gateway::endpoint(&url)?;
        let dial = tokio_tungstenite::connect_async(endpoint.as_str());
        let (socket, _) = tokio::time::timeout(HELLO_WAIT, dial)
            .await
            .context("the Discord gateway did not answer the connection")?
            .context("cannot connect to the Discord gateway")?;
        Conn {
            source: self,
            socket,
            beat: None,
            link,
            sink,
            facts,
            url,
        }
        .run()
        .await
    }
}

fn is_fatal(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|e| e.downcast_ref::<TokenRefused>().is_some() || e.downcast_ref::<Fatal>().is_some())
}

struct Beat {
    every: Duration,
    next: Instant,
    acked: bool,
}

struct Conn<'a> {
    source: &'a DiscordSource,
    socket: Socket,
    beat: Option<Beat>,
    link: &'a mut Link,
    sink: &'a TriggerSink,
    facts: &'a FactSink,
    url: String,
}

impl Conn<'_> {
    async fn run(&mut self) -> Result<Ended> {
        let hello_by = Instant::now() + HELLO_WAIT;
        loop {
            let due = self.beat.as_ref().map(|beat| beat.next);
            tokio::select! {
                next = self.socket.next() => {
                    if let Some(ended) = self.handle(next).await? {
                        return Ok(ended);
                    }
                }
                () = async move {
                    match due {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(ended) = self.heartbeat().await? {
                        return Ok(ended);
                    }
                }
                () = tokio::time::sleep_until(hello_by), if self.beat.is_none() => {
                    bail!("the Discord gateway sent no Hello within {HELLO_WAIT:?}");
                }
            }
        }
    }

    async fn handle(&mut self, next: Option<Result<Message, WsError>>) -> Result<Option<Ended>> {
        let Some(next) = next else {
            return Ok(Some(Ended::Resume));
        };
        let frame = match next.context("the Discord gateway failed")? {
            Message::Text(text) => match serde_json::from_str::<Frame>(text.as_str()) {
                Ok(frame) => frame,
                Err(err) => {
                    eprintln!("agent: discord: an unreadable frame was skipped: {err}");
                    return Ok(None);
                }
            },
            Message::Close(frame) => return self.closed(frame).map(Some),
            _ => return Ok(None),
        };
        self.frame(frame).await
    }

    async fn send(&mut self, text: &str) -> Result<()> {
        self.socket
            .send(Message::text(text))
            .await
            .context("cannot write to the Discord gateway")
    }

    /// Close with a code that keeps the session resumable.
    async fn leave(&mut self) {
        let frame = CloseFrame {
            code: CloseCode::Library(4000),
            reason: "reconnecting".into(),
        };
        let _ = tokio::time::timeout(Duration::from_secs(1), self.socket.close(Some(frame))).await;
    }

    fn closed(&mut self, frame: Option<CloseFrame>) -> Result<Ended> {
        let Some(frame) = frame else {
            return Ok(Ended::Resume);
        };
        let code = u16::from(frame.code);
        match gateway::closing(code) {
            Closing::Fatal(fix) => Err(Fatal { code, fix }.into()),
            Closing::Reidentify => {
                eprintln!(
                    "agent: discord: the gateway closed with code {code}; its session is gone"
                );
                self.link.forget();
                Ok(Ended::Reidentify)
            }
            Closing::Resume => {
                eprintln!("agent: discord: the gateway closed with code {code}; resuming");
                Ok(Ended::Resume)
            }
        }
    }

    async fn heartbeat(&mut self) -> Result<Option<Ended>> {
        while self.beat.as_ref().is_some_and(|beat| !beat.acked) {
            let Some(next) = self.socket.next().now_or_never() else {
                break;
            };
            if let Some(ended) = self.handle(next).await? {
                return Ok(Some(ended));
            }
        }
        let Some(beat) = &mut self.beat else {
            return Ok(None);
        };
        if !beat.acked {
            eprintln!("agent: discord: the gateway did not acknowledge a heartbeat; reconnecting");
            self.leave().await;
            return Ok(Some(Ended::Resume));
        }
        beat.acked = false;
        beat.next = Instant::now() + beat.every;
        let text = gateway::heartbeat(self.link.seq);
        self.send(&text).await?;
        Ok(None)
    }

    async fn frame(&mut self, frame: Frame) -> Result<Option<Ended>> {
        match frame.op {
            op::HELLO => self.hello(&frame.d).await?,
            op::HEARTBEAT => {
                let text = gateway::heartbeat(self.link.seq);
                self.send(&text).await?;
            }
            op::HEARTBEAT_ACK => {
                if let Some(beat) = &mut self.beat {
                    beat.acked = true;
                }
            }
            op::RECONNECT => {
                eprintln!("agent: discord: the gateway asked to reconnect");
                self.leave().await;
                return Ok(Some(Ended::Resume));
            }
            op::INVALID_SESSION => {
                self.leave().await;
                if frame.d.as_bool() == Some(true) && self.link.session.is_some() {
                    eprintln!("agent: discord: the session is invalid but resumable");
                    return Ok(Some(Ended::Resume));
                }
                eprintln!("agent: discord: the session is invalid; identifying again");
                self.link.forget();
                return Ok(Some(Ended::Reidentify));
            }
            op::DISPATCH => return self.dispatch(frame).await,
            _ => {}
        }
        Ok(None)
    }

    async fn hello(&mut self, d: &Value) -> Result<()> {
        let Some(ms) = d["heartbeat_interval"].as_u64() else {
            bail!("Discord's Hello named no heartbeat_interval");
        };
        let every = Duration::from_millis(ms).clamp(Duration::from_millis(1), MAX_HEARTBEAT);
        self.beat = Some(Beat {
            every,
            next: Instant::now() + every.mul_f64(rand::random::<f64>()),
            acked: true,
        });
        let token = self.source.api.token();
        let resume = self
            .link
            .session
            .as_ref()
            .zip(self.link.seq)
            .map(|(session, seq)| gateway::resume(token, &session.id, seq));
        let opening = match resume {
            Some(resume) => resume,
            None => {
                self.link.forget();
                gateway::identify(token)
            }
        };
        self.send(&opening).await
    }

    async fn dispatch(&mut self, frame: Frame) -> Result<Option<Ended>> {
        if let Some(seq) = frame.s {
            self.link.seq = Some(seq);
        }
        let Some(event) = frame.t.as_deref() else {
            return Ok(None);
        };
        let d = frame.d;
        match event {
            "READY" => self.ready(&d)?,
            "RESUMED" => self.link.unready = 0,
            "GUILD_CREATE" | "THREAD_LIST_SYNC" => {
                let threads = serde_json::from_value::<Vec<ChannelInfo>>(d["threads"].clone());
                for info in threads.unwrap_or_default() {
                    self.link.world.learn(&info);
                }
                let roles = serde_json::from_value::<Vec<Role>>(d["roles"].clone());
                if let Some(me) = self.link.session.as_ref().map(|session| &session.me) {
                    for role in roles.unwrap_or_default() {
                        self.link.world.learn_role(me, &role);
                    }
                }
            }
            "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" => {
                let role = serde_json::from_value::<Role>(d["role"].clone());
                if let (Some(session), Ok(role)) = (&self.link.session, role) {
                    self.link.world.learn_role(&session.me, &role);
                }
            }
            "THREAD_CREATE" | "THREAD_UPDATE" => {
                if let Some(info) = parse::<ChannelInfo>(event, d) {
                    self.link.world.learn(&info);
                }
            }
            "THREAD_DELETE" => {
                if let Some(id) = d["id"].as_str() {
                    self.link.world.forget_thread(id);
                }
            }
            "MESSAGE_CREATE" => {
                let Some(me) = self.link.session.as_ref().map(|session| session.me.clone()) else {
                    return Ok(None);
                };
                let Some(message) = parse::<MessageCreate>(event, d) else {
                    return Ok(None);
                };
                let trigger = self.link.world.trigger(&self.source.scope, &me, message);
                if let Some(trigger) = trigger
                    && self.sink.send(vec![trigger]).await.is_err()
                {
                    return Ok(Some(Ended::Stopped));
                }
            }
            "INTERACTION_CREATE" => {
                if d["type"].as_u64() != Some(COMPONENT_CLICK) {
                    return Ok(None);
                }
                if let (Some(id), Some(token)) = (d["id"].as_str(), d["token"].as_str()) {
                    self.acknowledge(id, token);
                }
                let Some(click) = parse::<Interaction>(event, d) else {
                    return Ok(None);
                };
                let command = self.link.world.command(&self.source.scope, &click);
                if let Some(command) = command
                    && self.facts.send(Fact::Command(command)).await.is_err()
                {
                    return Ok(Some(Ended::Stopped));
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn ready(&mut self, d: &Value) -> Result<()> {
        let text = |value: &Value| value.as_str().map(str::to_owned);
        let (Some(me), Some(id)) = (text(&d["user"]["id"]), text(&d["session_id"])) else {
            bail!("Discord's READY named no user or session");
        };
        let resume_url = text(&d["resume_gateway_url"]).unwrap_or_else(|| self.url.clone());
        self.link.session = Some(Session { id, resume_url, me });
        self.link.unready = 0;
        Ok(())
    }

    /// Every click is acknowledged, from a task of its own so that a slow
    /// call cannot hold up the heartbeats. Discord gives three seconds.
    fn acknowledge(&self, id: &str, token: &str) {
        let api = self.source.api.clone();
        let (id, token) = (id.to_owned(), token.to_owned());
        tokio::spawn(async move {
            if let Err(err) = api.acknowledge(&id, &token).await {
                eprintln!("agent: discord: a click was not acknowledged: {err:#}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{ChannelInfo, MessageCreate, Scope, World, snowflake_ms};

    #[test]
    fn a_snowflake_reads_as_its_creation_time() {
        assert_eq!(snowflake_ms("175928847299117063"), Some(1_462_015_105_796));
        assert_eq!(snowflake_ms("soon"), None);
    }

    fn message(id: &str, reference: Option<&str>) -> MessageCreate {
        serde_json::from_value(serde_json::json!({
            "id": id, "channel_id": "10", "guild_id": "1",
            "author": { "id": "7" },
            "message_reference": reference.map(|r| serde_json::json!({ "message_id": r })),
        }))
        .unwrap()
    }

    #[test]
    fn a_reply_chain_roots_at_its_first_message_and_a_thread_is_its_own_root() {
        let mut world = World::default();
        let scope = Scope {
            guilds: ["1".to_owned()].into(),
            ..Scope::default()
        };
        let thread: ChannelInfo = serde_json::from_value(serde_json::json!({
            "id": "20", "type": 11, "parent_id": "10",
        }))
        .unwrap();
        world.learn(&thread);
        assert_eq!(world.root(&message("100", None)), "100");
        assert_eq!(world.root(&message("101", Some("100"))), "100");
        assert_eq!(world.root(&message("102", Some("101"))), "100");
        assert_eq!(
            world.root(&message("200", Some("199"))),
            "199",
            "an unseen message is its own root"
        );
        assert_eq!(world.parent("20").as_deref(), Some("10"));
        assert!(scope.admits(Some("1"), "99", None));
        assert!(!scope.admits(Some("2"), "99", None));
    }
}
