use super::*;
use crate::node::receiver_tests::Harness;
use crate::operator::{OperatorCommand, OperatorKernel, OperatorRequest};
fn inference(text: &str) -> Value {
    json!({"model":"m1","messages":[{"role":"user","content":text}],"max_tokens":16})
}

// This receiver regression installs a signed held-out job as an explicit test fixture.
// It tests broker authority and lifecycle edges, not protected scoring or runtime qualification.
async fn final_completion_fixture(h: &Harness, elapsed_ms: u64, status: &str) -> String {
    use crate::evaluator::{Candidate, EvaluationAction, EvaluationPhase, EvaluationRequest};
    assert_eq!(
        h.send("drain-claim", "/tool/claim_task", json!({})).await.0,
        200
    );
    assert_eq!(
        h.send("drain-model", "/model", inference("prepare candidate"))
            .await
            .0,
        200
    );
    let (code, finding) = h
        .send(
            "drain-finding",
            "/tool/submit_finding",
            json!({"body":{"fact":"existing proof"}}),
        )
        .await;
    assert_eq!(code, 200, "{finding}");
    let finding_id = finding["id"].as_str().unwrap().to_owned();
    let candidate = json!({"answer":"fixture candidate"});
    let (code, attempt) = h
        .send(
            "drain-attempt",
            "/tool/submit_attempt",
            json!({"payload":candidate,"finding_ids":[finding_id]}),
        )
        .await;
    assert_eq!(code, 200, "{attempt}");
    let state = h.kernel.state().await.unwrap();
    let now = crate::cluster::now_ms();
    let issued_ms = now.saturating_sub(elapsed_ms);
    let request = EvaluationRequest {
        request_id: "test-held-out-job".into(),
        goal_id: "g".into(),
        contract: state.domain.current_contract("g").unwrap(),
        finding_ids: vec![finding_id.clone()],
        state_revision: state.domain.revision,
        issued_ms,
        deadline_ms: issued_ms + 60_000,
        action: EvaluationAction::Candidate {
            attempt_id: attempt["id"].as_str().unwrap().into(),
            candidate_hash: attempt["candidate_hash"].as_str().unwrap().into(),
            candidate: Candidate::Artifact { value: candidate },
            need_ids: vec![],
            phase: EvaluationPhase::HeldOut,
        },
    }
    .sign(&h.fixture.signer)
    .unwrap();
    drop(state);
    h.kernel
        .commit(
            "test-install-final-closure",
            "test-fixture",
            &(elapsed_ms, status),
            vec![],
            |state| {
                state.evaluation_jobs.insert(
                    "g/1/held-out".into(),
                    crate::evaluator_runtime::EvaluationJob {
                        key: "g/1/held-out".into(),
                        request: request.clone(),
                        status: "measured".into(),
                        result: None,
                        observation: None,
                        updated_ms: now + 60_000,
                    },
                );
                state.domain.goals.get_mut("g").unwrap().status = status.into();
                Ok(json!({"scope":"receiver authority fixture only"}))
            },
        )
        .await
        .unwrap()
        .require()
        .unwrap();
    finding_id
}

#[tokio::test]
async fn final_selection_drains_only_existing_task_completion_even_after_goal_completion() {
    let h = Harness::open().await;
    let finding_id = final_completion_fixture(&h, 0, "completed").await;
    assert_eq!(
        h.send("drain-no-model", "/model", inference("closed search"))
            .await
            .0,
        403
    );
    assert_eq!(
        h.send(
            "drain-wrong-result",
            "/tool/complete_task",
            json!({"result_ref":"foreign-finding"})
        )
        .await
        .0,
        403
    );
    let (status, result) = h
        .send(
            "drain-complete",
            "/tool/complete_task",
            json!({"result_ref":finding_id}),
        )
        .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(
        h.send(
            "drain-no-finding",
            "/tool/submit_finding",
            json!({"body":{"fact":"new write"}})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        h.send(
            "drain-no-board",
            "/tool/board_post",
            json!({"topic":"closed","body":{"new":true}})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        h.send(
            "drain-no-attempt",
            "/tool/submit_attempt",
            json!({"payload":{"answer":"new"},"finding_ids":[finding_id]})
        )
        .await
        .0,
        403
    );
    let state = h.kernel.state().await.unwrap();
    assert_eq!(
        state.domain.task("g", "task-1").unwrap().state,
        crate::domain::TaskState::Completed
    );
    assert_eq!(state.domain.goals["g"].status, "completed");
    assert_eq!(state.ledger.accounts["global"].total.spent, 7);
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    assert_eq!(h.upstream.lock().await.len(), 1);
    h.close().await;
}

#[tokio::test]
async fn final_completion_window_cannot_be_refreshed_and_stop_still_wins() {
    let h = Harness::open().await;
    let finding_id = final_completion_fixture(&h, 5_001, "completed").await;
    assert_eq!(
        h.send(
            "drain-too-late",
            "/tool/complete_task",
            json!({"result_ref":finding_id})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        h.kernel
            .state()
            .await
            .unwrap()
            .domain
            .task("g", "task-1")
            .unwrap()
            .state,
        crate::domain::TaskState::Claimed
    );
    h.close().await;

    let h = Harness::open().await;
    let finding_id = final_completion_fixture(&h, 0, "awaiting_evaluation").await;
    h.kernel
        .apply_operator(
            h.fixture.operators[0].clone(),
            OperatorRequest {
                request_id: "stop-during-final-drain".into(),
                operation: OperatorCommand::EngageStop {
                    scope: Scope::Goal {
                        tenant: "t".into(),
                        goal: "g".into(),
                    },
                    reason: "stop wins over final drain".into(),
                },
            },
            &h.fixture.signer,
            crate::cluster::now_ms() / 1000,
        )
        .await
        .unwrap();
    assert_eq!(
        h.send(
            "drain-stopped",
            "/tool/complete_task",
            json!({"result_ref":finding_id})
        )
        .await
        .0,
        403
    );
    assert_eq!(h.upstream.lock().await.len(), 1);
    assert_eq!(
        h.kernel.state().await.unwrap().ledger.accounts["global"]
            .total
            .held,
        0
    );
    h.close().await;
}

#[tokio::test]
async fn final_completion_cannot_use_security_retired_finding() {
    let h = Harness::open().await;
    let finding_id = final_completion_fixture(&h, 0, "completed").await;
    h.kernel
        .commit(
            "test-retire-final-result-source",
            "test-fixture",
            &finding_id,
            vec![],
            |state| {
                let source = state.finding_sources[&finding_id].clone();
                state.security.retraction_epoch += 1;
                let retraction = h.fixture.signer.sign(
                    "finding-retraction",
                    crate::security::Retraction {
                        finding_id: source.clone(),
                        actor: "test-fixture".into(),
                        reason_hash: crate::audit::hash(b"withdrawn during final completion"),
                        timestamp: crate::cluster::now_ms() / 1000,
                        epoch: state.security.retraction_epoch,
                    },
                )?;
                state.security.retractions.insert(source, retraction);
                Ok(json!({"security_source_retired":true}))
            },
        )
        .await
        .unwrap()
        .require()
        .unwrap();
    assert!(
        h.kernel
            .state()
            .await
            .unwrap()
            .domain
            .finding_eligible(&finding_id)
    );
    let (status, error) = h
        .send(
            "drain-retired-result",
            "/tool/complete_task",
            json!({"result_ref":finding_id}),
        )
        .await;
    assert_eq!(status, 403, "{error}");
    let state = h.kernel.state().await.unwrap();
    assert_eq!(
        state.domain.task("g", "task-1").unwrap().state,
        crate::domain::TaskState::Claimed
    );
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    assert_eq!(h.upstream.lock().await.len(), 1);
    h.close().await;
}
