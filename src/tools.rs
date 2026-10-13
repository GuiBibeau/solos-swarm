//! Installed tool adapters consume admitted plans; guest code never receives host credentials.
use crate::{
    artifacts::{ArtifactMetadata, ArtifactStore, ExportPermit, ReadPermit},
    audit::{CryptoSigner, Signed},
    ledger::PriceTable,
    node::{Session, ToolDefinition},
    sandbox::{
        self, MicroVmAssets, NativeToolSpec, PinnedAsset, PinnedTree, ProcessLimits, ToolLimits,
    },
    security::IsolationTier,
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, watch};

pub struct ToolContext {
    pub holder: String,
    pub goal: String,
    pub markings: BTreeSet<String>,
    pub clearance: BTreeSet<String>,
    pub parents: BTreeSet<String>,
    pub recipients: BTreeSet<String>,
    pub signer: Arc<CryptoSigner>,
}
pub struct ToolPlan {
    pub intent: String,
    pub session: Session,
    pub payload: Value,
    pub isolation: IsolationTier,
    pub prices: Option<Signed<PriceTable>>,
    pub usage_bound: BTreeMap<String, u64>,
    pub markings: BTreeSet<String>,
    pub deadline_ms: u64,
    pub inputs: Vec<crate::security::InputRecord>,
    pub broker_calls: Vec<ToolDefinition>,
    pub broker_budget_microusd: u64,
    prepared: PreparedTool,
}
impl ToolPlan {
    pub fn freeze_pending(&mut self, pending: &crate::kernel::PendingAction) -> Result<()> {
        ensure!(
            self.payload["tool"] == pending.payload["tool"]
                && self.payload["arguments"] == pending.payload["arguments"]
                && self.payload["execution_manifest"] == pending.payload["execution_manifest"]
                && self.payload["input_sources"] == pending.payload["input_sources"]
                && self.isolation == pending.request.action.isolation,
            "pending tool plan changed; use a new intent"
        );
        let deadline = pending.payload["deadline_ms"]
            .as_u64()
            .context("pending tool deadline absent")?;
        ensure!(
            deadline > crate::cluster::now_ms() && deadline <= self.deadline_ms,
            "pending tool deadline expired or widened"
        );
        self.deadline_ms = deadline;
        self.payload = pending.payload.clone();
        self.prices = pending.prices.clone();
        self.usage_bound = pending.request.usage_bound.clone();
        if let PreparedTool::Native(spec, ..) = &mut self.prepared {
            spec.expires_ms = deadline;
        }
        Ok(())
    }
}
enum PreparedTool {
    Wasi {
        module: Vec<u8>,
        input: Vec<u8>,
        limits: ToolLimits,
    },
    Native(NativeToolSpec, NativeBackend),
    Artifact {
        operation: String,
        args: Value,
        context: ToolContext,
    },
    TargetRead {
        endpoint: TargetEndpoint,
        path: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeBackend {
    V {
        assets: MicroVmAssets,
    },
    G {
        runsc: PinnedAsset,
        rootfs: PinnedTree,
        allow_no_kvm_fallback: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledConfig {
    pub owned_directory: PathBuf,
    pub artifacts: PathBuf,
    pub artifact_key: PathBuf,
    pub wasi_limits: ToolLimits,
    pub native_limits: ProcessLimits,
    pub native_deadline_ms: u64,
    pub native: Option<NativeBackend>,
    pub price: Signed<PriceTable>,
    pub export_roots: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub broker_calls: Vec<ToolDefinition>,
    #[serde(default)]
    pub broker_budget_microusd: u64,
    #[serde(default)]
    pub targets: BTreeMap<String, TargetEndpoint>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetEndpoint {
    pub endpoint: String,
    pub paths: BTreeSet<String>,
    pub ca_pem: Option<PathBuf>,
    pub timeout_ms: u64,
    pub max_response_bytes: usize,
    pub markings: BTreeSet<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolExecution {
    pub completed: bool,
    pub value: Value,
    pub usage: BTreeMap<String, u64>,
    pub markings: BTreeSet<String>,
    #[serde(default)]
    pub transport: Option<TargetTransport>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetTransport {
    pub contacted: bool,
    pub payload_request_bytes: u64,
    pub payload_response_bytes: u64,
}
pub type ToolPlanFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolPlan>> + Send>>;
pub type ToolExecutionFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<ToolExecution>> + Send>>;

#[derive(Clone)]
pub struct InstalledTools {
    config: Arc<InstalledConfig>,
    store: Arc<Mutex<ArtifactStore>>,
    factory: Arc<
        std::sync::RwLock<Option<std::sync::Weak<dyn crate::tool_identity::ToolIdentityFactory>>>,
    >,
}
impl InstalledTools {
    pub fn open(path: &Path) -> Result<Self> {
        ensure!(
            fs::metadata(path)?.len() <= 1_048_576,
            "installed tool config too large"
        );
        let config: InstalledConfig = serde_json::from_slice(&fs::read(path)?)?;
        config.wasi_limits.validate()?;
        config.native_limits.validate()?;
        ensure!(
            config.owned_directory.is_absolute()
                && (1..=60_000).contains(&config.native_deadline_ms),
            "owned tool root/deadline missing"
        );
        fs::create_dir_all(&config.owned_directory)?;
        ensure!(
            !fs::symlink_metadata(&config.owned_directory)?
                .file_type()
                .is_symlink(),
            "tool root symlink denied"
        );
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config.owned_directory, fs::Permissions::from_mode(0o700))?;
        let store = ArtifactStore::open(&config.artifacts, &config.artifact_key)?;
        Ok(Self {
            config: Arc::new(config),
            store: Arc::new(Mutex::new(store)),
            factory: Arc::new(std::sync::RwLock::new(None)),
        })
    }
    async fn prepare(
        &self,
        session: Session,
        tool: ToolDefinition,
        args: Value,
        intent: String,
        context: ToolContext,
    ) -> Result<ToolPlan> {
        ensure!(
            tool.kind == crate::security::CapabilityKind::ToolCall && tool.operation == tool.name,
            "installed tool declaration differs from adapter"
        );
        ensure!(
            tool.resource == format!("tool/{}/{}", context.goal, tool.name)
                || tool.name == "target_read" && self.config.targets.contains_key(&tool.resource),
            "tool resource outside exact Goal adapter"
        );
        let now = crate::cluster::now_ms();
        let mut inputs = Vec::new();
        let mut ambient = context.markings.clone();
        let mut broker_calls = Vec::new();
        let mut broker_budget_microusd = 0;
        let (prepared, isolation, prices, usage_bound, deadline_ms) = match tool.name.as_str() {
            "wasi_execute" => {
                let module = decode(&args, "module", 4 * 1024 * 1024)?;
                let input = decode(&args, "input", 1_048_576)?;
                let limits = self.config.wasi_limits.clone();
                let usage = [
                    ("elapsed_ms".into(), limits.deadline_ms),
                    ("output_bytes".into(), limits.output_bytes as u64 * 2),
                    ("fuel".into(), limits.fuel),
                ]
                .into();
                let deadline = now
                    .saturating_add(limits.deadline_ms)
                    .min(session.lease_expiry_ms);
                (
                    PreparedTool::Wasi {
                        module,
                        input,
                        limits,
                    },
                    IsolationTier::W,
                    Some(self.config.price.clone()),
                    usage,
                    deadline,
                )
            }
            "native_execute" => {
                let mut backend = self
                    .config
                    .native
                    .clone()
                    .context("native backend not installed")?;
                if let NativeBackend::V { assets } = &mut backend {
                    let hash = crate::audit::hash(
                        format!("{}:{intent}", session.workload.node).as_bytes(),
                    );
                    assets.guest_cid =
                        3 + u32::from_be_bytes(hash[..4].try_into()?) % (u32::MAX - 3);
                }
                let requested: Vec<String> =
                    serde_json::from_value(args.get("broker_tools").cloned().unwrap_or(json!([])))?;
                let mut unique = BTreeSet::new();
                for name in requested {
                    ensure!(unique.insert(name.clone()), "duplicate tool SDK request");
                    broker_calls.push(
                        self.config
                            .broker_calls
                            .iter()
                            .find(|call| call.name == name)
                            .context("tool SDK call is not operator registered")?
                            .clone(),
                    );
                }
                if !broker_calls.is_empty() {
                    ensure!(
                        (matches!(backend, NativeBackend::V { .. })
                            || matches!(
                                backend,
                                NativeBackend::G {
                                    allow_no_kvm_fallback: true,
                                    ..
                                }
                            ))
                            && self.config.broker_budget_microusd > 0,
                        "tool SDK requires bounded V or explicitly admitted no-KVM G binding"
                    );
                    broker_budget_microusd = self.config.broker_budget_microusd;
                }
                let code = args["code"]
                    .as_str()
                    .context("native Python source missing")?;
                ensure!(
                    code.len() <= 262_144 && !code.contains('\0'),
                    "native source bound exceeded"
                );
                let input = decode(&args, "input", 1_048_576)?;
                let deadline = now
                    .saturating_add(self.config.native_deadline_ms)
                    .min(session.lease_expiry_ms);
                let digest = crate::kernel::hex(&crate::audit::hash(intent.as_bytes()));
                let spec = NativeToolSpec {
                    run_id: context.goal.clone(),
                    tool_id: digest,
                    incarnation: session.workload.incarnation.parse()?,
                    expires_ms: deadline,
                    owned_dir: self
                        .config
                        .owned_directory
                        .join(crate::kernel::hex(&crate::audit::hash(intent.as_bytes()))),
                    limits: self.config.native_limits.clone(),
                    argv: vec![
                        "/usr/bin/python3".into(),
                        "-I".into(),
                        "-c".into(),
                        code.into(),
                    ],
                    input,
                };
                let tier = match &backend {
                    NativeBackend::V { .. } => IsolationTier::V,
                    NativeBackend::G { .. } => IsolationTier::G,
                };
                let usage = [
                    ("elapsed_ms".into(), self.config.native_deadline_ms),
                    ("output_bytes".into(), spec.limits.max_output_bytes * 2),
                    ("fuel".into(), 0),
                ]
                .into();
                (
                    PreparedTool::Native(spec, backend),
                    tier,
                    Some(self.config.price.clone()),
                    usage,
                    deadline,
                )
            }
            "target_read" => {
                ensure!(
                    matches!(session.isolation, IsolationTier::V | IsolationTier::G)
                        && tool.class == crate::security::OperationClass::ReadOnly,
                    "target SDK call requires an actual V or admitted no-KVM G tool session"
                );
                let endpoint = self
                    .config
                    .targets
                    .get(&tool.resource)
                    .context("target endpoint is not configured")?
                    .clone();
                let path = text(&args, "path")?;
                let url = reqwest::Url::parse(&endpoint.endpoint)?;
                ensure!(
                    url.username().is_empty()
                        && url.password().is_none()
                        && url.query().is_none()
                        && url.fragment().is_none()
                        && (url.scheme() == "https"
                            || url.scheme() == "http"
                                && url.host_str().is_some_and(|h| h == "localhost"
                                    || h.parse::<std::net::IpAddr>()
                                        .is_ok_and(|ip| ip.is_loopback())))
                        && endpoint.paths.contains(&path)
                        && path.starts_with('/')
                        && !path.contains("..")
                        && !path.contains(['?', '#', '\\'])
                        && (1..=10_000).contains(&endpoint.timeout_ms)
                        && (1..=1_048_576).contains(&endpoint.max_response_bytes),
                    "target endpoint/path/bounds not admitted"
                );
                ambient.extend(endpoint.markings.iter().cloned());
                ensure!(
                    endpoint.markings.is_subset(&context.clearance),
                    "target result classifications exceed read Capability"
                );
                (
                    PreparedTool::TargetRead { endpoint, path },
                    session.isolation,
                    None,
                    BTreeMap::new(),
                    now.saturating_add(10_000).min(session.lease_expiry_ms),
                )
            }
            "artifact_put"
            | "artifact_get"
            | "artifact_retract"
            | "artifact_export"
            | "artifact_prepare_version" => {
                ensure!(
                    tool.class
                        == match tool.name.as_str() {
                            "artifact_get" => crate::security::OperationClass::ReadOnly,
                            "artifact_export" => crate::security::OperationClass::External,
                            _ => crate::security::OperationClass::Reversible,
                        },
                    "artifact operation class differs"
                );
                let ids: Vec<String> = match tool.name.as_str() {
                    "artifact_put" => serde_json::from_value(args["parents"].clone())?,
                    "artifact_prepare_version" => serde_json::from_value(args["ids"].clone())?,
                    _ => vec![text(&args, "id")?],
                };
                ensure!(
                    ids.len() <= 128,
                    "artifact lineage/read batch exceeds bound"
                );
                let permit = ReadPermit {
                    holder_id: context.holder.clone(),
                    goal_ids: [context.goal.clone()].into(),
                    classifications: context.clearance.clone(),
                    expiry_ms: session.lease_expiry_ms,
                    allow_retracted: false,
                };
                let store = self.store.lock().await;
                for id in ids {
                    let info = store.inspect(&id, &permit, now)?;
                    let proof: Signed<ArtifactMetadata> =
                        serde_json::from_str(&info.metadata.signature)?;
                    proof.verify("artifact-provenance", &context.signer.public_key())?;
                    let mut unsigned = info.metadata.clone();
                    unsigned.signature.clear();
                    ensure!(
                        crate::audit::content_hash(&proof.payload)?
                            == crate::audit::content_hash(&unsigned)?,
                        "artifact provenance differs from metadata"
                    );
                    ambient.extend(info.metadata.classifications.iter().cloned());
                    inputs.push(crate::security::InputRecord {
                        id: format!("artifact/{id}"),
                        source_type: "artifact".into(),
                        hash: crate::audit::content_hash(&info)?,
                        goal_id: context.goal.clone(),
                        producer: info.metadata.producer_id,
                        trust: crate::security::Trust::Untrusted,
                        markings: info.metadata.classifications,
                        parents: serde_json::from_value(
                            info.metadata.provenance["input_sources"].clone(),
                        )?,
                    });
                }
                (
                    PreparedTool::Artifact {
                        operation: tool.name.clone(),
                        args: args.clone(),
                        context: ToolContext {
                            signer: context.signer.clone(),
                            holder: context.holder.clone(),
                            goal: context.goal.clone(),
                            markings: context.markings.clone(),
                            clearance: context.clearance.clone(),
                            parents: context.parents.clone(),
                            recipients: context.recipients.clone(),
                        },
                    },
                    IsolationTier::P,
                    None,
                    BTreeMap::new(),
                    now.saturating_add(10_000).min(session.lease_expiry_ms),
                )
            }
            _ => anyhow::bail!("unknown installed tool"),
        };
        ensure!(deadline_ms > now, "tool allocation expired");
        let directory = self
            .config
            .owned_directory
            .join(crate::kernel::hex(&crate::audit::hash(intent.as_bytes())));
        let (resources, unit, native, guest_cid) = match &prepared {
            PreparedTool::Wasi { limits, .. } => (
                Some(crate::scheduler::Resources {
                    cpu_millicores: 1000,
                    memory_bytes: (limits.memory_bytes as u64)
                        .checked_add(512 * 1024 * 1024)
                        .context("compiler memory bound overflow")?,
                    provider_slots: 0,
                    rate_tokens: 0,
                }),
                Some(crate::wasi_worker::unit_for_directory(&directory)?),
                None,
                None,
            ),
            PreparedTool::Native(spec, backend) => (
                Some(crate::scheduler::Resources {
                    cpu_millicores: spec.limits.cpu_millicores,
                    memory_bytes: spec.limits.memory_bytes,
                    provider_slots: 0,
                    rate_tokens: 0,
                }),
                Some(sandbox::native_unit(
                    spec,
                    if matches!(backend, NativeBackend::V { .. }) {
                        "V"
                    } else {
                        "G"
                    },
                )?),
                Some(backend.clone()),
                match backend {
                    NativeBackend::V { assets } => Some(assets.guest_cid),
                    _ => None,
                },
            ),
            _ => (None, None, None, None),
        };
        let payload = json!({"tool":tool.name,"arguments":args,"deadline_ms":deadline_ms,"isolation":isolation,
            "input_sources":context.parents,"input_markings":context.markings,
            "execution_manifest":{"native":native,"wasi_limits":self.config.wasi_limits,"native_limits":self.config.native_limits,
                "resources":resources,"resource_directory":directory,"resource_unit":unit,"guest_cid":guest_cid,
                "broker_calls":broker_calls,"broker_budget_microusd":broker_budget_microusd,"targets":self.config.targets}});
        Ok(ToolPlan {
            intent,
            session,
            payload,
            isolation,
            prices,
            usage_bound,
            markings: ambient,
            deadline_ms,
            inputs,
            broker_calls,
            broker_budget_microusd,
            prepared,
        })
    }
    async fn run(&self, plan: ToolPlan, stop: watch::Receiver<bool>) -> Result<ToolExecution> {
        ensure!(
            !*stop.borrow() && crate::cluster::now_ms() < plan.deadline_ms,
            "tool stopped before execution"
        );
        let started = std::time::Instant::now();
        let mut markings = plan.markings.clone();
        let resource_directory = PathBuf::from(
            plan.payload["execution_manifest"]["resource_directory"]
                .as_str()
                .context("frozen tool directory missing")?,
        );
        let resources: Option<crate::scheduler::Resources> =
            serde_json::from_value(plan.payload["execution_manifest"]["resources"].clone())?;
        let mut resource_guard =
            if let Some(resources) = resources.filter(|_| plan.broker_calls.is_empty()) {
                let factory = self
                    .factory
                    .read()
                    .map_err(|_| anyhow::anyhow!("tool factory lock poisoned"))?
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                    .context("aggregate tool resource owner unavailable")?;
                Some(
                    factory
                        .reserve(crate::resource_guard::ResourceRequest {
                            parent: plan.session.clone(),
                            invocation_intent: plan.intent.clone(),
                            resources,
                            deadline_ms: plan.deadline_ms,
                        })
                        .await?,
                )
            } else {
                None
            };
        let result = match plan.prepared {
            PreparedTool::Wasi {
                module,
                input,
                mut limits,
            } => {
                limits.deadline_ms = limits
                    .deadline_ms
                    .min(plan.deadline_ms.saturating_sub(crate::cluster::now_ms()));
                let directory = resource_directory;
                let result = crate::wasi_worker::run_isolated(
                    &module,
                    &input,
                    &limits,
                    &directory,
                    plan.deadline_ms,
                    stop.clone(),
                )
                .await;
                if let Some(guard) = resource_guard.take() {
                    guard.release(&directory.join("cleanup.json")).await?;
                }
                result_value(result?, markings)
            }
            PreparedTool::Native(spec, backend) => {
                let mut binding = None;
                if !plan.broker_calls.is_empty() {
                    let factory = self
                        .factory
                        .read()
                        .map_err(|_| anyhow::anyhow!("tool factory lock poisoned"))?
                        .as_ref()
                        .and_then(std::sync::Weak::upgrade)
                        .context("scoped tool identity factory unavailable")?;
                    binding = Some(
                        factory
                            .issue(crate::tool_identity::ToolSpawnRequest {
                                invocation_intent: plan.intent.clone(),
                                parent: plan.session.clone(),
                                calls: plan.broker_calls,
                                budget_microusd: plan.broker_budget_microusd,
                                deadline_ms: plan.deadline_ms,
                            })
                            .await?,
                    );
                }
                let native = match backend {
                    NativeBackend::V { assets } => sandbox::prepare_microvm_with_broker(
                        &spec,
                        &assets,
                        binding.as_ref().map(|guard| guard.binding()),
                    )?,
                    NativeBackend::G {
                        runsc,
                        rootfs,
                        allow_no_kvm_fallback,
                    } => sandbox::prepare_gvisor_with_broker(
                        &spec,
                        &runsc,
                        &rootfs,
                        allow_no_kvm_fallback,
                        binding.as_ref().map(|guard| guard.binding()),
                    )?,
                };
                let result = sandbox::execute_native_scoped(&native, stop.clone()).await;
                if let Some(binding) = binding {
                    binding.finish().await?;
                }
                if let Some(guard) = resource_guard.take() {
                    guard.release(&spec.owned_dir.join("cleanup.json")).await?;
                }
                result_value(result?, markings)
            }
            PreparedTool::TargetRead { endpoint, path } => {
                let mut measured = TargetTransport {
                    contacted: false,
                    payload_request_bytes: 0,
                    payload_response_bytes: 0,
                };
                let attempt=async {
                    ensure!(!*stop.borrow(),"target cancelled before dispatch");
                    let mut client=reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none())
                        .timeout(std::time::Duration::from_millis(endpoint.timeout_ms));
                    if let Some(ca)=&endpoint.ca_pem {
                        let mut pem=Vec::new();
                        fs::File::open(ca)?.take(65_537).read_to_end(&mut pem)?;
                        ensure!(!pem.is_empty()&&pem.len()<=65_536,"target CA material exceeds bound");
                        client=client.tls_built_in_root_certs(false).add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
                    }
                    let mut url=reqwest::Url::parse(&endpoint.endpoint)?;url.set_path(&path);
                    let mut cancellation=stop.clone();
                    let mut request=client.build()?.get(url);
                    if let Some((parent,state))=crate::diagnostics::current_trace_headers(){request=request.header("traceparent",parent);if let Some(state)=state{request=request.header("tracestate",state);}}
                    let mut response=tokio::select! {
                        changed=cancellation.changed()=>{let _=changed;anyhow::bail!("target cancelled before response headers; delivery unknown")},
                        response=tokio::time::timeout(std::time::Duration::from_millis(plan.deadline_ms.saturating_sub(crate::cluster::now_ms())),request.send())=>response??,
                    };
                    measured.contacted=true;
                    let status=response.status().as_u16();let mut bytes=Vec::new();
                    loop {
                        let chunk=tokio::select! {
                            changed=cancellation.changed()=>{let _=changed;anyhow::bail!("target body cancelled; outcome unknown")},
                            chunk=tokio::time::timeout(std::time::Duration::from_millis(plan.deadline_ms.saturating_sub(crate::cluster::now_ms())),response.chunk())=>chunk??,
                        };
                        let Some(chunk)=chunk else {break};
                        measured.payload_response_bytes=measured.payload_response_bytes.checked_add(chunk.len() as u64).context("target byte counter overflow")?;
                        ensure!(bytes.len().saturating_add(chunk.len())<=endpoint.max_response_bytes,"target response exceeds bound");
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok::<Value,anyhow::Error>(json!({"status":status,"body":STANDARD.encode(&bytes)}))
                }.await;
                markings.extend(endpoint.markings);
                Ok(ToolExecution {
                    completed:attempt.is_ok(),value:attempt.unwrap_or_else(|_|json!({"error":"target delivery or completion unknown; receipt reconciliation required"})),
                    usage:BTreeMap::new(),markings,transport:Some(measured),
                })
            }
            PreparedTool::Artifact {
                operation,
                args,
                context,
            } => {
                let mut store = self.store.lock().await;
                let permit = ReadPermit {
                    holder_id: context.holder.clone(),
                    goal_ids: [context.goal.clone()].into(),
                    classifications: context.clearance.clone(),
                    expiry_ms: plan.deadline_ms,
                    allow_retracted: false,
                };
                let now = crate::cluster::now_ms();
                let value = match operation.as_str() {
                    "artifact_put" => {
                        let bytes = decode(&args, "body", 1_048_576)?;
                        let parents: Vec<String> = serde_json::from_value(args["parents"].clone())?;
                        for parent in &parents {
                            let info = store.inspect(parent, &permit, now)?;
                            markings.extend(info.metadata.classifications);
                        }
                        if markings.is_empty() {
                            markings.insert("public".into());
                        }
                        let mut metadata = ArtifactMetadata {
                            goal_id: context.goal,
                            logical_name: text(&args, "name")?,
                            version: text(&args, "version")?,
                            classifications: markings.clone(),
                            trust: 0,
                            producer_id: context.holder,
                            broker_id: "trusted-broker".into(),
                            provenance: json!({"input_sources":context.parents,"body_hash":crate::kernel::hex(&crate::audit::hash(&bytes)),"intent":plan.intent}),
                            parents,
                            signature: String::new(),
                            created_ms: now,
                        };
                        metadata.signature = serde_json::to_string(
                            &context.signer.sign("artifact-provenance", &metadata)?,
                        )?;
                        serde_json::to_value(store.put(&bytes, metadata)?)?
                    }
                    "artifact_get" => {
                        let id = text(&args, "id")?;
                        let info = store.inspect(&id, &permit, now)?;
                        markings.extend(info.metadata.classifications.iter().cloned());
                        json!({"artifact":info,"body":STANDARD.encode(store.get(&id,&permit,now)?.as_slice())})
                    }
                    "artifact_retract" => {
                        let id = text(&args, "id")?;
                        let info = store.inspect(&id, &permit, now)?;
                        ensure!(
                            info.metadata.producer_id == context.holder,
                            "only own artifact retraction admitted"
                        );
                        let reason = text(&args, "reason")?;
                        let signature = serde_json::to_string(&context.signer.sign(
                            "artifact-retraction",
                            &json!({"id":id,"reason":reason,"actor":context.holder}),
                        )?)?;
                        json!({"retracted":store.retract(&id,&context.holder,&reason,&signature,now)?})
                    }
                    "artifact_export" => {
                        let recipient = text(&args, "recipient")?;
                        let root = self
                            .config
                            .export_roots
                            .get(&recipient)
                            .context("recipient has no provisioned export root")?;
                        ensure!(
                            context.recipients.contains(&recipient),
                            "artifact recipient outside Capability"
                        );
                        let export = ExportPermit {
                            read: permit,
                            destination_root: root.clone(),
                            recipient_id: recipient.clone(),
                            allowed_recipients: context.recipients,
                            max_bytes: 1_048_576,
                        };
                        json!({"destination":store.export(&text(&args,"id")?,&text(&args,"name")?,&export,now)?})
                    }
                    "artifact_prepare_version" => serde_json::to_value(store.prepare_version(
                        &text(&args, "namespace")?,
                        &text(&args, "version")?,
                        serde_json::from_value(args["ids"].clone())?,
                        &permit,
                        now,
                    )?)?,
                    _ => unreachable!(),
                };
                Ok(ToolExecution {
                    completed: true,
                    transport: None,
                    value,
                    usage: [("elapsed_ms".into(), started.elapsed().as_millis() as u64)].into(),
                    markings,
                })
            }
        }?;
        ensure!(
            serde_json::to_vec(&result.value)?.len() <= 4_194_304,
            "tool result exceeds broker output bound"
        );
        Ok(result)
    }
}
fn result_value(result: sandbox::ToolResult, markings: BTreeSet<String>) -> Result<ToolExecution> {
    let usage = [
        ("elapsed_ms".into(), result.elapsed_ms),
        (
            "output_bytes".into(),
            (result.stdout.len() + result.stderr.len()) as u64,
        ),
        ("fuel".into(), result.fuel_consumed.unwrap_or(0)),
    ]
    .into();
    Ok(ToolExecution {
        completed: true,
        transport: None,
        value: json!({"tier":result.tier,"stdout":STANDARD.encode(result.stdout),"stderr":STANDARD.encode(result.stderr)}),
        usage,
        markings,
    })
}
fn text(args: &Value, name: &str) -> Result<String> {
    Ok(args[name]
        .as_str()
        .context("missing tool text argument")?
        .into())
}
fn decode(args: &Value, name: &str, limit: usize) -> Result<Vec<u8>> {
    let encoded = args[name].as_str().context("base64 tool input missing")?;
    ensure!(
        encoded.len() <= limit.div_ceil(3) * 4,
        "encoded tool input exceeds bound"
    );
    let bytes = STANDARD.decode(encoded)?;
    ensure!(bytes.len() <= limit, "tool input exceeds bound");
    Ok(bytes)
}
impl crate::node::ToolExecutor for InstalledTools {
    fn attach_tool_factory(
        &self,
        factory: std::sync::Weak<dyn crate::tool_identity::ToolIdentityFactory>,
    ) -> Result<()> {
        *self
            .factory
            .write()
            .map_err(|_| anyhow::anyhow!("tool factory lock poisoned"))? = Some(factory);
        Ok(())
    }
    fn plan(
        &self,
        session: Session,
        tool: ToolDefinition,
        args: Value,
        intent: String,
        context: ToolContext,
    ) -> ToolPlanFuture {
        let adapter = self.clone();
        Box::pin(async move { adapter.prepare(session, tool, args, intent, context).await })
    }
    fn execute(&self, plan: ToolPlan, stop: watch::Receiver<bool>) -> ToolExecutionFuture {
        let adapter = self.clone();
        Box::pin(async move { adapter.run(plan, stop).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::WorkloadId,
        ledger::Rate,
        planner::HistoryConfig,
        security::{CapabilityKind, OperationClass},
    };
    #[test]
    fn binary_ingress_is_bounded_and_never_interprets_a_path() {
        assert_eq!(decode(&json!({"body":"YWJj"}), "body", 3).unwrap(), b"abc");
        assert!(decode(&json!({"body":"YWJj"}), "body", 2).is_err());
        assert!(decode(&json!({"body":"/etc/passwd"}), "body", 32).is_err());
    }
    fn setup() -> (PathBuf, InstalledTools, Session, Arc<CryptoSigner>) {
        let root = std::env::temp_dir().join(format!("solos-tools-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        ArtifactStore::create_key(&root.join("key")).unwrap();
        let signer = Arc::new(CryptoSigner::from_seed([41; 32]));
        let config = InstalledConfig {
            owned_directory: root.join("owned"),
            artifacts: root.join("artifacts"),
            artifact_key: root.join("key"),
            wasi_limits: ToolLimits {
                fuel: 100_000,
                memory_bytes: 65536,
                deadline_ms: 5000,
                output_bytes: 4096,
            },
            native_limits: ProcessLimits {
                cpu_millicores: 1000,
                memory_bytes: 268_435_456,
                pids: 32,
                max_output_bytes: 4096,
            },
            native_deadline_ms: 5000,
            native: None,
            price: signer
                .sign(
                    "price-table",
                    PriceTable {
                        version: "1".into(),
                        provider: "local-tools".into(),
                        model: "sandbox".into(),
                        currency: "microusd".into(),
                        not_before: 0,
                        expires: u64::MAX,
                        rates: ["elapsed_ms", "output_bytes", "fuel"]
                            .into_iter()
                            .map(|name| {
                                (
                                    name.into(),
                                    Rate {
                                        numerator: 1,
                                        denominator: 1,
                                    },
                                )
                            })
                            .collect(),
                    },
                )
                .unwrap(),
            export_roots: BTreeMap::new(),
            broker_calls: vec![],
            broker_budget_microusd: 0,
            targets: BTreeMap::new(),
        };
        fs::write(
            root.join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let workload = WorkloadId {
            trust_domain: "test.invalid".into(),
            tenant: "tenant".into(),
            goal: "goal".into(),
            node: "node".into(),
            agent: "agent".into(),
            incarnation: "1".into(),
        };
        let session = Session {
            workload,
            grant_id: "grant".into(),
            envelope_id: "envelope".into(),
            task_id: "task".into(),
            allocation_fence: 1,
            task_fence: 1,
            lease_expiry_ms: crate::cluster::now_ms() + 60_000,
            isolation: IsolationTier::P,
            provider: "p".into(),
            model: "m".into(),
            history: HistoryConfig::default(),
            tools: vec![],
        };
        (
            root.clone(),
            InstalledTools::open(&root.join("config.json")).unwrap(),
            session,
            signer,
        )
    }
    fn context(
        session: &Session,
        signer: Arc<CryptoSigner>,
        clearance: BTreeSet<String>,
    ) -> ToolContext {
        ToolContext {
            holder: session.workload.uri().unwrap(),
            goal: "goal".into(),
            markings: ["private".into()].into(),
            clearance,
            parents: BTreeSet::new(),
            recipients: BTreeSet::new(),
            signer,
        }
    }
    fn definition(name: &str, class: OperationClass) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: "test".into(),
            parameters: json!({}),
            operation: name.into(),
            kind: CapabilityKind::ToolCall,
            resource: format!("tool/goal/{name}"),
            class,
        }
    }
    #[tokio::test]
    async fn artifact_adapter_keeps_signed_encrypted_metadata_and_blocks_clearance_laundering() {
        let (root, adapter, session, signer) = setup();
        let args = json!({"name":"answer","version":"v1","body":STANDARD.encode(b"private answer"),"parents":[]});
        let plan = adapter
            .prepare(
                session.clone(),
                definition("artifact_put", OperationClass::Reversible),
                args,
                "put".into(),
                context(&session, signer.clone(), ["private".into()].into()),
            )
            .await
            .unwrap();
        let (_, stop) = watch::channel(false);
        let result = adapter.run(plan, stop.clone()).await.unwrap();
        let id = result.value["id"].as_str().unwrap();
        assert!(result.markings.contains("private"));
        assert!(result.transport.is_none());
        let encrypted = fs::read(root.join("artifacts").join(format!("{id}.enc"))).unwrap();
        assert!(!String::from_utf8_lossy(&encrypted).contains("private answer"));
        let reader = definition("artifact_get", OperationClass::ReadOnly);
        assert!(
            adapter
                .prepare(
                    session.clone(),
                    reader.clone(),
                    json!({"id":id}),
                    "bad-read".into(),
                    context(&session, signer.clone(), ["public".into()].into())
                )
                .await
                .is_err()
        );
        let read = adapter
            .prepare(
                session.clone(),
                reader,
                json!({"id":id}),
                "read".into(),
                context(&session, signer.clone(), ["private".into()].into()),
            )
            .await
            .unwrap();
        assert_eq!(read.inputs.len(), 1);
        assert!(read.inputs[0].markings.contains("private"));
        let body = adapter.run(read, stop.clone()).await.unwrap();
        assert_eq!(
            STANDARD
                .decode(body.value["body"].as_str().unwrap())
                .unwrap(),
            b"private answer"
        );
        let retract = adapter
            .prepare(
                session.clone(),
                definition("artifact_retract", OperationClass::Reversible),
                json!({"id":id,"reason":"superseded"}),
                "retract".into(),
                context(&session, signer.clone(), ["private".into()].into()),
            )
            .await
            .unwrap();
        adapter.run(retract, stop).await.unwrap();
        assert!(
            adapter
                .prepare(
                    session.clone(),
                    definition("artifact_get", OperationClass::ReadOnly),
                    json!({"id":id}),
                    "after".into(),
                    context(&session, signer, ["private".into()].into())
                )
                .await
                .is_err()
        );
        drop(adapter);
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn wasi_plan_freezes_compile_resources_and_owned_process_identity() {
        let (root, adapter, session, signer) = setup();
        let module = r#"(module (import "wasi_snapshot_preview1" "fd_write" (func $write (param i32 i32 i32 i32) (result i32))) (memory (export "memory") 1) (data (i32.const 16) "ok") (func (export "_start") i32.const 0 i32.const 16 i32.store i32.const 4 i32.const 2 i32.store i32.const 1 i32.const 0 i32.const 1 i32.const 8 call $write drop))"#;
        let plan = adapter
            .prepare(
                session.clone(),
                definition("wasi_execute", OperationClass::ReadOnly),
                json!({"module":STANDARD.encode(module),"input":""}),
                "wasi".into(),
                context(&session, signer, ["private".into()].into()),
            )
            .await
            .unwrap();
        assert_eq!(plan.isolation, IsolationTier::W);
        assert!(plan.prices.is_some());
        let manifest = &plan.payload["execution_manifest"];
        assert_eq!(
            manifest["resources"]["memory_bytes"],
            json!(adapter.config.wasi_limits.memory_bytes + 512 * 1024 * 1024)
        );
        assert_eq!(manifest["resources"]["cpu_millicores"], json!(1000));
        let directory = PathBuf::from(manifest["resource_directory"].as_str().unwrap());
        assert_eq!(
            manifest["resource_unit"],
            json!(crate::wasi_worker::unit_for_directory(&directory).unwrap())
        );
        assert!(directory.starts_with(&root));
        drop(adapter);
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn target_reads_enforce_classification_before_contact_and_cancel_unknown_delivery() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (root, mut adapter, mut session, signer) = setup();
        session.isolation = IsolationTier::V;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = TargetEndpoint {
            endpoint: format!("http://{}", listener.local_addr().unwrap()),
            paths: ["/data".into()].into(),
            ca_pem: None,
            timeout_ms: 1000,
            max_response_bytes: 1024,
            markings: ["private".into()].into(),
        };
        Arc::get_mut(&mut adapter.config)
            .unwrap()
            .targets
            .insert("owned-target".into(), endpoint);
        let mut tool = definition("target_read", OperationClass::ReadOnly);
        tool.resource = "owned-target".into();
        assert!(
            adapter
                .prepare(
                    session.clone(),
                    tool.clone(),
                    json!({"path":"/data"}),
                    "denied-target".into(),
                    context(&session, signer.clone(), ["public".into()].into())
                )
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );
        let plan = adapter
            .prepare(
                session.clone(),
                tool.clone(),
                json!({"path":"/data"}),
                "allowed-target".into(),
                context(&session, signer.clone(), ["private".into()].into()),
            )
            .await
            .unwrap();
        let receiver = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            let count = socket.read(&mut bytes).await.unwrap();
            assert!(String::from_utf8_lossy(&bytes[..count]).starts_with("GET /data "));
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nsecret",
                )
                .await
                .unwrap();
        });
        let (_sender, stop) = watch::channel(false);
        let result = adapter.run(plan, stop).await.unwrap();
        receiver.await.unwrap();
        assert_eq!(
            STANDARD
                .decode(result.value["body"].as_str().unwrap())
                .unwrap(),
            b"secret"
        );
        assert!(result.markings.contains("private"));
        assert!(result.transport.as_ref().unwrap().contacted);
        assert_eq!(result.transport.as_ref().unwrap().payload_response_bytes, 6);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        Arc::get_mut(&mut adapter.config)
            .unwrap()
            .targets
            .get_mut("owned-target")
            .unwrap()
            .endpoint = format!("http://{}", listener.local_addr().unwrap());
        let plan = adapter
            .prepare(
                session.clone(),
                tool,
                json!({"path":"/data"}),
                "cancel-target".into(),
                context(&session, signer, ["private".into()].into()),
            )
            .await
            .unwrap();
        let (sender, stop) = watch::channel(false);
        let running = adapter.clone();
        let execution = tokio::spawn(async move { running.run(plan, stop).await });
        let (_connection, _) = listener.accept().await.unwrap();
        sender.send(true).unwrap();
        let failed = tokio::time::timeout(std::time::Duration::from_millis(500), execution)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!failed.completed);
        assert!(!failed.transport.unwrap().contacted);
        assert!(failed.markings.contains("private"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        Arc::get_mut(&mut adapter.config)
            .unwrap()
            .targets
            .get_mut("owned-target")
            .unwrap()
            .endpoint = format!("http://{}", listener.local_addr().unwrap());
        let mut tool = definition("target_read", OperationClass::ReadOnly);
        tool.resource = "owned-target".into();
        let plan = adapter
            .prepare(
                session.clone(),
                tool,
                json!({"path":"/data"}),
                "oversize-target".into(),
                context(
                    &session,
                    Arc::new(CryptoSigner::from_seed([41; 32])),
                    ["private".into()].into(),
                ),
            )
            .await
            .unwrap();
        let receiver = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            let count = connection.read(&mut bytes).await.unwrap();
            assert!(String::from_utf8_lossy(&bytes[..count]).starts_with("GET /data "));
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            connection.write_all(&vec![b'x'; 4096]).await.unwrap();
        });
        let (_sender, stop) = watch::channel(false);
        let failed = adapter.run(plan, stop).await.unwrap();
        receiver.await.unwrap();
        assert!(!failed.completed);
        let measured = failed.transport.unwrap();
        assert!(measured.contacted);
        assert!(measured.payload_response_bytes > 1024);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ca = root.join("oversized-ca.pem");
        fs::write(&ca, vec![b'x'; 65_537]).unwrap();
        let endpoint = Arc::get_mut(&mut adapter.config)
            .unwrap()
            .targets
            .get_mut("owned-target")
            .unwrap();
        endpoint.endpoint = format!("http://{}", listener.local_addr().unwrap());
        endpoint.ca_pem = Some(ca);
        let mut tool = definition("target_read", OperationClass::ReadOnly);
        tool.resource = "owned-target".into();
        let plan = adapter
            .prepare(
                session.clone(),
                tool,
                json!({"path":"/data"}),
                "oversized-ca-target".into(),
                context(
                    &session,
                    Arc::new(CryptoSigner::from_seed([41; 32])),
                    ["private".into()].into(),
                ),
            )
            .await
            .unwrap();
        let (_sender, stop) = watch::channel(false);
        let failed = adapter.run(plan, stop).await.unwrap();
        assert!(!failed.completed);
        assert!(!failed.transport.unwrap().contacted);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
