//! Lives at: crates/agent-core/src/host.rs (new), with additions to model.rs,
//! engine.rs, policy.rs and ports.rs. Pure, like the rest of the core: serde,
//! serde_json and sha2 only.
//!
//! A host plan is what a person approves. The helper builds its body; the
//! core recomputes the digest, stores the plan, renders the message from the
//! body alone, and takes the click. The click approves one digest, once,
//! before it expires. Nothing a model writes reaches the rendered text.

use crate::model::{Actor, AssignmentId, Ms, TicketId};
use crate::trail::Digest;
use serde::{Deserialize, Serialize};

/// `hp_` and the first 16 hex digits of the plan's digest. A Discord button
/// names it: `solos_host_ok:<ticket>:<plan>` is 53 characters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostPlanId(String);

impl HostPlanId {
    pub fn of(digest: &Digest) -> Self {
        todo!("hp_ + digest.digits()[..16]")
    }
}

/// The survey row a delete names: `sv_` and 16 hex digits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SurveyId(pub String);

/// A root's name in the helper's config (`work`, `ci`), never a path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RootName(pub String);

/// A path under a root as its components. Each is non-empty, not `.` or
/// `..`, holds no `/` or NUL, and is at most 255 bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct RelPath(Vec<String>);

/// What may be deleted. One proof per kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    CargoTarget,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostAction {
    Delete { root: RootName, path: RelPath, target: TargetKind },
    /// An exact unit name from the helper's restart list.
    Restart { unit: String },
    /// An exact path from the helper's ops-file list.
    Edit { file: String },
    Restore { file: String, backup: String },
}

/// A file a restart depends on, pinned by identity and content. The uid and
/// gid must be unchanged, so a file the agent's uid could rewrite is refused
/// at plan time and again at apply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePin {
    pub path: String,
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub sha256: Digest,
}

/// What the helper measured at plan time. Apply refuses an item whose pins
/// no longer hold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Pins {
    Delete {
        dev: u64,
        ino: u64,
        bytes: u64,
        newest_ms: Ms,
        idle_days: u32,
    },
    Restart {
        /// The unit fragment and each drop-in, in the order systemd reads them.
        fragment: FilePin,
        dropins: Vec<FilePin>,
        /// Every regular file its Exec* lines name: the program and a script
        /// it runs.
        exec_files: Vec<FilePin>,
        environment_files: Vec<FilePin>,
        working_directory: String,
        no_new_privileges: bool,
        /// The unit cannot reach /run/user, so nothing it starts can ask a
        /// user manager for a transient unit.
        run_user_hidden: bool,
    },
    Edit {
        dev: u64,
        ino: u64,
        mode: u32,
        old_sha256: Digest,
        new_sha256: Digest,
        /// Unified diff, at most 60 lines, escaped as `render` escapes.
        diff: Vec<String>,
    },
    Restore {
        current_sha256: Digest,
        restored_sha256: Digest,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostItem {
    /// 1-based, in the order apply runs them.
    pub n: u16,
    pub action: HostAction,
    pub pins: Pins,
}

/// What the digest covers. Field order is fixed and there are no maps, so
/// `serde_json::to_vec` is canonical.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostPlanBody {
    pub version: u32,
    pub ticket: TicketId,
    /// The plan's number on its Ticket, so the same list asked twice is two plans.
    pub seq: u32,
    /// The requester's own Trigger words, clipped to 200 characters. Inside
    /// the digest, so the sentence the person reads under the plan is the
    /// sentence they approve.
    pub asked: String,
    /// The helper instance that planned it, from its `hello`.
    pub helper: String,
    pub planned_ms: Ms,
    pub expires_ms: Ms,
    pub items: Vec<HostItem>,
}

pub const HOST_PLAN_VERSION: u32 = 1;
pub const MAX_HOST_ITEMS: usize = 25;
pub const MAX_HOST_EDITS: usize = 1;
/// The requester's own words, quoted under the plan and inside the digest.
pub const MAX_ASKED_CHARS: usize = 200;
/// Fits Discord's three parts of 2,000 characters with room for the header.
pub const MAX_RENDERED_CHARS: usize = 5_400;
pub const MAX_PLANS_PER_TICKET: u32 = 10;

impl HostPlanBody {
    pub fn digest(&self) -> Digest {
        todo!("Digest::of(&serde_json::to_vec(self))")
    }
    /// Shape checks that need no host: item count, one edit at most, numbering
    /// from 1, RelPath components, `asked` at most 200 characters, and the
    /// rendering's length.
    pub fn validate(&self) -> Result<(), String> {
        todo!()
    }
}

/// The message a person reads, from the body alone. `body.asked` is the
/// requester's own Trigger text, clipped; it is the only quoted text. Every
/// path component, diff line and the quoted words are escaped: anything
/// outside printable ASCII becomes `\u{..}`, backticks are escaped, and the
/// list sits in a code block.
pub fn render(body: &HostPlanBody) -> String {
    todo!()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HostPlanState {
    Waiting,
    /// The Effect is queued; this is the write-ahead record of the apply.
    Approved { by: Actor, at_ms: Ms },
    Denied { by: Actor, at_ms: Ms },
    Closed { why: ClosedWhy, at_ms: Ms },
    Done {
        at_ms: Ms,
        receipts: Vec<HostReceipt>,
        freed_bytes: u64,
        free_before: u64,
        free_after: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosedWhy {
    Expired,
    TicketEnded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPlan {
    pub id: HostPlanId,
    pub digest: Digest,
    pub body: HostPlanBody,
    pub asked_ms: Ms,
    pub state: HostPlanState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostReceipt {
    pub n: u16,
    pub outcome: HostOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostOutcome {
    Done { freed_bytes: u64 },
    Refused { why: String },
    Failed { why: String },
}

/// The helper's answer to an apply, as the effect runner hands it to the Engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostApplied {
    pub digest: Digest,
    pub receipts: Vec<HostReceipt>,
    pub free_before: u64,
    pub free_after: u64,
    /// The head of the helper's own log after this apply, so the agent's
    /// chain can name the record outside its own database.
    pub helper_head: Digest,
}

// ---------------------------------------------------------------------------
// model.rs additions
// ---------------------------------------------------------------------------
//
// Mandate { .., #[serde(default)] pub host_ops: bool }          // fixed at accept
// Accept  { .., #[serde(default)] pub host_ops: bool }          // from router_decide's profile
// Ticket  { .., #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
//               pub host_plans: BTreeMap<HostPlanId, HostPlan> }
// Ask::HostPlan { plan: HostPlanId }
// InputBody::HostPlan { plan: HostPlanId, approved: bool, by: Actor }
// InputBody::HostDone { plan: HostPlanId, text: String }        // host-rendered receipts
// Effect::Host(HostJob)
// Ticket::awaits(Ask::HostPlan { plan }) == plan is Waiting
// Ticket::stage() == Stage::Approval while a plan is Waiting

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostJob {
    pub ticket: TicketId,
    pub plan: HostPlanId,
    pub digest: Digest,
}
// The runner sends Apply { digest, approved_by, agent_head }, where agent_head
// is the chain head read when the Effect runs, after HostPlanAnswered committed.

/// The new DomainEvent variants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostEvent {
    HostQueried {
        ticket: TicketId,
        assignment: AssignmentId,
        tool: String,
        args_sha256: Digest,
        answer_sha256: Digest,
    },
    HostPlanProposed {
        ticket: TicketId,
        plan: HostPlanId,
        digest: Digest,
        items: u16,
        bytes: u64,
        expires_ms: Ms,
    },
    HostPlanAnswered {
        ticket: TicketId,
        plan: HostPlanId,
        digest: Digest,
        by: Actor,
        approved: bool,
    },
    HostPlanClosed {
        ticket: TicketId,
        plan: HostPlanId,
        why: ClosedWhy,
    },
    HostActionDone {
        ticket: TicketId,
        plan: HostPlanId,
        n: u16,
        outcome: HostOutcome,
    },
    HostPlanDone {
        ticket: TicketId,
        plan: HostPlanId,
        freed_bytes: u64,
        free_before: u64,
        free_after: u64,
        helper_head: Digest,
    },
}

// ---------------------------------------------------------------------------
// policy.rs / ports.rs additions
// ---------------------------------------------------------------------------

pub trait HostPolicy {
    /// `policy.host_requesters`; each is also in `people`. Enters `version()`.
    fn may_request_host(&self, source: &str, actor: &Actor) -> bool;
    /// `policy.host_approvers` when set (each name also in `approvers`), else
    /// the `approvers` who are also `host_requesters`. A person only.
    fn may_approve_host(&self, source: &str, actor: &Actor) -> bool;
}

// ---------------------------------------------------------------------------
// engine.rs additions
// ---------------------------------------------------------------------------

pub struct EngineHostCommands;

impl EngineHostCommands {
    /// From the broker, after the helper planned. Refuses unless the Ticket is
    /// open and host-ops, `assignment` is its running one, the body's ticket
    /// and seq are this Ticket's next, the digest recomputes, `validate`
    /// passes, `expires_ms - now` is within the configured TTL, and no plan is
    /// already Waiting on this Ticket. A waiting plan is refused, not
    /// replaced, so a worker cannot put plan B under the message for plan A.
    /// Records HostPlanProposed and sends one Attention with `Ask::HostPlan`
    /// and `render(body)`.
    pub fn propose_host_plan(
        &mut self,
        ticket: &TicketId,
        assignment: &AssignmentId,
        body: HostPlanBody,
    ) -> Result<HostPlanId, crate::ports::Error> {
        todo!()
    }

    /// A person's click, from a Source's own controls. Refuses a non-person,
    /// one `may_approve_host` does not admit, a plan not Waiting, and a click at
    /// or after `expires_ms` (which closes the plan as Expired). Approved: the
    /// state, HostPlanAnswered and `Effect::Host` in one commit. Either way the
    /// worker gets `InputBody::HostPlan`.
    pub fn answer_host_plan(
        &mut self,
        ticket: &TicketId,
        plan: &HostPlanId,
        source: &str,
        by: Actor,
        approve: bool,
    ) -> Result<(), crate::ports::Error> {
        todo!()
    }

    /// The helper's receipts. Refuses a plan not Approved and a digest that
    /// differs. Records HostActionDone per item and HostPlanDone (with the
    /// helper's log head), moves the plan to Done, and gives the worker
    /// `InputBody::HostDone`. A repeat with the same receipts changes nothing,
    /// so a retried Effect is safe.
    pub fn host_done(
        &mut self,
        ticket: &TicketId,
        plan: &HostPlanId,
        applied: HostApplied,
    ) -> Result<(), crate::ports::Error> {
        todo!()
    }

    /// A read-only host call was answered. No state changes.
    pub fn host_queried(
        &mut self,
        ticket: &TicketId,
        assignment: &AssignmentId,
        tool: &str,
        args_sha256: Digest,
        answer_sha256: Digest,
    ) -> Result<(), crate::ports::Error> {
        todo!()
    }
}

// create_ticket: `accept.host_ops` needs `eval.strength == Explicit` and
// `policy.may_request_host(&primary.source, &primary.actor)`, else
// Error::Refused("<who> may not ask for host operations").
// revalidate: a host-ops Mandate also needs may_request_host.
// Fan-out: a host-ops Ticket takes Trigger Inputs only from allowed people.
