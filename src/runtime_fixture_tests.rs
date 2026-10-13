//! Operator-invoked fixture preparation only. It never alters a Kernel or launches a worker.
use super::*;
use crate::{
    domain::{NeedSpec, TaskSpec},
    evaluator::{
        DeliveryTarget, EvaluatorFiles, EvaluatorProcessConfig, ProtectedBundle, ProtectedCase,
        ProtectedGoal,
    },
    evaluator_runtime::{CandidateFormat, EvaluatorSupervisorConfig},
    identity::{TestOidcIssuer, verify_operator_jwt, workload_public_key},
    ledger::{PriceTable, Rate},
    operator::OperatorCommand,
    placement::{DesiredAgent, SwarmSpec},
    planner::{HistoryConfig, HistoryMode},
    provider::Features,
    sandbox::{ProcessLimits, ToolLimits},
    scheduler::{Resources, Strategy},
    security::{
        CapabilityKind, ResourceSelector, SecurityState, TaintExemption, bind_goal_contract,
        test_support,
    },
};
fn put(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic_private(path, &serde_json::to_vec_pretty(value)?)
}
fn dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn identity(directory: &Path, name: &str, issued: &IssuedIdentity) -> Result<IdentityFiles> {
    let files = IdentityFiles {
        workload: issued.identity.clone(),
        certificate_der: directory.join(format!("{name}.der")),
        private_key_der: directory.join(format!("{name}.key.der")),
        ca_der: directory.join("ca.der"),
    };
    persist_identity_files(&files, issued)?;
    atomic_private(&files.ca_der, &issued.ca_der)?;
    atomic_private(
        &directory.join(format!("{name}.pem")),
        issued.certificate_pem.as_bytes(),
    )?;
    atomic_private(
        &directory.join(format!("{name}.key.pem")),
        issued.private_key_pem.as_bytes(),
    )?;
    Ok(files)
}
fn env_path(key: &str) -> Result<PathBuf> {
    let value =
        PathBuf::from(std::env::var(key).with_context(|| format!("missing fixture input {key}"))?);
    ensure!(value.is_absolute(), "fixture input must be absolute");
    Ok(value)
}
fn env_u32(key: &str) -> Result<u32> {
    Ok(std::env::var(key)
        .with_context(|| format!("missing fixture identity {key}"))?
        .parse()?)
}
#[test]
#[ignore = "separate operator-invoked integration fixture; not a production identity or release qualification"]
fn prepare_runtime_fixture() -> Result<()> {
    let actual_local = std::env::var("SOLOS_RUNTIME_SMOKE_ACTUAL_LOCAL").is_ok_and(|v| v == "true");
    let api_model = if actual_local { "qwen3-4b" } else { "m1" };
    let model_resource = format!("provider/local/model/{api_model}");
    let ttl = if actual_local { 1800 } else { 600 };
    // Complete Finding provenance plus the required Task-status step exceed 16k.
    // Both profiles declare 32k; the actual CPU server uses two independent 32k slots.
    let context_tokens = 32768;
    // The complete two-call final response needs 1091 tokens with pretty JSON.
    let output_tokens = if actual_local { 2048 } else { 1024 };
    let root = env_path("SOLOS_RUNTIME_SMOKE_ROOT")?;
    ensure!(!root.exists(), "fixture destination must be fresh");
    let run_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .context("fixture destination needs a UTF-8 run name")?;
    ensure!(
        !run_name.is_empty()
            && run_name.len() <= 40
            && run_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "fixture run name must be bounded and contain only letters, digits and hyphens"
    );
    let deployment_id = format!("isolated-fixture-{run_name}");
    let binary = env_path("SOLOS_RUNTIME_SMOKE_BINARY")?;
    let pi = env_path("SOLOS_RUNTIME_SMOKE_PI_ROOT")?;
    let node = env_path("SOLOS_RUNTIME_SMOKE_NODE")?;
    let cp_uid = env_u32("SOLOS_RUNTIME_SMOKE_CP_UID")?;
    let eval_uid = env_u32("SOLOS_RUNTIME_SMOKE_EVAL_UID")?;
    let eval_gid = env_u32("SOLOS_RUNTIME_SMOKE_EVAL_GID")?;
    ensure!(
        cp_uid > 0 && eval_uid > 0 && cp_uid != eval_uid,
        "fixture protected services must have distinct nonroot identities"
    );
    for name in ["cp", "witness", "evaluator", "issuer", "shared"] {
        dir(&root.join(name))?;
    }
    let cp = root.join("cp");
    let ev = root.join("evaluator");
    let wi = root.join("witness");
    let issuer_dir = root.join("issuer");
    let shared = root.join("shared");
    let mut f = test_support::fixture();
    let now = f.now;
    if actual_local {
        f.policy.payload.expiry = now + 3600;
    }
    let ca = IdentityAuthority::generate("test.local")?;
    let mut issuer = TestOidcIssuer::new(now);
    issuer.config.issuer = "https://localhost:19443".into();
    issuer.config.audience = "isolated-runtime-integration".into();
    f.operators = ["fixture-admin-a", "fixture-admin-b"]
        .iter()
        .map(|id| {
            verify_operator_jwt(
                &issuer.issue(id, "fixture-preparation", now, true),
                &issuer.config,
                &issuer.jwks,
                "fixture-preparation",
                now,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    put(&issuer_dir.join("oidc.json"), &issuer.config)?;
    put(&issuer_dir.join("jwks.json"), &issuer.jwks)?;
    put(&cp.join("oidc.json"), &issuer.config)?;
    put(&cp.join("jwks.json"), &issuer.jwks)?;
    atomic_private(
        &issuer_dir.join("jwt.key.pem"),
        issuer.fixture_private_key_pem().as_bytes(),
    )?;
    for directory in [&cp, &wi, &issuer_dir] {
        let tls_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
        let tls = rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])?
            .self_signed(&tls_key)?;
        atomic_private(&directory.join("tls.der"), tls.der())?;
        atomic_private(&directory.join("tls.key.der"), &tls_key.serialize_der())?;
        atomic_private(&directory.join("tls.pem"), tls.pem().as_bytes())?;
        atomic_private(
            &directory.join("tls.key.pem"),
            tls_key.serialize_pem().as_bytes(),
        )?;
    }
    atomic_private(
        &cp.join("issuer-ca.pem"),
        &fs::read(issuer_dir.join("tls.pem"))?,
    )?;
    atomic_private(&cp.join("witness-ca.pem"), &fs::read(wi.join("tls.pem"))?)?;
    atomic_private(
        &issuer_dir.join("operator-ca.pem"),
        &fs::read(cp.join("tls.pem"))?,
    )?;
    let base = WorkloadId {
        trust_domain: "test.local".into(),
        tenant: "t".into(),
        goal: "g".into(),
        node: "n1".into(),
        agent: "runtime".into(),
        incarnation: "1".into(),
    };
    let service = ca.issue(base.clone(), now, 3600)?;
    let runtime_identity = identity(&cp, "runtime", &service)?;
    let mut cluster_workload = base.clone();
    cluster_workload.agent = "cluster".into();
    let cluster_leaf = ca.issue(cluster_workload, now, 3600)?;
    let cluster_identity = identity(&cp, "cluster", &cluster_leaf)?;
    let mut broker_workload = base.clone();
    broker_workload.agent = "broker".into();
    let broker_leaf = ca.issue(broker_workload, now, 3600)?;
    let broker_identity = identity(&cp, "broker", &broker_leaf)?;
    let mut evaluator_workload = base.clone();
    evaluator_workload.agent = "evaluator".into();
    let evaluator_leaf = ca.issue(evaluator_workload.clone(), now, 3600)?;
    let _ = identity(&ev, "evaluator", &evaluator_leaf)?;
    dir(&ev.join("protected"))?;
    dir(&ev.join("public"))?;
    dir(&ev.join("ipc"))?;
    fs::rename(ev.join("evaluator.key.der"), ev.join("protected/key.der"))?;
    fs::rename(ev.join("evaluator.key.pem"), ev.join("protected/key.pem"))?;
    fs::rename(ev.join("evaluator.der"), ev.join("public/evaluator.der"))?;
    atomic_private(&ev.join("public/ca.der"), ca.ca_der())?;
    atomic_private(&cp.join("ca.pem"), ca.ca_pem().as_bytes())?;
    atomic_private(&cp.join("ca.key.pem"), ca.private_key_pem().as_bytes())?;
    atomic_private(&ev.join("ca.der"), ca.ca_der())?;
    atomic_private(&wi.join("ca.der"), ca.ca_der())?;
    atomic_private(&cp.join("signer.seed"), &[44; 32])?;
    atomic_private(&cp.join("storage.seed"), &[61; 32])?;
    atomic_private(&cp.join("rotation.seed"), &[62; 32])?;
    put(
        &cp.join("rotation.pub"),
        &CryptoSigner::from_seed([62; 32]).public_key(),
    )?;
    put(&cp.join("authority.pub"), &f.signer.public_key())?;
    put(&wi.join("authority.pub"), &f.signer.public_key())?;
    atomic_private(&wi.join("witness.seed"), &[63; 32])?;
    let case =
        crate::scenarios::prepare_case(crate::scenarios::CatalogFamily::RecordReconciliation, 9137);
    put(&issuer_dir.join("public-case.json"), &case.public_value())?;
    f.posture.expiry = now + ttl;
    f.posture.evaluator_public_key = workload_public_key(&evaluator_leaf.certificate_der)?;
    f.posture.intent="Isolated integration fixture: reconcile ten public synthetic record shards; fixture OIDC claims are not production MFA.".into();
    f.posture.max_live_descendants = 11;
    f.posture.max_open_tasks = 11;
    f.posture.max_depth = 2;
    f.posture.resources = [
        "board/g",
        "finding/g",
        "task/g",
        "attempt/g",
        "need/g",
        "evaluate/g",
        model_resource.as_str(),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let operations: BTreeSet<_> = [
        "task_work",
        "task_status",
        "board_read",
        "board_post",
        "finding_append",
        "finding_read",
        "finding_retract",
        "attempt_submit",
        "evaluate",
        "inference",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    f.posture.capability_ceiling.retain(|cap| {
        cap.kind != CapabilityKind::PolicyRequest && cap.kind != CapabilityKind::TaskDelegate
    });
    for cap in &mut f.posture.capability_ceiling {
        cap.resource = ResourceSelector::Any;
        cap.constraints.resources = f.posture.resources.clone();
        cap.constraints.operations = operations.clone();
        cap.constraints.recipients = [model_resource.clone()].into();
        cap.expiry = now + ttl;
        cap.constraints.read_markings.insert("test".into());
        cap.constraints.max_payload_bytes = 262144;
    }
    f.posture.taint_exemptions.push(TaintExemption {
        kind: CapabilityKind::ToolCall,
        resource: ResourceSelector::Exact(model_resource.clone()),
        operations: ["inference".into()].into(),
        source_types: [
            "task_input",
            "model_context",
            "model_result",
            "finding",
            "task_status",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    });
    f.policy.payload.inventory = f.posture.resources.clone();
    f.policy.payload.root_humans = f.operators.iter().map(|op| op.id()).collect();
    f.policy.payload.trusted_components = [base.uri()?, evaluator_workload.uri()?].into();
    f.policy
        .payload
        .recipient_clearances
        .entry(model_resource.clone())
        .or_default()
        .extend(["test".into(), "internal".into()]);
    f.policy = f.signer.sign("deployment-policy", f.policy.payload)?;
    let mut goal = test_support::goal_spec(&f);
    goal.evaluator_id = evaluator_workload.uri()?;
    goal.expiry_ms = (now + ttl) * 1000;
    if actual_local {
        goal.intent = "Compare source_a and source_b for the assigned records. Record exactly unequal rows as submit_finding({body:{inconsistencies:[{id,source_a,source_b}]}}). Save that Finding id, then complete_task using that result_ref. Task-09 is coordinator: first submit its own shard Finding, then read_findings until ten shard Findings are present. Use task_status to wait until task-00 through task-08 are all completed. Merge the ten shard Findings. In one final assistant response, call submit_attempt({payload:{inconsistencies:[...]},finding_ids:[all ten IDs]}) followed by complete_task using its own shard Finding id; do not request another model turn between those final tool calls. Use only the approved tools; do not invent rows or IDs.".into();
    }
    goal.initial_needs = vec![
        NeedSpec {
            id: "answer".into(),
            parent_id: None,
            description: case.objective.clone(),
            predicate: json!({"kind":"catalog-goal"}),
            version: 1,
            required: true,
            required_capabilities: BTreeSet::new(),
        },
        NeedSpec {
            id: "final".into(),
            parent_id: Some("answer".into()),
            description: "independent frozen final acceptance".into(),
            predicate: json!({"kind":"catalog-final"}),
            version: 1,
            required: true,
            required_capabilities: BTreeSet::new(),
        },
    ];
    f.posture.intent = goal.intent.clone();
    bind_goal_contract(&mut f.posture, &goal)?;
    goal.security = serde_json::to_value(&f.posture)?;
    let security = SecurityState::new(f.policy.clone(), &f.signer.public_key(), now)?;
    let approvals = f
        .operators
        .iter()
        .map(|op| {
            security.issue_approval(
                op,
                f.posture.registration_hash()?,
                [0; 32],
                0,
                Scope::Tenant { tenant: "t".into() },
                now + ttl.min(600),
                &f.signer,
                now,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    put(&cp.join("policy.json"), &f.policy)?;
    let history = HistoryConfig {
        mode: HistoryMode::ScoredHistory,
        top_k: if actual_local { 1 } else { 20 },
        new_candidates: if actual_local { 1 } else { 8 },
        context_token_budget: context_tokens,
    };
    let resources = Resources {
        cpu_millicores: 2000,
        memory_bytes: 256 * 1024 * 1024,
        provider_slots: 1,
        rate_tokens: 1,
    };
    let mut agents = vec![];
    for index in 0..10 {
        // The scripted provider prices conservative UTF-8 input bounds at one unit/token.
        // Its coordinator reads all ten proofs; the actual-local fractional price stays unchanged.
        let budget = if index == 9 && !actual_local {
            100_000
        } else {
            50_000
        };
        let id = format!("agent-{index:02}");
        let mut workload = base.clone();
        workload.agent = id.clone();
        let caps = f
            .posture
            .capability_ceiling
            .iter()
            .enumerate()
            .filter(|(_, cap)| {
                !actual_local
                    || matches!(
                        cap.kind,
                        CapabilityKind::ToolCall
                            | CapabilityKind::FindingAppend
                            | CapabilityKind::BudgetSpend
                    )
                    || index == 9 && cap.kind == CapabilityKind::FindingRead
            })
            .map(|(j, cap)| {
                let mut cap = cap.clone();
                cap.id = format!("{id}-cap-{j}");
                cap.holder = workload.uri().unwrap();
                cap.budget.total = budget;
                cap.constraints.operations.remove("task_status");
                if index == 9 && cap.kind == CapabilityKind::ToolCall {
                    cap.constraints.operations.insert("task_status".into());
                }
                if actual_local {
                    cap.constraints.operations = ["task_work", "finding_append", "inference"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect();
                    if index == 9 {
                        cap.constraints
                            .operations
                            .extend(["finding_read".into(), "attempt_submit".into()]);
                        if cap.kind == CapabilityKind::ToolCall {
                            cap.constraints.operations.insert("task_status".into());
                        }
                    }
                }
                cap
            })
            .collect();
        let mut input = case.tasks[index]["input"].clone();
        input["fixture_scope"] = json!(if actual_local {
            "actual pinned Qwen3-4B CPU inference; synthetic OIDC; single-host functional proof only"
        } else {
            "deterministic-provider functional integration, no live-inference release grade"
        });
        if index == 9 && !actual_local {
            input["coordinator_records"] = serde_json::to_value(
                case.tasks
                    .iter()
                    .flat_map(|task| task["input"]["records"].as_array().unwrap().iter().cloned())
                    .collect::<Vec<_>>(),
            )?;
        }
        agents.push(DesiredAgent {
            id: id.clone(),
            capabilities: caps,
            task: TaskSpec {
                id: format!("task-{index:02}"),
                need_id: "answer".into(),
                parent_task_id: None,
                input,
                required_capabilities: BTreeSet::new(),
                capabilities: goal.capabilities.clone(),
                budget_microusd: budget,
                expiry_ms: goal.expiry_ms,
            },
            resources,
            labels: BTreeMap::new(),
            required_capabilities: BTreeSet::new(),
            skill_artifacts: vec![],
            history: history.clone(),
        });
    }
    let mut eval_cap = f
        .posture
        .capability_ceiling
        .iter()
        .find(|cap| cap.kind == CapabilityKind::ToolCall)
        .unwrap()
        .clone();
    eval_cap.id = "evaluator-only".into();
    eval_cap.holder = evaluator_workload.uri()?;
    eval_cap.resource = ResourceSelector::Exact("evaluate/g".into());
    eval_cap.constraints.resources = ["evaluate/g".into()].into();
    eval_cap.constraints.operations = ["evaluate".into()].into();
    eval_cap.budget.total = 1000;
    agents.push(DesiredAgent {
        id: "evaluator".into(),
        capabilities: vec![eval_cap],
        task: TaskSpec {
            id: "evaluator-task".into(),
            need_id: "answer".into(),
            parent_task_id: None,
            input: json!({"service":"protected independent scorer"}),
            required_capabilities: BTreeSet::new(),
            capabilities: goal.capabilities.clone(),
            budget_microusd: 1000,
            expiry_ms: goal.expiry_ms,
        },
        resources: Resources {
            cpu_millicores: 100,
            memory_bytes: 64 * 1024 * 1024,
            provider_slots: 0,
            rate_tokens: 0,
        },
        labels: BTreeMap::new(),
        required_capabilities: BTreeSet::new(),
        skill_artifacts: vec![],
        history: history.clone(),
    });
    let swarm = SwarmSpec {
        id: "fixture-ten-workers".into(),
        tenant: "t".into(),
        goal_id: "g".into(),
        trust_domain: "test.local".into(),
        provider: "local".into(),
        model: api_model.into(),
        strategy: Strategy::Pack,
        lease_ms: 90_000,
        expiry_ms: goal.expiry_ms,
        agents,
    };
    let quota = f.signer.sign(
        "provider-quota",
        crate::quota::QuotaConfig {
            scope: "organization/model".into(),
            providers: vec!["local".into()],
            version: "integration-v1".into(),
            window_ms: 60_000,
            requests: 500,
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            concurrent: 10,
            expiry_ms: goal.expiry_ms,
        },
    )?;
    let quota_approvals = f
        .operators
        .iter()
        .map(|op| {
            security.issue_approval(
                op,
                crate::audit::content_hash(&("configure-quota", &quota, security.grant_epoch))?,
                crate::audit::content_hash(&f.policy)?,
                security.grant_epoch,
                Scope::Global,
                now + ttl.min(600),
                &f.signer,
                now,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    put(
        &issuer_dir.join("commands.json"),
        &vec![
            OperatorCommand::RegisterGoal {
                spec: goal.clone(),
                posture: f.posture.clone(),
                approvals,
            },
            OperatorCommand::ConfigureQuota {
                quota,
                approvals: quota_approvals,
            },
            OperatorCommand::ApplySwarm {
                spec: swarm.clone(),
            },
        ],
    )?;
    let eval_files = EvaluatorFiles {
        bundle: ev.join("protected/bundle.json"),
        certificate: ev.join("public/evaluator.der"),
        private_key: ev.join("protected/key.der"),
        ca: ev.join("public/ca.der"),
        state: ev.join("protected/state/private.json"),
    };
    dir(&ev.join("protected/state"))?;
    let delivery = DeliveryTarget {
        socket: shared.join("broker.sock"),
        endpoint: "https://node-n1".into(),
    };
    let contract = crate::domain::EvaluationContract {
        goal_version: goal.version,
        need_versions: goal
            .initial_needs
            .iter()
            .map(|need| (need.id.clone(), need.version))
            .collect(),
        rubric_version: goal.rubric_version.clone(),
        evaluation_set_version: goal.evaluation_set_version.clone(),
        evaluator_version: goal.evaluator_version.clone(),
    };
    let bundle = ProtectedBundle {
        schema_version: 1,
        broker_public_key: f.signer.public_key(),
        evaluator: evaluator_workload.clone(),
        goals: [(
            "g".into(),
            ProtectedGoal {
                contract,
                need_specs: goal
                    .initial_needs
                    .iter()
                    .map(|need| (need.id.clone(), need.clone()))
                    .collect(),
                search: vec![ProtectedCase::freeze(&case)?],
                held_out: vec![ProtectedCase::freeze(&case)?],
                max_search_evaluations: 20,
            },
        )]
        .into(),
        wasi_limits: ToolLimits {
            fuel: 100_000,
            memory_bytes: 8 * 1024 * 1024,
            output_bytes: 65536,
            deadline_ms: 1000,
        },
        native_limits: ProcessLimits {
            cpu_millicores: 100,
            memory_bytes: 64 * 1024 * 1024,
            pids: 16,
            max_output_bytes: 65536,
        },
        native_policy: None,
        native_work_root: ev.join("protected/native"),
        delivery: Some(delivery.clone()),
    };
    put(&eval_files.bundle, &bundle)?;
    let evaluator = EvaluatorSupervisorConfig {
        process: EvaluatorProcessConfig {
            executable: binary.clone(),
            files: eval_files,
            uid: Some(eval_uid),
            gid: Some(eval_gid),
            max_request_bytes: 1_048_576,
            timeout_ms: 60_000,
            ipc_socket: Some(ev.join("ipc/service.sock")),
        },
        delivery,
        session: Session {
            workload: evaluator_workload,
            grant_id: "evaluator-1".into(),
            envelope_id: "unbound-evaluator-allocation".into(),
            task_id: "evaluator-task".into(),
            allocation_fence: 0,
            task_fence: 0,
            lease_expiry_ms: goal.expiry_ms,
            isolation: IsolationTier::P,
            provider: "local".into(),
            model: api_model.into(),
            history,
            tools: vec![],
        },
        runtime_identity: runtime_identity.clone(),
        interval_ms: 500,
        request_timeout_ms: 60_000,
        candidate_format: CandidateFormat::Artifact,
        candidate_store: None,
        max_jobs_per_goal: 64,
    };
    let tokenizer_path = cp.join("tokenizer.json");
    let template_path = cp.join("template.jinja");
    if actual_local {
        fs::copy(env_path("SOLOS_RUNTIME_SMOKE_TOKENIZER")?, &tokenizer_path)?;
        fs::copy(env_path("SOLOS_RUNTIME_SMOKE_TEMPLATE")?, &template_path)?;
    } else {
        let model = tokenizers::models::wordlevel::WordLevel::builder()
            .vocab([("[UNK]".into(), 0)].into())
            .unk_token("[UNK]".into())
            .build()
            .unwrap();
        tokenizers::Tokenizer::new(model)
            .save(&tokenizer_path, false)
            .unwrap();
        atomic_private(
            &template_path,
            b"{{ messages | tojson }}{{ tools | tojson }}",
        )?;
    }
    let tokenizer = TokenizerSpec {
        model: api_model.into(),
        revision: if actual_local {
            "1cfa9a7208912126459214e8b04321603b3df60c"
        } else {
            "synthetic-fixture-only-v1"
        }
        .into(),
        tokenizer_sha256: crate::kernel::hex(&crate::audit::hash(&fs::read(&tokenizer_path)?)),
        template_sha256: crate::kernel::hex(&crate::audit::hash(&fs::read(&template_path)?)),
        format: if actual_local {
            crate::context::ChatFormat::Qwen3LlamaCppB11429
        } else {
            crate::context::ChatFormat::Qwen3
        },
        context_tokens,
        max_output_tokens: output_tokens,
        server_overhead_tokens: if actual_local { 0 } else { 16 },
        max_request_bytes: 1_048_576,
    };
    let price = f.signer.sign(
        "price-table",
        PriceTable {
            version: if actual_local {
                "local-token-accounting-v1"
            } else {
                "synthetic-fixture-v1"
            }
            .into(),
            provider: "local".into(),
            model: api_model.into(),
            currency: "microusd".into(),
            not_before: now,
            expires: now + ttl,
            rates: ["input_tokens", "cached_input_tokens", "output_tokens"]
                .into_iter()
                .map(|category| {
                    (
                        category.into(),
                        Rate {
                            numerator: 1,
                            denominator: if actual_local { 100 } else { 1 },
                        },
                    )
                })
                .collect(),
        },
    )?;
    put(&cp.join("prices.json"), &price)?;
    let tls_listener = |directory: &Path, port: u16, client_ca: Option<PathBuf>| TlsListener {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        certificate_der: directory.join("tls.der"),
        private_key_der: directory.join("tls.key.der"),
        client_ca_der: client_ca,
        max_connections: 32,
        request_timeout_ms: 60_000,
    };
    let runtime = RuntimeConfig {
        egress: crate::flow::FlowConfig::default(),
        deployment_id,
        node: Node {
            id: "n1".into(),
            capacity: Resources {
                cpu_millicores: 21000,
                memory_bytes: 3 * 1024 * 1024 * 1024,
                provider_slots: 10,
                rate_tokens: 10,
            },
            used: Resources::default(),
            labels: BTreeMap::new(),
            capabilities: BTreeSet::new(),
            healthy: true,
            provider_rate_remaining: 1_000_000,
        },
        cluster: ClusterFiles {
            node_id: 1,
            bind_addr: "127.0.0.1:19446".parse()?,
            nodes: [(
                1,
                NodeAddress {
                    addr: "127.0.0.1:19446".parse()?,
                    server_name: cluster_leaf.server_name.clone(),
                    certificate_sha256: crate::kernel::hex(&crate::audit::hash(
                        &cluster_leaf.certificate_der,
                    )),
                },
            )]
            .into(),
            identity: cluster_identity,
            directory: cp.join("raft"),
            storage_key_file: cp.join("storage.seed"),
            rotation_trust_key_file: Some(cp.join("rotation.pub")),
            rotation_signing_seed_file: Some(cp.join("rotation.seed")),
            offline_admission_file: None,
            initialize_voters: Some([1].into()),
            max_frame_bytes: 32 * 1024 * 1024,
            max_queue_items: 64,
            history_limit: 256,
        },
        runtime_identity,
        signing_seed_file: cp.join("signer.seed"),
        policy_file: cp.join("policy.json"),
        policy_trust_key_file: cp.join("authority.pub"),
        max_state_bytes: 12 * 1024 * 1024,
        operator: Some(tls_listener(&cp, 19444, None)),
        operator_max_request_bytes: 32 * 1024 * 1024,
        oidc_file: Some(cp.join("oidc.json")),
        jwks_file: Some(cp.join("jwks.json")),
        oidc_issuer_ca_pem: Some(cp.join("issuer-ca.pem")),
        providers: vec![ProviderConfig {
            features: Features {
                provider: "local".into(),
                model: api_model.into(),
                revision: if actual_local {
                    "llama-cpp-b11429-qwen3-4b-q4-k-m"
                } else {
                    "deterministic-integration-only"
                }
                .into(),
                modalities: ["text".into()].into(),
                billing_scope: "organization/model".into(),
                idempotent_upstream: false,
                outcome_lookup: false,
                context_tokens: context_tokens as u64,
                output_tokens: output_tokens as u64,
                tools: true,
                streaming: false,
                local: true,
            },
            endpoint: if actual_local {
                "http://127.0.0.1:18471/v1/chat/completions"
            } else {
                "http://127.0.0.1:19447/chat/completions"
            }
            .into(),
            ca_pem: None,
            credential: None,
            price_file: cp.join("prices.json"),
            tokenizer,
            tokenizer_file: tokenizer_path,
            template_file: template_path,
            concurrency: 10,
            max_request_bytes: 1_048_576,
            max_response_bytes: 1_048_576,
        }],
        tools: None,
        evaluator: None,
        evaluators: vec![evaluator],
        workers: WorkerConfig {
            node_binary: node,
            pi_root: pi,
            guard_binary: binary,
            directory: cp.join("workers"),
            ca_pem: cp.join("ca.pem"),
            ca_private_key_pem: cp.join("ca.key.pem"),
            broker_identity,
            allow_degraded_landlock: false,
            identity_ttl_seconds: 900,
            rpc_line_bytes: 1_048_576,
            rpc_output_bytes: 8 * 1024 * 1024,
            pids: 128,
            skills: BTreeMap::new(),
            tool_definitions: vec![],
        },
        reconcile_ms: 1000,
        lease_safety_ms: 20000,
        witness: WitnessClient {
            endpoint: "https://localhost:19445".into(),
            expected_public_key: CryptoSigner::from_seed([63; 32]).public_key(),
            ca_pem: cp.join("witness-ca.pem"),
            client_certificate_pem: cp.join("runtime.pem"),
            client_private_key_pem: cp.join("runtime.key.pem"),
            max_unwitnessed_seconds: 60,
        },
    };
    put(&cp.join("runtime.json"), &runtime)?;
    put(
        &wi.join("witness.json"),
        &WitnessConfig {
            listener: tls_listener(&wi, 19445, Some(wi.join("ca.der"))),
            directory: wi.join("store"),
            log_id: runtime.deployment_id.clone(),
            controlplane_public_key_file: wi.join("authority.pub"),
            witness_seed_file: wi.join("witness.seed"),
            controlplane_uid: cp_uid,
        },
    )?;
    put(
        &root.join("fixture-manifest.json"),
        &json!({"scope":if actual_local { "synthetic OIDC + actual pinned Qwen3-4B CPU inference; single-host functional integration only" } else { "isolated functional integration; synthetic OIDC humans/MFA and deterministic provider; no production identity or live-inference release qualification" },"actual_local":actual_local,"workers":10,"protected_evaluator_uid":eval_uid,"controlplane_uid":cp_uid,"prepared_ms":crate::cluster::now_ms(),"public_case_digest":case.fixture_digest(),"configuration_sha256":crate::kernel::hex(&crate::audit::content_hash(&runtime)?),"runtime_binary":runtime.workers.guard_binary,"evaluator_key":f.posture.evaluator_public_key}),
    )?;
    // Pure typed validation is preparation evidence only; actual admission is performed later through HTTPS Gateway.
    let _: RuntimeConfig = serde_json::from_slice(&fs::read(cp.join("runtime.json"))?)?;
    Ok(())
}
