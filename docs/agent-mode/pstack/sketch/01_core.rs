//! Lives at: crates/agent-core/src/change.rs (new), plus additions to
//! model.rs (Plan, Mandate, Accept, Ticket, Ask, InputBody, DomainEvent) and
//! engine.rs (the commands below, and one gate inside `report`).
//!
//! The core learns three things about coding work and nothing about pstack:
//!
//! - **which repository a Ticket may change** (`Mandate.repo`). That is
//!   authority: the forge pushes and opens pull requests for that repository
//!   only, so it is fixed at accept time like the budget;
//! - **which pstack skill its worker enters** (`Plan::Session.method`), so a
//!   recovery launch enters the same one after a restart or a config change;
//! - **what the host saw**: the pull requests the Ticket published
//!   (`Ticket.changes`) and what proves each commit (`Ticket.proofs`: checks
//!   the host ran or saw the forge run, and reviews that delegates returned).
//!
//! Playbooks, phases, todo lists, model roles and families stay out. They are
//! pstack's text or the shell's config, so a new playbook or skill changes
//! nothing here. Everything below is written only from host observations,
//! never from model text: a worker cannot claim a check passed, it can only
//! ask the host to run one.
//!
//! Two gates live here because they are pure functions of the Ticket:
//! `ready` on a Ticket with an open pull request needs a passing proof at its
//! head ([`unproven`]), and a merge needs proof, CI, a review and a person
//! ([`merge_gate`]).

use crate::ids::key;
use crate::model::{Actor, AssignmentId, Ms, SecretGrant, ThreadKey, TicketId};
use crate::trail::Digest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Validated names. Each is parsed at the boundary (broker args, config,
// forge answers) and trusted inside, per boundary-discipline.
// ---------------------------------------------------------------------------

/// `owner/name`, as the forge names a repository. Same rule as
/// `github::valid_repo`, so a name cannot reach another API path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoName(String);

/// A full 40-digit lowercase hex commit id. Short ids are resolved by the
/// host in the sandbox before they reach the core.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommitSha(String);

/// A pstack skill a worker enters by name, such as `poteto-mode` or `how`:
/// 1 to 64 lowercase ASCII letters, digits and `-`. Whether it exists in the
/// vendored tree is the shell's check (`pstack::Tree::entry`), at config load
/// and at decide.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Entry(String);

/// A check the operator named for a repository, such as `unit` or `ci`:
/// 1 to 32 lowercase ASCII letters, digits and `-`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CheckName(String);

/// A branch in the Ticket's own namespace: `solos/<ticket>/<slug>`. The core
/// builds it from a slug, so a worker can never name `main` or a person's
/// branch.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BranchName(String);

impl BranchName {
    /// `solos/<ticket>/<slug>`, where `slug` is 1 to 40 of `[a-z0-9-]` and
    /// starts with a letter or digit.
    pub fn for_ticket(ticket: &TicketId, slug: &str) -> Result<Self, String> {
        todo!("validate slug; format solos/{{ticket}}/{{slug}}")
    }
    /// Whether this branch belongs to `ticket`'s namespace.
    pub fn owned_by(&self, ticket: &TicketId) -> bool {
        todo!()
    }
}

macro_rules! validated_string {
    ($($name:ident),*) => {$(
        impl TryFrom<String> for $name {
            type Error = String;
            fn try_from(value: String) -> Result<Self, String> {
                todo!(concat!("the ", stringify!($name), " rule above"))
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> String { value.0 }
        }
        impl $name {
            pub fn as_str(&self) -> &str { &self.0 }
        }
    )*};
}
validated_string!(RepoName, CommitSha, Entry, CheckName, BranchName);

impl CommitSha {
    /// The first 7 digits, for messages.
    pub fn short(&self) -> &str {
        &self.0[..7]
    }
}

/// One pull request: its repository and number.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PrRef {
    pub repo: RepoName,
    pub number: u64,
}

// ---------------------------------------------------------------------------
// Additions to model.rs, shown as the fields they add.
// ---------------------------------------------------------------------------

/// `Plan::Session` gains `method`:
///
/// ```ignore
/// Session {
///     harness: Option<String>,
///     environment: Option<String>,
///     /// The pstack skill the worker enters at every launch of this Ticket.
///     /// `None`: a plain worker, as before pstack. The broker fills it at
///     /// decide time from `pstack.entries` (the Router's choice, else the
///     /// first entry), so the Plan never depends on config read later.
///     #[serde(default, skip_serializing_if = "Option::is_none")]
///     method: Option<Entry>,
/// }
/// ```
///
/// `Mandate` and `Accept` gain `repo`:
///
/// ```ignore
/// /// The one repository this Ticket's worker may change through the forge.
/// /// `None`: no workspace, and no `check` or `pr_*` tool.
/// #[serde(default, skip_serializing_if = "Option::is_none")]
/// pub repo: Option<RepoName>,
/// ```
///
/// `Ticket` gains three maps, each written only by the commands below:
///
/// ```ignore
/// #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
/// pub changes: BTreeMap<PrRef, Change>,
/// #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
/// pub proofs: BTreeMap<CommitSha, Proof>,
/// /// A person's decision to merge one pull request, with the same three
/// /// states as a vault item's grant, final for the Ticket's life.
/// #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
/// pub merges: BTreeMap<PrRef, SecretGrant>,
/// ```
///
/// `Ask` gains `Merge { pr: PrRef }` (Merge and Don't merge buttons), and
/// `InputBody` gains `Merge { pr: PrRef, granted: bool, by: Actor }`, a
/// click and never text. Each PR that adds one of these bumps the store's
/// schema version with an empty migration, as V5 did for secrets, so an
/// older build refuses the file instead of dropping the fields when it
/// rewrites a Ticket.
pub struct ModelAdditions;

/// A pull request the Ticket published, as the forge last answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// The pull request's own Thread, such as `github:acme/widgets#61`. The
    /// Ticket subscribes to it on first sight, so comments and CI Triggers
    /// on the pull request reach the Ticket.
    pub thread: ThreadKey,
    pub branch: BranchName,
    /// The branch it targets: the repository's base or another of this
    /// Ticket's branches (a stack).
    pub base: String,
    pub head: CommitSha,
    pub url: String,
    pub state: ChangeState,
    /// The forge's CI at `head`. A new head resets it to `Pending`.
    pub ci: Ci,
    pub seen_ms: Ms,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ChangeState {
    Open,
    Merged { commit: CommitSha, at_ms: Ms },
    /// Closed without a merge, by anyone.
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Ci {
    /// No check has ended yet, or some still run.
    Pending,
    Passed { checks: u32 },
    Failed { failed: Vec<String> },
    /// The repository runs no CI on this head.
    None,
}

impl Ci {
    pub fn terminal(&self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// What proves one commit. Keyed by commit, not by pull request, so a check
/// a delegate ran in its fork counts once the lead publishes the same commit,
/// and a proof never follows a branch to a commit it did not see.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proof {
    /// The latest run of each check on this commit.
    pub checks: BTreeMap<CheckName, CheckRun>,
    pub reviews: Vec<Review>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    pub passed: bool,
    pub source: CheckSource,
    pub at_ms: Ms,
    /// The Assignment whose seat asked for it, or whose delegate did.
    pub assignment: AssignmentId,
    /// The delegate that asked, if one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckSource {
    /// The host ran the operator's command on a clean checkout of the commit.
    Host {
        exit_code: i32,
        duration_ms: u64,
        /// The log's digest; the log itself is in the Ticket's directory.
        log: Digest,
    },
    /// The operator named the forge's CI as this check, and the host saw it
    /// end at this commit.
    Forge,
}

/// A delegate's verdict on a commit, returned through `delegate_return` by a
/// delegate the lead started with `verifies`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub delegate: String,
    /// The model alias it ran on, and that model's family, from the host's
    /// model table. Data to the core: it compares families, never names them.
    pub model: String,
    pub family: String,
    pub outcome: ReviewOutcome,
    pub at_ms: Ms,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOutcome {
    /// pstack's PASS and PASS+NOTES.
    Pass,
    /// pstack's FAIL and swarm's ISSUES.
    Fail,
    /// swarm's BLOCKED, or a delegate that ended without a verdict.
    Inconclusive,
}

// ---------------------------------------------------------------------------
// Inputs to the Engine commands.
// ---------------------------------------------------------------------------

/// What the host is about to push, written before the push.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishAsk {
    pub repo: RepoName,
    pub branch: BranchName,
    pub head: CommitSha,
    pub base: String,
}

/// The shell's terms for a merge, from the repository's config and the
/// host's model table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeTerms {
    /// `forge.repos.<repo>.merge: auto`: no person's click is needed. The
    /// other conditions still hold.
    pub auto: bool,
    /// The lead's family, when the host has another family a reviewer could
    /// run on. Then a review counts only from a different family. `None`:
    /// one family, so any delegate's review counts (pstack's own fallback).
    pub other_than: Option<String>,
    /// `source:id`s who may click; empty: the Policy's approvers.
    pub answerable_by: Vec<String>,
    /// The label on the ask, from the host, never the model.
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeGate {
    /// Merge `head`, and nothing else: the forge's merge call names it, so a
    /// push after the gate makes the merge fail instead of landing it.
    Go { head: CommitSha },
    /// A person was asked once; their click comes back as an Input.
    Waiting,
}

/// A delegate the lead started, for the record. No state changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegated {
    pub name: String,
    pub model: String,
    pub family: String,
    /// `read` or `fork`.
    pub workspace: String,
    /// The pstack role it fills, as the lead named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifies: Option<PrRef>,
    pub max_microusd: u64,
}

/// What a session entered, for provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entered {
    pub entry: Entry,
    /// pstack's `plugin.json` version, such as `0.15.36`.
    pub version: String,
    /// The vendored tree's digest from its MANIFEST.
    pub tree: Digest,
    /// The adapter compiled into this binary.
    pub adapter: Digest,
}

// ---------------------------------------------------------------------------
// DomainEvent additions.
// ---------------------------------------------------------------------------

/// ```ignore
/// MethodEntered { ticket, assignment, entered: Entered },
/// /// Written before the push leaves, as KeyUsed is before a keyed fetch.
/// PublishAsked { ticket, assignment, ask: PublishAsk },
/// PrSeen { ticket, pr: PrRef, head: CommitSha, state: ChangeState, ci: Ci },
/// Checked { ticket, commit: CommitSha, check: CheckName, passed: bool },
/// Reviewed { ticket, commit: CommitSha, delegate: String, family: String, outcome: ReviewOutcome },
/// Delegated { ticket, assignment, delegated: Delegated },
/// MergeAsked { ticket, pr: PrRef, head: CommitSha },
/// MergeAnswered { ticket, pr: PrRef, by: Actor, granted: bool },
/// ```
pub struct EventAdditions;

// ---------------------------------------------------------------------------
// Engine commands (engine.rs). Each runs in one transaction, like the rest.
// ---------------------------------------------------------------------------

/// Signatures as they appear on `impl<R, P, C> Engine<R, P, C>`.
pub trait CodingCommands {
    /// Record which skill and which pstack a session entered. Event only.
    /// Refused unless `assignment` is the running one.
    fn entered(&mut self, ticket: &TicketId, assignment: &AssignmentId, entered: Entered)
    -> Result<(), crate::Error>;

    /// The host is about to push `ask.head` to `ask.branch`. Recorded first,
    /// so a push never goes unrecorded. Refused unless `assignment` is the
    /// running one, the Mandate names `ask.repo`, and the branch is in the
    /// Ticket's namespace. A repeat of the same ask records it again (the
    /// push after it is idempotent at the forge).
    fn publish_ask(&mut self, ticket: &TicketId, assignment: &AssignmentId, ask: PublishAsk)
    -> Result<(), crate::Error>;

    /// The forge answered a publish, a look or a merge, or the CI watcher saw
    /// a change. Upserts the Change. On first sight the Ticket subscribes to
    /// `change.thread`. A new head resets CI to the forge's word for that
    /// head. When CI at the head becomes terminal for the first time and the
    /// Ticket is open, the worker gets a Note in the same transaction
    /// ("CI on #61 at 3f2a9c1: 9 passed."). The same observation twice
    /// changes nothing and sends nothing, so the watcher can repeat freely.
    /// Allowed on an ended Ticket (the PR still exists) but adds no Note.
    fn pr_seen(&mut self, ticket: &TicketId, pr: PrRef, change: Change)
    -> Result<(), crate::Error>;

    /// The host saw check `check` end on `commit`. Replaces that check's
    /// earlier run on the commit. Allowed on an ended Ticket: the run
    /// happened.
    fn checked(&mut self, ticket: &TicketId, commit: CommitSha, check: CheckName, run: CheckRun)
    -> Result<(), crate::Error>;

    /// A delegate started with `verifies` returned its verdict on `commit`,
    /// the head it was bound to when it started.
    fn reviewed(&mut self, ticket: &TicketId, commit: CommitSha, review: Review)
    -> Result<(), crate::Error>;

    /// A delegate started. Event only, for the audit of who did what.
    fn delegated(&mut self, ticket: &TicketId, assignment: &AssignmentId, delegated: Delegated)
    -> Result<(), crate::Error>;

    /// Whether the worker may merge `pr` now. Runs [`merge_gate`]. When only
    /// a person's click is missing, it asks once (Attention with
    /// `Ask::Merge`) and returns `Waiting`; a repeat while it waits commits
    /// nothing. A Denied grant refuses for the Ticket's life.
    fn merge_ask(&mut self, ticket: &TicketId, assignment: &AssignmentId, pr: &PrRef, terms: &MergeTerms)
    -> Result<MergeGate, crate::Error>;

    /// A person's Merge or Don't merge click, from a Source's own controls.
    /// Same rules as `answer_secret`. The answer reaches the worker as
    /// `InputBody::Merge`.
    fn answer_merge(&mut self, ticket: &TicketId, pr: &PrRef, source: &str, by: Actor, allow: bool)
    -> Result<(), crate::Error>;
}

/// The gate inside `Engine::report`, before a `ready` Report is pushed:
///
/// ```ignore
/// if report.status == ReportStatus::Ready
///     && let Some(why) = change::unproven(&t)
/// {
///     return Err(Error::Refused(why));
/// }
/// // ...
/// ReportStatus::Ready => {
///     let text = change::result_text(&report.text, &t);
///     tx.send(&mut t, MessageKind::Result, text, None);
///     tx.end_ticket(&mut t, EndedBy::Executor, TicketOutcome::Completed, "ready");
/// }
/// ```
///
/// The refusal reaches the model as `worker_report`'s tool error, the house
/// pattern for a refused call, so the worker corrects course instead of the
/// person getting an unverified result.
pub struct ReportGate;

/// Why `ready` would not stand, or `None` when it would. Every open pull
/// request needs a passing check on its current head. A Ticket that
/// published nothing (an investigation, an answer) is unconditioned.
pub fn unproven(ticket: &crate::model::Ticket) -> Option<String> {
    // TODO: for each (pr, change) in ticket.changes with state Open:
    //   proofs[change.head].checks has any run with passed == true, else
    //   collect "#61 (3f2a9c1)". Message:
    //   "ready needs a passing check at the head of each open pull request:
    //    #61 (3f2a9c1) has none. Run check, or report waiting with what is
    //    missing."
    todo!()
}

/// The person's result, then one line per pull request built from state the
/// host recorded, so a link, a head or a verdict in it is never the model's
/// claim: `PR https://github.com/acme/widgets/pull/61 · head 3f2a9c1 ·
/// unit passed · CI passed`. A Ticket whose Mandate names a repository but
/// that published nothing ends with `No pull request was published.`, so a
/// link the model wrote into its own text cannot pass for a real one. A
/// Ticket with no repository gets nothing appended.
pub fn result_text(text: &str, ticket: &crate::model::Ticket) -> String {
    todo!()
}

/// The merge rule, pure. In order, the first that fails is the refusal:
///
/// 1. the Ticket published `pr` and it is open;
/// 2. the forge's CI at its head is `Passed` or `None`;
/// 3. a passing check at its head;
/// 4. a `Pass` review at its head, from a family other than
///    `terms.other_than` when that is set (pstack's "a verifier that isn't
///    its author, on a different model family");
/// 5. a person's grant, unless `terms.auto`. A Denied grant refuses.
///
/// Returns `Ok(Go)`, `Ok(Waiting)` when only 5 is missing and no Denied
/// grant exists, or `Err(reason)`.
pub fn merge_gate(
    ticket: &crate::model::Ticket,
    pr: &PrRef,
    terms: &MergeTerms,
) -> Result<MergeGate, String> {
    todo!()
}

/// The Note a terminal CI result sends, from the Change alone.
fn ci_note(pr: &PrRef, change: &Change) -> String {
    todo!()
}

/// An id for the merge ask's message, stable per (Ticket, pr, head), so a
/// head that moves asks again and a repeat does not.
fn merge_message_key(ticket: &TicketId, pr: &PrRef, head: &CommitSha) -> String {
    key("mg", &[ticket.as_str(), pr.repo.as_str(), &pr.number.to_string(), head.as_str()])
}

// Tests (crates/agent-core/tests/change.rs and engine unit tests):
//
// - names: RepoName refuses `a/../b`, `a`, `a/b/c`; CommitSha refuses 39 or
//   41 digits and uppercase; BranchName::for_ticket refuses `-x`, `X`, a
//   slash, 41 characters; `owned_by` is false for another Ticket's branch.
// - publish_ask is refused for a repo the Mandate does not name, for a
//   branch of another Ticket, and for an Assignment that is not running.
// - pr_seen subscribes the Ticket to the PR Thread once; a second identical
//   observation commits no change and sends no Note; a new head resets CI;
//   CI terminal at the head sends one Note; CI terminal at an old head sends
//   none; an ended Ticket records but gets no Note.
// - report(ready) with an open PR and no proof is refused, commits nothing,
//   and leaves the Ticket open; with a passing check at the head it ends the
//   Ticket and the Result holds the PR line; a check at an older head does
//   not count; a merged or closed PR does not need one; a Ticket that
//   published nothing is unconditioned.
// - checked replaces the earlier run of the same check on a commit; a check
//   run by a delegate on a commit the lead later publishes counts.
// - merge_gate: each of the five conditions alone refuses with its reason;
//   other_than refuses a same-family review and accepts another family's;
//   auto skips the click but nothing else; Denied refuses for good.
// - merge_ask asks once per head: a repeat while waiting commits nothing, a
//   new head asks again; answer_merge by someone not answerable is refused;
//   typed text never becomes InputBody::Merge (there is no path).
// - The seeded fuzzer gains the new commands, and the differential walk
//   between MemoryRepositories and SqliteStore covers the new fields.
