//! What the agent and a node say over their TLS connection: JSON frames,
//! each a 4-byte big-endian length and that many bytes, at most
//! [`MAX_FRAME`]. A frame above it ends the connection unread.
//!
//! The node speaks first with a [`Hello`]; the agent answers with a
//! [`Welcome`]. After that the agent sends [`Request`]s and the node answers
//! each with a [`Reply`] carrying its `seq`, in any order. A job is the
//! node's only state: it lives on one connection and dies with it.

use crate::code::{Ended, Unread};
use anyhow::{Context, Result, bail, ensure};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL: u32 = 1;
/// The largest frame either side reads or writes: a whole `run_code`
/// request, or a job's output at its 1 MiB bound per stream, fits.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// The longest a node holds a `JobWait` open.
pub const MAX_WAIT_MS: u64 = 30_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    /// The node's name in the agent's config.
    pub name: String,
    pub protocol: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Welcome {
    Accepted,
    Refused { why: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub seq: u64,
    pub call: Call,
}

/// `job` ids are the agent's, one per command; `work` names the Ticket
/// whose `/work` the command gets on the node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Call {
    /// `run_code`'s request, started as a job.
    Run {
        job: u64,
        work: String,
        args: Value,
    },
    /// A `job_start` command.
    JobStart {
        job: u64,
        work: String,
        code: String,
        timeout_ms: u64,
        stdout_bytes: usize,
        stderr_bytes: usize,
    },
    /// The job's end if it comes within `wait_ms`, else what it printed
    /// since the last wait.
    JobWait {
        job: u64,
        wait_ms: u64,
    },
    JobStop {
        job: u64,
    },
    Ping,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub seq: u64,
    pub answer: Answer,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Answer {
    Accepted,
    Progress {
        running: bool,
        stdout: String,
        stderr: String,
        truncated: bool,
    },
    Ended(End),
    Refused {
        why: String,
    },
    Pong,
}

impl Answer {
    pub fn progress(running: bool, unread: Unread) -> Self {
        Self::Progress {
            running,
            stdout: unread.stdout,
            stderr: unread.stderr,
            truncated: unread.truncated,
        }
    }
}

/// [`Ended`] on the wire. The receipt is `run_code`'s, as the node's
/// sandbox wrote it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum End {
    Ran { receipt: Value, stopped: bool },
    Failed { why: String },
}

impl From<Ended> for End {
    fn from(ended: Ended) -> Self {
        match ended {
            Ended::Ran { receipt, stopped } => Self::Ran { receipt, stopped },
            Ended::Failed(why) => Self::Failed { why },
        }
    }
}

impl From<End> for Ended {
    fn from(end: End) -> Self {
        match end {
            End::Ran { receipt, stopped } => Self::Ran { receipt, stopped },
            End::Failed { why } => Self::Failed(why),
        }
    }
}

/// `value` as one frame, its length first. Refused above [`MAX_FRAME`].
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(value)?;
    ensure!(
        body.len() <= MAX_FRAME,
        "a frame of {} bytes is above the {MAX_FRAME}-byte limit",
        body.len()
    );
    let length = u32::try_from(body.len()).context("frame length")?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

pub async fn send<W: AsyncWrite + Unpin, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
    writer.write_all(&encode(value)?).await?;
    writer.flush().await?;
    Ok(())
}

/// The next frame, or `None` when the peer closed between frames. Its
/// length is checked before anything is read or allocated for it.
pub async fn receive<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<Option<T>> {
    let mut length = [0u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let length = usize::try_from(u32::from_be_bytes(length)).context("frame length")?;
    if length > MAX_FRAME {
        bail!("refused a frame of {length} bytes, above the {MAX_FRAME}-byte limit");
    }
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .await
        .context("the connection ended inside a frame")?;
    Ok(Some(
        serde_json::from_slice(&body).context("an unreadable frame")?,
    ))
}

/// A Ticket id the node may name a directory by.
pub fn valid_work(work: &str) -> bool {
    !work.is_empty()
        && work.len() <= 64
        && work
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::{Answer, Call, End, MAX_FRAME, Reply, Request, encode, receive, send, valid_work};
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn frames_round_trip_and_one_above_the_limit_is_refused_before_it_is_read() {
        let (mut near, mut far) = tokio::io::duplex(1 << 16);
        let request = Request {
            seq: 7,
            call: Call::Run {
                job: 3,
                work: "tk_1".into(),
                args: json!({"code": "echo ok"}),
            },
        };
        send(&mut near, &request).await.unwrap();
        let reply = Reply {
            seq: 7,
            answer: Answer::Ended(End::Failed { why: "no".into() }),
        };
        send(&mut near, &reply).await.unwrap();
        assert_eq!(
            receive::<_, Request>(&mut far).await.unwrap(),
            Some(request)
        );
        assert_eq!(receive::<_, Reply>(&mut far).await.unwrap(), Some(reply));

        let oversized = u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes();
        near.write_all(&oversized).await.unwrap();
        let refused = receive::<_, Request>(&mut far).await.unwrap_err();
        assert!(refused.to_string().contains("above the"), "{refused}");

        drop(near);
        assert_eq!(receive::<_, Request>(&mut far).await.unwrap(), None);

        let big = "x".repeat(MAX_FRAME);
        assert!(
            encode(&json!({ "text": big })).is_err(),
            "an oversized frame was written"
        );
    }

    #[test]
    fn work_names_are_ticket_ids_only() {
        assert!(valid_work("tk_0f3a-9"));
        for bad in ["", "..", "a/b", "tk 1", &"x".repeat(65)] {
            assert!(!valid_work(bad), "accepted {bad:?}");
        }
    }
}
