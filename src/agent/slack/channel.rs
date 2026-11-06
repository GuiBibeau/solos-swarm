//! Replies through the Web API, in the thread of the Trigger they answer.
//! Slack does not deduplicate posts, so each carries its `MessageId` as
//! metadata, and a repeat looks for it in the thread before posting again.

use super::{
    ALLOW_SECRET_ACTION, APPROVE_ACTION, Answer, DENY_SECRET_ACTION, METADATA_EVENT, STOP_ACTION,
    SecretClick, SlackApi, gone, mrkdwn,
};
use crate::agent::links::TicketLinks;
use crate::agent::markdown::close_fences;
use crate::agent::ports::{BoxFuture, Channel, SendOptions, SendOutcome};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use solos_agent_core::model::{Ask, MessageId, MessageKind, OutboundMessage, TicketId};
use std::sync::Arc;

/// Acknowledgements and questions stay short; results are cut well under
/// Slack's 4,000-character guidance.
const SHORT_CHARS: usize = 500;
const RESULT_CHARS: usize = 3_000;
/// Bounds the search of one thread for an earlier post.
const MAX_PAGES: usize = 50;

pub struct SlackChannel {
    api: Arc<SlackApi>,
    links: TicketLinks,
}

enum Found {
    Posted,
    Missing,
    Gone,
}

impl SlackChannel {
    pub fn new(api: Arc<SlackApi>) -> Self {
        Self {
            api,
            links: TicketLinks::none(),
        }
    }

    /// Link the messages that carry one to their Ticket's page.
    pub fn with_links(mut self, links: TicketLinks) -> Self {
        self.links = links;
        self
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
        let body = body(&message, channel, thread, options.ask.as_ref(), &self.links);
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

fn body(
    message: &OutboundMessage,
    channel: &str,
    thread: Option<&str>,
    ask: Option<&Ask>,
    links: &TicketLinks,
) -> Value {
    let limit = match message.kind {
        MessageKind::Result => RESULT_CHARS,
        MessageKind::Acknowledgement | MessageKind::Attention => SHORT_CHARS,
    };
    let text = shape(&message.text, limit, links.page(message).as_deref());
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
    if let (Some(ask), Some(ticket)) = (ask, &message.ticket) {
        body["blocks"] = buttons(&text, ticket, ask);
    }
    body
}

fn buttons(text: &str, ticket: &TicketId, ask: &Ask) -> Value {
    let (value, offered) = match ask {
        Ask::Budget => (
            ticket.to_string(),
            [
                ("Approve", APPROVE_ACTION, "primary"),
                ("Stop", STOP_ACTION, "danger"),
            ],
        ),
        Ask::Secret { secret } => (
            serde_json::to_string(&SecretClick {
                ticket: ticket.clone(),
                secret: secret.clone(),
            })
            .expect("two strings serialize"),
            [
                ("Allow for this ticket", ALLOW_SECRET_ACTION, "primary"),
                ("Deny", DENY_SECRET_ACTION, "danger"),
            ],
        ),
    };
    let elements = offered.map(|(label, action, style)| {
        json!({
            "type": "button",
            "action_id": action,
            "style": style,
            "text": { "type": "plain_text", "text": label },
            "value": value,
        })
    });
    json!([
        { "type": "section", "text": { "type": "mrkdwn", "text": text } },
        { "type": "actions", "block_id": "solos_approval", "elements": elements },
    ])
}

/// Slack's mrkdwn for `text`, which cannot mention anyone or ping
/// `<!channel>` (see [`mrkdwn`]), cut to `limit` characters on a word with a
/// marker that says so. `page`, the Ticket's page, is linked after the text,
/// and where the rest is when the text is cut.
fn shape(text: &str, limit: usize, page: Option<&str>) -> String {
    let text = text.trim();
    let rendered = mrkdwn::render(text);
    let page = page.map(mrkdwn::escape);
    if rendered.chars().count() <= limit {
        return match page {
            Some(page) => format!("{rendered}\n\n<{page}|Task details>"),
            None => rendered,
        };
    }
    let cut: String = rendered.chars().take(limit).collect();
    // End on a whole word when one ends near the limit.
    let kept = match cut.rfind(char::is_whitespace) {
        Some(at) if at >= cut.len() * 4 / 5 => &cut[..at],
        _ => cut.as_str(),
    }
    .trim_end();
    let kept = whole_entities(kept);
    let shown = kept.chars().count();
    let kept = close_fences(kept);
    match page {
        Some(page) => format!("{kept}\n\n…the rest is on the <{page}|task page>"),
        None => {
            let total = text.chars().count();
            format!("{kept} …\n_[Cut short: {shown} of {total} characters shown.]_")
        }
    }
}

/// `text` without a link cut off in its middle, which Slack would show as
/// the address and a stray `<`. The only `<` in mrkdwn text begins one: the
/// rest is escaped.
fn whole_entities(text: &str) -> &str {
    match (text.rfind('<'), text.rfind('>')) {
        (Some(open), close) if close.is_none_or(|close| close < open) => text[..open].trim_end(),
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::{shape, whole_entities};

    #[test]
    fn short_text_is_only_escaped_and_long_text_is_cut_on_a_word_with_a_marker() {
        assert_eq!(
            shape("  <!channel> & <@U1>  ", 100, None),
            "&lt;!channel&gt; &amp; &lt;@U1&gt;"
        );
        let long = "word ".repeat(100);
        let shaped = shape(&long, 52, None);
        assert!(shaped.starts_with("word word"), "{shaped}");
        assert!(
            shaped.ends_with("…\n_[Cut short: 49 of 499 characters shown.]_"),
            "{shaped}"
        );
        let unbroken = "x".repeat(20);
        assert!(shape(&unbroken, 10, None).starts_with("xxxxxxxxxx …"));
    }

    #[test]
    fn a_page_is_linked_after_the_text_or_named_as_where_the_rest_is() {
        let page = "https://agent.example.com/tickets/tk_1";
        assert_eq!(
            shape("Done: **ok**.", 100, Some(page)),
            format!("Done: *ok*.\n\n<{page}|Task details>")
        );
        let shaped = shape(&"word ".repeat(100), 52, Some(page));
        assert!(shaped.starts_with("word word"), "{shaped}");
        assert!(
            shaped.ends_with(&format!("\n\n…the rest is on the <{page}|task page>")),
            "{shaped}"
        );
        assert!(!shaped.contains("Cut short") && !shaped.contains("Task details"));
    }

    #[test]
    fn a_cut_leaves_no_link_or_code_block_open() {
        let text = format!(
            "{} [the long label of a link](https://e.example/x) after",
            "w ".repeat(10)
        );
        // The limit falls inside the link.
        let shaped = shape(&text, 35, None);
        assert!(!shaped.contains('<'), "{shaped}");
        assert_eq!(whole_entities("a <https://e.example|lab"), "a");
        assert_eq!(
            whole_entities("a <https://e.example|label> b"),
            "a <https://e.example|label> b"
        );
        let code = format!("```\n{}", "let x = 1;\n".repeat(50));
        let shaped = shape(&code, 60, None);
        assert!(shaped.contains("let x = 1;\n```"), "{shaped}");
    }

    #[test]
    fn markdown_tables_and_headings_are_text_in_the_post() {
        let shaped = shape(
            "## Open\n\n| PR | State |\n|---|---|\n| #1 | open |",
            3_000,
            None,
        );
        assert_eq!(shaped, "*Open*\n\n```\nPR  State\n--  -----\n#1  open\n```");
    }
}
