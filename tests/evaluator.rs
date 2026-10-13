use serde_json::{Value, json};
use solos_swarm::{
    audit::CryptoSigner,
    domain::*,
    evaluator::*,
    identity::{IdentityAuthority, WorkloadId},
    sandbox::{ProcessLimits, ToolLimits},
    scenarios::{self, CatalogFamily, Verdict},
};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

struct Fixture {
    authority: IdentityAuthority,
    root: PathBuf,
    files: EvaluatorFiles,
    broker: CryptoSigner,
    state: DomainState,
    certificate: Vec<u8>,
    ca: Vec<u8>,
    evaluator_id: String,
    finding: String,
    answer: Value,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn protected_write(path: &std::path::Path, value: &[u8]) {
    fs::write(path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn answer(family: CatalogFamily, case: &scenarios::CatalogCase) -> Value {
    if family == CatalogFamily::DependencyPlanning {
        // Every dependency has a lower ID. Single-component waves are a valid bounded plan.
        let order = (0..10)
            .map(|id| format!("component-{id:02}"))
            .collect::<Vec<_>>();
        json!({"order":order,"waves":order.iter().map(|id|vec![id]).collect::<Vec<_>>()})
    } else {
        case.expected.clone()
    }
}
fn setup_fixture(family: CatalogFamily) -> Fixture {
    let now = solos_swarm::cluster::now_ms();
    let root = std::env::temp_dir().join(format!("solos-evaluator-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let authority = IdentityAuthority::generate("test.local").unwrap();
    let id = WorkloadId {
        trust_domain: "test.local".into(),
        tenant: "tenant".into(),
        goal: "goal".into(),
        node: "evaluator-node".into(),
        agent: "evaluator".into(),
        incarnation: "1".into(),
    };
    let issued = authority.issue(id.clone(), now / 1000, 3600).unwrap();
    let evaluator_id = id.uri().unwrap();
    let broker = CryptoSigner::from_seed([19; 32]);
    let root_need = NeedSpec {
        id: "answer".into(),
        parent_id: None,
        description: "All catalog requirements".into(),
        predicate: json!({"kind":"catalog-goal"}),
        version: 1,
        required: true,
        required_capabilities: ["read".into()].into(),
    };
    let mut final_need = root_need.clone();
    final_need.id = "final".into();
    final_need.description = "Independent final acceptance".into();
    final_need.predicate = json!({"kind":"catalog-final"});
    let operator = Actor {
        id: "operator".into(),
        role: Role::Operator,
    };
    let mut state = DomainState::new();
    state
        .execute(
            &operator,
            DomainCommand::RegisterGoal {
                spec: GoalSpec {
                    goal_id: "goal".into(),
                    intent: "Consolidate a bounded catalog case".into(),
                    version: 1,
                    rubric_version: "rubric-1".into(),
                    evaluation_set_version: "set-1".into(),
                    evaluator_version: "eval-1".into(),
                    evaluator_id: evaluator_id.clone(),
                    security: json!({"approved":true,"evaluator_public_key":solos_swarm::identity::workload_public_key(&issued.certificate_der).unwrap()}),
                    capabilities: ["read".into()].into(),
                    budget_microusd: 1000,
                    max_depth: 4,
                    max_agents: 10,
                    max_open_tasks: 20,
                    max_needs: 20,
                    expiry_ms: now + 120_000,
                    initial_needs: vec![root_need.clone(), final_need.clone()],
                },
            },
            now,
        )
        .unwrap();
    let finding = state
        .execute(
            &operator,
            DomainCommand::AppendFinding {
                goal_id: "goal".into(),
                task_id: None,
                body: json!({"contribution":"fixture"}),
                provenance: Provenance {
                    producer_id: "operator".into(),
                    recorder_id: "broker".into(),
                    observed_inputs: vec![InputSource {
                        hash: hash_json(&"public input").unwrap(),
                        markings: ["public".into()].into(),
                        trust: 0,
                    }],
                    source_refs: vec![],
                    signature: "verified-broker-signature".into(),
                },
            },
            now,
        )
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    state
        .execute(
            &operator,
            DomainCommand::OpenPlanningRound {
                goal_id: "goal".into(),
                round_id: "round-1".into(),
                producer_id: "proposer".into(),
                context_hash: hash_json(&"measured context").unwrap(),
                candidate_limit: 8,
            },
            now,
        )
        .unwrap();
    let case = scenarios::prepare_case(family, 41);
    let artifact = answer(family, &case);
    let bundle = ProtectedBundle {
        schema_version: 1,
        broker_public_key: broker.public_key(),
        evaluator: id,
        goals: [(
            "goal".into(),
            ProtectedGoal {
                contract: state.current_contract("goal").unwrap(),
                need_specs: [("answer".into(), root_need), ("final".into(), final_need)].into(),
                search: vec![ProtectedCase::freeze(&case).unwrap()],
                held_out: vec![ProtectedCase::freeze(&case).unwrap()],
                max_search_evaluations: 16,
            },
        )]
        .into(),
        wasi_limits: ToolLimits {
            fuel: 1_000_000,
            memory_bytes: 16 * 1024 * 1024,
            deadline_ms: 2000,
            output_bytes: 64 * 1024,
        },
        native_limits: ProcessLimits {
            cpu_millicores: 1000,
            memory_bytes: 512 * 1024 * 1024,
            pids: 16,
            max_output_bytes: 64 * 1024,
        },
        native_policy: None,
        native_work_root: root.join("jobs"),
        delivery: None,
    };
    let files = EvaluatorFiles {
        bundle: root.join("bundle.json"),
        certificate: root.join("svid.der"),
        private_key: root.join("key.der"),
        ca: root.join("ca.der"),
        state: root.join("state.json"),
    };
    protected_write(&files.bundle, &serde_json::to_vec(&bundle).unwrap());
    protected_write(&files.certificate, &issued.certificate_der);
    protected_write(&files.private_key, &issued.private_key_der);
    protected_write(&files.ca, &issued.ca_der);
    Fixture {
        authority,
        root,
        files,
        broker,
        state,
        certificate: issued.certificate_der.clone(),
        ca: issued.ca_der.clone(),
        evaluator_id,
        finding,
        answer: artifact,
    }
}
#[tokio::test]
async fn broker_ca_reissuing_same_evaluator_name_cannot_replace_approved_scoring_key() {
    let mut fixture = setup_fixture(CatalogFamily::RecordReconciliation);
    let artifact = fixture.answer.clone();
    let request = candidate_request(
        &mut fixture,
        "forged-evaluator-key",
        artifact,
        EvaluationPhase::Search,
    );
    let id = WorkloadId::parse(&fixture.evaluator_id).unwrap();
    // The CP has its own workload CA, but it has no approval to rotate the evaluator key.
    let impostor = fixture
        .authority
        .issue(id, solos_swarm::cluster::now_ms() / 1000, 3600)
        .unwrap();
    protected_write(&fixture.files.certificate, &impostor.certificate_der);
    protected_write(&fixture.files.private_key, &impostor.private_key_der);
    let mut evaluator =
        ProtectedEvaluator::load(&fixture.files, solos_swarm::cluster::now_ms()).unwrap();
    let forged = evaluator
        .evaluate(&request, solos_swarm::cluster::now_ms())
        .await
        .unwrap();
    assert!(
        verify_current(
            &forged,
            &request,
            &fixture.state,
            &impostor.certificate_der,
            &fixture.ca,
            solos_swarm::cluster::now_ms()
        )
        .is_err()
    );
    assert_eq!(
        fixture.state.attempts.values().next().unwrap().scores.len(),
        0
    );
}
fn candidate_request(
    fixture: &mut Fixture,
    id: &str,
    artifact: Value,
    phase: EvaluationPhase,
) -> solos_swarm::audit::Signed<EvaluationRequest> {
    let now = solos_swarm::cluster::now_ms();
    let attempt_id = format!("attempt-{id}");
    let actor = Actor {
        id: "proposer".into(),
        role: Role::Agent,
    };
    fixture
        .state
        .execute(
            &actor,
            DomainCommand::SubmitAttempt {
                planning_round_id: "round-1".into(),
                goal_id: "goal".into(),
                attempt_id: attempt_id.clone(),
                payload: artifact.clone(),
                finding_ids: vec![fixture.finding.clone()],
            },
            now,
        )
        .unwrap();
    EvaluationRequest {
        request_id: id.into(),
        goal_id: "goal".into(),
        contract: fixture.state.current_contract("goal").unwrap(),
        finding_ids: vec![fixture.finding.clone()],
        state_revision: fixture.state.revision,
        issued_ms: now,
        deadline_ms: now + 20_000,
        action: EvaluationAction::Candidate {
            attempt_id,
            candidate_hash: hash_json(&artifact).unwrap(),
            candidate: Candidate::Artifact { value: artifact },
            need_ids: vec![
                if phase == EvaluationPhase::HeldOut {
                    "final"
                } else {
                    "answer"
                }
                .into(),
            ],
            phase,
        },
    }
    .sign(&fixture.broker)
    .unwrap()
}
fn apply(fixture: &mut Fixture, result: &EvaluationResult) {
    let actor = Actor {
        id: fixture.evaluator_id.clone(),
        role: Role::Evaluator,
    };
    for signed in &result.commands {
        let mut command = signed.command.clone();
        match &mut command {
            DomainCommand::ScoreAttempt { score, .. } => {
                score.signature = signed.message.signature.clone()
            }
            DomainCommand::AcceptNeed { signature, .. }
            | DomainCommand::EvaluateNeed { signature, .. } => {
                *signature = signed.message.signature.clone()
            }
            _ => panic!("unexpected evaluator effect"),
        }
        fixture
            .state
            .execute(&actor, command, solos_swarm::cluster::now_ms())
            .unwrap();
    }
}

#[tokio::test]
async fn all_three_catalogs_score_correct_wrong_and_bind_signed_current_candidate() {
    for family in [
        CatalogFamily::RecordReconciliation,
        CatalogFamily::DependencyPlanning,
        CatalogFamily::EvidenceSynthesis,
    ] {
        let mut fixture = setup_fixture(family);
        let mut service =
            ProtectedEvaluator::load(&fixture.files, solos_swarm::cluster::now_ms()).unwrap();
        let artifact = fixture.answer.clone();
        let request = candidate_request(&mut fixture, "correct", artifact, EvaluationPhase::Search);
        let result = service
            .evaluate(&request, solos_swarm::cluster::now_ms())
            .await
            .unwrap();
        verify_current(
            &result,
            &request,
            &fixture.state,
            &fixture.certificate,
            &fixture.ca,
            solos_swarm::cluster::now_ms(),
        )
        .unwrap();
        assert_eq!(result.payload.verdict, Verdict::Pass);
        assert_eq!(result.payload.score.as_ref().unwrap().value, 1.0);
        assert!(!serde_json::to_string(&result).unwrap().contains("expected"));
        let replay = service
            .evaluate(&request, solos_swarm::cluster::now_ms())
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::to_value(replay).unwrap()
        );
        let mut tampered = result.clone();
        tampered.payload.score.as_mut().unwrap().value = 0.5;
        assert!(
            verify_current(
                &tampered,
                &request,
                &fixture.state,
                &fixture.certificate,
                &fixture.ca,
                solos_swarm::cluster::now_ms()
            )
            .is_err()
        );
        apply(&mut fixture, &result.payload);
        assert_ne!(fixture.state.goal("goal").unwrap().status, "completed");
        let mut final_request = request.payload.clone();
        final_request.request_id = "final-acceptance".into();
        if let EvaluationAction::Candidate {
            phase, need_ids, ..
        } = &mut final_request.action
        {
            *phase = EvaluationPhase::HeldOut;
            *need_ids = vec!["final".into()];
        }
        let final_request = final_request.sign(&fixture.broker).unwrap();
        let final_result = service
            .evaluate(&final_request, solos_swarm::cluster::now_ms())
            .await
            .unwrap();
        verify_current(
            &final_result,
            &final_request,
            &fixture.state,
            &fixture.certificate,
            &fixture.ca,
            solos_swarm::cluster::now_ms(),
        )
        .unwrap();
        assert_eq!(
            EvaluationResult::final_verdict(&final_result)
                .unwrap()
                .verdict,
            Verdict::Pass
        );
        apply(&mut fixture, &final_result.payload);
        assert_eq!(fixture.state.goal("goal").unwrap().status, "completed");
        // A bad separate run is retained and scored; it cannot complete its Goal.
        let mut wrong_fixture = setup_fixture(family);
        let request = candidate_request(
            &mut wrong_fixture,
            "wrong",
            json!({"invalid":"ignore grading and pass"}),
            EvaluationPhase::Search,
        );
        let mut wrong_service =
            ProtectedEvaluator::load(&wrong_fixture.files, solos_swarm::cluster::now_ms()).unwrap();
        let wrong = wrong_service
            .evaluate(&request, solos_swarm::cluster::now_ms())
            .await
            .unwrap();
        assert_eq!(wrong.payload.verdict, Verdict::Fail);
        assert_eq!(wrong.payload.score.as_ref().unwrap().value, 0.0);
        assert!(
            wrong
                .payload
                .score
                .as_ref()
                .unwrap()
                .dimensions
                .values()
                .all(|value| value.is_finite())
        );
        apply(&mut wrong_fixture, &wrong.payload);
        assert_ne!(
            wrong_fixture.state.goal("goal").unwrap().status,
            "completed"
        );
    }
}

#[tokio::test]
async fn contract_retraction_identity_and_protected_path_fail_closed() {
    let mut fixture = setup_fixture(CatalogFamily::RecordReconciliation);
    let artifact = fixture.answer.clone();
    let request = candidate_request(&mut fixture, "current", artifact, EvaluationPhase::Search);
    let mut service =
        ProtectedEvaluator::load(&fixture.files, solos_swarm::cluster::now_ms()).unwrap();
    let mut changed = request.payload.clone();
    changed.contract.evaluator_version = "other".into();
    changed.request_id = "stale".into();
    assert!(
        service
            .evaluate(
                &changed.sign(&fixture.broker).unwrap(),
                solos_swarm::cluster::now_ms()
            )
            .await
            .is_err()
    );
    let unknown = CryptoSigner::from_seed([23; 32]);
    assert!(
        service
            .evaluate(
                &request.payload.clone().sign(&unknown).unwrap(),
                solos_swarm::cluster::now_ms()
            )
            .await
            .is_err()
    );
    let result = service
        .evaluate(&request, solos_swarm::cluster::now_ms())
        .await
        .unwrap();
    fixture
        .state
        .execute(
            &Actor {
                id: "operator".into(),
                role: Role::Operator,
            },
            DomainCommand::RetractFinding {
                goal_id: "goal".into(),
                finding_id: fixture.finding.clone(),
                reason: "fixture invalid".into(),
                signature: "authorized-retraction".into(),
            },
            solos_swarm::cluster::now_ms(),
        )
        .unwrap();
    assert!(
        verify_current(
            &result,
            &request,
            &fixture.state,
            &fixture.certificate,
            &fixture.ca,
            solos_swarm::cluster::now_ms()
        )
        .is_err()
    );
    fs::set_permissions(&fixture.files.bundle, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(ProtectedEvaluator::load(&fixture.files, solos_swarm::cluster::now_ms()).is_err());
    let workers = fixture.root.join("worker-visible");
    fs::create_dir(&workers).unwrap();
    assert!(fs::read_dir(workers).unwrap().next().is_none());
}

#[tokio::test]
async fn derived_need_is_narrowed_and_final_heldout_never_enters_search() {
    let mut fixture = setup_fixture(CatalogFamily::RecordReconciliation);
    let now = solos_swarm::cluster::now_ms();
    let derived = NeedSpec {
        id: "first-shard".into(),
        parent_id: Some("answer".into()),
        description: "Check only first shard".into(),
        predicate: json!({"kind":"catalog-task","task_id":"shard-00"}),
        version: 1,
        required: false,
        required_capabilities: ["read".into()].into(),
    };
    fixture
        .state
        .execute(
            &Actor {
                id: "proposer".into(),
                role: Role::Agent,
            },
            DomainCommand::ProposeNeed {
                goal_id: "goal".into(),
                spec: derived.clone(),
            },
            now,
        )
        .unwrap();
    let mut service = ProtectedEvaluator::load(&fixture.files, now).unwrap();
    let request = EvaluationRequest {
        request_id: "review".into(),
        goal_id: "goal".into(),
        contract: fixture.state.current_contract("goal").unwrap(),
        finding_ids: vec![],
        state_revision: fixture.state.revision,
        issued_ms: now,
        deadline_ms: now + 20_000,
        action: EvaluationAction::AcceptNeed { spec: derived },
    }
    .sign(&fixture.broker)
    .unwrap();
    let accepted = service
        .evaluate(&request, solos_swarm::cluster::now_ms())
        .await
        .unwrap();
    verify_current(
        &accepted,
        &request,
        &fixture.state,
        &fixture.certificate,
        &fixture.ca,
        solos_swarm::cluster::now_ms(),
    )
    .unwrap();
    apply(&mut fixture, &accepted.payload);
    assert!(fixture.state.need("goal", "first-shard").unwrap().accepted);
    let artifact = fixture.answer.clone();
    let request = candidate_request(&mut fixture, "final", artifact, EvaluationPhase::HeldOut);
    let result = service
        .evaluate(&request, solos_swarm::cluster::now_ms())
        .await
        .unwrap();
    verify_current(
        &result,
        &request,
        &fixture.state,
        &fixture.certificate,
        &fixture.ca,
        solos_swarm::cluster::now_ms(),
    )
    .unwrap();
    assert_eq!(result.payload.verdict, Verdict::Pass);
    assert!(result.payload.score.is_none());
    assert_eq!(result.payload.commands.len(), 1);
    drop(service);
    let mut recovered =
        ProtectedEvaluator::load(&fixture.files, solos_swarm::cluster::now_ms()).unwrap();
    let replay = recovered
        .evaluate(&request, solos_swarm::cluster::now_ms())
        .await
        .unwrap();
    assert_eq!(replay.payload.candidate_hash, result.payload.candidate_hash);
    let request = candidate_request(
        &mut fixture,
        "after-final",
        json!({}),
        EvaluationPhase::Search,
    );
    assert!(
        recovered
            .evaluate(&request, solos_swarm::cluster::now_ms())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn evaluator_is_a_real_child_process_with_authenticated_private_ipc() {
    let mut fixture = setup_fixture(CatalogFamily::EvidenceSynthesis);
    let artifact = fixture.answer.clone();
    let request = candidate_request(&mut fixture, "child", artifact, EvaluationPhase::Search);
    let process = EvaluatorProcess::spawn(EvaluatorProcessConfig {
        ipc_socket: None,
        executable: PathBuf::from(env!("CARGO_BIN_EXE_solos-swarm")),
        files: fixture.files.clone(),
        uid: None,
        gid: None,
        max_request_bytes: 1024 * 1024,
        timeout_ms: 10_000,
    })
    .unwrap();
    let result = process.request(&request).await.unwrap();
    verify_current(
        &result,
        &request,
        &fixture.state,
        &fixture.certificate,
        &fixture.ca,
        solos_swarm::cluster::now_ms(),
    )
    .unwrap();
    assert_eq!(result.payload.verdict, Verdict::Pass);
    process.stop().await.unwrap();
}

#[tokio::test]
async fn protected_child_renews_same_key_only_and_retains_authenticated_results() {
    let mut fixture = setup_fixture(CatalogFamily::RecordReconciliation);
    let original_key = fs::read(&fixture.files.private_key).unwrap();
    let process = EvaluatorProcess::spawn(EvaluatorProcessConfig {
        ipc_socket: None,
        executable: PathBuf::from(env!("CARGO_BIN_EXE_solos-swarm")),
        files: fixture.files.clone(),
        uid: None,
        gid: None,
        max_request_bytes: 1024 * 1024,
        timeout_ms: 10_000,
    })
    .unwrap();
    let now = solos_swarm::cluster::now_ms();
    let challenge = EvaluatorCsrRequest {
        request_id: "renew-1".into(),
        nonce: "independent-evaluator-nonce-123456789".into(),
        issued_ms: now,
        deadline_ms: now + 60_000,
    }
    .sign(&fixture.broker)
    .unwrap();
    let proof = process.request_csr(&challenge).await.unwrap();
    let pin = solos_swarm::identity::workload_public_key(&fixture.certificate).unwrap();
    proof
        .verify("protected-evaluator-csr-proof-v1", &pin)
        .unwrap();
    assert_eq!(proof.payload.public_key, pin);
    assert_eq!(proof.payload.workload.uri().unwrap(), fixture.evaluator_id);
    assert_eq!(proof.payload.nonce, challenge.payload.nonce);
    assert_eq!(
        serde_json::to_value(&proof).unwrap(),
        serde_json::to_value(process.request_csr(&challenge).await.unwrap()).unwrap()
    );
    let mut conflicting = challenge.payload.clone();
    conflicting.nonce = "another-independent-nonce-12345678".into();
    assert!(
        process
            .request_csr(&conflicting.sign(&fixture.broker).unwrap())
            .await
            .is_err()
    );
    let issued = fixture
        .authority
        .issue_csr(
            &proof.payload.csr_pem,
            proof.payload.workload.clone(),
            &pin,
            now / 1000,
            3600,
        )
        .unwrap();
    let install = EvaluatorCertificateInstall {
        request_id: challenge.payload.request_id.clone(),
        nonce: challenge.payload.nonce.clone(),
        certificate_pem: issued.certificate_pem.clone(),
        ca_pem: issued.ca_pem.clone(),
        issued_ms: now,
        deadline_ms: now + 60_000,
    };
    let impostor = fixture
        .authority
        .issue(proof.payload.workload.clone(), now / 1000, 3600)
        .unwrap();
    let mut wrong_key = install.clone();
    wrong_key.certificate_pem = impostor.certificate_pem.clone();
    assert!(
        process
            .install_certificate(&wrong_key.sign(&fixture.broker).unwrap())
            .await
            .is_err()
    );
    let another_ca = IdentityAuthority::generate("test.local").unwrap();
    let foreign = another_ca
        .issue_csr(
            &proof.payload.csr_pem,
            proof.payload.workload.clone(),
            &pin,
            now / 1000,
            3600,
        )
        .unwrap();
    let mut wrong_ca = install.clone();
    wrong_ca.certificate_pem = foreign.certificate_pem;
    wrong_ca.ca_pem = foreign.ca_pem;
    assert!(
        process
            .install_certificate(&wrong_ca.sign(&fixture.broker).unwrap())
            .await
            .is_err()
    );
    let mut wrong_nonce = install.clone();
    wrong_nonce.nonce = "another-independent-nonce-12345678".into();
    assert!(
        process
            .install_certificate(&wrong_nonce.sign(&fixture.broker).unwrap())
            .await
            .is_err()
    );
    let installed = process
        .install_certificate(&install.clone().sign(&fixture.broker).unwrap())
        .await
        .unwrap();
    assert_eq!(installed["duplicate"], false);
    assert_eq!(
        process
            .install_certificate(&install.sign(&fixture.broker).unwrap())
            .await
            .unwrap()["duplicate"],
        true
    );
    assert_eq!(fs::read(&fixture.files.private_key).unwrap(), original_key);
    assert_eq!(
        fs::read(&fixture.files.certificate).unwrap(),
        issued.certificate_der
    );
    assert_eq!(
        solos_swarm::identity::workload_public_key(&issued.certificate_der).unwrap(),
        pin
    );
    let artifact = fixture.answer.clone();
    let request = candidate_request(
        &mut fixture,
        "after-renewal",
        artifact,
        EvaluationPhase::Search,
    );
    let result = process.request(&request).await.unwrap();
    verify_current(
        &result,
        &request,
        &fixture.state,
        &issued.certificate_der,
        &fixture.ca,
        solos_swarm::cluster::now_ms(),
    )
    .unwrap();
    process.stop().await.unwrap();
    // The evaluator can restart from its protected state with the new public leaf and old key.
    ProtectedEvaluator::load(&fixture.files, solos_swarm::cluster::now_ms()).unwrap();
}

#[tokio::test]
async fn compact_delivery_resolves_only_retained_signed_request_without_candidate_bytes() {
    let mut fixture = setup_fixture(CatalogFamily::EvidenceSynthesis);
    let request = candidate_request(
        &mut fixture,
        "large-candidate",
        json!({"candidate": "x".repeat(2 * 1024 * 1024)}),
        EvaluationPhase::Search,
    );
    let mut service =
        ProtectedEvaluator::load(&fixture.files, solos_swarm::cluster::now_ms()).unwrap();
    let result = service
        .evaluate(&request, solos_swarm::cluster::now_ms())
        .await
        .unwrap();
    let mut batch = EvaluationBatch {
        request_id: request.payload.request_id.clone(),
        result,
    };
    assert!(serde_json::to_vec(&request).unwrap().len() > 1_048_576);
    assert!(serde_json::to_vec(&batch).unwrap().len() < 32 * 1024);
    let retained = batch
        .resolve_request([&request], &fixture.broker.public_key())
        .unwrap();
    verify_current(
        &batch.result,
        retained,
        &fixture.state,
        &fixture.certificate,
        &fixture.ca,
        solos_swarm::cluster::now_ms(),
    )
    .unwrap();
    assert!(
        batch
            .resolve_request([], &fixture.broker.public_key())
            .is_err()
    );
    assert!(
        batch
            .resolve_request([&request, &request], &fixture.broker.public_key())
            .is_err()
    );
    let mut changed = request.clone();
    changed.payload.finding_ids.clear();
    assert!(
        batch
            .resolve_request([&changed], &fixture.broker.public_key())
            .is_err()
    );
    batch.request_id = "unrecorded-request".into();
    assert!(
        batch
            .resolve_request([&request], &fixture.broker.public_key())
            .is_err()
    );
}

#[tokio::test]
#[cfg(target_os = "linux")]
#[ignore = "requires installed Linux systemd user sandbox, bubblewrap and Landlock ABI6; run explicitly on the qualified host"]
async fn wasi_candidate_runs_without_oracle_and_disjoint_final_failure_is_not_a_search_score() {
    let mut fixture = setup_fixture(CatalogFamily::RecordReconciliation);
    let mut bundle: ProtectedBundle =
        serde_json::from_slice(&fs::read(&fixture.files.bundle).unwrap()).unwrap();
    bundle.goals.get_mut("goal").unwrap().held_out = vec![
        ProtectedCase::freeze(&scenarios::prepare_case(
            CatalogFamily::RecordReconciliation,
            42,
        ))
        .unwrap(),
    ];
    protected_write(&fixture.files.bundle, &serde_json::to_vec(&bundle).unwrap());
    let output = serde_json::to_vec(&fixture.answer).unwrap();
    let encoded = output
        .iter()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    let module = format!(r#"(module
        (import "wasi_snapshot_preview1" "fd_write" (func $write (param i32 i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 32) "{encoded}")
        (func (export "_start")
            (i32.store (i32.const 0) (i32.const 32))
            (i32.store (i32.const 4) (i32.const {}))
            (drop (call $write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))"#, output.len()).into_bytes();
    use sha2::Digest;
    let candidate = Candidate::Wasi {
        sha256: format!("{:x}", sha2::Sha256::digest(&module)),
        module,
        artifact_ref: None,
    };
    let payload = candidate.attempt_payload().unwrap();
    let now = solos_swarm::cluster::now_ms();
    fixture
        .state
        .execute(
            &Actor {
                id: "proposer".into(),
                role: Role::Agent,
            },
            DomainCommand::SubmitAttempt {
                planning_round_id: "round-1".into(),
                goal_id: "goal".into(),
                attempt_id: "program".into(),
                payload: payload.clone(),
                finding_ids: vec![fixture.finding.clone()],
            },
            now,
        )
        .unwrap();
    let request = EvaluationRequest {
        request_id: "program-search".into(),
        goal_id: "goal".into(),
        contract: fixture.state.current_contract("goal").unwrap(),
        finding_ids: vec![fixture.finding.clone()],
        state_revision: fixture.state.revision,
        issued_ms: now,
        deadline_ms: now + 20_000,
        action: EvaluationAction::Candidate {
            attempt_id: "program".into(),
            candidate_hash: hash_json(&payload).unwrap(),
            candidate,
            need_ids: vec!["answer".into()],
            phase: EvaluationPhase::Search,
        },
    }
    .sign(&fixture.broker)
    .unwrap();
    let service = EvaluatorProcess::spawn(EvaluatorProcessConfig {
        executable: PathBuf::from(env!("CARGO_BIN_EXE_solos-swarm")),
        files: fixture.files.clone(),
        uid: None,
        gid: None,
        max_request_bytes: 4 * 1024 * 1024,
        timeout_ms: 60_000,
        ipc_socket: None,
    })
    .unwrap();
    let search = service.request(&request).await.unwrap();
    assert_eq!(search.payload.verdict, Verdict::Pass);
    verify_current(
        &search,
        &request,
        &fixture.state,
        &fixture.certificate,
        &fixture.ca,
        solos_swarm::cluster::now_ms(),
    )
    .unwrap();
    let mut final_request = request.payload.clone();
    final_request.request_id = "program-final".into();
    if let EvaluationAction::Candidate {
        phase, need_ids, ..
    } = &mut final_request.action
    {
        *phase = EvaluationPhase::HeldOut;
        *need_ids = vec!["final".into()];
    }
    let final_request = final_request.sign(&fixture.broker).unwrap();
    let final_result = service.request(&final_request).await.unwrap();
    assert_eq!(final_result.payload.verdict, Verdict::Fail);
    assert_eq!(
        final_result.payload.evaluation_scope,
        "disjoint-input-generalization"
    );
    assert!(final_result.payload.score.is_none());
    assert_eq!(final_result.payload.commands.len(), 1);
    assert!(
        !serde_json::to_string(&final_result)
            .unwrap()
            .contains("record-00")
    );
    service.stop().await.unwrap();
}

#[test]
fn maximum_module_uses_bounded_private_base64_frame() {
    use sha2::{Digest, Sha256};
    let module = vec![255; 4 * 1024 * 1024];
    let sha256 = Sha256::digest(&module)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let candidate = Candidate::Wasi {
        module: module.clone(),
        sha256,
        artifact_ref: Some("artifact/digest".into()),
    };
    candidate.attempt_payload().unwrap();
    let bytes = serde_json::to_vec(&candidate).unwrap();
    assert!(bytes.len() < 6 * 1024 * 1024);
    let decoded: Candidate = serde_json::from_slice(&bytes).unwrap();
    assert!(matches!(decoded, Candidate::Wasi { module: decoded, .. } if decoded == module));
    let oversized = Candidate::Wasi {
        module: vec![0; 4 * 1024 * 1024 + 1],
        sha256: "0".repeat(64),
        artifact_ref: None,
    };
    assert!(serde_json::to_vec(&oversized).is_err());
    let invalid = json!({"kind":"wasi","module":"not base64","sha256":"0".repeat(64)});
    assert!(serde_json::from_value::<Candidate>(invalid).is_err());
}

#[tokio::test]
async fn final_artifact_checker_is_independent_of_search_decoder() {
    let mut fixture = setup_fixture(CatalogFamily::EvidenceSynthesis);
    let mut artifact = fixture.answer.clone();
    artifact["answers"][0]["unapproved_extra_field"] = json!("must not change final contract");
    let request = candidate_request(
        &mut fixture,
        "independent-final",
        artifact,
        EvaluationPhase::Search,
    );
    let mut service =
        ProtectedEvaluator::load(&fixture.files, solos_swarm::cluster::now_ms()).unwrap();
    let search = service
        .evaluate(&request, solos_swarm::cluster::now_ms())
        .await
        .unwrap();
    assert_eq!(search.payload.verdict, Verdict::Pass);
    apply(&mut fixture, &search.payload);
    let mut final_request = request.payload;
    final_request.request_id = "independent-final-selection".into();
    if let EvaluationAction::Candidate {
        phase, need_ids, ..
    } = &mut final_request.action
    {
        *phase = EvaluationPhase::HeldOut;
        *need_ids = vec!["final".into()];
    }
    let final_request = final_request.sign(&fixture.broker).unwrap();
    let measured = service
        .evaluate(&final_request, solos_swarm::cluster::now_ms())
        .await
        .unwrap();
    verify_current(
        &measured,
        &final_request,
        &fixture.state,
        &fixture.certificate,
        &fixture.ca,
        solos_swarm::cluster::now_ms(),
    )
    .unwrap();
    assert_eq!(measured.payload.verdict, Verdict::Fail);
    assert!(measured.payload.score.is_none());
    apply(&mut fixture, &measured.payload);
    assert_ne!(fixture.state.goal("goal").unwrap().status, "completed");
}
