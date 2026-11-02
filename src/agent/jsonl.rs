//! File adapters for tests and dry runs: Triggers in, messages out, one JSON
//! value per line.

use super::ports::{BoxFuture, Channel, FactSink, SendOptions, SendOutcome, Source, TriggerSink};
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use solos_agent_core::model::{MessageId, OutboundMessage, Trigger};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const POLL: Duration = Duration::from_millis(250);

/// Reads Trigger JSON lines from a file, from the start each time it runs.
pub struct JsonlSource {
    path: PathBuf,
    /// Keep reading lines appended later, instead of ending at the end of the
    /// file.
    follow: bool,
}

impl JsonlSource {
    pub fn new(path: impl Into<PathBuf>, follow: bool) -> Self {
        Self {
            path: path.into(),
            follow,
        }
    }
}

impl Source for JsonlSource {
    fn run(self: Box<Self>, sink: TriggerSink, _: FactSink) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let mut cursor = Cursor::default();
            loop {
                let batch = cursor.read(&self.path, !self.follow).await?;
                if !batch.is_empty() && sink.send(batch).await.is_err() {
                    return Ok(());
                }
                if !self.follow {
                    return Ok(());
                }
                tokio::time::sleep(POLL).await;
            }
        })
    }
}

#[derive(Default)]
struct Cursor {
    offset: u64,
    line: usize,
}

impl Cursor {
    /// The Triggers on whole lines past the cursor. With `to_end`, a last line
    /// without a newline counts as whole.
    async fn read(&mut self, path: &Path, to_end: bool) -> Result<Vec<Trigger>> {
        let mut file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("cannot open trigger file {}", path.display()))?;
        if file.metadata().await?.len() < self.offset {
            // Truncated or replaced: start over; stored Triggers come back as duplicates.
            *self = Self::default();
        }
        file.seek(SeekFrom::Start(self.offset)).await?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).await?;
        let whole = if to_end {
            bytes.len()
        } else {
            bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1)
        };
        self.offset += whole as u64;
        let mut batch = Vec::new();
        for raw in bytes[..whole].split_inclusive(|b| *b == b'\n') {
            self.line += 1;
            if raw.trim_ascii().is_empty() {
                continue;
            }
            match serde_json::from_slice::<Trigger>(raw) {
                Ok(trigger) => batch.push(trigger),
                Err(err) => eprintln!(
                    "agent: {}:{}: not a Trigger, skipped: {err}",
                    path.display(),
                    self.line
                ),
            }
        }
        Ok(batch)
    }
}

/// Appends each message to a file as one JSON line, once per `MessageId`,
/// and syncs it before reporting it sent.
pub struct JsonlChannel {
    outbox: Arc<Mutex<Outbox>>,
}

struct Outbox {
    file: File,
    len: u64,
    sent: HashSet<MessageId>,
}

#[derive(Deserialize)]
struct Line {
    id: MessageId,
}

impl JsonlChannel {
    /// Open or create the file and learn which messages it holds. A last line
    /// cut short by a crash is removed: it was never reported sent, so its
    /// Effect is still pending and sends it again.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)
            .with_context(|| format!("cannot open outbox {}", path.display()))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let whole = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        if whole < bytes.len() {
            file.set_len(whole as u64)?;
            file.sync_data()?;
        }
        let mut sent = HashSet::new();
        for (n, raw) in bytes[..whole].split_inclusive(|b| *b == b'\n').enumerate() {
            if raw.trim_ascii().is_empty() {
                continue;
            }
            let line: Line = serde_json::from_slice(raw).with_context(|| {
                format!("{}:{}: not an outbound message", path.display(), n + 1)
            })?;
            sent.insert(line.id);
        }
        Ok(Self {
            outbox: Arc::new(Mutex::new(Outbox {
                file,
                len: whole as u64,
                sent,
            })),
        })
    }
}

impl Outbox {
    fn append(&mut self, message: &OutboundMessage) -> Result<()> {
        if self.sent.contains(&message.id) {
            return Ok(());
        }
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        let written = self
            .file
            .write_all(&line)
            .and_then(|()| self.file.sync_data());
        if let Err(err) = written {
            // A half-written line would run into the next one.
            let _ = self.file.set_len(self.len);
            return Err(err.into());
        }
        self.len += line.len() as u64;
        self.sent.insert(message.id.clone());
        Ok(())
    }
}

/// The outbox already sends each id once, so a repeat needs no check.
impl Channel for JsonlChannel {
    fn send(&self, message: OutboundMessage, _: SendOptions) -> BoxFuture<'_, Result<SendOutcome>> {
        let outbox = self.outbox.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                outbox
                    .lock()
                    .map_err(|_| anyhow!("the outbox lock is poisoned"))?
                    .append(&message)
                    .map(|()| SendOutcome::Sent)
            })
            .await?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{JsonlChannel, JsonlSource};
    use crate::agent::ports::{Channel, FactSink, SendOptions, Source};
    use solos_agent_core::model::{
        Actor, Addressed, ChannelRef, MessageId, MessageKind, OutboundMessage, ThreadKey, Trigger,
        TriggerId,
    };
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-jsonl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn message(id: &str) -> OutboundMessage {
        OutboundMessage {
            id: MessageId::new(id),
            channel: ChannelRef {
                source: "slack".into(),
                address: [("thread_ts".to_owned(), "T1".to_owned())].into(),
            },
            kind: MessageKind::Result,
            text: format!("message {id}"),
            ticket: None,
        }
    }

    fn line(id: &str) -> String {
        let trigger = Trigger {
            id: TriggerId::new(id),
            source: "slack".into(),
            at_ms: 1,
            actor: Actor::person("alice"),
            addressed: Addressed::Mention,
            text: "hello".into(),
            threads: vec![ThreadKey::new("T1")],
            reply_to: Vec::new(),
            meta: serde_json::Value::Null,
        };
        serde_json::to_string(&trigger).unwrap()
    }

    fn ids(batch: &[Trigger]) -> Vec<&str> {
        batch.iter().map(|t| t.id.as_str()).collect()
    }

    fn facts() -> FactSink {
        FactSink(mpsc::channel(1).0)
    }

    async fn send(channel: &JsonlChannel, id: &str) {
        channel
            .send(message(id), SendOptions::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_reopened_outbox_sends_each_message_once_and_drops_a_torn_line() {
        let dir = scratch("outbox");
        let path = dir.join("outbox.jsonl");
        let channel = JsonlChannel::open(&path).unwrap();
        send(&channel, "ms_1").await;
        send(&channel, "ms_1").await;
        drop(channel);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"id":"ms_2","chan"#).unwrap();

        let reopened = JsonlChannel::open(&path).unwrap();
        send(&reopened, "ms_1").await;
        send(&reopened, "ms_2").await;

        let text = std::fs::read_to_string(&path).unwrap();
        let sent: Vec<OutboundMessage> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(sent, [message("ms_1"), message("ms_2")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_source_skips_bad_lines_and_waits_for_a_line_to_end() {
        let dir = scratch("source");
        let path = dir.join("triggers.jsonl");
        let text = format!("{}\nnot json\n\n{}", line("t1"), line("t2"));
        std::fs::write(&path, &text).unwrap();
        let (sink, mut batches) = mpsc::channel(4);
        Box::new(JsonlSource::new(&path, false))
            .run(sink, facts())
            .await
            .unwrap();
        assert_eq!(ids(&batches.recv().await.unwrap()), ["t1", "t2"]);
        assert!(
            batches.recv().await.is_none(),
            "it ends at the end of the file"
        );

        let (sink, mut batches) = mpsc::channel(4);
        let follow = tokio::spawn(Box::new(JsonlSource::new(&path, true)).run(sink, facts()));
        assert_eq!(
            ids(&batches.recv().await.unwrap()),
            ["t1"],
            "t2 has no newline yet"
        );
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file).unwrap();
        writeln!(file, "{}", line("t3")).unwrap();
        let next = tokio::time::timeout(Duration::from_secs(5), batches.recv()).await;
        assert_eq!(ids(&next.unwrap().unwrap()), ["t2", "t3"]);
        follow.abort();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
