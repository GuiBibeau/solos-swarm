//! Lives at: src/agent/broker.rs and src/agent/broker/{pstack,forge}.rs
//! (new handler modules, as browser.rs and secrets.rs are), plus
//! src/agent/ports.rs (`Fact`, `Order`), src/agent/runtime.rs (applying the
//! new facts) and src/agent/environment.rs (a seat's view of the sandbox).
//!
//! The handlers are where the deep tools hide their work. Each one is a
//! short sequence over ports the broker already has (the environment, the
//! fact sink) plus the new `Forge` and `Delegates`. None of them trusts a
//! model's claim about state: what the Engine records comes from a sandbox
//! the host ran, the forge's answer, or the ledger.

use super::delegate::Delegates;
use super::pstack::{Reader, Tree};
use crate::agent::forge::Forge;
use serde_json::Value;
use solos_agent_core::change::{
    BranchName, Change, CheckName, CheckRun, CommitSha, Delegated, Entered, MergeTerms, PrRef,
    PublishAsk, Review,
};
use solos_agent_core::model::{AssignmentId, TicketId};
use std::path::PathBuf;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// New facts (src/agent/ports.rs), each applied by the runtime as the Engine
// command of the same name. The "asked" ones wait for the Engine's answer, so
// the outside action happens only after the commit.
// ---------------------------------------------------------------------------

pub enum FactAdditions {
    /// At session start, before the first prompt.
    Entered { ticket: TicketId, assignment: AssignmentId, entered: Entered },
    /// Asked: the push waits for it.
    PublishAsk { ticket: TicketId, assignment: AssignmentId, ask: PublishAsk },
    PrSeen { ticket: TicketId, pr: PrRef, change: Change },
    /// Asked: the worker sees the check's end only after it is recorded.
    Checked { ticket: TicketId, commit: CommitSha, check: CheckName, run: CheckRun },
    Reviewed { ticket: TicketId, commit: CommitSha, review: Review },
    /// Asked: a delegate spends only after it is on the chain.
    Delegated { ticket: TicketId, assignment: AssignmentId, delegated: Delegated },
    /// Asked: answers `Applied::Merge(MergeGate)`.
    MergeAsk { ticket: TicketId, assignment: AssignmentId, pr: PrRef, terms: MergeTerms },
}

/// `Order` gains `Merge { pr: PrRef, allow: bool }`: the Merge and Don't
/// merge buttons on Slack and Discord, parsed as the vault's Allow and Deny
/// are, and applied as `Engine::answer_merge`.
pub struct OrderAdditions;

// ---------------------------------------------------------------------------
// A seat's view of the sandbox (src/agent/environment.rs).
// ---------------------------------------------------------------------------

/// `Work` gains the seat's view: what its `run_code` and jobs mount besides
/// `/work`, at the same virtual paths `read` serves. `Local` passes it to
/// `CodeExecutor::start_in`, which adds the binds after the operator's
/// mounts; a remote node ignores it, since a Ticket with a repository or a
/// method's delegates is local-only in this build.
pub struct View {
    /// `/work` is read-only: a `read` delegate's view of its lead's tree.
    pub work_read_only: bool,
    /// Read-only binds: `/pstack`, `/host`, `/children` for a lead,
    /// `/method` for a delegate.
    pub read_only: Vec<(&'static str, PathBuf)>,
    /// Writable binds: `/method` for a lead; for one publish or check call
    /// only, a fresh `/out` or `/checks`.
    pub writable: Vec<(&'static str, PathBuf)>,
    /// Single files bound read-only, such as this Ticket's transcripts at
    /// `/host/transcripts/<assignment>.jsonl`.
    pub files: Vec<(PathBuf, PathBuf)>,
}

// ---------------------------------------------------------------------------
// The handlers.
// ---------------------------------------------------------------------------

pub struct Handlers {
    tree: Option<Arc<Tree>>,
    forge: Option<Arc<dyn Forge>>,
    delegates: Option<Arc<dyn DelegateStarter>>,
}

/// `Delegates<H>` behind an object, so the broker stays free of the harness
/// type parameter.
pub trait DelegateStarter: Send + Sync {
    fn start(&self, lead: &Arc<super::broker::WorkerSeat>, args: Value)
    -> crate::agent::ports::BoxFuture<'_, std::result::Result<Value, super::jobs::Refused>>;
}

/// `read`: the seat's `Reader` over its roots. Pure file access on the
/// host, with the kernel enforcing the roots (03_pstack.rs).
pub async fn read(seat: &super::broker::WorkerSeat, args: Value) -> Value {
    todo!()
}

/// `check {name, commit?}`.
///
/// 1. Look `name` up in the repository's checks; an unknown one is refused
///    with the list.
/// 2. A `forge` check (`ci`) runs nothing: it says CI at that commit is
///    recorded when the watcher sees it end. `pr_seen` records it as a
///    `CheckSource::Forge` run.
/// 3. A `command` check starts a `JobKind::Check` in the seat's environment
///    with `/work` bound read-only (the code under test runs in this
///    sandbox, and must not be able to change the tree it is judged on), a
///    writable `/checks` (the Ticket's check cache, which no plain
///    `run_code` mounts), and this fixed host script around the operator's
///    command:
///
///    ```sh
///    set -eu
///    commit=$(git -C /work rev-parse --verify "$COMMIT^{commit}")
///    uncommitted=$(git -C /work status --porcelain)
///    rm -rf /checks/src && git clone --quiet --shared --no-checkout /work /checks/src
///    git -C /checks/src -c advice.detachedHead=false checkout --quiet --detach "$commit"
///    cd /checks/src && exec bash -c "$CHECK"
///    ```
///
///    The check runs on the committed tree of that commit, not the working
///    tree, so uncommitted edits, `assume-unchanged` bits or an excludes
///    file cannot make a check pass on code the commit does not hold. Build
///    caches live in `/checks/cache` (the operator's command points
///    `CARGO_TARGET_DIR` or the like there).
/// 4. When the job ends: write its log to `<ticket>/host/checks/`, then
///    `facts.ask(Fact::Checked)`, then hand the end to the worker. The
///    record is on the chain before the model sees "passed".
pub async fn check(seat: &super::broker::WorkerSeat, args: Value) -> Value {
    todo!()
}

/// `pr_publish {branch, title, body, base?, commit?}`.
///
/// 1. `BranchName::for_ticket(ticket, branch)`; `base` is the repository's
///    base or a branch in `ticket.changes`.
/// 2. In a sandbox with a fresh, empty `/out` bound writable for this call
///    only: resolve the commit, write `/out/publish.bundle` holding
///    `<base_commit>..<commit>`. The host opens it with `O_NOFOLLOW`, checks
///    it is a regular file under the size limit, and copies it into the
///    mirror's directory before git reads it.
/// 3. `facts.ask(Fact::PublishAsk)`: refused unless the Mandate names the
///    repository; recorded before anything leaves.
/// 4. `forge.publish(..)` with the lease set to the Change's last head.
/// 5. `facts.ask(Fact::PrSeen)`, then reply.
///
/// A refusal at 4 is the worker's to fix and is its tool error.
pub async fn pr_publish(seat: &super::broker::WorkerSeat, args: Value) -> Value {
    todo!()
}

/// `pr_view {pr}`: `forge.view(pr, Some(<ticket>/host/base.bundle))`. For a
/// pull request this Ticket published, it also sends `Fact::PrSeen` and
/// adds the recorded proofs and the verdict. The verdict is pstack's
/// watch-pr vocabulary: READY (open, clean, CI passed, no unresolved
/// threads), WAITING (CI pending), BLOCKED (conflicted, or threads open),
/// FAILED (CI failed), MERGED, CLOSED.
pub async fn pr_view(seat: &super::broker::WorkerSeat, args: Value) -> Value {
    todo!()
}

/// `pr_reply {pr, thread?, body}`: only on a pull request this Ticket
/// published. The marker is `MessageId(key("pr", [ticket, pr, thread,
/// body]))`, so the same reply twice posts once.
pub async fn pr_reply(seat: &super::broker::WorkerSeat, args: Value) -> Value {
    todo!()
}

/// `pr_merge {pr, method?}`.
///
/// 1. `forge.view(pr)` and `Fact::PrSeen`, so the gate judges the forge's
///    current head and CI, not a stale record.
/// 2. `facts.ask(Fact::MergeAsk)` with the repository's `MergeTerms`; the
///    Engine runs `merge_gate`. `Waiting` becomes the tool error that tells
///    the worker to end its turn; a refusal names what is missing.
/// 3. On `Go { head }`: `forge.merge(pr, head, method)`, then
///    `Fact::PrSeen` with the merged state, then reply.
pub async fn pr_merge(seat: &super::broker::WorkerSeat, args: Value) -> Value {
    todo!()
}

/// `delegate_return {status, text}`: only a delegate seat. Hands the report
/// to its `Delegates::run` task, which ends the delegate. A second call is
/// refused: "you have returned; stop here".
pub async fn delegate_return(seat: &super::broker::WorkerSeat, args: Value) -> Value {
    todo!()
}

// The `tool()` dispatch gains, after its existing arms:
//
//   (Principal::Worker(seat), "read") => pstack::read(seat, args),
//   (Principal::Worker(seat), "delegate") => delegates.start(seat, args),
//   (Principal::Worker(seat), "delegate_return") => pstack::delegate_return(seat, args),
//   (Principal::Worker(seat), "check") => forge::check(&broker, seat, args),
//   (Principal::Worker(seat), tool @ ("pr_publish" | "pr_view" | "pr_reply" | "pr_merge"))
//       => forge::call(&broker, seat, tool, args),
//
// and the generic refusal before it (`unmet_need`) also checks the seat
// kind against the table's `seats`, so a delegate calling `pr_publish` gets
// "tool outside admitted grants" from the same place as today.

// `worker_report`'s handler is unchanged: the gate is the Engine's, and its
// refusal comes back through the existing `Err(why) => 409` arm. Its reply's
// `next` for `ready` stays "Sent to whoever asked; the ticket has ended."
// `after(Waiting)` with a question gains nothing: a question on `waiting`
// already raises Attention without holding automatic Inputs
// (engine.rs:409-413), which is why the adapter maps AskQuestion to
// `waiting` plus `question`, not to `blocked`.

// Tests (tests/agent_broker.rs additions, offline): each handler's refusal
// texts above; `check` records before the worker sees the end (a fact sink
// that delays the answer shows the tool reply waiting on it); `pr_publish`
// with the Engine refusing PublishAsk pushes nothing to the LocalForge;
// `pr_merge` with the head moved between gate and merge merges nothing and
// says so; a delegate seat calling any lead tool is refused by the generic
// gate, not by the handler.
