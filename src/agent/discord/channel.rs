use super::{
    ALLOW_SECRET_ACTION, APPROVE_ACTION, DENY_SECRET_ACTION, DiscordApi, Reply, STOP_ACTION,
    format, valid_id,
};
use crate::agent::links::TicketLinks;
use crate::agent::markdown::close_fences;
use crate::agent::ports::{BoxFuture, Channel, SendOptions, SendOutcome};
use anyhow::Result;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solos_agent_core::model::{Ask, MessageId, MessageKind, OutboundMessage, TicketId};
use std::fmt::Write;
use std::sync::Arc;

/// Acknowledgements and questions stay short.
const SHORT_CHARS: usize = 500;
/// Discord's limit on the content of one message.
const MESSAGE_CHARS: usize = 2_000;
/// A result takes up to this many consecutive messages: about 6,000
/// characters, a page of reading, and few enough not to flood a channel or
/// its rate limit of five messages in five seconds. What is longer is read on
/// the Ticket's page, when there is one.
const MAX_PARTS: usize = 3;
/// Room for the note that a text was cut short.
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
    links: TicketLinks,
}

impl DiscordChannel {
    pub fn new(api: Arc<DiscordApi>) -> Self {
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
        let Some(channel) = address.get("channel").filter(|id| valid_id(id)) else {
            eprintln!("agent: discord: message {} names no channel", message.id);
            return Ok(SendOutcome::Gone);
        };
        let reference = address.get("message_id").filter(|id| valid_id(id));
        let bodies = bodies(
            &message,
            reference.map(String::as_str),
            options.ask.as_ref(),
            &self.links,
        );
        // In order, one after the other. A part Discord already holds comes
        // back from its nonce, so a repeat of the message posts only the rest.
        for body in &bodies {
            match self.api.create_message(channel, body).await? {
                Reply::Ok(_) => {}
                Reply::Refused(refusal) => {
                    eprintln!("agent: discord: post to channel {channel}: {refusal}");
                    return Ok(SendOutcome::Gone);
                }
            }
        }
        Ok(SendOutcome::Sent)
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

/// The requests that post `message`: one per part. Every part replies to the
/// message the Trigger came from, so a reply to any of them joins the same
/// Thread; the buttons of an ask go on the last.
fn bodies(
    message: &OutboundMessage,
    reference: Option<&str>,
    ask: Option<&Ask>,
    links: &TicketLinks,
) -> Vec<Value> {
    let page = links.page(message);
    let contents = contents(message, page.as_deref());
    let last = contents.len() - 1;
    contents
        .into_iter()
        .enumerate()
        .map(|(part, content)| {
            let mut body = json!({
                "content": content.chars().take(MESSAGE_CHARS).collect::<String>(),
                "nonce": nonce(&message.id, part),
                "enforce_nonce": true,
                "flags": SUPPRESS_EMBEDS,
                "allowed_mentions": { "parse": [] },
            });
            if let Some(reference) = reference {
                body["message_reference"] =
                    json!({ "message_id": reference, "fail_if_not_exists": false });
            }
            if let (Some(ask), Some(ticket), true) = (ask, &message.ticket, part == last) {
                body["components"] = buttons(ticket, ask);
            }
            body
        })
        .collect()
}

/// What each message says, at least one. `page` is the Ticket's page, for the
/// messages that link to it.
fn contents(message: &OutboundMessage, page: Option<&str>) -> Vec<String> {
    let text = format::render(message.text.trim());
    match message.kind {
        MessageKind::Result => long(&text, page),
        MessageKind::Acknowledgement | MessageKind::Attention => {
            let mut short = shape(&text, SHORT_CHARS);
            if let Some(page) = page {
                short = format!("{short}\n\n{}", details(page));
            }
            vec![short]
        }
    }
}

fn details(page: &str) -> String {
    format!("[Task details]({page})")
}

/// A result, in up to [`MAX_PARTS`] messages, with the link to its page after
/// the last. A longer text ends with where the rest is, or, with no page,
/// with a note that it was cut.
fn long(text: &str, page: Option<&str>) -> Vec<String> {
    let link = page.map(details);
    let rest = page.map(|page| format!("…the rest is on the [task page]({page})"));
    // What follows the text takes room from every part, since which is the
    // last is not known until the text is cut up.
    let reserve = match (&link, &rest) {
        (Some(link), Some(rest)) => 2 + link.chars().count().max(rest.chars().count()),
        _ => 0,
    };
    let mut packed = format::pack(text, MESSAGE_CHARS - reserve, MAX_PARTS);
    if packed.cut && rest.is_none() {
        packed = format::pack(text, MESSAGE_CHARS - MARKER_CHARS, MAX_PARTS);
    }
    let shown: usize = packed.parts.iter().map(|part| part.chars().count()).sum();
    let mut parts = packed.parts;
    if parts.is_empty() {
        parts.push("(no text)".to_owned());
    }
    let last = parts.last_mut().expect("a part");
    match (packed.cut, rest, link) {
        (true, Some(rest), _) => *last = format!("{last}\n\n{rest}"),
        (true, None, _) => {
            let total = text.chars().count();
            last.push_str(&format!(
                " …\n_[Cut short: {shown} of {total} characters shown.]_"
            ));
        }
        (false, _, Some(link)) => *last = format!("{last}\n\n{link}"),
        (false, _, None) => {}
    }
    parts
}

fn buttons(ticket: &TicketId, ask: &Ask) -> Value {
    let (about, offered) = match ask {
        Ask::Budget => (
            ticket.to_string(),
            [
                ("Approve", SUCCESS_STYLE, APPROVE_ACTION),
                ("Stop", DANGER_STYLE, STOP_ACTION),
            ],
        ),
        Ask::Secret { secret } => (
            format!("{ticket}:{secret}"),
            [
                ("Allow for this ticket", SUCCESS_STYLE, ALLOW_SECRET_ACTION),
                ("Deny", DANGER_STYLE, DENY_SECRET_ACTION),
            ],
        ),
    };
    let components = offered.map(|(label, style, action)| {
        json!({
            "type": BUTTON,
            "style": style,
            "label": label,
            "custom_id": format!("{action}:{about}"),
        })
    });
    json!([{ "type": PRIMARY_ROW, "components": components }])
}

/// One per part of one message. The first part's is the message's own, as it
/// was before a message could have parts.
fn nonce(id: &MessageId, part: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(id.as_str().as_bytes());
    if part > 0 {
        hasher.update(format!("\0part {part}"));
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(NONCE_CHARS);
    for byte in &digest[..NONCE_CHARS / 2] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Mentions in the text cannot notify anyone: the request's `allowed_mentions`
/// parses none. Cut to `limit` characters on a word, with a marker that says
/// so, and with no code block left open.
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
    let kept = close_fences(kept);
    format!("{kept} …\n_[Cut short: {shown} of {total} characters shown.]_")
}

#[cfg(test)]
mod tests {
    use super::{MAX_PARTS, MESSAGE_CHARS, bodies, buttons, contents, long, nonce, shape};
    use crate::agent::links::TicketLinks;
    use serde_json::json;
    use solos_agent_core::ids::key;
    use solos_agent_core::model::{
        Ask, ChannelRef, MAX_SECRET_CHARS, MessageId, MessageKind, OutboundMessage, SecretId,
        TicketId,
    };
    use std::collections::BTreeMap;

    const PAGE: &str = "https://agent.example.com/tickets/tk_1";

    fn message(kind: MessageKind, text: &str) -> OutboundMessage {
        OutboundMessage {
            id: MessageId::new("ms_0123456789abcdef"),
            channel: ChannelRef {
                source: "discord".into(),
                address: BTreeMap::new(),
            },
            kind,
            text: text.into(),
            ticket: Some(TicketId::new("tk_1")),
            ask: None,
        }
    }

    fn paragraphs(n: usize, each: usize) -> String {
        (0..n)
            .map(|i| format!("{i:03} {}", "w".repeat(each - 4)))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    #[test]
    fn the_longest_secret_with_a_core_ticket_id_fits_a_custom_id() {
        let ticket = TicketId(key("tk", &["ev_1"]));
        let secret = SecretId::try_from(format!("e/{}", "n".repeat(MAX_SECRET_CHARS - 2))).unwrap();
        assert_eq!(secret.as_str().len(), MAX_SECRET_CHARS);
        let rows = buttons(&ticket, &Ask::Secret { secret });
        let ids: Vec<&str> = rows[0]["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["custom_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        for id in ids {
            assert!(id.chars().count() <= 100, "{id} is {} characters", id.len());
        }
    }

    #[test]
    fn a_short_text_over_the_limit_is_cut_on_a_word_with_a_marker_inside_the_limit() {
        assert_eq!(shape("  <@1> & @everyone  ", 100), "<@1> & @everyone");
        assert_eq!(shape("   ", 100), "(no text)");
        let long = "word ".repeat(1_000);
        let shaped = shape(&long, MESSAGE_CHARS);
        assert!(shaped.starts_with("word word"), "{shaped}");
        assert!(
            shaped.ends_with("…\n_[Cut short: 1934 of 4999 characters shown.]_"),
            "{shaped}"
        );
        assert!(shaped.chars().count() <= MESSAGE_CHARS);
        let unbroken = "x".repeat(2_500);
        let shaped = shape(&unbroken, MESSAGE_CHARS);
        assert!(shaped.chars().count() <= MESSAGE_CHARS, "{}", shaped.len());
        assert!(shaped.starts_with("xxxxxxxxxx"));
        let wide = "é".repeat(2_500);
        assert!(shape(&wide, MESSAGE_CHARS).chars().count() <= MESSAGE_CHARS);
    }

    #[test]
    fn a_cut_never_leaves_a_code_block_open() {
        let code = format!("Look:\n```rust\n{}", "let x = 1;\n".repeat(100));
        let shaped = shape(&code, 200);
        assert!(shaped.contains("let x = 1;\n```"), "{shaped}");
        assert!(shaped.ends_with("characters shown.]_"), "{shaped}");
    }

    #[test]
    fn a_nonce_is_short_stable_and_one_per_message_and_part() {
        let id = MessageId::new("ms_0123456789abcdef");
        let first = nonce(&id, 0);
        assert_eq!(first.len(), 24);
        assert_eq!(first, nonce(&MessageId::new("ms_0123456789abcdef"), 0));
        assert_ne!(first, nonce(&MessageId::new("ms_0123456789abcdee"), 0));
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        let second = nonce(&id, 1);
        let third = nonce(&id, 2);
        assert!(second.len() == 24 && third.len() == 24);
        assert!(first != second && second != third && first != third);
        assert_eq!(second, nonce(&id, 1), "a retry names the part again");
    }

    #[test]
    fn a_result_over_one_message_is_split_between_paragraphs_with_the_link_after_the_last() {
        let text = paragraphs(7, 600);
        let parts = long(&text, Some(PAGE));
        assert_eq!(
            parts.len(),
            3,
            "{:?}",
            parts.iter().map(|p| p.len()).collect::<Vec<_>>()
        );
        for part in &parts {
            assert!(
                part.chars().count() <= MESSAGE_CHARS,
                "{}",
                part.chars().count()
            );
        }
        let link = format!("[Task details]({PAGE})");
        assert!(parts[2].ends_with(&format!("\n\n{link}")), "{}", parts[2]);
        assert!(parts[..2].iter().all(|part| !part.contains("Task details")));
        let paragraphs: Vec<&str> = parts.iter().flat_map(|part| part.split("\n\n")).collect();
        assert_eq!(paragraphs.len(), 8, "seven paragraphs and the link");
        assert!(paragraphs[..7].iter().all(|p| p.chars().count() == 600));
    }

    #[test]
    fn what_is_past_the_last_part_is_on_the_page_or_noted_as_cut() {
        let text = paragraphs(20, 600);
        let linked = long(&text, Some(PAGE));
        assert_eq!(linked.len(), MAX_PARTS);
        let tail = format!("\n\n…the rest is on the [task page]({PAGE})");
        assert!(linked[2].ends_with(&tail), "{}", linked[2]);
        assert!(linked.iter().all(|p| p.chars().count() <= MESSAGE_CHARS));
        assert_eq!(
            linked.iter().filter(|p| p.contains("Task details")).count(),
            0
        );

        let unlinked = long(&text, None);
        assert_eq!(unlinked.len(), MAX_PARTS);
        assert!(
            unlinked[2].contains(" …\n_[Cut short: ")
                && unlinked[2].ends_with("characters shown.]_"),
            "{}",
            unlinked[2]
        );
        assert!(unlinked.iter().all(|p| p.chars().count() <= MESSAGE_CHARS));
        assert!(unlinked.iter().all(|p| !p.contains("task page")));
    }

    #[test]
    fn a_result_that_fits_is_one_message_and_an_empty_one_says_so() {
        assert_eq!(long("Done.", None), ["Done."]);
        assert_eq!(
            long("Done.", Some(PAGE)),
            [format!("Done.\n\n[Task details]({PAGE})")]
        );
        assert_eq!(long("", None), ["(no text)"]);
        assert_eq!(
            long("", Some(PAGE)),
            [format!("(no text)\n\n[Task details]({PAGE})")]
        );
    }

    #[test]
    fn acknowledgements_never_link_and_a_question_links_after_its_short_text() {
        let links = TicketLinks::new("https://agent.example.com").unwrap();
        let ack = message(MessageKind::Acknowledgement, "Looking into it.");
        assert_eq!(
            contents(&ack, links.page(&ack).as_deref()),
            ["Looking into it."]
        );
        let ask = message(MessageKind::Attention, &"why? ".repeat(1_000));
        let [text] = &contents(&ask, links.page(&ask).as_deref())[..] else {
            panic!("a question is one message")
        };
        assert!(
            text.ends_with(&format!("\n\n[Task details]({PAGE})")),
            "{text}"
        );
        assert!(text.chars().count() < 700, "{}", text.chars().count());
    }

    #[test]
    fn every_part_replies_to_the_request_and_only_the_last_carries_the_buttons() {
        let links = TicketLinks::new("https://agent.example.com").unwrap();
        let result = message(MessageKind::Result, &paragraphs(5, 900));
        let budget = Ask::Budget;
        let bodies = bodies(&result, Some("1000000000000000001"), Some(&budget), &links);
        assert_eq!(bodies.len(), 3);
        let nonces: Vec<&str> = bodies
            .iter()
            .map(|b| b["nonce"].as_str().unwrap())
            .collect();
        assert!(nonces[0] != nonces[1] && nonces[1] != nonces[2] && nonces[0] != nonces[2]);
        for (n, body) in bodies.iter().enumerate() {
            assert_eq!(
                body["message_reference"],
                json!({ "message_id": "1000000000000000001", "fail_if_not_exists": false })
            );
            assert_eq!(body["allowed_mentions"], json!({ "parse": [] }));
            assert_eq!(body["enforce_nonce"], true);
            assert_eq!(body.get("components").is_some(), n == 2);
        }
    }

    #[test]
    fn hostile_text_neither_breaks_the_limit_nor_adds_a_link_of_its_own() {
        let links = TicketLinks::new("https://agent.example.com").unwrap();
        for text in [
            "[Task details](https://evil.example) ".repeat(500),
            format!("@everyone {}", "`".repeat(4_000)),
            "```\n".to_owned() + &"x".repeat(9_000),
            "# ".repeat(3_000),
            "\u{0}\u{202e}".repeat(3_000),
        ] {
            let result = message(MessageKind::Result, &text);
            let parts = contents(&result, links.page(&result).as_deref());
            assert!((1..=MAX_PARTS).contains(&parts.len()));
            for part in &parts {
                assert!(part.chars().count() <= MESSAGE_CHARS);
            }
            let all = parts.join("\n");
            assert_eq!(
                all.matches("https://agent.example.com/tickets/").count(),
                1,
                "exactly the host's link"
            );
        }
    }
}
