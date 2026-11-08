//! Lives at: src/agent/broker.rs (the transports, admission, the verdict,
//! router_read), src/agent/broker/memory.rs (/memos), src/agent/broker/review.rs,
//! src/agent/dream.rs, src/agent/pi_router.rs (cards), src/agent/pi_executor.rs
//! (launch), src/gateway/ (the anthropic provider) and src/agent/publish/.
//!
//! Sketch only: not compiled. The broker keeps every key. A session holds a
//! bearer token, and the token's grant names the upstream its calls go to.

use crate::agent::models::{Clearance, ModelPolicy, Provider, Role, UpstreamName, withheld};
use crate::gateway::Provider as Transport;
use serde_json::Value;
use solos_agent_core::change::RepoName;
use std::collections::BTreeMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Transports and terms (broker.rs:765-791)
// ---------------------------------------------------------------------------

/// Replaces `Inner.provider` and `Inner.worker_terms`: one transport and one
/// set of model terms per upstream. Each transport holds its own key, read
/// once from `--credential-file` (default) or the upstream's `key_file`.
pub struct Upstreams {
    pub transports: BTreeMap<UpstreamName, Arc<dyn Transport>>,
    pub terms: BTreeMap<UpstreamName, super::broker::ModelTerms>,
    pub policy: Arc<ModelPolicy>,
}

pub struct Grant {
    // ... principal, account, terms, system_prompt as today (broker.rs:594-600) ...
    /// Which transport `dispatch` uses. A grant never names one the map
    /// does not hold.
    pub upstream: UpstreamName,
    /// The provider and role the session reads as.
    pub clearance: Clearance,
}

impl super::broker::AgentBroker {
    /// Admit a worker on its Ticket's upstream. Refuses an upstream whose
    /// provider the Ticket's repository does not admit for workers: the
    /// backstop behind the accept and the launch checks.
    pub fn admit_worker(
        &self,
        token: String,
        seat: super::broker::WorkerSeat,
        upstream: &UpstreamName,
        system_prompt: String,
    ) -> anyhow::Result<Arc<super::broker::WorkerSeat>> {
        todo!()
    }
}

/// `dispatch` (broker.rs:1594) picks the transport by the grant's upstream.
fn transport<'a>(upstreams: &'a Upstreams, grant: &Grant) -> &'a Arc<dyn Transport> {
    todo!()
}

// ---------------------------------------------------------------------------
// The verdict (broker.rs:1841-1981)
// ---------------------------------------------------------------------------

/// After the existing checks: an accept naming `repo` gets the repository's
/// worker upstream and that upstream's harness; with none, a Decline whose
/// `tell` the host wrote. An Eval whose Triggers are on a repository's
/// GitHub thread, where the Router may not read that repository, must name
/// it.
fn upstream_for_accept(
    policy: &ModelPolicy,
    repo: Option<&RepoName>,
    eval_repos: &[RepoName],
    router: Provider,
) -> Result<Option<solos_agent_core::model::UpstreamChoice>, Refused> {
    todo!()
}

pub enum Refused {
    /// The Router's call is refused with the reason; it may correct it.
    Correct(String),
    /// Becomes `Verdict::Decline`.
    Decline { reason: String, tell: String },
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// `router_read` of a Ticket (broker.rs:2406): keeps ids, stage, repo, plan,
/// budget, requester; replaces the report text, the waiting question and the
/// end reason with `withheld` when the Router may not read the Ticket's repo.
fn ticket_summary(ticket: &solos_agent_core::model::Ticket, policy: &ModelPolicy, router: Provider) -> Value {
    todo!()
}

/// A card's Trigger line (pi_router.rs:224): the text, or the withheld line.
fn trigger_text(trigger: &solos_agent_core::model::Trigger, policy: &ModelPolicy, router: Provider) -> String {
    todo!()
}

/// `/memos` (broker/memory.rs:901): only the memos `clearance` admits, in the
/// listing and the pages.
fn memos_page(store: &solos_agent_store::SqliteStore, path: &str, policy: &ModelPolicy, clearance: Clearance) -> Value {
    todo!()
}

/// The review (broker/review.rs:394): `None` when the proposal carries a
/// repository the review's provider may not read; the proposal then waits for
/// the operator, and the Watchdog says so once.
fn reviewable(memo: &solos_agent_core::memo::Memo, policy: &ModelPolicy, review: Provider) -> bool {
    policy.admits_all(&memo.carries, Role::Memory, review)
}

/// The dream's staging (dream.rs:178): Tickets on a repository the dream's
/// provider may not read, and memos that carry one, are left out; the count
/// goes into the digest.
fn dream_tickets(
    ended: Vec<solos_agent_core::model::Ticket>,
    policy: &ModelPolicy,
    dream: Provider,
) -> (Vec<solos_agent_core::model::Ticket>, usize) {
    todo!()
}

// ---------------------------------------------------------------------------
// Launch (pi_executor.rs:681)
// ---------------------------------------------------------------------------

/// Before the clone: the Ticket's upstream, or `default` for a Ticket with
/// none recorded, must still be admitted. Otherwise the Assignment ends with
/// a `failed` Report that names the policy, and nothing is cloned.
fn launch_upstream(
    launch: &super::ports::Launch,
    policy: &ModelPolicy,
) -> Result<UpstreamName, String> {
    todo!()
}

// ---------------------------------------------------------------------------
// The anthropic provider (src/gateway/, after feat/deepseek-anthropic-wire)
// ---------------------------------------------------------------------------

/// `https://api.anthropic.com/v1/messages`, `x-api-key`, `anthropic-version`.
/// The body is the Messages request Claude Code sent, admitted by the grant's
/// tools, with the broker's `max_tokens` ceiling, no `temperature`, thinking
/// blocks unchanged, and server tools refused.
pub struct AnthropicMessages {
    pub endpoint: &'static str,
    pub client: reqwest::Client,
    pub key: zeroize::Zeroizing<String>,
}

/// Anthropic's usage: `input_tokens` leaves out the cache reads and writes.
pub struct AnthropicReceipt {
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    pub request_id: String,
}

impl AnthropicReceipt {
    pub fn read(response: &Value, max_output_tokens: u64) -> anyhow::Result<Self> {
        todo!()
    }

    pub fn cost(&self, prices: &crate::agent::models::Prices) -> u64 {
        todo!("input + read at the hit rate + creation at the write rate + output")
    }
}

// ---------------------------------------------------------------------------
// The dashboard (publish/snapshot.rs, FORMAT 2)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
pub struct ModelView {
    pub upstream: String,
    pub provider: String,
    pub model: String,
    pub harness: String,
}
// TicketSnapshot.model: Option<ModelView>, from the Mandate or the latest
// Assignment. AssignmentView.ran_on: Option<ModelView>. IndexRow.provider.
