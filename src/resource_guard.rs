//! Aggregate node capacity is reserved before a tool subprocess starts and released after host proof.
use crate::{
    audit::{AuditEvent, content_hash},
    cluster::{Compare, LeaseCheck, Mutation, Transaction},
    kernel::{Kernel, KernelState},
    node::Session,
    scheduler::{Node, Resources},
    security::AuthenticatedSubject,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{process::Command, sync::RwLock};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRequest {
    pub parent: Session,
    pub invocation_intent: String,
    pub resources: Resources,
    pub deadline_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    request_hash: crate::audit::Hash,
    intent: String,
    node_id: String,
    resources: Resources,
    deadline_ms: u64,
    unit: String,
    directory: PathBuf,
    guest_cid: Option<u32>,
    status: String,
}
pub type ResourceFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<ResourceGuard>> + Send>>;
pub struct ResourceGuard {
    kernel: Arc<Kernel>,
    executor: Arc<RwLock<AuthenticatedSubject>>,
    record: Record,
    done: bool,
}
impl ResourceGuard {
    pub async fn acquire(
        kernel: Arc<Kernel>,
        executor: Arc<RwLock<AuthenticatedSubject>>,
        request: ResourceRequest,
    ) -> Result<Self> {
        let request_hash = content_hash(&request)?;
        let key = format!("resources/tool/{}", request.invocation_intent);
        let write = kernel.cluster.kernel_write(false).await;
        for _ in 0..16 {
            let view = kernel.cluster.view().await?;
            let original = view
                .kv
                .get("kernel/state")
                .context("deployment not initialized")?
                .clone();
            let mut state: KernelState = serde_json::from_value(original.clone())?;
            let actor = executor.read().await.clone();
            trusted(&state, &actor)?;
            ensure!(
                crate::identity::WorkloadId::parse(actor.id())?.node
                    == request.parent.workload.node,
                "tool resource owner cannot reserve another node's capacity"
            );
            ensure!(
                !view.kv.contains_key(&key),
                "tool resource hold already exists; inspect it instead of replaying"
            );
            let (record, mut node, leases) = validate(&state, &view, &request, request_hash)?;
            node.used = node
                .used
                .checked_add(request.resources)
                .context("tool resource overflow")?;
            ensure!(
                node.used.fits(node.capacity),
                "aggregate node tool capacity exhausted"
            );
            node.provider_rate_remaining = node
                .provider_rate_remaining
                .checked_sub(request.resources.rate_tokens)
                .context("aggregate tool rate capacity exhausted")?;
            append(
                &mut state,
                &kernel,
                &record,
                actor.id(),
                "tool_resources_reserved",
            )?;
            let mut tx = Transaction::new(
                uuid::Uuid::new_v4().to_string(),
                vec![
                    Mutation::Put {
                        key: format!("nodes/{}", record.node_id),
                        value: serde_json::to_value(node)?,
                    },
                    Mutation::Put {
                        key: key.clone(),
                        value: serde_json::to_value(&record)?,
                    },
                    Mutation::Put {
                        key: "kernel/state".into(),
                        value: serde_json::to_value(state)?,
                    },
                ],
            );
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
            let result = kernel.cluster.transact(tx).await?;
            if result.applied {
                drop(write);
                return Ok(Self {
                    kernel,
                    executor,
                    record,
                    done: false,
                });
            }
            ensure!(
                result.error.as_deref() == Some("compare-and-swap conflict"),
                "resource admission refused: {:?}",
                result.error
            );
        }
        anyhow::bail!("resource admission retry bound reached")
    }
    pub async fn release(mut self, cleanup_json: &Path) -> Result<()> {
        ensure!(
            cleanup_json == self.record.directory.join("cleanup.json"),
            "resource cleanup proof is outside frozen invocation"
        );
        if self.record.directory.join("dispatch.json").exists() {
            verify_cleanup(cleanup_json, &self.record.unit).await?;
        }

        retire(&self.kernel, &self.executor, &self.record).await?;
        self.done = true;
        Ok(())
    }
}
impl Drop for ResourceGuard {
    fn drop(&mut self) {
        if !self.done
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let kernel = self.kernel.clone();
            let executor = self.executor.clone();
            let record = self.record.clone();
            handle.spawn(async move {
                // A failed or interrupted start can race service creation. Keep aggregate capacity until host cleanup confirms the exact unit.
                if !record.directory.join("dispatch.json").exists()
                    || verify_cleanup(&record.directory.join("cleanup.json"), &record.unit)
                        .await
                        .is_ok()
                {
                    let _ = retire(&kernel, &executor, &record).await;
                }
            });
        }
    }
}
fn trusted(state: &KernelState, executor: &AuthenticatedSubject) -> Result<()> {
    executor.ensure_current(crate::cluster::now_ms() / 1000)?;
    ensure!(
        state
            .security
            .policy
            .payload
            .trusted_components
            .contains(executor.id()),
        "resource owner is not an admitted runtime"
    );
    Ok(())
}
fn append(
    state: &mut KernelState,
    kernel: &Kernel,
    record: &Record,
    actor: &str,
    kind: &str,
) -> Result<()> {
    state.validate()?;
    let timestamp = (crate::cluster::now_ms() / 1000).max(
        state
            .audit
            .events()
            .last()
            .map_or(0, |event| event.timestamp),
    );
    let id = format!("{kind}/{}", record.intent);
    state.audit.append_committed(AuditEvent {
        id: id.clone(),
        kind: kind.into(),
        actor: actor.into(),
        goal: None,
        timestamp,
        decision: "committed".into(),
        payload_hash: content_hash(record)?,
    })?;
    state.checkpoint(&id, actor, timestamp)?;
    crate::kernel::ensure_state_capacity(
        state,
        serde_json::to_vec(state)?.len(),
        kernel.max_state_bytes,
        kind != "tool_resources_reserved",
    )?;
    Ok(())
}
fn validate(
    state: &KernelState,
    view: &crate::cluster::StateView,
    request: &ResourceRequest,
    request_hash: crate::audit::Hash,
) -> Result<(Record, Node, Vec<LeaseCheck>)> {
    let now = crate::cluster::now_ms();
    let parent = &request.parent;
    let holder = parent.workload.uri()?;
    let effect = state
        .effects
        .get(&request.invocation_intent)
        .context("resource hold lacks invocation")?;
    effect.permit.verify(
        "dispatch-permit",
        &state.security.policy.payload.permit_signer,
    )?;
    ensure!(
        effect.state == "dispatched"
            && effect.permit.payload.holder == holder
            && effect.request.grant_id == parent.grant_id
            && matches!(
                effect.request.action.operation.as_str(),
                "native_execute" | "wasi_execute"
            ),
        "resource hold requires current dispatched compute invocation"
    );
    ensure!(
        effect.request.action.arguments == effect.payload,
        "resource invocation payload changed"
    );
    ensure!(
        state.domain.goal(&parent.workload.goal)?.status == "active"
            && !state.security.stopped(&crate::security::Scope::Agent {
                tenant: parent.workload.tenant.clone(),
                goal: parent.workload.goal.clone(),
                holder: holder.clone()
            }),
        "compute Goal or holder stopped"
    );
    let mut ancestor = Some(parent.grant_id.as_str());
    let mut visited = std::collections::BTreeSet::new();
    while let Some(id) = ancestor {
        let grant = state
            .security
            .grants
            .get(id)
            .context("compute grant ancestry absent")?;
        ensure!(
            visited.insert(id)
                && visited.len() <= 5
                && !grant.revoked
                && grant.lease.not_before * 1000 <= now
                && grant.lease.expiry * 1000 > now,
            "compute grant ancestry revoked or expired"
        );
        ancestor = grant.parent.as_deref();
    }
    let task = state.domain.task(&parent.workload.goal, &parent.task_id)?;
    ensure!(
        task.claimed_by.as_deref() == Some(&holder)
            && task.incarnation == parent.workload.incarnation.parse::<u64>()?
            && task.fencing_token == parent.task_fence
            && task.lease_expiry_ms > now
            && matches!(
                task.state,
                crate::domain::TaskState::Claimed | crate::domain::TaskState::Running
            ),
        "resource parent Task expired or fenced"
    );
    let context = effect
        .task_context
        .as_ref()
        .context("compute Task context absent")?;
    ensure!(
        context.id == parent.task_id
            && context.incarnation == task.incarnation
            && context.fence == task.fencing_token,
        "compute Task context mismatch"
    );
    let grant = state
        .security
        .grants
        .get(&parent.grant_id)
        .context("compute grant missing")?;
    ensure!(
        !grant.revoked
            && grant.holder == parent.workload
            && grant.version == task.goal_version
            && grant.lease.fencing_token == parent.allocation_fence
            && grant.lease.expiry * 1000 > now,
        "compute parent grant expired or fenced"
    );
    ensure!(
        request.deadline_ms > now
            && request.deadline_ms <= task.lease_expiry_ms
            && request.deadline_ms <= grant.lease.expiry * 1000
            && request.deadline_ms <= parent.lease_expiry_ms
            && effect.payload["deadline_ms"].as_u64() == Some(request.deadline_ms),
        "resource deadline widens admitted invocation"
    );
    let manifest = &effect.payload["execution_manifest"];
    let directory = PathBuf::from(
        manifest["resource_directory"]
            .as_str()
            .context("frozen tool directory missing")?,
    );
    ensure!(
        directory.is_absolute()
            && directory.file_name().and_then(|name| name.to_str())
                == Some(
                    crate::kernel::hex(&crate::audit::hash(request.invocation_intent.as_bytes()))
                        .as_str()
                ),
        "resource directory is not the exact owned intent directory"
    );
    let (expected, unit, cid) = if effect.request.action.operation == "wasi_execute" {
        ensure!(
            effect.request.action.isolation == crate::security::IsolationTier::W,
            "WASI execution tier mismatch"
        );
        let limits: crate::sandbox::ToolLimits =
            serde_json::from_value(manifest["wasi_limits"].clone())?;
        limits.validate()?;
        (
            Resources {
                cpu_millicores: 1000,
                memory_bytes: (limits.memory_bytes as u64)
                    .checked_add(512 * 1024 * 1024)
                    .context("compiler memory bound overflow")?,
                provider_slots: 0,
                rate_tokens: 0,
            },
            crate::wasi_worker::unit_for_directory(&directory)?,
            None,
        )
    } else {
        let limits: crate::sandbox::ProcessLimits =
            serde_json::from_value(manifest["native_limits"].clone())?;
        limits.validate()?;
        let tier = match effect.request.action.isolation {
            crate::security::IsolationTier::V => "V",
            crate::security::IsolationTier::G => "G",
            _ => anyhow::bail!("native compute tier invalid"),
        };
        let spec = crate::sandbox::NativeToolSpec {
            run_id: parent.workload.goal.clone(),
            tool_id: crate::kernel::hex(&crate::audit::hash(request.invocation_intent.as_bytes())),
            incarnation: parent.workload.incarnation.parse()?,
            expires_ms: request.deadline_ms,
            owned_dir: directory.clone(),
            limits: limits.clone(),
            argv: vec![],
            input: vec![],
        };
        let cid = if tier == "V" {
            let cid: u32 = manifest["guest_cid"]
                .as_u64()
                .context("frozen guest CID absent")?
                .try_into()?;
            ensure!(
                manifest["native"]["assets"]["guest_cid"].as_u64() == Some(cid as u64),
                "native backend guest CID differs from manifest"
            );
            Some(cid)
        } else {
            None
        };
        (
            Resources {
                cpu_millicores: limits.cpu_millicores,
                memory_bytes: limits.memory_bytes,
                provider_slots: 0,
                rate_tokens: 0,
            },
            crate::sandbox::native_unit(&spec, tier)?,
            cid,
        )
    };
    ensure!(
        request.resources == expected
            && manifest["resources"] == serde_json::to_value(expected)?
            && manifest["resource_unit"].as_str() == Some(unit.as_str()),
        "compute resource request differs from frozen sandbox limits/unit"
    );
    if let Some(cid) = cid {
        cid_available(view, &parent.workload.node, cid)?;
    }
    let node: Node = serde_json::from_value(
        view.kv
            .get(&format!("nodes/{}", parent.workload.node))
            .context("compute node absent")?
            .clone(),
    )?;
    ensure!(
        node.id == parent.workload.node && node.healthy,
        "compute node is not healthy"
    );
    let lease = view
        .leases
        .get(&format!("allocation/{}", parent.workload.agent))
        .context("compute parent allocation missing")?;
    ensure!(
        !lease.revoked
            && lease.holder == parent.workload.node
            && lease.fence == parent.allocation_fence
            && lease.expires_ms > now,
        "compute allocation is fenced"
    );
    let nodes = [parent.workload.node.clone()];
    let mut leases = crate::scheduler::node_lease_checks(view, &nodes)?;
    leases.push(LeaseCheck {
        id: format!("allocation/{}", parent.workload.agent),
        holder: parent.workload.node.clone(),
        fence: parent.allocation_fence,
    });
    Ok((
        Record {
            request_hash,
            intent: request.invocation_intent.clone(),
            node_id: parent.workload.node.clone(),
            resources: expected,
            deadline_ms: request.deadline_ms,
            unit,
            directory,
            guest_cid: cid,
            status: "active".into(),
        },
        node,
        leases,
    ))
}
/// All admissions share the same StateView revision; a collision never starts another VM.
pub(crate) fn cid_available(view: &crate::cluster::StateView, node: &str, cid: u32) -> Result<()> {
    ensure!(cid > 2 && cid < u32::MAX, "reserved guest CID");
    for (key, value) in &view.kv {
        if key.starts_with("resources/tool/") || key.starts_with("tools/bindings/") {
            let active = value["status"]
                .as_str()
                .is_some_and(|status| status != "released" && status != "retired");
            let same_node = value["node_id"].as_str() == Some(node)
                || value["allocation"]["node_id"].as_str() == Some(node);
            ensure!(
                !(active && same_node && value["guest_cid"].as_u64() == Some(cid as u64)),
                "guest CID already reserved on this node"
            );
        }
    }
    Ok(())
}
pub(crate) async fn verify_cleanup(path: &Path, unit: &str) -> Result<()> {
    ensure!(
        std::fs::metadata(path)?.len() <= 65536,
        "cleanup proof exceeds bound"
    );
    let proof: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        proof["verdict"] == "PASS" && proof["unit"].as_str() == Some(unit),
        "sandbox cleanup proof is not confirmed for exact unit"
    );
    verify_stopped(unit).await
}
pub(crate) async fn verify_stopped(unit: &str) -> Result<()> {
    validate_owned_unit(unit)?;
    let mut command = if unit.starts_with("solos-tool-v-") {
        let mut command = Command::new("sudo");
        command.args(["-n", "systemctl"]);
        command
    } else {
        let mut command = Command::new("systemctl");
        command.arg("--user");
        command
    };
    let output = command
        .args([
            "show",
            unit,
            "--property=ActiveState,LoadState",
            "--no-pager",
        ])
        .output()
        .await?;
    let text = String::from_utf8_lossy(&output.stdout);
    ensure!(
        output.status.success() || text.contains("LoadState=not-found"),
        "owned resource status unavailable"
    );
    ensure!(
        text.lines()
            .any(|line| matches!(line, "ActiveState=inactive" | "ActiveState=failed")),
        "owned tool process remains live"
    );
    Ok(())
}
fn validate_owned_unit(unit: &str) -> Result<()> {
    ensure!(
        (unit.starts_with("solos-tool-v-")
            || unit.starts_with("solos-tool-g-")
            || unit.starts_with("solos-wasi-"))
            && unit.ends_with(".service")
            && unit.len() < 100
            && unit
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-.".contains(&byte)),
        "resource unit is outside owned tool namespace"
    );
    Ok(())
}
pub(crate) async fn stop_owned(unit: &str) -> Result<()> {
    validate_owned_unit(unit)?;
    if verify_stopped(unit).await.is_ok() {
        return Ok(());
    }
    let mut command = if unit.starts_with("solos-tool-v-") {
        let mut command = Command::new("sudo");
        command.args(["-n", "systemctl"]);
        command
    } else {
        let mut command = Command::new("systemctl");
        command.arg("--user");
        command
    };
    ensure!(
        command.args(["stop", unit]).status().await?.success(),
        "owned tool stop unconfirmed"
    );
    verify_stopped(unit).await
}
async fn retire(
    kernel: &Kernel,
    executor: &Arc<RwLock<AuthenticatedSubject>>,
    record: &Record,
) -> Result<()> {
    let key = format!("resources/tool/{}", record.intent);
    let _write = kernel.cluster.kernel_write(true).await;
    for _ in 0..16 {
        let view = kernel.cluster.view_control().await?;
        let value = view.kv.get(&key).context("resource record missing")?;
        let mut saved: Record = serde_json::from_value(value.clone())?;
        ensure!(
            saved.request_hash == record.request_hash,
            "resource identity mismatch"
        );
        if saved.status == "released" {
            return Ok(());
        }
        let original = view
            .kv
            .get("kernel/state")
            .context("deployment not initialized")?
            .clone();
        let mut state: KernelState = serde_json::from_value(original.clone())?;
        let actor = executor.read().await.clone();
        trusted(&state, &actor)?;
        let node_key = format!("nodes/{}", record.node_id);
        let mut node: Node = serde_json::from_value(
            view.kv
                .get(&node_key)
                .context("resource node missing")?
                .clone(),
        )?;
        node.used = node
            .used
            .subtract(record.resources)
            .context("resource release underflow")?;
        node.provider_rate_remaining = node
            .provider_rate_remaining
            .checked_add(record.resources.rate_tokens)
            .context("resource rate release overflow")?;
        saved.status = "released".into();
        append(
            &mut state,
            kernel,
            &saved,
            actor.id(),
            "tool_resources_released",
        )?;
        let mut tx = Transaction::new(
            uuid::Uuid::new_v4().to_string(),
            vec![
                Mutation::Put {
                    key: node_key,
                    value: serde_json::to_value(node)?,
                },
                Mutation::Put {
                    key: key.clone(),
                    value: serde_json::to_value(saved)?,
                },
                Mutation::Put {
                    key: "kernel/state".into(),
                    value: serde_json::to_value(state)?,
                },
            ],
        );
        tx.expected_revision = Some(view.revision);
        tx.compares.push(Compare {
            key: "kernel/state".into(),
            expected: Some(original),
        });
        tx.compares.push(Compare {
            key: key.clone(),
            expected: Some(value.clone()),
        });
        let result = kernel.cluster.transact_control(tx).await?;
        if result.applied {
            return Ok(());
        }
        ensure!(
            result.error.as_deref() == Some("compare-and-swap conflict"),
            "resource retirement refused: {:?}",
            result.error
        );
    }
    anyhow::bail!("resource retirement retry bound reached")
}
pub async fn recover(kernel: &Kernel, executor: &Arc<RwLock<AuthenticatedSubject>>) -> Result<()> {
    let node = crate::identity::WorkloadId::parse(executor.read().await.id())?.node;
    let view = kernel.cluster.view_control().await?;
    for (key, value) in &view.kv {
        if key.starts_with("resources/tool/") {
            let record: Record = serde_json::from_value(value.clone())?;
            if record.node_id != node || record.status == "released" {
                continue;
            }
            if verify_cleanup(&record.directory.join("cleanup.json"), &record.unit)
                .await
                .is_ok()
            {
                retire(kernel, executor, &record).await?;
            } else if record.deadline_ms <= crate::cluster::now_ms() {
                stop_owned(&record.unit).await?;
                retire(kernel, executor, &record).await?;
            }
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cid_collision_checks_both_owned_hold_kinds_and_exact_node() {
        let mut view = crate::cluster::StateView::default();
        view.kv.insert(
            "resources/tool/first".into(),
            serde_json::json!({"node_id":"one","guest_cid":71,"status":"active"}),
        );
        assert!(cid_available(&view, "one", 71).is_err());
        assert!(cid_available(&view, "two", 71).is_ok());
        view.kv.get_mut("resources/tool/first").unwrap()["status"] = serde_json::json!("released");
        assert!(cid_available(&view, "one", 71).is_ok());
        view.kv.insert(
            "tools/bindings/sdk".into(),
            serde_json::json!({"allocation":{"node_id":"one"},"guest_cid":71,"status":"active"}),
        );
        assert!(cid_available(&view, "one", 71).is_err());
        assert!(cid_available(&view, "one", 2).is_err());
        assert!(cid_available(&view, "one", u32::MAX).is_err());
    }
    #[tokio::test]
    async fn cancel_rejects_nonowned_units_before_any_subprocess() {
        let error = stop_owned("ssh.service").await.unwrap_err();
        assert!(error.to_string().contains("outside owned tool namespace"));
    }
}
