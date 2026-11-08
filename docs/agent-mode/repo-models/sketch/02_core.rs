//! Lives at: crates/agent-core/src/model.rs (Mandate, Assignment, Verdict,
//! DecisionKind), crates/agent-core/src/memo.rs (Memo.carries, MemoAudience,
//! memo_index) and crates/agent-core/src/engine.rs (decline, carries).
//!
//! Sketch only: not compiled. agent-core stays pure. It never reads a
//! provider name for a decision: it records what the host chose, and it
//! filters memos by a set of repositories the host hands it.

use crate::change::RepoName;
use crate::model::{AssignmentId, ChannelRef, TicketId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// What a Ticket runs on (model.rs)
// ---------------------------------------------------------------------------

/// The upstream a Ticket was accepted to run on, fixed at accept from the
/// repository's policy, never from a model's words. Plain strings: the core
/// does not know providers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamChoice {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub harness: String,
}

pub struct Mandate {
    // ... every field as today (model.rs:275-297) ...
    /// `None`: accepted before this build, or a Ticket with no repository on
    /// the default upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<UpstreamChoice>,
}

/// What one Assignment ran on, set by `assignment_started`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RanOn {
    pub upstream: String,
    pub provider: String,
    pub model: String,
    pub harness: String,
}

pub struct Assignment {
    // ... every field as today (model.rs:409-417) ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ran_on: Option<RanOn>,
}

// ---------------------------------------------------------------------------
// Declining (model.rs, engine.rs)
// ---------------------------------------------------------------------------

pub enum Verdict {
    Accept(Accept),
    Ignore { reason: String },
    Stop { ticket: TicketId, reason: String },
    /// The host's, never the Router's: an accept on a repository no upstream
    /// here may run. On an explicit Eval the Engine sends `tell` once to the
    /// request's channels; on an inferred one it only records the Decision.
    Decline { reason: String, tell: String },
}

pub enum DecisionKind {
    Accept,
    Ignore,
    Stop,
    Unavailable,
    Declined,
}

// ---------------------------------------------------------------------------
// Memos (memo.rs)
// ---------------------------------------------------------------------------

pub struct Memo {
    // ... every field as today (memo.rs:284-303) ...
    /// The repositories whose text the memo holds, derived by the core and
    /// never given: the repository of the Ticket each source belongs to
    /// (`Ticket`, `Call` and `Transcript` sources name a Ticket or an
    /// Assignment of one), and the scope's repository. An owner's global memo
    /// carries none. A `global` memo filed on R carries R.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub carries: BTreeSet<RepoName>,
}

/// Derive `carries` as `trust` is derived: from the sources the host checked.
pub(crate) fn carries(memo: &Memo, ticket_repo: impl Fn(&TicketId) -> Option<RepoName>,
                      assignment_ticket: impl Fn(&AssignmentId) -> Option<TicketId>) -> BTreeSet<RepoName> {
    todo!()
}

pub enum MemoAudience {
    Router { cleared: Cleared },
    Worker { ticket: TicketId, assignment: AssignmentId, cleared: Cleared },
}

/// The repositories this audience may read, as the host computed from the
/// policy and the session's provider. `All` is a config with no restriction
/// that matters to this audience.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cleared {
    All,
    Only(BTreeSet<RepoName>),
}

impl Cleared {
    pub fn admits(&self, memo: &Memo) -> bool {
        match self {
            Self::All => true,
            Self::Only(repos) => memo.carries.is_subset(repos),
        }
    }
}

/// As today (memo.rs:424), with every memo the audience is not cleared for
/// left out before the budget is spent, and not counted in the overflow line,
/// so the index does not even say how many were withheld.
pub fn memo_index(
    memos: &[Memo],
    scopes: &[crate::memo::MemoScope],
    budget: usize,
    to: &MemoAudience,
) -> crate::memo::MemoIndex {
    todo!()
}

// Unused here, named so the sketch reads: a Decline's Send goes to the
// Eval's reply channels.
#[allow(dead_code)]
fn reply_channels(_: &[ChannelRef]) {}
