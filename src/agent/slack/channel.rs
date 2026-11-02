//! Replies through the Web API, in the thread of the Trigger they answer.
//! Slack does not deduplicate posts, so each carries its `MessageId` as
//! metadata, and a repeat looks for it in the thread before posting again.

use super::{APPROVE_ACTION, Answer, METADATA_EVENT, STOP_ACTION, SlackApi, gone};
use crate::agent::ports::{BoxFuture, Channel, SendOptions, SendOutcome};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use solos_agent_core::model::{MessageId, MessageKind, OutboundMessage, TicketId};
use std::sync::Arc;

/// Acknowledgements and questions stay short; results are cut well under
/// Slack's 4,000-character guidance.
const SHORT_CHARS: usize = 500;
const RESULT_CHARS: usize = 3_000;
/// Bounds the search of one thread for an earlier post.
const MAX_PAGES: usize = 50;

pub struct SlackChannel {
    api: Arc<SlackApi>,
}

enum Found {
    Posted,
    Missing,
    Gone,
}

impl SlackChannel {
    pub fn new(api: Arc<SlackApi>) -> Self {
        Self { api }
    }

    async fn post(&self, message: OutboundMessage, options: SendOptions) -> Result<SendOutcome> {
        let address = &message.channel.address;
        let Some(channel) = address.get("channel") else {
            eprintln!("agent: slack: message {} names no channel", message.id);
            return Ok(SendOutcome::Gone);
        };
        let thread = address.get("thread_ts").map(String::as_str);
        if options.repeat
            && let Some(thread) = thread
        {
            match self.find(channel, thread, &message.id).await? {
                Found::Posted => return Ok(SendOutcome::Sent),
                Found::Gone => return Ok(SendOutcome::Gone),
                Found::Missing => {}
            }
        }
        let body = body(&message, channel, thread, options.approval);
        match self.api.post("chat.postMessage", None, Some(&body)).await? {
            Answer::Ok(_) => Ok(SendOutcome::Sent),
            Answer::Refused(code) if gone(&code) => {
                eprintln!("agent: slack: chat.postMessage to {channel}: {code}");
                Ok(SendOutcome::Gone)
            }
            Answer::Refused(code) => bail!("slack chat.postMessage: {code}"),
        }
    }

    /// Whether the thread already holds the message `id`.
    async fn find(&self, channel: &str, thread: &str, id: &MessageId) -> Result<Found> {
        let mut cursor = String::new();
        for _ in 0..MAX_PAGES {
            let mut query = vec![
                ("channel", channel),
                ("ts", thread),
                ("include_all_metadata", "true"),
                ("limit", "200"),
            ];
            if !cursor.is_empty() {
                query.push(("cursor", cursor.as_str()));
            }
            let page = match self.api.get("conversations.replies", &query).await? {
                Answer::Ok(page) => page,
                Answer::Refused(code) if gone(&code) => {
                    eprintln!("agent: slack: conversations.replies in {channel}: {code}");
                    return Ok(Found::Gone);
                }
                Answer::Refused(code) => bail!("slack conversations.replies: {code}"),
            };
            let messages = page["messages"].as_array().map(Vec::as_slice);
            let posted = messages.unwrap_or_default().iter().any(|m| {
                m["metadata"]["event_type"] == METADATA_EVENT
                    && m["metadata"]["event_payload"]["message_id"] == id.as_str()
            });
            if posted {
                return Ok(Found::Posted);
            }
            cursor = page["response_metadata"]["next_cursor"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if cursor.is_empty() {
                return Ok(Found::Missing);
            }
        }
        eprintln!("agent: slack: thread {thread} in {channel} is too long to search; posting");
        Ok(Found::Missing)
    }
}

impl Channel for SlackChannel {
    fn send(
        &self,
        message: OutboundMessage,
        options: SendOptions,
    ) -> BoxFuture<'_, Result<SendOutcome>> {
        Box::pin(self.post(message, options))
    }
}

fn body(message: &OutboundMessage, channel: &str, thread: Option<&str>, approval: bool) -> Value {
    let limit = match message.kind {
        MessageKind::Result => RESULT_CHARS,
        MessageKind::Acknowledgement | MessageKind::Attention => SHORT_CHARS,
    };
    let text = shape(&message.text, limit);
    let mut body = json!({
        "channel": channel,
        "text": text,
        "unfurl_links": false,
        "unfurl_media": false,
        "metadata": {
            "event_type": METADATA_EVENT,
            "event_payload": { "message_id": message.id },
        },
    });
    if let Some(thread) = thread {
        body["thread_ts"] = json!(thread);
    }
    if approval && let Some(ticket) = &message.ticket {
        body["blocks"] = buttons(&text, ticket);
    }
    body
}

/// The question, then Approve and Stop. Each click arrives on the socket as
/// a `block_actions` envelope carrying the Ticket id.
fn buttons(text: &str, ticket: &TicketId) -> Value {
    let button = |label: &str, action: &str, style: &str| {
        json!({
            "type": "button",
            "action_id": action,
            "style": style,
            "text": { "type": "plain_text", "text": label },
            "value": ticket,
        })
    };
    json!([
        { "type": "section", "text": { "type": "mrkdwn", "text": text } },
        {
            "type": "actions",
            "block_id": "solos_approval",
            "elements": [
                button("Approve", APPROVE_ACTION, "primary"),
                button("Stop", STOP_ACTION, "danger"),
            ],
        },
    ])
}

/// Escape for mrkdwn, so text from a worker cannot mention anyone or ping
/// `<!channel>`, and cut it to `limit` characters with a marker that says so.
fn shape(text: &str, limit: usize) -> String {
    let text = text.trim();
    let total = text.chars().count();
    if total <= limit {
        return escape(text);
    }
    let cut: String = text.chars().take(limit).collect();
    // End on a whole word when one ends near the limit.
    let kept = match cut.rfind(char::is_whitespace) {
        Some(at) if at >= cut.len() * 4 / 5 => &cut[..at],
        _ => cut.as_str(),
    }
    .trim_end();
    let shown = kept.chars().count();
    format!(
        "{} …\n_[Cut short: {shown} of {total} characters shown.]_",
        escape(kept)
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::shape;

    #[test]
    fn short_text_is_only_escaped_and_long_text_is_cut_on_a_word_with_a_marker() {
        assert_eq!(
            shape("  <!channel> & <@U1>  ", 100),
            "&lt;!channel&gt; &amp; &lt;@U1&gt;"
        );
        let long = "word ".repeat(100);
        let shaped = shape(&long, 52);
        assert!(shaped.starts_with("word word"), "{shaped}");
        assert!(
            shaped.ends_with("…\n_[Cut short: 49 of 499 characters shown.]_"),
            "{shaped}"
        );
        let unbroken = "x".repeat(20);
        assert!(shape(&unbroken, 10).starts_with("xxxxxxxxxx …"));
    }
}
