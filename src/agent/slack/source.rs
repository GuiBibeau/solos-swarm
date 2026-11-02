//! Socket Mode: Slack pushes events and button clicks over a WebSocket the
//! agent opened with the app-level token, and the agent acknowledges every
//! envelope by its id.

use super::{
    APPROVE_ACTION, Answer, SOURCE, STOP_ACTION, SlackApi, Token, TokenRefused, reply_to, thread,
};
use crate::agent::ports::{
    BoxFuture, Command, Fact, FactSink, Order, RetryAfter, Source, TriggerSink,
};
use crate::broker::now_ms;
use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use solos_agent_core::model::{Actor, ActorKind, Addressed, Ms, TicketId, Trigger, TriggerId};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::tungstenite::Message;

/// A quiet socket is pinged this often, and replaced once nothing at all has
/// arrived for `SILENCE`.
const PING: Duration = Duration::from_secs(30);
const SILENCE: Duration = Duration::from_secs(90);

/// Message subtypes that are new messages. The others (edits, deletions,
/// joins, topic changes) are not requests.
const SUBTYPES: [&str; 3] = ["bot_message", "thread_broadcast", "file_share"];

/// Listens to the configured channels only; everything else is dropped
/// before it reaches the Engine, and so are the bot's own messages.
pub struct SlackSource {
    api: Arc<SlackApi>,
    app: Token,
    channels: BTreeSet<String>,
    follow: bool,
    retry_base: Duration,
    retry_max: Duration,
}

impl SlackSource {
    /// `channels` are Slack ids; a direct message's `D` id can be listed too.
    /// With `follow` it reconnects whenever a connection ends; without, it
    /// serves one connection and ends with it, for tests and dry runs.
    pub fn new(api: Arc<SlackApi>, app: Token, channels: BTreeSet<String>, follow: bool) -> Self {
        Self {
            api,
            app,
            channels,
            follow,
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
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
}

impl Source for SlackSource {
    fn run(self: Box<Self>, sink: TriggerSink, facts: FactSink) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { self.serve(&sink, &facts).await })
    }
}

/// The bot, as `auth.test` names it.
struct Identity {
    user: String,
    bot: Option<String>,
}

enum Ended {
    /// Slack closed or asked to reconnect.
    Closed,
    /// The runtime stopped.
    Stopped,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    envelope_id: Option<String>,
    #[serde(default)]
    payload: Value,
    reason: Option<String>,
}

/// The fields of a `message` or `app_mention` event this Source reads.
#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    subtype: Option<String>,
    channel: Option<String>,
    channel_type: Option<String>,
    user: Option<String>,
    bot_id: Option<String>,
    username: Option<String>,
    text: Option<String>,
    ts: Option<String>,
    thread_ts: Option<String>,
}

impl SlackSource {
    async fn serve(&self, sink: &TriggerSink, facts: &FactSink) -> Result<()> {
        // tokio-tungstenite builds its TLS config from the process default.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let me = self.identity().await?;
        let mut failures = 0;
        loop {
            let wait = match self.connection(&me, sink, facts, &mut failures).await {
                Ok(Ended::Stopped) => return Ok(()),
                Ok(Ended::Closed) if !self.follow => return Ok(()),
                Ok(Ended::Closed) => self.backoff(failures, None),
                Err(err) if !self.follow || is_fatal(&err) => return Err(err),
                Err(err) => {
                    eprintln!("agent: slack: {err:#}");
                    self.backoff(failures, Some(&err))
                }
            };
            failures = failures.saturating_add(1);
            eprintln!("agent: slack: reconnecting in {wait:?}");
            tokio::time::sleep(wait).await;
        }
    }

    fn backoff(&self, failures: u32, err: Option<&anyhow::Error>) -> Duration {
        let asked = err
            .and_then(|err| err.chain().find_map(|e| e.downcast_ref::<RetryAfter>()))
            .map(|after| after.0)
            .unwrap_or_default();
        self.retry_base
            .saturating_mul(2u32.saturating_pow(failures))
            .min(self.retry_max)
            .max(asked)
    }

    async fn identity(&self) -> Result<Identity> {
        let mut failures = 0;
        loop {
            let err = match self.api.post("auth.test", None, None).await {
                Ok(Answer::Ok(me)) => {
                    let user = me["user_id"]
                        .as_str()
                        .context("slack auth.test named no user_id")?;
                    return Ok(Identity {
                        user: user.to_owned(),
                        bot: me["bot_id"].as_str().map(str::to_owned),
                    });
                }
                Ok(Answer::Refused(code)) => anyhow::anyhow!("slack auth.test: {code}"),
                Err(err) => err,
            };
            if !self.follow || is_fatal(&err) {
                return Err(err);
            }
            let wait = self.backoff(failures, Some(&err));
            failures = failures.saturating_add(1);
            eprintln!("agent: slack: {err:#}; retrying in {wait:?}");
            tokio::time::sleep(wait).await;
        }
    }

    /// One socket, from `apps.connections.open` to its close.
    async fn connection(
        &self,
        me: &Identity,
        sink: &TriggerSink,
        facts: &FactSink,
        failures: &mut u32,
    ) -> Result<Ended> {
        let url = match self
            .api
            .post("apps.connections.open", Some(&self.app), None)
            .await?
        {
            Answer::Ok(opened) => opened["url"]
                .as_str()
                .context("slack apps.connections.open named no url")?
                .to_owned(),
            Answer::Refused(code) => bail!("slack apps.connections.open: {code}"),
        };
        // The URL carries a connection ticket, so errors leave it out.
        let (mut socket, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .context("cannot connect to the Slack socket")?;
        let mut ping = tokio::time::interval(PING);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ping.tick().await;
        let mut heard = Instant::now();
        loop {
            let frame = tokio::select! {
                frame = socket.next() => frame,
                _ = ping.tick() => {
                    if heard.elapsed() > SILENCE {
                        bail!("the Slack socket went silent");
                    }
                    socket
                        .send(Message::Ping(Vec::new().into()))
                        .await
                        .context("cannot ping the Slack socket")?;
                    continue;
                }
            };
            let Some(frame) = frame else {
                return Ok(Ended::Closed);
            };
            heard = Instant::now();
            let text = match frame.context("the Slack socket failed")? {
                Message::Text(text) => text,
                Message::Close(_) => return Ok(Ended::Closed),
                _ => continue,
            };
            let envelope: Envelope = match serde_json::from_str(text.as_str()) {
                Ok(envelope) => envelope,
                Err(err) => {
                    eprintln!("agent: slack: an unreadable envelope was skipped: {err}");
                    continue;
                }
            };
            match envelope.kind.as_str() {
                "hello" => *failures = 0,
                "disconnect" => {
                    let reason = envelope.reason.as_deref().unwrap_or("none given");
                    eprintln!("agent: slack: Slack asked to reconnect ({reason})");
                    return Ok(Ended::Closed);
                }
                "events_api" => {
                    if let Some(trigger) = self.trigger(&envelope.payload, me)
                        && sink.send(vec![trigger]).await.is_err()
                    {
                        return Ok(Ended::Stopped);
                    }
                }
                "interactive" => {
                    for command in self.commands(&envelope.payload) {
                        if facts.send(Fact::Command(command)).await.is_err() {
                            return Ok(Ended::Stopped);
                        }
                    }
                }
                _ => {}
            }
            // Acknowledged once handed on: a crash before this makes Slack
            // deliver it again, and the Engine stores each Trigger id once.
            if let Some(id) = envelope.envelope_id {
                let ack = json!({ "envelope_id": id }).to_string();
                socket
                    .send(Message::text(ack))
                    .await
                    .context("cannot acknowledge a Slack envelope")?;
            }
        }
    }

    /// A `message` or `app_mention` event as a Trigger, or `None` when it
    /// is not a new message from someone else in a listened channel.
    fn trigger(&self, payload: &Value, me: &Identity) -> Option<Trigger> {
        let event = Event::deserialize(payload.get("event")?).ok()?;
        if !matches!(event.kind.as_str(), "message" | "app_mention")
            || event
                .subtype
                .as_deref()
                .is_some_and(|subtype| !SUBTYPES.contains(&subtype))
        {
            return None;
        }
        let channel = event.channel.clone()?;
        let own = event.user.as_deref() == Some(me.user.as_str())
            || (event.bot_id.is_some() && event.bot_id == me.bot);
        if !self.channels.contains(&channel) || own {
            return None;
        }
        let ts = event.ts.clone()?;
        let actor = if event.bot_id.is_some() || event.subtype.as_deref() == Some("bot_message") {
            Actor {
                id: event.bot_id.clone().or_else(|| event.user.clone())?,
                kind: ActorKind::Bot,
                name: event.username.clone(),
            }
        } else {
            Actor::person(event.user.clone()?)
        };
        let text = event.text.clone().unwrap_or_default();
        let mention = format!("<@{}>", me.user);
        let mentioned = event.kind == "app_mention"
            || text.contains(&mention)
            || text.contains(&format!("<@{}|", me.user));
        let direct = event
            .channel_type
            .as_deref()
            .map_or(channel.starts_with('D'), |kind| kind == "im");
        let addressed = if mentioned {
            Addressed::Mention
        } else if direct {
            Addressed::Direct
        } else {
            Addressed::Unaddressed
        };
        let root = event.thread_ts.clone().unwrap_or_else(|| ts.clone());
        let text = text
            .trim_start()
            .strip_prefix(&mention)
            .unwrap_or(&text)
            .trim()
            .to_owned();
        let field = |key: &str| payload[key].as_str().map(str::to_owned);
        let meta: Map<String, Value> = [
            ("event_type", Some(event.kind.clone())),
            ("channel", Some(channel.clone())),
            ("ts", Some(ts.clone())),
            ("thread_ts", event.thread_ts.clone()),
            ("user", event.user.clone()),
            ("bot_id", event.bot_id.clone()),
            ("subtype", event.subtype.clone()),
            ("channel_type", event.channel_type.clone()),
            ("team", field("team_id")),
            ("event_id", field("event_id")),
        ]
        .into_iter()
        .filter_map(|(key, value)| Some((key.to_owned(), Value::String(value?))))
        .collect();
        Some(Trigger {
            id: TriggerId::new(format!("{SOURCE}:{channel}:{ts}")),
            source: SOURCE.into(),
            at_ms: ts_ms(&ts).unwrap_or_else(now_ms),
            actor,
            addressed,
            text,
            threads: vec![thread(&channel, &root)],
            reply_to: vec![reply_to(&channel, Some(&root))],
            meta: Value::Object(meta),
        })
    }

    /// The Approve and Stop clicks of a `block_actions` payload. Slack
    /// vouches for the user; the runtime asks the Policy what they may do.
    fn commands(&self, payload: &Value) -> Vec<Command> {
        let text = |value: &Value| value.as_str().map(str::to_owned);
        if payload["type"] != "block_actions" {
            return Vec::new();
        }
        let container = &payload["container"];
        let message = &payload["message"];
        let (Some(user), Some(channel)) = (
            text(&payload["user"]["id"]),
            text(&container["channel_id"]).or_else(|| text(&payload["channel"]["id"])),
        ) else {
            return Vec::new();
        };
        let Some(root) = [
            &container["thread_ts"],
            &message["thread_ts"],
            &container["message_ts"],
            &message["ts"],
        ]
        .into_iter()
        .find_map(text) else {
            return Vec::new();
        };
        if !self.channels.contains(&channel) {
            return Vec::new();
        }
        let by = Actor {
            id: user.clone(),
            kind: ActorKind::Person,
            name: text(&payload["user"]["username"]).or_else(|| text(&payload["user"]["name"])),
        };
        let Some(actions) = payload["actions"].as_array() else {
            return Vec::new();
        };
        actions
            .iter()
            .filter_map(|action| {
                let id = action["action_id"].as_str()?;
                let order = match id {
                    APPROVE_ACTION => Order::Approve,
                    STOP_ACTION => Order::Stop,
                    _ => return None,
                };
                let at = action["action_ts"].as_str().unwrap_or_default();
                Some(Command {
                    id: format!("{SOURCE}:{channel}:{at}:{user}:{id}"),
                    order,
                    ticket: TicketId::new(action["value"].as_str()?),
                    source: SOURCE.into(),
                    by: by.clone(),
                    reply_to: reply_to(&channel, Some(&root)),
                })
            })
            .collect()
    }
}

fn is_fatal(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|e| e.downcast_ref::<TokenRefused>().is_some())
}

/// Slack's `ts` is seconds with a microsecond fraction: `1700000000.123456`.
fn ts_ms(ts: &str) -> Option<Ms> {
    let (seconds, fraction) = ts.split_once('.').unwrap_or((ts, ""));
    let millis: String = fraction
        .chars()
        .chain(std::iter::repeat('0'))
        .take(3)
        .collect();
    seconds
        .parse::<Ms>()
        .ok()?
        .checked_mul(1_000)?
        .checked_add(millis.parse().ok()?)
}

#[cfg(test)]
mod tests {
    use super::ts_ms;
    use tokio::io::AsyncWriteExt;
    use tokio_tungstenite::tungstenite::Error;

    #[test]
    fn a_slack_timestamp_reads_as_milliseconds() {
        assert_eq!(ts_ms("1700000000.123456"), Some(1_700_000_000_123));
        assert_eq!(ts_ms("1700000000.5"), Some(1_700_000_000_500));
        assert_eq!(ts_ms("1700000000"), Some(1_700_000_000_000));
        assert_eq!(ts_ms("soon"), None);
    }

    /// Slack's socket is `wss://`, which the fake Slack does not serve: the
    /// client must start a rustls handshake, not refuse the scheme or panic.
    #[tokio::test]
    async fn a_wss_url_is_dialed_over_rustls() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let plain = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
        });
        let url = format!("wss://127.0.0.1:{port}/link/");
        let err = tokio_tungstenite::connect_async(url).await.unwrap_err();
        assert!(!matches!(err, Error::Url(_)), "{err}");
        plain.await.unwrap();
    }
}
