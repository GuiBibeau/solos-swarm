//! Lives at: src/agent/delegate.rs (new), plus changes to src/agent/jobs.rs
//! (a delegate is a kind of job), src/agent/broker.rs (`Account`, the child
//! seat) and crates/agent-store/src/ledger.rs (a part cap on a hold).
//!
//! A delegate is pstack's `Task`: a separate model session the lead starts in
//! the background, on a model of its choosing from the rule, with its own
//! workspace and budget, whose report comes back to the lead. It is not a
//! Ticket and not an Assignment. The Engine's one-live-Assignment rule, the
//! Router's authority and the Mandate stay as they are: like a swarm's agents
//! today, delegates are invisible to the Engine except as events and as
//! spend on the Ticket's account.
//!
//! Four rules make it safe to fan out:
//!
//! 1. **Depth one.** A delegate's seat is never offered `delegate`
//!    (`Seats::Lead`). pstack already handles that: "A subagent forbidden to
//!    spawn satisfies this by owning the diff directly" (feature.md step 4),
//!    and every fan-out skill has the lead spawn. So no delegate ever waits
//!    on a delegate, and there is no cycle to deadlock.
//! 2. **Its own pool.** Delegates take slots from `delegates.max_live`, never
//!    from `max_workers`. A lead keeps its worker slot while it waits, as it
//!    does for a job, and its delegates never need one. With depth one this
//!    cannot deadlock: the only thing waiting on a delegate slot is a lead,
//!    and leads never hold one.
//! 3. **The Ticket's account.** A delegate's model calls are held on the
//!    Ticket's own ledger account, so the Mandate's budget caps lead and
//!    delegates together with no new accounting, and each hold also carries
//!    a part key with the delegate's own cap, checked in the same
//!    transaction. The Router's daily cap is a different account and is
//!    never touched.
//! 4. **Bounded and owned.** A delegate belongs to the Assignment that
//!    started it and dies with it, like a job. Each has a wall limit and a
//!    spend cap; an Assignment may start at most `per_assignment` of them.

use super::broker::{AgentBroker, ModelTerms, WorkerSeat};
use super::jobs::Refused;
use super::pstack::{Alias, Models, Tree};
use crate::harness::AgentHarness;
use solos_agent_core::change::{CommitSha, PrRef};
use solos_agent_core::model::{MicroUsd, TicketId};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// `pstack.delegates` in agent.yaml.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Delegate sessions live at once on this host. Default 4.
    pub max_live: usize,
    /// Delegates one Assignment may start in its life. Default 24: a
    /// Feature run (how, architect's arena, the delegate, interrogate) is
    /// about a dozen.
    pub per_assignment: usize,
    /// The default and the most one delegate may spend. Default $0.40.
    pub max_microusd: MicroUsd,
    /// The default and the longest one may run. Default 1 hour.
    pub timeout: Duration,
    /// While delegates run and the lead is between turns, the host reports
    /// `progress` this often on the lead's behalf, so the Watchdog's
    /// 30-minute stall rule (watchdog.rs:423-462) sees work, not silence.
    /// Default 10 minutes.
    ///
    /// The lead's latest progress text is its todo list (the adapter maps
    /// pstack's todolist there), so every progress report the host makes,
    /// this heartbeat and the existing quiet-turn `JOB_TURN` report alike,
    /// repeats the lead's last own progress text and adds one host line
    /// after it: `(host) Delegates running: explore-1 for 12 min.` The seat
    /// keeps that text (`WorkerSeat::last_progress`, set by the
    /// `worker_report` handler), so the latest report always holds the list.
    pub heartbeat: Duration,
}

/// `delegate`'s arguments, parsed at the boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// `[a-z0-9][a-z0-9-]{0,39}`, unique per Assignment. The idempotency
    /// key: a repeat returns the same delegate.
    pub name: String,
    pub brief: String,
    pub agent: AgentKind,
    /// `None`: the lead's alias.
    pub model: Option<String>,
    pub role: Option<String>,
    pub workspace: Workspace,
    pub group: Option<String>,
    pub verifies: Option<u64>,
    pub max_microusd: Option<MicroUsd>,
    pub timeout: Option<Duration>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentKind {
    /// pstack's `generalPurpose`: the brief alone.
    General,
    /// A definition in `/pstack/agents`, such as `poteto-agent`. Its body
    /// leads the brief; for `poteto-agent`, so does poteto-mode's SKILL.md,
    /// which the definition says to read in full first.
    Defined(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Workspace {
    /// The lead's `/work`, bound read-only into the delegate's `run_code`.
    /// pstack's `readonly: true`: enforced by the mount, not asked of the
    /// model. It sees the lead's uncommitted changes as they are.
    Read,
    /// Its own writable copy at `<ticket>/children/<name>/work`: a clone of
    /// the lead's repository at HEAD with the lead's uncommitted changes
    /// applied (`git diff --binary HEAD | git apply`), so interrogate's
    /// "copy the scope's uncommitted changes into each worktree" holds. The
    /// copy is made inside a run_code sandbox with the lead's work bound
    /// read-only at /parent, because the host never runs git in a
    /// repository a sandbox could write.
    Fork,
}

/// The delegate's end, as job_wait returns it and its Note tells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct End {
    pub name: String,
    pub outcome: Outcome,
    /// `<ticket>/children/<name>/report.md`, written by the host.
    pub report: PathBuf,
    /// Its /work HEAD and `git status --porcelain` paths, read in a sandbox.
    pub head: Option<CommitSha>,
    pub dirty: Vec<String>,
    pub spent_microusd: MicroUsd,
    pub alias: String,
    pub family: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Its `delegate_return` status.
    Pass,
    Issues,
    Blocked,
    Done,
    /// Its turn ended twice without `delegate_return`.
    NoReport,
    TimedOut,
    /// Its part cap or the Ticket's budget refused a model call.
    OverBudget,
    /// `job_stop`, or its Assignment ended.
    Stopped,
}

/// Every delegate on this host. Built by `PiParts::new` with the workers'
/// harness and handed to the broker, which calls [`Delegates::start`] from
/// the `delegate` handler.
pub struct Delegates<H> {
    harness: Arc<H>,
    broker: AgentBroker,
    tree: Arc<Tree>,
    models: Arc<Models>,
    slots: Arc<Semaphore>,
    limits: Limits,
    tickets_dir: PathBuf,
}

impl<H> Delegates<H>
where
    H: AgentHarness + Send + Sync + 'static,
    H::Session: Send + 'static,
{
    /// Start `request` for `lead`, or return the delegate already started
    /// under its name.
    ///
    /// 1. Validate against the lead seat and the host: the model alias (the
    ///    refusal lists the valid ones), the agent, `verifies` (a pull
    ///    request of this Ticket, open, and `workspace: read`; the head is
    ///    bound now, so a later push does not move what it verifies), the
    ///    per-assignment count, and the cap: the lesser of `max_microusd`,
    ///    `limits.max_microusd` and what the Ticket has left.
    /// 2. Register it in the lead's job table as `JobKind::Delegate`, with
    ///    its group, so its end reaches the lead once (jobs.rs's rule).
    /// 3. `facts.ask(Fact::Delegated)` so the event is on the chain before
    ///    it spends.
    /// 4. Spawn [`Delegates::run`]; return `{job, status: queued|running}`.
    pub async fn start(&self, lead: &Arc<WorkerSeat>, request: Request) -> Result<serde_json::Value, Refused> {
        todo!()
    }

    /// One delegate's life, on its own task:
    ///
    /// - take a delegate slot (status `queued` until it has one);
    /// - make its workspace (`Fork` runs the copy in a sandbox) and
    ///   `<ticket>/children/<name>/`, which holds only `work/` and the
    ///   host's `report.md`. Its session's own files (transcript, stderr,
    ///   settings) go to `<ticket>/runs/<name>/`, which no sandbox mounts;
    ///   its transcript is bound read-only, per file, at
    ///   `/host/transcripts/<name>.jsonl`, so a reviewer can read what it
    ///   did and not only what it said;
    /// - admit a child seat: `WorkerSeat::delegate(lead, name)` on the
    ///   alias's `ModelTerms`, account `Ticket { part: Some((name, cap)) }`,
    ///   grants `offered_to(Worker, caps, Seat::Delegate)`, and the system
    ///   prompt `system_prompt + pstack::section(.., Seat::Delegate)`;
    /// - start the session on the workers' Pi harness with that alias's wire
    ///   model, and prompt the host preamble, the agent definition (and
    ///   poteto-mode for poteto-agent, entered as `Tree::enter` does) and the
    ///   brief;
    /// - drive turns until `delegate_return`. A turn that ends without one
    ///   gets one prompt, "Call delegate_return with your report.", then the
    ///   delegate ends `NoReport` with its last progress text;
    /// - on return: write `report.md`; read its HEAD and dirty paths in a
    ///   sandbox; for `verifies`, `facts.ask(Fact::Reviewed)` at the bound
    ///   head; revoke the seat; free the slot; end the job entry.
    async fn run(self: Arc<Self>, lead: Arc<WorkerSeat>, request: Request, alias: Alias, cap: MicroUsd) {
        todo!()
    }
}

/// What a delegate is told before its brief. Host text, not pstack's:
///
/// ```text
/// You are a delegate of the lead worker on ticket tk_5f1e9a0c2b7d4e11. You work on one brief,
/// below, and return one report with delegate_return; the lead reads only that. You cannot start
/// delegates: where a skill tells you to spawn a subagent, do that work yourself. Your /work is
/// <the lead's repository, read-only | your own copy of the lead's repository>. Your budget is
/// 400000 microusd and your time 60 minutes. The lead's goal: <restated by the lead in the brief>.
/// ```
pub fn preamble(ticket: &TicketId, workspace: Workspace, cap: MicroUsd, timeout: Duration) -> String {
    todo!()
}

// ---------------------------------------------------------------------------
// jobs.rs: a delegate is a job.
// ---------------------------------------------------------------------------

/// `Job` gains a kind. Everything that makes a job's end reach the worker
/// exactly once (one lock over the turn and the table; whoever takes an end
/// removes it) applies to delegates unchanged, and `job_wait` / `job_stop`
/// work on both. `max_jobs` keeps counting commands only; delegates count
/// against `Limits::per_assignment` and the delegate pool.
pub enum JobKind {
    Command { sandbox: crate::code::Started },
    /// A check is a command whose end the host records before the worker
    /// sees it: `Fact::Checked` first, then the end.
    Check { sandbox: crate::code::Started, check: solos_agent_core::change::CheckName, commit: CommitSha },
    Delegate { name: String, group: Option<String>, handle: DelegateHandle },
}

/// The running delegate's stop switch and its end, once it has one.
pub struct DelegateHandle;

/// The group rule, inside `Jobs::end_turn` and a delegate's end between
/// turns: a delegate's end whose group still has a member running is held
/// in the table; the last member's end releases the whole group as one Note
/// listing every member's end. `job_wait` on any member still returns that
/// member's own end at once, and taking it removes it from the group's Note.
/// So pstack's "spawn all N in one message" fan-outs wake the lead once.
pub fn group_note(ends: &[End]) -> String {
    todo!()
}

// ---------------------------------------------------------------------------
// The ledger part (crates/agent-store, the next schema version).
// ---------------------------------------------------------------------------

/// `HoldRequest` gains `part`:
///
/// ```ignore
/// pub struct HoldRequest<'a> {
///     pub account: &'a str,              // ticket:tk_… for lead and delegates alike
///     pub provider: &'a str,
///     pub since_ms: Ms,
///     pub cap_microusd: MicroUsd,        // the Mandate's budget
///     pub hold_microusd: MicroUsd,
///     pub at_ms: Ms,
///     /// A slice of the account with its own cap, such as one delegate's.
///     pub part: Option<(&'a str, MicroUsd)>,
/// }
/// ```
///
/// `spend` gains a nullable `part TEXT` column. `reserve` checks the
/// account's committed sum against its cap and, when `part` is set, the
/// part's committed sum against the part's cap, in the same IMMEDIATE
/// transaction, so two delegates cannot both take the last of either.
/// `committed(account, since)` is unchanged, so a lead's Report keeps
/// showing what its Assignment and its delegates spent together.
///
/// `Account::Ticket` gains `part: Option<(String, MicroUsd)>`; its `key()`
/// is unchanged (`ticket:<id>`), so the Watchdog's 80% alert and
/// `ticket_committed` count delegates with no change.
pub struct LedgerPart;

/// The broker's `WorkerSeat` gains its kind and its delegates:
///
/// ```ignore
/// pub enum SeatKind {
///     Lead,
///     Delegate { lead: AssignmentId, name: String, verifies: Option<(PrRef, CommitSha)> },
/// }
/// ```
///
/// A delegate seat's `work_dir` is its fork, or the lead's work with
/// `View::work_read_only`. `stop_jobs()` (pi_executor.rs:615) also stops
/// the seat's delegates, so they die with the Assignment.
pub struct SeatChanges;

// Tests (tests/agent_delegate.rs, scripted harness, fake upstream):
// - with max_workers 1 and max_live 2, a lead starts 3 delegates in one
//   turn and ends its turn; all three run (two, then one), the group's one
//   Note wakes the lead, and the Ticket ends ready. No deadlock, and the
//   lead never gave up its worker slot.
// - a delegate seat's tools exclude delegate, worker_report and pr_*; a
//   scripted delegate that calls delegate is refused "tool outside admitted
//   grants".
// - the same name twice returns the same job; the 25th delegate of an
//   Assignment is refused.
// - a delegate's model calls settle on ticket:<id> with part <name>; at its
//   cap its next call is refused and it ends OverBudget; the lead's report
//   spend includes its spend; the Ticket's budget caps the sum.
// - stopping the lead (Effect::Stop) kills its delegates, and none of their
//   ends is told.
// - a read delegate's `run_code` cannot write /work; a fork delegate can,
//   and its fork holds the lead's uncommitted change; the lead fetches a
//   fork's commit with `git -C /work fetch /children/x/work HEAD`.
// - a delegate whose turn ends twice without delegate_return ends NoReport.
// - while two delegates run 40 minutes (scaled clock), the lead's Ticket
//   gets host progress reports and the Watchdog raises no stall.
// - model alias `gpt-5` is refused with the list of valid aliases.
