//! Lives at: crates/agent-core/src/change.rs (the names and the Change record),
//! crates/agent-core/src/delivery.rs (new: the two gates and the Change's pure
//! transitions) and additions to model.rs and engine.rs, listed at the bottom.
//!
//! Sketch only: not compiled. Candidate B's core, amended by the synthesis
//! (rationale.md, "Synthesis decision"): `Change.held` and a live worker are
//! merge-gate conditions, merge authority is a per-person verdict map at the
//! head, and `merge: auto` needs an untainted Ticket.
//!
//! agent-core stays pure: no I/O, no crypto, no HTTP types. A forge appears
//! here only as data the host produced: a branch the host named, a head the
//! host admitted, an observation the host made, a verdict a Source vouched
//! for. Nothing here is a claim a model made.
//!
//! The load-bearing idea: a pull request is not something a worker *does*, it
//! is something the Ticket *owes* and the host *delivers*. The worker's whole
//! vote is `ready` (which the host may refuse). Everything after `ready` is a
//! state machine over `Change`, driven by Effects the Engine commits in the
//! same transaction as the decision, and by Observations the host makes of
//! the forge. Crashes are the Effect queue's problem, which it already solves.
//!
//! ```text
//!  report(ready) ──ready_gate──▶ Propose{head} ── Effect::Forge(Publish) ──▶ [host converges the forge]
//!                                                                      ◀── Observed::Published{pr}
//!  Change.delivered = pr@head ──▶ Result sent, Ticket ends
//!  Observed::Seen{checks: Failed @ head} ──▶ repair round (resume, bounded) ──▶ report(ready) again
//!  Observed::Seen{checks: Passed @ head} ──merge_gate──▶ ask a person | Effect::Forge(Merge)
//!  Command(Order::Merge) ──merge_verdict──merge_gate──▶ Effect::Forge(Merge{head}) ──▶ Observed::Merged
//! ```

use crate::change::RepoName;
use crate::ids::key;
use crate::model::{Actor, ChannelRef, Ms, ThreadKey, Ticket, TicketId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Names (change.rs). Each is parsed once at the boundary and is valid after.
// ---------------------------------------------------------------------------

/// A commit id: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits. The forge's
/// merge call and the host's lease both take a sha, so a malformed one can
/// never reach git's argv or a URL.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommitSha(String);

impl CommitSha {
    /// Twelve digits, for people and for a Discord `custom_id`.
    pub fn short(&self) -> &str {
        &self.0[..12]
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for CommitSha {
    type Error = String;
    fn try_from(_value: String) -> Result<Self, String> {
        todo!("40 or 64 lowercase hex digits")
    }
}
impl From<CommitSha> for String {
    fn from(value: CommitSha) -> String {
        value.0
    }
}

/// `solos/<ticket id>/<slug>`: the only kind of branch the agent ever writes.
/// The host names it. The model's only influence is the Ticket's title, which
/// passes through `slug` (ASCII lowercase letters, digits and `-`, at most 40
/// characters, never empty). The Ticket id is inside the name, so ownership
/// is *derived*: a branch a forge reports belongs to `ticket()` or to nobody.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BranchName(String);

impl BranchName {
    pub const NAMESPACE: &'static str = "solos";

    pub fn for_ticket(_ticket: &TicketId, _title: &str) -> Self {
        todo!("solos/<ticket>/<slug(title)>")
    }
    /// Only the exact shape `for_ticket` makes. Used on branch names a forge
    /// reports (a workflow run's head branch, a pull request's head), which
    /// are not trusted to be ours until they parse.
    pub fn parse(_raw: &str) -> Option<Self> {
        todo!()
    }
    pub fn ticket(&self) -> TicketId {
        todo!("the id between the first two slashes")
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for BranchName {
    type Error = String;
    fn try_from(value: String) -> Result<Self, String> {
        Self::parse(&value).ok_or_else(|| format!("{value:?} is not a solos branch"))
    }
}
impl From<BranchName> for String {
    fn from(value: BranchName) -> String {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PrRef {
    pub repo: RepoName,
    pub number: u64,
}

// ---------------------------------------------------------------------------
// What the host observed of CI and of the pull request. Data, never orders.
// ---------------------------------------------------------------------------

/// CI at one head, as the host last read it from the forge. There is no
/// "absent": a repository with no CI says so in its terms (`ci: none`), and
/// then `Pending { reported: 0 }` never gates anything. That is a declaration,
/// not a timeout guess (see rationale: Open questions, path-filtered CI).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Checks {
    /// Something has not finished, or nothing has been reported yet.
    Pending { reported: u32 },
    Passed { checks: u32 },
    Failed { failed: Vec<FailedCheck> },
}

/// One failing check. Every string was clipped by the adapter and is data:
/// it reaches the worker quoted and labelled untrusted, never as an order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedCheck {
    pub name: String,
    pub url: String,
    pub summary: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mergeable {
    Clean,
    Conflicted,
    Blocked,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PrState {
    Open,
    Merged { commit: CommitSha },
    Closed,
}

// ---------------------------------------------------------------------------
// Terms: what an accept grants about the forge, fixed at accept like the
// budget. A config edit never changes the authority of work in flight.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    /// The agent never merges; people do, on GitHub.
    Off,
    /// A person's click on Slack/Discord, or an approving review on the pull
    /// request by an approver, at the current head.
    Ask,
    /// CI at the head passed (at least one check), the Ticket is untainted,
    /// and no approver said no: nothing else is asked. Reachable only
    /// through `ForgeTerms::for_requester`.
    Auto,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeTerms {
    pub merge: MergeMode,
    /// Whether the repository has CI the gate must wait for.
    pub ci_expected: bool,
    /// Heads the host will admit for one Ticket, repairs and review rounds
    /// included.
    pub max_pushes: u32,
    /// CI failures that may resume an ended Ticket.
    pub max_repairs: u32,
}

impl ForgeTerms {
    /// `Auto` is the owner's pre-approval of *their own* requests: it holds
    /// only when the requester may approve. Otherwise it is `Ask`. Called by
    /// `create_ticket`, so a Mandate can never carry an `Auto` the policy
    /// would not give this requester.
    pub fn for_requester(_configured: &ForgeTerms, _may_approve: bool) -> ForgeTerms {
        todo!("Auto -> Ask when the requester is not an approver")
    }
}

// ---------------------------------------------------------------------------
// The Change: one Ticket's one pull request. `Option<Change>` on Ticket, not a
// map: one deliverable, one PR. A stack would be a schema change, and that is
// a deliberate price (see rationale: Tradeoffs).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Named by the host at the first admitted `ready`; never changes.
    pub branch: BranchName,
    pub base: String,
    /// The latest head the host admitted from the worker.
    pub head: CommitSha,
    /// The worker's words for `head`, clipped. Kept here so the PR body and
    /// the Result say what was admitted, whatever later Reports say.
    pub text: String,
    /// What the forge holds, set only from the host's own read-back. `None`
    /// until the first Publish converges. `delivered.head != head` means a
    /// delivery is owed (derived, never a second flag).
    pub delivered: Option<Delivered>,
    /// CI at `delivered.head`.
    pub checks: Checks,
    pub state: ChangeState,
    pub pushes: u32,
    pub repairs: u32,
    /// A CI failure resumed the Ticket. Its next `ready` must carry a new head.
    pub repairing: bool,
    pub merge: MergeState,
    /// People's yes and no for `delivered.head`. Reset when it changes.
    pub verdicts: Verdicts,
    /// A decided "no" from the forge. Terminal for this Ticket: a person
    /// stops it and asks again. The agent never fights a forge. Both gates
    /// read it: `ready_gate` refuses, `merge_gate` waits (`Waiting::Held`).
    pub held: Option<Withheld>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
    pub pr: PrRef,
    pub head: CommitSha,
    pub url: String,
    pub at_ms: Ms,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ChangeState {
    Open,
    Merged { commit: CommitSha, at_ms: Ms },
    Closed { at_ms: Ms },
}

/// Who authorised a merge, recorded with `MergeStarted`. A person acted
/// through a Source's own control (a button, a review state); `Terms` is the
/// owner's `merge: auto`, fixed at accept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Grantor {
    Person { actor: Actor, source: String },
    Terms,
}

/// One approver's yes or no for one head. Only `merge_verdict` writes one,
/// after `policy.may_approve(source, actor)`; typed text never produces
/// one, because no code path leads from a Trigger to a verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub allow: bool,
    /// The control that carried it: `discord`, `slack` (a button) or
    /// `github` (a review state read by `Forge::observe`).
    pub source: String,
    pub at_ms: Ms,
}

/// The verdicts on one head, keyed by `source:actor`. A person's later
/// verdict replaces their earlier one (an approver who approves and then
/// requests changes withdraws the approval). Any approver's `no` holds the
/// merge: a second approver cannot launder the first (the pstack design's
/// amendment 3, for people instead of reviewers).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdicts {
    pub head: Option<CommitSha>,
    pub by: BTreeMap<String, Verdict>,
}

impl Verdicts {
    /// A yes from someone at `head`, and no no.
    pub fn granted(&self, _head: &CommitSha) -> Option<(&str, &Verdict)> {
        todo!()
    }
    pub fn denied(&self, _head: &CommitSha) -> bool {
        todo!()
    }
}

/// The merge's own progress at one head. Authority is not here: it is
/// `Verdicts` (people) or `ForgeTerms::merge == Auto` (the owner, at accept).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MergeState {
    Open,
    /// One `Ask::Merge` went out for this head.
    Asked { head: CommitSha, at_ms: Ms },
    /// The Merge Effect was committed for `head`.
    Merging { head: CommitSha, by: Grantor },
    /// The forge said no; told once per head. A new head resets it.
    Refused { head: CommitSha, why: String },
}

/// Why the forge would not take the work, decided (not a transport error).
/// A suspended or uninstalled App is not here: it is a wait (`RetryAfter`)
/// that the Effect queue repeats until the owner lifts it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum Withheld {
    /// Someone else pushed to the Ticket's branch (GitHub's "Update branch"
    /// button counts). The host never overwrites it.
    BranchMoved { found: CommitSha },
    /// A person closed the pull request; the host never reopens it.
    PrClosed,
    /// The forge refused the push or the pull request in words we keep
    /// (clipped): a ruleset on `solos/**`, a workflow file, a PR on our
    /// branch name without our marker.
    Forbidden { message: String },
    /// `github.writes: false`.
    ReadOnly,
}

// ---------------------------------------------------------------------------
// The `ready` gate (delivery.rs). Pure. First failure refuses; a refusal
// commits nothing and reaches the worker as the `worker_report` tool error
// (HTTP 409), so it corrects course in the same turn.
// ---------------------------------------------------------------------------

/// What the broker measured of the worker's tree and the forge adapter
/// admitted: only the broker builds one, from a sandbox the host ran. Never a
/// field of the model's tool arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Harvest {
    pub head: CommitSha,
    pub files: u32,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// The Ticket owes only an answer: `ready` is today's `ready`.
    Answer,
    /// `Deliverable::Change`: the head is admitted (so `ready` needs at least
    /// one commit) and the Result says so in a host line, "Committed <sha>
    /// (n files) in the Ticket's clone; not published". No Effect, the Ticket
    /// ends. The same gate, one fewer consequence.
    Committed(Harvest),
    /// A new head to deliver.
    Propose(Harvest),
    /// The worker answered (a reviewer's question) and changed nothing: the
    /// text is the reply, the head is as delivered.
    Reply,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotReady {
    /// The host has not admitted a head (the broker found no commits).
    NothingToDeliver,
    /// The previous proposal is still being delivered; wait for it.
    DeliveryInFlight { head: CommitSha },
    Held { why: Withheld },
    /// A CI repair round must end in a new commit, or in `blocked`.
    RepairNeedsCommit { failing: CommitSha },
    PushLimit { max: u32 },
    /// The pull request is merged or closed; new work is a new Ticket.
    Finished,
}

impl NotReady {
    /// The sentence the worker reads. Each says what to do next.
    pub fn tell(&self) -> String {
        todo!()
    }
}

/// `deliverable` comes from the Ticket's Plan; `terms` from its Mandate.
pub fn ready_gate(
    _ticket: &Ticket,
    _terms: Option<&ForgeTerms>,
    _harvest: Option<&Harvest>,
) -> Result<Admission, NotReady> {
    todo!(
        "Answer deliverable -> Admission::Answer. Change deliverable -> Committed(harvest)\n\
         (NothingToDeliver when none). PullRequest:\n\
         change.held -> Held; state != Open -> Finished;\n\
         delivered.head != head (a proposal is out) -> DeliveryInFlight;\n\
         harvest None -> NothingToDeliver;\n\
         harvest.head == change.head: repairing -> RepairNeedsCommit, else Reply;\n\
         pushes >= max_pushes -> PushLimit;\n\
         else Propose."
    )
}

// ---------------------------------------------------------------------------
// The merge gate (delivery.rs). Pure; evaluated at every fact that could
// change its answer: Observed::Seen, a verdict, a new delivery, the end of an
// Assignment, a stop. First failure is the reason it waits.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeGate {
    Go { head: CommitSha, by: Grantor },
    Waiting(Waiting),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waiting {
    /// `merge: off`, or the Mandate carries no forge terms.
    Off,
    /// A person stopped the Ticket: "stop" means stop.
    Stopped,
    NotDelivered,
    NotOpen,
    /// The Change is held (branch moved, PR closed, forbidden, read-only).
    /// Fixed by the synthesis: in B's sketch a held Change could still reach
    /// the gate.
    Held,
    /// A worker is live on the Ticket (a repair or a review round): it may
    /// be about to push, so nothing merges under it.
    Working,
    ChecksPending,
    ChecksFailed,
    /// An approver said no at this head. A new head resets it.
    Denied,
    /// CI is fine; nobody who may approve has said yes for this head.
    NoApproval,
    /// `merge: auto`, but the Ticket is tainted (`Ticket::tainted`), so a
    /// person decides. Asked like `NoApproval`; named apart so `agent
    /// status` can say why auto did not act.
    AutoWithheld,
    /// The Merge Effect is committed for this head.
    Merging,
    /// The forge refused this head; a new head resets it.
    Refused,
}

/// In order, first failure wins:
///
///  1. `mandate.forge.merge != Off`                        else `Off`
///  2. the Ticket is not stopped by a person               else `Stopped`
///  3. a Change, delivered, `state == Open`                else `NotDelivered` / `NotOpen`
///  4. `change.held.is_none()`                             else `Held`
///  5. `delivered.head == change.head` (nothing in flight) else `NotDelivered`
///  6. no live Assignment on the Ticket                    else `Working`
///  7. CI: `Passed { checks >= 1 }`, or `ci_expected == false`; `Pending`
///     and `Failed` wait                                   else `ChecksPending` / `ChecksFailed`
///  8. `!verdicts.denied(head)`                            else `Denied`
///  9. authority for *this head*: `verdicts.granted(head)` (the Grantor is
///     that person), or `Auto` with `checks >= 1` and `!ticket.tainted()`
///     (the Grantor is `Terms`)                            else `NoApproval` / `AutoWithheld`
/// 10. `merge` is not `Merging` or `Refused` at this head  else `Merging` / `Refused`
///
/// A verdict about another head counts for nothing: `verdicts.head` must be
/// the delivered head.
pub fn merge_gate(_ticket: &Ticket) -> MergeGate {
    todo!()
}

// ---------------------------------------------------------------------------
// What the host observed, applied by `Engine::forge`. One enum, one entry
// point. Every variant is idempotent: applied twice, the second changes
// nothing and appends no event.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observed {
    /// An Effect::Forge(Publish) converged: the forge holds `head` on the
    /// Ticket's branch and a ready pull request for it.
    Published {
        ticket: TicketId,
        head: CommitSha,
        pr: PrRef,
        url: String,
        /// The pull request's conversation, named by the adapter (the core
        /// does not know how a Source names Threads).
        thread: ThreadKey,
        reply_to: ChannelRef,
        created: bool,
    },
    Withheld {
        ticket: TicketId,
        head: CommitSha,
        why: Withheld,
    },
    /// A read of the pull request: CI, state, mergeability. Ownership is
    /// checked here (`branch` parses to a Ticket whose Change names this
    /// branch and PR); a head other than `delivered.head` is ignored.
    /// Review verdicts are not here: they are people's controls and arrive
    /// as `Command(Order::Merge)` (04_forge.rs `Look`).
    Seen(Seen),
    Merged {
        ticket: TicketId,
        head: CommitSha,
        commit: CommitSha,
    },
    MergeRefused {
        ticket: TicketId,
        head: CommitSha,
        why: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub pr: PrRef,
    pub branch: BranchName,
    pub head: CommitSha,
    pub state: PrState,
    pub checks: Checks,
    pub mergeable: Mergeable,
}

// ---------------------------------------------------------------------------
// Additions to model.rs.
// ---------------------------------------------------------------------------

/// `Mandate` gains `#[serde(default)] pub forge: Option<ForgeTerms>`, built at
/// accept from the repository's config and the requester (`for_requester`).
/// `Accept` gains the same field, filled by the broker's `verdict()` from
/// config, never from a model argument. `validate(accept)` refuses a
/// deliverable above `Answer` without terms.
///
/// `Ticket` gains `#[serde(default)] pub change: Option<Change>` and
/// `#[serde(default)] pub outsiders: BTreeSet<String>`: the `source:id` of
/// every actor the Policy does not admit as a person whose Trigger `receive`
/// stored on one of the Ticket's Threads (a stranger, a bot, a
/// `<login>#<id>` that failed its pin). Append-only, written in the same
/// transaction as the Trigger, which is already on the chain.
///
/// `ReportIn` gains `#[serde(default)] pub harvest: Option<Harvest>` (set by
/// the broker only; the tool's `ReportArgs` has `deny_unknown_fields` and no
/// such field).
pub struct ModelAdditions {
    pub _outsiders: BTreeSet<String>,
}

/// Graft from candidate A (`steered()`): `merge: auto` acts only for work
/// that nobody outside the Policy could steer.
pub trait TicketAdditions {
    /// The accept was `Inferred` (not an addressed request from an admitted
    /// person), or `outsiders` is not empty. Derived, never a flag.
    fn tainted(&self) -> bool;
    /// `change` is `Merged` or `Closed`. `may_resume` reads it.
    fn change_finished(&self) -> bool;
}

/// `Effect` gains `Forge(ForgeJob)`. It is committed in the same transaction
/// as the decision that owes it, so the commit *is* the write-ahead record:
/// there is no separate "asked" fact, and the runtime's retry (with
/// `repeat`) is the crash protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum ForgeJob {
    /// Make the forge hold `head` on the Ticket's branch, with a ready PR.
    Publish { ticket: TicketId, head: CommitSha },
    /// Merge the PR at exactly `head`, then read it back.
    Merge { ticket: TicketId, head: CommitSha },
}

/// A Ticket whose Change is `Merged` or `Closed` is not resumed by a follow-up
/// (`receive` skips it as `stopped_by_person` is skipped), so the follow-up
/// reaches the Router and, if it is new work, becomes a new Ticket with its own
/// branch. A review comment on a merged PR therefore costs one Router
/// judgement, not a worker.
///
/// `Stage` gains `Delivering`: the Ticket is open, no worker is live, and
/// `change.delivered.head != change.head`. `Ticket::stage` puts it after
/// Answer/Blocked/Failed and before Queued/Idle. `Runtime::idle` counts it as
/// owed work.
///
/// `MessageKind` gains `Status`: host-spoken progress ("CI passed on #13 at
/// 3f2a9c1", "Merged #13 as 7e1d0b2"). A Channel may drop it (the GitHub
/// channel does: GitHub already shows CI).
///
/// `Ask` gains `Merge { head: String }` (the short head; a Discord
/// `custom_id` carries it). `Ticket::awaits(Ask::Merge{head})` is true while
/// `change.merge == Asked { head }` and the Ticket is not stopped.
pub struct StageAndAsk;

/// `DomainEvent` gains these, all on the hash chain. `schema_version` goes to
/// V7 (PR 2), V8 (PR 3, `MessageKind::Status`) and V9 (PR 5, `Ask::Merge`,
/// `verdicts`, `outsiders`), each an empty migration so an older build
/// refuses the file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ForgeEvent {
    /// `ready` was accepted: this head will be delivered. `round` counts
    /// admitted heads (1 for the first PR, 2.. for repairs and review rounds).
    ProposalAdmitted { ticket: TicketId, head: CommitSha, files: u32, bytes: u64, round: u32 },
    Delivered { ticket: TicketId, head: CommitSha, pr: PrRef, created: bool },
    Withheld { ticket: TicketId, head: CommitSha, why: Withheld },
    /// Appended only when CI at the delivered head changed kind.
    ChecksObserved { ticket: TicketId, head: CommitSha, kind: String },
    RepairStarted { ticket: TicketId, head: CommitSha, n: u32 },
    ChangeEnded { ticket: TicketId, pr: PrRef, state: ChangeState },
    MergeAsked { ticket: TicketId, head: CommitSha },
    /// Appended only when the person's verdict at this head changed.
    MergeAnswered { ticket: TicketId, head: CommitSha, source: String, by: Actor, allow: bool },
    MergeStarted { ticket: TicketId, head: CommitSha, by: Grantor },
    Merged { ticket: TicketId, pr: PrRef, head: CommitSha, commit: CommitSha },
    MergeRefused { ticket: TicketId, head: CommitSha, why: String },
}

// ---------------------------------------------------------------------------
// Engine additions (engine.rs). Two public entry points beyond `report`.
// Each is one `run` transaction: state, Effects and events commit together.
// ---------------------------------------------------------------------------

/// Written as free signatures; they are methods of `Engine<R, P, C>`.
pub trait EngineAdditions {
    /// `report`, extended in place. For `ready` on a Ticket that owes more
    /// than an answer, *before anything is recorded*:
    ///
    /// ```text
    /// match ready_gate(&t, t.mandate.forge.as_ref(), report.harvest.as_ref())? {
    ///   Answer      => today's ready.
    ///   Propose(h)  => change = t.change.get_or_insert(Change::first(branch, base, h));
    ///                  change.admit(h, report.text, now)   // head, text, pushes += 1, repairing = false
    ///                  event ProposalAdmitted; effect Forge(Publish{ticket, h.head});
    ///                  end_assignment(..)                   // NOT end_ticket: stage = Delivering
    ///   Reply       => send Result(text + "PR <url> · head <sha> · unchanged"); end_ticket;
    ///                  then merge_gate (the worker is gone, a grant may now act).
    /// }
    /// ```
    /// A refusal is `Error::Refused(NotReady::tell())` and commits nothing.
    fn report_ready_for_change(&mut self);

    /// Apply one Observation (see `Observed`). In outline:
    ///
    /// * `Published`: require `change.head == head && delivered.head != head`
    ///   (else a repeat: Ok, nothing). Set `delivered`, `checks = Pending{0}`,
    ///   `merge = Open`, `verdicts = { head, by: {} }`. Subscribe the Ticket
    ///   to `thread` (first time). After sending the Result, set
    ///   `ticket.channels` to the old ones minus any channel of the same
    ///   `source` as `reply_to`, plus `reply_to`: the first Result reaches the
    ///   origin (the issue, the Discord thread), and from then on the PR
    ///   conversation supersedes an origin on the same Source, so a review
    ///   round does not answer twice on GitHub while a Discord origin keeps
    ///   hearing everything. Send Result = worker text + host line
    ///   `PR <url> · head <sha> · opened|updated`. End the Ticket
    ///   (Completed, "ready").
    /// * `Withheld`: `change.held = Some(why)`, `attention`, one Attention.
    /// * `Seen`: ownership + head checks; `change.checks = seen.checks`; PR
    ///   state to `ChangeState` (Merged/Closed end the Change, one Status).
    ///   Then by `checks`:
    ///   - `Failed` at the delivered head: `repair_or_attend` (below).
    ///   - `Passed`: one Status; evaluate `merge_gate`.
    ///   - `Mergeable::Conflicted`: treated as a repair reason with its own
    ///     Note ("conflicts with base; /host/base.bundle holds the tip").
    /// * `Merged` / `MergeRefused`: record, one Status/Attention per head.
    ///
    /// `repair_or_attend`: if the Ticket ended `Completed` by `Executor`, is
    /// not stopped by a person, `policy.revalidate(mandate)`, and
    /// `repairs < max_repairs && pushes < max_pushes`: `repairs += 1`,
    /// `repairing = true`, `ended = None`, event RepairStarted, one Note input
    /// (`from_allowed_person: false`) built from `FailedCheck`s and a pointer
    /// to `/host/ci/<head>.txt`, then `start`. If the Ticket is open with a
    /// live worker: just the Note. Otherwise: one Attention per head ("CI
    /// still failing after N repairs").
    fn forge(&mut self, observed: Observed) -> Result<(), crate::ports::Error>;

    /// A person's click (Slack/Discord) or GitHub review state (read by
    /// `Forge::observe`, sent by the github Source), already vouched for by
    /// the Source. Forbidden unless `policy.may_approve(source, by)`; the
    /// check runs before any state is read, as `approve` does, so a
    /// `<login>#<id>` actor (a failed pin) or a stranger is refused here.
    /// `head` is a prefix of at least 7 digits and must match
    /// `delivered.head` uniquely, else Refused ("that head is no longer the
    /// pull request's head"). Records the verdict in `verdicts.by`
    /// (`MergeAnswered` only when it changed, so the reconcile pass may send
    /// the same review every five minutes for free), then runs `merge_gate`;
    /// `Go` commits `Effect::Forge(Merge{head})`, `MergeStarted` and
    /// `Merging`. A stopped Ticket refuses.
    fn merge_verdict(
        &mut self,
        ticket: &TicketId,
        source: &str,
        by: &Actor,
        head: &str,
        allow: bool,
    ) -> Result<(), crate::ports::Error>;
}

/// `Engine::create_ticket` fills `Mandate.forge` through
/// `ForgeTerms::for_requester(accept.forge, policy.may_approve(source, requester))`.
/// `Engine::receive`, storing a Trigger on a Thread a Ticket subscribes to,
/// adds the actor to that Ticket's `outsiders` when the Policy does not
/// admit them as a person.
pub fn branch_key(ticket: &TicketId) -> String {
    key("br", &[ticket.as_str()])
}

// ---------------------------------------------------------------------------
// The tests that pin this module (crates/agent-core, no I/O):
//  - ready_gate: one row per NotReady, each alone; a Reply needs an unchanged
//    head and no repair; a Propose raises pushes; the same head twice is a
//    Reply, never a second Propose.
//  - merge_gate: each Waiting alone, in order; a verdict for another head
//    counts for nothing; a held Change never goes; a live worker waits and
//    the gate goes when its Assignment ends; an approver's yes then no at
//    one head is Denied; one approver's yes and another's no is Denied; a
//    new head resets verdicts; Auto needs >= 1 passed check; Auto on a
//    tainted Ticket is AutoWithheld; `ci_expected: false` needs only a yes.
//  - for_requester: Auto becomes Ask for a requester who may not approve.
//  - tainted: an Inferred accept is tainted; one outsider's Trigger on any
//    of the Ticket's Threads taints it for good; a host Note does not.
//  - Engine::forge: Published twice is one Delivered event; a Seen at an old
//    head changes nothing; a Seen for a branch no Ticket owns is Ok(()) and
//    appends nothing; Failed at the head resumes once, the (max_repairs+1)th
//    failure resumes nothing and asks once; a stopped Ticket never resumes.
//  - merge_verdict: the same verdict twice appends one MergeAnswered; a
//    non-approver is Forbidden and records nothing.
//  - BranchName: parse(for_ticket(..)) round-trips; `solos/x/../y`, uppercase,
//    spaces and a missing slug are refused; ticket() is the id inside.
// ---------------------------------------------------------------------------
