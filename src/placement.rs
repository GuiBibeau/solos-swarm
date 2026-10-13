//! Scheduler reservations share the kernel's ledger and registered Task definitions.
use crate::{
    audit::content_hash,
    cluster::{Mutation, StateView},
    domain::{Actor, DomainCommand, TaskSpec},
    identity::WorkloadId,
    kernel::KernelState,
    ledger::Envelope,
    planner::HistoryConfig,
    scheduler::{
        self, Allocation, Bundle, Gang, Node, PlacementSnapshot, Plan, Resources, Strategy,
    },
    security::{AgentDefinition, Capability, Lease},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredAgent {
    pub id: String,
    pub capabilities: Vec<Capability>,
    pub task: TaskSpec,
    pub resources: Resources,
    pub labels: BTreeMap<String, String>,
    pub required_capabilities: BTreeSet<String>,
    pub skill_artifacts: Vec<String>,
    pub history: HistoryConfig,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmSpec {
    pub id: String,
    pub tenant: String,
    pub goal_id: String,
    pub trust_domain: String,
    pub provider: String,
    pub model: String,
    pub strategy: Strategy,
    pub lease_ms: u64,
    pub agents: Vec<DesiredAgent>,
    pub expiry_ms: u64,
}
impl SwarmSpec {
    pub fn gang(&self, revision: u64, incarnations: &BTreeMap<String, u64>) -> Result<Gang> {
        ensure!(
            !self.id.is_empty()
                && !self.goal_id.is_empty()
                && !self.provider.is_empty()
                && !self.model.is_empty()
                && !self.agents.is_empty()
                && self.agents.len() <= 500
                && (5000..=3_600_000).contains(&self.lease_ms),
            "swarm definition missing finite bounds"
        );
        let mut ids = BTreeSet::new();
        let bundles = self
            .agents
            .iter()
            .map(|agent| {
                ensure!(
                    ids.insert(agent.id.clone())
                        && !agent.capabilities.is_empty()
                        && agent.resources.cpu_millicores > 0
                        && agent.resources.memory_bytes > 0
                        && (agent.resources.provider_slots > 0
                            || agent.capabilities.iter().all(|cap| cap.kind
                                != crate::security::CapabilityKind::BudgetSpend
                                && !(cap.kind == crate::security::CapabilityKind::ToolCall
                                    && cap.constraints.operations.contains("inference"))))
                        && agent.history.context_token_budget > 0,
                    "invalid or duplicate desired agent"
                );
                let incarnation = incarnations.get(&agent.id).copied().unwrap_or(1);
                Ok(Bundle {
                    agent_id: agent.id.clone(),
                    placement_intent_id: format!("{}-{}-{incarnation}", self.id, agent.id),
                    incarnation,
                    resources: agent.resources,
                    labels: agent.labels.clone(),
                    required_capabilities: agent.required_capabilities.clone(),
                    budget_microusd: agent.task.budget_microusd,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Gang {
            id: self.id.clone(),
            goal_id: self.goal_id.clone(),
            strategy: self.strategy,
            bundles,
            priority: 0,
            created_revision: revision,
        })
    }
}
pub fn snapshot(view: &StateView) -> Result<PlacementSnapshot> {
    let mut nodes = BTreeMap::new();
    let mut allocations = BTreeMap::new();
    for (key, value) in &view.kv {
        if let Some(id) = key.strip_prefix("nodes/") {
            let node: Node = serde_json::from_value(value.clone())?;
            ensure!(node.id == id, "node directory identity mismatch");
            nodes.insert(id.into(), node);
        }
        if let Some(id) = key.strip_prefix("allocations/") {
            let allocation: Allocation = serde_json::from_value(value.clone())?;
            ensure!(
                allocation.agent_id == id,
                "allocation directory identity mismatch"
            );
            allocations.insert(id.into(), allocation);
        }
    }
    Ok(PlacementSnapshot {
        revision: view.revision,
        nodes,
        allocations,
    })
}
pub struct Reserve {
    pub state: KernelState,
    pub spec: SwarmSpec,
    pub operator: Actor,
    pub now_ms: u64,
    pub max_state_bytes: usize,
}
impl scheduler::Reserve for Reserve {
    fn compares(&self) -> Result<Vec<crate::cluster::Compare>> {
        Ok(vec![crate::cluster::Compare {
            key: "kernel/state".into(),
            expected: Some(serde_json::to_value(&self.state)?),
        }])
    }
    fn mutations(&self, plan: &Plan) -> Result<Vec<Mutation>> {
        let mut state = self.state.clone();
        reserve_state(&mut state, &self.spec, &self.operator, self.now_ms, plan)?;
        state.validate()?;
        state.checkpoint(
            &format!("placement/{}/{}", plan.gang_id, plan.expected_revision),
            &self.operator.id,
            self.now_ms / 1000,
        )?;
        crate::kernel::ensure_state_capacity(
            &state,
            serde_json::to_vec(&state)?.len(),
            self.max_state_bytes,
            false,
        )?;
        Ok(vec![
            Mutation::Put {
                key: "kernel/state".into(),
                value: serde_json::to_value(state)?,
            },
            Mutation::Put {
                key: format!("swarms/{}", self.spec.goal_id),
                value: serde_json::to_value(&self.spec)?,
            },
        ])
    }
}
/// The caller commits this pure reservation with allocations, capacity and lease fencing.
pub(crate) fn reserve_state(
    state: &mut KernelState,
    spec: &SwarmSpec,
    actor: &Actor,
    now_ms: u64,
    plan: &Plan,
) -> Result<()> {
    let now_ms = now_ms.max(crate::cluster::now_ms());
    ensure!(
        plan.gang_id == spec.id && now_ms < spec.expiry_ms,
        "placement spec expired or mismatched"
    );
    let goal = state.domain.goal(&spec.goal_id)?.spec.clone();
    ensure!(
        crate::kernel::search_open(state, &spec.goal_id),
        "final selection closed fresh placement"
    );
    ensure!(
        spec.tenant == state.security.policy.payload.tenant && spec.expiry_ms <= goal.expiry_ms,
        "swarm widens tenant/Goal expiry"
    );
    for allocation in &plan.allocations {
        let agent = spec
            .agents
            .iter()
            .find(|agent| agent.id == allocation.agent_id)
            .context("placement has undeclared agent")?;
        ensure!(
            allocation.goal_id == goal.goal_id && allocation.fence == plan.expected_revision + 1,
            "placement Goal/fence mismatch"
        );
        let parent = agent
            .task
            .parent_task_id
            .as_ref()
            .map(|id| current_parent(state, &goal.goal_id, id, now_ms))
            .transpose()?;
        let holder = WorkloadId {
            trust_domain: spec.trust_domain.clone(),
            tenant: spec.tenant.clone(),
            goal: spec.goal_id.clone(),
            node: allocation.node_id.clone(),
            agent: agent.id.clone(),
            incarnation: allocation.incarnation.to_string(),
        };
        let holder_uri = holder.uri()?;
        let expiry = (agent.task.expiry_ms.min(spec.expiry_ms) / 1000).min(
            parent
                .as_ref()
                .map_or(u64::MAX, |(grant, _)| grant.lease.expiry),
        );
        let capabilities = agent
            .capabilities
            .iter()
            .map(|declared| {
                let mut cap = declared.clone();
                cap.id = if parent.is_some() {
                    format!("{}-{}-{}", agent.id, cap.id, allocation.incarnation)
                } else {
                    format!("{}-{}", cap.id, allocation.incarnation)
                };
                cap.holder = holder_uri.clone();
                cap.not_before = cap.not_before.max(now_ms / 1000);
                cap.expiry = cap.expiry.min(expiry);
                if let Some((grant, _)) = &parent {
                    let ancestor = grant
                        .capabilities
                        .iter()
                        .find(|ancestor| {
                            declared.parent.as_ref() == Some(&ancestor.id)
                                && cap.attenuates(ancestor)
                        })
                        .or_else(|| {
                            grant
                                .capabilities
                                .iter()
                                .find(|ancestor| cap.attenuates(ancestor))
                        })
                        .context("child placement no longer attenuates the current parent grant")?;
                    cap.parent = Some(ancestor.id.clone());
                    cap.depth = ancestor
                        .depth
                        .checked_add(1)
                        .context("capability depth overflow")?;
                }
                Ok(cap)
            })
            .collect::<Result<Vec<_>>>()?;
        let definition = AgentDefinition {
            id: format!("{}-{}", agent.id, allocation.incarnation),
            holder,
            goal_id: goal.goal_id.clone(),
            grant_version: goal.version,
            capabilities,
            lease: Lease {
                not_before: now_ms / 1000,
                expiry,
                fencing_token: allocation.fence,
            },
        };
        let envelope = Envelope {
            id: allocation.budget_envelope_ref.clone(),
            placement_intent_id: allocation.placement_intent_id.clone(),
            placement_hash: content_hash(allocation)?,
            parent: parent.as_ref().map(|(_, envelope)| envelope.clone()),
            goal_id: goal.goal_id.clone(),
            holder: holder_uri,
            accounts: [
                "global".into(),
                format!("tenant/{}", spec.tenant),
                format!("goal/{}", goal.goal_id),
            ]
            .into(),
            allocation: allocation_budget(state, &goal.goal_id, agent)?,
            spent: 0,
            held: 0,
            active: true,
        };
        state.ledger.allocate(envelope)?;
        state.security.compile_definition(
            definition,
            parent.as_ref().map(|(grant, _)| grant.id.clone()),
            &mut state.ledger,
            now_ms / 1000,
        )?;
        if !state
            .domain
            .tasks
            .contains_key(&format!("{}/{}", goal.goal_id, agent.task.id))
        {
            state.domain.execute(
                actor,
                DomainCommand::DelegateTask {
                    goal_id: goal.goal_id.clone(),
                    spec: agent.task.clone(),
                    intent_id: allocation.placement_intent_id.clone(),
                    placement_intent_id: allocation.placement_intent_id.clone(),
                    budget_envelope_ref: allocation.budget_envelope_ref.clone(),
                },
                now_ms,
            )?;
        } else {
            let task = state
                .domain
                .tasks
                .get_mut(&format!("{}/{}", goal.goal_id, agent.task.id))
                .unwrap();
            ensure!(
                content_hash(&task.spec)? == content_hash(&agent.task)?,
                "registered Task changed during placement"
            );
            task.placement_intent_id = allocation.placement_intent_id.clone();
            task.budget_envelope_ref = allocation.budget_envelope_ref.clone();
        }
    }
    state.audit.append_committed(crate::audit::AuditEvent {
        id: format!("placement/{}/{}", plan.gang_id, plan.expected_revision),
        kind: "gang_reserved".into(),
        actor: actor.id.clone(),
        goal: Some(spec.goal_id.clone()),
        timestamp: (now_ms / 1000).max(
            state
                .audit
                .events()
                .last()
                .map_or(0, |event| event.timestamp),
        ),
        decision: "allow".into(),
        payload_hash: content_hash(plan)?,
    })?;
    Ok(())
}

/// Remaining liability follows every retired incarnation; a restart cannot refill its budget.
fn allocation_budget(state: &KernelState, goal: &str, agent: &DesiredAgent) -> Result<u64> {
    let mut spent_and_held = 0u64;
    for envelope in state
        .ledger
        .envelopes
        .values()
        .filter(|env| env.goal_id == goal)
    {
        let Ok(holder) = WorkloadId::parse(&envelope.holder) else {
            continue;
        };
        if holder.agent == agent.id {
            spent_and_held = spent_and_held
                .checked_add(envelope.spent)
                .and_then(|v| v.checked_add(envelope.held))
                .context("Task liability overflow")?;
        }
    }
    let returned = state
        .domain
        .tasks
        .get(&format!("{goal}/{}", agent.task.id))
        .map_or(0, |task| task.returned_microusd);
    let remaining = agent
        .task
        .budget_microusd
        .checked_sub(spent_and_held)
        .and_then(|v| v.checked_sub(returned))
        .context("Task allocation exhausted")?;
    ensure!(remaining > 0, "Task allocation exhausted");
    Ok(remaining)
}

pub(crate) fn current_parent(
    state: &KernelState,
    goal: &str,
    task_id: &str,
    now_ms: u64,
) -> Result<(crate::security::Grant, String)> {
    let task = state.domain.task(goal, task_id)?;
    ensure!(
        task.goal_version == state.domain.goal(goal)?.spec.version
            && task.lease_expiry_ms > now_ms
            && matches!(
                task.state,
                crate::domain::TaskState::Claimed | crate::domain::TaskState::Running
            ),
        "child placement requires a current parent Task"
    );
    let holder = task
        .claimed_by
        .as_ref()
        .context("parent Task has no owner")?;
    let grant = state
        .security
        .grants
        .values()
        .find(|grant| {
            !grant.revoked
                && grant.holder.uri().ok().as_ref() == Some(holder)
                && grant.goal_id == goal
                && grant.version == task.goal_version
                && grant.lease.not_before <= now_ms / 1000
                && grant.lease.expiry > now_ms / 1000
        })
        .context("parent Task has no live compiled grant")?
        .clone();
    Ok((grant, task.budget_envelope_ref.clone()))
}

pub struct Unreserve {
    pub state: KernelState,
    pub max_state_bytes: usize,
}
impl scheduler::Unreserve for Unreserve {
    fn compares(&self) -> Result<Vec<crate::cluster::Compare>> {
        Ok(vec![crate::cluster::Compare {
            key: "kernel/state".into(),
            expected: Some(serde_json::to_value(&self.state)?),
        }])
    }
    fn mutations(&self, allocations: &[Allocation]) -> Result<Vec<Mutation>> {
        let mut state = self.state.clone();
        for allocation in allocations {
            state
                .ledger
                .release_allocation(&allocation.budget_envelope_ref)?;
            let id = format!("{}-{}", allocation.agent_id, allocation.incarnation);
            if state.security.grants.contains_key(&id) {
                state.security.revoke_grant(&id)?;
            }
        }
        state.validate()?;
        state.checkpoint(
            &format!(
                "unreserve/{}",
                allocations
                    .iter()
                    .map(|a| a.placement_intent_id.as_str())
                    .collect::<Vec<_>>()
                    .join("+")
            ),
            "trusted-scheduler",
            crate::cluster::now_ms() / 1000,
        )?;
        crate::kernel::ensure_state_capacity(
            &state,
            serde_json::to_vec(&state)?.len(),
            self.max_state_bytes,
            true,
        )?;
        Ok(vec![Mutation::Put {
            key: "kernel/state".into(),
            value: serde_json::to_value(state)?,
        }])
    }
}
