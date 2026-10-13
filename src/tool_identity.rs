//! Narrow native-tool holders are descendants of one dispatched invocation, never Pi agents.
use crate::{
    audit::{AuditEvent, content_hash},
    cluster::{Compare, LeaseCheck, Mutation, Transaction},
    domain::{Actor, DomainCommand, Role, TaskSpec, TaskState},
    identity::{IdentityAuthority, WorkloadId},
    kernel::{Kernel, KernelState},
    ledger::{Envelope, IntentState},
    node::{Session, ToolDefinition},
    sandbox::VmBrokerBinding,
    scheduler::{Allocation, Bundle, Gang, Resources, Scheduler, Strategy},
    security::{AgentDefinition, AuthenticatedSubject, Capability, CapabilityKind, Lease},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Weak},
};
use tokio::sync::{Mutex, RwLock, watch};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSpawnRequest {
    pub invocation_intent: String,
    pub parent: Session,
    pub calls: Vec<ToolDefinition>,
    pub budget_microusd: u64,
    pub deadline_ms: u64,
}
pub type ToolBindingFuture = Pin<Box<dyn Future<Output = Result<ToolBindingGuard>> + Send>>;
pub trait ToolIdentityFactory: Send + Sync {
    fn issue(&self, request: ToolSpawnRequest) -> ToolBindingFuture;
    fn reserve(
        &self,
        request: crate::resource_guard::ResourceRequest,
    ) -> crate::resource_guard::ResourceFuture;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    request_hash: crate::audit::Hash,
    invocation_intent: String,
    parent_holder: String,
    allocation: Allocation,
    session: Session,
    status: String,
    unit: String,
    directory: PathBuf,
    guest_cid: Option<u32>,
}
pub struct ScopedToolIdentityFactory {
    me: Weak<Self>,
    kernel: Arc<Kernel>,
    authority: Arc<IdentityAuthority>,
    runtime: Arc<RwLock<AuthenticatedSubject>>,
    sessions: Arc<Mutex<BTreeMap<String, Session>>>,
    stops: Arc<Mutex<BTreeMap<String, watch::Sender<bool>>>>,
    worker_directory: PathBuf,
    identity_directory: PathBuf,
}
impl ScopedToolIdentityFactory {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kernel: Arc<Kernel>,
        authority: Arc<IdentityAuthority>,
        runtime: Arc<RwLock<AuthenticatedSubject>>,
        sessions: Arc<Mutex<BTreeMap<String, Session>>>,
        stops: Arc<Mutex<BTreeMap<String, watch::Sender<bool>>>>,
        worker_directory: PathBuf,
        identity_directory: PathBuf,
    ) -> Result<Arc<Self>> {
        ensure!(
            worker_directory.is_absolute() && identity_directory.is_absolute(),
            "tool identity roots must be absolute"
        );
        private_dir(&identity_directory)?;
        Ok(Arc::new_cyclic(|me| Self {
            me: me.clone(),
            kernel,
            authority,
            runtime,
            sessions,
            stops,
            worker_directory,
            identity_directory,
        }))
    }
    async fn issue_owned(self: Arc<Self>, request: ToolSpawnRequest) -> Result<ToolBindingGuard> {
        let record = self.spawn(&request).await?;
        let holder = record.session.workload.uri()?;
        let dir = self.identity_directory.join(&record.allocation.agent_id);
        let mut guard = ToolBindingGuard {
            factory: self.clone(),
            record: record.clone(),
            identity_dir: dir.clone(),
            done: false,
            binding: VmBrokerBinding {
                own_identity_dir: dir.clone(),
                broker_socket: self
                    .worker_directory
                    .join(format!(
                        "{}-{}",
                        &crate::kernel::hex(&crate::audit::hash(record.parent_holder.as_bytes()))
                            [..24],
                        request.parent.workload.incarnation
                    ))
                    .join("broker.sock"),
                server_name: format!("node-{}", request.parent.workload.node),
            },
        };
        private_dir(&dir)?;
        ensure!(
            guard.binding.broker_socket.is_file()
                || std::os::unix::fs::FileTypeExt::is_socket(
                    &std::fs::metadata(&guard.binding.broker_socket)?.file_type()
                ),
            "parent broker socket absent"
        );
        let now = crate::cluster::now_ms() / 1000;
        let ttl = (record.session.lease_expiry_ms / 1000)
            .checked_sub(now)
            .context("tool identity deadline expired")?
            .min(3600);
        ensure!(ttl > 0, "tool identity has no finite lifetime");
        let identity = self
            .authority
            .issue(record.session.workload.clone(), now, ttl)?;
        write_identity(&dir, &identity)?;
        let runtime = self.runtime.read().await.clone();
        self.kernel
            .register_workload_certificate(
                &format!("tool-certificate-{}", uuid::Uuid::new_v4()),
                &runtime,
                &record.session.grant_id,
                &identity.certificate_der,
                &identity.ca_der,
                now,
            )
            .await?
            .require()?;
        let mut sessions = self.sessions.lock().await;
        ensure!(
            !sessions.contains_key(&holder),
            "tool holder already issued; recover its scope instead of replaying"
        );
        sessions.insert(holder.clone(), record.session.clone());
        drop(sessions);
        let (stop, _) = watch::channel(false);
        self.stops.lock().await.insert(holder, stop);
        // Guard remains responsible for identity removal and authoritative retirement after every error/cancel.
        guard.done = false;
        Ok(guard)
    }
    async fn spawn(&self, request: &ToolSpawnRequest) -> Result<Record> {
        let hash = content_hash(request)?;
        let key = format!("tools/bindings/{}", request.invocation_intent);
        for _ in 0..16 {
            let view = self.kernel.cluster.view().await?;
            let original = view
                .kv
                .get("kernel/state")
                .context("deployment not initialized")?
                .clone();
            let mut state: KernelState = serde_json::from_value(original.clone())?;
            let runtime = self.runtime.read().await.clone();
            let now = crate::cluster::now_ms();
            trusted(&state, &runtime, now)?;
            ensure!(
                WorkloadId::parse(runtime.id())?.node == request.parent.workload.node,
                "tool identity owner cannot reserve another node's capacity"
            );
            if let Some(value) = view.kv.get(&key) {
                let record: Record = serde_json::from_value(value.clone())?;
                ensure!(
                    record.request_hash == hash
                        && record.status == "active"
                        && record.session.lease_expiry_ms > now,
                    "tool scope identity conflict or already retired"
                );
                anyhow::bail!(
                    "tool scope already created; inspect its recorded status instead of issuing again"
                );
            }
            let (record, mut ops, leases) = stage(&mut state, &view, request, hash, now)?;
            checkpoint(
                &mut state,
                &self.kernel,
                &format!("tool-spawn/{}", request.invocation_intent),
                runtime.id(),
                now,
            )?;
            ops.push(Mutation::Put {
                key: "kernel/state".into(),
                value: serde_json::to_value(&state)?,
            });
            ops.push(Mutation::Put {
                key: key.clone(),
                value: serde_json::to_value(&record)?,
            });
            let mut tx = Transaction::new(uuid::Uuid::new_v4().to_string(), ops);
            tx.expected_revision = Some(view.revision);
            tx.compares.push(Compare {
                key: "kernel/state".into(),
                expected: Some(original),
            });
            tx.compares.push(Compare {
                key: key.clone(),
                expected: None,
            });
            tx.leases = leases;
            let result = self.kernel.cluster.transact(tx).await?;
            if result.applied {
                return Ok(record);
            }
            ensure!(
                result.error.as_deref() == Some("compare-and-swap conflict"),
                "tool admission transaction refused: {:?}",
                result.error
            );
        }
        anyhow::bail!("tool admission retry bound reached")
    }
    async fn retire(&self, record: &Record, identity_dir: &Path) -> Result<()> {
        let holder = record.session.workload.uri()?;
        if let Some(stop) = self.stops.lock().await.remove(&holder) {
            let _ = stop.send(true);
        }
        self.sessions.lock().await.remove(&holder);
        let path = identity_dir.join("identity.json");
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        if record.directory.join("dispatch.json").exists() {
            if record.session.lease_expiry_ms <= crate::cluster::now_ms() {
                crate::resource_guard::stop_owned(&record.unit).await?;
            } else {
                crate::resource_guard::verify_cleanup(
                    &record.directory.join("cleanup.json"),
                    &record.unit,
                )
                .await?;
            }
        }
        let key = format!("tools/bindings/{}", record.invocation_intent);
        for _ in 0..16 {
            let view = self.kernel.cluster.view_control().await?;
            let value = view.kv.get(&key).context("tool scope record missing")?;
            let mut saved: Record = serde_json::from_value(value.clone())?;
            ensure!(
                saved.request_hash == record.request_hash,
                "tool scope retirement identity mismatch"
            );
            if saved.status == "retired" {
                return Ok(());
            }
            let original = view
                .kv
                .get("kernel/state")
                .context("deployment not initialized")?
                .clone();
            let mut state: KernelState = serde_json::from_value(original.clone())?;
            let now = crate::cluster::now_ms();
            let runtime = self.runtime.read().await.clone();
            trusted(&state, &runtime, now)?;
            let mut pending = state
                .security
                .observed
                .get(&holder)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();
            let mut inherited = BTreeSet::new();
            while let Some(id) = pending.pop() {
                if !inherited.insert(id.clone()) {
                    continue;
                }
                if let Some(input) = state.security.inputs.get(&id) {
                    ensure!(
                        input.goal_id == record.session.workload.goal,
                        "cross-Goal tool output provenance"
                    );
                    pending.extend(input.parents.iter().cloned());
                } else if let Some(finding) = state.security.findings.get(&id) {
                    ensure!(
                        finding.payload.goal_id == record.session.workload.goal,
                        "cross-Goal tool Finding provenance"
                    );
                    pending.extend(finding.payload.inputs.iter().cloned());
                } else {
                    anyhow::bail!("tool output lineage source missing");
                }
            }
            state
                .security
                .observed
                .entry(record.parent_holder.clone())
                .or_default()
                .extend(inherited);
            let proof = content_hash(&("tool-scope-retired", record))?;
            let intents = state
                .ledger
                .intents
                .iter()
                .filter(|(_, intent)| {
                    intent.request.envelope_id == record.allocation.budget_envelope_ref
                })
                .map(|(id, intent)| (id.clone(), intent.state.clone()))
                .collect::<Vec<_>>();
            for (id, status) in intents {
                match status {
                    IntentState::Reserved => {
                        state.ledger.release_proven_unbilled(&id, proof)?;
                    }
                    IntentState::Dispatched => {
                        state.ledger.mark_unknown(&id)?;
                        if let Some(effect) = state.effects.get_mut(&id) {
                            effect.state = "unknown".into();
                        }
                    }
                    _ => {}
                }
            }
            let envelope = state
                .ledger
                .envelopes
                .get(&record.allocation.budget_envelope_ref)
                .context("tool envelope missing")?
                .clone();
            let unused = envelope
                .allocation
                .saturating_sub(envelope.spent.saturating_add(envelope.held));
            state.ledger.release_allocation(&envelope.id)?;
            if state.security.grants.contains_key(&record.session.grant_id) {
                state.security.revoke_grant(&record.session.grant_id)?;
            }
            let task = state
                .domain
                .tasks
                .get_mut(&format!(
                    "{}/{}",
                    record.session.workload.goal, record.session.task_id
                ))
                .context("tool Task absent")?;
            task.lease_expiry_ms = 0;
            let actor = Actor {
                id: runtime.id().into(),
                role: Role::Operator,
            };
            if !matches!(task.state, TaskState::Completed | TaskState::Failed) {
                state.domain.execute(
                    &actor,
                    DomainCommand::ExpireTask {
                        goal_id: record.session.workload.goal.clone(),
                        task_id: record.session.task_id.clone(),
                    },
                    now,
                )?;
            }
            if unused > 0 {
                state.domain.execute(
                    &actor,
                    DomainCommand::ReturnTaskAllocation {
                        goal_id: record.session.workload.goal.clone(),
                        task_id: record.session.task_id.clone(),
                        intent_id: format!("return-{}", record.allocation.agent_id),
                        unused_microusd: unused,
                    },
                    now,
                )?;
            }
            let allocation_key = format!("allocations/{}", record.allocation.agent_id);
            let mut ops = vec![];
            if let Some(current) = view.kv.get(&allocation_key) {
                ensure!(
                    *current == serde_json::to_value(&record.allocation)?,
                    "tool allocation replaced before retirement"
                );
                let node_key = format!("nodes/{}", record.allocation.node_id);
                let mut node: crate::scheduler::Node = serde_json::from_value(
                    view.kv.get(&node_key).context("tool node missing")?.clone(),
                )?;
                node.used = node
                    .used
                    .subtract(record.allocation.resources)
                    .context("tool capacity retirement underflow")?;
                node.provider_rate_remaining = node
                    .provider_rate_remaining
                    .checked_add(record.allocation.resources.rate_tokens)
                    .context("tool node rate overflow")?;
                ops.push(Mutation::Put {
                    key: node_key,
                    value: serde_json::to_value(node)?,
                });
                ops.push(Mutation::Delete {
                    key: allocation_key,
                });
            }
            ops.push(Mutation::RevokeLease {
                id: format!("allocation/{}", record.allocation.agent_id),
            });
            saved.status = "retired".into();
            checkpoint(
                &mut state,
                &self.kernel,
                &format!("tool-retire/{}", record.invocation_intent),
                runtime.id(),
                now,
            )?;
            ops.push(Mutation::Put {
                key: "kernel/state".into(),
                value: serde_json::to_value(state)?,
            });
            ops.push(Mutation::Put {
                key: key.clone(),
                value: serde_json::to_value(saved)?,
            });
            let mut tx = Transaction::new(uuid::Uuid::new_v4().to_string(), ops);
            tx.expected_revision = Some(view.revision);
            tx.compares.push(Compare {
                key: "kernel/state".into(),
                expected: Some(original),
            });
            tx.compares.push(Compare {
                key: key.clone(),
                expected: Some(value.clone()),
            });
            let result = self.kernel.cluster.transact_control(tx).await?;
            if result.applied {
                return Ok(());
            }
            ensure!(
                result.error.as_deref() == Some("compare-and-swap conflict"),
                "tool retirement refused: {:?}",
                result.error
            );
        }
        anyhow::bail!("tool retirement retry bound reached")
    }
    /// Call at runtime startup and during reconciliation; owned expired keys/scopes are removed.
    pub async fn recover(&self) -> Result<()> {
        crate::resource_guard::recover(&self.kernel, &self.runtime).await?;
        let view = self.kernel.cluster.view_control().await?;
        for (key, value) in &view.kv {
            if key.starts_with("tools/bindings/") {
                let record: Record = serde_json::from_value(value.clone())?;
                if record.session.workload.node
                    != WorkloadId::parse(self.runtime.read().await.id())?.node
                {
                    continue;
                }
                let live = view
                    .leases
                    .get(&format!("allocation/{}", record.allocation.agent_id))
                    .is_some_and(|lease| {
                        !lease.revoked && lease.expires_ms > crate::cluster::now_ms()
                    });
                if record.status == "retired"
                    || record.session.lease_expiry_ms <= crate::cluster::now_ms()
                    || !live
                {
                    self.retire(
                        &record,
                        &self.identity_directory.join(&record.allocation.agent_id),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }
}
impl ToolIdentityFactory for ScopedToolIdentityFactory {
    fn reserve(
        &self,
        request: crate::resource_guard::ResourceRequest,
    ) -> crate::resource_guard::ResourceFuture {
        let kernel = self.kernel.clone();
        let runtime = self.runtime.clone();
        Box::pin(async move {
            crate::resource_guard::ResourceGuard::acquire(kernel, runtime, request).await
        })
    }

    fn issue(&self, request: ToolSpawnRequest) -> ToolBindingFuture {
        let owner = self.me.upgrade();
        Box::pin(async move {
            owner
                .context("tool factory stopped")?
                .issue_owned(request)
                .await
        })
    }
}
pub struct ToolBindingGuard {
    factory: Arc<ScopedToolIdentityFactory>,
    record: Record,
    identity_dir: PathBuf,
    binding: VmBrokerBinding,
    done: bool,
}
impl ToolBindingGuard {
    pub fn binding(&self) -> VmBrokerBinding {
        self.binding.clone()
    }
    pub async fn finish(mut self) -> Result<()> {
        self.factory
            .retire(&self.record, &self.identity_dir)
            .await?;
        self.done = true;
        Ok(())
    }
}
impl Drop for ToolBindingGuard {
    fn drop(&mut self) {
        if !self.done
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let factory = self.factory.clone();
            let record = self.record.clone();
            let dir = self.identity_dir.clone();
            handle.spawn(async move {
                let _ = factory.retire(&record, &dir).await;
            });
        }
    }
}
fn trusted(state: &KernelState, runtime: &AuthenticatedSubject, now: u64) -> Result<()> {
    runtime.ensure_current(now / 1000)?;
    ensure!(
        state
            .security
            .policy
            .payload
            .trusted_components
            .contains(runtime.id()),
        "tool factory is not an admitted runtime"
    );
    Ok(())
}
fn checkpoint(
    state: &mut KernelState,
    kernel: &Kernel,
    id: &str,
    actor: &str,
    now: u64,
) -> Result<()> {
    state.validate()?;
    let timestamp = (now / 1000).max(
        state
            .audit
            .events()
            .last()
            .map_or(0, |event| event.timestamp),
    );
    state.audit.append_committed(AuditEvent {
        id: id.into(),
        kind: "scoped_tool_lifecycle".into(),
        actor: actor.into(),
        goal: None,
        timestamp,
        decision: "committed".into(),
        payload_hash: state.content_hash()?,
    })?;
    state.checkpoint(id, actor, timestamp)?;
    crate::kernel::ensure_state_capacity(
        state,
        serde_json::to_vec(state)?.len(),
        kernel.max_state_bytes,
        !id.starts_with("tool-spawn/"),
    )?;
    Ok(())
}
fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    ensure!(
        !std::fs::symlink_metadata(path)?.file_type().is_symlink(),
        "tool identity directory symlink denied"
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn write_identity(path: &Path, identity: &crate::identity::IssuedIdentity) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    #[derive(Serialize)]
    struct IdentityRecord<'a> {
        identity: &'a WorkloadId,
        certificate_pem: &'a str,
        private_key_pem: &'a str,
        ca_pem: &'a str,
        server_name: &'a str,
    }
    let bytes = zeroize::Zeroizing::new(serde_json::to_vec(&IdentityRecord {
        identity: &identity.identity,
        certificate_pem: &identity.certificate_pem,
        private_key_pem: &identity.private_key_pem,
        ca_pem: &identity.ca_pem,
        server_name: &identity.server_name,
    })?);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path.join("identity.json"))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}
fn stage(
    state: &mut KernelState,
    view: &crate::cluster::StateView,
    request: &ToolSpawnRequest,
    request_hash: crate::audit::Hash,
    now: u64,
) -> Result<(Record, Vec<Mutation>, Vec<LeaseCheck>)> {
    ensure!(
        (1..=8).contains(&request.calls.len())
            && request.budget_microusd > 0
            && request.deadline_ms > now + 1000,
        "tool scope lacks bounded calls/budget/lifetime"
    );
    let parent = &request.parent;
    let holder = parent.workload.uri()?;
    let effect = state
        .effects
        .get(&request.invocation_intent)
        .context("native invocation effect absent")?;
    effect.permit.verify(
        "dispatch-permit",
        &state.security.policy.payload.permit_signer,
    )?;
    ensure!(
        effect.request.action.arguments == effect.payload
            && effect.permit.payload.action_hash != [0; 32],
        "native invocation frozen proof mismatch"
    );
    ensure!(
        effect.state == "dispatched"
            && effect.permit.payload.holder == holder
            && effect.request.grant_id == parent.grant_id
            && effect.request.action.operation == "native_execute"
            && matches!(
                effect.request.action.isolation,
                crate::security::IsolationTier::V | crate::security::IsolationTier::G
            ),
        "tool scope requires a dispatched admitted native invocation"
    );
    ensure!(
        effect.payload["execution_manifest"]["broker_calls"]
            == serde_json::to_value(&request.calls)?
            && effect.payload["execution_manifest"]["broker_budget_microusd"].as_u64()
                == Some(request.budget_microusd)
            && effect.payload["deadline_ms"]
                .as_u64()
                .is_some_and(|deadline| request.deadline_ms <= deadline),
        "tool scope differs from frozen native manifest"
    );
    let task = state
        .domain
        .task(&parent.workload.goal, &parent.task_id)?
        .clone();
    ensure!(
        task.claimed_by.as_deref() == Some(&holder)
            && task.incarnation == parent.workload.incarnation.parse::<u64>()?
            && task.fencing_token == parent.task_fence
            && task.lease_expiry_ms > now
            && matches!(task.state, TaskState::Claimed | TaskState::Running),
        "native parent Task is expired or fenced"
    );
    ensure!(
        effect.request.envelope_id.as_deref() == Some(parent.envelope_id.as_str())
            && task.budget_envelope_ref == parent.envelope_id,
        "native effect parent envelope mismatch"
    );
    let context = effect
        .task_context
        .as_ref()
        .context("native effect Task context missing")?;
    ensure!(
        context.id == parent.task_id
            && context.incarnation == task.incarnation
            && context.fence == task.fencing_token,
        "native effect parent context mismatch"
    );
    let grant = state
        .security
        .grants
        .get(&parent.grant_id)
        .context("native parent grant missing")?
        .clone();
    ensure!(
        !grant.revoked
            && grant.holder == parent.workload
            && grant.lease.fencing_token == parent.allocation_fence
            && grant.lease.expiry * 1000 > now
            && grant.version == task.goal_version,
        "native parent grant expired or fenced"
    );
    let lease = view
        .leases
        .get(&format!("allocation/{}", parent.workload.agent))
        .context("parent allocation lease missing")?;
    ensure!(
        !lease.revoked
            && lease.holder == parent.workload.node
            && lease.fence == parent.allocation_fence
            && lease.expires_ms > now,
        "parent allocation is no longer current"
    );
    let deadline = request
        .deadline_ms
        .min(task.spec.expiry_ms)
        .min(task.lease_expiry_ms)
        .min(grant.lease.expiry * 1000)
        .min(parent.lease_expiry_ms);
    ensure!(
        deadline == request.deadline_ms,
        "tool scope widens parent lifetime"
    );
    let manifest = &effect.payload["execution_manifest"];
    let directory = PathBuf::from(
        manifest["resource_directory"]
            .as_str()
            .context("frozen native directory absent")?,
    );
    ensure!(
        directory.is_absolute()
            && directory.file_name().and_then(|name| name.to_str())
                == Some(
                    crate::kernel::hex(&crate::audit::hash(request.invocation_intent.as_bytes()))
                        .as_str()
                ),
        "tool scope directory differs from owned invocation"
    );
    let limits: crate::sandbox::ProcessLimits =
        serde_json::from_value(manifest["native_limits"].clone())?;
    limits.validate()?;
    ensure!(
        manifest["resources"]
            == serde_json::to_value(Resources {
                cpu_millicores: limits.cpu_millicores,
                memory_bytes: limits.memory_bytes,
                provider_slots: 0,
                rate_tokens: 0,
            })?,
        "tool resources differ from frozen native limits"
    );
    let native_spec = crate::sandbox::NativeToolSpec {
        run_id: parent.workload.goal.clone(),
        tool_id: crate::kernel::hex(&crate::audit::hash(request.invocation_intent.as_bytes())),
        incarnation: parent.workload.incarnation.parse()?,
        expires_ms: deadline,
        owned_dir: directory.clone(),
        limits: limits.clone(),
        argv: vec![],
        input: vec![],
    };
    let isolation = effect.request.action.isolation;
    let tier = if isolation == crate::security::IsolationTier::V {
        "V"
    } else {
        "G"
    };
    let unit = crate::sandbox::native_unit(&native_spec, tier)?;
    ensure!(
        manifest["resource_unit"].as_str() == Some(unit.as_str()),
        "tool scope unit differs from frozen native plan"
    );
    let guest_cid = if isolation == crate::security::IsolationTier::V {
        let cid: u32 = manifest["guest_cid"]
            .as_u64()
            .context("frozen guest CID absent")?
            .try_into()?;
        ensure!(
            manifest["native"]["assets"]["guest_cid"].as_u64() == Some(cid as u64),
            "tool scope guest CID differs from frozen native backend"
        );
        crate::resource_guard::cid_available(view, &parent.workload.node, cid)?;
        Some(cid)
    } else {
        ensure!(
            manifest["native"]["tier"] == "g"
                && manifest["native"]["allow_no_kvm_fallback"] == true
                && !crate::sandbox::kvm_present(),
            "G SDK requires exact explicit no-KVM admission"
        );
        None
    };
    let child_id = format!(
        "tool-{}",
        &crate::kernel::hex(&content_hash(&(
            holder.as_str(),
            &request.invocation_intent
        ))?)[..24]
    );
    let task_id = child_id.clone();
    let workload = WorkloadId {
        trust_domain: parent.workload.trust_domain.clone(),
        tenant: parent.workload.tenant.clone(),
        goal: parent.workload.goal.clone(),
        node: parent.workload.node.clone(),
        agent: child_id.clone(),
        incarnation: "1".into(),
    };
    let child_holder = workload.uri()?;
    let template = state
        .desired_swarms
        .get(&parent.workload.goal)
        .and_then(|(_, swarm)| {
            swarm
                .agents
                .iter()
                .find(|agent| agent.id == parent.workload.agent)
        });
    let mut caps = vec![];
    let mut names = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for (index, call) in request.calls.iter().enumerate() {
        ensure!(
            call.kind == CapabilityKind::ToolCall
                && call.name == call.operation
                && seen.insert(&call.name),
            "tool SDK declarations must be distinct exact operations"
        );
        let ancestor = grant
            .capabilities
            .iter()
            .find(|cap| {
                cap.kind == CapabilityKind::ToolCall
                    && cap.resource.matches(&call.resource)
                    && cap.constraints.operations.contains(&call.operation)
            })
            .context("target SDK operation outside parent grant")?;
        let name = if task.spec.capabilities.contains(&call.operation) {
            Some(call.operation.clone())
        } else {
            template.and_then(|template| {
                template
                    .capabilities
                    .iter()
                    .find(|cap| {
                        task.spec.capabilities.contains(&cap.id)
                            && cap.kind == call.kind
                            && cap.resource.matches(&call.resource)
                            && cap.constraints.operations.contains(&call.operation)
                    })
                    .map(|cap| cap.id.clone())
            })
        }
        .context("target SDK operation was not explicitly declared by parent Task")?;
        names.insert(name);
        let mut cap: Capability = ancestor.clone();
        cap.id = format!("{child_id}-cap-{index}-1");
        cap.holder = child_holder.clone();
        cap.parent = Some(ancestor.id.clone());
        cap.depth = ancestor
            .depth
            .checked_add(1)
            .context("tool grant depth overflow")?;
        cap.expiry = cap.expiry.min(deadline / 1000);
        cap.not_before = cap.not_before.max(now / 1000);
        cap.minimum_isolation = cap.minimum_isolation.max(isolation);
        cap.resource = crate::security::ResourceSelector::Exact(call.resource.clone());
        cap.constraints.operations = [call.operation.clone()].into();
        cap.constraints.resources = [call.resource.clone()].into();
        caps.push(cap);
    }
    let native_limits: crate::sandbox::ProcessLimits =
        serde_json::from_value(effect.payload["execution_manifest"]["native_limits"].clone())
            .context("frozen native resource limits missing")?;
    native_limits.validate()?;
    let resources = Resources {
        cpu_millicores: native_limits.cpu_millicores,
        memory_bytes: native_limits.memory_bytes,
        provider_slots: 0,
        rate_tokens: 0,
    };
    let gang = Gang {
        id: format!("native/{}", request.invocation_intent),
        goal_id: parent.workload.goal.clone(),
        strategy: Strategy::Pack,
        bundles: vec![Bundle {
            agent_id: child_id.clone(),
            placement_intent_id: child_id.clone(),
            incarnation: 1,
            resources,
            labels: BTreeMap::new(),
            required_capabilities: BTreeSet::new(),
            budget_microusd: request.budget_microusd,
        }],
        priority: 0,
        created_revision: view.revision,
    };
    let mut snapshot = crate::placement::snapshot(view)?;
    snapshot
        .nodes
        .retain(|node, _| node == &parent.workload.node);
    let plan = Scheduler::new(1)?.plan(&gang, &snapshot)?;
    let allocation = plan.allocations[0].clone();
    let mut leases = crate::scheduler::node_lease_checks(view, plan.updated_nodes.keys())?;
    leases.push(LeaseCheck {
        id: format!("allocation/{}", parent.workload.agent),
        holder: parent.workload.node.clone(),
        fence: parent.allocation_fence,
    });
    let definition = AgentDefinition {
        id: format!("{child_id}-1"),
        holder: workload.clone(),
        goal_id: parent.workload.goal.clone(),
        grant_version: grant.version,
        capabilities: caps,
        lease: Lease {
            not_before: now / 1000,
            expiry: deadline / 1000,
            fencing_token: allocation.fence,
        },
    };
    state.ledger.allocate(Envelope {
        id: allocation.budget_envelope_ref.clone(),
        placement_intent_id: allocation.placement_intent_id.clone(),
        placement_hash: content_hash(&allocation)?,
        parent: Some(parent.envelope_id.clone()),
        goal_id: parent.workload.goal.clone(),
        holder: child_holder.clone(),
        accounts: [
            "global".into(),
            format!("tenant/{}", parent.workload.tenant),
            format!("goal/{}", parent.workload.goal),
        ]
        .into(),
        allocation: request.budget_microusd,
        spent: 0,
        held: 0,
        active: true,
    })?;
    state
        .security
        .compile_definition(definition, Some(grant.id), &mut state.ledger, now / 1000)?;
    let spec = TaskSpec {
        id: task_id.clone(),
        need_id: task.spec.need_id,
        parent_task_id: Some(task.spec.id),
        input: json!({"native_invocation":request.invocation_intent,"calls":request.calls}),
        required_capabilities: BTreeSet::new(),
        capabilities: names,
        budget_microusd: request.budget_microusd,
        expiry_ms: deadline,
    };
    state.domain.execute(
        &Actor {
            id: holder.clone(),
            role: Role::Agent,
        },
        DomainCommand::DelegateTask {
            goal_id: parent.workload.goal.clone(),
            spec,
            intent_id: child_id.clone(),
            placement_intent_id: allocation.placement_intent_id.clone(),
            budget_envelope_ref: allocation.budget_envelope_ref.clone(),
        },
        now,
    )?;
    state.domain.execute(
        &Actor {
            id: child_holder.clone(),
            role: Role::Agent,
        },
        DomainCommand::ClaimTask {
            goal_id: parent.workload.goal.clone(),
            task_id: task_id.clone(),
            incarnation: 1,
            lease_expiry_ms: deadline,
            fencing_token: allocation.fence,
        },
        now,
    )?;
    let sources = state
        .security
        .observed
        .get(&holder)
        .cloned()
        .unwrap_or_default();
    state
        .security
        .observed
        .insert(child_holder, sources.clone());
    state
        .task_input_sources
        .insert(format!("{}/{}", parent.workload.goal, task_id), sources);
    let session = Session {
        workload,
        grant_id: format!("{child_id}-1"),
        envelope_id: allocation.budget_envelope_ref.clone(),
        task_id,
        allocation_fence: allocation.fence,
        task_fence: allocation.fence,
        lease_expiry_ms: deadline,
        isolation,
        provider: parent.provider.clone(),
        model: parent.model.clone(),
        history: parent.history.clone(),
        tools: request.calls.clone(),
    };
    let record = Record {
        request_hash,
        invocation_intent: request.invocation_intent.clone(),
        parent_holder: holder,
        allocation: allocation.clone(),
        session,
        status: "active".into(),
        unit,
        directory,
        guest_cid,
    };
    let mut ops = plan
        .updated_nodes
        .into_iter()
        .map(|(id, node)| {
            Ok(Mutation::Put {
                key: format!("nodes/{id}"),
                value: serde_json::to_value(node)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    ops.push(Mutation::Put {
        key: format!("allocations/{child_id}"),
        value: serde_json::to_value(allocation)?,
    });
    ops.push(Mutation::GrantLease {
        id: format!("allocation/{child_id}"),
        holder: parent.workload.node.clone(),
        expires_ms: deadline,
    });
    Ok((record, ops, leases))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        kernel::{Effect, TaskContext},
        ledger::{PriceTable, Rate},
        placement::{DesiredAgent, SwarmSpec},
        scheduler::Node,
        security::{self, Action, ActionRequest, IsolationTier, OperationClass, Scope},
    };
    fn setup_tier(
        fence: u64,
        isolation: IsolationTier,
    ) -> (
        KernelState,
        crate::cluster::StateView,
        ToolSpawnRequest,
        security::test_support::Fixture,
    ) {
        let mut f = security::test_support::fixture();
        for cap in &mut f.posture.capability_ceiling {
            cap.constraints.operations.insert("native_execute".into());
        }
        f.definition.capabilities = f
            .posture
            .capability_ceiling
            .iter()
            .enumerate()
            .map(|(index, cap)| {
                let mut cap = cap.clone();
                cap.id = format!("agent-cap-{index}");
                cap.holder = f.subject.id().into();
                cap
            })
            .collect();
        f.definition.lease.fencing_token = fence;
        let contract = security::test_support::goal_spec_posture(&f.posture);
        security::bind_goal_contract(&mut f.posture, &contract).unwrap();
        let initial =
            security::SecurityState::new(f.policy.clone(), &f.signer.public_key(), f.now).unwrap();
        f.approvals = vec![
            initial
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
        let operator = AuthenticatedSubject::operator(&f.operators[0]);
        let mut state = KernelState::new(
            f.policy.clone(),
            &f.signer.public_key(),
            f.now,
            "tool-test".into(),
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
                &Actor {
                    id: operator.id().into(),
                    role: Role::Operator,
                },
                DomainCommand::RegisterGoal {
                    spec: security::test_support::goal_spec(&f),
                },
                f.now * 1000,
            )
            .unwrap();
        state
            .ledger
            .allocate(Envelope {
                id: "parent-envelope".into(),
                placement_intent_id: "parent-placement".into(),
                placement_hash: [2; 32],
                parent: None,
                goal_id: "g".into(),
                holder: f.subject.id().into(),
                accounts: ["global".into(), "tenant/t".into(), "goal/g".into()].into(),
                allocation: 900000,
                spent: 0,
                held: 0,
                active: true,
            })
            .unwrap();
        state
            .security
            .compile_definition(f.definition.clone(), None, &mut state.ledger, f.now)
            .unwrap();
        let task = TaskSpec {
            id: "parent".into(),
            need_id: "answer".into(),
            parent_task_id: None,
            input: json!({}),
            required_capabilities: BTreeSet::new(),
            capabilities: ["ceiling-0".into(), "ceiling-6".into()].into(),
            budget_microusd: 900000,
            expiry_ms: (f.now + 500) * 1000,
        };
        state
            .domain
            .execute(
                &Actor {
                    id: operator.id().into(),
                    role: Role::Operator,
                },
                DomainCommand::DelegateTask {
                    goal_id: "g".into(),
                    spec: task.clone(),
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
                    fencing_token: fence,
                },
                f.now * 1000,
            )
            .unwrap();
        let template = DesiredAgent {
            id: "a1".into(),
            capabilities: f.posture.capability_ceiling.clone(),
            task,
            resources: Resources {
                cpu_millicores: 100,
                memory_bytes: 64 * 1024 * 1024,
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
        };
        state.desired_swarms.insert(
            "g".into(),
            (
                operator.id().into(),
                SwarmSpec {
                    id: "swarm".into(),
                    tenant: "t".into(),
                    goal_id: "g".into(),
                    trust_domain: "test.local".into(),
                    provider: "local".into(),
                    model: "m1".into(),
                    strategy: Strategy::Pack,
                    lease_ms: 60000,
                    agents: vec![template.clone()],
                    expiry_ms: (f.now + 500) * 1000,
                },
            ),
        );
        let call = ToolDefinition {
            name: "read".into(),
            description: "owned fixture".into(),
            parameters: json!({"type":"object"}),
            operation: "read".into(),
            kind: CapabilityKind::ToolCall,
            resource: "tool/data/a".into(),
            class: OperationClass::ReadOnly,
        };
        let parent = Session {
            workload: f.definition.holder.clone(),
            grant_id: f.definition.id.clone(),
            envelope_id: "parent-envelope".into(),
            task_id: "parent".into(),
            allocation_fence: fence,
            task_fence: fence,
            lease_expiry_ms: (f.now + 500) * 1000,
            isolation: IsolationTier::P,
            provider: "local".into(),
            model: "m1".into(),
            history: template.history,
            tools: vec![],
        };
        let request = ToolSpawnRequest {
            invocation_intent: "native-intent".into(),
            parent,
            calls: vec![call],
            budget_microusd: 100,
            deadline_ms: (f.now + 60) * 1000,
        };
        let payload = json!({"deadline_ms":request.deadline_ms,"execution_manifest":{"broker_calls":request.calls,"broker_budget_microusd":request.budget_microusd,"native_limits":{"cpu_millicores":100,"memory_bytes":67108864,"pids":64,"max_output_bytes":65536}}});
        let directory = PathBuf::from("/tmp/tool-fixture").join(crate::kernel::hex(
            &crate::audit::hash(request.invocation_intent.as_bytes()),
        ));
        let native_spec = crate::sandbox::NativeToolSpec {
            run_id: "g".into(),
            tool_id: crate::kernel::hex(&crate::audit::hash(request.invocation_intent.as_bytes())),
            incarnation: 1,
            expires_ms: request.deadline_ms,
            owned_dir: directory.clone(),
            limits: crate::sandbox::ProcessLimits {
                cpu_millicores: 100,
                memory_bytes: 67108864,
                pids: 64,
                max_output_bytes: 65536,
            },
            argv: vec![],
            input: vec![],
        };
        let mut payload = payload;
        payload["execution_manifest"]["resources"] = json!({"cpu_millicores":100,"memory_bytes":67108864,"provider_slots":0,"rate_tokens":0});
        payload["execution_manifest"]["resource_directory"] =
            serde_json::to_value(directory).unwrap();
        payload["execution_manifest"]["resource_unit"] = serde_json::to_value(
            crate::sandbox::native_unit(
                &native_spec,
                if isolation == IsolationTier::V {
                    "V"
                } else {
                    "G"
                },
            )
            .unwrap(),
        )
        .unwrap();
        payload["execution_manifest"]["guest_cid"] = json!(93);
        payload["execution_manifest"]["native"] = if isolation == IsolationTier::V {
            json!({"assets":{"guest_cid":93}})
        } else {
            json!({"tier":"g","allow_no_kvm_fallback":true})
        };
        if isolation == IsolationTier::G {
            payload["execution_manifest"]["guest_cid"] = serde_json::Value::Null;
        }
        let action = ActionRequest {
            intent_id: request.invocation_intent.clone(),
            grant_id: f.definition.id.clone(),
            grant_version: 1,
            capability_id: "agent-cap-0".into(),
            budget_capability_id: Some("agent-cap-6".into()),
            envelope_id: Some("parent-envelope".into()),
            usage_bound: [("elapsed_ms".into(), 10)].into(),
            action: Action {
                kind: CapabilityKind::ToolCall,
                resource: "tool/data/a".into(),
                operation: "native_execute".into(),
                arguments: payload.clone(),
                class: OperationClass::ReadOnly,
                isolation,
                recipient: None,
                evidence: BTreeSet::new(),
            },
        };
        let price = f
            .signer
            .sign(
                "price-table",
                PriceTable {
                    version: "1".into(),
                    provider: "native".into(),
                    model: "compute".into(),
                    currency: "microusd".into(),
                    not_before: f.now,
                    expires: f.now + 600,
                    rates: [(
                        "elapsed_ms".into(),
                        Rate {
                            numerator: 1,
                            denominator: 1,
                        },
                    )]
                    .into(),
                },
            )
            .unwrap();
        let decision = state
            .security
            .authorize(
                &f.subject,
                action.clone(),
                &[],
                Some(&price),
                &mut state.ledger,
                &f.signer,
                f.now,
            )
            .unwrap();
        state
            .security
            .consume_permit(&f.subject, &decision.permit, &mut state.ledger, f.now)
            .unwrap();
        state.effects.insert(
            request.invocation_intent.clone(),
            Effect {
                request: action,
                payload,
                permit: decision.permit,
                state: "dispatched".into(),
                result: None,
                task_context: Some(TaskContext {
                    id: "parent".into(),
                    incarnation: 1,
                    fence,
                }),
            },
        );
        let node = crate::scheduler::Node {
            id: "n1".into(),
            capacity: Resources {
                cpu_millicores: 1000,
                memory_bytes: 1024 * 1024 * 1024,
                provider_slots: 10,
                rate_tokens: 100,
            },
            used: Default::default(),
            labels: BTreeMap::new(),
            capabilities: BTreeSet::new(),
            healthy: true,
            provider_rate_remaining: 100,
        };
        let view = crate::cluster::StateView {
            revision: 7,
            kv: [("nodes/n1".into(), serde_json::to_value(node).unwrap())].into(),
            leases: [
                (
                    "allocation/a1".into(),
                    crate::cluster::Lease {
                        holder: "n1".into(),
                        fence,
                        expires_ms: (f.now + 500) * 1000,
                        revoked: false,
                    },
                ),
                (
                    "node/n1".into(),
                    crate::cluster::Lease {
                        holder: "n1".into(),
                        fence: 2,
                        expires_ms: (f.now + 500) * 1000,
                        revoked: false,
                    },
                ),
            ]
            .into(),
        };
        (state, view, request, f)
    }
    #[test]
    fn tool_scope_requires_exact_native_manifest_and_explicit_parent_authority() {
        let (original, view, request, f) = setup_tier(3, IsolationTier::V);
        let mut state = original.clone();
        let (record, ops, leases) = stage(
            &mut state,
            &view,
            &request,
            content_hash(&request).unwrap(),
            f.now * 1000,
        )
        .unwrap();
        assert_eq!(state.desired_swarms["g"].1.agents.len(), 1);
        assert_eq!(record.session.isolation, IsolationTier::V);
        assert_eq!(record.session.tools.len(), 1);
        assert_eq!(leases.len(), 2);
        assert!(
            ops.iter()
                .any(|op| matches!(op, Mutation::GrantLease { .. }))
        );
        let grant = &state.security.grants[&record.session.grant_id];
        assert_eq!(grant.parent.as_deref(), Some(f.definition.id.as_str()));
        assert_eq!(grant.capabilities.len(), 1);
        assert_eq!(
            grant.capabilities[0].constraints.operations,
            ["read".into()].into()
        );
        assert_eq!(grant.capabilities[0].minimum_isolation, IsolationTier::V);
        assert!(
            !grant
                .capabilities
                .iter()
                .any(|cap| cap.kind == CapabilityKind::BudgetSpend)
        );
        assert_eq!(
            state
                .domain
                .task("g", &record.session.task_id)
                .unwrap()
                .state,
            TaskState::Claimed
        );
        assert_eq!(
            state
                .domain
                .task("g", "parent")
                .unwrap()
                .allocated_to_children_microusd,
            100
        );
        let mut changed = request.clone();
        changed.calls[0].operation = "append".into();
        assert!(
            stage(
                &mut original.clone(),
                &view,
                &changed,
                content_hash(&changed).unwrap(),
                f.now * 1000
            )
            .is_err()
        );
        let mut missing = original.clone();
        missing
            .domain
            .tasks
            .get_mut("g/parent")
            .unwrap()
            .spec
            .capabilities
            .clear();
        assert!(
            stage(
                &mut missing,
                &view,
                &request,
                content_hash(&request).unwrap(),
                f.now * 1000
            )
            .is_err()
        );
        let mut fenced = view.clone();
        fenced.leases.get_mut("allocation/a1").unwrap().revoked = true;
        assert!(
            stage(
                &mut original.clone(),
                &fenced,
                &request,
                content_hash(&request).unwrap(),
                f.now * 1000
            )
            .is_err()
        );
    }
    #[test]
    fn g_sdk_scope_is_explicit_no_kvm_and_carries_no_guest_cid() {
        let (mut state, view, request, f) = setup_tier(3, IsolationTier::G);
        let staged = stage(
            &mut state,
            &view,
            &request,
            content_hash(&request).unwrap(),
            f.now * 1000,
        );
        if crate::sandbox::kvm_present() {
            assert!(staged.is_err());
            return;
        }
        let (record, _, _) = staged.unwrap();
        assert_eq!(record.session.isolation, IsolationTier::G);
        assert!(record.guest_cid.is_none());
        assert!(
            state.security.grants[&record.session.grant_id]
                .capabilities
                .iter()
                .all(|cap| cap.minimum_isolation == IsolationTier::G)
        );
        let (mut denied, view, request, f) = setup_tier(3, IsolationTier::G);
        denied
            .effects
            .get_mut(&request.invocation_intent)
            .unwrap()
            .payload["execution_manifest"]["native"]["allow_no_kvm_fallback"] = json!(false);
        assert!(
            stage(
                &mut denied,
                &view,
                &request,
                content_hash(&request).unwrap(),
                f.now * 1000
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn issued_scope_carves_once_keeps_key_host_only_and_retires_without_replay() {
        use crate::cluster::{Cluster, ClusterConfig, NodeAddress};
        let fixture = security::test_support::fixture();
        let root =
            PathBuf::from("/tmp").join(format!("swti-{}", &uuid::Uuid::new_v4().to_string()[..8]));
        std::fs::create_dir(&root).unwrap();
        let address = "127.0.0.1:0".parse().unwrap();
        let cluster = Cluster::open(ClusterConfig {
            node_id: 1,
            bind_addr: address,
            nodes: [(
                1,
                NodeAddress {
                    addr: address,
                    server_name: fixture.identity.server_name.clone(),
                    certificate_sha256: crate::kernel::hex(&crate::audit::hash(
                        &fixture.identity.certificate_der,
                    )),
                },
            )]
            .into(),
            certificate_der: fixture.identity.certificate_der.clone(),
            private_key_der: fixture.identity.private_key_der.clone(),
            ca_der: fixture.identity.ca_der.clone(),
            directory: root.join("state"),
            storage_key: [27; 32],
            rotation_trust_key: None,
            max_frame_bytes: 8 * 1024 * 1024,
            max_queue_items: 32,
            history_limit: 128,
        })
        .await
        .unwrap();
        cluster.initialize([1].into()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let leases = cluster
            .transact(Transaction::new(
                "fixture-membership",
                vec![
                    Mutation::GrantLease {
                        id: "allocation/a1".into(),
                        holder: "n1".into(),
                        expires_ms: (fixture.now + 500) * 1000,
                    },
                    Mutation::GrantLease {
                        id: "node/n1".into(),
                        holder: "n1".into(),
                        expires_ms: (fixture.now + 500) * 1000,
                    },
                ],
            ))
            .await
            .unwrap();
        let (mut state, view, request, f) = setup_tier(leases.revision, IsolationTier::V);
        state
            .security
            .policy
            .payload
            .trusted_components
            .insert(f.subject.id().into());
        state.security.policy = f
            .signer
            .sign("deployment-policy", state.security.policy.payload)
            .unwrap();
        let kernel = Arc::new(Kernel {
            cluster: cluster.clone(),
            signer: Arc::new(crate::audit::CryptoSigner::from_seed([44; 32])),
            max_state_bytes: 8 * 1024 * 1024,
            response_store: None,
        });
        kernel.bootstrap(state).await.unwrap();
        cluster
            .transact(Transaction::new(
                "fixture-node",
                vec![Mutation::Put {
                    key: "nodes/n1".into(),
                    value: view.kv["nodes/n1"].clone(),
                }],
            ))
            .await
            .unwrap();
        let workers = root.join("workers");
        let parent_dir = workers.join(format!(
            "{}-1",
            &crate::kernel::hex(&crate::audit::hash(f.subject.id().as_bytes()))[..24]
        ));
        std::fs::create_dir_all(&parent_dir).unwrap();
        let socket = tokio::net::UnixListener::bind(parent_dir.join("broker.sock")).unwrap();
        let sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let stops = Arc::new(Mutex::new(BTreeMap::new()));
        let factory = ScopedToolIdentityFactory::new(
            kernel.clone(),
            Arc::new(IdentityAuthority::generate("test.local").unwrap()),
            Arc::new(RwLock::new(f.subject.clone())),
            sessions.clone(),
            stops,
            workers,
            root.join("identities"),
        )
        .unwrap();
        let resource_request = crate::resource_guard::ResourceRequest {
            parent: request.parent.clone(),
            invocation_intent: request.invocation_intent.clone(),
            resources: Resources {
                cpu_millicores: 100,
                memory_bytes: 67108864,
                provider_slots: 0,
                rate_tokens: 0,
            },
            deadline_ms: request.deadline_ms,
        };
        let mut full_node: Node = serde_json::from_value(view.kv["nodes/n1"].clone()).unwrap();
        full_node.used.cpu_millicores = full_node.capacity.cpu_millicores;
        cluster
            .transact(Transaction::new(
                "capacity-full",
                vec![Mutation::Put {
                    key: "nodes/n1".into(),
                    value: serde_json::to_value(full_node).unwrap(),
                }],
            ))
            .await
            .unwrap();
        assert!(factory.reserve(resource_request.clone()).await.is_err());
        assert!(
            cluster
                .read("resources/tool/native-intent")
                .await
                .unwrap()
                .value
                .is_none()
        );
        assert!(
            !Path::new(request_directory(&kernel, &request).await.as_str())
                .join("dispatch.json")
                .exists()
        );
        cluster
            .transact(Transaction::new(
                "capacity-restored",
                vec![Mutation::Put {
                    key: "nodes/n1".into(),
                    value: view.kv["nodes/n1"].clone(),
                }],
            ))
            .await
            .unwrap();
        let (first, second) = tokio::join!(
            factory.reserve(resource_request.clone()),
            factory.reserve(resource_request)
        );
        assert_eq!(
            usize::from(first.is_ok()) + usize::from(second.is_ok()),
            1,
            "CAS admits one resource/CID hold"
        );
        let hold = first.or(second).unwrap();
        assert!(
            factory.issue(request.clone()).await.is_err(),
            "SDK cannot reserve the already-held VM CID a second time"
        );
        let node: Node =
            serde_json::from_value(cluster.read("nodes/n1").await.unwrap().value.unwrap()).unwrap();
        assert_eq!(node.used.cpu_millicores, 100);
        let directory = request_directory(&kernel, &request).await;
        hold.release(&PathBuf::from(directory).join("cleanup.json"))
            .await
            .unwrap();
        let guard = factory.issue(request.clone()).await.unwrap();
        let identity = guard.binding().own_identity_dir.join("identity.json");
        assert!(identity.exists());
        assert!(factory.issue(request.clone()).await.is_err());
        assert!(identity.exists());
        assert_eq!(sessions.lock().await.len(), 1);
        let state = kernel.state().await.unwrap();
        assert_eq!(
            state
                .domain
                .task("g", "parent")
                .unwrap()
                .allocated_to_children_microusd,
            request.budget_microusd
        );
        assert_eq!(state.desired_swarms["g"].1.agents.len(), 1);
        let scope_state = kernel.state().await.unwrap();
        let leaf = scope_state
            .workload_certificates
            .values()
            .find(|leaf| {
                crate::identity::verify_workload_certificate(
                    leaf,
                    factory.authority.ca_der(),
                    &guard.record.session.workload,
                    crate::cluster::now_ms() / 1000,
                )
                .is_ok()
            })
            .unwrap();
        let tool_subject = AuthenticatedSubject::workload(
            leaf,
            factory.authority.ca_der(),
            guard.record.session.workload.clone(),
            crate::cluster::now_ms() / 1000,
        )
        .unwrap();
        kernel
            .observe(
                "sdk-read-observation",
                &tool_subject,
                "sdk-private-result".into(),
                "target_result".into(),
                "g".into(),
                json!({"body":"classified fixture output"}),
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        guard.finish().await.unwrap();
        assert!(!identity.exists());
        assert!(sessions.lock().await.is_empty());
        factory.recover().await.unwrap();
        assert!(factory.issue(request.clone()).await.is_err());
        let state = kernel.state().await.unwrap();
        assert_eq!(
            state
                .domain
                .task("g", "parent")
                .unwrap()
                .allocated_to_children_microusd,
            0
        );
        assert_eq!(state.ledger.accounts["global"].total.held, 10);
        assert!(state.security.observed[f.subject.id()].contains("sdk-private-result"));
        assert_eq!(state.security.inputs["sdk-private-result"].goal_id, "g");
        let view = cluster.view().await.unwrap();
        assert!(
            !view
                .kv
                .keys()
                .any(|key| key.starts_with("allocations/tool-"))
        );
        assert!(
            view.leases
                .iter()
                .filter(|(key, _)| key.starts_with("allocation/tool-"))
                .all(|(_, lease)| lease.revoked)
        );
        drop(socket);
        cluster.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    async fn request_directory(kernel: &Kernel, request: &ToolSpawnRequest) -> String {
        kernel.state().await.unwrap().effects[&request.invocation_intent].payload["execution_manifest"]["resource_directory"].as_str().unwrap().into()
    }
}
