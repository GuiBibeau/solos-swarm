//! `solos-swarm agent memo`: an operator lists, adds, retires and forgets
//! the agent's memos. `list` reads the Store, read-only. The others are
//! filed in the `mcp` Source's commands file as `mcp:<operator>`, as `agent
//! stop` is, so the running agent's Engine applies them or refuses them under
//! the Policy (`policy.people` or `policy.approvers` must name that
//! principal). Its answer is posted on the command's own thread, which this
//! waits for. Nothing here writes the Store.

use super::{STORE_FILE, mcp, utc};
use crate::broker::now_ms;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use solos_agent_core::memo::{Memo, MemoAuthor, MemoScope, MemoState, MemoTrust};
use solos_agent_core::model::{MemoId, Ms};
use solos_agent_store::SqliteStore;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(250);

/// A memo as an operator reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Listed {
    pub id: MemoId,
    pub scope: MemoScope,
    pub state: &'static str,
    pub trust: MemoTrust,
    /// Empty once forgotten.
    pub text: String,
    /// `mcp:gui`, or the worker's `ticket/assignment`.
    pub author: String,
    pub created_ms: Ms,
    pub verified_ms: Ms,
    /// Why it was retired, or the review's reason for keeping it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The memo it corrects, retired once it was kept (the dream's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<MemoId>,
    /// The memo that holds what it said, once it was retired for one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaced_by: Option<MemoId>,
}

impl Listed {
    pub fn of(memo: &Memo) -> Self {
        let reason = match &memo.state {
            MemoState::Retired { reason, .. } => Some(reason.clone()),
            MemoState::Active {
                review: Some(review),
                ..
            } => Some(format!("kept by {}: {}", review.by, review.reason)),
            _ => None,
        };
        Self {
            id: memo.id.clone(),
            scope: memo.scope.clone(),
            state: memo.state.name(),
            trust: memo.trust,
            text: memo.text.clone(),
            author: match &memo.author {
                MemoAuthor::Operator { source, by } => format!("{source}:{}", by.id),
                MemoAuthor::Worker { ticket, assignment } => format!("{ticket}/{assignment}"),
            },
            created_ms: memo.created_ms,
            verified_ms: memo.verified_ms,
            reason,
            supersedes: memo.supersedes.clone(),
            replaced_by: match &memo.state {
                MemoState::Retired { replaced_by, .. } => replaced_by.clone(),
                _ => None,
            },
        }
    }
}

fn open(state_dir: &Path) -> Result<SqliteStore> {
    let path = state_dir.join(STORE_FILE);
    SqliteStore::open_read_only(&path)
        .with_context(|| format!("cannot open {} to read", path.display()))
}

/// The memos in the Store, by id: only the proposed and active ones unless
/// `all`.
pub fn list(state_dir: &Path, all: bool) -> Result<Vec<Listed>> {
    let memos = open(state_dir)?.memos()?;
    Ok(memos
        .iter()
        .filter(|memo| all || memo.is_live())
        .map(Listed::of)
        .collect())
}

pub fn render(listed: &[Listed]) -> String {
    if listed.is_empty() {
        return "no memos\n".to_owned();
    }
    let mut out = String::new();
    for memo in listed {
        let _ = writeln!(
            out,
            "{} {} {} {} by {} at {}",
            memo.id,
            memo.state,
            memo.scope,
            memo.trust.as_str(),
            memo.author,
            utc::format(memo.created_ms)
        );
        for line in memo.text.lines() {
            let _ = writeln!(out, "  > {line}");
        }
        if let Some(reason) = &memo.reason {
            let _ = writeln!(out, "  ({reason})");
        }
    }
    out
}

/// What an operator asks of the agent's memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Add { scope: MemoScope, text: String },
    Retire { memo: MemoId, reason: String },
    Forget { memo: MemoId },
}

#[derive(Clone, Debug)]
pub struct Args {
    pub state_dir: PathBuf,
    /// The `mcp:<operator>` principal the Policy is asked about.
    pub operator: String,
    pub change: Change,
    /// How long to wait for the agent's answer.
    pub wait: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
    /// The agent applied it; `memo` is the memo as it is now.
    Done { reply: String, memo: Listed },
    /// It was already so: the same memo is live, or the memo is already
    /// retired or forgotten. Nothing was filed.
    Already { memo: Listed },
    /// The agent answered and nothing changed: the Policy does not allow
    /// `mcp:<operator>`, or the Engine refused.
    Refused { reply: String },
    /// No answer in time. The order stays filed, and a running agent
    /// applies it if it reads it within [`mcp::COMMAND_TTL_MS`].
    NotApplied { waited_secs: u64 },
}

impl Outcome {
    /// 0 once the memory is as asked, 1 when the agent refused, 2 when it
    /// did not answer.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Done { .. } | Self::Already { .. } => 0,
            Self::Refused { .. } => 1,
            Self::NotApplied { .. } => 2,
        }
    }

    pub fn render(&self) -> String {
        match self {
            Self::Done { reply, .. } => reply.clone(),
            Self::Already { memo } => {
                format!("{} is already {}; nothing to do", memo.id, memo.state)
            }
            Self::Refused { reply } => format!("not changed. {reply}"),
            Self::NotApplied { waited_secs } => format!(
                "filed, but the agent did not answer in {waited_secs} s. Is it running? A running \
                 agent applies it if it reads it within {} minutes; otherwise run this again",
                mcp::COMMAND_TTL_MS / 60_000
            ),
        }
    }
}

/// The memo `change` is about as the Store holds it now, and whether the
/// change is already true of it.
fn settled(store: &SqliteStore, change: &Change) -> Result<(Option<Memo>, bool)> {
    Ok(match change {
        Change::Add { scope, text } => {
            let text = text.trim();
            let held = store
                .memos()?
                .into_iter()
                .find(|m| m.is_live() && m.scope == *scope && m.text == text);
            let done = held.is_some();
            (held, done)
        }
        Change::Retire { memo, .. } => {
            let found = store
                .read_memo(memo)?
                .with_context(|| format!("there is no memo {memo}"))?;
            let done = !found.is_live();
            (Some(found), done)
        }
        Change::Forget { memo } => {
            let found = store
                .read_memo(memo)?
                .with_context(|| format!("there is no memo {memo}"))?;
            let done = matches!(found.state, MemoState::Forgotten { .. });
            (Some(found), done)
        }
    })
}

/// Make the change through the agent and say what happened.
pub async fn run(args: &Args) -> Result<Outcome> {
    let change = match &args.change {
        Change::Add { scope, text } => Change::Add {
            scope: scope.clone(),
            text: text.trim().to_owned(),
        },
        other => other.clone(),
    };
    if let (Some(memo), true) = settled(&open(&args.state_dir)?, &change)? {
        return Ok(Outcome::Already {
            memo: Listed::of(&memo),
        });
    }
    let order = match &change {
        Change::Add { scope, text } => mcp::OperatorOrder::MemoAdd {
            scope: scope.clone(),
            text: text.clone(),
        },
        Change::Retire { memo, reason } => mcp::OperatorOrder::MemoRetire {
            memo: memo.clone(),
            reason: reason.trim().to_owned(),
        },
        Change::Forget { memo } => mcp::OperatorOrder::MemoForget { memo: memo.clone() },
    };
    let filed = mcp::file_memo(&args.state_dir, &args.operator, order, now_ms())?;
    let start = Instant::now();
    loop {
        let replies = mcp::replies(&args.state_dir, &args.operator, &filed.thread)?;
        if let Some(answer) = replies.replies.first() {
            let reply = answer.text.clone();
            return Ok(match settled(&open(&args.state_dir)?, &change)? {
                (Some(memo), true) => Outcome::Done {
                    reply,
                    memo: Listed::of(&memo),
                },
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

/// A scope as an operator writes it: `global` or `repo:<owner>/<name>`.
pub fn scope(text: &str) -> Result<MemoScope> {
    MemoScope::try_from(text.to_owned()).map_err(anyhow::Error::msg)
}

/// A memo id as `list` prints it.
pub fn memo_id(text: &str) -> Result<MemoId> {
    let hex = text.strip_prefix("mm_").unwrap_or_default();
    ensure!(
        hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{text:?} is not a memo id: expected mm_ and 16 hex digits, as `agent memo list` prints"
    );
    Ok(MemoId::new(text))
}

#[cfg(test)]
mod tests {
    use super::{Listed, memo_id, render, scope};
    use solos_agent_core::memo::{
        Memo, MemoAuthor, MemoReview, MemoScope, MemoSource, MemoState, MemoTrust,
    };
    use solos_agent_core::model::{Actor, MemoId};
    use solos_agent_core::trail::Digest;

    #[test]
    fn ids_and_scopes_are_checked_before_anything_is_filed() {
        assert!(memo_id("mm_0123456789abcdef").is_ok());
        for bad in [
            "",
            "mm_",
            "mm_0123",
            "tk_0123456789abcdef",
            "mm_0123456789ABCDEF",
        ] {
            assert!(memo_id(bad).is_err(), "{bad}");
        }
        assert_eq!(scope("global").unwrap(), MemoScope::Global);
        assert!(scope("repo:o/r").is_ok());
        assert!(scope("repo:o").is_err());
    }

    #[test]
    fn a_listing_quotes_each_line_and_says_why_a_memo_was_kept() {
        let memo = Memo {
            id: MemoId::new("mm_0123456789abcdef"),
            scope: MemoScope::Global,
            text: "Answer in French.\nUnless I write in English.".into(),
            text_sha256: Digest::of(b"x"),
            sources: vec![MemoSource::Operator],
            trust: MemoTrust::Model,
            author: MemoAuthor::Operator {
                source: "mcp".into(),
                by: Actor::person("gui"),
            },
            state: MemoState::Active {
                since_ms: 0,
                review: Some(MemoReview {
                    by: "review".into(),
                    reason: "two tickets agree".into(),
                }),
            },
            created_ms: 0,
            verified_ms: 0,
            supersedes: None,
        };
        let text = render(&[Listed::of(&memo)]);
        assert!(
            text.starts_with("mm_0123456789abcdef active global model by mcp:gui at "),
            "{text}"
        );
        assert!(text.contains("\n  > Answer in French.\n  > Unless I write in English.\n"));
        assert!(
            text.ends_with("  (kept by review: two tickets agree)\n"),
            "{text}"
        );
        assert_eq!(render(&[]), "no memos\n");
    }
}
