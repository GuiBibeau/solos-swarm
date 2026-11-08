//! `web.chat`: the hosted dashboard's chat, read from the private Blob store
//! and spooled as Trigger JSON lines for the agent's `kind: jsonl` Source.
//!
//! The dashboard writes `web/inbox.json` in the same private store the
//! publisher uploads to. It keeps the last 200 messages, oldest first. Every
//! [`tick`](ChatBridge::poll) reads that file, turns each new message from a
//! listed person into the core's [`Trigger`] shape, and appends the batch to
//! `<state>/web/triggers.jsonl` in one write and an `fsync`. The agent's
//! [`JsonlSource`](crate::agent::jsonl::JsonlSource) follows the file.
//!
//! What has been read is remembered in `<state>/web/cursor.json`: the last
//! processed message id and its time, and the last [`SEEN`] ids, so a restart
//! does not turn the same message into a second Trigger. The core also drops a
//! Trigger id it already holds, which covers a crash between the spool write
//! and the cursor write.
//!
//! The trust boundary is the Blob token: only the publisher and the dashboard
//! can read the inbox. This bridge is the second check on who may speak (the
//! first is the dashboard, which writes only signed-in people's messages),
//! and the agent's `Policy` is the third: a Trigger's principal is
//! `web:<author email>`, which `policy.people` must name too.

use super::blob::{BlobClient, GetError};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use solos_agent_core::model::{
    Actor, ActorKind, Addressed, ChannelRef, Ms, ThreadKey, Trigger, TriggerId,
};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The Source's name, so a trigger's principal is `web:<author email>`.
pub const SOURCE: &str = "web";
/// The version of the cursor file's shape.
const CURSOR_VERSION: u32 = 1;
/// The version of the inbox the dashboard writes.
const INBOX_VERSION: u64 = 1;
/// How many ids the cursor remembers. The inbox keeps 200 messages, so this
/// survives a whole inbox's rotation.
const SEEN: usize = 200;

/// What `web.chat` needs, with the state directory's defaults filled in.
#[derive(Clone, Debug)]
pub struct ChatOptions {
    /// The inbox's pathname in the Blob store.
    pub inbox: String,
    /// The authors whose messages may become Triggers.
    pub people: BTreeSet<String>,
    /// Where Trigger lines are appended.
    pub spool: PathBuf,
    /// Where what has been read is remembered.
    pub cursor: PathBuf,
    /// The dashboard's origin, which a Trigger's `meta.url` names.
    pub public_url: String,
}

/// Reads the inbox and appends what is new to the spool.
pub struct ChatBridge {
    options: ChatOptions,
    cursor: Cursor,
    /// Whether the last look found no inbox: a dashboard that has not sent a
    /// message yet. Logged once, not every tick.
    missing: bool,
    /// The last whole-inbox error, so a broken writer is logged once.
    body_error: Option<String>,
    /// Malformed entries already logged in this run, by their JSON, since an
    /// entry the cursor cannot pass is read again next tick.
    skipped: HashSet<String>,
}

impl ChatBridge {
    /// Load the cursor beside the spool.
    pub fn new(options: ChatOptions) -> Self {
        let cursor = Cursor::load(&options.cursor);
        Self {
            options,
            cursor,
            missing: false,
            body_error: None,
            skipped: HashSet::new(),
        }
    }

    /// Read the inbox once and spool the new messages. Returns how many
    /// Trigger lines were appended.
    pub async fn poll(&mut self, client: &BlobClient) -> Result<usize, GetError> {
        let body = match client.get(&self.options.inbox).await {
            Ok(body) => {
                self.missing = false;
                body
            }
            Err(GetError::Missing(why)) => {
                if !self.missing {
                    eprintln!("publish: web.chat: {why}; waiting for the first message");
                    self.missing = true;
                }
                return Ok(0);
            }
            Err(err) => return Err(err),
        };
        Ok(self.ingest(&body))
    }

    /// Turn the inbox's new messages into Trigger lines. Malformed messages
    /// and authors off the list are logged and skipped, never fatal.
    fn ingest(&mut self, body: &[u8]) -> usize {
        let inbox: Value = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(err) => {
                self.note_body_error(format!("the inbox is not JSON: {err}"));
                return 0;
            }
        };
        if inbox.get("v").and_then(Value::as_u64) != Some(INBOX_VERSION) {
            self.note_body_error(format!(
                "the inbox is version {:?}, not {INBOX_VERSION}; skipping it",
                inbox.get("v")
            ));
            return 0;
        }
        let Some(messages) = inbox.get("messages").and_then(Value::as_array) else {
            self.note_body_error("the inbox has no messages array".into());
            return 0;
        };
        self.body_error = None;
        let start = self.start(messages);
        let mut cursor = self.cursor.clone();
        let mut lines = Vec::new();
        for message in &messages[start..] {
            self.message(message, &mut cursor, &mut lines);
        }
        if !lines.is_empty()
            && let Err(err) = self.append(&lines)
        {
            // The cursor is not committed: next tick tries the whole batch
            // again. A crash after the write and before the cursor write
            // spools the same Trigger ids again, which the core drops.
            eprintln!("publish: web.chat: {err:#}");
            return 0;
        }
        if cursor != self.cursor {
            self.cursor = cursor;
            if let Err(err) = self.cursor.save(&self.options.cursor) {
                eprintln!("publish: web.chat: {err:#}");
            }
        }
        lines.len()
    }

    /// Where to start in the inbox: after the last processed id when it is
    /// still there, and otherwise at the first message newer than the stored
    /// time.
    fn start(&self, messages: &[Value]) -> usize {
        if let Some(id) = &self.cursor.id
            && let Some(index) = messages
                .iter()
                .rposition(|message| message.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            return index + 1;
        }
        messages
            .iter()
            .position(|message| {
                message
                    .get("at_ms")
                    .and_then(Value::as_u64)
                    .is_some_and(|at| at > self.cursor.at_ms)
            })
            .unwrap_or(messages.len())
    }

    /// One message: a Trigger, a refused author, or a skip.
    fn message(&mut self, message: &Value, cursor: &mut Cursor, lines: &mut Vec<String>) {
        let id = message
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| shaped("wm_", id));
        let at_ms = message.get("at_ms").and_then(Value::as_u64);

        // An id already processed is never processed again, even when the
        // cursor had to start from the time instead of the id.
        if let Some(id) = id
            && cursor.seen(id)
        {
            cursor.pass(id, at_ms);
            return;
        }

        let author = message.get("author").and_then(Value::as_str);
        // The dashboard omits the name when the viewer has none.
        let name = match message.get("name") {
            None => Some(None),
            Some(Value::String(name)) => Some(Some(name.as_str())),
            Some(_) => None,
        };
        let text = message.get("text").and_then(Value::as_str);
        let thread = message
            .get("thread")
            .and_then(Value::as_str)
            .filter(|thread| shaped("wc_", thread));
        let Some((id, at_ms, author, name, text, thread)) = id
            .zip(at_ms)
            .zip(author)
            .zip(name)
            .zip(text)
            .zip(thread)
            .map(|(((((id, at_ms), author), name), text), thread)| {
                (id, at_ms, author, name, text, thread)
            })
        else {
            self.skip(message, id, at_ms, cursor);
            return;
        };

        if !self.options.people.contains(author) {
            // The text is not logged: it is not for this agent.
            eprintln!("publish: web.chat: refused a message from {author}");
            cursor.pass(id, Some(at_ms));
            return;
        }

        let thread = thread.to_owned();
        let trigger = Trigger {
            id: TriggerId::new(format!("tr_{}", &id["wm_".len()..])),
            source: SOURCE.into(),
            at_ms,
            actor: Actor {
                id: author.to_owned(),
                kind: ActorKind::Person,
                name: name
                    .map(str::to_owned)
                    .filter(|name| !name.trim().is_empty()),
            },
            addressed: Addressed::Mention,
            text: text.to_owned(),
            threads: vec![ThreadKey::new(format!("{SOURCE}:{thread}"))],
            reply_to: vec![ChannelRef {
                source: SOURCE.into(),
                address: BTreeMap::from([("thread".to_owned(), thread.clone())]),
            }],
            meta: json!({
                "thread": thread,
                "url": format!("{}/?t={thread}", self.options.public_url.trim_end_matches('/')),
            }),
        };
        match serde_json::to_string(&trigger) {
            Ok(line) => {
                lines.push(line);
                cursor.pass(id, Some(at_ms));
            }
            Err(err) => eprintln!("publish: web.chat: cannot serialise a Trigger: {err}"),
        }
    }

    /// A malformed entry: logged once per run, and passed as far as its id or
    /// time allows so its neighbours still move the cursor.
    fn skip(&mut self, message: &Value, id: Option<&str>, at_ms: Option<Ms>, cursor: &mut Cursor) {
        let fingerprint = serde_json::to_string(message).unwrap_or_default();
        if self.skipped.insert(fingerprint) {
            match id {
                Some(id) => eprintln!("publish: web.chat: skipped a malformed message {id}"),
                None => eprintln!("publish: web.chat: skipped a malformed message"),
            }
        }
        match id {
            Some(id) => cursor.pass(id, at_ms),
            None => {
                if let Some(at) = at_ms {
                    cursor.at_ms = cursor.at_ms.max(at);
                }
            }
        }
    }

    /// One write and one sync, so a reader never sees half a batch.
    fn append(&self, lines: &[String]) -> Result<()> {
        let path = &self.options.spool;
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let mut bytes = Vec::new();
        for line in lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("cannot open the web spool {}", path.display()))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("cannot append to the web spool {}", path.display()))?;
        Ok(())
    }

    fn note_body_error(&mut self, why: String) {
        if self.body_error.as_deref() != Some(why.as_str()) {
            eprintln!("publish: web.chat: {why}");
            self.body_error = Some(why);
        }
    }
}

/// What the bridge remembers between runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    v: u32,
    /// The last processed message's id.
    #[serde(default)]
    id: Option<String>,
    /// Its time: where to start when the id has fallen out of the inbox.
    #[serde(default)]
    at_ms: Ms,
    /// The last [`SEEN`] ids processed, oldest first.
    #[serde(default)]
    seen: VecDeque<String>,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            v: CURSOR_VERSION,
            id: None,
            at_ms: 0,
            seen: VecDeque::new(),
        }
    }
}

impl Cursor {
    fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(mut cursor) if cursor.v == CURSOR_VERSION => {
                while cursor.seen.len() > SEEN {
                    cursor.seen.pop_front();
                }
                cursor
            }
            _ => Self::default(),
        }
    }

    /// Written whole or not at all, readable by the agent's user only.
    fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))
    }

    /// Whether `id` was processed already.
    fn seen(&self, id: &str) -> bool {
        self.id.as_deref() == Some(id) || self.seen.iter().any(|seen| seen == id)
    }

    /// Remember `id` as processed, and move the time forward.
    fn pass(&mut self, id: &str, at_ms: Option<Ms>) {
        self.id = Some(id.to_owned());
        if let Some(at) = at_ms {
            self.at_ms = self.at_ms.max(at);
        }
        if !self.seen.iter().any(|seen| seen == id) {
            self.seen.push_back(id.to_owned());
            while self.seen.len() > SEEN {
                self.seen.pop_front();
            }
        }
    }
}

/// `prefix` and exactly 32 hex digits, the ids the dashboard writes.
fn shaped(prefix: &str, value: &str) -> bool {
    value.len() == prefix.len() + 32
        && value.starts_with(prefix)
        && value[prefix.len()..].bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{Cursor, SEEN, shaped};

    #[test]
    fn the_ids_the_dashboard_writes_are_prefixed_and_hex() {
        assert!(shaped(
            "wm_",
            &format!("wm_{}", "0123456789abcdef".repeat(2))
        ));
        assert!(shaped("wc_", &format!("wc_{}", "AbCdEf".repeat(5) + "ab")));
        for bad in [
            "wm_",
            "wm_0123",
            "wx_0123456789abcdef0123456789abcdef",
            "wm_0123456789abcdef0123456789abcde",
            "wm_0123456789abcdef0123456789abcdeg",
            " wm_0123456789abcdef0123456789abcdef",
        ] {
            assert!(!shaped("wm_", bad), "{bad:?}");
        }
    }

    #[test]
    fn the_cursor_keeps_the_last_two_hundred_ids_and_never_repeats_one() {
        let mut cursor = Cursor::default();
        for n in 0..(SEEN + 5) {
            cursor.pass(&format!("wm_{n:032x}"), Some(n as u64));
        }
        assert_eq!(cursor.seen.len(), SEEN);
        assert!(!cursor.seen(&format!("wm_{:032x}", 0)));
        assert!(cursor.seen(&format!("wm_{:032x}", SEEN + 4)));
        assert_eq!(
            cursor.id.as_deref(),
            Some(format!("wm_{:032x}", SEEN + 4).as_str())
        );
        assert_eq!(cursor.at_ms, (SEEN + 4) as u64);

        // Passing the same id again moves the time but keeps one entry.
        cursor.pass(&format!("wm_{:032x}", SEEN + 4), Some(9_999));
        assert_eq!(cursor.seen.len(), SEEN);
        assert_eq!(cursor.at_ms, 9_999);
    }
}
