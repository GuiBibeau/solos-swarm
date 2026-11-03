//! Discord: a Gateway Source and a REST Channel. Triggers name source
//! [`SOURCE`]; their ids, Threads and reply addresses carry Discord's
//! snowflake ids.
//!
//! Who is who. An actor is `discord:<user id>`, so the Policy lists
//! `people: [discord:123]`. `author.bot` and webhook posts are Bots.
//!
//! Threads. A Thread is `discord:<id>` where the id is one snowflake:
//! - a message inside a Discord thread belongs to the Thread of that thread
//!   channel, `discord:<thread channel id>`;
//! - a top-level message in a channel or a DM starts the Thread
//!   `discord:<its message id>`;
//! - a reply (`message_reference`) belongs to the Thread of the message at
//!   the top of its reply chain. The Source keeps the roots of the messages
//!   it has seen, the agent's own posts included, so a reply to the agent's
//!   answer joins the request it answered. After a restart it reads one
//!   level up from the `referenced_message` Discord attaches.
//!
//! A thread started from a message has that message's id, so the message's
//! reply chain and the thread it grew into are one Thread.
//!
//! Replies. A reply address is `{channel, message_id?}`: post in `channel`,
//! and with `message_id` as a reply to it. A Thread's id is
//! `message_id.unwrap_or(channel)`, so [`thread`] and [`reply_to`] agree for
//! a message in a channel (`{C, M}` is Thread M) and for one in a thread
//! channel (`{T}` is Thread T).
//!
//! Addressed. A message that mentions the bot (`<@id>`, the role Discord makes
//! for it, or a reply that pings it) is a `Mention`, a DM is `Direct`, the
//! rest are `Unaddressed`.
//!
//! Idempotent posts. Each post carries a `nonce` derived from its `MessageId`
//! and `enforce_nonce`, so Discord returns the earlier message for a retry
//! instead of posting twice. Discord checks the nonce for a few minutes
//! only, so a retry that comes later can post a message twice.

mod channel;
mod gateway;
mod source;

pub use channel::DiscordChannel;
pub use gateway::INTENTS;
pub use source::DiscordSource;

use super::ports::RetryAfter;
use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::header::{AUTHORIZATION, HeaderValue, RETRY_AFTER};
use reqwest::{RequestBuilder, StatusCode, Url};
use serde_json::Value;
use solos_agent_core::model::{ChannelRef, ThreadKey};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

pub const SOURCE: &str = "discord";
pub const DEFAULT_API_BASE: &str = "https://discord.com/api/v10/";
/// The `custom_id` prefixes of an approval request's buttons. A budget
/// button is followed by `:` and the Ticket id.
pub const APPROVE_ACTION: &str = "solos_approve";
pub const STOP_ACTION: &str = "solos_stop";
/// The prefixes of a vault item's Allow and Deny. Each is followed by `:`, the
/// Ticket id, `:` and the item's `SecretId`, which holds no `:`.
pub const ALLOW_SECRET_ACTION: &str = "solos_secret_allow";
pub const DENY_SECRET_ACTION: &str = "solos_secret_deny";

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRY_AFTER: f64 = 3_600.0;
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(1);

/// The Thread of a message and what follows it: a message id, or a thread
/// channel's id.
pub fn thread(id: &str) -> ThreadKey {
    ThreadKey::new(format!("{SOURCE}:{id}"))
}

pub fn reply_to(channel: &str, message: Option<&str>) -> ChannelRef {
    let mut address = BTreeMap::from([("channel".to_owned(), channel.to_owned())]);
    if let Some(message) = message {
        address.insert("message_id".to_owned(), message.to_owned());
    }
    ChannelRef {
        source: SOURCE.into(),
        address,
    }
}

/// A snowflake as Discord writes it: decimal digits.
pub fn valid_id(id: &str) -> bool {
    (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// A Discord bot token, held in memory only. Its `Debug` never shows it.
#[derive(Clone)]
pub struct Token(Arc<Zeroizing<String>>);

impl Token {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(Arc::new(Zeroizing::new(secret.into())))
    }

    /// Read the file once and delete it, so the token lives only in this process.
    pub fn take(path: &Path) -> Result<Self> {
        let read = std::fs::read_to_string(path);
        if let Err(err) = std::fs::remove_file(path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "agent: discord: cannot delete the token file {}: {err}",
                path.display()
            );
        }
        let raw =
            Zeroizing::new(read.with_context(|| {
                format!("cannot read the Discord token file {}", path.display())
            })?);
        let token = raw.trim();
        ensure!(
            !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()),
            "the Discord token in {} is empty or holds a space or a control character",
            path.display()
        );
        Ok(Self::new(token))
    }

    fn secret(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

#[derive(Debug)]
pub(crate) struct TokenRefused {
    what: &'static str,
}

impl std::fmt::Display for TokenRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "discord {} refused the bot token (HTTP 401). Reset the token on the application's \
             Bot page in the developer portal and write the new one to the token file",
            self.what
        )
    }
}

impl std::error::Error for TokenRefused {}

pub(crate) enum Reply {
    Ok(Value),
    Refused(Refusal),
}

pub(crate) struct Refusal {
    pub(crate) status: StatusCode,
    /// Discord's JSON error code, such as 50001 (Missing Access).
    pub(crate) code: Option<i64>,
    pub(crate) message: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.status)?;
        if let Some(code) = self.code {
            write!(f, ", code {code}")?;
        }
        write!(f, ": {}", self.message)
    }
}

pub(crate) struct Gateway {
    pub(crate) url: String,
    /// The sessions Discord still lets this bot start in its current window.
    pub(crate) remaining: Option<u64>,
    pub(crate) reset_after: Duration,
}

/// The REST API, called with the bot token unless a call carries its own
/// credential.
pub struct DiscordApi {
    base: Url,
    http: reqwest::Client,
    bot: Token,
}

impl DiscordApi {
    /// `base` is the API root, [`DEFAULT_API_BASE`] unless a test points it
    /// at a fake.
    pub fn new(base: &str, bot: Token) -> Result<Self> {
        let base =
            Url::parse(base.trim()).with_context(|| format!("invalid Discord API base {base}"))?;
        ensure!(
            matches!(base.scheme(), "http" | "https") && !base.cannot_be_a_base(),
            "the Discord API base {base} is not an http(s) URL"
        );
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!(
                "DiscordBot (",
                env!("CARGO_PKG_REPOSITORY"),
                ", ",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .build()?;
        Ok(Self { base, http, bot })
    }

    pub(crate) fn token(&self) -> &Token {
        &self.bot
    }

    pub(crate) async fn gateway(&self) -> Result<Gateway> {
        let request = self.http.get(self.url(&["gateway", "bot"])?);
        match self
            .call("GET /gateway/bot", self.authorize(request)?)
            .await?
        {
            Reply::Ok(found) => {
                let url = found["url"]
                    .as_str()
                    .context("discord GET /gateway/bot named no url")?;
                let limit = &found["session_start_limit"];
                Ok(Gateway {
                    url: url.to_owned(),
                    remaining: limit["remaining"].as_u64(),
                    reset_after: limit["reset_after"]
                        .as_f64()
                        .and_then(|ms| wait(ms / 1_000.0))
                        .unwrap_or(DEFAULT_RETRY_AFTER),
                })
            }
            Reply::Refused(refusal) => bail!("discord GET /gateway/bot: {refusal}"),
        }
    }

    pub(crate) async fn create_message(&self, channel: &str, body: &Value) -> Result<Reply> {
        let request = self
            .http
            .post(self.url(&["channels", channel, "messages"])?)
            .json(body);
        self.call("POST /channels/{id}/messages", self.authorize(request)?)
            .await
    }

    /// Acknowledge a click with DEFERRED_UPDATE_MESSAGE, which leaves the
    /// message as it is. The interaction's own token is the credential, so
    /// the bot token stays out of the call.
    pub(crate) async fn acknowledge(&self, interaction: &str, token: &str) -> Result<()> {
        let request = self
            .http
            .post(self.url(&["interactions", interaction, token, "callback"])?)
            .json(&serde_json::json!({ "type": 6 }));
        match self
            .call("POST /interactions/{id}/{token}/callback", request)
            .await?
        {
            Reply::Ok(_) => Ok(()),
            Reply::Refused(refusal) => bail!("discord interaction callback: {refusal}"),
        }
    }

    fn url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| anyhow!("the Discord API base cannot take a path"))?
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }

    fn authorize(&self, request: RequestBuilder) -> Result<RequestBuilder> {
        let bot = Zeroizing::new(format!("Bot {}", self.bot.secret()));
        let mut value =
            HeaderValue::from_str(&bot).context("the Discord token is not a valid header value")?;
        value.set_sensitive(true);
        Ok(request.header(AUTHORIZATION, value))
    }

    /// Errors never carry the request's URL, which can hold an interaction's
    /// token.
    async fn call(&self, what: &'static str, request: RequestBuilder) -> Result<Reply> {
        let response = request
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("discord {what} failed"))?;
        let status = response.status();
        let header_wait = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<f64>().ok());
        let bytes = response
            .bytes()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("discord {what}: the answer did not arrive"))?;
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status == StatusCode::TOO_MANY_REQUESTS {
            let wait = body["retry_after"]
                .as_f64()
                .and_then(wait)
                .or_else(|| header_wait.and_then(wait))
                .unwrap_or(DEFAULT_RETRY_AFTER);
            return Err(anyhow::Error::new(RetryAfter(wait)).context(format!("discord {what}")));
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(TokenRefused { what }.into());
        }
        if status.is_success() {
            return Ok(Reply::Ok(body));
        }
        ensure!(status.is_client_error(), "discord {what}: HTTP {status}");
        Ok(Reply::Refused(Refusal {
            status,
            code: body["code"].as_i64(),
            message: body["message"].as_str().unwrap_or("no message").to_owned(),
        }))
    }
}

fn wait(seconds: f64) -> Option<Duration> {
    seconds
        .is_finite()
        .then(|| Duration::from_secs_f64(seconds.clamp(0.0, MAX_RETRY_AFTER)))
}

#[cfg(test)]
mod tests {
    use super::{valid_id, wait};
    use std::time::Duration;

    #[test]
    fn ids_are_snowflakes_and_waits_are_bounded() {
        assert!(valid_id("123456789012345678"));
        for bad in ["", "12a", "../1", "1/2", &"1".repeat(21)] {
            assert!(!valid_id(bad), "{bad}");
        }
        assert_eq!(wait(1.5), Some(Duration::from_millis(1_500)));
        assert_eq!(wait(1e12), Some(Duration::from_secs(3_600)));
        assert_eq!(wait(-3.0), Some(Duration::ZERO));
        assert_eq!(wait(f64::NAN), None);
    }
}
