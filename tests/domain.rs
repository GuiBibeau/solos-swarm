use serde_json::{Value, json};
use solos_swarm::domain::*;
use std::collections::{BTreeMap, BTreeSet};

fn actor(id: &str, role: Role) -> Actor {
    Actor {
        id: id.into(),
        role,
    }
}
fn caps(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|v| v.to_string()).collect()
}
fn goal() -> GoalSpec {
    GoalSpec {
        goal_id: "goal".into(),
        intent: "Find and verify an answer".into(),
        version: 1,
        rubric_version: "rubric-1".into(),
        evaluation_set_version: "set-1".into(),
        evaluator_version: "eval-1".into(),
        evaluator_id: "evaluator".into(),
        security: json!({"approved":true}),
        capabilities: caps(&["read", "write", "delegate"]),
        budget_microusd: 1000,
        max_depth: 4,
        max_agents: 10,
        max_open_tasks: 10,
        max_needs: 20,
        expiry_ms: 10000,
        initial_needs: vec![NeedSpec {
            id: "answer".into(),
            parent_id: None,
            description: "A verified answer".into(),
            predicate: json!({"checker":"answer-v1"}),
            version: 1,
            required: true,
            required_capabilities: caps(&["read"]),
        }],
    }
}
fn setup() -> DomainState {
    let mut state = DomainState::new();
    state
        .execute(
            &actor("operator", Role::Operator),
            DomainCommand::RegisterGoal { spec: goal() },
            1,
        )
        .unwrap();
    state
        .execute(
            &actor("operator", Role::Operator),
            DomainCommand::OpenPlanningRound {
                goal_id: "goal".into(),
                round_id: "round-1".into(),
                producer_id: "alice".into(),
                context_hash: hash_json(&"measured context").unwrap(),
                candidate_limit: 8,
            },
            2,
        )
        .unwrap();
    state
}
fn delegate(
    state: &mut DomainState,
    id: &str,
    parent: Option<&str>,
    who: &Actor,
    budget: u64,
    capabilities: BTreeSet<String>,
) -> anyhow::Result<Value> {
    state.execute(
        who,
        DomainCommand::DelegateTask {
            goal_id: "goal".into(),
            spec: TaskSpec {
                id: id.into(),
                need_id: "answer".into(),
                parent_task_id: parent.map(String::from),
                input: json!({"work":id}),
                required_capabilities: caps(&["read"]),
                capabilities,
                budget_microusd: budget,
                expiry_ms: 9000,
            },
            intent_id: format!("delegate-{id}"),
            placement_intent_id: format!("place-{id}"),
            budget_envelope_ref: format!("envelope-{id}"),
        },
        10,
    )
}
fn claim(
    state: &mut DomainState,
    id: &str,
    who: &Actor,
    token: u64,
    now: u64,
) -> anyhow::Result<Value> {
    state.execute(
        who,
        DomainCommand::ClaimTask {
            goal_id: "goal".into(),
            task_id: id.into(),
            incarnation: 1,
            lease_expiry_ms: 5000,
            fencing_token: token,
        },
        now,
    )
}
fn finding(
    state: &mut DomainState,
    task: &str,
    who: &Actor,
    sources: Vec<String>,
    markings: &[&str],
    trust: u8,
    body: Value,
) -> String {
    let value = state
        .execute(
            who,
            DomainCommand::AppendFinding {
                goal_id: "goal".into(),
                task_id: Some(task.into()),
                body,
                provenance: Provenance {
                    producer_id: who.id.clone(),
                    recorder_id: "broker".into(),
                    observed_inputs: vec![InputSource {
                        hash: hash_json(&json!({"input":task})).unwrap(),
                        markings: caps(markings),
                        trust,
                    }],
                    source_refs: sources,
                    signature: "broker-verified-signature".into(),
                },
            },
            100,
        )
        .unwrap();
    value["id"].as_str().unwrap().to_owned()
}

#[test]
fn complete_workflow_decomposition_narrowed_delegation_finding_score_and_retraction() {
    let mut state = setup();
    let operator = actor("operator", Role::Operator);
    let alice = actor("alice", Role::Agent);
    let bob = actor("bob", Role::Agent);
    let evaluator = actor("evaluator", Role::Evaluator);
    let derived = NeedSpec {
        id: "evidence".into(),
        parent_id: Some("answer".into()),
        description: "Check evidence".into(),
        predicate: json!({"checker":"evidence-v1"}),
        version: 1,
        required: true,
        required_capabilities: caps(&["read"]),
    };
    state
        .execute(
            &alice,
            DomainCommand::ProposeNeed {
                goal_id: "goal".into(),
                spec: derived,
            },
            2,
        )
        .unwrap();
    assert!(!state.need("goal", "evidence").unwrap().accepted);
    assert!(
        state
            .execute(
                &alice,
                DomainCommand::AcceptNeed {
                    goal_id: "goal".into(),
                    need_id: "evidence".into(),
                    goal_version: 1,
                    signature: "forged".into()
                },
                3
            )
            .is_err()
    );
    state
        .execute(
            &evaluator,
            DomainCommand::AcceptNeed {
                goal_id: "goal".into(),
                need_id: "evidence".into(),
                goal_version: 1,
                signature: "verified".into(),
            },
            3,
        )
        .unwrap();
    delegate(
        &mut state,
        "parent",
        None,
        &operator,
        1000,
        caps(&["read", "write", "delegate"]),
    )
    .unwrap();
    claim(&mut state, "parent", &alice, 1, 20).unwrap();
    delegate(
        &mut state,
        "child",
        Some("parent"),
        &alice,
        400,
        caps(&["read"]),
    )
    .unwrap();
    claim(&mut state, "child", &bob, 2, 20).unwrap();
    let source = finding(
        &mut state,
        "parent",
        &alice,
        vec![],
        &["private"],
        0,
        json!({"answer":42}),
    );
    let derived = finding(
        &mut state,
        "child",
        &bob,
        vec![source.clone()],
        &[],
        2,
        json!({"corroborated":42}),
    );
    assert_eq!(state.findings[&derived].markings, caps(&["private"]));
    assert_eq!(state.findings[&derived].trust, 0);
    state
        .execute(
            &bob,
            DomainCommand::CompleteTask {
                goal_id: "goal".into(),
                task_id: "child".into(),
                fencing_token: 2,
                result_ref: derived.clone(),
            },
            110,
        )
        .unwrap();
    state
        .execute(
            &alice,
            DomainCommand::SubmitAttempt {
                planning_round_id: "round-1".into(),
                goal_id: "goal".into(),
                attempt_id: "candidate".into(),
                payload: json!({"answer":42}),
                finding_ids: vec![derived.clone()],
            },
            120,
        )
        .unwrap();
    assert_eq!(state.goal("goal").unwrap().status, "awaiting_evaluation");
    let score = Score {
        candidate_hash: state
            .attempt("goal", "candidate")
            .unwrap()
            .candidate_hash
            .clone(),
        contract: state.current_contract("goal").unwrap(),
        value: 1.0,
        dimensions: BTreeMap::from([("correctness".into(), 1.0)]),
        evaluator_id: evaluator.id.clone(),
        signature: "verified-score".into(),
        recorded_ms: 130,
    };
    state
        .execute(
            &evaluator,
            DomainCommand::ScoreAttempt {
                goal_id: "goal".into(),
                attempt_id: "candidate".into(),
                score,
            },
            130,
        )
        .unwrap();
    state
        .execute(
            &operator,
            DomainCommand::BindActionEvidence {
                action: EvidenceAction {
                    id: "write-action".into(),
                    goal_id: "goal".into(),
                    goal_version: 1,
                    finding_ids: vec![derived.clone()],
                    state: "approved".into(),
                },
            },
            131,
        )
        .unwrap();
    assert!(state.action_eligible("write-action", 131));
    for need_id in ["answer", "evidence"] {
        state
            .execute(
                &evaluator,
                DomainCommand::EvaluateNeed {
                    goal_id: "goal".into(),
                    need_id: need_id.into(),
                    satisfied: true,
                    finding_ids: vec![derived.clone()],
                    contract: state.current_contract("goal").unwrap(),
                    signature: "verified-evidence".into(),
                },
                140,
            )
            .unwrap();
    }
    assert_eq!(state.goal("goal").unwrap().status, "completed");
    state
        .execute(
            &operator,
            DomainCommand::RetractFinding {
                goal_id: "goal".into(),
                finding_id: source.clone(),
                reason: "invalid input".into(),
                signature: "verified-retraction".into(),
            },
            150,
        )
        .unwrap();
    assert!(!state.finding_eligible(&derived));
    assert!(state.findings[&source].flagged);
    assert_eq!(state.findings.len(), 2);
    assert_eq!(state.goal("goal").unwrap().status, "active");
    assert!(!state.need("goal", "answer").unwrap().satisfied);
    assert!(state.attempt("goal", "candidate").unwrap().flagged);
    assert_eq!(state.actions["write-action"].state, "reopened");
    assert!(!state.action_eligible("write-action", 151));
    assert!(
        state
            .execute(
                &alice,
                DomainCommand::SubmitAttempt {
                    planning_round_id: "round-1".into(),
                    goal_id: "goal".into(),
                    attempt_id: "repeat".into(),
                    payload: json!({"answer":42}),
                    finding_ids: vec![derived]
                },
                160
            )
            .is_err()
    );
    // The barrier is authoritative even if a resumed descendant walk has not set flags.
    state.findings.get_mut(&source).unwrap().flagged = false;
    assert!(!state.finding_eligible(&source));
}

#[test]
fn authority_budget_depth_claim_and_stale_fencing_fail_atomically() {
    let mut state = setup();
    let operator = actor("operator", Role::Operator);
    let alice = actor("alice", Role::Agent);
    let bob = actor("bob", Role::Agent);
    delegate(
        &mut state,
        "parent",
        None,
        &operator,
        1000,
        caps(&["read", "delegate"]),
    )
    .unwrap();
    claim(&mut state, "parent", &alice, 1, 20).unwrap();
    let before = hash_json(&state).unwrap();
    assert!(
        delegate(
            &mut state,
            "wide",
            Some("parent"),
            &alice,
            100,
            caps(&["read", "write"])
        )
        .is_err()
    );
    assert_eq!(hash_json(&state).unwrap(), before);
    assert!(
        delegate(
            &mut state,
            "overspend",
            Some("parent"),
            &alice,
            1001,
            caps(&["read"])
        )
        .is_err()
    );
    assert!(claim(&mut state, "parent", &bob, 2, 30).is_err());
    state
        .execute(
            &operator,
            DomainCommand::ExpireTask {
                goal_id: "goal".into(),
                task_id: "parent".into(),
            },
            5001,
        )
        .unwrap();
    state
        .execute(
            &bob,
            DomainCommand::ClaimTask {
                goal_id: "goal".into(),
                task_id: "parent".into(),
                incarnation: 2,
                lease_expiry_ms: 8000,
                fencing_token: 2,
            },
            5002,
        )
        .unwrap();
    assert!(
        state
            .execute(
                &alice,
                DomainCommand::StartTask {
                    goal_id: "goal".into(),
                    task_id: "parent".into(),
                    fencing_token: 1
                },
                5003
            )
            .is_err()
    );
    assert_eq!(
        state.task("goal", "parent").unwrap().claimed_by.as_deref(),
        Some("bob")
    );
}

#[test]
fn signed_idempotent_board_replay_cursors_and_stop() {
    let mut state = setup();
    let alice = actor("alice", Role::Agent);
    let post = DomainCommand::AppendBoard {
        goal_id: "goal".into(),
        entry_id: "entry-1".into(),
        topic: "coordination".into(),
        body: json!({"text":"not an authority grant"}),
        signature: "verified-post".into(),
    };
    state.execute(&alice, post.clone(), 5).unwrap();
    state.execute(&alice, post, 6).unwrap();
    assert_eq!(state.boards["goal"].len(), 1);
    let revision = state.revision;
    let read = DomainCommand::ReadBoard {
        goal_id: "goal".into(),
        subscriber_id: "alice".into(),
        after_seq: 0,
        limit: 1,
    };
    assert_eq!(
        state.execute(&alice, read.clone(), 7).unwrap()["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(state.revision, revision);
    state
        .execute(
            &alice,
            DomainCommand::CommitBoardCursor {
                goal_id: "goal".into(),
                subscriber_id: "alice".into(),
                seq: 1,
            },
            8,
        )
        .unwrap();
    assert!(
        state
            .execute(
                &alice,
                DomainCommand::CommitBoardCursor {
                    goal_id: "goal".into(),
                    subscriber_id: "alice".into(),
                    seq: 0
                },
                9
            )
            .is_err()
    );
    state
        .execute(
            &actor("policy", Role::Policy),
            DomainCommand::StopGoal {
                goal_id: "goal".into(),
                reason: "scoped verified policy trigger".into(),
            },
            10,
        )
        .unwrap();
    assert!(
        state
            .execute(
                &alice,
                DomainCommand::AppendBoard {
                    goal_id: "goal".into(),
                    entry_id: "after-stop".into(),
                    topic: "test".into(),
                    body: json!({}),
                    signature: "verified".into()
                },
                11
            )
            .is_err()
    );
    assert!(state.execute(&alice, read, 12).is_ok());
}

#[test]
fn forged_provenance_bad_evaluator_and_contract_version_fail() {
    let mut state = setup();
    let operator = actor("operator", Role::Operator);
    let alice = actor("alice", Role::Agent);
    delegate(&mut state, "task", None, &operator, 1000, caps(&["read"])).unwrap();
    claim(&mut state, "task", &alice, 1, 20).unwrap();
    let provenance = Provenance {
        producer_id: "alice".into(),
        recorder_id: "broker".into(),
        observed_inputs: vec![InputSource {
            hash: "0".repeat(64),
            markings: BTreeSet::new(),
            trust: 0,
        }],
        source_refs: vec![],
        signature: "verified".into(),
    };
    assert!(
        state
            .execute(
                &alice,
                DomainCommand::AppendFinding {
                    goal_id: "goal".into(),
                    task_id: Some("task".into()),
                    body: json!({"provenance":{"trust":"trusted"}}),
                    provenance
                },
                30
            )
            .is_err()
    );
    let source = finding(
        &mut state,
        "task",
        &alice,
        vec![],
        &[],
        0,
        json!({"answer":42}),
    );
    let contract = state.current_contract("goal").unwrap();
    assert!(
        state
            .execute(
                &actor("wrong-evaluator", Role::Evaluator),
                DomainCommand::EvaluateNeed {
                    goal_id: "goal".into(),
                    need_id: "answer".into(),
                    satisfied: true,
                    finding_ids: vec![source.clone()],
                    contract: contract.clone(),
                    signature: "verified".into()
                },
                110
            )
            .is_err()
    );
    let mut new_goal = goal();
    new_goal.version = 2;
    new_goal.initial_needs[0].version = 2;
    new_goal.rubric_version = "rubric-2".into();
    state
        .execute(&operator, DomainCommand::AmendGoal { spec: new_goal }, 120)
        .unwrap();
    assert!(
        state
            .execute(
                &actor("evaluator", Role::Evaluator),
                DomainCommand::EvaluateNeed {
                    goal_id: "goal".into(),
                    need_id: "answer".into(),
                    satisfied: true,
                    finding_ids: vec![source],
                    contract,
                    signature: "verified".into()
                },
                130
            )
            .is_err()
    );
    assert_eq!(state.goal("goal").unwrap().versions.len(), 2);
}

#[test]
fn json_roundtrip_keeps_stops_archives_and_revision() {
    let mut state = setup();
    state
        .execute(
            &actor("operator", Role::Operator),
            DomainCommand::StopGoal {
                goal_id: "goal".into(),
                reason: "maintenance".into(),
            },
            10,
        )
        .unwrap();
    let restored: DomainState =
        serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    assert_eq!(restored.revision, state.revision);
    assert_eq!(restored.goal("goal").unwrap().status, "stopped");
}

#[test]
fn maximum_depth_four_rejects_five_and_cannot_rewiden_a_dropped_grant() {
    let mut state = setup();
    let operator = actor("operator", Role::Operator);
    delegate(
        &mut state,
        "root",
        None,
        &operator,
        1000,
        caps(&["read", "delegate"]),
    )
    .unwrap();
    let mut parent = "root".to_string();
    for depth in 0..=4 {
        let who = actor(&format!("worker-{depth}"), Role::Agent);
        claim(&mut state, &parent, &who, depth + 1, 20).unwrap();
        let child = format!("depth-{}", depth + 1);
        let result = delegate(
            &mut state,
            &child,
            Some(&parent),
            &who,
            1000,
            caps(&["read", "delegate"]),
        );
        if depth == 4 {
            assert!(result.is_err());
        } else {
            result.unwrap();
            parent = child;
        }
    }
    assert_eq!(state.tasks.len(), 5);
}

#[test]
fn planning_rounds_cap_new_candidates_and_resume_retains_evidence_and_fences() {
    let mut state = setup();
    let operator = actor("operator", Role::Operator);
    let alice = actor("alice", Role::Agent);
    let evaluator = actor("evaluator", Role::Evaluator);
    delegate(&mut state, "task", None, &operator, 1000, caps(&["read"])).unwrap();
    claim(&mut state, "task", &alice, 1, 20).unwrap();
    let source = finding(
        &mut state,
        "task",
        &alice,
        vec![],
        &[],
        0,
        json!({"input":"case"}),
    );
    for candidate in 0..8 {
        state
            .execute(
                &alice,
                DomainCommand::SubmitAttempt {
                    goal_id: "goal".into(),
                    attempt_id: format!("candidate-{candidate}"),
                    planning_round_id: "round-1".into(),
                    payload: json!({"candidate":candidate}),
                    finding_ids: vec![source.clone()],
                },
                110 + candidate,
            )
            .unwrap();
        let attempt = state
            .attempt("goal", &format!("candidate-{candidate}"))
            .unwrap();
        let score = Score {
            candidate_hash: attempt.candidate_hash.clone(),
            contract: state.current_contract("goal").unwrap(),
            value: 0.0,
            dimensions: [("correctness".into(), 0.0)].into(),
            evaluator_id: evaluator.id.clone(),
            signature: "verified-evaluator".into(),
            recorded_ms: 120 + candidate,
        };
        state
            .execute(
                &evaluator,
                DomainCommand::ScoreAttempt {
                    goal_id: "goal".into(),
                    attempt_id: format!("candidate-{candidate}"),
                    score,
                },
                120 + candidate,
            )
            .unwrap();
    }
    let before = hash_json(&state).unwrap();
    assert!(
        state
            .execute(
                &alice,
                DomainCommand::SubmitAttempt {
                    goal_id: "goal".into(),
                    attempt_id: "ninth".into(),
                    planning_round_id: "round-1".into(),
                    payload: json!({"candidate":9}),
                    finding_ids: vec![source.clone()]
                },
                130
            )
            .is_err()
    );
    assert_eq!(before, hash_json(&state).unwrap());
    assert_eq!(state.planning_rounds["goal/round-1"].evaluated.len(), 8);
    assert!(
        state
            .execute(
                &alice,
                DomainCommand::OpenPlanningRound {
                    goal_id: "goal".into(),
                    round_id: "forged".into(),
                    producer_id: alice.id.clone(),
                    context_hash: hash_json(&"context").unwrap(),
                    candidate_limit: 8
                },
                140
            )
            .is_err()
    );
    state
        .execute(
            &operator,
            DomainCommand::OpenPlanningRound {
                goal_id: "goal".into(),
                round_id: "round-2".into(),
                producer_id: alice.id.clone(),
                context_hash: hash_json(&"new completed inference").unwrap(),
                candidate_limit: 8,
            },
            140,
        )
        .unwrap();
    state
        .execute(
            &alice,
            DomainCommand::SubmitAttempt {
                goal_id: "goal".into(),
                attempt_id: "next-round".into(),
                planning_round_id: "round-2".into(),
                payload: json!({"candidate":9}),
                finding_ids: vec![source.clone()],
            },
            150,
        )
        .unwrap();
    assert_eq!(state.attempts.len(), 9);
    assert_eq!(
        state.latest_planning_round("goal", "alice").unwrap().id,
        "round-2"
    );
    let round_digest = hash_json(&state.planning_rounds).unwrap();
    let attempts_digest = hash_json(&state.attempts).unwrap();
    let old_fence = state.task("goal", "task").unwrap().fencing_token;
    state
        .execute(
            &operator,
            DomainCommand::StopGoal {
                goal_id: "goal".into(),
                reason: "scoped stop".into(),
            },
            160,
        )
        .unwrap();
    assert!(
        state
            .execute(
                &alice,
                DomainCommand::ResumeGoal {
                    goal_id: "goal".into()
                },
                170
            )
            .is_err()
    );
    state
        .execute(
            &operator,
            DomainCommand::ResumeGoal {
                goal_id: "goal".into(),
            },
            170,
        )
        .unwrap();
    assert_eq!(state.goal("goal").unwrap().spec.version, 1);
    assert_eq!(state.task("goal", "task").unwrap().fencing_token, old_fence);
    assert_eq!(
        state.task("goal", "task").unwrap().state,
        TaskState::Expired
    );
    assert_eq!(attempts_digest, hash_json(&state.attempts).unwrap());
    assert_eq!(round_digest, hash_json(&state.planning_rounds).unwrap());
}

#[test]
fn wrong_candidate_hash_nan_and_cross_contract_scores_are_rejected() {
    let mut state = setup();
    let operator = actor("operator", Role::Operator);
    let alice = actor("alice", Role::Agent);
    let evaluator = actor("evaluator", Role::Evaluator);
    delegate(&mut state, "task", None, &operator, 1000, caps(&["read"])).unwrap();
    claim(&mut state, "task", &alice, 1, 20).unwrap();
    let source = finding(
        &mut state,
        "task",
        &alice,
        vec![],
        &[],
        0,
        json!({"answer":42}),
    );
    state
        .execute(
            &alice,
            DomainCommand::SubmitAttempt {
                planning_round_id: "round-1".into(),
                goal_id: "goal".into(),
                attempt_id: "candidate".into(),
                payload: json!({"answer":42}),
                finding_ids: vec![source],
            },
            110,
        )
        .unwrap();
    let mut score = Score {
        candidate_hash: "wrong".into(),
        contract: state.current_contract("goal").unwrap(),
        value: 1.0,
        dimensions: BTreeMap::new(),
        evaluator_id: evaluator.id.clone(),
        signature: "verified".into(),
        recorded_ms: 120,
    };
    assert!(
        state
            .execute(
                &evaluator,
                DomainCommand::ScoreAttempt {
                    goal_id: "goal".into(),
                    attempt_id: "candidate".into(),
                    score: score.clone()
                },
                120
            )
            .is_err()
    );
    score.candidate_hash = state
        .attempt("goal", "candidate")
        .unwrap()
        .candidate_hash
        .clone();
    score.value = f64::NAN;
    assert!(
        state
            .execute(
                &evaluator,
                DomainCommand::ScoreAttempt {
                    goal_id: "goal".into(),
                    attempt_id: "candidate".into(),
                    score: score.clone()
                },
                120
            )
            .is_err()
    );
    score.value = 1.0;
    score.contract.evaluation_set_version = "different-set".into();
    assert!(
        state
            .execute(
                &evaluator,
                DomainCommand::ScoreAttempt {
                    goal_id: "goal".into(),
                    attempt_id: "candidate".into(),
                    score
                },
                120
            )
            .is_err()
    );
    assert!(
        state
            .attempt("goal", "candidate")
            .unwrap()
            .scores
            .is_empty()
    );
}

#[test]
fn unused_allocation_returns_once_without_reusing_retained_child_allocation() {
    let mut state = setup();
    let operator = actor("operator", Role::Operator);
    let alice = actor("alice", Role::Agent);
    delegate(
        &mut state,
        "parent",
        None,
        &operator,
        1000,
        caps(&["read", "delegate"]),
    )
    .unwrap();
    claim(&mut state, "parent", &alice, 1, 20).unwrap();
    delegate(
        &mut state,
        "child",
        Some("parent"),
        &alice,
        400,
        caps(&["read"]),
    )
    .unwrap();
    let command = DomainCommand::ReturnTaskAllocation {
        goal_id: "goal".into(),
        task_id: "child".into(),
        intent_id: "return-child".into(),
        unused_microusd: 300,
    };
    state.execute(&operator, command.clone(), 30).unwrap();
    state.execute(&operator, command, 31).unwrap();
    assert_eq!(
        state
            .task("goal", "parent")
            .unwrap()
            .allocated_to_children_microusd,
        100
    );
    assert_eq!(state.task("goal", "child").unwrap().returned_microusd, 300);
    assert!(
        state
            .execute(
                &operator,
                DomainCommand::ReturnTaskAllocation {
                    goal_id: "goal".into(),
                    task_id: "child".into(),
                    intent_id: "over-return".into(),
                    unused_microusd: 101
                },
                32
            )
            .is_err()
    );
    delegate(
        &mut state,
        "second-child",
        Some("parent"),
        &alice,
        900,
        caps(&["read"]),
    )
    .unwrap();
    assert_eq!(
        state
            .task("goal", "parent")
            .unwrap()
            .allocated_to_children_microusd,
        1000
    );
}
