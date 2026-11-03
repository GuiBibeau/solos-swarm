use super::{APPROVE_ACTION, DiscordApi, Reply, STOP_ACTION, valid_id};
use crate::agent::ports::{BoxFuture, Channel, SendOptions, SendOutcome};
use anyhow::Result;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solos_agent_core::model::{MessageId, MessageKind, OutboundMessage, TicketId};
use std::fmt::Write;
use std::sync::Arc;

/// Acknowledgements and questions stay short; a result takes all the room
/// Discord's 2,000-character limit leaves.
const SHORT_CHARS: usize = 500;
const RESULT_CHARS: usize = 2_000;
const MARKER_CHARS: usize = 64;
/// Discord accepts a nonce of at most 25 characters.
const NONCE_CHARS: usize = 24;
const SUPPRESS_EMBEDS: u32 = 1 << 2;
const BUTTON: u8 = 2;
const PRIMARY_ROW: u8 = 1;
const SUCCESS_STYLE: u8 = 3;
const DANGER_STYLE: u8 = 4;

pub struct DiscordChannel {
    api: Arc<DiscordApi>,
}

impl DiscordChannel {
    pub fn new(api: Arc<DiscordApi>) -> Self {
        Self { api }
    }

    async fn post(&self, message: OutboundMessage, options: SendOptions) -> Result<SendOutcome> {
        let address = &message.channel.address;
        let Some(channel) = address.get("channel").filter(|id| valid_id(id)) else {
            eprintln!("agent: discord: message {} names no channel", message.id);
            return Ok(SendOutcome::Gone);
        };
        let reference = address.get("message_id").filter(|id| valid_id(id));
        let body = body(&message, reference.map(String::as_str), options.approval);
        match self.api.create_message(channel, &body).await? {
            Reply::Ok(_) => Ok(SendOutcome::Sent),
            Reply::Refused(refusal) => {
                eprintln!("agent: discord: post to channel {channel}: {refusal}");
                Ok(SendOutcome::Gone)
            }
        }
    }
}

impl Channel for DiscordChannel {
    fn send(
        &self,
        message: OutboundMessage,
        options: SendOptions,
    ) -> BoxFuture<'_, Result<SendOutcome>> {
        Box::pin(self.post(message, options))
    }
}

fn body(message: &OutboundMessage, reference: Option<&str>, approval: bool) -> Value {
    let limit = match message.kind {
        MessageKind::Result => RESULT_CHARS,
        MessageKind::Acknowledgement | MessageKind::Attention => SHORT_CHARS,
    };
    let mut body = json!({
        "content": shape(&message.text, limit),
        "nonce": nonce(&message.id),
        "enforce_nonce": true,
        "flags": SUPPRESS_EMBEDS,
        "allowed_mentions": { "parse": [] },
    });
    if let Some(reference) = reference {
        body["message_reference"] = json!({ "message_id": reference, "fail_if_not_exists": false });
    }
    if approval && let Some(ticket) = &message.ticket {
        body["components"] = buttons(ticket);
    }
    body
}

/// Each click arrives on the gateway as an interaction whose `custom_id`
/// carries the Ticket id.
fn buttons(ticket: &TicketId) -> Value {
    let button = |label: &str, style: u8, action: &str| {
        json!({
            "type": BUTTON,
            "style": style,
            "label": label,
            "custom_id": format!("{action}:{ticket}"),
        })
    };
    json!([{
        "type": PRIMARY_ROW,
        "components": [
            button("Approve", SUCCESS_STYLE, APPROVE_ACTION),
            button("Stop", DANGER_STYLE, STOP_ACTION),
        ],
    }])
}

fn nonce(id: &MessageId) -> String {
    let digest = Sha256::digest(id.as_str().as_bytes());
    let mut out = String::with_capacity(NONCE_CHARS);
    for byte in &digest[..NONCE_CHARS / 2] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Mentions in the text cannot notify anyone: the request's `allowed_mentions`
/// parses none.
fn shape(text: &str, limit: usize) -> String {
    let text = text.trim();
    let total = text.chars().count();
    if total <= limit {
        return if text.is_empty() {
            "(no text)".to_owned()
        } else {
            text.to_owned()
        };
    }
    let cut: String = text.chars().take(limit - MARKER_CHARS).collect();
    let kept = match cut.rfind(char::is_whitespace) {
        Some(at) if at >= cut.len() * 4 / 5 => &cut[..at],
        _ => cut.as_str(),
    }
    .trim_end();
    let shown = kept.chars().count();
    format!("{kept} …\n_[Cut short: {shown} of {total} characters shown.]_")
}

#[cfg(test)]
mod tests {
    use super::{RESULT_CHARS, nonce, shape};
    use solos_agent_core::model::MessageId;

    #[test]
    fn text_over_the_limit_is_cut_on_a_word_with_a_marker_inside_the_limit() {
        assert_eq!(shape("  <@1> & @everyone  ", 100), "<@1> & @everyone");
        assert_eq!(shape("   ", 100), "(no text)");
        let long = "word ".repeat(1_000);
        let shaped = shape(&long, RESULT_CHARS);
        assert!(shaped.starts_with("word word"), "{shaped}");
        assert!(
            shaped.ends_with("…\n_[Cut short: 1934 of 4999 characters shown.]_"),
            "{shaped}"
        );
        assert!(shaped.chars().count() <= RESULT_CHARS);
        let unbroken = "x".repeat(2_500);
        let shaped = shape(&unbroken, RESULT_CHARS);
        assert!(shaped.chars().count() <= RESULT_CHARS, "{}", shaped.len());
        assert!(shaped.starts_with("xxxxxxxxxx"));
        let wide = "é".repeat(2_500);
        assert!(shape(&wide, RESULT_CHARS).chars().count() <= RESULT_CHARS);
    }

    #[test]
    fn a_nonce_is_short_stable_and_one_per_message() {
        let first = nonce(&MessageId::new("ms_0123456789abcdef"));
        assert_eq!(first.len(), 24);
        assert_eq!(first, nonce(&MessageId::new("ms_0123456789abcdef")));
        assert_ne!(first, nonce(&MessageId::new("ms_0123456789abcdee")));
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
