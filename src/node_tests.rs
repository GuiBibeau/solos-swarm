//! Receiver-level test: real OIDC admission, QUIC transactions, mTLS broker and HTTP upstream.
use super::*;
use crate::{
    audit::{CryptoSigner, hash},
    cluster::{Cluster, ClusterConfig, Mutation, NodeAddress, Transaction},
    context::{ChatFormat, TokenizerSpec},
    domain::{Actor, Role, TaskSpec},
    identity::{IdentityAuthority, IssuedIdentity},
    kernel::KernelState,
    ledger::{IntentState, Rate},
    operator::{OperatorCommand, OperatorKernel, OperatorRequest},
    placement::{DesiredAgent, Reserve, SwarmSpec},
    provider::{Features, OpenAiProvider},
    scheduler::{Node, Resources, Scheduler, Strategy},
    security::{AuthenticatedSubject, SecurityState, bind_goal_contract, test_support},
    transport::SocketConfig,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey, pkcs8::DecodePrivateKey};
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

pub(crate) struct DenyTools;
impl ToolExecutor for DenyTools {
    fn plan(
        &self,
        _: Session,
        _: ToolDefinition,
        _: Value,
        _: String,
        _: crate::tools::ToolContext,
    ) -> crate::tools::ToolPlanFuture {
        Box::pin(async { anyhow::bail!("fixture has no external tools") })
    }
    fn execute(
        &self,
        _: crate::tools::ToolPlan,
        _: watch::Receiver<bool>,
    ) -> crate::tools::ToolExecutionFuture {
        Box::pin(async { anyhow::bail!("fixture has no external tools") })
    }
}
type CapturedTraceHeaders = Arc<Mutex<Vec<(Option<String>, Option<String>)>>>;
pub(crate) struct Harness {
    pub(crate) runtime_subject: AuthenticatedSubject,
    pub(crate) kernel: Arc<Kernel>,
    pub(crate) fixture: test_support::Fixture,
    pub(crate) socket: PathBuf,
    pub(crate) directory: PathBuf,
    pub(crate) client: reqwest::Client,
    pub(crate) provider: Arc<OpenAiProvider>,
    pub(crate) upstream: Arc<Mutex<Vec<Value>>>,
    pub(crate) upstream_trace_headers: CapturedTraceHeaders,
    pub(crate) worker_stop: watch::Sender<bool>,
    pub(crate) server_stop: watch::Sender<bool>,
    pub(crate) server: JoinHandle<Result<()>>,
    pub(crate) receiver: JoinHandle<()>,
    pub(crate) tool_factory: Option<Arc<crate::tool_identity::ScopedToolIdentityFactory>>,
    pub(crate) worker_directory: PathBuf,
}
impl Harness {
    pub(crate) async fn open() -> Self {
        Self::open_with_tools(Arc::new(DenyTools), vec![]).await
    }
    pub(crate) async fn open_with_concurrency(concurrency: usize) -> Self {
        Self::open_configured(Arc::new(DenyTools), vec![], concurrency, None, None, None).await
    }
    pub(crate) async fn open_with_confirmation(
        kind: CapabilityKind,
        tier: crate::security::ConfirmationTier,
    ) -> Self {
        Self::open_configured(
            Arc::new(DenyTools),
            vec![],
            1,
            Some((kind, tier)),
            None,
            None,
        )
        .await
    }
    pub(crate) async fn open_with_tools(
        tools: Arc<dyn ToolExecutor>,
        extra_tools: Vec<ToolDefinition>,
    ) -> Self {
        Self::open_configured(tools, extra_tools, 1, None, None, None).await
    }
    pub(crate) async fn open_with_funding_cap(total: u64) -> Self {
        Self::open_configured(Arc::new(DenyTools), vec![], 1, None, Some(total), None).await
    }
    pub(crate) async fn open_with_quota_concurrency(concurrent: u64) -> Self {
        Self::open_configured(Arc::new(DenyTools), vec![], 1, None, None, Some(concurrent)).await
    }
    async fn open_configured(
        tools: Arc<dyn ToolExecutor>,
        extra_tools: Vec<ToolDefinition>,
        concurrency: usize,
        confirmation: Option<(CapabilityKind, crate::security::ConfirmationTier)>,
        financial_cap: Option<u64>,
        quota_concurrent: Option<u64>,
    ) -> Self {
        let mut f = test_support::fixture();
        if let Some(cap) = financial_cap {
            f.posture.limits.total = cap;
            f.posture.limits.per_call = cap;
            f.posture.limits.per_tick = cap;
            f.posture.limits.per_day = cap;
            for capability in &mut f.posture.capability_ceiling {
                capability.budget = f.posture.limits.clone();
            }
        }
        let ca = IdentityAuthority::generate("test.local").unwrap();
        f.identity = ca.issue(f.definition.holder.clone(), f.now, 900).unwrap();
        f.subject = AuthenticatedSubject::workload(
            &f.identity.certificate_der,
            &f.identity.ca_der,
            f.identity.identity.clone(),
            f.now,
        )
        .unwrap();
        let mut service_id = f.definition.holder.clone();
        service_id.agent = "runtime".into();
        let service = ca.issue(service_id.clone(), f.now, 900).unwrap();
        let executor = AuthenticatedSubject::workload(
            &service.certificate_der,
            &service.ca_der,
            service_id,
            f.now,
        )
        .unwrap();
        f.policy
            .payload
            .trusted_components
            .insert(executor.id().into());
        f.policy
            .payload
            .recipient_clearances
            .get_mut("provider/local/model/m1")
            .unwrap()
            .insert("test".into());
        f.policy.payload.inventory.insert("attempt/g".into());
        f.policy = f
            .signer
            .sign("deployment-policy", f.policy.payload)
            .unwrap();
        f.posture.resources.insert("attempt/g".into());
        for tool in &extra_tools {
            f.policy.payload.inventory.insert(tool.resource.clone());
            f.posture.resources.insert(tool.resource.clone());
            f.policy.payload.classes.insert(tool.class);
            f.posture.classes.insert(tool.class);
            let mut capability = f
                .posture
                .capability_ceiling
                .iter()
                .find(|cap| cap.kind == tool.kind)
                .unwrap()
                .clone();
            capability.id = format!("ceil-extra-{}", tool.name);
            capability.resource = crate::security::ResourceSelector::Exact(tool.resource.clone());
            f.posture.capability_ceiling.push(capability);
        }
        f.policy = f
            .signer
            .sign("deployment-policy", f.policy.payload)
            .unwrap();
        let mut operations: BTreeSet<_> = [
            "task_work",
            "board_read",
            "board_post",
            "finding_append",
            "finding_read",
            "finding_retract",
            "attempt_submit",
            "inference",
            "task_delegate",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        operations.extend(extra_tools.iter().map(|tool| tool.operation.clone()));
        f.posture
            .capability_ceiling
            .retain(|c| c.kind != CapabilityKind::PolicyRequest);
        for c in &mut f.posture.capability_ceiling {
            if let Some((kind, tier)) = confirmation
                && c.kind == kind
            {
                c.confirmation = tier;
            }
            c.constraints.operations = operations.clone();
            c.constraints.resources = f
                .posture
                .resources
                .iter()
                .filter(|resource| c.resource.matches(resource))
                .cloned()
                .collect();
            c.constraints.read_markings.insert("test".into());
            if extra_tools.iter().any(|tool| tool.name == "native_execute") {
                c.constraints.read_markings.insert("sdk-private".into());
            }
        }
        let mut spec = test_support::goal_spec(&f);
        spec.capabilities
            .extend(extra_tools.iter().map(|tool| tool.operation.clone()));
        bind_goal_contract(&mut f.posture, &spec).unwrap();
        spec.security = serde_json::to_value(&f.posture).unwrap();
        let state = SecurityState::new(f.policy.clone(), &f.signer.public_key(), f.now).unwrap();
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
        if confirmation.is_some_and(|(_, tier)| tier == crate::security::ConfirmationTier::C3) {
            f.approvals.push(
                state
                    .issue_approval(
                        &f.operators[1],
                        f.posture.registration_hash().unwrap(),
                        [0; 32],
                        0,
                        Scope::Tenant { tenant: "t".into() },
                        f.now + 600,
                        &f.signer,
                        f.now,
                    )
                    .unwrap(),
            );
        }
        let directory = std::env::temp_dir().join(format!("ss-node-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let address = "127.0.0.1:0".parse().unwrap();
        let cluster = Cluster::open(ClusterConfig {
            node_id: 1,
            bind_addr: address,
            nodes: [(
                1,
                NodeAddress {
                    addr: address,
                    server_name: service.server_name.clone(),
                    certificate_sha256: hex(&hash(&service.certificate_der)),
                },
            )]
            .into(),
            certificate_der: service.certificate_der.clone(),
            private_key_der: service.private_key_der.clone(),
            ca_der: service.ca_der.clone(),
            directory: directory.join("raft"),
            storage_key: [13; 32],
            rotation_trust_key: None,
            max_frame_bytes: 8 * 1024 * 1024,
            max_queue_items: 32,
            history_limit: 128,
        })
        .await
        .unwrap();
        cluster.initialize([1].into()).await.unwrap();
        for _ in 0..100 {
            if cluster.view().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let kernel = Arc::new(Kernel {
            cluster: cluster.clone(),
            signer: Arc::new(CryptoSigner::from_seed([44; 32])),
            max_state_bytes: 32 * 1024 * 1024,
            response_store: None,
        });
        kernel
            .bootstrap(
                KernelState::new(
                    f.policy.clone(),
                    &f.signer.public_key(),
                    f.now,
                    "node-receiver-test".into(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        kernel
            .apply_operator(
                f.operators[0].clone(),
                OperatorRequest {
                    request_id: "register".into(),
                    operation: OperatorCommand::RegisterGoal {
                        spec: spec.clone(),
                        posture: f.posture.clone(),
                        approvals: f.approvals.clone(),
                    },
                },
                &f.signer,
                f.now,
            )
            .await
            .unwrap();
        let quota = f
            .signer
            .sign(
                "provider-quota",
                crate::quota::QuotaConfig {
                    scope: "organization/model".into(),
                    providers: vec!["local".into()],
                    version: "1".into(),
                    window_ms: 60_000,
                    requests: 100,
                    input_tokens: 1_000_000,
                    output_tokens: 10000,
                    concurrent: quota_concurrent.unwrap_or(10),
                    expiry_ms: (f.now + 600) * 1000,
                },
            )
            .unwrap();
        let state = kernel.state().await.unwrap();
        let qhash = content_hash(&("configure-quota", &quota, state.security.grant_epoch)).unwrap();
        let approvals = f
            .operators
            .iter()
            .map(|op| {
                state
                    .security
                    .issue_approval(
                        op,
                        qhash,
                        content_hash(&state.security.policy).unwrap(),
                        state.security.grant_epoch,
                        Scope::Global,
                        f.now + 300,
                        &f.signer,
                        f.now,
                    )
                    .unwrap()
            })
            .collect();
        kernel
            .apply_operator(
                f.operators[0].clone(),
                OperatorRequest {
                    request_id: "quota".into(),
                    operation: OperatorCommand::ConfigureQuota { quota, approvals },
                },
                &f.signer,
                f.now,
            )
            .await
            .unwrap();
        let resources = Resources {
            cpu_millicores: 100,
            memory_bytes: 1024 * 1024,
            provider_slots: 1,
            rate_tokens: 1,
        };
        let capabilities = f
            .posture
            .capability_ceiling
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut c = c.clone();
                c.id = format!("agent-cap-{i}");
                c.holder = f.subject.id().into();
                c
            })
            .collect();
        let swarm = SwarmSpec {
            id: "s".into(),
            tenant: "t".into(),
            goal_id: "g".into(),
            trust_domain: "test.local".into(),
            provider: "local".into(),
            model: "m1".into(),
            strategy: Strategy::Pack,
            lease_ms: 300_000,
            expiry_ms: (f.now + 300) * 1000,
            agents: vec![DesiredAgent {
                id: "a1".into(),
                capabilities,
                task: TaskSpec {
                    id: "task-1".into(),
                    need_id: "answer".into(),
                    parent_task_id: None,
                    input: json!({"assignment":"inspect synthetic evidence"}),
                    required_capabilities: BTreeSet::new(),
                    capabilities: spec.capabilities.clone(),
                    budget_microusd: financial_cap.unwrap_or(900_000),
                    expiry_ms: (f.now + 300) * 1000,
                },
                resources,
                labels: BTreeMap::new(),
                required_capabilities: BTreeSet::new(),
                skill_artifacts: vec![],
                history: HistoryConfig {
                    mode: crate::planner::HistoryMode::ScoredHistory,
                    top_k: 20,
                    new_candidates: 8,
                    context_token_budget: 32768,
                },
            }],
        };
        kernel
            .apply_operator(
                f.operators[0].clone(),
                OperatorRequest {
                    request_id: "swarm".into(),
                    operation: OperatorCommand::ApplySwarm {
                        spec: swarm.clone(),
                    },
                },
                &f.signer,
                f.now,
            )
            .await
            .unwrap();
        let node = Node {
            id: "n1".into(),
            capacity: if extra_tools.is_empty() {
                resources
            } else {
                Resources {
                    cpu_millicores: 4000,
                    memory_bytes: 4 * 1024 * 1024 * 1024,
                    provider_slots: 4,
                    rate_tokens: 100,
                }
            },
            used: Resources::default(),
            labels: BTreeMap::new(),
            capabilities: BTreeSet::new(),
            healthy: true,
            provider_rate_remaining: 100,
        };
        cluster
            .transact(Transaction::new(
                "node",
                vec![
                    Mutation::Put {
                        key: "nodes/n1".into(),
                        value: serde_json::to_value(node).unwrap(),
                    },
                    Mutation::GrantLease {
                        id: "node/n1".into(),
                        holder: "n1".into(),
                        expires_ms: (f.now + 300) * 1000,
                    },
                ],
            ))
            .await
            .unwrap();
        let view = cluster.view().await.unwrap();
        let scheduler = Scheduler::new(10).unwrap();
        let plan = scheduler
            .plan(
                &swarm.gang(view.revision, &BTreeMap::new()).unwrap(),
                &crate::placement::snapshot(&view).unwrap(),
            )
            .unwrap();
        let reserve = Reserve {
            max_state_bytes: kernel.max_state_bytes,
            state: kernel.state().await.unwrap(),
            spec: swarm.clone(),
            operator: Actor {
                id: f.operators[0].id(),
                role: Role::Operator,
            },
            now_ms: crate::cluster::now_ms(),
        };
        assert!(
            scheduler
                .bind(&cluster, &plan, &reserve, swarm.lease_ms)
                .await
                .unwrap()
                .applied
        );
        let allocation = &plan.allocations[0];
        let state = kernel.state().await.unwrap();
        let grant = state.security.grants["a1-1"].clone();
        kernel
            .register_workload_certificate(
                "register-leaf",
                &executor,
                &grant.id,
                &f.identity.certificate_der,
                &f.identity.ca_der,
                f.now,
            )
            .await
            .unwrap()
            .require()
            .unwrap();
        let context = tokenizer(&directory);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/chat/completions", listener.local_addr().unwrap());
        let upstream = Arc::new(Mutex::new(vec![]));
        let upstream_trace_headers = Arc::new(Mutex::new(vec![]));
        let seen = upstream.clone();
        let trace_seen = upstream_trace_headers.clone();
        let receiver = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen.clone();
                let trace_seen = trace_seen.clone();
                tokio::spawn(async move {
                    let mut bytes = vec![];
                    let mut chunk = [0; 4096];
                    let mut end = None;
                    let mut length = 0;
                    loop {
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                        if end.is_none()
                            && let Some(p) = bytes.windows(4).position(|b| b == b"\r\n\r\n")
                        {
                            end = Some(p + 4);
                            length = String::from_utf8_lossy(&bytes[..p])
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap())
                                })
                                .unwrap();
                        }
                        if let Some(p) = end
                            && bytes.len() >= p + length
                        {
                            break;
                        }
                    }
                    let body: Value =
                        serde_json::from_slice(&bytes[end.unwrap()..end.unwrap() + length])
                            .unwrap();
                    let raw_headers = String::from_utf8_lossy(&bytes[..end.unwrap()]);
                    let header = |name: &str| {
                        raw_headers.lines().find_map(|line| {
                            line.split_once(':')
                                .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                                .map(|(_, value)| value.trim().to_owned())
                        })
                    };
                    trace_seen
                        .lock()
                        .await
                        .push((header("traceparent"), header("tracestate")));
                    let lost = body.to_string().contains("LOSE_ACK");
                    let malformed = body
                        .to_string()
                        .contains("__valid_receipt_invalid_choice__");
                    let missing_usage = body.to_string().contains("__missing_receipt_usage__");
                    let delayed = body.to_string().contains("__delayed_receipt__");
                    seen.lock().await.push(body);
                    if lost {
                        return;
                    }
                    if delayed {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    let mut response = json!({"id":format!("receipt-{}",uuid::Uuid::new_v4()),"object":"chat.completion","model":"m1","choices":[{"index":0,"message":{"role":"assistant","content":"synthetic answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":3}});
                    if malformed {
                        response["choices"][0]["message"] = json!("not-an-object");
                    }
                    if missing_usage {
                        response.as_object_mut().unwrap().remove("usage");
                    }
                    let body = serde_json::to_vec(&response).unwrap();
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                });
            }
        });
        let provider = Arc::new(
            OpenAiProvider::new(
                Features {
                    provider: "local".into(),
                    model: "m1".into(),
                    context_tokens: 32768,
                    output_tokens: 1024,
                    modalities: ["text".into()].into(),
                    tools: true,
                    streaming: false,
                    local: true,
                    billing_scope: "organization/model".into(),
                    idempotent_upstream: false,
                    outcome_lookup: false,
                    revision: "fixture-1".into(),
                },
                endpoint,
                None,
                concurrency,
                1_000_000,
                1_000_000,
            )
            .unwrap(),
        );
        let prices = f
            .signer
            .sign(
                "price-table",
                PriceTable {
                    version: "1".into(),
                    provider: "local".into(),
                    model: "m1".into(),
                    currency: "microusd".into(),
                    not_before: f.now,
                    expires: f.now + 600,
                    rates: ["input_tokens", "cached_input_tokens", "output_tokens"]
                        .into_iter()
                        .map(|c| {
                            (
                                c.into(),
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
        let (worker_stop, _) = watch::channel(false);
        let mut session = Session {
            workload: f.identity.identity.clone(),
            grant_id: grant.id.clone(),
            envelope_id: allocation.budget_envelope_ref.clone(),
            task_id: "task-1".into(),
            allocation_fence: allocation.fence,
            task_fence: allocation.fence,
            lease_expiry_ms: swarm.expiry_ms,
            isolation: IsolationTier::P,
            provider: "local".into(),
            model: "m1".into(),
            history: swarm.agents[0].history.clone(),
            tools: make_tools(&grant),
        };
        let has_native_tools = extra_tools
            .iter()
            .any(|tool| tool.name == "native_execute" || tool.name == "wasi_execute");
        session.tools.extend(extra_tools);
        let broker = BrokerNode {
            ca_der: service.ca_der.clone(),
            kernel: kernel.clone(),
            executor_subject: Arc::new(tokio::sync::RwLock::new(executor.clone())),
            sessions: Arc::new(Mutex::new([(f.subject.id().into(), session)].into())),
            backends: [(
                "local/m1".into(),
                Backend {
                    provider: provider.clone(),
                    prices,
                    context: Arc::new(context),
                },
            )]
            .into(),
            stops: Arc::new(Mutex::new(
                [(f.subject.id().into(), worker_stop.clone())].into(),
            )),
            tools,
        };
        // Keep the Unix path below sockaddr_un's platform limit.
        let worker_directory =
            PathBuf::from("/tmp").join(format!("ssw-{}", &uuid::Uuid::new_v4().to_string()[..8]));
        let parent_directory = worker_directory.join(format!(
            "{}-{}",
            &hex(&hash(f.subject.id().as_bytes()))[..24],
            f.identity.identity.incarnation
        ));
        std::fs::create_dir_all(&parent_directory).unwrap();
        let socket = parent_directory.join("broker.sock");
        let tool_factory = if has_native_tools {
            let factory = crate::tool_identity::ScopedToolIdentityFactory::new(
                kernel.clone(),
                Arc::new(ca),
                broker.executor_subject.clone(),
                broker.sessions.clone(),
                broker.stops.clone(),
                worker_directory.clone(),
                directory.join("tool-identities"),
            )
            .unwrap();
            let interface: Arc<dyn crate::tool_identity::ToolIdentityFactory> = factory.clone();
            broker
                .tools
                .attach_tool_factory(Arc::downgrade(&interface))
                .unwrap();
            Some(factory)
        } else {
            None
        };
        let (server_stop, rx) = watch::channel(false);
        let server = tokio::spawn(crate::transport::serve(
            SocketConfig {
                path: socket.clone(),
                certificate_der: service.certificate_der.clone(),
                private_key_der: service.private_key_der.clone(),
                ca_der: service.ca_der.clone(),
                resolver: None,
                max_request_bytes: 1_000_000,
                max_response_bytes: 1_000_000,
                connections: concurrency.saturating_mul(4).max(8),
                request_timeout_ms: 60_000,
            },
            Arc::new(broker),
            rx,
        ));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            assert!(!server.is_finished());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let client = client(&socket, &f.identity);
        Self {
            runtime_subject: executor,
            kernel,
            fixture: f,
            socket,
            directory,
            client,
            provider,
            upstream,
            upstream_trace_headers,
            worker_stop,
            server_stop,
            server,
            receiver,
            tool_factory,
            worker_directory,
        }
    }
    pub(crate) async fn send(&self, id: &str, path: &str, payload: Value) -> (u16, Value) {
        self.send_as(id, path, path, payload, None).await
    }
    pub(crate) async fn send_traced(
        &self,
        id: &str,
        path: &str,
        payload: Value,
        trace: &crate::telemetry::TraceContext,
        tamper_after_signing: bool,
    ) -> (u16, Value) {
        let key = SigningKey::from_pkcs8_der(&self.fixture.identity.private_key_der).unwrap();
        let mut message = Message {
            traceparent: Some(trace.traceparent()),
            tracestate: trace.tracestate.clone(),
            request_id: id.into(),
            issued_ms: crate::cluster::now_ms(),
            path: path.into(),
            payload: serde_json::to_string(&payload).unwrap(),
            signature: String::new(),
        };
        message.signature = STANDARD.encode(key.sign(&message.signing_bytes().unwrap()).to_bytes());
        if tamper_after_signing {
            message.traceparent = Some(trace.child("tampered", id, 1).traceparent());
        }
        let response = self
            .client
            .post(format!("https://node-n1{path}"))
            .headers(message.trace_headers().unwrap())
            .json(&message)
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.json().await.unwrap())
    }
    async fn send_as(
        &self,
        id: &str,
        path: &str,
        signed_path: &str,
        payload: Value,
        wrong_key: Option<SigningKey>,
    ) -> (u16, Value) {
        let key = wrong_key.unwrap_or_else(|| {
            SigningKey::from_pkcs8_der(&self.fixture.identity.private_key_der).unwrap()
        });
        let mut message = Message {
            traceparent: None,
            tracestate: None,
            request_id: id.into(),
            issued_ms: crate::cluster::now_ms(),
            path: signed_path.into(),
            payload: serde_json::to_string(&payload).unwrap(),
            signature: String::new(),
        };
        message.signature = STANDARD.encode(key.sign(&message.signing_bytes().unwrap()).to_bytes());
        let response = self
            .client
            .post(format!("https://node-n1{path}"))
            .headers(message.trace_headers().unwrap())
            .json(&message)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap())
    }
    pub(crate) async fn close(self) {
        if let Some(factory) = &self.tool_factory {
            factory.recover().await.unwrap();
        }
        let _ = self.server_stop.send(true);
        self.server.await.unwrap().unwrap();
        self.receiver.abort();
        self.kernel.cluster.shutdown().await.unwrap();
        std::fs::remove_dir_all(self.directory).unwrap();
        assert!(!self.socket.exists());
        std::fs::remove_dir_all(self.worker_directory).unwrap();
    }
}
fn client(socket: &std::path::Path, id: &IssuedIdentity) -> reqwest::Client {
    reqwest::Client::builder()
        .unix_socket(socket)
        .identity(
            reqwest::Identity::from_pem(
                format!("{}\n{}", id.private_key_pem, id.certificate_pem).as_bytes(),
            )
            .unwrap(),
        )
        .add_root_certificate(reqwest::Certificate::from_pem(id.ca_pem.as_bytes()).unwrap())
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(65))
        .build()
        .unwrap()
}
fn tokenizer(directory: &std::path::Path) -> ContextBuilder {
    use tokenizers::{Tokenizer, models::wordlevel::WordLevel};
    let model = WordLevel::builder()
        .vocab([("[UNK]".into(), 0)].into())
        .unk_token("[UNK]".into())
        .build()
        .unwrap();
    let tokenizer = Tokenizer::new(model);
    let path = directory.join("tokenizer.json");
    tokenizer.save(&path, false).unwrap();
    let template = directory.join("template.jinja");
    std::fs::write(&template, "{{ messages | tojson }}{{ tools | tojson }}").unwrap();
    ContextBuilder::load(
        TokenizerSpec {
            model: "m1".into(),
            revision: "fixture-1".into(),
            tokenizer_sha256: hex(&hash(&std::fs::read(&path).unwrap())),
            template_sha256: hex(&hash(&std::fs::read(&template).unwrap())),
            format: ChatFormat::Qwen3,
            context_tokens: 32768,
            max_output_tokens: 1024,
            server_overhead_tokens: 16,
            max_request_bytes: 1_000_000,
        },
        &path,
        &template,
    )
    .unwrap()
}
fn inference(text: &str) -> Value {
    json!({"model":"m1","messages":[{"role":"user","content":text}],"max_tokens":16})
}
#[tokio::test]
async fn actual_worker_claim_context_receipts_unknown_cancellation_and_stop() {
    let h = Harness::open().await;
    let (status, _) = h.send("before-claim", "/model", inference("hello")).await;
    assert_eq!(status, 403);
    assert!(h.upstream.lock().await.is_empty());
    assert_eq!(
        h.send_as(
            "wrong-signer",
            "/tool/claim_task",
            "/tool/claim_task",
            json!({}),
            Some(SigningKey::from_bytes(&[9; 32]))
        )
        .await
        .0,
        403
    );
    assert_eq!(
        h.send_as("wrong-path", "/tool/claim_task", "/model", json!({}), None)
            .await
            .0,
        403
    );
    assert_eq!(h.send("claim", "/tool/claim_task", json!({})).await.0, 200);
    let (status, completion) = h.send("first", "/model", inference("hello")).await;
    assert_eq!(status, 200, "{completion}");
    assert_eq!(completion["swarm_cost_microusd"], 7);
    let payload = h.upstream.lock().await[0].clone();
    assert!(
        payload["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("test synthetic Goal")
    );
    let before = h.kernel.state().await.unwrap();
    assert_eq!(before.ledger.accounts["global"].total.spent, 7);
    assert_eq!(before.quotas.reservations["model-first"].state, "done");
    assert_eq!(h.send("first", "/model", inference("hello")).await.0, 200);
    assert_eq!(h.upstream.lock().await.len(), 1);
    assert_eq!(
        h.kernel.state().await.unwrap().ledger.accounts["global"]
            .total
            .spent,
        7
    );
    let (status, finding) = h
        .send(
            "finding",
            "/tool/submit_finding",
            json!({"body":{"fact":"synthetic evidence"}}),
        )
        .await;
    assert_eq!(status, 200, "{finding}");
    let finding_id = finding["id"].as_str().unwrap();
    let (status, result) = h
        .send(
            "candidate",
            "/tool/submit_attempt",
            json!({"payload":{"answer":"candidate A"},"finding_ids":[finding_id]}),
        )
        .await;
    assert_eq!(status, 200, "{result}");
    let (status, result) = h.send("history", "/model", inference("compare")).await;
    assert_eq!(status, 200, "{result}");
    assert!(
        h.upstream.lock().await[1]["messages"]
            .to_string()
            .contains("candidate A")
    );
    // Cancellation while waiting for a local provider slot creates no funded intent.
    let (_slot_stop, rx) = watch::channel(false);
    let held_slot = h
        .provider
        .acquire(crate::cluster::now_ms() + 10_000, rx)
        .await
        .unwrap();
    let request = h.send("cancel", "/model", inference("CANCEL_BEFORE_SEND"));
    let cancel = async {
        for _ in 0..200 {
            if h.kernel
                .state()
                .await
                .unwrap()
                .security
                .inputs
                .contains_key("context/model-cancel")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        h.worker_stop.send_replace(true);
    };
    let ((status, result), ()) = tokio::join!(request, cancel);
    assert_eq!(status, 403, "{result}");
    drop(held_slot);
    let state = h.kernel.state().await.unwrap();
    assert!(!state.effects.contains_key("model-cancel"));
    assert!(!state.ledger.intents.contains_key("model-cancel"));
    assert!(!state.quotas.reservations.contains_key("model-cancel"));
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    assert_eq!(h.upstream.lock().await.len(), 2);
    h.worker_stop.send_replace(false);
    let (status, _) = h.send("lost", "/model", inference("LOSE_ACK")).await;
    assert_eq!(status, 403);
    assert_eq!(h.upstream.lock().await.len(), 3);
    let state = h.kernel.state().await.unwrap();
    assert_eq!(
        state.ledger.intents["model-lost"].state,
        IntentState::Unknown
    );
    let held = state.ledger.accounts["global"].total.held;
    assert!(held > 0);
    assert_eq!(state.quotas.reservations["model-lost"].state, "unknown");
    assert_eq!(h.send("lost", "/model", inference("LOSE_ACK")).await.0, 403);
    assert_eq!(h.upstream.lock().await.len(), 3);
    assert_eq!(
        h.kernel.state().await.unwrap().ledger.accounts["global"]
            .total
            .held,
        held
    );
    h.kernel
        .apply_operator(
            h.fixture.operators[0].clone(),
            OperatorRequest {
                request_id: "stop".into(),
                operation: OperatorCommand::EngageStop {
                    scope: Scope::Goal {
                        tenant: "t".into(),
                        goal: "g".into(),
                    },
                    reason: "test stop".into(),
                },
            },
            &h.fixture.signer,
            crate::cluster::now_ms() / 1000,
        )
        .await
        .unwrap();
    assert_eq!(
        h.send("stopped", "/model", inference("must not reach provider"))
            .await
            .0,
        403
    );
    assert_eq!(h.upstream.lock().await.len(), 3);
    assert_eq!(
        h.kernel.state().await.unwrap().ledger.accounts["global"]
            .total
            .held,
        held
    );
    h.close().await;
}

#[tokio::test]
async fn durable_exact_action_review_reserves_only_after_two_verified_humans() {
    let h = Harness::open().await;
    assert_eq!(h.send("claim", "/tool/claim_task", json!({})).await.0, 200);
    let view = h.kernel.cluster.view().await.unwrap();
    let lease = &view.leases["allocation/a1"];
    let checks = vec![crate::cluster::LeaseCheck {
        id: "allocation/a1".into(),
        holder: "n1".into(),
        fence: lease.fence,
    }];
    let task = crate::kernel::TaskContext {
        id: "task-1".into(),
        incarnation: 1,
        fence: lease.fence,
    };
    h.kernel
        .observe_task_input(
            "record-task-input",
            &h.fixture.subject,
            "a1-1",
            task.clone(),
            checks.clone(),
        )
        .await
        .unwrap()
        .require()
        .unwrap();
    let state = h.kernel.state().await.unwrap();
    let grant = &state.security.grants["a1-1"];
    let cap = grant
        .capabilities
        .iter()
        .find(|c| c.kind == CapabilityKind::ToolCall)
        .unwrap();
    let budget = grant
        .capabilities
        .iter()
        .find(|c| c.kind == CapabilityKind::BudgetSpend)
        .unwrap();
    let payload = json!({"exact_destination":"synthetic-action","amount":1});
    let request = ActionRequest {
        intent_id: "reviewed-action".into(),
        grant_id: grant.id.clone(),
        grant_version: grant.version,
        capability_id: cap.id.clone(),
        budget_capability_id: Some(budget.id.clone()),
        envelope_id: Some(
            state
                .domain
                .task("g", "task-1")
                .unwrap()
                .budget_envelope_ref
                .clone(),
        ),
        usage_bound: [
            ("input_tokens".into(), 10),
            ("cached_input_tokens".into(), 10),
            ("output_tokens".into(), 10),
        ]
        .into(),
        action: Action {
            kind: CapabilityKind::ToolCall,
            resource: "task/g".into(),
            operation: "task_work".into(),
            arguments: payload.clone(),
            class: OperationClass::External,
            isolation: IsolationTier::P,
            recipient: Some("provider/local/model/m1".into()),
            evidence: BTreeSet::new(),
        },
    };
    let prices = h
        .fixture
        .signer
        .sign(
            "price-table",
            PriceTable {
                version: "1".into(),
                provider: "local".into(),
                model: "m1".into(),
                currency: "microusd".into(),
                not_before: h.fixture.now,
                expires: h.fixture.now + 600,
                rates: ["input_tokens", "cached_input_tokens", "output_tokens"]
                    .into_iter()
                    .map(|c| {
                        (
                            c.into(),
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
    let admission = || crate::kernel::Admission {
        request: request.clone(),
        payload: payload.clone(),
        prices: Some(prices.clone()),
        approvals: vec![],
        leases: checks.clone(),
        quota: None,
        task: Some(task.clone()),
    };
    let first = h
        .kernel
        .admit("review-admit", &h.fixture.subject, admission())
        .await
        .unwrap()
        .require()
        .unwrap();
    assert_eq!(first["status"], "confirmation_required");
    let review: crate::security::ConfirmationReview =
        serde_json::from_value(first["review"].clone()).unwrap();
    assert_eq!(review.required_humans, 2);
    let state = h.kernel.state().await.unwrap();
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    assert!(!state.effects.contains_key("reviewed-action"));
    assert!(state.pending_actions.contains_key("reviewed-action"));
    let approve = |id: &str| OperatorRequest {
        request_id: id.into(),
        operation: OperatorCommand::Approve {
            action_hash: review.action_hash,
            evidence_hash: review.evidence_hash,
            grant_version: review.grant_version,
            scope: review.scope.clone(),
            expiry: review.expiry.min(h.fixture.now + 300),
        },
    };
    h.kernel
        .apply_operator(
            h.fixture.operators[0].clone(),
            approve("approval-a"),
            &h.fixture.signer,
            h.fixture.now,
        )
        .await
        .unwrap();
    assert_eq!(
        h.kernel
            .admit("review-admit", &h.fixture.subject, admission())
            .await
            .unwrap()
            .require()
            .unwrap()["status"],
        "confirmation_required"
    );
    h.kernel
        .apply_operator(
            h.fixture.operators[0].clone(),
            approve("approval-a-again"),
            &h.fixture.signer,
            h.fixture.now,
        )
        .await
        .unwrap();
    assert_eq!(
        h.kernel
            .admit("review-admit", &h.fixture.subject, admission())
            .await
            .unwrap()
            .require()
            .unwrap()["status"],
        "confirmation_required"
    );
    assert_eq!(
        h.kernel.state().await.unwrap().ledger.accounts["global"]
            .total
            .held,
        0
    );
    h.kernel
        .apply_operator(
            h.fixture.operators[1].clone(),
            approve("approval-b"),
            &h.fixture.signer,
            h.fixture.now,
        )
        .await
        .unwrap();
    h.kernel
        .admit("review-admit", &h.fixture.subject, admission())
        .await
        .unwrap()
        .require()
        .unwrap();
    let state = h.kernel.state().await.unwrap();
    assert_eq!(state.ledger.accounts["global"].total.held, 30);
    assert!(!state.pending_actions.contains_key("reviewed-action"));
    assert_eq!(state.effects["reviewed-action"].state, "pending");
    assert!(
        h.kernel
            .admit("review-admit", &h.fixture.subject, admission())
            .await
            .unwrap()
            .duplicate
    );
    assert_eq!(
        h.kernel.state().await.unwrap().ledger.accounts["global"]
            .total
            .held,
        30
    );
    h.kernel
        .apply_operator(
            h.fixture.operators[0].clone(),
            OperatorRequest {
                request_id: "stop-before-reviewed-dispatch".into(),
                operation: OperatorCommand::EngageStop {
                    scope: Scope::Goal {
                        tenant: "t".into(),
                        goal: "g".into(),
                    },
                    reason: "stop between approval and effect".into(),
                },
            },
            &h.fixture.signer,
            h.fixture.now,
        )
        .await
        .unwrap();
    assert!(
        !h.kernel
            .dispatch(
                "review-dispatch",
                &h.fixture.subject,
                "reviewed-action",
                checks
            )
            .await
            .unwrap()
            .ok
    );
    assert!(h.upstream.lock().await.is_empty());
    h.close().await;
}

#[tokio::test]
async fn fresh_verified_root_login_and_stop_use_control_headroom() {
    let h = Harness::open().await;
    let size = serde_json::to_vec(&h.kernel.state().await.unwrap())
        .unwrap()
        .len();
    let saturated = Kernel {
        cluster: h.kernel.cluster.clone(),
        signer: h.kernel.signer.clone(),
        max_state_bytes: size * 6 / 5,
        response_store: None,
    };
    let now = crate::cluster::now_ms() / 1000;
    assert!(
        saturated
            .audit_authentication(
                "rejected-login",
                None,
                false,
                [3; 32],
                &h.fixture.signer,
                now
            )
            .await
            .is_err()
    );
    saturated
        .audit_control_authentication(
            "root-login",
            &h.fixture.operators[0],
            [4; 32],
            &h.fixture.signer,
            now,
        )
        .await
        .unwrap();
    saturated
        .apply_operator(
            h.fixture.operators[0].clone(),
            OperatorRequest {
                request_id: "saturated-stop".into(),
                operation: OperatorCommand::EngageStop {
                    scope: Scope::Goal {
                        tenant: "t".into(),
                        goal: "g".into(),
                    },
                    reason: "bounded state recovery test".into(),
                },
            },
            &h.fixture.signer,
            now,
        )
        .await
        .unwrap();
    assert!(
        h.kernel
            .state()
            .await
            .unwrap()
            .security
            .stopped(&Scope::Goal {
                tenant: "t".into(),
                goal: "g".into()
            })
    );
    h.close().await;
}

#[tokio::test]
async fn formatted_context_replay_retains_digest_and_exact_durable_request() {
    let h = Harness::open().await;
    assert_eq!(
        h.send("compact-claim", "/tool/claim_task", json!({}))
            .await
            .0,
        200
    );
    let text = "frozen-context-marker ".repeat(200);
    let (status, completion) = h.send("compact-context", "/model", inference(&text)).await;
    assert_eq!(status, 200, "{completion}");
    let state = h.kernel.state().await.unwrap();
    let effect = &state.effects["model-compact-context"];
    let context = &effect.payload["context"];
    assert_eq!(effect.payload, effect.request.action.arguments);
    let replay = &state.replies["context-compact-context"].request;
    assert_eq!(
        replay[3],
        json!(crate::audit::content_hash(&effect.payload).unwrap())
    );
    assert!(serde_json::to_vec(replay).unwrap().len() < 4096);
    assert!(context.to_string().contains("frozen-context-marker"));
    assert!(
        state
            .replies
            .values()
            .all(|reply| !reply.value.to_string().contains("frozen-context-marker"))
    );
    let before_contacts = h.upstream.lock().await.len();
    assert_eq!(
        h.send("compact-context", "/model", inference(&text))
            .await
            .0,
        200
    );
    assert_eq!(h.upstream.lock().await.len(), before_contacts);
    assert_eq!(state.ledger.accounts["global"].total.spent, 7);
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    h.close().await;
}
