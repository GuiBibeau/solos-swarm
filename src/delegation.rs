//! A child Task is runnable only after its authority, money and fenced placement commit.
use crate::{
    audit::{AuditEvent, content_hash},
    cluster::{Compare, LeaseCheck, Mutation, Transaction},
    domain::{Actor, DomainCommand, Role, TaskSpec},
    identity::WorkloadId,
    kernel::{
        Kernel, KernelState, RecordedReply, Reply, TaskContext, check_allocation, check_task,
    },
    placement::{self, DesiredAgent, SwarmSpec},
    scheduler::{Plan, Scheduler},
    security::{AuthenticatedSubject, CapabilityKind, Grant, InputRecord, Trust},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Build a child from an operator-admitted template and one current parent grant.
/// Capability names are exact template IDs or exact declared operations.
pub fn child_template(
    template: &DesiredAgent,
    parent: &Grant,
    task: TaskSpec,
    child_id: String,
    now_ms: u64,
) -> Result<DesiredAgent> {
    ensure!(
        task.capabilities.is_subset(&template.task.capabilities)
            && task.required_capabilities.is_subset(&task.capabilities),
        "child Task widens declared capability names"
    );
    ensure!(
        !task.capabilities.is_empty() && task.parent_task_id.as_deref() == Some(&template.task.id),
        "child Task parent or capabilities missing"
    );
    let mut selected = Vec::new();
    let mut resolved = BTreeSet::new();
    for declared in &template.capabilities {
        let full = task.capabilities.contains(&declared.id);
        let operations: BTreeSet<_> = declared
            .constraints
            .operations
            .intersection(&task.capabilities)
            .cloned()
            .collect();
        if !full && operations.is_empty() {
            continue;
        }
        let mut cap = declared.clone();
        if full {
            resolved.insert(declared.id.clone());
        } else {
            cap.constraints.operations = operations.clone();
        }
        resolved.extend(operations);
        cap.not_before = cap.not_before.max(now_ms / 1000);
        cap.expiry = cap.expiry.min(task.expiry_ms / 1000);
        let ancestor = parent
            .capabilities
            .iter()
            .find(|ancestor| cap.attenuates(ancestor))
            .context("requested child capability is outside the current parent grant")?;
        // Keep the exact declared name for descendants; placement gives each compiled cap a unique holder/incarnation ID.
        cap.parent = Some(ancestor.id.clone());
        cap.depth = ancestor
            .depth
            .checked_add(1)
            .context("capability depth overflow")?;
        cap.holder = parent.holder.uri()?;
        selected.push(cap);
    }
    ensure!(
        resolved == task.capabilities,
        "requested capability name is not an exact registered ID or operation"
    );
    ensure!(
        selected
            .iter()
            .any(|cap| cap.kind == CapabilityKind::ToolCall
                && cap.constraints.operations.contains("inference"))
            && selected
                .iter()
                .any(|cap| cap.kind == CapabilityKind::BudgetSpend)
            && selected
                .iter()
                .any(|cap| cap.kind == CapabilityKind::ToolCall
                    && cap.constraints.operations.contains("task_work")),
        "child requires explicitly requested inference, spend and Task-claim capabilities"
    );
    let mut child = template.clone();
    child.id = child_id;
    child.task = task;
    child.capabilities = selected;
    Ok(child)
}

impl Kernel {
    #[allow(clippy::too_many_arguments)] // Caller authority, current Task and its allocation must be checked in the same CAS.
    pub async fn delegate_current(
        &self,
        id: &str,
        subject: &AuthenticatedSubject,
        intent: &str,
        parent_grant_id: &str,
        parent_envelope_id: &str,
        context: TaskContext,
        task: TaskSpec,
        leases: Vec<LeaseCheck>,
    ) -> Result<Reply> {
        check_allocation(subject, &leases)?;
        ensure!(
            !id.is_empty() && id.len() <= 200,
            "invalid delegation request identity"
        );
        let request = json!({"intent":intent,"parent_grant":parent_grant_id,"parent_envelope":parent_envelope_id,"context":context,"task":task});
        let request_hash = content_hash(&(subject.id(), &request))?;
        let _write = self.cluster.kernel_write(false).await;
        for _ in 0..16 {
            let view = self.cluster.view().await?;
            let old = view
                .kv
                .get("kernel/state")
                .context("deployment not initialized")?
                .clone();
            let state: KernelState = serde_json::from_value(old.clone())?;
            if let Some(reply) = state.replies.get(id) {
                ensure!(
                    reply.request_hash == request_hash,
                    "delegation request ID content conflict"
                );
                return Ok(Reply {
                    ok: reply.ok,
                    value: reply.value.clone(),
                    duplicate: true,
                    revision: view.revision,
                });
            }
            let now_ms = crate::cluster::now_ms();
            let mut next = state.clone();
            let staged = stage(
                &mut next,
                subject,
                intent,
                parent_grant_id,
                parent_envelope_id,
                &context,
                &task,
                &view,
                &self.signer,
                now_ms,
            );
            let (ok, value, mut ops) = match staged {
                Ok((value, plan, spec)) => (true, value, placement_ops(&plan, &spec, now_ms)?),
                Err(error) => {
                    next = state;
                    (false, json!({"error":error.to_string()}), vec![])
                }
            };
            let timestamp = (now_ms / 1000).max(
                next.audit
                    .events()
                    .last()
                    .map_or(0, |event| event.timestamp),
            );
            next.audit.append_committed(AuditEvent {
                id: format!("request/{id}"),
                kind: "kernel_request".into(),
                actor: subject.id().into(),
                goal: Some(task_goal(subject)?),
                timestamp,
                decision: if ok { "allow" } else { "deny" }.into(),
                payload_hash: request_hash,
            })?;
            next.replies.insert(
                id.into(),
                RecordedReply {
                    actor: subject.id().into(),
                    request: request.clone(),
                    request_hash,
                    ok,
                    value: value.clone(),
                },
            );
            next.validate()?;
            next.checkpoint(id, subject.id(), timestamp)?;
            let encoded = serde_json::to_vec(&next)?;
            crate::kernel::ensure_state_capacity(
                &next,
                encoded.len(),
                self.max_state_bytes,
                false,
            )?;
            ops.push(Mutation::Put {
                key: "kernel/state".into(),
                value: serde_json::from_slice(&encoded)?,
            });
            let mut tx = Transaction::new(uuid::Uuid::new_v4().to_string(), ops);
            tx.expected_revision = Some(view.revision);
            tx.compares.push(Compare {
                key: "kernel/state".into(),
                expected: Some(old),
            });
            tx.leases = leases.clone();
            if ok {
                let nodes = tx
                    .ops
                    .iter()
                    .filter_map(|op| match op {
                        Mutation::Put { key, .. } => key.strip_prefix("nodes/").map(String::from),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                tx.leases
                    .extend(crate::scheduler::node_lease_checks(&view, &nodes)?);
            }
            let result = self.cluster.transact(tx).await?;
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
                "delegation transaction refused: {:?}",
                result.error
            );
        }
        anyhow::bail!("delegation concurrent state changes exceeded retry bound")
    }
}

#[allow(clippy::too_many_arguments)]
fn stage(
    state: &mut KernelState,
    subject: &AuthenticatedSubject,
    intent: &str,
    parent_grant_id: &str,
    parent_envelope_id: &str,
    context: &TaskContext,
    task: &TaskSpec,
    view: &crate::cluster::StateView,
    signer: &crate::audit::CryptoSigner,
    now_ms: u64,
) -> Result<(Value, Plan, SwarmSpec)> {
    check_task(state, subject, Some(context), false)?;
    let workload = WorkloadId::parse(subject.id())?;
    let goal = workload.goal.clone();
    ensure!(
        crate::kernel::search_open(state, &goal),
        "final selection closed child delegation"
    );
    ensure!(
        task.parent_task_id.as_deref() == Some(&context.id),
        "delegation must name current parent Task"
    );
    let effect = state
        .effects
        .get(intent)
        .context("delegation lacks an admitted intent")?
        .clone();
    ensure!(
        effect.state == "pending"
            && !effect.permit.payload.billable
            && effect.permit.payload.holder == subject.id()
            && effect.request.grant_id == parent_grant_id
            && effect.request.action.kind == CapabilityKind::TaskDelegate
            && effect.request.action.operation == "task_delegate",
        "delegation intent authority or state mismatch"
    );
    let DomainCommand::DelegateTask {
        goal_id,
        spec: admitted,
        ..
    } = serde_json::from_value::<DomainCommand>(effect.payload.clone())?
    else {
        anyhow::bail!("delegation intent payload is not a Task")
    };
    ensure!(
        goal_id == goal
            && content_hash(&admitted)? == content_hash(task)?
            && effect.request.action.arguments == effect.payload,
        "child Task differs from admitted arguments"
    );
    let effect_context = effect
        .task_context
        .as_ref()
        .context("delegation effect lacks current Task context")?;
    ensure!(
        content_hash(effect_context)? == content_hash(context)?,
        "delegation Task context changed"
    );
    let (parent, parent_envelope) = placement::current_parent(state, &goal, &context.id, now_ms)?;
    ensure!(
        parent.id == parent_grant_id
            && parent.holder.uri()? == subject.id()
            && parent_envelope == parent_envelope_id,
        "parent grant or envelope mismatch"
    );
    let allocation_lease = view
        .leases
        .get(&format!("allocation/{}", workload.agent))
        .context("current parent allocation absent")?;
    ensure!(
        !allocation_lease.revoked
            && allocation_lease.holder == workload.node
            && allocation_lease.fence == parent.lease.fencing_token
            && allocation_lease.expires_ms > now_ms,
        "parent allocation is fenced or expired"
    );
    ensure!(
        task.capabilities
            .is_subset(&state.domain.task(&goal, &context.id)?.spec.capabilities),
        "child Task widens current parent names"
    );
    let (operator, mut swarm) = state
        .desired_swarms
        .get(&goal)
        .context("parent runtime definition not registered")?
        .clone();
    let template = swarm
        .agents
        .iter()
        .find(|agent| agent.id == workload.agent && agent.task.id == context.id)
        .context("parent is not an admitted runtime template")?;
    let child_id = format!(
        "child-{}",
        hex(&content_hash(&(&goal, intent, &task.id))?[..16])
    );
    ensure!(
        swarm.agents.len() < 500
            && !swarm
                .agents
                .iter()
                .any(|agent| agent.id == child_id || agent.task.id == task.id),
        "child Task or agent already registered"
    );
    let child = child_template(template, &parent, task.clone(), child_id.clone(), now_ms)?;
    swarm.agents.push(child.clone());
    let mut only_child = swarm.clone();
    only_child.agents = vec![child];
    let gang = only_child.gang(view.revision, &BTreeMap::new())?;
    let plan = Scheduler::new(1)?.plan(&gang, &placement::snapshot(view)?)?;
    crate::scheduler::node_lease_checks(view, plan.updated_nodes.keys())?;
    let allocation = plan
        .allocations
        .first()
        .context("child placement missing")?
        .clone();
    state
        .security
        .consume_permit(subject, &effect.permit, &mut state.ledger, now_ms / 1000)?;
    placement::reserve_state(
        state,
        &only_child,
        &Actor {
            id: subject.id().into(),
            role: Role::Agent,
        },
        now_ms,
        &plan,
    )?;
    let parents = state
        .security
        .observed
        .get(subject.id())
        .cloned()
        .unwrap_or_default();
    let source = format!("delegation/{goal}/{child_id}");
    state.security.record_input(
        subject,
        InputRecord {
            id: source,
            source_type: "delegated_task".into(),
            hash: content_hash(&task.input)?,
            goal_id: goal.clone(),
            producer: subject.id().into(),
            trust: Trust::Untrusted,
            markings: BTreeSet::new(),
            parents,
        },
        signer,
        now_ms / 1000,
    )?;
    let child_uri = WorkloadId {
        trust_domain: swarm.trust_domain.clone(),
        tenant: swarm.tenant.clone(),
        goal: goal.clone(),
        node: allocation.node_id.clone(),
        agent: child_id,
        incarnation: "1".into(),
    }
    .uri()?;
    let inherited = state.security.observed[subject.id()].clone();
    state
        .task_input_sources
        .insert(format!("{goal}/{}", task.id), inherited.clone());
    state.security.observed.insert(child_uri, inherited);
    state
        .desired_swarms
        .insert(goal.clone(), (operator, swarm.clone()));
    let value = json!({"task":state.domain.task(&goal,&task.id)?,"allocation":allocation,"grant_id":format!("{}-1",allocation.agent_id)});
    let effect = state.effects.get_mut(intent).unwrap();
    effect.state = "done".into();
    effect.result = Some(value.clone());
    Ok((value, plan, swarm))
}
fn placement_ops(plan: &Plan, spec: &SwarmSpec, now_ms: u64) -> Result<Vec<Mutation>> {
    let mut ops = vec![Mutation::Put {
        key: format!("swarms/{}", spec.goal_id),
        value: serde_json::to_value(spec)?,
    }];
    for (id, node) in &plan.updated_nodes {
        ops.push(Mutation::Put {
            key: format!("nodes/{id}"),
            value: serde_json::to_value(node)?,
        });
    }
    for allocation in &plan.allocations {
        ops.push(Mutation::Put {
            key: format!("allocations/{}", allocation.agent_id),
            value: serde_json::to_value(allocation)?,
        });
        ops.push(Mutation::GrantLease {
            id: format!("allocation/{}", allocation.agent_id),
            holder: allocation.node_id.clone(),
            expires_ms: now_ms.saturating_add(spec.lease_ms).min(spec.expiry_ms),
        });
    }
    Ok(ops)
}
fn task_goal(subject: &AuthenticatedSubject) -> Result<String> {
    Ok(WorkloadId::parse(subject.id())?.goal)
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cluster::{Cluster, ClusterConfig, NodeAddress},
        kernel::Admission,
        ledger::Envelope,
        scheduler::{Node, Resources, Strategy},
        security::{self, Action, ActionRequest, IsolationTier, OperationClass, Scope},
    };
    use std::sync::Arc;
    fn template(f: &security::test_support::Fixture) -> DesiredAgent {
        DesiredAgent {
            id: "a1".into(),
            capabilities: f.posture.capability_ceiling.clone(),
            task: TaskSpec {
                id: "parent".into(),
                need_id: "answer".into(),
                parent_task_id: None,
                input: json!({"private_context":"tracked"}),
                required_capabilities: BTreeSet::new(),
                capabilities: f
                    .posture
                    .capability_ceiling
                    .iter()
                    .map(|cap| cap.id.clone())
                    .collect(),
                budget_microusd: 900_000,
                expiry_ms: (f.now + 500) * 1000,
            },
            resources: Resources {
                cpu_millicores: 500,
                memory_bytes: 256 * 1024 * 1024,
                provider_slots: 1,
                rate_tokens: 1,
            },
            labels: BTreeMap::new(),
            required_capabilities: BTreeSet::new(),
            skill_artifacts: vec![],
            history: crate::planner::HistoryConfig {
                context_token_budget: 8192,
                ..Default::default()
            },
        }
    }
    fn child(template: &DesiredAgent, id: &str) -> TaskSpec {
        TaskSpec {
            id: id.into(),
            need_id: "answer".into(),
            parent_task_id: Some("parent".into()),
            input: json!({"work":"narrow bounded work"}),
            required_capabilities: ["ceiling-0".into()].into(),
            capabilities: ["ceiling-0".into(), "ceiling-6".into()].into(),
            budget_microusd: 100_000,
            expiry_ms: template.task.expiry_ms,
        }
    }
    fn configured() -> security::test_support::Fixture {
        let mut f = security::test_support::fixture();
        f.posture.capability_ceiling[0]
            .constraints
            .operations
            .insert("task_work".into());
        f.posture.capability_ceiling[5]
            .constraints
            .operations
            .insert("task_delegate".into());
        f.definition.capabilities = f
            .posture
            .capability_ceiling
            .iter()
            .enumerate()
            .map(|(n, cap)| {
                let mut cap = cap.clone();
                cap.id = format!("agent-cap-{n}");
                cap.holder = f.subject.id().into();
                cap
            })
            .collect();
        let contract = security::test_support::goal_spec_posture(&f.posture);
        security::bind_goal_contract(&mut f.posture, &contract).unwrap();
        let state =
            security::SecurityState::new(f.policy.clone(), &f.signer.public_key(), f.now).unwrap();
        f.approvals = vec![
            state
                .issue_approval(
                    &f.operators[0],
                    f.posture.registration_hash().unwrap(),
                    [0; 32],
                    0,
                    Scope::Tenant { tenant: "t".into() },
                    f.now + 600,
                    &f.signer,
                    f.now,
                )
                .unwrap(),
        ];
        f
    }
    #[test]
    fn child_mapping_is_exact_and_preserves_template_limits() {
        let f = configured();
        let template = template(&f);
        let parent = Grant {
            id: "parent".into(),
            holder: f.definition.holder.clone(),
            goal_id: "g".into(),
            version: 1,
            definition_hash: [0; 32],
            capabilities: f.definition.capabilities.clone(),
            parent: None,
            lease: f.definition.lease.clone(),
            revoked: false,
        };
        let task = child(&template, "child");
        let derived = child_template(
            &template,
            &parent,
            task.clone(),
            "child-id".into(),
            f.now * 1000,
        )
        .unwrap();
        assert_eq!(derived.resources, template.resources);
        assert_eq!(derived.capabilities.len(), 2);
        assert!(
            derived
                .capabilities
                .iter()
                .all(|cap| cap.depth == 1
                    && cap.parent.as_deref().unwrap().starts_with("agent-cap-"))
        );
        let mut grandchild = task.clone();
        grandchild.id = "grandchild-task".into();
        grandchild.parent_task_id = Some(derived.task.id.clone());
        let mut child_grant = parent.clone();
        child_grant.capabilities = derived.capabilities.clone();
        for cap in &mut child_grant.capabilities {
            cap.id = format!("child-{}-1", cap.id);
        }
        let grandchild = child_template(
            &derived,
            &child_grant,
            grandchild,
            "grandchild-agent".into(),
            f.now * 1000,
        )
        .unwrap();
        assert!(
            grandchild
                .capabilities
                .iter()
                .all(|cap| cap.depth == 2 && cap.parent.as_deref().unwrap().starts_with("child-"))
        );
        let mut invalid = task.clone();
        invalid.capabilities.insert("tool.*".into());
        assert!(
            child_template(&template, &parent, invalid, "child-id".into(), f.now * 1000).is_err()
        );
        let mut missing_spend = task;
        missing_spend.capabilities.remove("ceiling-6");
        assert!(
            child_template(
                &template,
                &parent,
                missing_spend,
                "child-id".into(),
                f.now * 1000
            )
            .is_err()
        );
        let mut too_broad = parent;
        too_broad.capabilities[0]
            .constraints
            .operations
            .remove("inference");
        assert!(
            child_template(
                &template,
                &too_broad,
                child(&template, "child"),
                "child-id".into(),
                f.now * 1000
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn child_commits_once_with_parent_budget_fences_runtime_and_provenance() {
        let mut f = configured();
        let directory =
            std::env::temp_dir().join(format!("swarm-delegation-{}", uuid::Uuid::new_v4()));
        let address = "127.0.0.1:0".parse().unwrap();
        let cluster = Cluster::open(ClusterConfig {
            node_id: 1,
            bind_addr: address,
            nodes: [(
                1,
                NodeAddress {
                    addr: address,
                    server_name: f.identity.server_name.clone(),
                    certificate_sha256: hex(&crate::audit::hash(&f.identity.certificate_der)),
                },
            )]
            .into(),
            certificate_der: f.identity.certificate_der.clone(),
            private_key_der: f.identity.private_key_der.clone(),
            ca_der: f.identity.ca_der.clone(),
            directory: directory.clone(),
            storage_key: [19; 32],
            rotation_trust_key: None,
            max_frame_bytes: 8 * 1024 * 1024,
            max_queue_items: 32,
            history_limit: 128,
        })
        .await
        .unwrap();
        cluster.initialize([1].into()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let lease = cluster
            .transact(Transaction::new(
                "parent-lease",
                vec![Mutation::GrantLease {
                    id: "allocation/a1".into(),
                    holder: "n1".into(),
                    expires_ms: (f.now + 500) * 1000,
                }],
            ))
            .await
            .unwrap();
        f.definition.lease.fencing_token = lease.revision;
        let operator = AuthenticatedSubject::operator(&f.operators[0]);
        let actor = Actor {
            id: operator.id().into(),
            role: Role::Operator,
        };
        let mut state = KernelState::new(
            f.policy.clone(),
            &f.signer.public_key(),
            f.now,
            "delegation-test".into(),
        )
        .unwrap();
        state
            .security
            .register_goal(
                &operator,
                f.posture.clone(),
                &f.approvals,
                &mut state.ledger,
                f.now,
            )
            .unwrap();
        state
            .domain
            .execute(
                &actor,
                DomainCommand::RegisterGoal {
                    spec: security::test_support::goal_spec(&f),
                },
                f.now * 1000,
            )
            .unwrap();
        let template = template(&f);
        state
            .ledger
            .allocate(Envelope {
                id: "parent-envelope".into(),
                placement_intent_id: "parent-placement".into(),
                placement_hash: [1; 32],
                parent: None,
                goal_id: "g".into(),
                holder: f.subject.id().into(),
                accounts: ["global".into(), "tenant/t".into(), "goal/g".into()].into(),
                allocation: 900_000,
                spent: 0,
                held: 0,
                active: true,
            })
            .unwrap();
        state
            .security
            .compile_definition(f.definition.clone(), None, &mut state.ledger, f.now)
            .unwrap();
        state
            .domain
            .execute(
                &actor,
                DomainCommand::DelegateTask {
                    goal_id: "g".into(),
                    spec: template.task.clone(),
                    intent_id: "parent".into(),
                    placement_intent_id: "parent-placement".into(),
                    budget_envelope_ref: "parent-envelope".into(),
                },
                f.now * 1000,
            )
            .unwrap();
        state
            .domain
            .execute(
                &Actor {
                    id: f.subject.id().into(),
                    role: Role::Agent,
                },
                DomainCommand::ClaimTask {
                    goal_id: "g".into(),
                    task_id: "parent".into(),
                    incarnation: 1,
                    lease_expiry_ms: (f.now + 500) * 1000,
                    fencing_token: lease.revision,
                },
                f.now * 1000,
            )
            .unwrap();
        state
            .security
            .record_input(
                &f.subject,
                InputRecord {
                    id: "parent-observation".into(),
                    source_type: "fixture".into(),
                    hash: [4; 32],
                    goal_id: "g".into(),
                    producer: f.subject.id().into(),
                    trust: Trust::Untrusted,
                    markings: ["internal".into()].into(),
                    parents: BTreeSet::new(),
                },
                &f.signer,
                f.now,
            )
            .unwrap();
        let swarm = SwarmSpec {
            id: "swarm".into(),
            tenant: "t".into(),
            goal_id: "g".into(),
            trust_domain: "test.local".into(),
            provider: "local".into(),
            model: "m1".into(),
            strategy: Strategy::Pack,
            lease_ms: 60_000,
            agents: vec![template.clone()],
            expiry_ms: (f.now + 500) * 1000,
        };
        state
            .desired_swarms
            .insert("g".into(), (operator.id().into(), swarm));
        let kernel = Kernel {
            cluster: cluster.clone(),
            signer: Arc::new(crate::audit::CryptoSigner::from_seed([44; 32])),
            max_state_bytes: 8 * 1024 * 1024,
            response_store: None,
        };
        kernel.bootstrap(state).await.unwrap();
        cluster
            .transact(Transaction::new(
                "node",
                vec![
                    Mutation::GrantLease {
                        id: "node/n1".into(),
                        holder: "n1".into(),
                        expires_ms: (f.now + 500) * 1000,
                    },
                    Mutation::Put {
                        key: "nodes/n1".into(),
                        value: serde_json::to_value(Node {
                            id: "n1".into(),
                            capacity: Resources {
                                cpu_millicores: 1000,
                                memory_bytes: 512 * 1024 * 1024,
                                provider_slots: 2,
                                rate_tokens: 2,
                            },
                            used: template.resources,
                            labels: BTreeMap::new(),
                            capabilities: BTreeSet::new(),
                            healthy: true,
                            provider_rate_remaining: 1,
                        })
                        .unwrap(),
                    },
                ],
            ))
            .await
            .unwrap();
        let context = TaskContext {
            id: "parent".into(),
            incarnation: 1,
            fence: lease.revision,
        };
        let checks = vec![LeaseCheck {
            id: "allocation/a1".into(),
            holder: "n1".into(),
            fence: lease.revision,
        }];
        let spec = child(&template, "child-task");
        let admit = |intent: &str, spec: &TaskSpec| {
            let payload = serde_json::to_value(DomainCommand::DelegateTask {
                goal_id: "g".into(),
                spec: spec.clone(),
                intent_id: intent.into(),
                placement_intent_id: format!("request-{intent}"),
                budget_envelope_ref: format!("request-{intent}"),
            })
            .unwrap();
            Admission {
                request: ActionRequest {
                    intent_id: intent.into(),
                    grant_id: f.definition.id.clone(),
                    grant_version: 1,
                    capability_id: "agent-cap-5".into(),
                    budget_capability_id: None,
                    envelope_id: None,
                    usage_bound: BTreeMap::new(),
                    action: Action {
                        kind: CapabilityKind::TaskDelegate,
                        resource: "task/g".into(),
                        operation: "task_delegate".into(),
                        arguments: payload.clone(),
                        class: OperationClass::ReadOnly,
                        isolation: IsolationTier::P,
                        recipient: None,
                        evidence: BTreeSet::new(),
                    },
                },
                payload,
                prices: None,
                approvals: vec![],
                leases: checks.clone(),
                quota: None,
                task: Some(context.clone()),
            }
        };
        kernel
            .admit("admit-child", &f.subject, admit("child-intent", &spec))
            .await
            .unwrap()
            .require()
            .unwrap();
        let reply = kernel
            .delegate_current(
                "apply-child",
                &f.subject,
                "child-intent",
                &f.definition.id,
                "parent-envelope",
                context.clone(),
                spec.clone(),
                checks.clone(),
            )
            .await
            .unwrap();
        assert!(reply.ok, "{:?}", reply.value);
        assert!(
            kernel
                .delegate_current(
                    "apply-child",
                    &f.subject,
                    "child-intent",
                    &f.definition.id,
                    "parent-envelope",
                    context.clone(),
                    spec,
                    checks.clone()
                )
                .await
                .unwrap()
                .duplicate
        );
        let state = kernel.state().await.unwrap();
        let child_id = reply.value["allocation"]["agent_id"].as_str().unwrap();
        let grant = &state.security.grants[reply.value["grant_id"].as_str().unwrap()];
        assert_eq!(grant.parent.as_deref(), Some(f.definition.id.as_str()));
        assert_eq!(state.desired_swarms["g"].1.agents.len(), 2);
        assert_eq!(
            state
                .domain
                .task("g", "parent")
                .unwrap()
                .allocated_to_children_microusd,
            100_000
        );
        assert!(
            state.security.observed[&grant.holder.uri().unwrap()].contains("parent-observation")
        );
        assert!(state.task_input_sources["g/child-task"].contains("parent-observation"));
        let view = cluster.view().await.unwrap();
        assert!(view.kv.contains_key(&format!("allocations/{child_id}")));
        assert!(view.leases.contains_key(&format!("allocation/{child_id}")));
        let second = child(&template, "too-many");
        kernel
            .admit(
                "admit-overcapacity",
                &f.subject,
                admit("overcapacity", &second),
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        let denied = kernel
            .delegate_current(
                "apply-overcapacity",
                &f.subject,
                "overcapacity",
                &f.definition.id,
                "parent-envelope",
                context.clone(),
                second,
                checks.clone(),
            )
            .await
            .unwrap();
        assert!(!denied.ok);
        let state = kernel.state().await.unwrap();
        assert_eq!(state.desired_swarms["g"].1.agents.len(), 2);
        assert!(state.domain.task("g", "too-many").is_err());
        assert_eq!(
            state
                .domain
                .task("g", "parent")
                .unwrap()
                .allocated_to_children_microusd,
            100_000
        );
        // Admission can happen before final selection; creation must recheck fresh authority.
        let late = child(&template, "after-final");
        kernel
            .admit(
                "admit-before-final",
                &f.subject,
                admit("before-final", &late),
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        let mut closed = kernel.state().await.unwrap();
        let result = crate::evaluator::EvaluationResult {
            request_id: "final-fixture".into(),
            goal_id: "g".into(),
            evaluator_id: "protected-fixture".into(),
            contract: closed.domain.current_contract("g").unwrap(),
            state_revision: closed.domain.revision,
            attempt_id: Some("final".into()),
            candidate_hash: Some("frozen".into()),
            finding_ids: vec![],
            phase: Some(crate::evaluator::EvaluationPhase::HeldOut),
            score: None,
            measurements: BTreeMap::new(),
            verdict: crate::scenarios::Verdict::Pass,
            commands: vec![],
            protected_set_digest: "fixture".into(),
            evaluation_scope: "trusted-test-fixture".into(),
            measured_ms: crate::cluster::now_ms(),
        };
        closed.final_evaluations.insert(
            "g/1".into(),
            f.signer.sign("protected-evaluation-v1", result).unwrap(),
        );
        cluster
            .transact(Transaction::new(
                "close-search-fixture",
                vec![Mutation::Put {
                    key: "kernel/state".into(),
                    value: serde_json::to_value(closed).unwrap(),
                }],
            ))
            .await
            .unwrap();
        let denied = kernel
            .delegate_current(
                "apply-after-final",
                &f.subject,
                "before-final",
                &f.definition.id,
                "parent-envelope",
                context,
                late,
                checks,
            )
            .await
            .unwrap();
        assert!(!denied.ok);
        assert!(
            denied.value["error"]
                .as_str()
                .unwrap()
                .contains("final selection closed")
        );
        let unchanged = kernel.state().await.unwrap();
        assert!(unchanged.domain.task("g", "after-final").is_err());
        assert_eq!(unchanged.desired_swarms["g"].1.agents.len(), 2);
        assert_eq!(
            unchanged
                .domain
                .task("g", "parent")
                .unwrap()
                .allocated_to_children_microusd,
            100_000
        );
        cluster.shutdown().await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
