//! `solos-swarm agent stop`: an operator stops a Ticket the way a person's
//! Stop button does, through the running agent.
//!
//! The command is filed in the `mcp` Source's commands file as
//! `mcp:<operator>`. `agent serve` reads it, hands the runtime a
//! [`Command`](super::ports::Command) with [`Order::StopBecause`](super::ports::Order),
//! and the Engine asks the Policy whether that principal may stop work
//! (`policy.people` or `policy.approvers` must name `mcp:<operator>`). The
//! answer is posted on the command's own thread in the `mcp` Channel's outbox,
//! which this command waits for and prints. Nothing here writes the Store: a
//! stop that bypassed the agent could race its workers and its replies.

use super::{STORE_FILE, mcp};
use crate::broker::now_ms;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use solos_agent_core::model::TicketId;
use solos_agent_store::SqliteStore;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(250);
/// The longest Ticket id: the one a directory under `tickets/` may have.
const ID_CHARS: usize = 64;

#[derive(Clone, Debug)]
pub struct Args {
    pub state_dir: PathBuf,
    /// The `mcp:<operator>` principal the Policy is asked about.
    pub operator: String,
    pub ticket: String,
    /// Kept in the Ticket's end reason and its record.
    pub reason: String,
    /// How long to wait for the agent's answer.
    pub wait: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
    /// The agent applied it.
    Stopped { reply: String },
    /// A person or an operator had already stopped it.
    AlreadyStopped,
    /// It ended on its own before: `completed` or `dropped`.
    AlreadyEnded { outcome: String },
    /// The agent answered and the Ticket is still not stopped: the Policy
    /// does not allow `mcp:<operator>`, or the Engine refused.
    Refused { reply: String },
    /// No answer in time. The command stays filed, and a running agent
    /// applies it if it reads it within [`mcp::COMMAND_TTL_MS`].
    NotApplied { waited_secs: u64 },
}

impl Outcome {
    /// 0 when the Ticket is stopped or was never going to run again, 1 when
    /// the agent refused, 2 when it did not answer.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Stopped { .. } | Self::AlreadyStopped | Self::AlreadyEnded { .. } => 0,
            Self::Refused { .. } => 1,
            Self::NotApplied { .. } => 2,
        }
    }

    pub fn render(&self, ticket: &str) -> String {
        match self {
            Self::Stopped { reply } => format!("{ticket}: {reply}"),
            Self::AlreadyStopped => format!("{ticket}: already stopped; nothing to do"),
            Self::AlreadyEnded { outcome } => {
                format!("{ticket}: already ended ({outcome}); nothing to stop")
            }
            Self::Refused { reply } => format!("{ticket}: not stopped. {reply}"),
            Self::NotApplied { waited_secs } => format!(
                "{ticket}: filed, but the agent did not answer in {waited_secs} s. Is it running? \
                 A running agent applies it if it reads it within {} minutes; otherwise run this again",
                mcp::COMMAND_TTL_MS / 60_000
            ),
        }
    }
}

/// Stop `args.ticket` through the agent and say what happened.
pub async fn run(args: &Args) -> Result<Outcome> {
    ensure!(
        !args.ticket.is_empty()
            && args.ticket.len() <= ID_CHARS
            && args
                .ticket
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "{:?} is not a ticket id",
        args.ticket
    );
    let ticket = TicketId::new(args.ticket.clone());
    if let Some(done) = settled(&args.state_dir, &ticket)? {
        return Ok(done);
    }
    let filed = mcp::file_stop(
        &args.state_dir,
        &args.operator,
        &ticket,
        &args.reason,
        now_ms(),
    )?;
    let start = Instant::now();
    loop {
        let replies = mcp::replies(&args.state_dir, &args.operator, &filed.thread)?;
        if let Some(answer) = replies.replies.first() {
            let reply = answer.text.clone();
            return Ok(match settled(&args.state_dir, &ticket)? {
                Some(Outcome::AlreadyStopped) => Outcome::Stopped { reply },
                _ => Outcome::Refused { reply },
            });
        }
        if start.elapsed() >= args.wait {
            return Ok(Outcome::NotApplied {
                waited_secs: args.wait.as_secs(),
            });
        }
        tokio::time::sleep(POLL).await;
    }
}

/// `Some` once the Store says the Ticket has ended, with how.
fn settled(state_dir: &Path, ticket: &TicketId) -> Result<Option<Outcome>> {
    let path = state_dir.join(STORE_FILE);
    let store = SqliteStore::open_read_only(&path)
        .with_context(|| format!("cannot open {} to read", path.display()))?;
    let found = store
        .read_ticket(ticket)?
        .with_context(|| format!("{} has no ticket {ticket}", path.display()))?;
    Ok(found.ended.as_ref().map(|ended| {
        if found.stopped_by_person() {
            Outcome::AlreadyStopped
        } else {
            Outcome::AlreadyEnded {
                outcome: serde_json::to_value(ended.outcome)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default(),
            }
        }
    }))
}
