//! The `mcp` Source and Channel: two files in the state directory, which an
//! operator's MCP host (`solos-swarm mcp --state-dir`) writes requests into
//! and reads replies from. A request is a Trigger of source `mcp` whose
//! actor is the operator, so the Policy names it `mcp:<operator>` and admits
//! it, or not, as it would a Slack person.

use super::jsonl::JsonlChannel;
use super::links::TicketLinks;
use super::ports::{BoxFuture, Channel, SendOptions, SendOutcome};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use solos_agent_core::model::{
    Actor, Addressed, ChannelRef, MessageId, MessageKind, Ms, OutboundMessage, ThreadKey, TicketId,
    Trigger, TriggerId,
};
use std::collections::BTreeMap;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The Triggers' source, and the source the `mcp` channel serves.
pub const SOURCE: &str = "mcp";
const DIR: &str = "mcp";
const INBOX: &str = "inbox.jsonl";
const OUTBOX: &str = "outbox.jsonl";
/// The longest operator or thread name.
const NAME_CHARS: usize = 64;
pub const REQUEST_CHARS: usize = 16_000;
/// How many of a thread's latest replies `replies` returns.
const RECENT_REPLIES: usize = 20;

pub fn inbox(state_dir: &Path) -> PathBuf {
    state_dir.join(DIR).join(INBOX)
}

pub fn outbox(state_dir: &Path) -> PathBuf {
    state_dir.join(DIR).join(OUTBOX)
}

/// Create the directory both files live in, private like the state
/// directory.
pub fn open_dir(state_dir: &Path) -> Result<()> {
    let dir = state_dir.join(DIR);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("cannot create {}", dir.display()))
}

/// The inbox `agent serve` reads, created if missing. Only `agent serve`
/// creates it, so a request filed where no agent reads them is refused
/// instead of waiting forever.
pub fn open_inbox(state_dir: &Path) -> Result<PathBuf> {
    open_dir(state_dir)?;
    let path = inbox(state_dir);
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    Ok(path)
}

/// An operator or thread name: letters, digits, `.`, `_`, `-` and `@`, so
/// `mcp:<operator>` and the Thread key read unambiguously.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= NAME_CHARS
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@'))
}

pub fn thread_key(operator: &str, thread: &str) -> ThreadKey {
    ThreadKey::new(format!("{SOURCE}:{operator}:{thread}"))
}

pub fn reply_to(operator: &str, thread: &str) -> ChannelRef {
    ChannelRef {
        source: SOURCE.into(),
        address: BTreeMap::from([
            ("operator".to_owned(), operator.to_owned()),
            ("thread".to_owned(), thread.to_owned()),
        ]),
    }
}

/// The `mcp` Channel: the outbox, with the Ticket's page named after the text
/// of a reply that carries a link. The text is otherwise as it was written;
/// the operator's MCP host shows it as it is.
pub struct McpChannel {
    outbox: JsonlChannel,
    links: TicketLinks,
}

impl McpChannel {
    pub fn new(outbox: JsonlChannel, links: TicketLinks) -> Self {
        Self { outbox, links }
    }
}

impl Channel for McpChannel {
    fn send(
        &self,
        mut message: OutboundMessage,
        options: SendOptions,
    ) -> BoxFuture<'_, Result<SendOutcome>> {
        if let Some(page) = self.links.page(&message) {
            message.text = format!("{}\n\nDetails: {page}", message.text.trim_end());
        }
        self.outbox.send(message, options)
    }
}

/// A request in the inbox.
#[derive(Debug, Serialize)]
pub struct Filed {
    pub trigger: TriggerId,
    pub thread: String,
}

/// Append `text` from `operator` to the inbox, on `thread` or on a new one.
pub fn file(
    state_dir: &Path,
    operator: &str,
    thread: Option<&str>,
    text: &str,
    now: Ms,
) -> Result<Filed> {
    ensure!(valid_name(operator), "operator {operator:?} is not a name");
    let text = text.trim();
    ensure!(!text.is_empty(), "a request needs a text");
    ensure!(
        text.chars().count() <= REQUEST_CHARS,
        "a request is at most {REQUEST_CHARS} characters"
    );
    let id = uuid::Uuid::new_v4().simple().to_string();
    let thread = match thread {
        Some(thread) => {
            ensure!(
                valid_name(thread),
                "thread {thread:?} may hold only letters, digits, ., _, - and @, at most \
                 {NAME_CHARS} of them"
            );
            thread.to_owned()
        }
        None => format!("t-{}", &id[..12]),
    };
    let trigger = Trigger {
        id: TriggerId::new(format!("{SOURCE}:{operator}:{id}")),
        source: SOURCE.into(),
        at_ms: now,
        actor: Actor::person(operator),
        addressed: Addressed::Direct,
        text: text.to_owned(),
        threads: vec![thread_key(operator, &thread)],
        reply_to: vec![reply_to(operator, &thread)],
        meta: serde_json::Value::Null,
    };
    let path = inbox(state_dir);
    let mut inbox = match std::fs::OpenOptions::new().append(true).open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => anyhow::bail!(
            "no agent reads MCP requests in {}: its agent.yaml needs `sources: [{{kind: mcp}}]` \
             and `channels: [{{kind: mcp}}]`, and `agent serve` creates {} when it starts",
            state_dir.display(),
            path.display()
        ),
        Err(err) => {
            return Err(err).with_context(|| format!("cannot open {}", path.display()));
        }
    };
    let mut line = serde_json::to_vec(&trigger)?;
    line.push(b'\n');
    // The whole line in one append, so lines from hosts filing at once stay whole.
    inbox
        .write_all(&line)
        .and_then(|()| inbox.sync_data())
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(Filed {
        trigger: trigger.id,
        thread,
    })
}

#[derive(Debug, Serialize)]
pub struct Reply {
    pub id: MessageId,
    pub kind: MessageKind,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<TicketId>,
}

#[derive(Debug, Serialize)]
pub struct Replies {
    pub thread: String,
    /// The latest, oldest first.
    pub replies: Vec<Reply>,
    /// How many came before them.
    pub earlier: usize,
}

/// What the agent sent `operator` on `thread`, from the outbox.
pub fn replies(state_dir: &Path, operator: &str, thread: &str) -> Result<Replies> {
    ensure!(valid_name(thread), "thread {thread:?} is not a name");
    let path = outbox(state_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    let wanted = reply_to(operator, thread);
    let mut all: Vec<Reply> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<OutboundMessage>(line).ok())
        .filter(|message| message.channel == wanted)
        .map(|message| Reply {
            id: message.id,
            kind: message.kind,
            text: message.text,
            ticket: message.ticket,
        })
        .collect();
    let earlier = all.len().saturating_sub(RECENT_REPLIES);
    Ok(Replies {
        thread: thread.to_owned(),
        replies: all.split_off(earlier),
        earlier,
    })
}

#[cfg(test)]
mod tests {
    use super::{McpChannel, file, open_inbox, replies, reply_to, valid_name};
    use crate::agent::jsonl::JsonlChannel;
    use crate::agent::links::TicketLinks;
    use crate::agent::ports::{Channel, SendOptions};
    use solos_agent_core::model::{MessageId, MessageKind, OutboundMessage, TicketId};

    #[test]
    fn names_are_short_and_plain() {
        for name in ["alice", "a.b_c-d@e", "T1"] {
            assert!(valid_name(name), "{name}");
        }
        for name in ["", "a b", "a:b", "a/b", "é", &"x".repeat(65)] {
            assert!(!valid_name(name), "{name}");
        }
    }

    #[test]
    fn a_request_needs_an_inbox_a_text_and_a_plain_thread() {
        let dir = std::env::temp_dir().join(format!("agent-mcp-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            file(&dir, "alice", None, "hi", 1)
                .unwrap_err()
                .to_string()
                .contains("kind: mcp"),
            "no agent created the inbox"
        );
        open_inbox(&dir).unwrap();
        assert!(file(&dir, "alice", None, "  ", 1).is_err());
        assert!(file(&dir, "alice", Some("a b"), "hi", 1).is_err());
        assert!(file(&dir, "a:b", None, "hi", 1).is_err());
        let filed = file(&dir, "alice", None, "hi", 1).unwrap();
        assert!(filed.thread.starts_with("t-"), "{}", filed.thread);
        assert!(filed.trigger.as_str().starts_with("mcp:alice:"));
        let again = file(&dir, "alice", Some(&filed.thread), "more", 2).unwrap();
        assert_eq!(again.thread, filed.thread);
        assert_eq!(
            std::fs::read_to_string(super::inbox(&dir))
                .unwrap()
                .lines()
                .count(),
            2
        );
        let none = replies(&dir, "alice", &filed.thread).unwrap();
        assert_eq!((none.replies.len(), none.earlier), (0, 0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_result_names_the_tickets_page_after_its_text_and_an_acknowledgement_does_not() {
        let dir = std::env::temp_dir().join(format!("agent-mcp-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        open_inbox(&dir).unwrap();
        let channel = McpChannel::new(
            JsonlChannel::open(super::outbox(&dir)).unwrap(),
            TicketLinks::new("https://agent.example.com").unwrap(),
        );
        let send = |id: &str, kind, text: &str| {
            channel.send(
                OutboundMessage {
                    id: MessageId::new(id),
                    channel: reply_to("alice", "t-1"),
                    kind,
                    text: text.into(),
                    ticket: Some(TicketId::new("tk_1")),
                    ask: None,
                },
                SendOptions::default(),
            )
        };
        send("ms_1", MessageKind::Acknowledgement, "On it.")
            .await
            .unwrap();
        send("ms_2", MessageKind::Result, "Done.\n").await.unwrap();
        send("ms_2", MessageKind::Result, "Done.\n").await.unwrap();

        let found = replies(&dir, "alice", "t-1").unwrap();
        let texts: Vec<&str> = found.replies.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "On it.",
                "Done.\n\nDetails: https://agent.example.com/tickets/tk_1"
            ],
            "once, and the same text as the message otherwise"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
