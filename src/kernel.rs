//! The only mutation boundary: authorization, state, money and audit commit together.
use crate::{
    audit::{AuditEvent, AuditLog, CryptoSigner, Hash, Signed, content_hash},
    cluster::{Cluster, Compare, LeaseCheck, Mutation, Transaction},
    domain::{self, Actor, DomainCommand, DomainState},
    ledger::{Envelope, Ledger, PriceTable, Receipt},
    modules::ModuleRegistry,
    security::{
        self, ActionRequest, AgentDefinition, AuthenticatedSubject, ConfirmationTier,
        DeploymentPolicy, DispatchPermit, GoalPosture, HumanApproval, IsolationTier, Scope,
        SecurityState,
    },
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

const STATE_KEY: &str = "kernel/state";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordedReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<crate::telemetry::Correlation>,
    pub actor: String,
    pub request: Value,
    pub request_hash: Hash,
    pub ok: bool,
    pub value: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Effect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<crate::telemetry::Correlation>,
    pub request: ActionRequest,
    pub payload: Value,
    pub permit: Signed<DispatchPermit>,
    pub state: String,
    pub result: Option<Value>,
    pub task_context: Option<TaskContext>,
}
fn effect_ack(effect: &Effect) -> Value {
    json!({"intent":effect.request.intent_id,"state":effect.state,"permit":effect.permit,
        "task_context":effect.task_context})
}
/// Refresh local application authority and apply it within the caller's same atomic commit.
pub(crate) fn refresh_nonbillable_effect(
    state: &mut KernelState,
    subject: &AuthenticatedSubject,
    intent: &str,
    signer: &CryptoSigner,
    now: u64,
) -> Result<Option<Value>> {
    let effect = state
        .effects
        .get(intent)
        .context("local effect absent")?
        .clone();
    ensure!(
        effect.state == "pending" && !effect.permit.payload.billable,
        "only unused local effects may refresh application authority"
    );
    let approvals: Vec<_> = state.action_approvals.values().cloned().collect();
    match state.security.refresh_nonbillable_permit(
        subject,
        &effect.request,
        &effect.permit,
        &approvals,
        &mut state.ledger,
        signer,
        now,
    )? {
        security::NonbillablePermitRefresh::Ready { permit, audit } => {
            if let Some(audit) = audit {
                state.audit.append_committed(audit)?;
            }
            state.effects.get_mut(intent).unwrap().permit = permit;
            state.pending_actions.remove(intent);
            Ok(None)
        }
        security::NonbillablePermitRefresh::ConfirmationRequired(review) => {
            state.pending_actions.insert(
                intent.into(),
                PendingAction {
                    request: effect.request,
                    payload: effect.payload,
                    prices: None,
                    quota: None,
                    task: effect.task_context,
                    review: review.clone(),
                },
            );
            let event = AuditEvent {
                correlation: None,
                id: format!("apply-review/{intent}/{}", hex(&content_hash(&review)?)),
                kind: "confirmation_required".into(),
                actor: subject.id().into(),
                goal: Some(effect.permit.payload.goal_id),
                timestamp: now,
                decision: "pending_human_confirmation".into(),
                payload_hash: content_hash(&review)?,
            };
            if let Some(existing) = state.audit.events().iter().find(|old| old.id == event.id) {
                ensure!(
                    existing.kind == event.kind
                        && existing.actor == event.actor
                        && existing.goal == event.goal
                        && existing.decision == event.decision
                        && existing.payload_hash == event.payload_hash,
                    "application review audit identity reused for different content"
                );
            } else {
                state.audit.append_committed(event)?;
            }
            Ok(Some(
                json!({"status":"confirmation_required","intent":intent,"review":review}),
            ))
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingAction {
    pub request: ActionRequest,
    pub payload: Value,
    pub prices: Option<Signed<PriceTable>>,
    pub quota: Option<crate::quota::QuotaRequest>,
    pub task: Option<TaskContext>,
    pub review: security::ConfirmationReview,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskContext {
    pub id: String,
    pub incarnation: u64,
    pub fence: u64,
}
pub struct Admission {
    pub request: ActionRequest,
    pub payload: Value,
    pub prices: Option<Signed<PriceTable>>,
    pub approvals: Vec<Signed<HumanApproval>>,
    pub leases: Vec<LeaseCheck>,
    pub quota: Option<crate::quota::QuotaRequest>,
    pub task: Option<TaskContext>,
}
fn diagnostic_key(goal: &str, task: Option<&str>) -> String {
    format!("{goal}/{}", task.unwrap_or("@goal"))
}
fn store_diagnostic(
    state: &mut KernelState,
    key: String,
    record: crate::telemetry::DiagnosticRecord,
) {
    const MAX_DIAGNOSTICS: usize = 8192;
    if !state.diagnostic_reasons.contains_key(&key)
        && state.diagnostic_reasons.len() >= MAX_DIAGNOSTICS
        && let Some(oldest) = state
            .diagnostic_reasons
            .iter()
            .min_by_key(|(_, r)| r.recorded_ms)
            .map(|(k, _)| k.clone())
    {
        state.diagnostic_reasons.remove(&oldest);
    }
    state.diagnostic_reasons.insert(key, record);
}
fn diagnostic_markings(
    state: &KernelState,
    goal: &str,
    task: Option<&str>,
) -> Result<BTreeSet<String>> {
    let mut markings = BTreeSet::from([state.security.policy.payload.classification.clone()]);
    let mut pending: Vec<_> = task
        .and_then(|id| state.task_input_sources.get(&format!("{goal}/{id}")))
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    if let Some(holder) = task
        .and_then(|id| state.domain.task(goal, id).ok())
        .and_then(|record| record.claimed_by.as_ref())
        && let Some(observed) = state.security.observed.get(holder)
    {
        pending.extend(observed.iter().cloned());
    }
    let mut seen = BTreeSet::new();
    while let Some(source) = pending.pop() {
        if !seen.insert(source.clone()) {
            continue;
        }
        if let Some(input) = state.security.inputs.get(&source) {
            markings.extend(input.markings.iter().cloned());
            pending.extend(input.parents.iter().cloned());
        } else if let Some(finding) = state.security.findings.get(&source) {
            markings.extend(finding.payload.markings.iter().cloned());
            pending.extend(finding.payload.inputs.iter().cloned());
        } else {
            anyhow::bail!("diagnostic input lineage missing");
        }
    }
    Ok(markings)
}
#[derive(Serialize, Deserialize)]
#[serde(
    tag = "kernel_metadata",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum ActionCorrelationMetadata {
    DiagnosticObservation {
        correlation: crate::telemetry::Correlation,
        reason: crate::telemetry::DiagnosticReason,
        observed_revision: u64,
    },
    AuthenticatedWorkloadDenial {
        correlation: crate::telemetry::Correlation,
        proof_hash: Hash,
        reason_hash: Hash,
    },
}
fn committed_correlation(
    state: &KernelState,
    actor: &str,
    request: &Value,
) -> Result<crate::telemetry::Correlation> {
    if state
        .security
        .policy
        .payload
        .trusted_components
        .contains(actor)
        && matches!(
            request.get("kernel_metadata").and_then(Value::as_str),
            Some("diagnostic_observation" | "authenticated_workload_denial")
        )
        && let Ok(metadata) = serde_json::from_value::<ActionCorrelationMetadata>(request.clone())
    {
        let correlation = match metadata {
            ActionCorrelationMetadata::DiagnosticObservation { correlation, .. }
            | ActionCorrelationMetadata::AuthenticatedWorkloadDenial { correlation, .. } => {
                correlation
            }
        };
        correlation.validate()?;
        ensure!(
            correlation.deployment_id == state.audit.log_id && correlation.run_id == state.run_id,
            "trusted action correlation deployment or run mismatch"
        );
        return Ok(correlation);
    }
    if let Some(values) = request.as_array()
        && matches!(values.len(), 8 | 9)
        && values[0].as_array().is_some_and(|v| v.len() == 32)
        && let Ok(context) =
            serde_json::from_value::<crate::telemetry::Correlation>(values[7].clone())
    {
        context.validate()?;
        ensure!(
            context.deployment_id == state.audit.log_id,
            "admission correlation deployment mismatch"
        );
        return Ok(context);
    }
    let first = request
        .as_array()
        .and_then(|values| values.first())
        .unwrap_or(request);
    let intent = first.get("intent_id").and_then(Value::as_str).or_else(|| {
        first
            .as_str()
            .filter(|id| state.effects.contains_key(*id) || state.pending_actions.contains_key(*id))
    });
    let grant = first
        .get("grant_id")
        .and_then(Value::as_str)
        .or_else(|| first.as_str())
        .and_then(|id| state.security.grants.get(id));
    let goal = grant
        .map(|g| g.goal_id.as_str())
        .or_else(|| first.get("goal_id").and_then(Value::as_str))
        .filter(|id| state.domain.goals.contains_key(*id));
    let explicit = if first
        .as_str()
        .is_some_and(|id| state.security.grants.contains_key(id))
    {
        request
            .as_array()
            .and_then(|values| values.get(1))
            .and_then(|v| serde_json::from_value::<TaskContext>(v.clone()).ok())
    } else {
        None
    };
    let inferred = if explicit.is_none() {
        let mut owned = state.domain.tasks.values().filter(|t| {
            t.claimed_by.as_deref() == Some(actor)
                && matches!(
                    t.state,
                    domain::TaskState::Claimed | domain::TaskState::Running
                )
        });
        let first = owned.next();
        if owned.next().is_none() {
            first.map(|t| TaskContext {
                id: t.spec.id.clone(),
                incarnation: t.incarnation,
                fence: t.fencing_token,
            })
        } else {
            None
        }
    } else {
        None
    };
    let context = explicit.as_ref().or(inferred.as_ref());
    let fallback_goal = crate::identity::WorkloadId::parse(actor)
        .ok()
        .map(|id| id.goal)
        .filter(|id| state.domain.goals.contains_key(id));
    state.correlation_for(actor, goal.or(fallback_goal.as_deref()), context, intent)
}
fn correlate_action_records(
    old: &Value,
    next: &mut KernelState,
    correlation: &crate::telemetry::Correlation,
) {
    for (id, effect) in &mut next.effects {
        if old["effects"].get(id).is_none() || correlation.intent_id.as_deref() == Some(id.as_str())
        {
            effect.correlation = Some(correlation.clone());
        }
    }
    for (id, reservation) in &mut next.ledger.intents {
        if old["ledger"]["intents"].get(id).is_none()
            || correlation.intent_id.as_deref() == Some(id.as_str())
        {
            reservation.correlation = Some(correlation.clone());
        }
    }
    for (goal, entries) in &mut next.domain.boards {
        let before = old["domain"]["boards"][goal].as_array().map_or(0, Vec::len);
        for entry in entries.iter_mut().skip(before) {
            entry.correlation = Some(correlation.clone());
        }
    }
    for (id, finding) in &mut next.domain.findings {
        if old["domain"]["findings"].get(id).is_none() {
            finding.correlation = Some(correlation.clone());
        }
    }
    for (id, attempt) in &mut next.domain.attempts {
        if old["domain"]["attempts"].get(id).is_none() {
            attempt.correlation = Some(correlation.clone());
        }
    }
    for (id, need) in &mut next.domain.needs {
        let before = old["domain"]["needs"][id]["decisions"]
            .as_array()
            .map_or(0, Vec::len);
        for decision in need.decisions.iter_mut().skip(before) {
            decision.correlation = Some(correlation.clone());
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub diagnostic_reasons: BTreeMap<String, crate::telemetry::DiagnosticRecord>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub budget_pauses: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rate_pauses: BTreeMap<String, crate::telemetry::RatePause>,
    pub domain: DomainState,
    pub security: SecurityState,
    pub ledger: Ledger,
    pub modules: ModuleRegistry,
    pub audit: AuditLog,
    pub replies: BTreeMap<String, RecordedReply>,
    pub effects: BTreeMap<String, Effect>,
    pub finding_sources: BTreeMap<String, String>,
    pub quotas: crate::quota::Quotas,
    pub desired_swarms: BTreeMap<String, (String, crate::placement::SwarmSpec)>,
    pub workload_certificates: BTreeMap<String, Vec<u8>>,
    pub task_input_sources: BTreeMap<String, BTreeSet<String>>,
    pub final_evaluations: BTreeMap<String, Signed<crate::evaluator::EvaluationResult>>,
    #[serde(default)]
    pub evaluation_jobs: BTreeMap<String, crate::evaluator_runtime::EvaluationJob>,
    #[serde(default)]
    pub pending_actions: BTreeMap<String, PendingAction>,
    #[serde(default)]
    pub action_approvals: BTreeMap<String, Signed<HumanApproval>>,
    #[serde(default)]
    pub runtime_instances: BTreeMap<String, crate::runtime::RuntimeAttestation>,
    #[serde(default)]
    pub egress: crate::flow::FlowMonitor,
}
impl KernelState {
    pub fn rate_paused(&self, goal: &str, task: &str, now_ms: u64) -> bool {
        self.domain.goal(goal).is_ok_and(|g| {
            self.rate_pauses
                .get(&diagnostic_key(goal, Some(task)))
                .filter(|pause| pause.goal_version == g.spec.version)
                .is_some_and(|pause| {
                    let Some(quota) = self.quotas.configs.get(&pause.request.scope) else {
                        return true;
                    };
                    let deadline = now_ms
                        .saturating_add(1)
                        .min(g.spec.expiry_ms)
                        .min(quota.payload.expiry_ms);
                    if deadline <= now_ms {
                        return true;
                    }
                    // This read-only probe asks whether new work fits; it never revives the denied intent.
                    let mut probe = pause.request.clone();
                    probe.intent = format!(
                        "rate-check-{}",
                        hex(&crate::audit::hash(pause.request.intent.as_bytes()))
                    );
                    probe.deadline_ms = deadline;
                    self.quotas
                        .check_available(&probe, &pause.provider, now_ms)
                        .is_err()
                })
        })
    }
    pub fn budget_paused(&self, goal: &str, task: &str) -> bool {
        self.domain.goal(goal).is_ok_and(|g| {
            [diagnostic_key(goal, Some(task)), diagnostic_key(goal, None)]
                .iter()
                .any(|key| self.budget_pauses.get(key) == Some(&g.spec.version))
        })
    }
    pub fn correlation_for(
        &self,
        actor: &str,
        goal: Option<&str>,
        task: Option<&TaskContext>,
        intent: Option<&str>,
    ) -> Result<crate::telemetry::Correlation> {
        let effect = intent.and_then(|id| self.effects.get(id));
        if let Some(effect) = effect
            && let Some(saved) = &effect.correlation
        {
            let mut correlation = saved.clone();
            correlation.execution_attempt =
                matches!(effect.state.as_str(), "dispatched" | "done" | "unknown").then_some(1);
            correlation.validate()?;
            return Ok(correlation);
        }
        let holder = effect.map_or(actor, |e| e.permit.payload.holder.as_str());
        let mut correlation = crate::telemetry::Correlation::for_actor(&self.audit.log_id, holder);
        correlation.run_id = self.run_id.clone();
        if let Some(goal) = effect.map(|e| e.permit.payload.goal_id.as_str()).or(goal) {
            self.domain.goal(goal)?;
            correlation.goal_id = Some(goal.into());
            if let Some(task) = effect.and_then(|e| e.task_context.as_ref()).or(task) {
                let registered = self.domain.task(goal, &task.id)?;
                ensure!(
                    registered.goal_id == goal,
                    "correlation Task belongs to another Goal"
                );
                correlation = correlation.with_task(goal, &task.id);
            }
        }
        correlation.intent_id = intent.map(str::to_owned);
        correlation.execution_attempt = effect
            .filter(|e| matches!(e.state.as_str(), "dispatched" | "done" | "unknown"))
            .map(|_| 1);
        correlation.validate()?;
        Ok(correlation)
    }
    pub fn new(
        policy: Signed<DeploymentPolicy>,
        trusted_key: &Hash,
        now: u64,
        deployment: String,
    ) -> Result<Self> {
        Ok(Self {
            run_id: None,
            diagnostic_reasons: BTreeMap::new(),
            budget_pauses: BTreeMap::new(),
            rate_pauses: BTreeMap::new(),
            domain: DomainState::new(),
            security: SecurityState::new(policy, trusted_key, now)?,
            ledger: Ledger::new("microusd".into())?,
            modules: ModuleRegistry::default(),
            audit: AuditLog::new(deployment)?,
            replies: BTreeMap::new(),
            effects: BTreeMap::new(),
            finding_sources: BTreeMap::new(),
            quotas: crate::quota::Quotas::default(),
            desired_swarms: BTreeMap::new(),
            workload_certificates: BTreeMap::new(),
            task_input_sources: BTreeMap::new(),
            final_evaluations: BTreeMap::new(),
            evaluation_jobs: BTreeMap::new(),
            pending_actions: BTreeMap::new(),
            action_approvals: BTreeMap::new(),
            runtime_instances: BTreeMap::new(),
            egress: crate::flow::FlowMonitor::default(),
        })
    }
    pub fn validate(&self) -> Result<()> {
        self.ledger.validate()?;
        self.audit.validate()?;
        self.egress.config.validate()?;
        Ok(())
    }
    pub fn content_hash(&self) -> Result<Hash> {
        let mut snapshot = self.clone();
        snapshot.audit = AuditLog::new(self.audit.log_id.clone())?;
        content_hash(&snapshot)
    }
    pub fn checkpoint(&mut self, id: &str, actor: &str, now: u64) -> Result<()> {
        let timestamp = now.max(
            self.audit
                .events()
                .last()
                .map_or(0, |event| event.timestamp),
        );
        self.audit.append_committed(AuditEvent {
            correlation: self
                .audit
                .events()
                .last()
                .and_then(|event| event.correlation.clone()),
            id: format!("checkpoint/{id}"),
            kind: "state_checkpoint".into(),
            actor: actor.into(),
            goal: None,
            timestamp,
            decision: "committed".into(),
            payload_hash: self.content_hash()?,
        })?;
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    pub value: Value,
    pub duplicate: bool,
    pub revision: u64,
}
#[derive(Debug, Clone, Copy)]
pub struct StateCapacityError;
impl std::fmt::Display for StateCapacityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("deployment state admission limit reached; outstanding receipt and control capacity remain reserved")
    }
}
impl std::error::Error for StateCapacityError {}
impl Reply {
    pub fn require(self) -> Result<Value> {
        if !self.ok && self.value["code"] == "state_capacity" {
            return Err(StateCapacityError.into());
        }
        if !self.ok && self.value["code"] == "budget_exhausted" {
            return Err(crate::ledger::BudgetExhausted {
                category: "registered",
            }
            .into());
        }
        if !self.ok && self.value["code"] == "provider_paused" {
            return Err(serde_json::from_value::<crate::quota::QuotaPaused>(
                self.value["quota_pause"].clone(),
            )?
            .into());
        }
        ensure!(
            self.ok,
            "{}",
            self.value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("request denied")
        );
        Ok(self.value)
    }
}
pub const MAX_RESULT_RECEIPT_BYTES: usize = 16 * 1024;
/// Eight bounded financial/provenance records, including audit/checkpoint and unknown recovery.
const EFFECT_RECONCILIATION_CREDIT_BYTES: usize = 8 * MAX_RESULT_RECEIPT_BYTES;
pub struct Kernel {
    pub cluster: Arc<Cluster>,
    pub signer: Arc<CryptoSigner>,
    pub max_state_bytes: usize,
    pub response_store: Option<Arc<tokio::sync::Mutex<crate::artifacts::ArtifactStore>>>,
}
impl Kernel {
    pub async fn correlation_for(
        &self,
        actor: &str,
        goal: Option<&str>,
        task: Option<&TaskContext>,
        intent: Option<&str>,
    ) -> Result<crate::telemetry::Correlation> {
        self.control_state()
            .await?
            .correlation_for(actor, goal, task, intent)
    }
    pub async fn record_diagnostic(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        goal: &str,
        task: Option<&str>,
        reason: crate::telemetry::DiagnosticReason,
        observed_revision: u64,
    ) -> Result<Reply> {
        let read = self.cluster.read_control(STATE_KEY).await?;
        ensure!(
            observed_revision <= read.revision,
            "diagnostic observation revision is not committed"
        );
        let state: KernelState = serde_json::from_value(read.value.context("deployment absent")?)?;
        subject.ensure_current(crate::cluster::now_ms() / 1000)?;
        ensure!(
            state
                .security
                .policy
                .payload
                .trusted_components
                .contains(subject.id()),
            "diagnostic recorder is not trusted runtime"
        );
        let version = state.domain.goal(goal)?.spec.version;
        let fence = task
            .map(|id| state.domain.task(goal, id).map(|t| t.fencing_token))
            .transpose()?;
        let key = diagnostic_key(goal, task);
        if let Some(previous) = state.diagnostic_reasons.get(&key)
            && previous.reason == reason
            && previous.goal_version == version
            && previous.task_fence == fence
        {
            return Ok(Reply {
                ok: true,
                value: serde_json::to_value(previous)?,
                duplicate: true,
                revision: read.revision,
            });
        }
        let context = task
            .map(|id| {
                state.domain.task(goal, id).map(|record| TaskContext {
                    id: id.into(),
                    incarnation: record.incarnation,
                    fence: record.fencing_token,
                })
            })
            .transpose()?;
        let intent = match &reason {
            crate::telemetry::DiagnosticReason::ProviderOutcome { intent_id, .. } => {
                Some(intent_id.as_str())
            }
            _ => None,
        };
        let metadata = ActionCorrelationMetadata::DiagnosticObservation {
            correlation: state.correlation_for(
                subject.id(),
                Some(goal),
                context.as_ref(),
                intent,
            )?,
            reason: reason.clone(),
            observed_revision,
        };
        self.runtime_control_commit(id, subject, &metadata, |state| {
            let version = state.domain.goal(goal)?.spec.version;
            let context = task
                .map(|id| {
                    state.domain.task(goal, id).map(|record| TaskContext {
                        id: id.into(),
                        incarnation: record.incarnation,
                        fence: record.fencing_token,
                    })
                })
                .transpose()?;
            let correlated_intent =
                if let crate::telemetry::DiagnosticReason::ProviderOutcome { intent_id, outcome } =
                    &reason
                {
                    let effect = state
                        .effects
                        .get(intent_id)
                        .context("provider diagnostic effect absent")?;
                    ensure!(
                        effect.permit.payload.goal_id == goal
                            && effect.task_context.as_ref().map(|task| task.id.as_str()) == task,
                        "provider diagnostic scope differs from original intent"
                    );
                    let expected = match outcome {
                        crate::provider::Outcome::Completed
                        | crate::provider::Outcome::ReceiptWithoutCompletion => "done",
                        crate::provider::Outcome::Unknown => "unknown",
                        crate::provider::Outcome::DefinitelyNotDispatched => "not_dispatched",
                    };
                    ensure!(
                        effect.state == expected
                            && effect
                                .result
                                .as_ref()
                                .is_some_and(|result| result["outcome"]["outcome"]
                                    == serde_json::to_value(outcome).unwrap_or(Value::Null)),
                        "provider diagnostic outcome differs from durable receipt metadata"
                    );
                    Some(intent_id.as_str())
                } else {
                    None
                };
            let correlation = state.correlation_for(
                subject.id(),
                Some(goal),
                context.as_ref(),
                correlated_intent,
            )?;
            let markings = diagnostic_markings(state, goal, task)?;
            if let crate::telemetry::DiagnosticReason::MissingDependency { service } = &reason {
                ensure!(
                    !service.is_empty()
                        && service.len() <= 128
                        && !service.contains(['\r', '\n', '\0']),
                    "invalid diagnostic dependency"
                );
            }
            let record = crate::telemetry::DiagnosticRecord {
                goal_id: goal.into(),
                goal_version: version,
                task_id: task.map(str::to_owned),
                task_fence: context.as_ref().map(|t| t.fence),
                reason: reason.clone(),
                observed_revision,
                recorded_ms: crate::cluster::now_ms(),
                markings,
                correlation,
            };
            store_diagnostic(state, key.clone(), record.clone());
            Ok(serde_json::to_value(record)?)
        })
        .await
    }
    /// Fresh authenticated scoped inspection; no replay, admission, observation or revision write.
    pub async fn explain(
        &self,
        subject: &AuthenticatedSubject,
        goal: &str,
        task: &TaskContext,
        leases: &[LeaseCheck],
    ) -> Result<Value> {
        let view = self.cluster.view_control().await?;
        let state: KernelState =
            serde_json::from_value(view.kv.get(STATE_KEY).context("deployment absent")?.clone())?;
        let now = crate::cluster::now_ms();
        subject.ensure_current(now / 1000)?;
        check_allocation(subject, leases)?;
        check_task(&state, subject, Some(task), false)?;
        for check in leases {
            ensure!(
                view.leases
                    .get(&check.id)
                    .is_some_and(|lease| !lease.revoked
                        && lease.holder == check.holder
                        && lease.fence == check.fence
                        && lease.expires_ms > now),
                "diagnostic allocation fenced"
            );
        }
        let markings = diagnostic_markings(&state, goal, Some(&task.id))?;
        let grant = state
            .security
            .grants
            .values()
            .filter(|grant| {
                grant.goal_id == goal && grant.holder.uri().ok().as_deref() == Some(subject.id())
            })
            .find_map(|grant| state.security.live_grant(&grant.id, now / 1000).ok())
            .context("diagnostic current grant absent")?;
        ensure!(
            grant
                .capabilities
                .iter()
                .any(|cap| cap.kind == security::CapabilityKind::ToolCall
                    && cap.resource.matches(&format!("task/{goal}"))
                    && cap.constraints.operations.contains("task_work")
                    && cap.not_before <= now / 1000
                    && cap.expiry > now / 1000
                    && markings.is_subset(&cap.constraints.read_markings)),
            "diagnostic read clearance denied"
        );
        let version = state.domain.goal(goal)?.spec.version;
        let record = state
            .diagnostic_reasons
            .get(&diagnostic_key(goal, Some(&task.id)))
            .or_else(|| state.diagnostic_reasons.get(&diagnostic_key(goal, None)))
            .filter(|record| {
                record.goal_version == version
                    && record.task_fence.is_none_or(|fence| fence == task.fence)
            });
        Ok(
            json!({"revision":view.revision,"goal_id":goal,"task_id":task.id,"record":record,"reason":record.map(|r|r.reason.clone()).unwrap_or(crate::telemetry::DiagnosticReason::Unknown)}),
        )
    }
    pub async fn bootstrap(&self, state: KernelState) -> Result<()> {
        state.validate()?;
        let mut tx = Transaction::new(
            "kernel/bootstrap",
            vec![Mutation::Put {
                key: STATE_KEY.into(),
                value: serde_json::to_value(state)?,
            }],
        );
        tx.compares.push(Compare {
            key: STATE_KEY.into(),
            expected: None,
        });
        ensure!(
            self.cluster.transact(tx).await?.applied,
            "deployment is already initialized"
        );
        Ok(())
    }
    pub async fn state(&self) -> Result<KernelState> {
        Ok(serde_json::from_value(
            self.cluster
                .read(STATE_KEY)
                .await?
                .value
                .context("deployment not initialized")?,
        )?)
    }
    pub(crate) async fn control_state(&self) -> Result<KernelState> {
        Ok(serde_json::from_value(
            self.cluster
                .read_control(STATE_KEY)
                .await?
                .value
                .context("deployment not initialized")?,
        )?)
    }
    pub(crate) async fn configure_egress(
        &self,
        executor: &AuthenticatedSubject,
        config: &crate::flow::FlowConfig,
    ) -> Result<()> {
        config.validate()?;
        self.runtime_control_commit(
            &format!("egress-config/{}", hex(&content_hash(config)?)),
            executor,
            config,
            |state| {
                state
                    .egress
                    .configure(config.clone(), crate::cluster::now_ms())?;
                Ok(json!({"configured":true}))
            },
        )
        .await?
        .require()?;
        Ok(())
    }
    pub(crate) async fn observe_flow(
        &self,
        executor: &AuthenticatedSubject,
        flow: crate::flow::ObservedFlow,
    ) -> Result<()> {
        self.runtime_control_commit(
            &format!("egress/{}", hex(&content_hash(&flow)?)),
            executor,
            &flow,
            |state| {
                let effect = state
                    .effects
                    .get(&flow.intent_id)
                    .context("flow has no retained effect")?;
                ensure!(
                    effect.permit.payload.holder == flow.holder
                        && effect.permit.payload.goal_id == flow.goal_id
                        && effect.request.action.operation == flow.operation
                        && effect.request.action.resource == flow.resource
                        && effect
                            .task_context
                            .as_ref()
                            .is_some_and(|task| task.id == flow.task_id),
                    "flow attribution differs from admitted effect"
                );
                ensure!(
                    effect.state != "pending",
                    "flow cannot precede committed dispatch"
                );
                match flow.route {
                    crate::flow::RouteKind::Provider => ensure!(
                        flow.operation == "inference"
                            && effect.request.action.recipient.as_deref() == Some(&flow.recipient),
                        "flow does not match its admitted provider route"
                    ),
                    crate::flow::RouteKind::Target => {
                        let endpoint = effect.payload["execution_manifest"]["targets"]
                            [&flow.resource]["endpoint"]
                            .as_str()
                            .context("flow has no frozen target receiver")?;
                        ensure!(
                            flow.operation == "target_read"
                                && flow.recipient == flow.resource
                                && reqwest::Url::parse(endpoint)?.origin()
                                    == reqwest::Url::parse(&flow.destination)?.origin(),
                            "flow differs from its frozen target receiver"
                        );
                    }
                }
                let now = crate::cluster::now_ms();
                let flags = state.egress.record(flow.clone(), now)?;
                for flag in flags {
                    state.audit.append_committed(AuditEvent {
                        correlation: None,
                        id: format!("egress-flag/{}", flag.id),
                        kind: "egress_anomaly".into(),
                        actor: flag.holder.clone(),
                        goal: Some(flag.goal_id.clone()),
                        timestamp: now / 1000,
                        decision: format!("{:?}", flag.kind),
                        payload_hash: content_hash(&flag)?,
                    })?;
                }
                Ok(json!({"recorded":flow.intent_id}))
            },
        )
        .await?
        .require()?;
        Ok(())
    }
    pub(crate) async fn validate_cached_effect(
        &self,
        subject: &AuthenticatedSubject,
        intent: &str,
        leases: &[LeaseCheck],
    ) -> Result<()> {
        let now = crate::cluster::now_ms();
        subject.ensure_current(now / 1000)?;
        check_allocation(subject, leases)?;
        let view = self.cluster.view().await?;
        let state: KernelState = serde_json::from_value(
            view.kv
                .get(STATE_KEY)
                .context("deployment not initialized")?
                .clone(),
        )?;
        let effect = state.effects.get(intent).context("cached effect missing")?;
        action_service(&state.modules, &effect.request.action.operation)?;
        ensure!(
            effect.state == "done" && effect.permit.payload.holder == subject.id(),
            "cached effect holder or state changed"
        );
        let grant = state
            .security
            .grants
            .get(&effect.request.grant_id)
            .context("cached effect grant absent")?;
        let goal = state.domain.goal(&grant.goal_id)?;
        let final_completion = final_task_completion(
            &state,
            &effect.request,
            &effect.payload,
            effect.task_context.as_ref(),
            now,
        );
        ensure!(
            (matches!(goal.status.as_str(), "active" | "awaiting_evaluation")
                || goal.status == "completed" && final_completion)
                && grant.version == goal.spec.version
                && grant.version == effect.request.grant_version
                && grant.holder.uri()? == subject.id()
                && !state.security.stopped(&Scope::Agent {
                    tenant: grant.holder.tenant.clone(),
                    goal: grant.goal_id.clone(),
                    holder: subject.id().into()
                }),
            "cached effect Goal or authority retired"
        );
        check_search_action(
            &state,
            &effect.request,
            &effect.payload,
            effect.task_context.as_ref(),
            now,
        )?;
        if effect.request.action.operation == "inference" {
            ensure!(
                search_open(&state, &grant.goal_id),
                "final selection closed model result delivery"
            );
        }
        let mut ancestor = Some(grant.id.as_str());
        let mut seen = BTreeSet::new();
        while let Some(id) = ancestor {
            let current = state
                .security
                .grants
                .get(id)
                .context("cached grant ancestor absent")?;
            ensure!(
                seen.insert(id)
                    && seen.len() <= 5
                    && !current.revoked
                    && current.lease.not_before * 1000 <= now
                    && current.lease.expiry * 1000 > now,
                "cached grant expired or revoked"
            );
            ancestor = current.parent.as_deref();
        }
        for check in leases {
            ensure!(
                view.leases
                    .get(&check.id)
                    .is_some_and(|lease| !lease.revoked
                        && lease.holder == check.holder
                        && lease.fence == check.fence
                        && lease.expires_ms > now),
                "cached effect allocation fenced"
            );
        }
        let context = effect
            .task_context
            .as_ref()
            .context("cached effect Task context absent")?;
        let task = state.domain.task(&grant.goal_id, &context.id)?;
        ensure!(
            task.claimed_by.as_deref() == Some(subject.id())
                && task.goal_version == goal.spec.version
                && task.incarnation == context.incarnation
                && task.fencing_token == context.fence
                && task.lease_expiry_ms > now
                && matches!(
                    task.state,
                    domain::TaskState::Claimed
                        | domain::TaskState::Running
                        | domain::TaskState::Completed
                ),
            "cached effect Task fenced"
        );
        let clearance = &grant
            .capabilities
            .iter()
            .find(|cap| cap.id == effect.request.capability_id)
            .context("cached effect Capability absent")?
            .constraints
            .read_markings;
        let mut roots = state
            .security
            .observed
            .get(subject.id())
            .cloned()
            .unwrap_or_default();
        roots.extend(
            state
                .task_input_sources
                .get(&format!("{}/{}", grant.goal_id, context.id))
                .cloned()
                .unwrap_or_default(),
        );
        if let Some(ids) = effect.payload["input_sources"].as_array() {
            roots.extend(ids.iter().filter_map(|id| id.as_str().map(String::from)));
        }
        if effect.request.action.operation == "inference" {
            roots.insert(format!("context/{intent}"));
        }
        for source in [
            format!("response/{intent}"),
            format!("tool-result/{intent}"),
        ] {
            if state.security.inputs.contains_key(&source) {
                roots.insert(source);
            }
        }
        ensure!(
            lineage_live(&state, &roots),
            "cached effect input lineage retired or absent"
        );
        ensure!(
            lineage_markings(&state, &roots)?.is_subset(clearance),
            "cached effect classifications exceed current read Capability"
        );
        Ok(())
    }
    pub(crate) async fn cached_local_application(
        &self,
        subject: &AuthenticatedSubject,
        intent: &str,
        payload: &Value,
        capability: (security::CapabilityKind, &str, &str),
        leases: &[LeaseCheck],
    ) -> Result<Option<Value>> {
        let _writer = self.cluster.kernel_write(false).await;
        let state = self.state().await?;
        let Some(effect) = state.effects.get(intent) else {
            return Ok(None);
        };
        if effect.state != "done" {
            return Ok(None);
        }
        ensure!(
            !effect.permit.payload.billable
                && effect.permit.payload.holder == subject.id()
                && &effect.payload == payload
                && effect.request.action.kind == capability.0
                && effect.request.action.operation == capability.1
                && effect.request.action.resource == capability.2,
            "local tool request ID conflict"
        );
        let result = effect
            .result
            .clone()
            .context("completed local effect result absent")?;
        drop(state);
        self.validate_cached_effect(subject, intent, leases).await?;
        Ok(Some(result))
    }
    pub(crate) async fn open_planning_round(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        inference_intent: &str,
        leases: Vec<LeaseCheck>,
    ) -> Result<Reply> {
        check_allocation(subject, &leases)?;
        let now = crate::cluster::now_ms();
        self.commit(id,subject.id(),&inference_intent,leases,|state| {
            let effect=state.effects.get(inference_intent).context("planning round lacks completed inference")?;
            ensure!(effect.state=="done" && effect.request.action.operation=="inference" && effect.permit.payload.holder==subject.id(),"planning round requires producer's recorded successful inference");
            let effect=effect.clone();
            action_service(&state.modules,&effect.request.action.operation)?;
            check_search_action(state,&effect.request,&effect.payload,effect.task_context.as_ref(),crate::cluster::now_ms())?;
            Ok(open_completed_round(state,&effect,inference_intent,now).unwrap_or_else(||json!({"planning_round":null,"status":"completion-retained-search-not-eligible"})))
        }).await
    }
    pub(crate) async fn evaluator_bridge(
        &self,
        id: &str,
        peer: &crate::transport::Peer,
        intent: &str,
        batch: &crate::evaluator::EvaluationBatch,
        ca: &[u8],
        leases: Vec<LeaseCheck>,
    ) -> Result<Reply> {
        check_allocation(&peer.subject, &leases)?;
        let now = crate::cluster::now_ms();
        self.commit_local(id,&peer.subject,&(intent,batch),leases,|state| {
            let effect=state.effects.get(intent).context("evaluation lacks admitted compiled-grant intent")?.clone();
            ensure!(effect.state=="pending" && !effect.permit.payload.billable && effect.permit.payload.holder==peer.subject.id()
                && effect.payload==serde_json::to_value(batch)? && effect.request.action.operation=="evaluate"
                && effect.request.action.kind==security::CapabilityKind::ToolCall,"evaluation authority or payload mismatch");
            ensure!(state.security.policy.payload.trusted_components.contains(peer.subject.id()) && batch.result.payload.evaluator_id==peer.subject.id(),"registered trusted evaluator required");
            let request=batch.resolve_request(state.evaluation_jobs.values().map(|job|&job.request),&self.signer.public_key())?.clone();
            let job=state.evaluation_jobs.values().find(|job|job.request.payload.request_id==batch.request_id).context("evaluation job missing")?;
            ensure!(job.status=="measured" && content_hash(job.result.as_ref().context("evaluation job lacks measured result")?)?==content_hash(&batch.result)?,"evaluation result is not the durably measured exact job result");
            ensure!(peer.public_key==state.security.goals.get(&request.payload.goal_id).context("evaluation Goal missing")?.evaluator_public_key,"evaluator signing key differs from operator-approved contract");
            crate::evaluator::verify_current(&batch.result,&request,&state.domain,&peer.certificate_der,ca,now)?;
            let grant=state.security.grants.get(&effect.request.grant_id).context("evaluator grant absent")?;
            let capability=grant.capabilities.iter().find(|capability|capability.id==effect.request.capability_id).context("evaluator capability absent")?;
            for finding in &batch.result.payload.finding_ids {
                ensure!(state.domain.findings[finding].markings.is_subset(&capability.constraints.read_markings),"evaluator evidence outside compiled clearance");
            }
            if let Some(confirmation)=refresh_nonbillable_effect(state,&peer.subject,intent,&self.signer,now/1000)? {return Ok(confirmation);}
            let effect=state.effects[intent].clone();
            state.security.consume_permit(&peer.subject,&effect.permit,&mut state.ledger,now/1000)?;
            let actor=Actor {id:peer.subject.id().into(),role:domain::Role::Evaluator};
            for signed in &batch.result.payload.commands {
                let mut command=signed.command.clone();
                let signature=serde_json::to_string(&json!({"message":signed.message,"certificate_hash":hex(&crate::audit::hash(&peer.certificate_der))}))?;
                set_signature(&mut command,&signature)?;
                state.domain.execute(&actor,command,now)?;
            }
            if batch.result.payload.phase==Some(crate::evaluator::EvaluationPhase::HeldOut) {
                let attachment=crate::evaluator::EvaluationResult::final_verdict(&batch.result)?;
                let key=format!("{}/{}",attachment.goal_id,attachment.contract.goal_version);
                if let Some(previous)=state.final_evaluations.get(&key) {ensure!(content_hash(previous)?==content_hash(&batch.result)?,"immutable final selection conflict");}
                state.final_evaluations.insert(key,batch.result.clone());
            }
            state.workload_certificates.insert(hex(&crate::audit::hash(&peer.certificate_der)),peer.certificate_der.clone());
            let result=json!({"evaluation":batch.result,"goal_status":state.domain.goal(&request.payload.goal_id)?.status});
            let stored=state.effects.get_mut(intent).unwrap();stored.state="done".into();stored.result=Some(result.clone());
            Ok(result)
        }).await
    }
    pub(crate) async fn runtime_commit<F>(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        request: &impl Serialize,
        mutate: F,
    ) -> Result<Reply>
    where
        F: Fn(&mut KernelState) -> Result<Value>,
    {
        self.commit(id, subject.id(), request, vec![], |state| {
            subject.ensure_current(crate::cluster::now_ms() / 1000)?;
            ensure!(
                state
                    .security
                    .policy
                    .payload
                    .trusted_components
                    .contains(subject.id()),
                "runtime identity is not an admitted trusted component"
            );
            mutate(state)
        })
        .await
    }
    pub(crate) async fn runtime_control_commit<F>(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        request: &impl Serialize,
        mutate: F,
    ) -> Result<Reply>
    where
        F: Fn(&mut KernelState) -> Result<Value>,
    {
        self.commit_control(id, subject.id(), request, vec![], |state| {
            subject.ensure_current(crate::cluster::now_ms() / 1000)?;
            ensure!(
                state
                    .security
                    .policy
                    .payload
                    .trusted_components
                    .contains(subject.id()),
                "control reconciliation requires an admitted runtime"
            );
            mutate(state)
        })
        .await
    }
    pub(crate) async fn audit_workload_denial(
        &self,
        executor: &AuthenticatedSubject,
        peer: &crate::transport::Peer,
        message: &crate::transport::Message,
        reason: &str,
    ) -> Result<()> {
        let proof_hash =
            content_hash(&(peer.subject.id(), peer.subject.certificate_hash(), message))?;
        let reason_hash = content_hash(&reason)?;
        let id = format!(
            "workload-denial-{}",
            hex(&content_hash(&(proof_hash, reason_hash))?)
        );
        let state = self.control_state().await?;
        let mut assigned = state.domain.tasks.values().filter(|task| {
            task.goal_id == peer.workload.goal
                && task.claimed_by.as_deref() == Some(peer.subject.id())
        });
        let first = assigned.next();
        let task = if assigned.next().is_none() {
            first.map(|record| TaskContext {
                id: record.spec.id.clone(),
                incarnation: record.incarnation,
                fence: record.fencing_token,
            })
        } else {
            None
        };
        let proposed_intent = if message.path == "/model" {
            Some(format!("model-{}", message.request_id))
        } else if message.path.starts_with("/tool/") {
            Some(format!("tool-{}", message.request_id))
        } else {
            None
        };
        let intent = proposed_intent.as_deref().filter(|intent| {
            state
                .effects
                .get(*intent)
                .is_some_and(|effect| effect.permit.payload.holder == peer.subject.id())
        });
        let correlation = state.correlation_for(
            peer.subject.id(),
            Some(&peer.workload.goal),
            task.as_ref(),
            intent,
        )?;
        let metadata = ActionCorrelationMetadata::AuthenticatedWorkloadDenial {
            correlation: correlation.clone(),
            proof_hash,
            reason_hash,
        };
        self.runtime_commit(&id, executor, &metadata, |state| {
            let timestamp = (crate::cluster::now_ms() / 1000).max(
                state
                    .audit
                    .events()
                    .last()
                    .map_or(0, |event| event.timestamp),
            );
            state.audit.append_committed(AuditEvent {
                correlation: Some(correlation.clone()),
                id: format!("denial/{id}"),
                kind: "workload_request_denied".into(),
                actor: peer.subject.id().into(),
                goal: Some(peer.workload.goal.clone()),
                timestamp,
                decision: "deny".into(),
                payload_hash: content_hash(&(proof_hash, reason_hash))?,
            })?;
            Ok(json!({"denial_recorded":true}))
        })
        .await?
        .require()?;
        Ok(())
    }
    pub(crate) async fn register_workload_certificate(
        &self,
        id: &str,
        runtime: &AuthenticatedSubject,
        grant_id: &str,
        leaf: &[u8],
        ca: &[u8],
        now: u64,
    ) -> Result<Reply> {
        self.register_workload_certificate_class(id, runtime, grant_id, leaf, ca, now, false)
            .await
    }
    pub(crate) async fn register_workload_certificate_control(
        &self,
        id: &str,
        runtime: &AuthenticatedSubject,
        grant_id: &str,
        leaf: &[u8],
        ca: &[u8],
        now: u64,
    ) -> Result<Reply> {
        self.register_workload_certificate_class(id, runtime, grant_id, leaf, ca, now, true)
            .await
    }
    #[allow(clippy::too_many_arguments)] // The renewal class retains the same certificate authority and grant checks.
    async fn register_workload_certificate_class(
        &self,
        id: &str,
        runtime: &AuthenticatedSubject,
        grant_id: &str,
        leaf: &[u8],
        ca: &[u8],
        now: u64,
        control: bool,
    ) -> Result<Reply> {
        self.commit_class(id, runtime.id(), &(grant_id, leaf), vec![], control, |state| {
            let now = (crate::cluster::now_ms() / 1000).max(now);
            runtime.ensure_current(now)?;
            ensure!(state.security.policy.payload.trusted_components.contains(runtime.id()),
                "certificate registration requires admitted runtime authority");
            let grant = state
                .security
                .grants
                .get(grant_id)
                .context("workload grant absent")?;
            ensure!(
                !grant.revoked && grant.lease.expiry > now,
                "workload grant revoked or expired"
            );
            if control {
                let holder = grant.holder.uri()?;
                let goal = state.domain.goal(&grant.goal_id)?;
                ensure!(if holder == goal.spec.evaluator_id {
                    matches!(goal.status.as_str(), "active" | "awaiting_evaluation")
                } else { search_open(state, &grant.goal_id) }, "final selection closed workload renewal");
                ensure!(state.workload_certificates.values().any(|old| {
                    let Ok((_, certificate)) = x509_parser::parse_x509_certificate(old) else { return false };
                    certificate.subject_alternative_name().is_ok_and(|san| san.is_some_and(|san|
                        san.value.general_names.iter().any(|name| matches!(name, x509_parser::extensions::GeneralName::URI(uri) if *uri == holder))))
                }), "control capacity is reserved for already enrolled workload renewal");
            }
            crate::identity::verify_workload_certificate(leaf, ca, &grant.holder, now)?;
            if grant.holder.uri()? == state.domain.goal(&grant.goal_id)?.spec.evaluator_id {
                ensure!(
                    crate::identity::workload_public_key(leaf)?
                        == state.security.goals[&grant.goal_id].evaluator_public_key,
                    "evaluator certificate key is not independently pinned"
                );
            }
            let digest = hex(&crate::audit::hash(leaf));
            state
                .workload_certificates
                .insert(digest.clone(), leaf.into());
            Ok(json!({"certificate_hash":digest}))
        })
        .await
    }
    pub(crate) async fn record_certificate(&self, peer: &crate::transport::Peer) -> Result<()> {
        let digest = crate::audit::hash(&peer.certificate_der);
        ensure!(
            peer.subject.certificate_hash() == Some(digest),
            "certificate differs from authenticated subject"
        );
        self.commit(
            &format!("certificate-{}", hex(&digest)),
            peer.subject.id(),
            &peer.certificate_der,
            vec![],
            |state| {
                ensure!(
                    state
                        .security
                        .grants
                        .values()
                        .any(|grant| grant.holder == peer.workload && !grant.revoked),
                    "certificate holder has no admitted grant"
                );
                if peer.subject.id() == state.domain.goal(&peer.workload.goal)?.spec.evaluator_id {
                    ensure!(
                        peer.public_key
                            == state.security.goals[&peer.workload.goal].evaluator_public_key,
                        "evaluator certificate key is not independently pinned"
                    );
                }
                state
                    .workload_certificates
                    .insert(hex(&digest), peer.certificate_der.clone());
                Ok(json!({"certificate_hash":hex(&digest)}))
            },
        )
        .await?
        .require()?;
        Ok(())
    }
    async fn commit<F>(
        &self,
        id: &str,
        actor: &str,
        request: &impl Serialize,
        leases: Vec<LeaseCheck>,
        mutate: F,
    ) -> Result<Reply>
    where
        F: Fn(&mut KernelState) -> Result<Value>,
    {
        self.commit_class(id, actor, request, leases, false, mutate)
            .await
    }
    async fn commit_control<F>(
        &self,
        id: &str,
        actor: &str,
        request: &impl Serialize,
        leases: Vec<LeaseCheck>,
        mutate: F,
    ) -> Result<Reply>
    where
        F: Fn(&mut KernelState) -> Result<Value>,
    {
        self.commit_class(id, actor, request, leases, true, mutate)
            .await
    }
    async fn commit_class<F>(
        &self,
        id: &str,
        actor: &str,
        request: &impl Serialize,
        leases: Vec<LeaseCheck>,
        control: bool,
        mutate: F,
    ) -> Result<Reply>
    where
        F: Fn(&mut KernelState) -> Result<Value>,
    {
        self.commit_transaction(id, actor, request, leases, (control, None), mutate)
            .await
    }
    async fn commit_local<F>(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        request: &impl Serialize,
        leases: Vec<LeaseCheck>,
        mutate: F,
    ) -> Result<Reply>
    where
        F: Fn(&mut KernelState) -> Result<Value>,
    {
        #[cfg(test)]
        admission_sync::after_review(&format!("application:{id}")).await;
        self.commit_transaction(
            id,
            subject.id(),
            request,
            leases,
            (false, Some(subject)),
            mutate,
        )
        .await
    }
    async fn commit_transaction<F>(
        &self,
        id: &str,
        actor: &str,
        request: &impl Serialize,
        leases: Vec<LeaseCheck>,
        class: (bool, Option<&AuthenticatedSubject>),
        mutate: F,
    ) -> Result<Reply>
    where
        F: Fn(&mut KernelState) -> Result<Value>,
    {
        let (control, local_subject) = class;
        let local_application = local_subject.is_some();
        ensure!(
            !id.is_empty() && id.len() <= 200 && !actor.is_empty(),
            "invalid request identity"
        );
        let request_hash = content_hash(&(actor, request))?;
        let request_metadata = serde_json::to_value(request)?;
        let _writer = self.cluster.kernel_write(control).await;
        for _ in 0..16 {
            let read = if control {
                self.cluster.read_control(STATE_KEY).await?
            } else {
                self.cluster.read(STATE_KEY).await?
            };
            let old = read.value.context("deployment not initialized")?;
            let state: KernelState = serde_json::from_value(old.clone())?;
            let binding_id = format!("application-binding/{}", hex(&content_hash(&id)?));
            if local_application && let Some(binding) = state.replies.get(&binding_id) {
                ensure!(
                    binding.actor == actor && binding.request_hash == request_hash,
                    "application request identity reused for different content"
                );
            }
            if let Some(reply) = state.replies.get(id) {
                ensure!(
                    reply.request_hash == request_hash,
                    "request identity reused for different content"
                );
                if reply.ok
                    && let Some(subject) = local_subject
                {
                    let binding = serde_json::to_value(request)?;
                    let intent = binding[0]
                        .as_str()
                        .context("local application intent absent")?;
                    let effect = state
                        .effects
                        .get(intent)
                        .context("cached local effect absent")?;
                    if effect.task_context.is_none()
                        && effect.request.action.operation == "evaluate"
                    {
                        let goal = state.domain.goal(&effect.permit.payload.goal_id)?;
                        ensure!(
                            effect.state == "done"
                                && effect.permit.payload.holder == subject.id()
                                && goal.spec.evaluator_id == subject.id()
                                && state
                                    .security
                                    .policy
                                    .payload
                                    .trusted_components
                                    .contains(subject.id())
                                && goal.spec.version == effect.request.grant_version
                                && matches!(
                                    goal.status.as_str(),
                                    "active" | "awaiting_evaluation" | "completed"
                                ),
                            "cached evaluator acknowledgement authority retired"
                        );
                        action_service(&state.modules, &effect.request.action.operation)?;
                        let now = crate::cluster::now_ms();
                        state.security.confirmation_review(
                            subject,
                            &effect.request,
                            &state.ledger,
                            now / 1000,
                        )?;
                        check_allocation(subject, &leases)?;
                        let view = self.cluster.view().await?;
                        for check in &leases {
                            ensure!(
                                view.leases
                                    .get(&check.id)
                                    .is_some_and(|lease| !lease.revoked
                                        && lease.holder == check.holder
                                        && lease.fence == check.fence
                                        && lease.expires_ms > now),
                                "cached evaluator allocation fenced"
                            );
                        }
                    } else {
                        self.validate_cached_effect(subject, intent, &leases)
                            .await?;
                    }
                }
                return Ok(Reply {
                    ok: reply.ok,
                    value: reply.value.clone(),
                    duplicate: true,
                    revision: read.revision,
                });
            }
            let mut next = state.clone();
            let audit_start = next.audit.events().len();
            let (ok, value) = match mutate(&mut next) {
                Ok(value) => {
                    next.validate()?;
                    (true, value)
                }
                Err(error) => {
                    next = state;
                    let mut value = json!({"error":error.to_string()});
                    if let Some(paused) = error.downcast_ref::<crate::quota::QuotaPaused>() {
                        value["code"] = json!("provider_paused");
                        value["retry_after_ms"] = json!(paused.retry_after_ms);
                        value["quota_pause"] = serde_json::to_value(paused)?;
                        let correlation = committed_correlation(&next, actor, &request_metadata)?;
                        if let (Some(goal), Some(task), Some(values)) = (
                            &correlation.goal_id,
                            &correlation.task_id,
                            request_metadata.as_array(),
                        ) && values.len() == 9
                        {
                            let request: crate::quota::QuotaRequest =
                                serde_json::from_value(values[4].clone())?;
                            let provider = values[8]
                                .as_str()
                                .context("quota pause provider absent")?
                                .to_owned();
                            let version = next.domain.goal(goal)?.spec.version;
                            next.rate_pauses.insert(
                                diagnostic_key(goal, Some(task)),
                                crate::telemetry::RatePause {
                                    goal_version: version,
                                    request,
                                    provider,
                                    pause: paused.clone(),
                                },
                            );
                            let record = crate::telemetry::DiagnosticRecord {
                                goal_id: goal.clone(),
                                goal_version: version,
                                task_id: Some(task.clone()),
                                task_fence: next
                                    .domain
                                    .task(goal, task)
                                    .ok()
                                    .map(|t| t.fencing_token),
                                reason: crate::telemetry::DiagnosticReason::RateLimited {
                                    retry_after_ms: paused.retry_after_ms,
                                    quota_kind: paused.kind,
                                },
                                observed_revision: read.revision,
                                recorded_ms: crate::cluster::now_ms(),
                                markings: diagnostic_markings(&next, goal, Some(task))?,
                                correlation: correlation.clone(),
                            };
                            store_diagnostic(&mut next, diagnostic_key(goal, Some(task)), record);
                        }
                    }
                    if error
                        .downcast_ref::<crate::ledger::BudgetExhausted>()
                        .is_some()
                    {
                        value["code"] = json!("budget_exhausted");
                        let correlation = committed_correlation(&next, actor, &request_metadata)?;
                        if let Some(goal) = &correlation.goal_id
                            && let Ok(registered) = next.domain.goal(goal)
                        {
                            let record = crate::telemetry::DiagnosticRecord {
                                goal_id: goal.clone(),
                                goal_version: registered.spec.version,
                                task_id: correlation.task_id.clone(),
                                task_fence: correlation.task_id.as_ref().and_then(|id| {
                                    next.domain.task(goal, id).ok().map(|t| t.fencing_token)
                                }),
                                reason: crate::telemetry::DiagnosticReason::BudgetExhausted,
                                observed_revision: read.revision,
                                recorded_ms: crate::cluster::now_ms(),
                                markings: diagnostic_markings(
                                    &next,
                                    goal,
                                    correlation.task_id.as_deref(),
                                )?,
                                correlation: correlation.clone(),
                            };
                            next.budget_pauses.insert(
                                diagnostic_key(goal, correlation.task_id.as_deref()),
                                record.goal_version,
                            );
                            store_diagnostic(
                                &mut next,
                                diagnostic_key(goal, correlation.task_id.as_deref()),
                                record,
                            );
                        }
                    }
                    if error.downcast_ref::<StateCapacityError>().is_some() {
                        value["code"] = json!("state_capacity");
                    }
                    (false, value)
                }
            };
            let timestamp = (crate::cluster::now_ms() / 1000)
                .max(next.audit.events().last().map_or(0, |e| e.timestamp));
            // Review replies stay immutable without trapping the stable intent in an old review.
            // Successful application and terminal failures keep the base id for exact replay.
            let record_id = if local_application && ok && value["status"] == "confirmation_required"
            {
                let review: security::ConfirmationReview =
                    serde_json::from_value(value["review"].clone())?;
                let selected = next.security.review_approvals(
                    &review,
                    next.action_approvals.values().cloned(),
                    crate::cluster::now_ms() / 1000,
                )?;
                format!(
                    "apply-phase/{}",
                    hex(&content_hash(&(id, &review, &selected))?)
                )
            } else {
                id.into()
            };
            if let Some(reply) = next.replies.get(&record_id) {
                ensure!(
                    reply.request_hash == request_hash,
                    "application phase reused for different content"
                );
                return Ok(Reply {
                    ok: reply.ok,
                    value: reply.value.clone(),
                    duplicate: true,
                    revision: read.revision,
                });
            }
            if local_application && record_id != id && !next.replies.contains_key(&binding_id) {
                next.audit.append_committed(AuditEvent {
                    correlation: None,
                    id: format!("request/{binding_id}"),
                    kind: "kernel_request".into(),
                    actor: actor.into(),
                    goal: None,
                    timestamp,
                    decision: "allow".into(),
                    payload_hash: request_hash,
                })?;
                next.replies.insert(
                    binding_id.clone(),
                    RecordedReply {
                        correlation: None,
                        actor: actor.into(),
                        request: serde_json::to_value(request)?,
                        request_hash,
                        ok: true,
                        value: json!({"status":"application_bound"}),
                    },
                );
            }
            next.audit.append_committed(AuditEvent {
                correlation: None,
                id: format!("request/{record_id}"),
                kind: "kernel_request".into(),
                actor: actor.into(),
                goal: None,
                timestamp,
                decision: if ok { "allow" } else { "deny" }.into(),
                payload_hash: request_hash,
            })?;
            next.replies.insert(
                record_id.clone(),
                RecordedReply {
                    correlation: None,
                    actor: actor.into(),
                    request: serde_json::to_value(request)?,
                    request_hash,
                    ok,
                    value: value.clone(),
                },
            );
            let correlation = committed_correlation(&next, actor, &request_metadata)?;
            next.audit.correlate_new(audit_start, &correlation);
            correlate_action_records(&old, &mut next, &correlation);
            if let Some(reply) = next.replies.get_mut(&record_id) {
                reply.correlation = Some(correlation.clone());
            }
            if let Some(reply) = next.replies.get_mut(&binding_id) {
                reply.correlation = Some(correlation.clone());
            }
            next.checkpoint(&record_id, actor, timestamp)?;
            let encoded = serde_json::to_vec(&next)?;
            ensure_state_capacity(&next, encoded.len(), self.max_state_bytes, control)?;
            let mut tx = Transaction::new(
                uuid::Uuid::new_v4().to_string(),
                vec![Mutation::Put {
                    key: STATE_KEY.into(),
                    value: serde_json::from_slice(&encoded)?,
                }],
            );
            tx.compares.push(Compare {
                key: STATE_KEY.into(),
                expected: Some(old),
            });
            tx.leases = leases.clone();
            let result = if control {
                self.cluster.transact_control(tx).await?
            } else {
                self.cluster.transact(tx).await?
            };
            if result.applied {
                return Ok(Reply {
                    ok,
                    value,
                    duplicate: false,
                    revision: result.revision,
                });
            }
            ensure!(
                result.error.as_deref() == Some("compare-and-swap conflict"),
                "transaction refused: {:?}",
                result.error
            );
        }
        anyhow::bail!("concurrent state changes exceeded retry bound")
    }
    pub async fn register_goal(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        spec: domain::GoalSpec,
        posture: GoalPosture,
        approvals: Vec<Signed<HumanApproval>>,
    ) -> Result<Reply> {
        let now = crate::cluster::now_ms();
        self.commit(
            id,
            subject.id(),
            &(&spec, &posture, &approvals),
            vec![],
            |state| {
                let now = (crate::cluster::now_ms()).max(now);
                validate_goal_pair(&spec, &posture)?;
                state.security.register_goal(
                    subject,
                    posture.clone(),
                    &approvals,
                    &mut state.ledger,
                    now / 1000,
                )?;
                state.domain.execute(
                    &Actor {
                        id: subject.id().into(),
                        role: domain::Role::Operator,
                    },
                    DomainCommand::RegisterGoal { spec: spec.clone() },
                    now,
                )
            },
        )
        .await
    }
    pub async fn define_agent(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        definition: AgentDefinition,
        parent: Option<String>,
        envelope: Envelope,
    ) -> Result<Reply> {
        let now = crate::cluster::now_ms() / 1000;
        self.commit(
            id,
            subject.id(),
            &(&definition, &parent, &envelope),
            vec![],
            |state| {
                let now = (crate::cluster::now_ms() / 1000).max(now);
                let tenant = state.security.policy.payload.tenant.clone();
                ensure!(
                    state.security.permitted(
                        subject,
                        "define",
                        "agents",
                        &Scope::Goal {
                            tenant,
                            goal: definition.goal_id.clone()
                        },
                        envelope.allocation,
                        ConfirmationTier::C1,
                        IsolationTier::P,
                        now
                    ),
                    "agent definition denied"
                );
                ensure!(
                    envelope.holder == definition.holder.uri()?
                        && envelope.goal_id == definition.goal_id,
                    "envelope holder/Goal mismatch"
                );
                state.ledger.allocate(envelope.clone())?;
                Ok(serde_json::to_value(state.security.compile_definition(
                    definition.clone(),
                    parent.clone(),
                    &mut state.ledger,
                    now,
                )?)?)
            },
        )
        .await
    }
    pub async fn admit(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        admission: Admission,
    ) -> Result<Reply> {
        let Admission {
            request,
            payload,
            prices,
            approvals,
            leases,
            quota,
            task,
        } = admission;
        for _ in 0..16 {
            let now = crate::cluster::now_ms() / 1000;
            check_allocation(subject, &leases)?;
            // Each review phase gets an immutable audit reply, while the effect intent remains stable.
            let state = self.state().await?;
            let reviewed = (|| -> Result<_> {
                let review =
                    state
                        .security
                        .confirmation_review(subject, &request, &state.ledger, now)?;
                let candidates = state
                    .action_approvals
                    .values()
                    .cloned()
                    .chain(approvals.iter().cloned());
                let selected = state.security.review_approvals(&review, candidates, now)?;
                Ok((review, selected))
            })();
            let (review, selected) = match reviewed {
                Ok(value) => value,
                Err(error) => {
                    let phase = content_hash(&(
                        "denied-review",
                        state.security.grant_epoch,
                        state.security.stop_epoch,
                        state.security.retraction_epoch,
                        &request,
                        &payload,
                    ))?;
                    let reason = error.to_string();
                    drop(state);
                    return self
                        .commit(
                            &format!("{id}/{}", hex(&phase)),
                            subject.id(),
                            &(
                                content_hash(&request)?,
                                content_hash(&payload)?,
                                content_hash(&prices)?,
                                content_hash(&approvals)?,
                                &quota,
                                &task,
                            ),
                            leases,
                            |_| Err(anyhow::anyhow!(reason.clone())),
                        )
                        .await;
                }
            };
            let phase = content_hash(&(
                review.grant_epoch,
                review.stop_epoch,
                review.retraction_epoch,
                &review.action_hash,
                &review.evidence_hash,
                &selected,
            ))?;
            let phase_id = format!("{id}/{}", hex(&phase));
            let mut admission_correlation = state.correlation_for(
                subject.id(),
                Some(&state.security.grants[&request.grant_id].goal_id),
                task.as_ref(),
                Some(&request.intent_id),
            )?;
            // Admission describes the frozen pre-execution action. Later dispatch/receipt
            // observation must not alter this exact replay binding.
            admission_correlation.execution_attempt = None;
            drop(state); // The immutable review owns everything needed; do not queue a full deployment snapshot.
            #[cfg(test)]
            admission_sync::after_review(&request.intent_id).await;
            let reply = self.commit(&phase_id, subject.id(), &(content_hash(&request)?, content_hash(&payload)?, content_hash(&prices)?, content_hash(&approvals)?, &quota, &task,&phase,&admission_correlation,&prices.as_ref().map(|price|price.payload.provider.clone())), leases.clone(), |state| {
                let now=(crate::cluster::now_ms() / 1000).max(now);
            action_service(&state.modules,&request.action.operation)?;
            check_search_action(state,&request,&payload,task.as_ref(),crate::cluster::now_ms())?;
            if let Some(price)=&prices { ensure!(price.payload.provider.len()<=128 && price.payload.provider.bytes().all(|byte|byte.is_ascii_alphanumeric() || b"_./-".contains(&byte)) && price.payload.rates.len()<=64 && price.payload.rates.keys().all(|key| !key.is_empty() && key.len()<=128 && key.bytes().all(|byte|byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))),"price category metadata exceeds bounded receipt contract"); }
            if matches!(request.action.operation.as_str(),"inference"|"attempt_submit"){ensure!(search_open(state,&state.security.grants[&request.grant_id].goal_id),"final selection closed paid search for this Goal version");}
            ensure!(request.action.arguments == payload, "admitted payload differs from authorized arguments");
            check_task(state, subject, task.as_ref(), payload["kind"] == "claim_task")?;
            if request.envelope_id.is_some() && task.as_ref().is_some_and(|task|state.budget_paused(&state.security.grants[&request.grant_id].goal_id,&task.id)) {
                return Err(crate::ledger::BudgetExhausted{category:"paused Task"}.into());
            }
            let mut review=state.security.confirmation_review(subject,&request,&state.ledger,now)?;
            if let Some(price)=&prices { review.expiry=review.expiry.min(price.payload.expires); }
            if let Some(quota)=&quota { review.expiry=review.expiry.min(quota.deadline_ms/1000); }
            let selected=state.security.review_approvals(&review,state.action_approvals.values().cloned().chain(approvals.iter().cloned()),now)?;
            if content_hash(&(review.grant_epoch,review.stop_epoch,review.retraction_epoch,&review.action_hash,&review.evidence_hash,&selected))?!=phase { return Ok(json!({"status":"authority_changed","intent":request.intent_id})); }
            if let Some(existing)=state.pending_actions.get(&request.intent_id) {
                ensure!(existing.request==request && existing.payload==payload && content_hash(&existing.prices)?==content_hash(&prices)?
                    && existing.quota==quota && content_hash(&existing.task)?==content_hash(&task)?
                    && existing.review.holder==subject.id() && existing.review.certificate_hash==subject.certificate_hash(),"pending intent changed identity, payload or bound resources");
                ensure!(now<existing.review.expiry,"pending action deadline expired; create a new bounded intent");
            }
            if !state.security.review_confirmed(&review,&selected,now)? {
                let pending=PendingAction{request:request.clone(),payload:payload.clone(),prices:prices.clone(),quota:quota.clone(),task:task.clone(),review:review.clone()};
                state.pending_actions.insert(request.intent_id.clone(),pending);
                state.audit.append_committed(AuditEvent{ correlation: None,id:format!("review/{}/{}",request.intent_id,hex(&phase)),kind:"confirmation_required".into(),actor:subject.id().into(),goal:Some(state.security.grants[&request.grant_id].goal_id.clone()),timestamp:now,decision:"pending_human_confirmation".into(),payload_hash:content_hash(&review)?})?;
                return Ok(json!({"status":"confirmation_required","intent":request.intent_id,"review":review}));
            }
            let decision = state.security.authorize(subject, request.clone(), &selected, prices.as_ref(), &mut state.ledger, &self.signer, now)?;
            if let Some(quota) = &quota {
                ensure!(quota.intent == request.intent_id && request.action.operation == "inference", "quota does not match inference intent");
                state.quotas.reserve(quota.clone(), &prices.as_ref().context("inference prices absent")?.payload.provider, now * 1000)?;
            } else { ensure!(request.action.operation != "inference", "inference requires shared quota"); }
            if let Some(existing) = state.effects.get(&request.intent_id) {
                ensure!(existing.request == request && existing.payload == payload, "effect ID reused with different payload");
                return Ok(effect_ack(existing));
            }
            state.pending_actions.remove(&request.intent_id);
            state.audit.append_committed(decision.audit)?;
            let effect = Effect { correlation: None, request: request.clone(), payload: payload.clone(), permit: decision.permit, state: "pending".into(), result: None, task_context: task.clone() };
            state.effects.insert(request.intent_id.clone(), effect.clone()); Ok(effect_ack(&effect))
        }).await?;
            if reply.ok && reply.value["status"] == "authority_changed" {
                continue;
            }
            return Ok(reply);
        }
        anyhow::bail!("admission authority did not stabilize within the bounded retry limit")
    }
    /// Only a newly committed dispatch may contact a receiver; duplicate replies are recovery reads.
    pub async fn dispatch(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        intent: &str,
        leases: Vec<LeaseCheck>,
    ) -> Result<Reply> {
        let now = crate::cluster::now_ms() / 1000;
        check_allocation(subject, &leases)?;
        self.commit(id, subject.id(), &intent, leases, |state| {
            let now = (crate::cluster::now_ms() / 1000).max(now);
            let effect = state.effects.get(intent).context("unknown effect")?.clone();
            action_service(&state.modules, &effect.request.action.operation)?;
            check_search_action(
                state,
                &effect.request,
                &effect.payload,
                effect.task_context.as_ref(),
                crate::cluster::now_ms(),
            )?;
            if effect.request.action.operation == "inference" {
                ensure!(
                    search_open(state, &effect.permit.payload.goal_id),
                    "final selection closed paid search for this Goal version"
                );
            }
            ensure!(
                effect.state == "pending",
                "effect already dispatched; reconciliation required"
            );
            check_task(state, subject, effect.task_context.as_ref(), false)?;
            state
                .security
                .consume_permit(subject, &effect.permit, &mut state.ledger, now)?;
            if effect.request.action.operation == "inference" {
                state.quotas.dispatch(intent, now * 1000)?;
            }
            state.effects.get_mut(intent).unwrap().state = "dispatched".into();
            Ok(serde_json::to_value(effect.permit)?)
        })
        .await
    }
    pub async fn settle(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        intent: &str,
        receipt: Option<Receipt>,
        result: Value,
        proven_not_dispatched: bool,
    ) -> Result<Reply> {
        let now = crate::cluster::now_ms() / 1000;
        let retained = self
            .retain_effect_result(subject, intent, &result, false)
            .await?;
        let receipt = receipt.map(normalize_receipt);
        if let Some(receipt) = &receipt {
            ensure!(
                serde_json::to_vec(receipt)?.len() <= MAX_RESULT_RECEIPT_BYTES,
                "receipt exceeded the pre-admitted bounded financial contract"
            );
        }
        self.commit_control(
            id,
            subject.id(),
            &(
                intent,
                &receipt,
                content_hash(&result)?,
                proven_not_dispatched,
            ),
            vec![],
            |state| {
                let now = (crate::cluster::now_ms() / 1000).max(now);
                ensure!(
                    state
                        .security
                        .policy
                        .payload
                        .trusted_components
                        .contains(subject.id()),
                    "only trusted executor may reconcile receipts"
                );
                let effect = state.effects.get(intent).context("unknown effect")?.clone();
                ensure!(
                    effect.state == "dispatched" || effect.state == "unknown",
                    "effect is not awaiting settlement"
                );
                let status = if let Some(receipt) = receipt.clone() {
                    ensure!(receipt.intent_id == intent, "receipt intent mismatch");
                    let settlement = state.ledger.settle(receipt)?;
                    if settlement.overrun {
                        state.security.engage_ledger_stop(
                            state.security.policy.payload.tenant.clone(),
                            effect.permit.payload.goal_id.clone(),
                            content_hash(&settlement)?,
                            now,
                        )?;
                    }
                    "done"
                } else if proven_not_dispatched {
                    state
                        .ledger
                        .release_proven_unbilled(intent, content_hash(&result)?)?;
                    "not_dispatched"
                } else {
                    state.ledger.mark_unknown(intent)?;
                    "unknown"
                };
                if effect.request.action.operation == "inference" {
                    if let Some(delay) = result["outcome"]["retry_after_ms"].as_u64() {
                        let scope = state
                            .quotas
                            .reservations
                            .get(intent)
                            .context("provider quota reservation absent")?
                            .request
                            .scope
                            .clone();
                        state.quotas.retry_after(&scope, now * 1000, delay)?;
                    }
                    let usage = receipt.as_ref().map(|r| {
                        (
                            r.usage
                                .get("input_tokens")
                                .copied()
                                .unwrap_or(0)
                                .saturating_add(
                                    r.usage.get("cached_input_tokens").copied().unwrap_or(0),
                                ),
                            r.usage.get("output_tokens").copied().unwrap_or(0),
                        )
                    });
                    if state.quotas.finish(intent, usage, proven_not_dispatched)? {
                        state.security.engage_ledger_stop(
                            state.security.policy.payload.tenant.clone(),
                            effect.permit.payload.goal_id.clone(),
                            content_hash(&result)?,
                            now,
                        )?;
                    }
                }
                let stored = state.effects.get_mut(intent).unwrap();
                stored.state = status.into();
                stored.result = Some(retained.clone());
                if retained["body_missing"] == true {
                    stop_result_retention(state, &effect, intent)?;
                }
                let value = effect_ack(state.effects.get(intent).unwrap());
                if status == "done"
                    && effect.request.action.operation == "inference"
                    && !result["completion"].is_null()
                {
                    // Financial settlement remains authoritative even if a late response can no
                    // longer open planning authority. Eligible rounds commit with this receipt.
                    let _ = open_completed_round(state, &effect, intent, crate::cluster::now_ms());
                }
                Ok(value)
            },
        )
        .await
    }
    pub(crate) async fn cancel_pending(
        &self,
        id: &str,
        executor: &AuthenticatedSubject,
        intent: &str,
        reason: String,
    ) -> Result<Reply> {
        self.runtime_control_commit(id, executor, &(intent, &reason), |state| {
            let effect = state
                .effects
                .get(intent)
                .context("pending effect absent")?
                .clone();
            ensure!(
                effect.state == "pending",
                "external dispatch already admitted; outcome reconciliation required"
            );
            if effect.permit.payload.billable {
                state.ledger.release_proven_unbilled(
                    intent,
                    content_hash(&("never-dispatched", intent, &reason))?,
                )?;
            }
            if effect.request.action.operation == "inference" {
                state.quotas.finish(intent, None, true)?;
            }
            let effect = state.effects.get_mut(intent).unwrap();
            effect.state = "not_dispatched".into();
            effect.result = Some(json!({"reason":reason,"upstream_contacted":false}));
            Ok(serde_json::to_value(effect)?)
        })
        .await
    }
    pub(crate) async fn complete_tool(
        &self,
        id: &str,
        executor: &AuthenticatedSubject,
        holder: &AuthenticatedSubject,
        intent: &str,
        result: crate::tools::ToolExecution,
    ) -> Result<Reply> {
        let body = serde_json::to_value(&result)?;
        let mut retained = self
            .retain_effect_result(executor, intent, &body, result.completed)
            .await?;
        let digest = content_hash(&body)?;
        let financial = self
            .commit_control(
                id,
                executor.id(),
                &(holder.id(), intent, digest),
                vec![],
                |state| {
                    ensure!(
                        state
                            .security
                            .policy
                            .payload
                            .trusted_components
                            .contains(executor.id()),
                        "only trusted executor may reconcile paid tool result"
                    );
                    let effect = state
                        .effects
                        .get(intent)
                        .context("tool effect absent")?
                        .clone();
                    ensure!(
                        effect.permit.payload.holder == holder.id()
                            && matches!(effect.state.as_str(), "dispatched" | "unknown"),
                        "tool result identity/state mismatch"
                    );
                    if effect.permit.payload.billable {
                        if result.completed {
                            let reservation = state
                                .ledger
                                .intents
                                .get(intent)
                                .context("tool reservation absent")?;
                            let receipt = normalize_receipt(Receipt {
                                id: format!("local/{intent}"),
                                provider: reservation.price.provider.clone(),
                                intent_id: intent.into(),
                                request_hash: reservation.request.request_hash,
                                usage: result.usage.clone(),
                            });
                            ensure!(
                                serde_json::to_vec(&receipt)?.len() <= MAX_RESULT_RECEIPT_BYTES,
                                "tool receipt exceeds admitted financial bound"
                            );
                            let settlement = state.ledger.settle(receipt)?;
                            if settlement.overrun {
                                state.security.engage_ledger_stop(
                                    state.security.policy.payload.tenant.clone(),
                                    effect.permit.payload.goal_id.clone(),
                                    content_hash(&settlement)?,
                                    crate::cluster::now_ms() / 1000,
                                )?;
                            }
                        } else {
                            state.ledger.mark_unknown(intent)?;
                        }
                    }
                    let stored = state.effects.get_mut(intent).unwrap();
                    stored.state = if result.completed { "done" } else { "unknown" }.into();
                    stored.result = Some(retained.clone());
                    if retained["body_missing"] == true {
                        stop_result_retention(state, &effect, intent)?;
                    }
                    Ok(retained.clone())
                },
            )
            .await?;
        if !financial.ok || !result.completed {
            return Ok(financial);
        }
        let post = self
            .runtime_control_commit(
                &format!("{id}-provenance"),
                executor,
                &(intent, digest),
                |state| {
                    let effect = state
                        .effects
                        .get(intent)
                        .context("tool effect absent")?
                        .clone();
                    ensure!(effect.state == "done", "tool financial settlement missing");
                    if result.completed {
                        if effect.request.action.operation == "artifact_retract" {
                            let affected: Vec<String> =
                                serde_json::from_value(result.value["retracted"].clone())?;
                            for artifact in affected {
                                let source = format!("artifact/{artifact}");
                                if state.security.retractions.contains_key(&source) {
                                    continue;
                                }
                                state.security.retraction_epoch = state
                                    .security
                                    .retraction_epoch
                                    .checked_add(1)
                                    .context("retraction epoch overflow")?;
                                let record = self.signer.sign(
                                    "finding-retraction",
                                    security::Retraction {
                                        finding_id: source.clone(),
                                        actor: holder.id().into(),
                                        reason_hash: content_hash(&effect.payload)?,
                                        timestamp: crate::cluster::now_ms() / 1000,
                                        epoch: state.security.retraction_epoch,
                                    },
                                )?;
                                state.security.retractions.insert(source, record);
                            }
                            let retired: Vec<_> = state
                                .finding_sources
                                .iter()
                                .filter_map(|(id, source)| {
                                    let proof = state.security.findings.get(source)?;
                                    (!lineage_live(state, &proof.payload.inputs))
                                        .then_some(id.clone())
                                })
                                .collect();
                            for id in retired {
                                let goal = state.domain.findings[&id].goal_id.clone();
                                let signature = serde_json::to_string(&self.signer.sign(
                                    "artifact-derived-finding-retraction",
                                    &(id.as_str(), intent),
                                )?)?;
                                state.domain.execute(
                                    &Actor {
                                        id: executor.id().into(),
                                        role: domain::Role::Operator,
                                    },
                                    DomainCommand::RetractFinding {
                                        goal_id: goal,
                                        finding_id: id,
                                        reason: "artifact ancestor retracted".into(),
                                        signature,
                                    },
                                    crate::cluster::now_ms(),
                                )?;
                            }
                        }
                        let source_id = format!("tool-result/{intent}");
                        let mut parents: BTreeSet<String> =
                            serde_json::from_value(effect.payload["input_sources"].clone())?;
                        parents.extend(
                            state
                                .security
                                .observed
                                .get(holder.id())
                                .cloned()
                                .unwrap_or_default(),
                        );
                        let mut markings = result.markings.clone();
                        for parent in &parents {
                            markings.extend(
                                state
                                    .security
                                    .inputs
                                    .get(parent)
                                    .context("tool input provenance absent")?
                                    .markings
                                    .iter()
                                    .cloned(),
                            );
                        }
                        let input = security::InputRecord {
                            id: source_id.clone(),
                            source_type: "tool_result".into(),
                            hash: content_hash(&result.value)?,
                            goal_id: effect.permit.payload.goal_id.clone(),
                            producer: holder.id().into(),
                            trust: security::Trust::Untrusted,
                            markings,
                            parents,
                        };
                        // Trusted executor retains a result even after the worker's certificate expires.
                        state.security.inputs.insert(source_id.clone(), input);
                        state
                            .security
                            .observed
                            .entry(holder.id().into())
                            .or_default()
                            .insert(source_id);
                    }

                    let stored = state.effects.get_mut(intent).unwrap();
                    if stored
                        .result
                        .as_ref()
                        .is_some_and(|value| value["result_archive_version"] == 1)
                    {
                        stored.result.as_mut().unwrap()["postprocess_status"] = json!("ready");
                    }
                    Ok(stored.result.clone().unwrap())
                },
            )
            .await?;
        if post.ok {
            return Ok(post);
        }
        retained["postprocess_status"] = json!("failed");
        retained["body_missing"] = json!(true);
        self.commit_control(
            &format!("{id}-retention-stop"),
            executor.id(),
            &(intent, digest),
            vec![],
            |state| {
                ensure!(
                    state
                        .security
                        .policy
                        .payload
                        .trusted_components
                        .contains(executor.id()),
                    "only trusted executor may stop missing tool retention"
                );
                let effect = state.effects[intent].clone();
                stop_result_retention(state, &effect, intent)?;
                state.effects.get_mut(intent).unwrap().result = Some(retained.clone());
                Ok(retained.clone())
            },
        )
        .await
    }
    pub(crate) async fn domain(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        intent: &str,
        command: DomainCommand,
        workload_signature: &str,
        leases: Vec<LeaseCheck>,
    ) -> Result<Reply> {
        let now = crate::cluster::now_ms();
        check_allocation(subject, &leases)?;
        self.commit_local(id, subject, &(intent, &command), leases, |state| {
            let now = (crate::cluster::now_ms()).max(now);
            let mut command = command.clone();
            let effect = state
                .effects
                .get(intent)
                .context("domain operation lacks an admitted intent")?
                .clone();
            ensure!(
                !effect.permit.payload.billable
                    && effect.state == "pending"
                    && effect.permit.payload.holder == subject.id(),
                "domain effect identity/state mismatch"
            );
            ensure!(
                effect.payload == serde_json::to_value(&command)?,
                "command differs from admitted payload"
            );
            action_service(&state.modules, &effect.request.action.operation)?;
            check_search_action(
                state,
                &effect.request,
                &effect.payload,
                effect.task_context.as_ref(),
                now,
            )?;
            check_task(
                state,
                subject,
                effect.task_context.as_ref(),
                matches!(command, DomainCommand::ClaimTask { .. }),
            )?;
            validate_domain_action(&effect.request, &command)?;
            if let Some(confirmation) =
                refresh_nonbillable_effect(state, subject, intent, &self.signer, now / 1000)?
            {
                return Ok(confirmation);
            }
            let effect = state.effects[intent].clone();
            state.security.consume_permit(
                subject,
                &effect.permit,
                &mut state.ledger,
                now / 1000,
            )?;
            let actor = domain_actor(state, subject, &command)?;
            if let DomainCommand::CompleteTask { result_ref, .. } = &command {
                let source = state
                    .finding_sources
                    .get(result_ref)
                    .context("Task result lacks broker-recorded provenance")?;
                ensure!(
                    lineage_live(state, &BTreeSet::from([source.clone()])),
                    "Task result lineage retired"
                );
            }
            let mut produced_source = None;
            if let DomainCommand::AppendFinding {
                goal_id,
                task_id: _,
                body,
                provenance,
            } = &mut command
            {
                let finding_id = domain::hash_json(&(
                    goal_id.as_str(),
                    subject.id(),
                    &*body,
                    state.domain.revision + 1,
                ))?;
                let signed = state.security.append_finding(
                    subject,
                    finding_id,
                    content_hash(body)?,
                    &effect.request.grant_id,
                    &self.signer,
                    now / 1000,
                )?;
                *provenance = domain::Provenance {
                    producer_id: subject.id().into(),
                    recorder_id: "trusted-broker".into(),
                    observed_inputs: signed
                        .payload
                        .inputs
                        .iter()
                        .map(|id| {
                            let input = &state.security.inputs[id];
                            domain::InputSource {
                                hash: hex(&input.hash),
                                markings: input.markings.clone(),
                                trust: trust_number(input.trust),
                            }
                        })
                        .collect(),
                    source_refs: state
                        .finding_sources
                        .iter()
                        .filter(|(_, source)| signed.payload.inputs.contains(*source))
                        .map(|(id, _)| id.clone())
                        .collect(),
                    signature: serde_json::to_string(&signed)?,
                };
                produced_source = Some(signed.payload.id.clone());
            }
            set_signature(&mut command, workload_signature)?;
            let mut value = state.domain.execute(&actor, command.clone(), now)?;
            if let DomainCommand::ClaimTask {
                goal_id, task_id, ..
            } = &command
            {
                value["spec"]["input"] = usable_task_input(state, goal_id, task_id)?;
            }
            if let Some(source) = produced_source {
                let finding_id = value["id"].as_str().context("Finding ID absent")?;
                state.finding_sources.insert(finding_id.into(), source);
            }
            if let DomainCommand::AppendBoard { goal_id, .. } = &command {
                let seq = value["seq"].as_u64().context("Board sequence missing")?;
                let parents = state
                    .security
                    .observed
                    .get(subject.id())
                    .cloned()
                    .unwrap_or_default();
                let markings = parents
                    .iter()
                    .filter_map(|id| state.security.inputs.get(id))
                    .flat_map(|input| input.markings.iter().cloned())
                    .collect();
                state.security.record_input(
                    subject,
                    security::InputRecord {
                        id: format!("board/{goal_id}/{seq}"),
                        source_type: "board".into(),
                        hash: content_hash(&value)?,
                        goal_id: goal_id.clone(),
                        producer: subject.id().into(),
                        trust: security::Trust::Untrusted,
                        markings,
                        parents,
                    },
                    &self.signer,
                    now / 1000,
                )?;
            }
            if let DomainCommand::ReadBoard { goal_id, .. } = &command {
                let clearance = &state.security.grants[&effect.request.grant_id]
                    .capabilities
                    .iter()
                    .find(|cap| cap.id == effect.request.capability_id)
                    .context("Board read Capability missing")?
                    .constraints
                    .read_markings;
                let mut allowed = Vec::new();
                let mut excluded_sequences = Vec::new();
                let scanned_through = value["entries"]
                    .as_array()
                    .and_then(|entries| entries.last())
                    .and_then(|entry| entry["seq"].as_u64());
                for entry in value["entries"]
                    .as_array()
                    .context("Board response malformed")?
                {
                    verify_board_entry(state, entry)?;
                    let input_id = format!(
                        "board/{goal_id}/{}",
                        entry["seq"].as_u64().context("Board sequence absent")?
                    );
                    let input = state
                        .security
                        .inputs
                        .get(&input_id)
                        .context("Board provenance absent")?;
                    ensure!(
                        input.hash == content_hash(entry)?,
                        "Board content differs from broker provenance"
                    );
                    if input.markings.is_subset(clearance)
                        && lineage_live(state, &[input_id.clone()].into())
                    {
                        state
                            .security
                            .observed
                            .entry(subject.id().into())
                            .or_default()
                            .insert(input_id);
                        allowed.push(entry.clone());
                    } else {
                        excluded_sequences.push(entry["seq"].clone());
                    }
                }
                let mut value = value;
                value["entries"] = json!(allowed);
                value["excluded_sequences"] = json!(excluded_sequences);
                value["scanned_through"] = json!(scanned_through);
                let effect = state.effects.get_mut(intent).unwrap();
                effect.state = "done".into();
                effect.result = Some(value.clone());
                return Ok(value);
            }
            if let DomainCommand::RetractFinding { finding_id, .. } = &command {
                let source = state
                    .finding_sources
                    .get(finding_id)
                    .context("Finding security provenance missing")?
                    .clone();
                state.security.retract_finding(
                    subject,
                    &source,
                    content_hash(&command)?,
                    &self.signer,
                    now / 1000,
                )?;
            }
            let effect = state.effects.get_mut(intent).unwrap();
            effect.state = "done".into();
            effect.result = Some(value.clone());
            Ok(value)
        })
        .await
    }
    #[allow(clippy::too_many_arguments)] // Pagination, identity and allocation fence must share one transaction.
    pub(crate) async fn findings(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        intent: &str,
        goal_id: &str,
        after_id: Option<String>,
        limit: usize,
        leases: Vec<LeaseCheck>,
    ) -> Result<Reply> {
        let now = crate::cluster::now_ms() / 1000;
        check_allocation(subject, &leases)?;
        self.commit_local(id, subject, &(intent, goal_id, &after_id, limit), leases, |state| {
                let now=(crate::cluster::now_ms() / 1000).max(now);
            ensure!((1..=128).contains(&limit), "invalid Finding batch limit");
            let effect = state.effects.get(intent).context("read lacks admitted intent")?.clone();
            ensure!(!effect.permit.payload.billable && effect.state == "pending" && effect.request.action.kind == security::CapabilityKind::FindingRead
                && effect.request.action.operation == "finding_read" && effect.permit.payload.goal_id == goal_id,
                "Finding read Capability/scope mismatch");
            ensure!(effect.payload == json!({"goal_id":goal_id,"after_id":after_id,"limit":limit}), "read differs from admitted payload");
            action_service(&state.modules,&effect.request.action.operation)?;
            check_search_action(state,&effect.request,&effect.payload,effect.task_context.as_ref(),crate::cluster::now_ms())?;
            check_task(state, subject, effect.task_context.as_ref(), false)?;
            if let Some(confirmation) = refresh_nonbillable_effect(state, subject, intent, &self.signer, now)? { return Ok(confirmation); }
            let effect = state.effects[intent].clone();
            state.security.consume_permit(subject, &effect.permit, &mut state.ledger, now)?;
            let grant = state.security.grants[&effect.request.grant_id].clone();
            let clearance = &grant.capabilities.iter().find(|c| c.id == effect.request.capability_id).context("read Capability absent")?.constraints.read_markings;
            let findings: Vec<_> = state.domain.findings.values().filter(|f| f.goal_id == goal_id && f.markings.is_subset(clearance)
                && state.domain.finding_eligible(&f.id)
                && state.finding_sources.get(&f.id).is_some_and(|source| lineage_live(state, &[source.clone()].into()))
                && after_id.as_ref().is_none_or(|after| f.id > *after)).take(limit).cloned().collect();
            for finding in &findings {
                let source_id = state.finding_sources.get(&finding.id).context("Finding security provenance missing")?.clone();
                let provenance = state.security.findings[&source_id].payload.clone();
                if !state.security.inputs.contains_key(&source_id) {
                    state.security.record_input(subject, security::InputRecord { id: source_id.clone(), source_type: "finding".into(),
                        hash: provenance.result_hash, goal_id: goal_id.into(), producer: provenance.producer, trust: security::Trust::Untrusted,
                        markings: provenance.markings, parents: provenance.inputs }, &self.signer, now)?;
                } else { state.security.observed.entry(subject.id().into()).or_default().insert(source_id); }
            }
            let value = json!({"findings":findings,"next_id":findings.last().map(|f|&f.id),"revision":state.domain.revision});
            let effect = state.effects.get_mut(intent).unwrap(); effect.state = "done".into(); effect.result = Some(value.clone()); Ok(value)
        }).await
    }
    #[allow(clippy::too_many_arguments)] // Bounded Goal metadata and current Task authority commit together.
    pub(crate) async fn task_status(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        intent: &str,
        goal_id: &str,
        after_id: Option<String>,
        limit: usize,
        leases: Vec<LeaseCheck>,
    ) -> Result<Reply> {
        check_allocation(subject, &leases)?;
        self.commit_local(id, subject, &(intent, goal_id, &after_id, limit), leases, |state| {
            ensure!((1..=128).contains(&limit) && after_id.as_ref().is_none_or(|after| after.len() <= 200), "invalid Task-status page bounds");
            let now = crate::cluster::now_ms() / 1000;
            let effect = state.effects.get(intent).context("Task status lacks admitted intent")?.clone();
            ensure!(!effect.permit.payload.billable && effect.state == "pending"
                && effect.request.action.kind == security::CapabilityKind::ToolCall
                && effect.request.action.operation == "task_status"
                && effect.request.action.resource == format!("task/{goal_id}")
                && effect.permit.payload.goal_id == goal_id, "Task-status Capability/scope mismatch");
            ensure!(effect.payload == json!({"goal_id":goal_id,"after_id":after_id,"limit":limit}), "Task-status read differs from admitted payload");
            action_service(&state.modules,&effect.request.action.operation)?;
            check_search_action(state,&effect.request,&effect.payload,effect.task_context.as_ref(),crate::cluster::now_ms())?;
            check_task(state, subject, effect.task_context.as_ref(), false)?;
            let clearance = state.security.grants[&effect.request.grant_id].capabilities.iter()
                .find(|cap| cap.id == effect.request.capability_id).context("Task-status Capability absent")?.constraints.read_markings.clone();
            let classification = state.security.policy.payload.classification.clone();
            ensure!(clearance.contains(&classification), "Task metadata exceeds read clearance");
            if let Some(confirmation) = refresh_nonbillable_effect(state, subject, intent, &self.signer, now)? { return Ok(confirmation); }
            let effect = state.effects[intent].clone();
            state.security.consume_permit(subject, &effect.permit, &mut state.ledger, now)?;
            let version = state.domain.goal(goal_id)?.spec.version;
            let mut rows = vec![];
            let mut parents = BTreeSet::new();
            let mut markings = BTreeSet::from([classification]);
            for task in state.domain.tasks.values().filter(|task| task.goal_id == goal_id && task.goal_version == version
                && after_id.as_ref().is_none_or(|after| task.spec.id > *after)) {
                let mut roots = state.task_input_sources.get(&format!("{goal_id}/{}", task.spec.id)).cloned().unwrap_or_default();
                if let Some(source) = task.result_ref.as_ref().and_then(|finding| state.finding_sources.get(finding)) {
                    roots.insert(source.clone());
                }
                let Ok(mut labels) = lineage_markings(state, &roots) else { continue; };
                if let Some(finding) = task.result_ref.as_ref().and_then(|id| state.domain.findings.get(id)) {
                    labels.extend(finding.markings.iter().cloned());
                }
                if !labels.is_subset(&clearance) { continue; }
                markings.extend(labels);
                parents.extend(roots);
                if let Some(source) = task.result_ref.as_ref().and_then(|finding| state.finding_sources.get(finding)) {
                    parents.insert(source.clone());
                }
                rows.push(json!({"id":task.spec.id,"need_id":task.spec.need_id,"state":task.state,
                    "claimed_by":task.claimed_by,"incarnation":task.incarnation,"fencing_token":task.fencing_token,
                    "lease_expiry_ms":task.lease_expiry_ms,"result_ref":task.result_ref}));
                if rows.len() == limit { break; }
            }
            let value = json!({"goal_id":goal_id,"goal_version":version,"next_id":rows.last().map(|row| &row["id"]),
                "tasks":rows,"domain_revision":state.domain.revision});
            let source = format!("task-status/{intent}");
            observe(state, subject, source.clone(), "task_status", goal_id, &value, &self.signer, now)?;
            let input = state.security.inputs.get_mut(&source).unwrap();
            input.parents.extend(parents);
            input.markings.extend(markings);
            let stored = state.effects.get_mut(intent).unwrap();
            stored.state = "done".into(); stored.result = Some(value.clone());
            Ok(value)
        }).await
    }
    #[allow(clippy::too_many_arguments)] // Pagination and the current Task/allocation share the authorization transaction.
    pub(crate) async fn audit_view(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        intent: &str,
        goal_id: &str,
        after_index: u64,
        limit: usize,
        leases: Vec<LeaseCheck>,
    ) -> Result<Reply> {
        check_allocation(subject, &leases)?;
        self.commit_local(
            id,
            subject,
            &(intent, goal_id, after_index, limit),
            leases,
            |state| {
                let now = crate::cluster::now_ms() / 1000;
                let effect = state
                    .effects
                    .get(intent)
                    .context("audit read lacks admitted intent")?
                    .clone();
                ensure!(
                    !effect.permit.payload.billable
                        && effect.state == "pending"
                        && effect.request.action.kind == security::CapabilityKind::ToolCall
                        && effect.request.action.operation == "audit_read"
                        && effect.request.action.resource == format!("audit/{goal_id}")
                        && effect.permit.payload.goal_id == goal_id,
                    "audit read Capability/scope mismatch"
                );
                ensure!(
                    effect.payload
                        == json!({"goal_id":goal_id,"after_index":after_index,"limit":limit}),
                    "audit read differs from admitted payload"
                );
                action_service(&state.modules, &effect.request.action.operation)?;
                check_search_action(
                    state,
                    &effect.request,
                    &effect.payload,
                    effect.task_context.as_ref(),
                    crate::cluster::now_ms(),
                )?;
                check_task(state, subject, effect.task_context.as_ref(), false)?;
                let cap = state.security.grants[&effect.request.grant_id]
                    .capabilities
                    .iter()
                    .find(|cap| cap.id == effect.request.capability_id)
                    .context("audit Capability absent")?;
                ensure!(
                    cap.constraints
                        .read_markings
                        .contains(&state.security.policy.payload.classification),
                    "audit metadata exceeds read clearance"
                );
                let clearance = cap.constraints.read_markings.clone();
                if let Some(confirmation) =
                    refresh_nonbillable_effect(state, subject, intent, &self.signer, now)?
                {
                    return Ok(confirmation);
                }
                let effect = state.effects[intent].clone();
                state
                    .security
                    .consume_permit(subject, &effect.permit, &mut state.ledger, now)?;
                let mut value = crate::audit::goal_view(
                    &state.audit,
                    goal_id,
                    after_index,
                    limit,
                    &self.signer,
                    now,
                )?;
                let mut flows = state.egress.goal_view(goal_id, limit);
                let mut visible = BTreeSet::new();
                let mut markings =
                    BTreeSet::from([state.security.policy.payload.classification.clone()]);
                flows.records.retain(|flow| {
                    let roots = state
                        .security
                        .observed
                        .get(&flow.holder)
                        .cloned()
                        .unwrap_or_default();
                    if let Ok(labels) = lineage_markings(state, &roots)
                        && labels.is_subset(&clearance)
                    {
                        markings.extend(labels);
                        visible.insert(flow.intent_id.clone());
                        true
                    } else {
                        false
                    }
                });
                flows.flags.retain(|flag| visible.contains(&flag.intent_id));
                value["egress"] =
                    serde_json::to_value(self.signer.sign("goal-egress-view", flows)?)?;
                observe(
                    state,
                    subject,
                    format!("audit/{intent}"),
                    "audit_view",
                    goal_id,
                    &value,
                    &self.signer,
                    now,
                )?;
                state
                    .security
                    .inputs
                    .get_mut(&format!("audit/{intent}"))
                    .unwrap()
                    .markings
                    .extend(markings);
                let effect = state.effects.get_mut(intent).unwrap();
                effect.state = "done".into();
                effect.result = Some(value.clone());
                Ok(value)
            },
        )
        .await
    }
    #[allow(clippy::too_many_arguments)] // Explicit broker input identity and scope are audited together.
    pub(crate) async fn observe(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        source_id: String,
        source_type: String,
        goal_id: String,
        value: Value,
    ) -> Result<Reply> {
        let now = crate::cluster::now_ms() / 1000;
        self.commit(
            id,
            subject.id(),
            &(&source_id, &source_type, &goal_id, content_hash(&value)?),
            vec![],
            |state| {
                let now = (crate::cluster::now_ms() / 1000).max(now);
                observe(
                    state,
                    subject,
                    source_id.clone(),
                    &source_type,
                    &goal_id,
                    &value,
                    &self.signer,
                    now,
                )?;
                Ok(json!({"recorded":source_id}))
            },
        )
        .await
    }
    /// The trusted launcher observes only the current eligible Task input in the same committed snapshot.
    pub(crate) async fn observe_task_input(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        grant_id: &str,
        task: TaskContext,
        leases: Vec<LeaseCheck>,
    ) -> Result<Reply> {
        check_allocation(subject, &leases)?;
        self.commit(id, subject.id(), &(grant_id, &task), leases, |state| {
            let now = crate::cluster::now_ms() / 1000;
            subject.ensure_current(now)?;
            check_task(state, subject, Some(&task), false)?;
            let grant = state
                .security
                .grants
                .get(grant_id)
                .context("Task input grant absent")?
                .clone();
            ensure!(
                grant.holder.uri()? == subject.id() && !grant.revoked && now < grant.lease.expiry,
                "Task input grant expired or mismatched"
            );
            let archived = state
                .domain
                .task(&grant.goal_id, &task.id)?
                .spec
                .input
                .clone();
            let input = usable_task_input(state, &grant.goal_id, &task.id)?;
            let original = state
                .task_input_sources
                .get(&format!("{}/{}", grant.goal_id, task.id))
                .cloned()
                .unwrap_or_default();
            let mut markings =
                BTreeSet::from([state.security.policy.payload.classification.clone()]);
            let mut pending: Vec<_> = original.iter().cloned().collect();
            let mut seen = BTreeSet::new();
            while let Some(source) = pending.pop() {
                if !seen.insert(source.clone()) {
                    continue;
                }
                if let Some(record) = state.security.inputs.get(&source) {
                    markings.extend(record.markings.iter().cloned());
                    pending.extend(record.parents.iter().cloned());
                } else if let Some(record) = state.security.findings.get(&source) {
                    markings.extend(record.payload.markings.iter().cloned());
                    pending.extend(record.payload.inputs.iter().cloned());
                }
            }
            let clearance = grant
                .capabilities
                .iter()
                .filter(|c| {
                    c.kind == security::CapabilityKind::ToolCall
                        && c.resource.matches(&format!("task/{}", grant.goal_id))
                        && c.constraints.operations.contains("task_work")
                })
                .map(|c| &c.constraints.read_markings)
                .find(|clearance| markings.is_subset(clearance))
                .context("Task input marking clearance absent")?;
            ensure!(
                markings.is_subset(clearance),
                "Task input marking disclosure denied"
            );
            let mut parents = state
                .security
                .observed
                .get(subject.id())
                .cloned()
                .unwrap_or_default();
            if input == archived {
                for source in original {
                    if !state.security.inputs.contains_key(&source) {
                        let proof = state
                            .security
                            .findings
                            .get(&source)
                            .context("Task input source absent")?
                            .payload
                            .clone();
                        state.security.record_input(
                            subject,
                            security::InputRecord {
                                id: source.clone(),
                                source_type: "finding".into(),
                                hash: proof.result_hash,
                                goal_id: grant.goal_id.clone(),
                                producer: proof.producer,
                                trust: security::Trust::Untrusted,
                                markings: proof.markings,
                                parents: proof.inputs,
                            },
                            &self.signer,
                            now,
                        )?;
                    }
                    parents.insert(source);
                }
            }
            let source_id = format!(
                "task-input/{}/{}/{}/{}",
                grant.goal_id, task.id, task.incarnation, task.fence
            );
            if !state.security.inputs.contains_key(&source_id) {
                state.security.record_input(
                    subject,
                    security::InputRecord {
                        id: source_id.clone(),
                        source_type: "task_input".into(),
                        hash: content_hash(&input)?,
                        goal_id: grant.goal_id,
                        producer: "trusted-task-broker".into(),
                        trust: security::Trust::Untrusted,
                        markings,
                        parents,
                    },
                    &self.signer,
                    now,
                )?;
            } else {
                ensure!(
                    state.security.inputs[&source_id].hash == content_hash(&input)?,
                    "Task input observation changed after retraction; renew incarnation"
                );
                state
                    .security
                    .observed
                    .entry(subject.id().into())
                    .or_default()
                    .insert(source_id);
            }
            Ok(json!({"input":input}))
        })
        .await
    }
    #[allow(clippy::too_many_arguments)] // Only the broker records the exact model context and its admitted Goal/Task.
    pub(crate) async fn observe_context(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        grant_id: &str,
        task_id: &str,
        source_id: String,
        value: Value,
        leases: Vec<LeaseCheck>,
    ) -> Result<Reply> {
        check_allocation(subject, &leases)?;
        self.commit(
            id,
            subject.id(),
            &(grant_id, task_id, &source_id, content_hash(&value)?),
            leases,
            |state| {
                let grant = state
                    .security
                    .grants
                    .get(grant_id)
                    .context("context grant absent")?
                    .clone();
                ensure!(
                    grant.holder.uri()? == subject.id() && !grant.revoked,
                    "context holder mismatch"
                );
                let now = crate::cluster::now_ms() / 1000;
                let clearance = grant
                    .capabilities
                    .iter()
                    .find(|c| {
                        c.kind == security::CapabilityKind::FindingRead
                            && c.resource.matches(&format!("finding/{}", grant.goal_id))
                            && c.constraints.operations.contains("finding_read")
                    })
                    .map(|c| c.constraints.read_markings.clone())
                    .unwrap_or_default();
                let attempts: Vec<_> = state
                    .domain
                    .attempts
                    .values()
                    .filter(|a| {
                        a.goal_id == grant.goal_id
                            && a.markings.is_subset(&clearance)
                            && state.domain.attempt_eligible(a)
                    })
                    .cloned()
                    .collect();
                for attempt in attempts {
                    let source = format!("attempt/{}", attempt.id);
                    let mut parents = BTreeSet::new();
                    for id in &attempt.finding_ids {
                        let finding_source = state
                            .finding_sources
                            .get(id)
                            .context("candidate Finding lineage absent")?
                            .clone();
                        let provenance = state
                            .security
                            .findings
                            .get(&finding_source)
                            .context("candidate Finding proof absent")?
                            .payload
                            .clone();
                        if !state.security.inputs.contains_key(&finding_source) {
                            state.security.record_input(
                                subject,
                                security::InputRecord {
                                    id: finding_source.clone(),
                                    source_type: "finding".into(),
                                    hash: provenance.result_hash,
                                    goal_id: grant.goal_id.clone(),
                                    producer: provenance.producer,
                                    trust: security::Trust::Untrusted,
                                    markings: provenance.markings,
                                    parents: provenance.inputs,
                                },
                                &self.signer,
                                now,
                            )?;
                        }
                        parents.insert(finding_source);
                    }
                    if !state.security.inputs.contains_key(&source) {
                        state.security.record_input(
                            subject,
                            security::InputRecord {
                                id: source.clone(),
                                source_type: "candidate_history".into(),
                                hash: content_hash(&attempt.payload)?,
                                goal_id: grant.goal_id.clone(),
                                producer: attempt.producer_id,
                                trust: security::Trust::Untrusted,
                                markings: attempt.markings,
                                parents,
                            },
                            &self.signer,
                            now,
                        )?;
                    } else {
                        state
                            .security
                            .observed
                            .entry(subject.id().into())
                            .or_default()
                            .insert(source);
                    }
                }
                observe(
                    state,
                    subject,
                    source_id.clone(),
                    "model_context",
                    &grant.goal_id,
                    &value,
                    &self.signer,
                    now,
                )?;
                Ok(json!({"recorded":source_id}))
            },
        )
        .await
    }
    pub(crate) async fn observe_tool_inputs(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        grant_id: &str,
        inputs: Vec<security::InputRecord>,
        task: Option<TaskContext>,
        leases: Vec<LeaseCheck>,
    ) -> Result<Reply> {
        check_allocation(subject, &leases)?;
        self.commit(
            id,
            subject.id(),
            &(grant_id, &inputs, &task),
            leases,
            |state| {
                check_task(state, subject, task.as_ref(), false)?;
                let grant = state
                    .security
                    .grants
                    .get(grant_id)
                    .context("artifact reader grant absent")?
                    .clone();
                ensure!(
                    grant.holder.uri()? == subject.id() && !grant.revoked,
                    "artifact observer holder mismatch"
                );
                for input in &inputs {
                    ensure!(
                        input.goal_id == grant.goal_id && input.trust == security::Trust::Untrusted,
                        "artifact input scope/trust mismatch"
                    );
                    if let Some(previous) = state.security.inputs.get(&input.id) {
                        ensure!(
                            content_hash(previous)? == content_hash(input)?,
                            "artifact source changed"
                        );
                        state
                            .security
                            .observed
                            .entry(subject.id().into())
                            .or_default()
                            .insert(input.id.clone());
                    } else {
                        state.security.record_input(
                            subject,
                            input.clone(),
                            &self.signer,
                            crate::cluster::now_ms() / 1000,
                        )?;
                    }
                }
                Ok(json!({"observed":inputs.iter().map(|i|&i.id).collect::<Vec<_>>()}))
            },
        )
        .await
    }
    pub async fn stop(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        scope: Scope,
        reason: String,
    ) -> Result<Reply> {
        let now = crate::cluster::now_ms() / 1000;
        self.commit(id, subject.id(), &(&scope, &reason), vec![], |state| {
            let now = (crate::cluster::now_ms() / 1000).max(now);
            state
                .security
                .engage_stop(subject, scope.clone(), content_hash(&reason)?, now)?;
            if let Scope::Goal { goal, .. } = &scope {
                state.domain.execute(
                    &Actor {
                        id: subject.id().into(),
                        role: domain::Role::Policy,
                    },
                    DomainCommand::StopGoal {
                        goal_id: goal.clone(),
                        reason: reason.clone(),
                    },
                    now * 1000,
                )?;
            }
            Ok(json!({"stopped":scope,"epoch":state.security.stop_epoch}))
        })
        .await
    }
    pub(crate) async fn policy_intervention(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        request: ActionRequest,
        leases: Vec<LeaseCheck>,
    ) -> Result<Reply> {
        let now = crate::cluster::now_ms() / 1000;
        let stop = request.action.operation == "stop";
        self.commit_class(id, subject.id(), &request, leases, stop, |state| {
                let now=(crate::cluster::now_ms() / 1000).max(now);
            let decision = if stop {
                state.security.policy_stop(subject, request.clone(), &mut state.ledger, &self.signer, now)?
            } else {
                state.security.policy_flag(subject, request.clone(), &mut state.ledger, &self.signer, now)?
            };
            state.audit.append_committed(decision.audit)?;
            if !stop {
                state.audit.append_committed(AuditEvent { correlation: None,
                    id: format!("policy-flag/{}", request.intent_id),
                    kind: "policy_flag".into(),
                    actor: subject.id().into(),
                    goal: Some(decision.permit.payload.goal_id.clone()),
                    timestamp: now,
                    decision: "record_flag".into(),
                    payload_hash: content_hash(&request.action.arguments)?,
                })?;
            }
            let result = json!({"scope":request.action.arguments["scope"],"flagged":!stop,"engaged":stop,"epoch":state.security.stop_epoch});
            state.effects.insert(request.intent_id.clone(), Effect { correlation: None, request: request.clone(), payload: request.action.arguments.clone(), permit: decision.permit, state: "done".into(), result: Some(result.clone()), task_context: None });
            Ok(result)
        }).await
    }
}

/// Test synchronization only: normal authenticated APIs still perform every authority mutation.
#[cfg(test)]
pub(crate) mod admission_sync {
    use std::{
        collections::BTreeMap,
        sync::{Arc, LazyLock, Mutex},
    };
    use tokio::sync::Notify;

    pub(crate) struct Gate {
        entered: Notify,
        resume: Notify,
    }
    static GATES: LazyLock<Mutex<BTreeMap<String, Arc<Gate>>>> =
        LazyLock::new(|| Mutex::new(BTreeMap::new()));
    pub(crate) fn pause_once(intent: String) -> Arc<Gate> {
        let gate = Arc::new(Gate {
            entered: Notify::new(),
            resume: Notify::new(),
        });
        assert!(GATES.lock().unwrap().insert(intent, gate.clone()).is_none());
        gate
    }
    impl Gate {
        pub(crate) async fn wait_reviewed(&self) {
            self.entered.notified().await;
        }
        pub(crate) fn release(&self) {
            self.resume.notify_one();
        }
    }
    pub(super) async fn after_review(intent: &str) {
        let gate = GATES.lock().unwrap().remove(intent);
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.resume.notified().await;
        }
    }
}
#[allow(clippy::too_many_arguments)] // This internal helper retains full input provenance at every broker entry point.
fn observe(
    state: &mut KernelState,
    subject: &AuthenticatedSubject,
    id: String,
    source_type: &str,
    goal_id: &str,
    value: &Value,
    signer: &CryptoSigner,
    now: u64,
) -> Result<()> {
    let digest = content_hash(value)?;
    if let Some(existing) = state.security.inputs.get(&id) {
        ensure!(
            existing.hash == digest && existing.goal_id == goal_id,
            "source ID content changed"
        );
        state
            .security
            .observed
            .entry(subject.id().into())
            .or_default()
            .insert(id);
        return Ok(());
    }
    let parents = state
        .security
        .observed
        .get(subject.id())
        .cloned()
        .unwrap_or_default();
    let mut markings: BTreeSet<String> = parents
        .iter()
        .filter_map(|parent| state.security.inputs.get(parent))
        .flat_map(|input| input.markings.iter().cloned())
        .collect();
    markings.insert(state.security.policy.payload.classification.clone());
    state.security.record_input(
        subject,
        security::InputRecord {
            id,
            source_type: source_type.into(),
            hash: digest,
            goal_id: goal_id.into(),
            producer: "trusted-broker".into(),
            trust: security::Trust::Untrusted,
            markings,
            parents,
        },
        signer,
        now,
    )
}
fn trust_number(trust: security::Trust) -> u8 {
    match trust {
        security::Trust::Untrusted => 0,
        security::Trust::Corroborated => 1,
        security::Trust::Trusted => 2,
    }
}
pub(crate) fn action_service(modules: &ModuleRegistry, operation: &str) -> Result<()> {
    let service = match operation {
        "inference" => "inference",
        "evaluate" => "evaluation",
        "wasi_execute"
        | "native_execute"
        | "artifact_put"
        | "artifact_get"
        | "artifact_retract"
        | "artifact_export"
        | "artifact_prepare_version" => "installed-tools",
        _ => "worker-broker",
    };
    if modules
        .modules
        .values()
        .any(|module| module.spec.provides.contains(service))
    {
        ensure!(
            modules.services().contains(service),
            "required service {service} is withdrawn"
        );
    }
    Ok(())
}
fn reconciliation_credit(state: &KernelState) -> Result<usize> {
    state
        .effects
        .values()
        .filter(|effect| {
            matches!(effect.state.as_str(), "pending" | "dispatched" | "unknown")
                || effect
                    .result
                    .as_ref()
                    .is_some_and(|value| value["postprocess_status"] == "pending")
        })
        .count()
        .checked_mul(EFFECT_RECONCILIATION_CREDIT_BYTES)
        .context("outstanding result credit overflow")
}
/// Every writer of the shared state must preserve already-dispatched receipt capacity.
pub(crate) fn ensure_state_capacity(
    state: &KernelState,
    encoded_bytes: usize,
    max_bytes: usize,
    control: bool,
) -> Result<()> {
    let limit = if control {
        max_bytes
    } else {
        max_bytes - max_bytes / 4
    };
    let stop_reserve = if control { 0 } else { max_bytes / 16 };
    if encoded_bytes > limit
        || encoded_bytes
            .checked_add(reconciliation_credit(state)?)
            .and_then(|bytes| bytes.checked_add(stop_reserve))
            .is_none_or(|bytes| bytes > max_bytes)
    {
        return Err(StateCapacityError.into());
    }
    Ok(())
}
fn normalize_receipt(mut receipt: Receipt) -> Receipt {
    if receipt.id.len() > 512 {
        receipt.id = format!(
            "upstream-sha256/{}",
            hex(&crate::audit::hash(receipt.id.as_bytes()))
        );
    }
    receipt
}
fn stop_result_retention(state: &mut KernelState, effect: &Effect, intent: &str) -> Result<()> {
    state.security.engage_ledger_stop(
        state.security.policy.payload.tenant.clone(),
        effect.permit.payload.goal_id.clone(),
        content_hash(&("result-evidence-unavailable", intent))?,
        crate::cluster::now_ms() / 1000,
    )?;
    Ok(())
}
fn bounded_result_metadata(result: &Value) -> Value {
    let mut metadata = json!({});
    for key in ["completed", "usage", "markings"] {
        if let Some(value) = result.get(key) {
            metadata[key] = value.clone();
        }
    }
    if let Some(value) = result.get("outcome") {
        let mut outcome = value.clone();
        if let Some(object) = outcome.as_object_mut() {
            object.remove("completion");
            object.remove("evidence");
        }
        metadata["outcome"] = outcome;
    }
    fn bound_strings(value: &mut Value) {
        match value {
            Value::String(text) if text.len() > 512 => {
                *text = format!(
                    "sha256:{}:bytes:{}",
                    hex(&crate::audit::hash(text.as_bytes())),
                    text.len()
                );
            }
            Value::Array(values) => {
                for value in values {
                    bound_strings(value);
                }
            }
            Value::Object(values) => {
                for value in values.values_mut() {
                    bound_strings(value);
                }
            }
            _ => {}
        }
    }
    bound_strings(&mut metadata);
    // All large/unbounded fields remain in authenticated encrypted evidence, not financial state.
    if serde_json::to_vec(&metadata).map_or(true, |bytes| bytes.len() > MAX_RESULT_RECEIPT_BYTES) {
        metadata = json!({"metadata_sha256":content_hash(&metadata).map(|hash|hex(&hash)).unwrap_or_default()});
    }
    metadata
}

impl Kernel {
    async fn retain_effect_result(
        &self,
        executor: &AuthenticatedSubject,
        intent: &str,
        result: &Value,
        postprocess: bool,
    ) -> Result<Value> {
        let state = self.control_state().await?;
        ensure!(
            state
                .security
                .policy
                .payload
                .trusted_components
                .contains(executor.id()),
            "only trusted executor may retain effect results"
        );
        let effect = state.effects.get(intent).context("result effect absent")?;
        let bytes = crate::audit::canonical(result)?;
        // Fixtures retain small inline values; production always uses its private response store.
        if self.response_store.is_none() && bytes.len() <= MAX_RESULT_RECEIPT_BYTES / 2 {
            return Ok(result.clone());
        }
        let mut markings = BTreeSet::from([state.security.policy.payload.classification.clone()]);
        let mut parents = state
            .security
            .observed
            .get(&effect.permit.payload.holder)
            .cloned()
            .unwrap_or_default();
        if let Some(inputs) = effect.payload.get("input_sources")
            && let Ok(inputs) = serde_json::from_value::<BTreeSet<String>>(inputs.clone())
        {
            parents.extend(inputs);
        }
        match lineage_markings(&state, &parents) {
            Ok(labels) => markings.extend(labels),
            Err(_) => {
                markings.insert("unverified-lineage".into());
            }
        }
        if let Some(labels) = result["markings"].as_array() {
            markings.extend(labels.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        let sha256 = hex(&crate::audit::hash(&bytes));
        let signature=self.signer.sign("effect-result-retention",json!({"intent":intent,"holder":effect.permit.payload.holder,"result_sha256":sha256,"bytes":bytes.len()}))?;
        let info = crate::artifacts::ArtifactMetadata {
            goal_id: effect.permit.payload.goal_id.clone(),
            logical_name: format!(
                "effect-{}",
                &hex(&crate::audit::hash(intent.as_bytes()))[..32]
            ),
            version: "effect-result-v1".into(),
            classifications: markings,
            trust: 0,
            producer_id: effect.permit.payload.holder.clone(),
            broker_id: "trusted-kernel".into(),
            provenance: json!({"intent":intent,"inputs":parents,"semantic_result_sha256":sha256}),
            parents: vec![],
            signature: serde_json::to_string(&signature)?,
            created_ms: crate::cluster::now_ms(),
        };
        let reference = if let Some(store) = &self.response_store {
            store
                .lock()
                .await
                .put(&bytes, info)
                .ok()
                .map(|artifact| artifact.id)
        } else {
            None
        };
        let mut metadata = bounded_result_metadata(result);
        metadata["result_archive_version"] = json!(1);
        metadata["result_ref"] = json!(reference);
        metadata["result_sha256"] = json!(sha256);
        metadata["result_bytes"] = json!(bytes.len());
        metadata["body_missing"] = json!(reference.is_none());
        metadata["postprocess_status"] = json!(if postprocess { "pending" } else { "ready" });
        Ok(metadata)
    }
    /// Called after the broker's central live replay guard. It never bypasses holder/clearance.
    pub async fn load_effect_result(
        &self,
        subject: &AuthenticatedSubject,
        intent: &str,
    ) -> Result<Value> {
        subject.ensure_current(crate::cluster::now_ms() / 1000)?;
        let state = self.state().await?;
        let effect = state.effects.get(intent).context("effect absent")?;
        action_service(&state.modules, &effect.request.action.operation)?;
        check_search_action(
            &state,
            &effect.request,
            &effect.payload,
            effect.task_context.as_ref(),
            crate::cluster::now_ms(),
        )?;
        ensure!(
            effect.permit.payload.holder == subject.id(),
            "result holder mismatch"
        );
        let value = effect.result.as_ref().context("effect result absent")?;
        ensure!(
            value["body_missing"] != true
                && value["postprocess_status"]
                    .as_str()
                    .is_none_or(|status| status == "ready"),
            "result evidence unavailable or postprocessing incomplete"
        );
        let goal = state.domain.goal(&effect.permit.payload.goal_id)?;
        ensure!(
            goal.status != "stopped"
                && goal.spec.expiry_ms > crate::cluster::now_ms()
                && !state.security.stopped(&Scope::Goal {
                    tenant: state.security.policy.payload.tenant.clone(),
                    goal: goal.spec.goal_id.clone()
                }),
            "result Goal is stopped or expired"
        );
        let mut roots = state
            .security
            .observed
            .get(subject.id())
            .cloned()
            .unwrap_or_default();
        if let Some(inputs) = effect.payload.get("input_sources")
            && let Ok(inputs) = serde_json::from_value::<BTreeSet<String>>(inputs.clone())
        {
            roots.extend(inputs);
        }
        for source in [
            format!("context/{intent}"),
            format!("response/{intent}"),
            format!("tool-result/{intent}"),
        ] {
            if state.security.inputs.contains_key(&source) {
                roots.insert(source);
            }
        }
        ensure!(lineage_live(&state, &roots), "result lineage is retired");
        let grant = state
            .security
            .grants
            .get(&effect.request.grant_id)
            .context("result grant absent")?;
        ensure!(
            !grant.revoked
                && grant.holder.uri()? == subject.id()
                && grant.version == effect.request.grant_version
                && grant.lease.expiry > crate::cluster::now_ms() / 1000,
            "result grant no longer current"
        );
        let capability = grant
            .capabilities
            .iter()
            .find(|cap| cap.id == effect.request.capability_id)
            .context("result read capability absent")?;
        ensure!(
            lineage_markings(&state, &roots)?.is_subset(&capability.constraints.read_markings),
            "result transitive clearance denied"
        );
        if value["result_archive_version"] != 1 {
            return Ok(value.clone());
        }
        let id = value["result_ref"]
            .as_str()
            .context("result artifact absent")?;
        let store = self
            .response_store
            .as_ref()
            .context("private response store absent")?;
        let permit = crate::artifacts::ReadPermit {
            holder_id: subject.id().into(),
            goal_ids: [effect.permit.payload.goal_id.clone()].into(),
            classifications: capability.constraints.read_markings.clone(),
            expiry_ms: grant.lease.expiry * 1000,
            allow_retracted: false,
        };
        let bytes = store
            .lock()
            .await
            .get(id, &permit, crate::cluster::now_ms())?;
        ensure!(
            value["result_sha256"] == hex(&crate::audit::hash(&bytes))
                && value["result_bytes"].as_u64() == Some(bytes.len() as u64),
            "retained result digest or byte count mismatch"
        );
        Ok(serde_json::from_slice(&bytes)?)
    }
}

fn open_completed_round(
    state: &mut KernelState,
    effect: &Effect,
    intent: &str,
    now: u64,
) -> Option<Value> {
    let goal_id = &effect.permit.payload.goal_id;
    if let Some(round) = state
        .domain
        .planning_rounds
        .get(&format!("{goal_id}/{intent}"))
    {
        return (round.producer_id == effect.permit.payload.holder
            && round.goal_version == effect.request.grant_version)
            .then(|| serde_json::to_value(round).ok())
            .flatten();
    }
    let goal = state.domain.goal(goal_id).ok()?;
    if !matches!(goal.status.as_str(), "active" | "awaiting_evaluation")
        || goal.spec.expiry_ms <= now
        || goal.spec.version != effect.request.grant_version
        || !search_open(state, goal_id)
    {
        return None;
    }
    let grant = state.security.grants.get(&effect.request.grant_id)?;
    if grant.revoked
        || grant.version != goal.spec.version
        || grant.holder.uri().ok().as_deref() != Some(effect.permit.payload.holder.as_str())
        || grant.lease.expiry <= now / 1000
        || state.security.stopped(&Scope::Agent {
            tenant: grant.holder.tenant.clone(),
            goal: goal_id.clone(),
            holder: effect.permit.payload.holder.clone(),
        })
    {
        return None;
    }
    let context = effect.task_context.as_ref()?;
    let task = state.domain.task(goal_id, &context.id).ok()?;
    if task.claimed_by.as_deref() != Some(effect.permit.payload.holder.as_str())
        || task.incarnation != context.incarnation
        || task.fencing_token != context.fence
        || task.lease_expiry_ms <= now
        || !matches!(
            task.state,
            domain::TaskState::Claimed | domain::TaskState::Running
        )
    {
        return None;
    }
    let source = format!("context/{intent}");
    if !state.security.inputs.contains_key(&source)
        || !lineage_live(state, &BTreeSet::from([source]))
    {
        return None;
    }
    let limit: usize = effect.payload["context"]["history"]["candidate_limit"]
        .as_u64()?
        .try_into()
        .ok()?;
    let context_hash = effect.payload["context"]["rendered_prompt_sha256"].as_str()?;
    if !(1..=8).contains(&limit)
        || context_hash.len() != 64
        || !context_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    state
        .domain
        .execute(
            &Actor {
                id: "trusted-model-broker".into(),
                role: domain::Role::Policy,
            },
            DomainCommand::OpenPlanningRound {
                goal_id: goal_id.clone(),
                round_id: intent.into(),
                producer_id: effect.permit.payload.holder.clone(),
                context_hash: context_hash.into(),
                candidate_limit: limit,
            },
            now,
        )
        .ok()
}
pub(crate) fn search_open(state: &KernelState, goal_id: &str) -> bool {
    let Ok(goal) = state.domain.goal(goal_id) else {
        return false;
    };
    !state
        .final_evaluations
        .contains_key(&format!("{goal_id}/{}", goal.spec.version))
        && !state.evaluation_jobs.values().any(|job| {
            job.request.payload.goal_id == goal_id
                && job.request.payload.contract.goal_version == goal.spec.version
                && matches!(
                    job.request.payload.action,
                    crate::evaluator::EvaluationAction::Candidate {
                        phase: crate::evaluator::EvaluationPhase::HeldOut,
                        ..
                    }
                )
        })
}
/// A held-out selection closes work immediately; only completion of the existing Task drains.
pub(crate) fn final_bookkeeping_deadline(state: &KernelState, goal_id: &str) -> Option<u64> {
    let goal = state.domain.goal(goal_id).ok()?;
    state
        .evaluation_jobs
        .values()
        .filter(|job| {
            job.request.payload.goal_id == goal_id
                && job.request.payload.contract.goal_version == goal.spec.version
                && matches!(
                    job.request.payload.action,
                    crate::evaluator::EvaluationAction::Candidate {
                        phase: crate::evaluator::EvaluationPhase::HeldOut,
                        ..
                    }
                )
        })
        .map(|job| {
            job.request
                .payload
                .issued_ms
                .saturating_add(5_000)
                .min(job.request.payload.deadline_ms)
                .min(goal.spec.expiry_ms)
        })
        .min()
}
fn final_task_completion(
    state: &KernelState,
    request: &ActionRequest,
    payload: &Value,
    task: Option<&TaskContext>,
    now_ms: u64,
) -> bool {
    let Some(task) = task else { return false };
    let Some(grant) = state.security.grants.get(&request.grant_id) else {
        return false;
    };
    if request.action.kind != security::CapabilityKind::ToolCall
        || request.action.operation != "task_work"
        || request.budget_capability_id.is_some()
        || request.envelope_id.is_some()
        || !request.usage_bound.is_empty()
        || final_bookkeeping_deadline(state, &grant.goal_id)
            .is_none_or(|deadline| now_ms >= deadline)
    {
        return false;
    }
    matches!(
        serde_json::from_value::<DomainCommand>(payload.clone()),
        Ok(DomainCommand::CompleteTask { goal_id, task_id, fencing_token, .. })
            if goal_id == grant.goal_id && task_id == task.id && fencing_token == task.fence
    )
}
fn check_search_action(
    state: &KernelState,
    request: &ActionRequest,
    payload: &Value,
    task: Option<&TaskContext>,
    now_ms: u64,
) -> Result<()> {
    if task.is_none() {
        return Ok(()); // Protected service operations still use their separate authority checks.
    }
    let grant = state
        .security
        .grants
        .get(&request.grant_id)
        .context("action grant absent")?;
    ensure!(
        search_open(state, &grant.goal_id)
            || final_task_completion(state, request, payload, task, now_ms),
        "final selection closed work; only bounded existing Task completion may drain"
    );
    Ok(())
}
pub(crate) fn check_allocation(
    subject: &AuthenticatedSubject,
    leases: &[LeaseCheck],
) -> Result<()> {
    if !subject.is_human() {
        let id = crate::identity::WorkloadId::parse(subject.id())?;
        ensure!(
            leases
                .iter()
                .any(|lease| lease.id == format!("allocation/{}", id.agent)
                    && lease.holder == id.node
                    && lease.fence > 0),
            "operation requires the current allocation fence"
        );
    }
    Ok(())
}
pub(crate) fn check_task(
    state: &KernelState,
    subject: &AuthenticatedSubject,
    context: Option<&TaskContext>,
    claim: bool,
) -> Result<()> {
    if state
        .security
        .policy
        .payload
        .trusted_components
        .contains(subject.id())
        && context.is_none()
    {
        return Ok(());
    }
    let context = context.context("operation requires a Task context")?;
    let workload = crate::identity::WorkloadId::parse(subject.id())?;
    ensure!(
        context.incarnation == workload.incarnation.parse::<u64>()? && context.fence > 0,
        "Task incarnation/fence missing"
    );
    let task = state.domain.task(&workload.goal, &context.id)?;
    if claim {
        ensure!(
            task.spec.expiry_ms > crate::cluster::now_ms()
                && (task.fencing_token < context.fence
                    || task.claimed_by.as_deref() == Some(subject.id())
                        && task.fencing_token == context.fence),
            "Task claim is stale"
        );
    } else {
        ensure!(
            task.claimed_by.as_deref() == Some(subject.id())
                && task.incarnation == context.incarnation
                && task.fencing_token == context.fence
                && task.lease_expiry_ms > crate::cluster::now_ms()
                && matches!(
                    task.state,
                    domain::TaskState::Claimed | domain::TaskState::Running
                ),
            "Task ownership expired or fenced"
        );
    }
    Ok(())
}
pub fn verify_board_entry(state: &KernelState, entry: &Value) -> Result<()> {
    let signed: Value = serde_json::from_str(
        entry["signature"]
            .as_str()
            .context("Board signature absent")?,
    )?;
    let message: crate::transport::Message = serde_json::from_value(signed["message"].clone())?;
    let certificate = state
        .workload_certificates
        .get(
            signed["certificate_hash"]
                .as_str()
                .context("Board certificate reference absent")?,
        )
        .context("Board poster identity not recorded")?;
    let (_, parsed) = x509_parser::parse_x509_certificate(certificate)
        .map_err(|_| anyhow::anyhow!("Board certificate invalid"))?;
    let issued =
        x509_parser::time::ASN1Time::from_timestamp((message.issued_ms / 1000).try_into()?)?;
    ensure!(
        parsed.validity().is_valid_at(issued),
        "Board signer certificate was invalid when posted"
    );
    let identity = parsed
        .subject_alternative_name()?
        .context("Board poster SAN absent")?;
    ensure!(identity.value.general_names.iter().any(|name| matches!(name,x509_parser::extensions::GeneralName::URI(uri) if Some(*uri) == entry["author_id"].as_str())), "Board author differs from authenticated signer");
    let body = message.verify(
        &crate::identity::workload_public_key(certificate)?,
        message.issued_ms,
        1_048_576,
    )?;
    ensure!(
        message.path == "/tool/board_post"
            && body["topic"] == entry["topic"]
            && body["body"] == entry["body"]
            && message.request_id == entry["entry_id"],
        "Board poster signature does not bind entry contents"
    );
    Ok(())
}
pub fn hex(hash: &Hash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}
pub(crate) fn usable_task_input(
    state: &KernelState,
    goal_id: &str,
    task_id: &str,
) -> Result<Value> {
    let task = state.domain.task(goal_id, task_id)?;
    let mut pending: Vec<String> = state
        .task_input_sources
        .get(&format!("{goal_id}/{task_id}"))
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
    let mut visited = BTreeSet::new();
    let mut invalid = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        if state.security.retractions.contains_key(&id) {
            invalid.insert(id.clone());
        }
        if let Some(input) = state.security.inputs.get(&id) {
            pending.extend(input.parents.iter().cloned());
        } else if let Some(finding) = state.security.findings.get(&id) {
            pending.extend(finding.payload.inputs.iter().cloned());
        } else {
            invalid.insert(id);
        }
    }
    if invalid.is_empty() {
        Ok(task.spec.input.clone())
    } else {
        Ok(
            json!({"status":"replanning_required","task_id":task_id,"retired_input_sources":invalid,
        "excluded_candidate_hashes":state.domain.excluded_candidate_hashes.get(goal_id),"instruction":"Obtain fresh eligible evidence. Retired Task input remains archived and is unavailable to this incarnation."}),
        )
    }
}
pub(crate) fn lineage_markings(
    state: &KernelState,
    roots: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let mut pending: Vec<_> = roots.iter().cloned().collect();
    let mut seen = BTreeSet::new();
    let mut markings = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(input) = state.security.inputs.get(&id) {
            markings.extend(input.markings.iter().cloned());
            pending.extend(input.parents.iter().cloned());
        } else if let Some(finding) = state.security.findings.get(&id) {
            markings.extend(finding.payload.markings.iter().cloned());
            pending.extend(finding.payload.inputs.iter().cloned());
        } else {
            anyhow::bail!("lineage classification source missing");
        }
    }
    Ok(markings)
}
fn lineage_live(state: &KernelState, roots: &BTreeSet<String>) -> bool {
    let mut pending: Vec<_> = roots.iter().cloned().collect();
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if state.security.retractions.contains_key(&id) {
            return false;
        }
        if let Some(input) = state.security.inputs.get(&id) {
            pending.extend(input.parents.iter().cloned());
        } else if let Some(finding) = state.security.findings.get(&id) {
            pending.extend(finding.payload.inputs.iter().cloned());
        } else {
            return false;
        }
    }
    true
}
fn domain_actor(
    state: &KernelState,
    subject: &AuthenticatedSubject,
    command: &DomainCommand,
) -> Result<Actor> {
    let goal_id = serde_json::to_value(command)?["goal_id"]
        .as_str()
        .map(String::from)
        .context("domain command lacks Goal scope")?;
    let goal = state.domain.goal(&goal_id)?;
    let role = if matches!(
        command,
        DomainCommand::AcceptNeed { .. }
            | DomainCommand::EvaluateNeed { .. }
            | DomainCommand::ScoreAttempt { .. }
    ) {
        ensure!(
            subject.id() == goal.spec.evaluator_id
                && state
                    .security
                    .policy
                    .payload
                    .trusted_components
                    .contains(subject.id()),
            "separate registered evaluator required"
        );
        domain::Role::Evaluator
    } else if subject.is_human() {
        domain::Role::Operator
    } else {
        domain::Role::Agent
    };
    Ok(Actor {
        id: subject.id().into(),
        role,
    })
}
fn set_signature(command: &mut DomainCommand, signature: &str) -> Result<()> {
    match command {
        DomainCommand::AppendBoard {
            signature: value, ..
        }
        | DomainCommand::RetractFinding {
            signature: value, ..
        }
        | DomainCommand::AcceptNeed {
            signature: value, ..
        }
        | DomainCommand::EvaluateNeed {
            signature: value, ..
        } => {
            ensure!(
                !signature.is_empty(),
                "authenticated workload signature required"
            );
            *value = signature.into();
        }
        DomainCommand::ScoreAttempt { score, .. } => {
            ensure!(!signature.is_empty(), "evaluator signature required");
            score.signature = signature.into();
        }
        _ => {}
    }
    Ok(())
}
fn validate_domain_action(request: &ActionRequest, command: &DomainCommand) -> Result<()> {
    use security::CapabilityKind as K;
    let (kind, operation) = match command {
        DomainCommand::ReadBoard { .. } => (K::BoardRead, "board_read"),
        DomainCommand::AppendBoard { .. } | DomainCommand::CommitBoardCursor { .. } => {
            (K::BoardPost, "board_post")
        }
        DomainCommand::AppendFinding { .. } => (K::FindingAppend, "finding_append"),
        DomainCommand::DelegateTask { .. } => (K::TaskDelegate, "task_delegate"),
        DomainCommand::ProposeNeed { .. } => (K::ToolCall, "need_propose"),
        DomainCommand::AcceptNeed { .. }
        | DomainCommand::EvaluateNeed { .. }
        | DomainCommand::ScoreAttempt { .. } => (K::ToolCall, "evaluate"),
        DomainCommand::RetractFinding { .. } => (K::ToolCall, "finding_retract"),
        DomainCommand::SubmitAttempt { .. } => (K::ToolCall, "attempt_submit"),
        DomainCommand::ClaimTask { .. }
        | DomainCommand::StartTask { .. }
        | DomainCommand::RenewTask { .. }
        | DomainCommand::CompleteTask { .. }
        | DomainCommand::FailTask { .. } => (K::ToolCall, "task_work"),
        _ => anyhow::bail!("operator lifecycle commands require the operator interface"),
    };
    ensure!(
        request.action.kind == kind && request.action.operation == operation,
        "Capability does not authorize this domain command"
    );
    Ok(())
}

impl crate::operator::OperatorKernel for Kernel {
    fn audit_authentication<'a>(
        &'a self,
        id: &'a str,
        operator: Option<&'a crate::identity::VerifiedOperator>,
        accepted: bool,
        reason_hash: Hash,
        signer: &'a CryptoSigner,
        now: u64,
    ) -> crate::operator::KernelFuture<'a, ()> {
        Box::pin(async move {
            self.audit_operator_login(id, operator, accepted, reason_hash, signer, now, false)
                .await
        })
    }
    fn audit_control_authentication<'a>(
        &'a self,
        id: &'a str,
        operator: &'a crate::identity::VerifiedOperator,
        reason_hash: Hash,
        signer: &'a CryptoSigner,
        now: u64,
    ) -> crate::operator::KernelFuture<'a, ()> {
        Box::pin(async move {
            self.audit_operator_login(id, Some(operator), true, reason_hash, signer, now, true)
                .await
        })
    }
    fn control_security_state<'a>(
        &'a self,
        operator: &'a crate::identity::VerifiedOperator,
        command: &'a crate::operator::OperatorCommand,
        now: u64,
    ) -> crate::operator::KernelFuture<'a, SecurityState> {
        Box::pin(async move {
            ensure!(
                matches!(
                    command,
                    crate::operator::OperatorCommand::EngageStop { .. }
                        | crate::operator::OperatorCommand::Module {
                            command: crate::runtime::ModuleCommand::Unload { .. }
                                | crate::runtime::ModuleCommand::RetryCleanup { .. }
                        }
                ),
                "typed command is not a control operation"
            );
            let read = self.cluster.read_control(STATE_KEY).await?;
            let state: KernelState =
                serde_json::from_value(read.value.context("deployment not initialized")?)?;
            state.validate()?;
            operator.require_step_up(now, state.security.policy.payload.step_up_max_age)?;
            let subject = AuthenticatedSubject::operator(operator);
            let (verb, resource, scope, cost, tier) = command.permission();
            ensure!(
                state
                    .security
                    .policy
                    .payload
                    .root_humans
                    .contains(subject.id())
                    && state.security.permitted(
                        &subject,
                        verb,
                        resource,
                        &scope,
                        cost,
                        tier,
                        IsolationTier::P,
                        now
                    ),
                "current authoritative root control permission denied"
            );
            Ok(state.security)
        })
    }
    fn security_state(&self) -> crate::operator::KernelFuture<'_, SecurityState> {
        Box::pin(async move { Ok(self.state().await?.security) })
    }
    fn apply_operator<'a>(
        &'a self,
        operator: crate::identity::VerifiedOperator,
        request: crate::operator::OperatorRequest,
        signer: &'a CryptoSigner,
        now: u64,
    ) -> crate::operator::KernelFuture<'a, Value> {
        Box::pin(async move {
            use crate::operator::OperatorCommand as C;
            let subject = AuthenticatedSubject::operator(&operator);
            if let C::ReadState { scope } = &request.operation {
                let read = self.cluster.read(STATE_KEY).await?;
                let state: KernelState =
                    serde_json::from_value(read.value.context("deployment not initialized")?)?;
                return operator_read_view(
                    &state,
                    &subject,
                    scope,
                    read.revision,
                    &self.signer,
                    now,
                );
            }
            let control = matches!(
                &request.operation,
                C::EngageStop { .. }
                    | C::Module {
                        command: crate::runtime::ModuleCommand::Unload { .. }
                            | crate::runtime::ModuleCommand::RetryCleanup { .. }
                    }
            );
            let reply = self.commit_class(&request.request_id, subject.id(), &request.operation, vec![], control, |state| {
                let now = (crate::cluster::now_ms()/1000).max(now);
                subject.ensure_current(now)?;
                match &request.operation {
                    C::RegisterGoal { spec, posture, approvals } => {
                        validate_goal_pair(spec, posture)?;
                        state.security.register_goal(&subject, posture.clone(), approvals, &mut state.ledger, now)?;
                        state.domain.execute(&Actor { id: subject.id().into(), role: domain::Role::Operator }, DomainCommand::RegisterGoal { spec: spec.clone() }, now * 1000)
                    },
                    C::AmendGoal { spec, posture, approvals } => {
                        validate_goal_pair(spec, posture)?;
                        state.security.amend_goal(&subject, posture.clone(), approvals, &mut state.ledger, now)?;
                        state.desired_swarms.remove(&spec.goal_id);
                        state.domain.execute(&Actor { id: subject.id().into(), role: domain::Role::Operator }, DomainCommand::AmendGoal { spec: spec.clone() }, now * 1000)
                    },
                    C::Approve { action_hash, evidence_hash, grant_version, scope, expiry } => {
                        let approval=state.security.issue_approval(&operator,*action_hash,*evidence_hash,*grant_version,scope.clone(),*expiry,signer,now)?;
                        state.action_approvals.insert(hex(&content_hash(&approval)?),approval.clone());
                        Ok(serde_json::to_value(approval)?)
                    },
                    C::EngageStop { scope, reason } => {
                        state.security.engage_stop(&subject, scope.clone(), content_hash(reason)?, now)?;
                        if let Scope::Goal { goal, .. } = scope {
                            state.domain.execute(&Actor { id: subject.id().into(), role: domain::Role::Operator }, DomainCommand::StopGoal { goal_id: goal.clone(), reason: reason.clone() }, now * 1000)?;
                        }
                        Ok(json!({"scope":scope,"engaged":true,"epoch":state.security.stop_epoch}))
                    },
                    C::ReleaseStop { scope, approvals } => {
                        state.security.release_stop(&subject, scope.clone(), approvals, now)?;
                        if let Scope::Goal{goal,..}=scope
                            && !state.security.stopped(scope) && state.domain.goal(goal)?.status=="stopped" {
                                state.domain.execute(&Actor {id:subject.id().into(),role:domain::Role::Operator},DomainCommand::ResumeGoal {goal_id:goal.clone()},now*1000)?;
                        }
                        Ok(json!({"scope":scope,"engaged":false,"epoch":state.security.stop_epoch}))
                    },
                    C::CreateRole { role, approvals } => { state.security.create_role(&subject, role.clone(), approvals, now)?; Ok(json!({"role":role.id})) },
                    C::BindRole { binding, approvals } => { state.security.bind_role(&subject, binding.clone(), approvals, now)?; Ok(json!({"binding":binding.id})) },
                    C::InstallPolicy { policy, approvals } => {
                        state.security.install_policy(&subject, policy.clone(), approvals, &mut state.ledger, now)?;
                        state.desired_swarms.clear(); Ok(json!({"policy_version":policy.payload.version,"goals_require_readmission":true}))
                    },
                    C::ApplySwarm { spec } => {
                        let scope = Scope::Goal { tenant: spec.tenant.clone(), goal: spec.goal_id.clone() };
                        ensure!(state.security.permitted(&subject, "define", "agents", &scope, 0, ConfirmationTier::C2, IsolationTier::P, now), "swarm definition denied");
                        let goal = state.domain.goal(&spec.goal_id)?;
                        ensure!(goal.status == "active" && spec.tenant == state.security.policy.payload.tenant && spec.expiry_ms <= goal.spec.expiry_ms
                            && spec.agents.len() <= goal.spec.max_agents && spec.expiry_ms > now * 1000, "swarm exceeds Goal posture");
                        spec.gang(state.domain.revision, &BTreeMap::new())?;
                        ensure!(spec.agents.iter().all(|agent| agent.task.parent_task_id.is_none() && agent.task.capabilities.is_subset(&goal.spec.capabilities)
                            && agent.task.expiry_ms <= spec.expiry_ms && agent.capabilities.iter().all(|cap| cap.goal_id == spec.goal_id && cap.grant_version == goal.spec.version
                                && state.security.goals[&spec.goal_id].capability_ceiling.iter().any(|ceiling| cap.attenuates(ceiling)))), "swarm definitions widen Goal authority");
                        let allocation = spec.agents.iter().try_fold(0u64, |sum, agent| sum.checked_add(agent.task.budget_microusd).context("swarm allocation overflow"))?;
                        ensure!(allocation <= goal.spec.budget_microusd, "gang overcommits Goal budget");
                        state.desired_swarms.insert(spec.goal_id.clone(), (subject.id().into(), spec.clone()));
                        Ok(json!({"desired_swarm":spec.id,"status":"pending_placement"}))
                    },
                    C::ConfigureQuota { quota, approvals } => {
                        let hash = content_hash(&("configure-quota", quota, state.security.grant_epoch))?;
                        state.security.authorize_configuration_change(&subject, hash, content_hash(&state.security.policy)?, state.security.grant_epoch, Scope::Global, approvals, now)?;
                        ensure!(state.security.policy.payload.price_signers.contains(&quota.signer), "untrusted quota signer");
                        state.quotas.configure(quota.clone(), &quota.signer, now * 1000)?;
                        Ok(json!({"quota_scope":quota.payload.scope,"version":quota.payload.version}))
                    },
                    C::Module { command } => {
                        use crate::runtime::ModuleCommand as M;
                        let verb = match command { M::Register { .. } => "register", M::Unload { .. } => "unload", M::Reload { .. } => "reload", M::RetryCleanup { .. } => "retry_cleanup" };
                        let tier=if matches!(command,M::Register {..}) {ConfirmationTier::C2}else{ConfirmationTier::C1};
                        ensure!(state.security.permitted(&subject,verb,"modules",&Scope::Global,0,tier,IsolationTier::P,now), "module lifecycle permission denied");
                        match command {
                            M::Register { spec } => { state.modules.register(spec.clone())?; Ok(json!({"registered":spec.id,"services":state.modules.services()})) },
                            M::Unload { id } => Ok(serde_json::to_value(state.modules.unload(id, |_|anyhow::bail!("owned cleanup queued for the runtime"))?)?),
                            M::Reload { id } => Ok(json!({"activated":state.modules.reload(id)?})),
                            M::RetryCleanup { id } => {
                                state.modules.retry_cleanup(id, |_|anyhow::bail!("owned cleanup queued for retry"))?;
                                Ok(json!({"queued_cleanup":id}))
                            },
                        }
                    },
                    C::ReadState { .. } => anyhow::bail!("read queries require the read-only path"),
                }
            }).await?;
            reply.require()
        })
    }
}
fn operator_read_view(
    state: &KernelState,
    subject: &AuthenticatedSubject,
    scope: &Scope,
    revision: u64,
    signer: &CryptoSigner,
    now: u64,
) -> Result<Value> {
    let now = (crate::cluster::now_ms() / 1000).max(now);
    subject.ensure_current(now)?;
    ensure!(
        state.security.permitted(
            subject,
            "read",
            "state",
            scope,
            0,
            ConfirmationTier::C0,
            IsolationTier::P,
            now
        ),
        "state read denied"
    );
    if state
        .security
        .policy
        .payload
        .root_humans
        .contains(subject.id())
    {
        let head_time = now.max(
            state
                .audit
                .events()
                .last()
                .map_or(0, |event| event.timestamp),
        );
        return Ok(
            json!({"revision":revision,"domain_revision":state.domain.revision,"snapshot":state,"head":state.audit.signed_head(signer,head_time)?}),
        );
    }

    let clearance = state
        .security
        .policy
        .payload
        .recipient_clearances
        .get(subject.id())
        .cloned()
        .unwrap_or_default();
    ensure!(
        state.security.policy.payload.classification == "public"
            || clearance.contains(&state.security.policy.payload.classification),
        "operator lacks deployment classification clearance"
    );
    let tenant = &state.security.policy.payload.tenant;
    let visible = |goal: &str| match scope {
        Scope::Global => true,
        Scope::Tenant { tenant: requested } => requested == tenant,
        Scope::Goal {
            tenant: requested,
            goal: requested_goal,
        }
        | Scope::Agent {
            tenant: requested,
            goal: requested_goal,
            ..
        } => requested == tenant && requested_goal == goal,
    };
    let goals: Vec<_> = state.domain.goals.values().filter(|goal|visible(&goal.spec.goal_id)).map(|goal|json!({"goal_id":goal.spec.goal_id,"intent":goal.spec.intent,"version":goal.spec.version,"status":goal.status,"expiry_ms":goal.spec.expiry_ms})).collect();
    let needs: Vec<_> = state.domain.needs.values().filter(|need|visible(&need.goal_id)).map(|need|json!({"goal_id":need.goal_id,"id":need.spec.id,"description":need.spec.description,"accepted":need.accepted,"satisfied":need.satisfied})).collect();
    let tasks: Vec<_> = state.domain.tasks.values().filter(|task|visible(&task.goal_id) && match scope { Scope::Agent { holder, .. } => task.claimed_by.as_ref() == Some(holder), _ => true }).map(|task|json!({"goal_id":task.goal_id,"id":task.spec.id,"need_id":task.spec.need_id,"state":task.state,"claimed_by":task.claimed_by,"lease_expiry_ms":task.lease_expiry_ms,"fence":task.fencing_token})).collect();
    let findings: Vec<_> = state
        .domain
        .findings
        .values()
        .filter(|finding| visible(&finding.goal_id) && finding.markings.is_subset(&clearance))
        .collect();
    let attempts: Vec<_> = state
        .domain
        .attempts
        .values()
        .filter(|attempt| visible(&attempt.goal_id) && attempt.markings.is_subset(&clearance))
        .collect();
    let intents: Vec<_> = state.effects.iter().filter(|(_,effect)|visible(&effect.permit.payload.goal_id)).map(|(id,effect)|json!({"id":id,"goal_id":effect.permit.payload.goal_id,"state":effect.state,"operation":effect.request.action.operation})).collect();
    let accounts: BTreeMap<_, _> = state
        .ledger
        .accounts
        .iter()
        .filter(|(id, _)| id.strip_prefix("goal/").is_some_and(visible))
        .collect();
    let boards: BTreeMap<_, _> = state
        .domain
        .boards
        .iter()
        .filter(|(goal, _)| visible(goal))
        .map(|(goal, entries)| {
            (
                goal,
                entries
                    .iter()
                    .filter(|entry| {
                        state
                            .security
                            .inputs
                            .get(&format!("board/{goal}/{}", entry.seq))
                            .is_some_and(|input| input.markings.is_subset(&clearance))
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let reviews: Vec<_> = state.pending_actions.iter().filter(|(_,pending)|state.security.grants.get(&pending.request.grant_id).is_some_and(|grant|visible(&grant.goal_id))).map(|(intent,pending)|json!({"intent":intent,"review":pending.review,"resource":pending.request.action.resource,"operation":pending.request.action.operation})).collect();
    Ok(
        json!({"revision":revision,"domain_revision":state.domain.revision,"goals":goals,"needs":needs,"tasks":tasks,"boards":boards,"findings":findings,"attempts":attempts,"intents":intents,"reviews":reviews,"ledger_accounts":accounts,"services":state.modules.services(),"audit_size":state.audit.events().len(),"runtime_instances":state.runtime_instances}),
    )
}
fn validate_goal_pair(spec: &domain::GoalSpec, posture: &GoalPosture) -> Result<()> {
    ensure!(
        spec.goal_id == posture.id
            && spec.intent == posture.intent
            && spec.security == serde_json::to_value(posture)?
            && spec.version == posture.version
            && security::goal_contract_hash(spec)? == posture.contract_hash
            && spec.budget_microusd == posture.limits.total
            && spec.max_depth == u32::from(posture.max_depth)
            && spec.max_agents as u64 == posture.max_live_descendants
            && spec.max_open_tasks as u64 == posture.max_open_tasks
            && spec.expiry_ms / 1000 == posture.expiry,
        "domain/security Goal contract mismatch"
    );
    Ok(())
}

impl Kernel {
    #[allow(clippy::too_many_arguments)] // Only the trusted Gateway invokes these typed audit wrappers.
    async fn audit_operator_login(
        &self,
        id: &str,
        operator: Option<&crate::identity::VerifiedOperator>,
        accepted: bool,
        reason_hash: Hash,
        signer: &CryptoSigner,
        now: u64,
        control: bool,
    ) -> Result<()> {
        ensure!(
            accepted == operator.is_some(),
            "authentication result lacks verified identity"
        );
        let actor = operator
            .map(|human| human.id())
            .unwrap_or_else(|| "unauthenticated".into());
        if let Some(operator) = operator {
            ensure!(
                now < operator.expires_unix(),
                "authenticated operator expired"
            );
        }
        let read = if control {
            self.cluster.read_control(STATE_KEY).await?
        } else {
            self.cluster.read(STATE_KEY).await?
        };
        let state: KernelState =
            serde_json::from_value(read.value.context("deployment not initialized")?)?;
        state.validate()?;
        if control {
            let human = operator.context("root control login requires verified human")?;
            human.require_step_up(now, state.security.policy.payload.step_up_max_age)?;
            ensure!(
                state
                    .security
                    .policy
                    .payload
                    .root_humans
                    .contains(&human.id())
                    && state.security.permitted(
                        &AuthenticatedSubject::operator(human),
                        "stop",
                        "control",
                        &Scope::Global,
                        0,
                        ConfirmationTier::C3,
                        IsolationTier::P,
                        now
                    ),
                "root control login lost authority"
            );
        }
        let outcome = json!({"subject":actor,"accepted":accepted,"reason_hash":reason_hash});
        self.commit_class(
            id,
            "operator-authentication-gateway",
            &outcome,
            vec![],
            control,
            |state| {
                ensure!(
                    state
                        .security
                        .policy
                        .payload
                        .approval_signers
                        .contains(&signer.public_key()),
                    "authentication gateway signer is not admitted"
                );
                if control {
                    let human = operator.context("root control login has no verified human")?;
                    human.require_step_up(now, state.security.policy.payload.step_up_max_age)?;
                    ensure!(
                        state
                            .security
                            .policy
                            .payload
                            .root_humans
                            .contains(&human.id())
                            && state.security.permitted(
                                &AuthenticatedSubject::operator(human),
                                "stop",
                                "control",
                                &Scope::Global,
                                0,
                                ConfirmationTier::C3,
                                IsolationTier::P,
                                now
                            ),
                        "root control login lost current authority"
                    );
                }
                let signed = signer.sign("operator-authentication", outcome.clone())?;
                state.audit.append_committed(AuditEvent {
                    correlation: None,
                    id: format!("authentication/{id}"),
                    kind: "authentication".into(),
                    actor: actor.clone(),
                    goal: None,
                    timestamp: now.max(
                        state
                            .audit
                            .events()
                            .last()
                            .map_or(0, |event| event.timestamp),
                    ),
                    decision: if accepted { "accept" } else { "deny" }.into(),
                    payload_hash: content_hash(&signed)?,
                })?;
                Ok(json!({"audited":true}))
            },
        )
        .await?
        .require()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_empty_telemetry_fields_preserve_snapshot_and_checkpoint_hashes() {
        let fixture = crate::security::test_support::fixture();
        let mut state = KernelState::new(
            fixture.policy.clone(),
            &fixture.signer.public_key(),
            fixture.now,
            "legacy-snapshot".into(),
        )
        .unwrap();
        let checkpoint = state.content_hash().unwrap();
        state
            .audit
            .append(AuditEvent {
                correlation: None,
                id: "checkpoint/original".into(),
                kind: "state_checkpoint".into(),
                actor: "legacy-operator".into(),
                goal: None,
                timestamp: fixture.now,
                decision: "committed".into(),
                payload_hash: checkpoint,
            })
            .unwrap();
        let original = serde_json::to_value(&state).unwrap();
        for field in [
            "run_id",
            "diagnostic_reasons",
            "budget_pauses",
            "rate_pauses",
        ] {
            assert!(original.get(field).is_none());
        }
        assert!(original["audit"]["events"][0].get("correlation").is_none());
        let restored: KernelState = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(&restored).unwrap(), original);
        assert_eq!(restored.content_hash().unwrap(), checkpoint);
        assert_eq!(
            restored
                .audit
                .signed_head(&fixture.signer, fixture.now)
                .unwrap()
                .payload,
            state
                .audit
                .signed_head(&fixture.signer, fixture.now)
                .unwrap()
                .payload
        );
    }
    /// Only the diagnostic cache is manipulated on a retained test copy; authority was created normally.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn diagnostic_cache_eviction_cannot_clear_a_normally_registered_budget_pause() {
        let h = crate::node::receiver_tests::Harness::open_with_funding_cap(1).await;
        assert_eq!(
            h.send("eviction-claim", "/tool/claim_task", json!({}))
                .await
                .0,
            200
        );
        let denied=h.send("eviction-budget","/model",json!({"model":"m1","messages":[{"role":"user","content":"declared budget is too small"}],"max_tokens":16})).await;
        assert_eq!(denied.1["code"], "budget_exhausted");
        let mut retained = h.kernel.state().await.unwrap();
        assert!(retained.budget_paused("g", "task-1"));
        let record = retained.diagnostic_reasons["g/task-1"].clone();
        for index in 0..8193 {
            let mut diagnostic = record.clone();
            diagnostic.task_id = Some(format!("cache-only-{index}"));
            diagnostic.recorded_ms = record.recorded_ms + index as u64 + 1;
            diagnostic.reason = crate::telemetry::DiagnosticReason::Unknown;
            store_diagnostic(&mut retained, format!("g/cache-only-{index}"), diagnostic);
        }
        assert_eq!(retained.diagnostic_reasons.len(), 8192);
        assert!(!retained.diagnostic_reasons.contains_key("g/task-1"));
        assert!(retained.budget_paused("g", "task-1"));
        let decoded: KernelState =
            serde_json::from_slice(&serde_json::to_vec(&retained).unwrap()).unwrap();
        assert!(decoded.budget_paused("g", "task-1"));
        h.close().await;
    }
    use super::*;
    use crate::{
        cluster::{ClusterConfig, NodeAddress},
        ledger::{IntentState, Rate},
        security::test_support,
    };
    #[test]
    fn operator_consent_binds_all_goal_acceptance_terms() {
        let fixture = test_support::fixture();
        let spec = test_support::goal_spec(&fixture);
        validate_goal_pair(&spec, &fixture.posture).unwrap();
        let mut changed = spec.clone();
        changed.initial_needs[0].predicate = json!({"checker":"always-pass"});
        assert!(validate_goal_pair(&changed, &fixture.posture).is_err());
        changed = spec.clone();
        changed.evaluator_id = "other".into();
        assert!(validate_goal_pair(&changed, &fixture.posture).is_err());
        changed = spec;
        changed.rubric_version = "unapproved".into();
        assert!(validate_goal_pair(&changed, &fixture.posture).is_err());
    }
    #[tokio::test]
    async fn quorum_commits_admission_money_audit_once_and_preserves_unknown_receipts() {
        let mut fixture = test_support::fixture();
        for cap in &mut fixture.posture.capability_ceiling {
            cap.constraints.read_markings.insert("test".into());
        }
        for cap in &mut fixture.definition.capabilities {
            cap.constraints.read_markings.insert("test".into());
        }
        let approval_state = SecurityState::new(
            fixture.policy.clone(),
            &fixture.signer.public_key(),
            fixture.now,
        )
        .unwrap();
        fixture.approvals = vec![
            approval_state
                .issue_approval(
                    &fixture.operators[0],
                    fixture.posture.registration_hash().unwrap(),
                    [0; 32],
                    0,
                    Scope::Tenant { tenant: "t".into() },
                    fixture.now + 600,
                    &fixture.signer,
                    fixture.now,
                )
                .unwrap(),
        ];

        fixture
            .policy
            .payload
            .trusted_components
            .insert(fixture.subject.id().into());
        fixture.policy = fixture
            .signer
            .sign("deployment-policy", fixture.policy.payload)
            .unwrap();
        let address = "127.0.0.1:0".parse().unwrap();
        let directory = std::env::temp_dir().join(format!("solos-kernel-{}", uuid::Uuid::new_v4()));
        let cluster = Cluster::open(ClusterConfig {
            node_id: 1,
            bind_addr: address,
            nodes: [(
                1,
                NodeAddress {
                    addr: address,
                    server_name: fixture.identity.server_name.clone(),
                    certificate_sha256: hex(&crate::audit::hash(&fixture.identity.certificate_der)),
                },
            )]
            .into(),
            certificate_der: fixture.identity.certificate_der.clone(),
            private_key_der: fixture.identity.private_key_der.clone(),
            ca_der: fixture.identity.ca_der.clone(),
            directory: directory.clone(),
            storage_key: [13; 32],
            rotation_trust_key: None,
            max_frame_bytes: 4 * 1024 * 1024,
            max_queue_items: 32,
            history_limit: 128,
        })
        .await
        .unwrap();
        cluster.initialize([1].into()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let trusted = fixture.signer.public_key();
        let mut state = KernelState::new(
            fixture.policy.clone(),
            &trusted,
            fixture.now,
            "test-deployment".into(),
        )
        .unwrap();
        state
            .quotas
            .configure(
                fixture
                    .signer
                    .sign(
                        "provider-quota",
                        crate::quota::QuotaConfig {
                            scope: "organization/model".into(),
                            providers: vec!["local".into()],
                            version: "1".into(),
                            window_ms: 60_000,
                            requests: 100,
                            input_tokens: 10000,
                            output_tokens: 10000,
                            concurrent: 10,
                            expiry_ms: (fixture.now + 600) * 1000,
                        },
                    )
                    .unwrap(),
                &trusted,
                fixture.now * 1000,
            )
            .unwrap();
        let kernel = Kernel {
            cluster: cluster.clone(),
            signer: Arc::new(CryptoSigner::from_seed([44; 32])),
            max_state_bytes: 4 * 1024 * 1024,
            response_store: None,
        };
        kernel.bootstrap(state).await.unwrap();
        let operator = AuthenticatedSubject::operator(&fixture.operators[0]);
        let spec = test_support::goal_spec(&fixture);
        kernel
            .register_goal(
                "register",
                &operator,
                spec,
                fixture.posture.clone(),
                fixture.approvals.clone(),
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        let lease = cluster
            .transact(Transaction::new(
                "lease",
                vec![Mutation::GrantLease {
                    id: "allocation/a1".into(),
                    holder: "n1".into(),
                    expires_ms: (fixture.now + 600) * 1000,
                }],
            ))
            .await
            .unwrap();
        fixture.definition.lease.fencing_token = lease.revision;
        kernel
            .define_agent(
                "define",
                &operator,
                fixture.definition.clone(),
                None,
                Envelope {
                    id: "envelope".into(),
                    placement_intent_id: "placement".into(),
                    placement_hash: [1; 32],
                    parent: None,
                    goal_id: "g".into(),
                    holder: fixture.subject.id().into(),
                    accounts: ["global".into(), "tenant/t".into(), "goal/g".into()].into(),
                    allocation: 900_000,
                    spent: 0,
                    held: 0,
                    active: true,
                },
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        let prices = fixture
            .signer
            .sign(
                "price-table",
                PriceTable {
                    version: "1".into(),
                    provider: "local".into(),
                    model: "m1".into(),
                    currency: "microusd".into(),
                    not_before: fixture.now,
                    expires: fixture.now + 600,
                    rates: ["input_tokens", "cached_input_tokens", "output_tokens"]
                        .into_iter()
                        .map(|category| {
                            (
                                category.into(),
                                Rate {
                                    numerator: 1,
                                    denominator: 1,
                                },
                            )
                        })
                        .collect(),
                },
            )
            .unwrap();
        let payload = json!({"bounded":"request", "frozen_context": "x".repeat(32 * 1024)});
        let request = ActionRequest {
            intent_id: "effect-1".into(),
            grant_id: fixture.definition.id.clone(),
            grant_version: 1,
            capability_id: "agent-cap-0".into(),
            budget_capability_id: Some("agent-cap-6".into()),
            envelope_id: Some("envelope".into()),
            usage_bound: [
                ("input_tokens".into(), 10),
                ("cached_input_tokens".into(), 10),
                ("output_tokens".into(), 10),
            ]
            .into(),
            action: security::Action {
                kind: security::CapabilityKind::ToolCall,
                resource: "provider/local/model/m1".into(),
                operation: "inference".into(),
                arguments: payload.clone(),
                class: security::OperationClass::ReadOnly,
                isolation: IsolationTier::P,
                recipient: Some("provider/local/model/m1".into()),
                evidence: BTreeSet::new(),
            },
        };
        let checks = vec![LeaseCheck {
            id: "allocation/a1".into(),
            holder: "n1".into(),
            fence: lease.revision,
        }];
        let admission = || Admission {
            request: request.clone(),
            payload: payload.clone(),
            prices: Some(prices.clone()),
            approvals: vec![],
            leases: checks.clone(),
            quota: Some(crate::quota::QuotaRequest {
                intent: "effect-1".into(),
                scope: "organization/model".into(),
                input: 20,
                output: 10,
                deadline_ms: (fixture.now + 300) * 1000,
            }),
            task: None,
        };
        let admission_reply = kernel
            .admit("admit", &fixture.subject, admission())
            .await
            .unwrap()
            .require()
            .unwrap();
        assert!(
            serde_json::to_vec(&admission_reply).unwrap().len() < MAX_RESULT_RECEIPT_BYTES,
            "admission reply duplicated the complete frozen request"
        );
        let admitted = kernel.state().await.unwrap();
        assert_eq!(admitted.effects["effect-1"].payload, payload);
        assert_eq!(
            admitted.effects["effect-1"].request.action.arguments,
            payload
        );
        assert!(
            serde_json::to_vec(
                &admitted
                    .replies
                    .iter()
                    .find(|(id, _)| id.starts_with("admit/"))
                    .unwrap()
                    .1
                    .request
            )
            .unwrap()
            .len()
                < 4096,
            "admission replay identity duplicated the complete frozen request"
        );
        assert!(
            kernel
                .admit("admit", &fixture.subject, admission())
                .await
                .unwrap()
                .duplicate
        );
        assert_eq!(
            kernel.state().await.unwrap().ledger.accounts["global"]
                .total
                .held,
            30
        );
        let first = kernel
            .dispatch("dispatch", &fixture.subject, "effect-1", checks.clone())
            .await
            .unwrap();
        assert!(!first.duplicate);
        first.require().unwrap();
        let dispatched_state = kernel.state().await.unwrap();
        let dispatched_revision = cluster.view_control().await.unwrap().revision;
        assert!(
            kernel
                .admit("admit", &fixture.subject, admission())
                .await
                .unwrap()
                .duplicate
        );
        assert_eq!(
            cluster.view_control().await.unwrap().revision,
            dispatched_revision
        );
        assert_eq!(
            kernel.state().await.unwrap().content_hash().unwrap(),
            dispatched_state.content_hash().unwrap()
        );
        assert!(
            kernel
                .dispatch("dispatch", &fixture.subject, "effect-1", checks.clone())
                .await
                .unwrap()
                .duplicate
        );
        kernel
            .settle(
                "unknown",
                &fixture.subject,
                "effect-1",
                None,
                json!({"transport":"lost"}),
                false,
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        assert_eq!(
            kernel.state().await.unwrap().ledger.intents["effect-1"].state,
            IntentState::Unknown
        );
        let unknown_state = kernel.state().await.unwrap();
        assert!(
            kernel
                .admit("admit", &fixture.subject, admission())
                .await
                .unwrap()
                .duplicate
        );
        assert_eq!(
            kernel.state().await.unwrap().content_hash().unwrap(),
            unknown_state.content_hash().unwrap()
        );
        let state = kernel.state().await.unwrap();
        let request_hash = state.ledger.intents["effect-1"].request.request_hash;
        let bad = Receipt {
            id: "receipt".into(),
            provider: "local".into(),
            intent_id: "effect-1".into(),
            request_hash: [0; 32],
            usage: [
                ("input_tokens".into(), 4),
                ("cached_input_tokens".into(), 0),
                ("output_tokens".into(), 3),
            ]
            .into(),
        };
        let denial = kernel
            .settle(
                "bad-receipt",
                &fixture.subject,
                "effect-1",
                Some(bad.clone()),
                json!({}),
                false,
            )
            .await
            .unwrap();
        assert!(!denial.ok);
        assert_eq!(
            kernel.state().await.unwrap().ledger.accounts["global"]
                .total
                .held,
            30
        );
        let mut receipt = bad;
        receipt.request_hash = request_hash;
        kernel
            .settle(
                "receipt",
                &fixture.subject,
                "effect-1",
                Some(receipt.clone()),
                json!({"receipt":"receipt"}),
                false,
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        assert!(
            kernel
                .settle(
                    "receipt",
                    &fixture.subject,
                    "effect-1",
                    Some(receipt),
                    json!({"receipt":"receipt"}),
                    false
                )
                .await
                .unwrap()
                .duplicate
        );
        let state = kernel.state().await.unwrap();
        assert_eq!(state.ledger.accounts["global"].total.spent, 7);
        assert_eq!(state.ledger.accounts["global"].total.held, 0);
        let settled_hash = state.content_hash().unwrap();
        assert!(
            kernel
                .admit("admit", &fixture.subject, admission())
                .await
                .unwrap()
                .duplicate
        );
        assert_eq!(
            kernel.state().await.unwrap().content_hash().unwrap(),
            settled_hash
        );
        assert_eq!(
            state.effects["effect-1"]
                .correlation
                .as_ref()
                .unwrap()
                .execution_attempt,
            Some(1)
        );
        assert_eq!(
            state.audit.events().last().unwrap().payload_hash,
            state.content_hash().unwrap()
        );
        // A response larger than the entire quorum state ceiling remains paid and encrypted.
        use std::os::unix::fs::PermissionsExt;
        let private_root = directory.join("response-private");
        std::fs::create_dir(&private_root).unwrap();
        std::fs::set_permissions(&private_root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let response_key = private_root.join("key");
        crate::artifacts::ArtifactStore::create_key(&response_key).unwrap();
        let response_store = Arc::new(tokio::sync::Mutex::new(
            crate::artifacts::ArtifactStore::open(&private_root.join("blobs"), &response_key)
                .unwrap(),
        ));
        let capacity = Kernel {
            cluster: cluster.clone(),
            signer: kernel.signer.clone(),
            max_state_bytes: 1024 * 1024,
            response_store: Some(response_store),
        };
        let mut large_request = request.clone();
        large_request.intent_id = "large-paid".into();
        capacity
            .admit(
                "large-paid-admit",
                &fixture.subject,
                Admission {
                    request: large_request,
                    payload: payload.clone(),
                    prices: Some(prices.clone()),
                    approvals: vec![],
                    leases: checks.clone(),
                    quota: Some(crate::quota::QuotaRequest {
                        intent: "large-paid".into(),
                        scope: "organization/model".into(),
                        input: 20,
                        output: 10,
                        deadline_ms: (fixture.now + 300) * 1000,
                    }),
                    task: None,
                },
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        capacity
            .dispatch(
                "large-paid-dispatch",
                &fixture.subject,
                "large-paid",
                checks.clone(),
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        let large_hash = capacity.state().await.unwrap().ledger.intents["large-paid"]
            .request
            .request_hash;
        let pending = capacity.control_state().await.unwrap();
        let reserved_boundary = capacity.max_state_bytes - EFFECT_RECONCILIATION_CREDIT_BYTES + 1;
        assert!(
            ensure_state_capacity(&pending, reserved_boundary, capacity.max_state_bytes, true)
                .is_err()
        );
        assert!(
            ensure_state_capacity(
                &pending,
                capacity.max_state_bytes / 2,
                capacity.max_state_bytes,
                false
            )
            .is_ok()
        );
        let original_id = "provider-receipt-".repeat(80);
        let large_result = json!({"outcome":{"request_id":original_id,"usage":{"input_tokens":4,"output_tokens":3},"flow":{"payload_request_bytes":100,"payload_response_bytes":2097152}},"completion":{"content":"z".repeat(2*1024*1024)}});
        capacity
            .settle(
                "large-paid-settle",
                &fixture.subject,
                "large-paid",
                Some(Receipt {
                    id: original_id.clone(),
                    provider: "local".into(),
                    intent_id: "large-paid".into(),
                    request_hash: large_hash,
                    usage: [
                        ("input_tokens".into(), 4),
                        ("cached_input_tokens".into(), 0),
                        ("output_tokens".into(), 3),
                    ]
                    .into(),
                }),
                large_result.clone(),
                false,
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        let paid = capacity.state().await.unwrap();
        assert_eq!(paid.ledger.accounts["global"].total.spent, 14);
        assert!(
            paid.ledger.intents["large-paid"]
                .receipt
                .as_ref()
                .unwrap()
                .id
                .starts_with("upstream-sha256/")
        );
        assert_eq!(
            paid.effects["large-paid"].result.as_ref().unwrap()["body_missing"],
            false
        );
        assert!(serde_json::to_vec(&paid).unwrap().len() < 1024 * 1024);
        assert!(
            ensure_state_capacity(
                &paid,
                capacity.max_state_bytes,
                capacity.max_state_bytes,
                true
            )
            .is_ok()
        );
        assert_eq!(
            capacity
                .load_effect_result(&fixture.subject, "large-paid")
                .await
                .unwrap(),
            large_result
        );
        let mut wrong_holder = fixture.identity.identity.clone();
        wrong_holder.agent = "another-agent".into();
        let other = crate::identity::IdentityAuthority::generate(&wrong_holder.trust_domain)
            .unwrap()
            .issue(wrong_holder.clone(), fixture.now, 900)
            .unwrap();
        let other_subject = AuthenticatedSubject::workload(
            &other.certificate_der,
            &other.ca_der,
            wrong_holder,
            fixture.now,
        )
        .unwrap();
        assert!(
            capacity
                .load_effect_result(&other_subject, "large-paid")
                .await
                .is_err()
        );
        // An unavailable archive cannot roll back a known overrun receipt or its automatic stop.
        // Simulate a real filesystem failure while preserving existing ciphertext for restoration.
        let response_directory = private_root.join("blobs");
        let preserved_directory = private_root.join("blobs-preserved");
        std::fs::rename(&response_directory, &preserved_directory).unwrap();
        std::fs::write(&response_directory, b"archive filesystem unavailable").unwrap();
        let missing = Kernel {
            cluster: cluster.clone(),
            signer: kernel.signer.clone(),
            max_state_bytes: 1024 * 1024,
            response_store: capacity.response_store.clone(),
        };
        let mut missing_request = request.clone();
        missing_request.intent_id = "missing-body".into();
        missing
            .admit(
                "missing-body-admit",
                &fixture.subject,
                Admission {
                    request: missing_request,
                    payload: payload.clone(),
                    prices: Some(prices.clone()),
                    approvals: vec![],
                    leases: checks.clone(),
                    quota: Some(crate::quota::QuotaRequest {
                        intent: "missing-body".into(),
                        scope: "organization/model".into(),
                        input: 20,
                        output: 10,
                        deadline_ms: (fixture.now + 300) * 1000,
                    }),
                    task: None,
                },
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        missing
            .dispatch(
                "missing-body-dispatch",
                &fixture.subject,
                "missing-body",
                checks.clone(),
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        let missing_hash = missing.state().await.unwrap().ledger.intents["missing-body"]
            .request
            .request_hash;
        missing
            .settle(
                "missing-body-settle",
                &fixture.subject,
                "missing-body",
                Some(Receipt {
                    id: "missing-body-receipt".into(),
                    provider: "local".into(),
                    intent_id: "missing-body".into(),
                    request_hash: missing_hash,
                    usage: [
                        ("input_tokens".into(), 4),
                        ("cached_input_tokens".into(), 0),
                        ("output_tokens".into(), 100),
                    ]
                    .into(),
                }),
                json!({"completion":{"content":"q".repeat(2*1024*1024)}}),
                false,
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        std::fs::remove_file(&response_directory).unwrap();
        std::fs::rename(&preserved_directory, &response_directory).unwrap();
        let missing_state = missing.state().await.unwrap();
        assert_eq!(missing_state.ledger.accounts["global"].total.spent, 118);
        assert_eq!(missing_state.ledger.accounts["global"].total.held, 0);
        assert_eq!(
            missing_state.effects["missing-body"]
                .result
                .as_ref()
                .unwrap()["body_missing"],
            true
        );
        assert!(missing_state.security.stopped(&Scope::Goal {
            tenant: "t".into(),
            goal: "g".into()
        }));
        assert!(
            missing
                .load_effect_result(&fixture.subject, "missing-body")
                .await
                .is_err()
        );
        let state = missing_state;
        let baseline = serde_json::to_vec(&state).unwrap().len();
        let bounded = Kernel {
            cluster: cluster.clone(),
            signer: kernel.signer.clone(),
            max_state_bytes: (baseline + 2048) * 4 / 3,
            response_store: None,
        };
        assert!(
            bounded
                .commit(
                    "bulk-overflow",
                    fixture.subject.id(),
                    &"bulk",
                    vec![],
                    |_| Ok(json!({"body":"x".repeat(16_384)}))
                )
                .await
                .is_err()
        );
        assert_eq!(
            serde_json::to_vec(&bounded.state().await.unwrap())
                .unwrap()
                .len(),
            baseline
        );
        use crate::operator::OperatorKernel;
        let stop = bounded
            .apply_operator(
                fixture.operators[0].clone(),
                crate::operator::OperatorRequest {
                    request_id: "saturated-stop".into(),
                    operation: crate::operator::OperatorCommand::EngageStop {
                        scope: Scope::Global,
                        reason: "bounded-state probe".into(),
                    },
                },
                &fixture.signer,
                fixture.now,
            )
            .await
            .unwrap();
        assert_eq!(stop["engaged"], true);
        assert!(
            bounded
                .state()
                .await
                .unwrap()
                .security
                .stopped(&Scope::Global)
        );
        let before_read = serde_json::to_value(bounded.state().await.unwrap()).unwrap();
        let read = bounded
            .apply_operator(
                fixture.operators[0].clone(),
                crate::operator::OperatorRequest {
                    request_id: "read-only-query".into(),
                    operation: crate::operator::OperatorCommand::ReadState {
                        scope: Scope::Global,
                    },
                },
                &fixture.signer,
                fixture.now,
            )
            .await
            .unwrap();
        assert!(read["revision"].as_u64().unwrap() >= read["domain_revision"].as_u64().unwrap());
        assert_eq!(
            serde_json::to_value(bounded.state().await.unwrap()).unwrap(),
            before_read
        );
        cluster.shutdown().await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[cfg(test)]
#[path = "kernel_final_tests.rs"]
mod final_tests;
