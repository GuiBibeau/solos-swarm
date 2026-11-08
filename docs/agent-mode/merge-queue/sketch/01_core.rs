//! Merge queue, core (crates/agent-core): the queue record, its one pure step
//! function, and the merge gate generalized so a bot's own `Change` and a
//! queue `Entry` share it. Documentation, not compiled code.
//!
//!  accept (Plan::Queue) ──▶ Ticket.queue, no worker launched
//!  observe(active entry) ──▶ Observed::Pull ──queue_step──▶ one of:
//!     Effect::Forge(Update{from,base,head:None})   the host merges the base in, in its mirror
//!     Launch (a resolve round)                      a worker resolves conflicts, `ready` is admitted
//!     Effect::Forge(Update{.., head:Some(h)})       push the admitted round's head, leased on `from`
//!     Ask::Merge{pr,head} | Effect::Forge(Merge{pr,head})
//!     a stop: Attention + Ask::Continue
//!  Observed::Merged ──▶ next entry ... last entry ──▶ the per-PR report, the Ticket ends

use crate::change::{Checks, CommitSha, PrRef, RepoName};
use crate::model::{Actor, Ms, Ticket, TicketId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// The record. `Ticket` gains `#[serde(default)] pub queue: Option<Queue>`;
// `Plan` gains `Queue { pulls: Vec<u64> }`. Schema: the next free version,
// an empty migration, so an older build refuses the file.
// ---------------------------------------------------------------------------

/// A pull request's head ref in the queue's own repository: the only kind of
/// branch outside `solos/` the agent may push to, and only as a fast-forward
/// of a head it observed. Built only by preflight, from GitHub's own read of
/// the pull request, never from a model's words.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PrBranch(String);

impl PrBranch {
    /// Refuses: the base, the repository's default branch, anything
    /// `git check-ref-format --branch` refuses, and `refs/` spellings.
    pub fn new(_raw: &str, _base: &str, _default_branch: &str) -> Result<Self, String> {
        todo!()
    }
}

impl TryFrom<String> for PrBranch {
    type Error = String;
    fn try_from(_value: String) -> Result<Self, String> {
        todo!("the stored form re-validated on load, minus the base check")
    }
}

impl From<PrBranch> for String {
    fn from(value: PrBranch) -> String {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queue {
    pub repo: RepoName,
    /// The base every entry merges into, from config at accept.
    pub base: String,
    /// In the order the person named them.
    pub entries: Vec<Entry>,
}

impl Queue {
    /// The first entry neither merged nor skipped. Derived, never stored.
    pub fn active(&self) -> Option<&Entry> {
        todo!()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub pr: PrRef,
    /// `None` until preflight read the pull request.
    pub branch: Option<PrBranch>,
    /// From GitHub, clipped and mention-quieted; for people, never for a
    /// decision.
    pub title: String,
    pub author: String,
    /// The bot's own pull request, and the Ticket whose Change it is.
    pub own: Option<TicketId>,
    pub pull: PullView,
    /// Who made `pull.head`.
    pub made: Made,
    /// Host-made updates so far (`QueueTerms::max_updates`).
    pub updates: u32,
    /// Resolve rounds so far (`QueueTerms::max_rounds`).
    pub rounds: u32,
    /// An Update Effect is committed for this `(from, base)`.
    pub updating: Option<Updating>,
    pub state: EntryState,
    /// A question only a person clears (Continue, a reply to a round, Stop).
    pub stop: Option<Stop>,
}

/// What both a bot's own `Change` (GitHub App PR 5) and a queue `Entry` hold
/// of one pull request, so one gate judges both.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullView {
    pub head: Option<CommitSha>,
    /// The base's tip at the last observation, and whether the head lacks
    /// it (GitHub's compare, `behind_by > 0`).
    pub base_tip: Option<CommitSha>,
    pub behind: bool,
    pub checks: Checks,
    pub verdicts: Verdicts,
    pub merge: MergeState,
    pub open: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Made {
    /// Whatever the pull request held when observed: its author's, or a
    /// person's later push.
    Person,
    /// The host's own clean merge of `base` into `from`.
    Host { from: CommitSha, base: CommitSha },
    /// A resolve round's admitted head.
    Round { n: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Updating {
    pub from: CommitSha,
    pub base: CommitSha,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EntryState {
    Pending,
    Merged { commit: CommitSha, head: CommitSha, by: Grantor, at_ms: Ms },
    /// A person merged it outside the queue.
    MergedByPerson { commit: CommitSha, at_ms: Ms },
    /// A person closed it, and someone pressed Continue.
    Skipped { at_ms: Ms },
}

/// Why the queue waits for a person. Only these are stored: a wait whose
/// cause is an observation (CI pending or failed, behind) is derived.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum Stop {
    /// Not open, wrong base, a draft, a fork that is behind, the base itself,
    /// held by another open queue.
    Preflight { reason: String },
    Closed { by: Option<String> },
    /// Conflicts no worker may or could resolve.
    Conflict { paths: Vec<String>, protected: Vec<String>, question: Option<String> },
    /// The forge refused the update push (workflow files, a ruleset,
    /// signed commits), in its words, clipped.
    UpdateRefused { message: String },
    BaseKeepsMoving { updates: u32 },
    CiSilent { head: CommitSha, waited_secs: u64 },
    /// CI failed at a round's head and its repairs are used up.
    RepairsExhausted { head: CommitSha },
    MergeRefused { head: CommitSha, message: String },
    /// The read-back found the squash commit's parent is not the tip the gate
    /// saw: what landed is not what CI tested.
    UntestedMerge { pr: PrRef, commit: CommitSha },
}

// ---------------------------------------------------------------------------
// Verdicts and merge progress: GitHub App PR 5's types (its sketch
// 01_core.rs), unchanged but for the `pr` the asks and orders now carry.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Grantor {
    Person { actor: Actor, source: String },
    Terms,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub allow: bool,
    pub source: String,
    pub at_ms: Ms,
}

/// Keyed by head: a verdict for another head counts for nothing by
/// construction.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdicts {
    pub head: Option<CommitSha>,
    pub by: BTreeMap<String, Verdict>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MergeState {
    Open,
    Asked { head: CommitSha, at_ms: Ms },
    /// `base` is the tip the gate saw; the read-back checks the squash
    /// commit's parent against it.
    Merging { head: CommitSha, base: CommitSha, by: Grantor },
    Refused { head: CommitSha, why: String },
}

// ---------------------------------------------------------------------------
// Terms, fixed at accept (config.rs `QueueConfig::terms`).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueTerms {
    pub max_pulls: u32,
    pub max_updates: u32,
    pub resolve: Resolve,
    pub max_rounds: u32,
    pub ci_wait_secs: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolve {
    Worker,
    Stop,
}

// ---------------------------------------------------------------------------
// The gate, generalized (GitHub App PR 5's ten conditions plus two).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeGate {
    Go { head: CommitSha, base: CommitSha, by: Grantor },
    Waiting(Waiting),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waiting {
    Off,
    Stopped,
    NotOpen,
    Held,
    InFlight,
    Working,
    /// The head lacks the base's tip: the queue updates it.
    Behind,
    ChecksPending,
    ChecksFailed,
    Denied,
    NoApproval,
    AutoWithheld,
    Merging,
    Refused,
}

/// What the gate reads, built from a `Change` or from an `Entry` and its
/// Ticket. `auto_allowed` is the caller's: for a `Change`, PR 5's
/// untainted-requester rule; for an `Entry`, also `own.is_some()` and
/// `made` not a `Round` (decision 5).
pub struct GateInput<'a> {
    pub pull: &'a PullView,
    pub merge_mode: crate::change::MergeMode,
    pub ci_expected: bool,
    pub stopped_by_person: bool,
    pub held: bool,
    pub in_flight: bool,
    pub worker_live: bool,
    /// `Some` for a queue entry: the head must contain the base's tip.
    /// `None` for a bot's own Change, as PR 5 planned.
    pub must_contain_base: Option<bool>,
    pub auto_allowed: bool,
}

/// First failure wins, in PR 5's order, with `Behind` after `Working`.
pub fn merge_gate(_input: &GateInput<'_>) -> MergeGate {
    todo!()
}

// ---------------------------------------------------------------------------
// The step: pure, run by the Engine after every fact that touches the Ticket.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    Nothing,
    /// A person or an observation is awaited; `told` is the one message to
    /// send for this head, if it was not sent yet.
    Wait { why: Waiting, told: Option<String> },
    Stop(Stop),
    /// Commit `Effect::Forge(ForgeJob::Update)`.
    Update { pr: PrRef, from: CommitSha, base: CommitSha, head: Option<CommitSha> },
    /// Launch a resolve round on the entry.
    Resolve { pr: PrRef, from: CommitSha, base: CommitSha, conflicts: Vec<String> },
    Ask { pr: PrRef, head: CommitSha },
    Merge { pr: PrRef, head: CommitSha, base: CommitSha, by: Grantor },
    /// Every entry is merged or skipped: end with the report.
    Finish,
}

/// The order of §5.2 of the design.
pub fn queue_step(_ticket: &Ticket, _terms: &QueueTerms, _now: Ms) -> Next {
    todo!()
}

/// The Result: one line per entry, from the Store only.
pub fn report(_queue: &Queue) -> String {
    todo!()
}

// ---------------------------------------------------------------------------
// Facts, Effects, Asks and Orders.
// ---------------------------------------------------------------------------

/// `ForgeJob` gains:
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum QueueJob {
    /// Make `pr`'s branch hold a head that contains `base`, as a fast-forward
    /// of `from`, leased on `from`. `head: None`: the host's own merge, or
    /// `Observed::Conflicted` when it cannot. `Some`: an admitted round's head.
    Update { ticket: TicketId, pr: PrRef, from: CommitSha, base: CommitSha, head: Option<CommitSha> },
    /// GitHub App PR 5's merge, with the PR named: squash, pinned to `head`,
    /// read back, the squash commit's parent checked against `base`.
    Merge { ticket: TicketId, pr: PrRef, head: CommitSha, base: CommitSha },
}

/// `Observed` gains (all idempotent):
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueObserved {
    /// A read of one pull request: Seen generalized from a bot branch to a PR.
    Pull { ticket: TicketId, seen: PullSeen },
    Updated { ticket: TicketId, pr: PrRef, from: CommitSha, base: CommitSha, head: CommitSha, by: Made },
    Conflicted { ticket: TicketId, pr: PrRef, from: CommitSha, base: CommitSha, paths: Vec<String> },
    /// The update push was refused in words, or the lease found the branch
    /// moved (`moved: Some`), which only re-observes.
    UpdateRefused { ticket: TicketId, pr: PrRef, from: CommitSha, moved: Option<CommitSha>, message: String },
    Merged { ticket: TicketId, pr: PrRef, head: CommitSha, commit: CommitSha, parent: CommitSha },
    MergeRefused { ticket: TicketId, pr: PrRef, head: CommitSha, status: u16, why: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullSeen {
    pub pr: PrRef,
    pub state: PullState,
    pub head: CommitSha,
    pub head_ref: String,
    pub same_repo: bool,
    pub base_ref: String,
    pub draft: bool,
    pub base_tip: CommitSha,
    pub behind: bool,
    pub checks: Checks,
    pub body_has_marker_of: Option<TicketId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PullState {
    Open,
    Closed { by: Option<String> },
    Merged { commit: CommitSha },
}

/// `Ask` gains `Merge { pr, head }` (PR 5's, with the PR and the full head in
/// the button's id) and `Continue { pr }`. `Order` gains `Merge { pr, head,
/// allow }` and `Continue { pr }`: adapter commands, `policy.may_approve`.
pub struct AsksAndOrders;

/// `DomainEvent` gains: QueueAccepted, EntryObserved (kind changes only),
/// EntryUpdated, EntryConflicted, EntryStopped, EntryContinued, EntrySkipped,
/// QueueFinished; PR 5's merge events gain `pr`.
pub struct Events;

/// `Withheld` gains `Queued { by: TicketId }`: a bot's own Change a queue
/// adopted. `ready_gate` refuses, PR 3's repair does not resume it, and its
/// Ticket's follow-ups reach the Router as new work.
pub struct WithheldQueued;

// Tests (crates/agent-core):
//  - queue_step: each branch of §5.2 alone, in order; the active entry is the
//    first not merged or skipped; later entries are never updated early.
//  - merge_gate: PR 5's table, plus Behind; a verdict at another head grants
//    nothing; auto on an entry needs own + not Round + untainted.
//  - Observed::Pull for a PR no open queue or Change names changes nothing.
//  - Updated twice appends one EntryUpdated; Merged twice, one Merged.
//  - Merged whose parent != MergeState::Merging.base stops UntestedMerge.
//  - report() reads only the Store.
