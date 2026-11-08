//! Lives at: src/agent/host_ops.rs (new), with additions to
//! src/agent/config.rs, src/agent/broker/{registry.rs, toolbox.rs},
//! src/agent/broker.rs (router_decide), src/agent/ports.rs (Order, Fact),
//! src/agent/runtime.rs (command, the effect runner) and
//! src/agent/discord/{mod.rs, channel.rs, source.rs}.
//!
//! The agent's half: a socket client to the helper, four worker tools that
//! only a host-ops seat holds, the Router's `profile`, and the click.
//! No tool executes a host action. The apply follows the click, as an Effect.

use solos_agent_core::host::{HostApplied, HostPlanBody, HostPlanId, SurveyId, TargetKind};
use solos_agent_core::model::{AssignmentId, TicketId};
use solos_agent_core::trail::Digest;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// config.rs
// ---------------------------------------------------------------------------

/// `host_ops:` in agent.yaml. It holds no allowlist: the agent's user can
/// edit this file, so the allowlist lives in the helper's root-owned config.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostOpsConfig {
    pub socket: PathBuf,
    #[serde(default = "thirty")]
    pub plan_ttl_mins: u32,
}
fn thirty() -> u32 {
    30
}

// AllowlistPolicy (crates/agent-core/src/policy.rs) gains:
//     #[serde(default)] pub host_requesters: BTreeSet<String>,
//     #[serde(default)] pub host_approvers: BTreeSet<String>,
// AgentConfig::validate: each host requester is in `people` and each host
// approver is in `approvers`; with host_ops set, at least one requester, and
// a host approver (the default is approvers ∩ host_requesters) on a source
// whose channel shows buttons (approval_sources(), config.rs:1699).

// ---------------------------------------------------------------------------
// host_ops.rs: the client
// ---------------------------------------------------------------------------

pub struct HostClient {
    socket: PathBuf,
    /// From the helper's hello.
    pub helper: String,
}

/// The protocol this agent speaks. An upgrade of either side is a refusal,
/// not a guess.
pub const HOST_PROTOCOL_VERSION: u32 = 1;

impl HostClient {
    /// Refuses to start the agent unless: the socket's directory is
    /// root-owned and not writable by our uid; the helper answers hello with
    /// protocol version 1, a uid that is not ours, and `agent_uid` equal to
    /// ours; our /proc/self/status says NoNewPrivs: 1; our uid is not 0.
    /// (SO_PEERCRED on our side would name systemd, which created the
    /// listening socket, so the helper checks SO_PEERCRED from its side
    /// instead.)
    pub async fn connect(config: &HostOpsConfig) -> anyhow::Result<Self> {
        todo!()
    }
    pub async fn survey(&self, ticket: &TicketId, kind: TargetKind, root: Option<String>, min_bytes: Option<u64>) -> anyhow::Result<serde_json::Value> {
        todo!()
    }
    pub async fn units(&self, units: Option<Vec<String>>) -> anyhow::Result<serde_json::Value> {
        todo!()
    }
    pub async fn read(&self, file: &str) -> anyhow::Result<serde_json::Value> {
        todo!()
    }
    pub async fn plan(&self, request: PlanRequest) -> anyhow::Result<(HostPlanBody, Digest)> {
        todo!()
    }
    pub async fn apply(&self, digest: &Digest, approved_by: &str) -> anyhow::Result<HostApplied> {
        todo!()
    }
}

pub struct PlanRequest {
    pub ticket: TicketId,
    pub seq: u32,
    pub ttl_mins: u32,
    /// The requester's Trigger words, clipped to 200 characters; the helper
    /// puts them in the body, so they are inside the digest.
    pub asked: String,
    pub delete: Vec<SurveyId>,
    pub restart: Vec<String>,
    pub edit: Option<(String, Vec<(String, String)>)>,
    pub restore: Option<(String, String)>,
}

/// The effect runner's job for `Effect::Host`: load the plan from the
/// Store, check it is Approved with this digest, read the current chain head,
/// call `apply(digest, approved_by, agent_head)`, then `Engine::host_done`.
/// Fails with retry while the helper is down, so the Watchdog's failing-Effect
/// signal reports it after three attempts.
pub async fn run_host_job(client: &HostClient, ticket: &TicketId, plan: &HostPlanId, digest: &Digest) -> anyhow::Result<HostApplied> {
    todo!()
}

// ---------------------------------------------------------------------------
// broker/registry.rs and broker/toolbox.rs
// ---------------------------------------------------------------------------

// WorkerOp gains HostSurvey, HostUnits, HostRead, HostPlan.
//
// host_survey {kind: "cargo_target", root?: string, min_bytes?: integer}
//   "List directories on the host that could be deleted to free disk, with
//    their size, how long they have been idle, and whether anything uses
//    them. Each row has an id; a plan names rows by id."
// host_units {units?: [string]}
//   "Show the state of the host's systemd units that you may see."
// host_read {file: string}
//   "Read one ops file you may edit."
// host_plan {delete?: [survey id], restart?: [unit], edit?: {file, replace:
//            [{old, new}]}, restore?: {file, backup}}
//   "Propose host actions. The host checks them, and a person approves the
//    whole list with a button. Nothing runs until then, and you get the
//    result as new input. A second plan is refused while one waits."
//
// None takes a path for a delete; none executes. host_survey, host_units and
// host_read are repeatable (what they answer moves); host_plan is not.

/// Toolbox's Resource gains HostOps: "this ticket is not a host-operations
/// ticket", or "no host helper on this host".
pub const HOST_OPS_TOOLS: [&str; 6] = [
    "worker_report",
    "worker_handback",
    "host_survey",
    "host_units",
    "host_read",
    "host_plan",
];

// worker_toolbox(environment, reader, dream, host_ops):
//     if host_ops {
//         toolbox.retain(&HOST_OPS_TOOLS,
//             "a host-operations ticket has its host tools and nothing else");
//     }
// A seat that is not host-ops is built without HostOps, so the four host
// tools are refused with the reason, as the browser is on a host without one.

/// `host_plan`'s handler: refuses without a channel that can show the
/// buttons and while a plan waits; asks the helper to plan with the Trigger's
/// own words; then `Fact::HostPlanAsk` so the Engine stores and asks. Answers
/// `{plan, items, bytes, status: "waiting"}`.
pub async fn host_plan(ticket: &TicketId, assignment: &AssignmentId, args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    todo!()
}

// host_survey, host_units and host_read: call the helper, then
// `Fact::HostQueried` with the digests of the arguments and the answer, and
// only then return the answer to the worker.

// ---------------------------------------------------------------------------
// broker.rs: router_decide
// ---------------------------------------------------------------------------

// DecideArgs gains `profile: Option<Profile>`:
//     host_ops is refused on a swarm, with a repo or deliverable, and with an
//     environment other than local; it sets Accept.host_ops. The core decides
//     whether the requester may ask for it.

#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    HostOps,
}

// ---------------------------------------------------------------------------
// ports.rs and runtime.rs
// ---------------------------------------------------------------------------

/// Order gains HostPlan. Runtime::command applies it with
/// `engine.answer_host_plan(&ticket, &plan, &source, by, approve)` and
/// replies "Approved by <who>: <n> host actions run now." or the refusal.
pub enum OrderAddition {
    HostPlan { plan: HostPlanId, approve: bool },
}

/// Fact gains:
///     HostQueried { ticket, assignment, tool, args_sha256, answer_sha256 }
///     HostPlanAsk { ticket, assignment, body: HostPlanBody }   // asked: the answer is the plan id
///     HostDone { ticket, plan, applied: HostApplied }
pub enum FactAdditions {}

/// `solos-swarm agent host-check --state-dir DIR --helper-log PATH`, run by
/// the owner with read access to both: verifies the helper's chain, then
/// that every Applying has an approving HostPlanAnswered with the same digest
/// in agent.db and every HostPlanDone matches an Applied.
pub fn host_check(state_dir: &std::path::Path, helper_log: &std::path::Path) -> anyhow::Result<()> {
    todo!()
}

// ---------------------------------------------------------------------------
// discord
// ---------------------------------------------------------------------------

/// mod.rs: beside APPROVE_ACTION, STOP_ACTION and the secret actions. Each
/// is followed by `:`, the Ticket id, `:` and the plan id: 53 characters.
pub const HOST_APPROVE_ACTION: &str = "solos_host_ok";
pub const HOST_DENY_ACTION: &str = "solos_host_no";

// channel.rs:
// - buttons(): Ask::HostPlan { plan } renders "Approve <n> actions" (success)
//   and "Deny" (danger). <n> comes from the Store's plan, never the text.
// - contents(): an Attention with Ask::HostPlan is packed into up to
//   MAX_PARTS parts and never cut. If it would be cut, it is posted without
//   buttons and ends with "This plan is too long to show here, so it cannot
//   be approved here." The core's MAX_RENDERED_CHARS keeps that from
//   happening.
//
// source.rs command(): HOST_APPROVE_ACTION | HOST_DENY_ACTION split the rest
// once more on ':' into the Ticket and a HostPlanId that must parse.
