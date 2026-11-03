//! Slack: a Socket Mode Source and a Web API Channel. The agent opens an
//! outbound WebSocket, so nothing on the host listens to the internet.
//! Triggers name source [`SOURCE`]; their ids, Threads and reply addresses
//! carry Slack's channel id and message timestamps.

mod channel;
mod source;

pub use channel::SlackChannel;
pub use source::SlackSource;

use super::ports::RetryAfter;
use anyhow::{Context, Result, ensure};
use reqwest::header::RETRY_AFTER;
use reqwest::{RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solos_agent_core::model::{ChannelRef, SecretId, ThreadKey, TicketId};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

/// The Source name Slack Triggers, replies and approvals carry.
pub const SOURCE: &str = "slack";
pub const DEFAULT_API_BASE: &str = "https://slack.com/api/";
/// The Block Kit action ids of an approval request's buttons. A budget
/// button's value is the Ticket id.
pub const APPROVE_ACTION: &str = "solos_approve";
pub const STOP_ACTION: &str = "solos_stop";
/// The action ids of a vault item's Allow and Deny. Each button's value is a
/// [`SecretClick`] as JSON.
pub const ALLOW_SECRET_ACTION: &str = "solos_secret_allow";
pub const DENY_SECRET_ACTION: &str = "solos_secret_deny";
/// The message metadata type that marks what the agent posted; its payload's
/// `message_id` is the `OutboundMessage` id.
pub const METADATA_EVENT: &str = "solos_swarm_message";

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRY_AFTER: u64 = 3_600;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretClick {
    ticket: TicketId,
    secret: SecretId,
}

/// The Thread of a message and its replies.
pub fn thread(channel: &str, thread_ts: &str) -> ThreadKey {
    ThreadKey::new(format!("{SOURCE}:{channel}:{thread_ts}"))
}

/// Where a reply goes: a thread, or with no `thread_ts` a new message in
/// the channel.
pub fn reply_to(channel: &str, thread_ts: Option<&str>) -> ChannelRef {
    let mut address = BTreeMap::from([("channel".to_owned(), channel.to_owned())]);
    if let Some(thread_ts) = thread_ts {
        address.insert("thread_ts".to_owned(), thread_ts.to_owned());
    }
    ChannelRef {
        source: SOURCE.into(),
        address,
    }
}

/// A Slack token, held in memory only. Its `Debug` never shows it.
#[derive(Clone)]
pub struct Token(Arc<Zeroizing<String>>);

impl Token {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(Arc::new(Zeroizing::new(secret.into())))
    }

    /// Read the file once and delete it, as `solos-swarm run` does with its
    /// credential, so the token lives only in this process. `prefix` is the
    /// token type's (`xapp-`, `xoxb-`), which catches swapped files early.
    pub fn take(path: &Path, prefix: &str) -> Result<Self> {
        let read = std::fs::read_to_string(path);
        let _ = std::fs::remove_file(path);
        let raw = Zeroizing::new(
            read.with_context(|| format!("cannot read the Slack token file {}", path.display()))?,
        );
        let token = raw.trim();
        ensure!(
            token.starts_with(prefix),
            "the Slack token in {} does not start with {prefix}",
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

/// The Web API, called with the bot token unless a call names another.
pub struct SlackApi {
    base: String,
    http: reqwest::Client,
    bot: Token,
}

/// A Web API answer that arrived: `ok`, or Slack's error code.
pub(crate) enum Answer {
    Ok(Value),
    Refused(String),
}

/// Slack refused a token, so retrying cannot help until the operator
/// replaces it.
#[derive(Debug)]
pub(crate) struct TokenRefused {
    method: &'static str,
    code: String,
}

impl std::fmt::Display for TokenRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "slack {} refused the token: {}", self.method, self.code)
    }
}

impl std::error::Error for TokenRefused {}

impl SlackApi {
    /// `base` is the Web API root, [`DEFAULT_API_BASE`] unless a test points
    /// it at a fake.
    pub fn new(base: &str, bot: Token) -> Result<Self> {
        let mut base = base.trim().to_owned();
        if !base.ends_with('/') {
            base.push('/');
        }
        reqwest::Url::parse(&base).with_context(|| format!("invalid Slack API base {base}"))?;
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { base, http, bot })
    }

    /// POST a JSON body (or none) with `token`, or the bot token.
    pub(crate) async fn post(
        &self,
        method: &'static str,
        token: Option<&Token>,
        body: Option<&Value>,
    ) -> Result<Answer> {
        let token = token.unwrap_or(&self.bot);
        let mut request = self
            .http
            .post(format!("{}{method}", self.base))
            .bearer_auth(token.secret());
        if let Some(body) = body {
            request = request.json(body);
        }
        answer(method, request).await
    }

    pub(crate) async fn get(&self, method: &'static str, query: &[(&str, &str)]) -> Result<Answer> {
        let request = self
            .http
            .get(format!("{}{method}", self.base))
            .bearer_auth(self.bot.secret())
            .query(query);
        answer(method, request).await
    }
}

/// HTTP 429 and 5xx are errors to retry, a 429 with its `Retry-After`; an
/// answer Slack gave is returned for the caller to judge.
async fn answer(method: &'static str, request: RequestBuilder) -> Result<Answer> {
    let response = request
        .send()
        .await
        .with_context(|| format!("slack {method} failed"))?;
    let status = response.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        let seconds = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(1)
            .min(MAX_RETRY_AFTER);
        let wait = RetryAfter(Duration::from_secs(seconds));
        return Err(anyhow::Error::new(wait).context(format!("slack {method}")));
    }
    ensure!(status.is_success(), "slack {method}: HTTP {status}");
    let body: Value = response
        .json()
        .await
        .with_context(|| format!("slack {method}: the answer is not JSON"))?;
    if body["ok"].as_bool() == Some(true) {
        return Ok(Answer::Ok(body));
    }
    let code = body["error"].as_str().unwrap_or("unknown_error").to_owned();
    if matches!(
        code.as_str(),
        "invalid_auth"
            | "not_authed"
            | "token_revoked"
            | "token_expired"
            | "account_inactive"
            | "not_allowed_token_type"
            | "missing_scope"
    ) {
        return Err(TokenRefused { method, code }.into());
    }
    Ok(Answer::Refused(code))
}

/// The conversation can never take a message: it was deleted or archived,
/// the bot was removed, or the thread is locked.
fn gone(code: &str) -> bool {
    matches!(
        code,
        "channel_not_found"
            | "is_archived"
            | "not_in_channel"
            | "thread_not_found"
            | "cannot_reply_to_message"
            | "messages_tab_disabled"
            | "restricted_action_read_only_channel"
            | "restricted_action_thread_locked"
            | "restricted_action_non_threadable_channel"
    )
}
