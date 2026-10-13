//! The Pi-facing broker. Worker input cannot select identity, authority, or provider keys.
use crate::{
    audit::{Signed, content_hash},
    cluster::LeaseCheck,
    context::ContextBuilder,
    domain::{DomainCommand, NeedSpec, Provenance, TaskSpec, TaskState},
    identity::WorkloadId,
    kernel::{Admission, Kernel, TaskContext, hex},
    ledger::{PriceTable, Receipt},
    planner::HistoryConfig,
    provider::{InferenceRequest, Outcome, Provider},
    quota::QuotaRequest,
    security::{Action, ActionRequest, CapabilityKind, IsolationTier, OperationClass, Scope},
    transport::{Handler, HandlerFuture, Message, Peer},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio::sync::{Mutex, watch};
use tracing::Instrument;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub operation: String,
    pub kind: CapabilityKind,
    pub resource: String,
    pub class: OperationClass,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub workload: WorkloadId,
    pub grant_id: String,
    pub envelope_id: String,
    pub task_id: String,
    pub allocation_fence: u64,
    pub task_fence: u64,
    pub lease_expiry_ms: u64,
    pub isolation: IsolationTier,
    pub provider: String,
    pub model: String,
    pub history: HistoryConfig,
    pub tools: Vec<ToolDefinition>,
}
#[derive(Clone)]
pub struct Backend {
    pub provider: Arc<dyn Provider>,
    pub prices: Signed<PriceTable>,
    pub context: Arc<ContextBuilder>,
}
pub trait ToolExecutor: Send + Sync {
    fn attach_tool_factory(
        &self,
        _factory: std::sync::Weak<dyn crate::tool_identity::ToolIdentityFactory>,
    ) -> Result<()> {
        anyhow::bail!("tool identity factory is not installed")
    }
    fn plan(
        &self,
        session: Session,
        definition: ToolDefinition,
        args: Value,
        intent: String,
        context: crate::tools::ToolContext,
    ) -> crate::tools::ToolPlanFuture;
    fn execute(
        &self,
        plan: crate::tools::ToolPlan,
        stop: watch::Receiver<bool>,
    ) -> crate::tools::ToolExecutionFuture;
}
#[derive(Clone)]
pub struct BrokerNode {
    pub ca_der: Vec<u8>,
    pub kernel: Arc<Kernel>,
    pub executor_subject: Arc<tokio::sync::RwLock<crate::security::AuthenticatedSubject>>,
    pub sessions: Arc<Mutex<BTreeMap<String, Session>>>,
    pub backends: BTreeMap<String, Backend>,
    pub stops: Arc<Mutex<BTreeMap<String, watch::Sender<bool>>>>,
    pub tools: Arc<dyn ToolExecutor>,
}
impl BrokerNode {
    async fn session(&self, peer: &Peer) -> Result<Session> {
        let session = self
            .sessions
            .lock()
            .await
            .get(peer.subject.id())
            .cloned()
            .context("workload has no compiled session")?;
        ensure!(
            session.workload == peer.workload && session.workload.incarnation.parse::<u64>()? > 0,
            "session incarnation mismatch"
        );
        let view = self.kernel.cluster.view().await?;
        let lease = view
            .leases
            .get(&format!("allocation/{}", peer.workload.agent))
            .context("allocation lease missing")?;
        ensure!(
            !lease.revoked
                && lease.holder == peer.workload.node
                && lease.fence == session.allocation_fence
                && lease.expires_ms > crate::cluster::now_ms() + 1000,
            "allocation expired or fenced; side effects stopped"
        );
        let state = self.kernel.state().await?;
        let grant = state
            .security
            .grants
            .get(&session.grant_id)
            .context("compiled grant absent")?;
        ensure!(
            grant.holder == peer.workload
                && !grant.revoked
                && grant.lease.fencing_token == session.allocation_fence
                && grant.lease.expiry > crate::cluster::now_ms() / 1000,
            "grant expired, revoked or superseded"
        );
        ensure!(
            !state.security.stopped(&Scope::Agent {
                tenant: peer.workload.tenant.clone(),
                goal: peer.workload.goal.clone(),
                holder: peer.subject.id().into()
            }),
            "kill switch engaged"
        );
        Ok(session)
    }
    fn leases(session: &Session) -> Vec<LeaseCheck> {
        vec![LeaseCheck {
            id: format!("allocation/{}", session.workload.agent),
            holder: session.workload.node.clone(),
            fence: session.allocation_fence,
        }]
    }
    fn task_context(session: &Session) -> Option<TaskContext> {
        Some(TaskContext {
            id: session.task_id.clone(),
            incarnation: session.workload.incarnation.parse().ok()?,
            fence: session.task_fence,
        })
    }
    #[allow(clippy::too_many_arguments)] // The broker supplies identity, operation, data recipient and billable bound explicitly.
    async fn request(
        &self,
        session: &Session,
        intent: &str,
        kind: CapabilityKind,
        operation: &str,
        resource: &str,
        payload: Value,
        class: OperationClass,
        billable: Option<BTreeMap<String, u64>>,
        recipient: Option<String>,
    ) -> Result<ActionRequest> {
        let state = self.kernel.state().await?;
        let grant = state
            .security
            .grants
            .get(&session.grant_id)
            .context("grant missing")?;
        let capability = grant
            .capabilities
            .iter()
            .find(|cap| {
                cap.kind == kind
                    && cap.resource.matches(resource)
                    && cap.constraints.operations.contains(operation)
            })
            .context("operation is not in the compiled grant")?;
        let budget = if billable.is_some() {
            Some(
                grant
                    .capabilities
                    .iter()
                    .find(|cap| {
                        cap.kind == CapabilityKind::BudgetSpend && cap.resource.matches(resource)
                    })
                    .context("model/tool has no spend Capability")?
                    .id
                    .clone(),
            )
        } else {
            None
        };
        Ok(ActionRequest {
            intent_id: intent.into(),
            grant_id: grant.id.clone(),
            grant_version: grant.version,
            capability_id: capability.id.clone(),
            budget_capability_id: budget,
            envelope_id: billable.as_ref().map(|_| session.envelope_id.clone()),
            usage_bound: billable.unwrap_or_default(),
            action: Action {
                kind,
                resource: resource.into(),
                operation: operation.into(),
                arguments: payload,
                class,
                isolation: session.isolation,
                recipient,
                evidence: BTreeSet::new(),
            },
        })
    }
    async fn route(
        &self,
        peer: Peer,
        path: String,
        message: Message,
        payload: Value,
    ) -> Result<Value> {
        ensure!(
            message.path == path
                && peer.subject.certificate_hash()
                    == Some(crate::audit::hash(&peer.certificate_der))
                && peer.public_key == crate::identity::workload_public_key(&peer.certificate_der)?,
            "workload proof binding mismatch"
        );
        ensure!(
            message.verify(&peer.public_key, crate::cluster::now_ms(), 1_048_576)? == payload,
            "signed payload differs from decoded request"
        );
        let session = self.session(&peer).await?;
        self.kernel.record_certificate(&peer).await?;
        let state = self.kernel.state().await?;
        let action_intent = if path == "/model" {
            Some(format!("model-{}", message.request_id))
        } else if path.starts_with("/tool/") {
            Some(format!("tool-{}", message.request_id))
        } else if path == crate::evaluator::EVALUATOR_COMMAND_PATH {
            Some(format!("evaluation-{}", message.request_id))
        } else {
            None
        };
        let correlation = state.correlation_for(
            peer.subject.id(),
            Some(&peer.workload.goal),
            Self::task_context(&session).as_ref(),
            action_intent.as_deref(),
        )?;
        drop(state);
        let span =
            crate::diagnostics::operation_span(&correlation, crate::diagnostics::Stage::Message, 1);
        if let Some(parent) = &message.traceparent {
            let incoming =
                crate::telemetry::TraceContext::parse(parent, message.tracestate.as_deref())?;
            ensure!(
                correlation
                    .trace_context
                    .as_ref()
                    .is_some_and(|expected| expected.trace_id == incoming.trace_id
                        && expected.tracestate == incoming.tracestate),
                "diagnostic Task trace differs from assigned Task"
            );
            crate::diagnostics::attach_parent(&span, &incoming)?;
        }
        let wait_ms = crate::cluster::now_ms().saturating_sub(message.issued_ms);
        crate::diagnostics::record(
            crate::diagnostics::Stage::Message,
            crate::diagnostics::Outcome::Success,
            wait_ms,
        );
        crate::diagnostics::with_trace_context(
            correlation.trace_context.clone(),
            self.route_session(peer, path, message, payload, session, correlation)
                .instrument(span),
        )
        .await
    }
    async fn route_session(
        &self,
        peer: Peer,
        path: String,
        message: Message,
        payload: Value,
        session: Session,
        mut correlation: crate::telemetry::Correlation,
    ) -> Result<Value> {
        if path == crate::evaluator::EVALUATOR_COMMAND_PATH {
            let state = self.kernel.state().await?;
            ensure!(
                state
                    .security
                    .policy
                    .payload
                    .trusted_components
                    .contains(peer.subject.id())
                    && state.domain.goal(&peer.workload.goal)?.spec.evaluator_id
                        == peer.subject.id(),
                "evaluator control route denied"
            );
            let batch: crate::evaluator::EvaluationBatch = serde_json::from_value(payload.clone())?;
            let intent = format!("evaluation-{}", message.request_id);
            let request = self
                .request(
                    &session,
                    &intent,
                    CapabilityKind::ToolCall,
                    "evaluate",
                    &format!("evaluate/{}", peer.workload.goal),
                    payload.clone(),
                    OperationClass::ReadOnly,
                    None,
                    None,
                )
                .await?;
            let admission = self
                .kernel
                .admit(
                    &format!("admit-{}", message.request_id),
                    &peer.subject,
                    Admission {
                        request,
                        payload,
                        prices: None,
                        approvals: vec![],
                        leases: Self::leases(&session),
                        quota: None,
                        task: None,
                    },
                )
                .await?
                .require()?;
            if admission["status"] == "confirmation_required" {
                return Ok(admission);
            }
            return self
                .kernel
                .evaluator_bridge(
                    &format!("apply-{}", message.request_id),
                    &peer,
                    &intent,
                    &batch,
                    &self.ca_der,
                    Self::leases(&session),
                )
                .await?
                .require();
        }
        if path == "/session" {
            ensure!(
                payload == json!({}),
                "session request accepts no authority overrides"
            );
            let backend = self
                .backends
                .get(&format!("{}/{}", session.provider, session.model))
                .context("admitted provider unavailable")?;
            let features = backend.provider.features();
            return Ok(
                json!({"workload":session.workload,"goal_id":session.workload.goal,"task_id":session.task_id,
                "trace_context":correlation.trace_context,
                "tools":session.tools,"model":{"id":session.model,"name":format!("Brokered {}",session.model),"reasoning":false,"input":["text"],
                    "contextWindow":features.context_tokens,"maxTokens":features.output_tokens,
                    "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
                    "compat":{"supportsStore":false,"supportsDeveloperRole":false,"maxTokensField":"max_tokens","supportsStrictMode":false}}}),
            );
        }
        if path == "/model" {
            correlation.intent_id = Some(format!("model-{}", message.request_id));
            let span = crate::diagnostics::operation_span(
                &correlation,
                crate::diagnostics::Stage::Provider,
                1,
            );
            return self
                .infer(&peer, &session, &message, payload)
                .instrument(span)
                .await;
        }
        if path == "/tool/explain" {
            validate_arguments(
                &json!({"type":"object","properties":{},"additionalProperties":false}),
                &payload,
            )?;
            let span = crate::diagnostics::operation_span(
                &correlation,
                crate::diagnostics::Stage::Tool,
                1,
            );
            return self
                .kernel
                .explain(
                    &peer.subject,
                    &peer.workload.goal,
                    &Self::task_context(&session).context("assigned Task absent")?,
                    &Self::leases(&session),
                )
                .instrument(span)
                .await;
        }
        let name = path.strip_prefix("/tool/").context("unknown broker path")?;
        let definition = session
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .context("tool not in approved definition")?
            .clone();
        validate_arguments(&definition.parameters, &payload)?;
        correlation.intent_id = Some(format!("tool-{}", message.request_id));
        let span =
            crate::diagnostics::operation_span(&correlation, crate::diagnostics::Stage::Tool, 1);
        self.tool(&peer, &session, &message, definition, payload)
            .instrument(span)
            .await
    }
    async fn infer(
        &self,
        peer: &Peer,
        session: &Session,
        message: &Message,
        payload: Value,
    ) -> Result<Value> {
        let intent = format!("model-{}", message.request_id);
        let wire_hash = hex(&content_hash(&payload)?);
        let state = self.kernel.state().await?;
        if let Some(effect) = state.effects.get(&intent) {
            ensure!(
                effect.permit.payload.holder == peer.subject.id()
                    && effect.payload["wire_hash"] == wire_hash,
                "model request ID conflict"
            );
            if effect.state == "done" {
                drop(state);
                self.kernel
                    .validate_cached_effect(&peer.subject, &intent, &Self::leases(session))
                    .await?;
                let completion = self
                    .kernel
                    .load_effect_result(&peer.subject, &intent)
                    .await?["completion"]
                    .clone();
                ensure!(
                    completion.is_object(),
                    "paid effect has no usable model completion; inspect its recorded receipt"
                );
                self.kernel
                    .observe(
                        &format!("response-{}", message.request_id),
                        &peer.subject,
                        format!("response/{intent}"),
                        "model_result".into(),
                        peer.workload.goal.clone(),
                        completion.clone(),
                    )
                    .await?
                    .require()?;
                self.kernel
                    .open_planning_round(
                        &format!("round-{}", message.request_id),
                        &peer.subject,
                        &intent,
                        Self::leases(session),
                    )
                    .await?
                    .require()?;
                return Ok(completion);
            }
            anyhow::bail!(
                "model effect {} is {}; reconcile this intent before issuing another request",
                intent,
                effect.state
            );
        }
        let task = state.domain.task(&peer.workload.goal, &session.task_id)?;
        ensure!(
            !state.rate_paused(
                &peer.workload.goal,
                &session.task_id,
                crate::cluster::now_ms()
            ),
            "Task is paused until shared provider quota is available"
        );
        if state.budget_paused(&peer.workload.goal, &session.task_id) {
            return Err(crate::ledger::BudgetExhausted {
                category: "Task paused until an authorized Goal budget amendment",
            }
            .into());
        }
        ensure!(
            crate::kernel::search_open(&state, &peer.workload.goal),
            "final selection closed search; inspect the signed final evaluation"
        );
        ensure!(
            task.claimed_by.as_deref() == Some(peer.subject.id())
                && task.incarnation == peer.workload.incarnation.parse::<u64>()?
                && task.fencing_token == session.task_fence
                && task.lease_expiry_ms > crate::cluster::now_ms()
                && matches!(task.state, TaskState::Claimed | TaskState::Running),
            "model call lacks current Task ownership"
        );
        let backend = self
            .backends
            .get(&format!("{}/{}", session.provider, session.model))
            .context("admitted provider unavailable")?;
        let (inference, admitted_payload, request, prices, quota) = if let Some(pending) =
            state.pending_actions.get(&intent)
        {
            ensure!(
                pending.review.holder == peer.subject.id()
                    && pending.review.certificate_hash == peer.subject.certificate_hash()
                    && pending.payload["wire_hash"] == wire_hash,
                "pending model intent changed proof or input"
            );
            let inference: InferenceRequest =
                serde_json::from_value(pending.payload["inference"].clone())?;
            (
                inference,
                pending.payload.clone(),
                pending.request.clone(),
                pending.prices.clone(),
                pending.quota.clone(),
            )
        } else {
            ensure!(
                payload["model"] == session.model,
                "model outside Goal allowlist"
            );
            ensure!(
                payload.get("chat_template_kwargs").is_none(),
                "worker cannot override the pinned chat template profile"
            );
            let messages = payload["messages"]
                .as_array()
                .context("model messages missing")?;
            let tools = payload
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for tool in &tools {
                let approved = session
                    .tools
                    .iter()
                    .find(|definition| tool["function"]["name"] == definition.name)
                    .context("undeclared model tool")?;
                ensure!(
                    tool["type"] == "function"
                        && tool["function"]["parameters"] == approved.parameters,
                    "model tool schema differs from pinned definition"
                );
            }
            let grant = &state.security.grants[&session.grant_id];
            let resource = format!("provider/{}/model/{}", session.provider, session.model);
            let cap = grant
                .capabilities
                .iter()
                .find(|cap| {
                    cap.kind == CapabilityKind::ToolCall
                        && cap.resource.matches(&resource)
                        && cap.constraints.operations.contains("inference")
                })
                .context("inference permission missing")?;
            let output = payload["max_tokens"]
                .as_u64()
                .context("explicit output bound required")?
                .min(backend.provider.features().output_tokens);
            let history_clearance = grant
                .capabilities
                .iter()
                .find(|c| {
                    c.kind == CapabilityKind::FindingRead
                        && c.resource
                            .matches(&format!("finding/{}", peer.workload.goal))
                        && c.constraints.operations.contains("finding_read")
                })
                .map(|c| {
                    cap.constraints
                        .read_markings
                        .intersection(&c.constraints.read_markings)
                        .cloned()
                        .collect::<BTreeSet<_>>()
                });
            ensure!(
                history_clearance.is_some()
                    || !state
                        .domain
                        .attempts
                        .values()
                        .any(|a| a.goal_id == peer.workload.goal),
                "candidate history requires a live FindingRead Capability"
            );
            let history_clearance = history_clearance.unwrap_or_default();
            let context = backend.context.prepare(
                &state.domain,
                &peer.workload.goal,
                &history_clearance,
                &session.history,
                messages,
                &tools,
                output.try_into()?,
            )?;
            let input_bound =
                serde_json::to_vec(&json!({"messages":context.messages,"tools":context.tools}))?
                    .len() as u64
                    + backend.context.spec.server_overhead_tokens as u64;
            let inference = InferenceRequest {
                intent_id: intent.clone(),
                goal_id: peer.workload.goal.clone(),
                task_id: session.task_id.clone(),
                holder: peer.subject.id().into(),
                grant_version: grant.version,
                provider: session.provider.clone(),
                model: session.model.clone(),
                input_tokens: context.input_tokens as u64 + context.server_overhead_tokens as u64,
                max_output_tokens: output,
                deadline_ms: (crate::cluster::now_ms() + 90_000)
                    .min(task.lease_expiry_ms)
                    .min(session.lease_expiry_ms),
                stream: false,
                messages: context.messages.clone(),
                tools: context.tools.clone(),
                temperature: payload["temperature"].as_f64().unwrap_or(0.0),
                disable_thinking: matches!(
                    backend.context.spec.format,
                    crate::context::ChatFormat::Qwen3LlamaCppB11429
                ),
            };
            backend
                .provider
                .validate(&inference, crate::cluster::now_ms())?;
            let admitted_payload =
                json!({"inference":inference,"wire_hash":wire_hash,"context":context});
            self.kernel
                .observe_context(
                    &format!("context-{}", message.request_id),
                    &peer.subject,
                    &session.grant_id,
                    &session.task_id,
                    format!("context/{intent}"),
                    admitted_payload.clone(),
                    Self::leases(session),
                )
                .await?
                .require()?;
            let request = self
                .request(
                    session,
                    &intent,
                    CapabilityKind::ToolCall,
                    "inference",
                    &resource,
                    admitted_payload.clone(),
                    OperationClass::ReadOnly,
                    Some(
                        [
                            ("input_tokens".into(), input_bound),
                            ("cached_input_tokens".into(), input_bound),
                            ("output_tokens".into(), output),
                        ]
                        .into(),
                    ),
                    Some(format!(
                        "provider/{}/model/{}",
                        session.provider, session.model
                    )),
                )
                .await?;
            let quota = QuotaRequest {
                intent: intent.clone(),
                scope: backend.provider.features().billing_scope.clone(),
                input: input_bound,
                output,
                deadline_ms: inference.deadline_ms,
            };
            (
                inference,
                admitted_payload,
                request,
                Some(backend.prices.clone()),
                Some(quota),
            )
        };
        drop(state); // Retain the frozen request, not the whole deployment, while awaiting provider/commit slots.
        backend
            .provider
            .validate(&inference, crate::cluster::now_ms())?;
        let cancel = self
            .stops
            .lock()
            .await
            .get(peer.subject.id())
            .context("workload supervisor stop channel missing")?
            .subscribe();
        // A local slot is not spend authority. Wait before creating any funded intent, then
        // derive and consume current quorum authority without a provider queue between them.
        let queue_started = std::time::Instant::now();
        let slot_result = backend
            .provider
            .acquire(inference.deadline_ms, cancel.clone())
            .await;
        crate::diagnostics::record(
            crate::diagnostics::Stage::ProviderQueue,
            if slot_result.is_ok() {
                crate::diagnostics::Outcome::Success
            } else {
                crate::diagnostics::Outcome::Denied
            },
            queue_started.elapsed().as_millis() as u64,
        );
        let slot = slot_result?;
        backend
            .provider
            .validate(&inference, crate::cluster::now_ms())?;
        let frozen_price = prices
            .as_ref()
            .context("inference prices missing")?
            .payload
            .clone();
        let admission = self
            .kernel
            .admit(
                &format!("admit-{}", message.request_id),
                &peer.subject,
                Admission {
                    request,
                    payload: admitted_payload,
                    prices,
                    approvals: vec![],
                    leases: Self::leases(session),
                    quota,
                    task: Self::task_context(session),
                },
            )
            .await?
            .require()?;
        if admission["status"] == "confirmation_required" {
            return Ok(admission);
        }
        let funded_request_hash =
            serde_json::from_value(admission["permit"]["payload"]["action_hash"].clone())?;
        drop(admission); // The financial binding is sufficient after admission; the effect is durable in quorum state.
        let dispatched = self
            .kernel
            .dispatch(
                &format!("dispatch-{}", message.request_id),
                &peer.subject,
                &intent,
                Self::leases(session),
            )
            .await?;
        if !dispatched.ok {
            let executor = self.executor_subject.read().await.clone();
            self.kernel
                .cancel_pending(
                    &format!("not-dispatched-{}", message.request_id),
                    &executor,
                    &intent,
                    "dispatch revalidation denied before provider contact".into(),
                )
                .await?
                .require()?;
        }
        ensure!(
            !dispatched.duplicate,
            "dispatch result already consumed; recover its recorded outcome"
        );
        dispatched.require()?;
        let provider_started = std::time::Instant::now();
        let outcome = backend.provider.dispatch(inference, cancel, slot).await;
        crate::diagnostics::record(
            crate::diagnostics::Stage::Provider,
            match outcome.outcome {
                Outcome::Completed => crate::diagnostics::Outcome::Success,
                Outcome::ReceiptWithoutCompletion => crate::diagnostics::Outcome::Failure,
                Outcome::DefinitelyNotDispatched => crate::diagnostics::Outcome::Denied,
                Outcome::Unknown => crate::diagnostics::Outcome::Unknown,
            },
            provider_started.elapsed().as_millis() as u64,
        );
        if let Some(usage) = &outcome.usage {
            crate::diagnostics::record_tokens(usage.input_tokens, usage.output_tokens);
        }
        let observed_flow = outcome.flow.as_ref().map(|flow| crate::flow::ObservedFlow {
            intent_id: flow.intent_id.clone(),
            goal_id: flow.goal_id.clone(),
            task_id: flow.task_id.clone(),
            holder: flow.holder.clone(),
            recipient: flow.recipient.clone(),
            destination: flow.destination.clone(),
            operation: "inference".into(),
            resource: format!("provider/{}/model/{}", session.provider, session.model),
            route: crate::flow::RouteKind::Provider,
            opened_ms: flow.opened_ms,
            closed_ms: Some(flow.closed_ms),
            contacted: flow.contacted,
            outcome: match flow.outcome {
                Outcome::Completed => crate::flow::FlowOutcome::Completed,
                Outcome::ReceiptWithoutCompletion => {
                    crate::flow::FlowOutcome::ReceiptWithoutCompletion
                }
                Outcome::DefinitelyNotDispatched => {
                    crate::flow::FlowOutcome::DefinitelyNotDispatched
                }
                Outcome::Unknown => crate::flow::FlowOutcome::Unknown,
            },
            payload_request_bytes: flow.payload_request_bytes,
            payload_response_bytes: flow.payload_response_bytes,
        });
        let mut completion = outcome.completion.clone();
        let receipt = if matches!(
            outcome.outcome,
            Outcome::Completed | Outcome::ReceiptWithoutCompletion
        ) {
            let usage = outcome
                .usage
                .as_ref()
                .context("complete inference lacks usage")?;
            let amount = frozen_price.cost(&usage.categories)?;
            crate::diagnostics::provider_attributes(
                &session.provider,
                &session.model,
                usage.input_tokens,
                usage.output_tokens,
                amount,
            );
            if let Some(value) = &mut completion {
                value["swarm_cost_microusd"] = json!(amount);
            }
            Some(Receipt {
                id: outcome
                    .request_id
                    .clone()
                    .context("complete inference lacks receipt ID")?,
                provider: session.provider.clone(),
                intent_id: intent.clone(),
                request_hash: funded_request_hash,
                usage: usage.categories.clone(),
            })
        } else {
            None
        };
        let result = json!({"outcome":outcome,"completion":completion});
        let executor = self.executor_subject.read().await.clone();
        self.kernel
            .settle(
                &format!("settle-{}", message.request_id),
                &executor,
                &intent,
                receipt,
                result,
                outcome.outcome == Outcome::DefinitelyNotDispatched,
            )
            .await?
            .require()?;
        if let Some(flow) = observed_flow {
            self.kernel.observe_flow(&executor, flow).await?;
        }
        if outcome.outcome != Outcome::Completed {
            let observed_revision = self.kernel.cluster.view_control().await?.revision;
            self.kernel
                .record_diagnostic(
                    &format!("provider-reason-{intent}"),
                    &executor,
                    &peer.workload.goal,
                    Some(&session.task_id),
                    crate::telemetry::DiagnosticReason::ProviderOutcome {
                        intent_id: intent.clone(),
                        outcome: outcome.outcome.clone(),
                    },
                    observed_revision,
                )
                .await?
                .require()?;
        }
        self.kernel
            .validate_cached_effect(&peer.subject, &intent, &Self::leases(session))
            .await?;
        let completion = completion.context(
            "provider returned no usable completion; inspect its recorded outcome and receipt",
        )?;
        self.kernel
            .observe(
                &format!("response-{}", message.request_id),
                &peer.subject,
                format!("response/{intent}"),
                "model_result".into(),
                peer.workload.goal.clone(),
                completion.clone(),
            )
            .await?
            .require()?;
        self.kernel
            .open_planning_round(
                &format!("round-{}", message.request_id),
                &peer.subject,
                &intent,
                Self::leases(session),
            )
            .await?
            .require()?;
        Ok(completion)
    }
    async fn tool(
        &self,
        peer: &Peer,
        session: &Session,
        message: &Message,
        tool: ToolDefinition,
        args: Value,
    ) -> Result<Value> {
        let intent = format!("tool-{}", message.request_id);
        let goal_id = peer.workload.goal.clone();
        let command = match tool.name.as_str() {
            "claim_task" => DomainCommand::ClaimTask {
                goal_id,
                task_id: session.task_id.clone(),
                incarnation: peer.workload.incarnation.parse()?,
                lease_expiry_ms: session.lease_expiry_ms,
                fencing_token: session.task_fence,
            },
            "board_read" => DomainCommand::ReadBoard {
                goal_id,
                subscriber_id: peer.subject.id().into(),
                after_seq: args["after_seq"].as_u64().unwrap_or(0),
                limit: args["limit"].as_u64().unwrap_or(32).try_into()?,
            },
            "board_post" => DomainCommand::AppendBoard {
                goal_id,
                entry_id: message.request_id.clone(),
                topic: args["topic"].as_str().context("topic missing")?.into(),
                body: args["body"].clone(),
                signature: String::new(),
            },
            "propose_need" => DomainCommand::ProposeNeed {
                goal_id,
                spec: serde_json::from_value::<NeedSpec>(args["spec"].clone())?,
            },
            "delegate_task" => {
                let mut spec: TaskSpec = serde_json::from_value(args["spec"].clone())?;
                spec.parent_task_id = Some(session.task_id.clone());
                DomainCommand::DelegateTask {
                    goal_id,
                    spec,
                    intent_id: message.request_id.clone(),
                    placement_intent_id: format!("child-{}", message.request_id),
                    budget_envelope_ref: format!("child-{}", message.request_id),
                }
            }
            "submit_finding" => DomainCommand::AppendFinding {
                goal_id,
                task_id: Some(session.task_id.clone()),
                body: args["body"].clone(),
                provenance: Provenance {
                    producer_id: String::new(),
                    recorder_id: String::new(),
                    observed_inputs: vec![],
                    source_refs: vec![],
                    signature: String::new(),
                },
            },
            "complete_task" => DomainCommand::CompleteTask {
                goal_id,
                task_id: session.task_id.clone(),
                fencing_token: session.task_fence,
                result_ref: args["result_ref"]
                    .as_str()
                    .context("result reference missing")?
                    .into(),
            },
            "submit_attempt" => {
                let state = self.kernel.state().await?;
                let planning_round_id = if let Some(effect) = state
                    .effects
                    .get(&intent)
                    .filter(|effect| effect.state == "done")
                {
                    effect.payload["planning_round_id"]
                        .as_str()
                        .context("completed attempt lost its original planning round")?
                        .into()
                } else {
                    state
                        .domain
                        .latest_planning_round(&goal_id, peer.subject.id())?
                        .id
                        .clone()
                };
                DomainCommand::SubmitAttempt {
                    goal_id,
                    attempt_id: message.request_id.clone(),
                    payload: args["payload"].clone(),
                    finding_ids: serde_json::from_value(args["finding_ids"].clone())?,
                    planning_round_id,
                }
            }
            "retract_finding" => DomainCommand::RetractFinding {
                goal_id,
                finding_id: args["finding_id"]
                    .as_str()
                    .context("Finding ID missing")?
                    .into(),
                reason: args["reason"]
                    .as_str()
                    .context("retraction reason missing")?
                    .into(),
                signature: String::new(),
            },
            "audit_read" => {
                let after_index = args["after_index"].as_u64().unwrap_or(0);
                let limit: usize = args["limit"].as_u64().unwrap_or(16).try_into()?;
                let payload = json!({"goal_id":goal_id,"after_index":after_index,"limit":limit});
                if let Some(cached) = self
                    .kernel
                    .cached_local_application(
                        &peer.subject,
                        &intent,
                        &payload,
                        (tool.kind, &tool.operation, &tool.resource),
                        &Self::leases(session),
                    )
                    .await?
                {
                    return Ok(cached);
                }
                let request = self
                    .request(
                        session,
                        &intent,
                        tool.kind,
                        &tool.operation,
                        &tool.resource,
                        payload.clone(),
                        tool.class,
                        None,
                        None,
                    )
                    .await?;
                let admission = self
                    .kernel
                    .admit(
                        &format!("admit-{}", message.request_id),
                        &peer.subject,
                        Admission {
                            request,
                            payload,
                            prices: None,
                            approvals: vec![],
                            leases: Self::leases(session),
                            quota: None,
                            task: Self::task_context(session),
                        },
                    )
                    .await?
                    .require()?;
                if admission["status"] == "confirmation_required" {
                    return Ok(admission);
                }
                return self
                    .kernel
                    .audit_view(
                        &format!("apply-{}", message.request_id),
                        &peer.subject,
                        &intent,
                        &goal_id,
                        after_index,
                        limit,
                        Self::leases(session),
                    )
                    .await?
                    .require();
            }
            "read_findings" => {
                let limit = args["limit"].as_u64().unwrap_or(128) as usize;
                let after_id = args["after_id"].as_str().map(str::to_owned);
                let payload = json!({"goal_id":goal_id,"after_id":after_id,"limit":limit});
                if let Some(cached) = self
                    .kernel
                    .cached_local_application(
                        &peer.subject,
                        &intent,
                        &payload,
                        (tool.kind, &tool.operation, &tool.resource),
                        &Self::leases(session),
                    )
                    .await?
                {
                    return Ok(cached);
                }
                let request = self
                    .request(
                        session,
                        &intent,
                        tool.kind,
                        &tool.operation,
                        &tool.resource,
                        payload.clone(),
                        tool.class,
                        None,
                        None,
                    )
                    .await?;
                let admission = self
                    .kernel
                    .admit(
                        &format!("admit-{}", message.request_id),
                        &peer.subject,
                        Admission {
                            request,
                            payload,
                            prices: None,
                            approvals: vec![],
                            leases: Self::leases(session),
                            quota: None,
                            task: Self::task_context(session),
                        },
                    )
                    .await?
                    .require()?;
                if admission["status"] == "confirmation_required" {
                    return Ok(admission);
                }
                return self
                    .kernel
                    .findings(
                        &format!("apply-{}", message.request_id),
                        &peer.subject,
                        &intent,
                        &goal_id,
                        after_id,
                        limit,
                        Self::leases(session),
                    )
                    .await?
                    .require();
            }
            "task_status" => {
                let limit = args["limit"].as_u64().unwrap_or(128) as usize;
                let after_id = args["after_id"].as_str().map(str::to_owned);
                let payload = json!({"goal_id":goal_id,"after_id":after_id,"limit":limit});
                if let Some(cached) = self
                    .kernel
                    .cached_local_application(
                        &peer.subject,
                        &intent,
                        &payload,
                        (tool.kind, &tool.operation, &tool.resource),
                        &Self::leases(session),
                    )
                    .await?
                {
                    return Ok(cached);
                }
                let request = self
                    .request(
                        session,
                        &intent,
                        tool.kind,
                        &tool.operation,
                        &tool.resource,
                        payload.clone(),
                        tool.class,
                        None,
                        None,
                    )
                    .await?;
                let admission = self
                    .kernel
                    .admit(
                        &format!("admit-{}", message.request_id),
                        &peer.subject,
                        Admission {
                            request,
                            payload,
                            prices: None,
                            approvals: vec![],
                            leases: Self::leases(session),
                            quota: None,
                            task: Self::task_context(session),
                        },
                    )
                    .await?
                    .require()?;
                if admission["status"] == "confirmation_required" {
                    return Ok(admission);
                }
                return self
                    .kernel
                    .task_status(
                        &format!("apply-{}", message.request_id),
                        &peer.subject,
                        &intent,
                        &goal_id,
                        after_id,
                        limit,
                        Self::leases(session),
                    )
                    .await?
                    .require();
            }
            "request_stop" | "request_flag" => {
                ensure!(
                    tool.kind == CapabilityKind::PolicyRequest,
                    "policy intervention Capability missing"
                );
                let operation = if tool.name == "request_stop" {
                    "stop"
                } else {
                    "flag"
                };
                ensure!(
                    tool.operation == operation,
                    "policy tool operation mismatch"
                );
                let request = self
                    .request(
                        session,
                        &intent,
                        tool.kind,
                        operation,
                        &tool.resource,
                        args,
                        OperationClass::ReadOnly,
                        None,
                        None,
                    )
                    .await?;
                return self
                    .kernel
                    .policy_intervention(
                        &message.request_id,
                        &peer.subject,
                        request,
                        Self::leases(session),
                    )
                    .await?
                    .require();
            }
            _ => {
                return self
                    .installed_tool(peer, session, message, tool, args)
                    .await;
            }
        };
        let payload = serde_json::to_value(&command)?;
        if let Some(cached) = self
            .kernel
            .cached_local_application(
                &peer.subject,
                &intent,
                &payload,
                (tool.kind, &tool.operation, &tool.resource),
                &Self::leases(session),
            )
            .await?
        {
            return Ok(cached);
        }
        let request = self
            .request(
                session,
                &intent,
                tool.kind,
                &tool.operation,
                &tool.resource,
                payload.clone(),
                tool.class,
                None,
                None,
            )
            .await?;
        let admission = self
            .kernel
            .admit(
                &format!("admit-{}", message.request_id),
                &peer.subject,
                Admission {
                    request,
                    payload,
                    prices: None,
                    approvals: vec![],
                    leases: Self::leases(session),
                    quota: None,
                    task: Self::task_context(session),
                },
            )
            .await?
            .require()?;
        if admission["status"] == "confirmation_required" {
            return Ok(admission);
        }
        if let DomainCommand::DelegateTask { spec, .. } = command {
            return self
                .kernel
                .delegate_current(
                    &format!("apply-{}", message.request_id),
                    &peer.subject,
                    &intent,
                    &session.grant_id,
                    &session.envelope_id,
                    Self::task_context(session).context("delegation Task context absent")?,
                    spec,
                    Self::leases(session),
                )
                .await?
                .require();
        }
        let signature = serde_json::to_string(
            &json!({"message":message,"certificate_hash":hex(&crate::audit::hash(&peer.certificate_der))}),
        )?;
        self.kernel
            .domain(
                &format!("apply-{}", message.request_id),
                &peer.subject,
                &intent,
                command,
                &signature,
                Self::leases(session),
            )
            .await?
            .require()
    }
    async fn installed_tool(
        &self,
        peer: &Peer,
        session: &Session,
        message: &Message,
        tool: ToolDefinition,
        args: Value,
    ) -> Result<Value> {
        let intent = format!("tool-{}", message.request_id);
        let state = self.kernel.state().await?;
        if let Some(effect) = state.effects.get(&intent) {
            ensure!(
                effect.permit.payload.holder == peer.subject.id()
                    && effect.payload["arguments"] == args
                    && effect.payload["tool"] == tool.name,
                "tool request ID conflict"
            );
            ensure!(
                effect.state == "done",
                "tool outcome {} requires reconciliation; effects are not replayed",
                effect.state
            );
            self.kernel
                .validate_cached_effect(&peer.subject, &intent, &Self::leases(session))
                .await?;
            return Ok(self
                .kernel
                .load_effect_result(&peer.subject, &intent)
                .await?["value"]
                .clone());
        }
        let grant = state
            .security
            .grants
            .get(&session.grant_id)
            .context("tool grant absent")?;
        let capability = grant
            .capabilities
            .iter()
            .find(|c| {
                c.kind == tool.kind
                    && c.resource.matches(&tool.resource)
                    && c.constraints.operations.contains(&tool.operation)
            })
            .context("tool Capability absent")?;
        let parents = state
            .security
            .observed
            .get(peer.subject.id())
            .cloned()
            .unwrap_or_default();
        let mut markings: BTreeSet<String> = parents
            .iter()
            .filter_map(|id| state.security.inputs.get(id))
            .flat_map(|input| input.markings.iter().cloned())
            .collect();
        markings.insert(state.security.policy.payload.classification.clone());
        let context = crate::tools::ToolContext {
            holder: peer.subject.id().into(),
            goal: peer.workload.goal.clone(),
            markings,
            clearance: capability.constraints.read_markings.clone(),
            parents,
            recipients: capability.constraints.recipients.clone(),
            signer: self.kernel.signer.clone(),
        };
        let mut plan = self
            .tools
            .plan(session.clone(), tool.clone(), args, intent.clone(), context)
            .await?;
        if !plan.inputs.is_empty() {
            self.kernel
                .observe_tool_inputs(
                    &format!("inputs-{}", message.request_id),
                    &peer.subject,
                    &session.grant_id,
                    plan.inputs.clone(),
                    Self::task_context(session),
                    Self::leases(session),
                )
                .await?
                .require()?;
            let state = self.kernel.state().await?;
            plan.payload["input_sources"] = serde_json::to_value(
                state
                    .security
                    .observed
                    .get(peer.subject.id())
                    .cloned()
                    .unwrap_or_default(),
            )?;
            plan.payload["input_markings"] = serde_json::to_value(&plan.markings)?;
        }
        if let Some(pending) = state.pending_actions.get(&intent) {
            plan.freeze_pending(pending)?;
        }
        let billable = plan.prices.as_ref().map(|_| plan.usage_bound.clone());
        let recipient = plan.payload["arguments"]["recipient"]
            .as_str()
            .map(str::to_owned);
        let mut request = self
            .request(
                session,
                &intent,
                tool.kind,
                &tool.operation,
                &tool.resource,
                plan.payload.clone(),
                tool.class,
                billable,
                recipient,
            )
            .await?;
        request.action.isolation = plan.isolation;
        let admitted = self
            .kernel
            .admit(
                &format!("admit-{}", message.request_id),
                &peer.subject,
                Admission {
                    request,
                    payload: plan.payload.clone(),
                    prices: plan.prices.clone(),
                    approvals: vec![],
                    leases: Self::leases(session),
                    quota: None,
                    task: Self::task_context(session),
                },
            )
            .await?;
        let admission = admitted.require()?;
        if admission["status"] == "confirmation_required" {
            return Ok(admission);
        }
        let stop = self
            .stops
            .lock()
            .await
            .get(peer.subject.id())
            .context("tool supervisor missing")?
            .subscribe();
        let dispatched = self
            .kernel
            .dispatch(
                &format!("dispatch-{}", message.request_id),
                &peer.subject,
                &intent,
                Self::leases(session),
            )
            .await?;
        if !dispatched.ok {
            let executor = self.executor_subject.read().await.clone();
            self.kernel
                .cancel_pending(
                    &format!("not-dispatched-{}", message.request_id),
                    &executor,
                    &intent,
                    "dispatch revalidation denied before tool execution".into(),
                )
                .await?
                .require()?;
        }
        ensure!(
            !dispatched.duplicate,
            "tool dispatch already consumed; recover recorded outcome"
        );
        dispatched.require()?;
        let target_flow = if tool.operation == "target_read" {
            Some(crate::flow::ObservedFlow {
                intent_id: intent.clone(),
                goal_id: peer.workload.goal.clone(),
                task_id: session.task_id.clone(),
                holder: peer.subject.id().into(),
                recipient: tool.resource.clone(),
                destination:
                    plan.payload["execution_manifest"]["targets"][&tool.resource]["endpoint"]
                        .as_str()
                        .context("target observation has no frozen receiver")?
                        .into(),
                operation: tool.operation.clone(),
                resource: tool.resource.clone(),
                route: crate::flow::RouteKind::Target,
                opened_ms: crate::cluster::now_ms(),
                closed_ms: None,
                contacted: false,
                outcome: crate::flow::FlowOutcome::DispatchAttempt,
                payload_request_bytes: 0,
                payload_response_bytes: 0,
            })
        } else {
            None
        };
        let executor = self.executor_subject.read().await.clone();
        if let Some(flow) = &target_flow {
            self.kernel.observe_flow(&executor, flow.clone()).await?;
        }
        let tool_started = std::time::Instant::now();
        let execution = match self.tools.execute(plan, stop).await {
            Ok(result) => result,
            Err(error) => crate::tools::ToolExecution {
                completed: false,
                value: json!({"error":error.to_string()}),
                usage: BTreeMap::new(),
                markings: BTreeSet::new(),
                transport: None,
            },
        };
        let executor = self.executor_subject.read().await.clone();
        let completed = execution.completed;
        crate::diagnostics::record(
            crate::diagnostics::Stage::Tool,
            if completed {
                crate::diagnostics::Outcome::Success
            } else {
                crate::diagnostics::Outcome::Unknown
            },
            tool_started.elapsed().as_millis() as u64,
        );
        let target_flow = target_flow.map(|mut flow| {
            flow.closed_ms = Some(crate::cluster::now_ms());
            flow.outcome = if completed {
                crate::flow::FlowOutcome::Completed
            } else {
                crate::flow::FlowOutcome::Unknown
            };
            if let Some(transport) = &execution.transport {
                flow.contacted = transport.contacted;
                flow.payload_request_bytes = transport.payload_request_bytes;
                flow.payload_response_bytes = transport.payload_response_bytes;
            }
            flow
        });
        self.kernel
            .complete_tool(
                &format!("result-{}", message.request_id),
                &executor,
                &peer.subject,
                &intent,
                execution,
            )
            .await?
            .require()?;
        if let Some(flow) = target_flow {
            self.kernel.observe_flow(&executor, flow).await?;
        }
        ensure!(
            completed,
            "tool outcome unknown; held liabilities remain reserved"
        );
        self.kernel
            .validate_cached_effect(&peer.subject, &intent, &Self::leases(session))
            .await?;
        Ok(self
            .kernel
            .load_effect_result(&peer.subject, &intent)
            .await?["value"]
            .clone())
    }
}
pub fn make_tools(grant: &crate::security::Grant) -> Vec<ToolDefinition> {
    let goal = &grant.goal_id;
    let object = |properties: Value, required: Vec<&str>| json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
    let text = || json!({"type":"string","minLength":1,"maxLength":65536});
    let loose = || json!({"type":"object","properties":{},"additionalProperties":true});
    let entries = [
        (
            "claim_task",
            "Claim the assigned Task and read its input.",
            CapabilityKind::ToolCall,
            "task_work",
            format!("task/{goal}"),
            object(json!({}), vec![]),
        ),
        (
            "board_read",
            "Read committed Board entries with source restrictions.",
            CapabilityKind::BoardRead,
            "board_read",
            format!("board/{goal}"),
            object(
                json!({"after_seq":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":128}}),
                vec![],
            ),
        ),
        (
            "audit_read",
            "Read Goal-scoped audit metadata and signed inclusion proofs.",
            CapabilityKind::ToolCall,
            "audit_read",
            format!("audit/{goal}"),
            object(
                json!({"after_index":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":32}}),
                vec![],
            ),
        ),
        (
            "board_post",
            "Post an untrusted observation. A post grants no authority.",
            CapabilityKind::BoardPost,
            "board_post",
            format!("board/{goal}"),
            object(
                json!({"topic":text(),"body":loose()}),
                vec!["topic", "body"],
            ),
        ),
        (
            "propose_need",
            "Propose a derived Need for independent evaluator acceptance.",
            CapabilityKind::ToolCall,
            "need_propose",
            format!("need/{goal}"),
            object(json!({"spec":loose()}), vec!["spec"]),
        ),
        (
            "delegate_task",
            "Delegate narrower work and carve its budget from the current Task.",
            CapabilityKind::TaskDelegate,
            "task_delegate",
            format!("task/{goal}"),
            object(json!({"spec":loose()}), vec!["spec"]),
        ),
        (
            "submit_finding",
            "Record a Finding. The broker supplies its provenance and markings.",
            CapabilityKind::FindingAppend,
            "finding_append",
            format!("finding/{goal}"),
            object(json!({"body":loose()}), vec!["body"]),
        ),
        (
            "read_findings",
            "Read eligible Goal Findings within clearance.",
            CapabilityKind::FindingRead,
            "finding_read",
            format!("finding/{goal}"),
            object(
                json!({"after_id":text(),"limit":{"type":"integer","minimum":1,"maximum":128}}),
                vec![],
            ),
        ),
        (
            "task_status",
            "Read bounded current Goal Task statuses within metadata clearance; no Task inputs or bodies.",
            CapabilityKind::ToolCall,
            "task_status",
            format!("task/{goal}"),
            object(
                json!({"after_id":text(),"limit":{"type":"integer","minimum":1,"maximum":128}}),
                vec![],
            ),
        ),
        (
            "complete_task",
            "Complete the current Task with an immutable result reference.",
            CapabilityKind::ToolCall,
            "task_work",
            format!("task/{goal}"),
            object(json!({"result_ref":text()}), vec!["result_ref"]),
        ),
        (
            "submit_attempt",
            "Submit a candidate and its recorded Finding references for scoring.",
            CapabilityKind::ToolCall,
            "attempt_submit",
            format!("attempt/{goal}"),
            object(
                json!({"payload":loose(),"finding_ids":{"type":"array","items":text(),"maxItems":500}}),
                vec!["payload", "finding_ids"],
            ),
        ),
        (
            "retract_finding",
            "Retract an eligible own Finding while retaining its archive.",
            CapabilityKind::ToolCall,
            "finding_retract",
            format!("finding/{goal}"),
            object(
                json!({"finding_id":text(),"reason":text()}),
                vec!["finding_id", "reason"],
            ),
        ),
        (
            "request_flag",
            "Record only the exact scope, trigger and evidence registered in the policy Capability.",
            CapabilityKind::PolicyRequest,
            "flag",
            format!("policy/{goal}"),
            object(
                json!({"scope":loose(),"trigger":text(),"reason_hash":{"type":"array","items":{"type":"integer","minimum":0,"maximum":255},"minItems":32,"maxItems":32},"confidence_milli":{"type":"integer","minimum":0,"maximum":1000}}),
                vec!["scope", "trigger", "reason_hash", "confidence_milli"],
            ),
        ),
        (
            "request_stop",
            "Request only the exact safety scope and trigger registered in the policy Capability.",
            CapabilityKind::PolicyRequest,
            "stop",
            format!("stop/{goal}"),
            object(
                json!({"scope":loose(),"trigger":text(),"reason_hash":{"type":"array","items":{"type":"integer","minimum":0,"maximum":255},"maxItems":32},"confidence_milli":{"type":"integer","minimum":0,"maximum":1000}}),
                vec!["scope", "trigger", "reason_hash", "confidence_milli"],
            ),
        ),
        (
            "explain",
            "Read the recorded reason for this Task at the authoritative revision; an absent reason is unknown.",
            CapabilityKind::ToolCall,
            "task_work",
            format!("task/{goal}"),
            object(json!({}), vec![]),
        ),
    ];
    entries
        .into_iter()
        .filter(|(_, _, kind, operation, resource, _)| {
            grant.capabilities.iter().any(|cap| {
                cap.kind == *kind
                    && cap.resource.matches(resource)
                    && cap.constraints.operations.contains(*operation)
                    && cap.constraints.resources.contains(resource)
            })
        })
        .map(
            |(name, description, kind, operation, resource, parameters)| ToolDefinition {
                name: name.into(),
                description: description.into(),
                parameters,
                operation: operation.into(),
                kind,
                resource,
                class: OperationClass::ReadOnly,
            },
        )
        .collect()
}
impl Handler for BrokerNode {
    fn handle(&self, peer: Peer, path: String, message: Message, payload: Value) -> HandlerFuture {
        let node = self.clone();
        Box::pin(async move {
            match node
                .route(peer.clone(), path, message.clone(), payload)
                .await
            {
                Ok(value) => Ok(value),
                Err(error) => {
                    let executor = node.executor_subject.read().await.clone();
                    node.kernel
                        .audit_workload_denial(&executor, &peer, &message, &error.to_string())
                        .await
                        .context("request denied; durable denial journal unavailable")?;
                    Err(error)
                }
            }
        })
    }
}
fn validate_arguments(schema: &Value, value: &Value) -> Result<()> {
    if schema["type"] == "object" {
        let args = value
            .as_object()
            .context("tool arguments must be an object")?;
        if let Some(required) = schema["required"].as_array() {
            for name in required {
                ensure!(
                    args.contains_key(name.as_str().context("invalid required schema")?),
                    "required tool argument missing"
                );
            }
        }
        let properties = schema["properties"]
            .as_object()
            .context("tool object schema missing properties")?;
        for (name, value) in args {
            if let Some(property) = properties.get(name) {
                validate_arguments(property, value)?;
            } else {
                ensure!(
                    schema["additionalProperties"] == true,
                    "unknown tool argument"
                );
            }
        }
    } else if schema["type"] == "string" {
        let text = value.as_str().context("string tool argument expected")?;
        ensure!(
            schema["minLength"]
                .as_u64()
                .is_none_or(|n| text.chars().count() as u64 >= n)
                && schema["maxLength"]
                    .as_u64()
                    .is_none_or(|n| text.chars().count() as u64 <= n),
            "tool text length outside bound"
        );
    } else if schema["type"] == "integer" {
        let n = value.as_i64().context("integer tool argument expected")?;
        ensure!(
            schema["minimum"].as_i64().is_none_or(|bound| n >= bound)
                && schema["maximum"].as_i64().is_none_or(|bound| n <= bound),
            "tool integer outside bound"
        );
    } else if schema["type"] == "array" {
        let items = value.as_array().context("array tool argument expected")?;
        ensure!(
            schema["maxItems"]
                .as_u64()
                .is_none_or(|n| items.len() as u64 <= n),
            "tool array outside bound"
        );
        for item in items {
            validate_arguments(&schema["items"], item)?;
        }
    } else {
        ensure!(schema["type"].is_null(), "unsupported tool schema type");
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tool_ingress_enforces_required_unknown_types_and_bounds() {
        let schema = json!({"type":"object","properties":{"n":{"type":"integer","minimum":1,"maximum":3}},"required":["n"],"additionalProperties":false});
        assert!(validate_arguments(&schema, &json!({"n":2})).is_ok());
        for invalid in [
            json!({}),
            json!({"n":4}),
            json!({"n":"2"}),
            json!({"n":2,"authority":"root"}),
        ] {
            assert!(validate_arguments(&schema, &invalid).is_err());
        }
    }
}

#[cfg(test)]
#[path = "node_tests.rs"]
pub(crate) mod receiver_tests;

#[cfg(test)]
#[path = "node_replay_tests.rs"]
mod replay_tests;
