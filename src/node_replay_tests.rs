//! Regression checks through the real OIDC/QUIC/mTLS broker and independent HTTP receiver.
use super::receiver_tests::Harness;
use crate::ledger::IntentState;
use serde_json::{Value, json};

fn inference(text: &str) -> Value {
    json!({"model":"m1","messages":[{"role":"user","content":text}],"max_tokens":16})
}

async fn advance_authority(h: &Harness, suffix: &str) -> u64 {
    let state = h.kernel.state().await.unwrap();
    let original = &state.security.grants["a1-1"];
    let mut holder = original.holder.clone();
    holder.agent = format!("epoch-{suffix}");
    let uri = holder.uri().unwrap();
    let capabilities = original
        .capabilities
        .iter()
        .enumerate()
        .map(|(index, cap)| {
            let mut cap = cap.clone();
            cap.id = format!("epoch-{suffix}-{index}");
            cap.holder = uri.clone();
            cap
        })
        .collect();
    let definition = crate::security::AgentDefinition {
        id: format!("epoch-{suffix}"),
        holder,
        goal_id: original.goal_id.clone(),
        grant_version: original.version,
        capabilities,
        lease: original.lease.clone(),
    };
    h.kernel
        .define_agent(
            &format!("define-epoch-{suffix}"),
            &crate::security::AuthenticatedSubject::operator(&h.fixture.operators[0]),
            definition,
            None,
            crate::ledger::Envelope {
                id: format!("epoch-envelope-{suffix}"),
                placement_intent_id: format!("epoch-placement-{suffix}"),
                placement_hash: crate::audit::content_hash(&("epoch-placement", suffix)).unwrap(),
                parent: None,
                goal_id: "g".into(),
                holder: uri,
                accounts: ["global".into(), "tenant/t".into(), "goal/g".into()].into(),
                allocation: 1,
                spent: 0,
                held: 0,
                active: true,
            },
        )
        .await
        .unwrap()
        .require()
        .unwrap();
    let next = h.kernel.state().await.unwrap();
    assert!(next.security.grant_epoch > state.security.grant_epoch);
    next.security.grant_epoch
}

#[tokio::test]
async fn typed_authority_phase_retry_preserves_one_model_intent_contact_and_fee() {
    let h = Harness::open().await;
    assert_eq!(
        h.send("phase-claim", "/tool/claim_task", json!({})).await.0,
        200
    );
    let id = format!("phase-model-{}", uuid::Uuid::new_v4().simple());
    let intent = format!("model-{id}");
    let gate = crate::kernel::admission_sync::pause_once(intent.clone());
    let payload = inference("one exact paid model action across authority churn");
    let model = h.send(&id, "/model", payload.clone());
    let churn = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.wait_reviewed())
            .await
            .unwrap();
        let before = h.kernel.state().await.unwrap();
        assert!(!before.effects.contains_key(&intent));
        assert!(!before.ledger.intents.contains_key(&intent));
        assert!(!before.quotas.reservations.contains_key(&intent));
        assert_eq!(before.ledger.accounts["global"].total.held, 0);
        assert!(h.upstream.lock().await.is_empty());
        advance_authority(&h, &uuid::Uuid::new_v4().simple().to_string()).await;
        gate.release();
    };
    let ((status, completion), ()) = tokio::join!(model, churn);
    assert_eq!(status, 200, "{completion}");
    let state = h.kernel.state().await.unwrap();
    assert_eq!(
        state
            .replies
            .values()
            .filter(|reply| reply.ok
                && reply.value["status"] == "authority_changed"
                && reply.value["intent"] == intent)
            .count(),
        1
    );
    assert_eq!(state.effects[&intent].request.intent_id, intent);
    assert_eq!(state.effects[&intent].state, "done");
    assert_eq!(state.ledger.intents[&intent].state, IntentState::Settled);
    assert_eq!(state.ledger.accounts["global"].total.spent, 7);
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    assert_eq!(state.quotas.reservations[&intent].state, "done");
    assert_eq!(h.upstream.lock().await.len(), 1);
    assert_eq!(h.send(&id, "/model", payload).await.0, 200);
    assert_eq!(h.upstream.lock().await.len(), 1);
    assert_eq!(
        h.kernel.state().await.unwrap().ledger.accounts["global"]
            .total
            .spent,
        7
    );
    h.close().await;
}

#[tokio::test]
async fn authority_phase_retry_requires_fresh_c2_and_distinct_c3_approvals() {
    use crate::{
        kernel::{Admission, TaskContext},
        ledger::{PriceTable, Rate},
        operator::{OperatorCommand, OperatorKernel, OperatorRequest},
        security::{
            Action, ActionRequest, CapabilityKind, ConfirmationReview, ConfirmationTier,
            IsolationTier, OperationClass,
        },
    };
    for (class, tier, required) in [
        (OperationClass::Reversible, ConfirmationTier::C2, 1usize),
        (OperationClass::External, ConfirmationTier::C3, 2usize),
    ] {
        let h = Harness::open().await;
        assert_eq!(
            h.send("privileged-claim", "/tool/claim_task", json!({}))
                .await
                .0,
            200
        );
        let view = h.kernel.cluster.view().await.unwrap();
        let lease = &view.leases["allocation/a1"];
        let checks = vec![crate::cluster::LeaseCheck {
            id: "allocation/a1".into(),
            holder: "n1".into(),
            fence: lease.fence,
        }];
        let task = TaskContext {
            id: "task-1".into(),
            incarnation: 1,
            fence: lease.fence,
        };
        h.kernel
            .observe_task_input(
                "privileged-input",
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
            .find(|cap| cap.kind == CapabilityKind::ToolCall)
            .unwrap();
        let spend = grant
            .capabilities
            .iter()
            .find(|cap| cap.kind == CapabilityKind::BudgetSpend)
            .unwrap();
        let intent = format!("privileged-phase-{}", uuid::Uuid::new_v4().simple());
        let id = format!("admit-{intent}");
        let payload = json!({"exact_destination":"synthetic-action","amount":1});
        let request = ActionRequest {
            intent_id: intent.clone(),
            grant_id: grant.id.clone(),
            grant_version: grant.version,
            capability_id: cap.id.clone(),
            budget_capability_id: Some(spend.id.clone()),
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
                class,
                isolation: IsolationTier::P,
                recipient: Some("provider/local/model/m1".into()),
                evidence: Default::default(),
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
            .unwrap();
        let admission = || Admission {
            request: request.clone(),
            payload: payload.clone(),
            prices: Some(prices.clone()),
            approvals: vec![],
            leases: checks.clone(),
            quota: None,
            task: Some(task.clone()),
        };
        let review_value = h
            .kernel
            .admit(&id, &h.fixture.subject, admission())
            .await
            .unwrap()
            .require()
            .unwrap();
        assert_eq!(review_value["status"], "confirmation_required");
        let original: ConfirmationReview =
            serde_json::from_value(review_value["review"].clone()).unwrap();
        assert_eq!(original.tier, tier);
        assert_eq!(original.required_humans, required);
        for index in 0..required {
            h.kernel
                .apply_operator(
                    h.fixture.operators[index].clone(),
                    OperatorRequest {
                        request_id: format!("old-approval-{intent}-{index}"),
                        operation: OperatorCommand::Approve {
                            action_hash: original.action_hash,
                            evidence_hash: original.evidence_hash,
                            grant_version: original.grant_version,
                            scope: original.scope.clone(),
                            expiry: original.expiry.min(h.fixture.now + 240),
                        },
                    },
                    &h.fixture.signer,
                    crate::cluster::now_ms() / 1000,
                )
                .await
                .unwrap();
        }
        let gate = crate::kernel::admission_sync::pause_once(intent.clone());
        let reviewed = h.kernel.admit(&id, &h.fixture.subject, admission());
        let churn = async {
            tokio::time::timeout(std::time::Duration::from_secs(5), gate.wait_reviewed())
                .await
                .unwrap();
            advance_authority(&h, &uuid::Uuid::new_v4().simple().to_string()).await;
            gate.release();
        };
        let (refreshed, ()) = tokio::join!(reviewed, churn);
        let refreshed = refreshed.unwrap().require().unwrap();
        assert_eq!(refreshed["status"], "confirmation_required");
        let fresh: ConfirmationReview =
            serde_json::from_value(refreshed["review"].clone()).unwrap();
        assert_eq!(fresh.tier, tier);
        assert_eq!(fresh.required_humans, required);
        assert!(fresh.grant_epoch > original.grant_epoch);
        let state = h.kernel.state().await.unwrap();
        assert_eq!(state.ledger.accounts["global"].total.held, 0);
        assert!(!state.ledger.intents.contains_key(&intent));
        assert!(!state.effects.contains_key(&intent));
        assert_eq!(
            state
                .replies
                .values()
                .filter(|reply| reply.value["status"] == "authority_changed"
                    && reply.value["intent"] == intent)
                .count(),
            1
        );
        // An authority retry cannot change the immutable action while its review is pending.
        let mut changed = admission();
        changed.payload["amount"] = json!(2);
        changed.request.action.arguments = changed.payload.clone();
        assert!(
            !h.kernel
                .admit(&id, &h.fixture.subject, changed)
                .await
                .unwrap()
                .ok
        );
        for index in 0..required {
            h.kernel
                .apply_operator(
                    h.fixture.operators[index].clone(),
                    OperatorRequest {
                        request_id: format!("fresh-approval-{intent}-{index}"),
                        operation: OperatorCommand::Approve {
                            action_hash: fresh.action_hash,
                            evidence_hash: fresh.evidence_hash,
                            grant_version: fresh.grant_version,
                            scope: fresh.scope.clone(),
                            expiry: fresh.expiry.min(h.fixture.now + 240),
                        },
                    },
                    &h.fixture.signer,
                    crate::cluster::now_ms() / 1000,
                )
                .await
                .unwrap();
            let reply = h
                .kernel
                .admit(&id, &h.fixture.subject, admission())
                .await
                .unwrap()
                .require()
                .unwrap();
            if index + 1 < required {
                assert_eq!(reply["status"], "confirmation_required");
                assert_eq!(
                    h.kernel.state().await.unwrap().ledger.accounts["global"]
                        .total
                        .held,
                    0
                );
                h.kernel
                    .apply_operator(
                        h.fixture.operators[index].clone(),
                        OperatorRequest {
                            request_id: format!("repeat-fresh-human-{intent}-{index}"),
                            operation: OperatorCommand::Approve {
                                action_hash: fresh.action_hash,
                                evidence_hash: fresh.evidence_hash,
                                grant_version: fresh.grant_version,
                                scope: fresh.scope.clone(),
                                expiry: fresh.expiry.min(h.fixture.now + 240),
                            },
                        },
                        &h.fixture.signer,
                        crate::cluster::now_ms() / 1000,
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    h.kernel
                        .admit(&id, &h.fixture.subject, admission())
                        .await
                        .unwrap()
                        .require()
                        .unwrap()["status"],
                    "confirmation_required"
                );
            }
        }
        let state = h.kernel.state().await.unwrap();
        assert_eq!(state.ledger.accounts["global"].total.held, 30);
        assert_eq!(state.ledger.accounts["global"].total.spent, 0);
        assert_eq!(state.effects[&intent].state, "pending");
        assert!(
            h.kernel
                .admit(&id, &h.fixture.subject, admission())
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
        assert!(h.upstream.lock().await.is_empty());
        h.close().await;
    }
}

#[tokio::test]
async fn retired_evidence_blocks_cached_model_body_without_another_provider_contact() {
    let h = Harness::open().await;
    assert_eq!(
        h.send("replay-claim", "/tool/claim_task", json!({}))
            .await
            .0,
        200
    );
    // A policy audit surface needs its own declared capability, even for readonly metadata.
    assert_eq!(
        h.send(
            "undeclared-audit",
            "/tool/audit_read",
            json!({"after_index":0,"limit":16})
        )
        .await
        .0,
        403
    );
    // The real model/context path records inputs before a worker can produce a Finding.
    let (status, bootstrap) = h
        .send(
            "bootstrap-evidence",
            "/model",
            inference("observe the current task"),
        )
        .await;
    assert_eq!(status, 200, "{bootstrap}");
    let (status, finding) = h
        .send(
            "replay-finding",
            "/tool/submit_finding",
            json!({"body":{"fact":"retired synthetic input"}}),
        )
        .await;
    assert_eq!(status, 200, "{finding}");
    let finding_id = finding["id"].as_str().unwrap().to_owned();
    let (status, read) = h
        .send("replay-read", "/tool/read_findings", json!({"limit":128}))
        .await;
    assert_eq!(status, 200, "{read}");
    let (status, completion) = h
        .send(
            "cached-model",
            "/model",
            inference("use the current evidence"),
        )
        .await;
    assert_eq!(status, 200, "{completion}");
    assert_eq!(completion["swarm_cost_microusd"], 7);
    let before_contact = h.upstream.lock().await.len();
    let (status, retracted) = h
        .send(
            "replay-retract",
            "/tool/retract_finding",
            json!({"finding_id":finding_id,"reason":"source withdrawn"}),
        )
        .await;
    assert_eq!(status, 200, "{retracted}");
    let (status, replay) = h
        .send(
            "cached-model",
            "/model",
            inference("use the current evidence"),
        )
        .await;
    assert_eq!(status, 403, "{replay}");
    assert!(replay.get("choices").is_none());
    assert!(!replay.to_string().contains("synthetic answer"));
    assert_eq!(h.upstream.lock().await.len(), before_contact);
    let state = h.kernel.state().await.unwrap();
    assert_eq!(state.ledger.accounts["global"].total.spent, 14);
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    assert_eq!(state.effects["model-cached-model"].state, "done");
    assert!(state.domain.findings.contains_key(&finding_id));
    assert!(!state.domain.finding_eligible(&finding_id));
    h.close().await;
}

#[tokio::test]
async fn actual_http_receipt_with_invalid_completion_is_charged_and_never_replayed() {
    let h = Harness::open().await;
    assert_eq!(
        h.send("invalid-claim", "/tool/claim_task", json!({}))
            .await
            .0,
        200
    );
    let (status, error) = h
        .send(
            "invalid-completion",
            "/model",
            inference("__valid_receipt_invalid_choice__"),
        )
        .await;
    assert_eq!(status, 403, "{error}");
    assert!(error.get("choices").is_none());
    assert!(error.get("evidence").is_none());
    assert!(!error.to_string().contains("not-an-object"));
    let state = h.kernel.state().await.unwrap();
    assert_eq!(h.upstream.lock().await.len(), 1);
    assert_eq!(state.ledger.accounts["global"].total.spent, 7);
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    assert_eq!(
        state.ledger.intents["model-invalid-completion"].state,
        IntentState::Settled
    );
    assert!(
        state.ledger.intents["model-invalid-completion"]
            .receipt
            .is_some()
    );
    assert_eq!(state.effects["model-invalid-completion"].state, "done");
    let retained = state.effects["model-invalid-completion"]
        .result
        .as_ref()
        .unwrap();
    assert!(retained["completion"].is_null());
    assert_eq!(
        retained["outcome"]["evidence"]["choices"][0]["message"],
        "not-an-object"
    );
    assert_eq!(
        retained["outcome"]["evidence"]["id"].as_str(),
        state.ledger.intents["model-invalid-completion"]
            .receipt
            .as_ref()
            .map(|receipt| receipt.id.as_str())
    );
    assert!(
        state
            .domain
            .latest_planning_round("g", h.fixture.subject.id())
            .is_err()
    );
    let (status, replay) = h
        .send(
            "invalid-completion",
            "/model",
            inference("__valid_receipt_invalid_choice__"),
        )
        .await;
    assert_eq!(status, 403, "{replay}");
    assert!(replay.get("choices").is_none());
    assert_eq!(h.upstream.lock().await.len(), 1);
    assert_eq!(
        h.kernel.state().await.unwrap().ledger.accounts["global"]
            .total
            .spent,
        7
    );
    h.close().await;
}

#[tokio::test]
async fn missing_usage_preserves_quarantined_receipt_and_unknown_hold_without_replay() {
    let h = Harness::open().await;
    assert_eq!(
        h.send("missing-claim", "/tool/claim_task", json!({}))
            .await
            .0,
        200
    );
    let payload = inference("__missing_receipt_usage__");
    let (status, error) = h.send("missing-usage", "/model", payload.clone()).await;
    assert_eq!(status, 403, "{error}");
    assert!(error.get("choices").is_none() && error.get("evidence").is_none());
    assert!(!error.to_string().contains("synthetic answer"));
    assert_eq!(h.upstream.lock().await.len(), 1);
    let state = h.kernel.state().await.unwrap();
    let intent = &state.ledger.intents["model-missing-usage"];
    assert_eq!(intent.state, IntentState::Unknown);
    assert!(intent.receipt.is_none());
    assert_eq!(state.ledger.accounts["global"].total.spent, 0);
    let held = state.ledger.accounts["global"].total.held;
    assert!(held > 0);
    let effect = &state.effects["model-missing-usage"];
    assert_eq!(effect.state, "unknown");
    let retained = effect.result.as_ref().unwrap();
    assert!(retained["completion"].is_null());
    assert!(retained["outcome"]["request_id"].as_str().is_some());
    assert_eq!(
        retained["outcome"]["evidence"]["id"],
        retained["outcome"]["request_id"]
    );
    assert_eq!(
        retained["outcome"]["evidence"]["choices"][0]["message"]["content"],
        "synthetic answer"
    );
    let (status, replay) = h.send("missing-usage", "/model", payload).await;
    assert_eq!(status, 403, "{replay}");
    assert!(replay.get("choices").is_none() && replay.get("evidence").is_none());
    assert!(!replay.to_string().contains("synthetic answer"));
    assert_eq!(h.upstream.lock().await.len(), 1);
    assert_eq!(
        h.kernel.state().await.unwrap().ledger.accounts["global"]
            .total
            .held,
        held
    );
    h.close().await;
}

#[tokio::test]
async fn retraction_during_actual_provider_contact_keeps_charge_and_blocks_fresh_body() {
    let h = Harness::open().await;
    assert_eq!(
        h.send("flight-claim", "/tool/claim_task", json!({}))
            .await
            .0,
        200
    );
    let (status, bootstrap) = h
        .send(
            "flight-bootstrap",
            "/model",
            inference("observe current evidence"),
        )
        .await;
    assert_eq!(status, 200, "{bootstrap}");
    let (status, finding) = h
        .send(
            "flight-finding",
            "/tool/submit_finding",
            json!({"body":{"fact":"withdraw while receipt is in flight"}}),
        )
        .await;
    assert_eq!(status, 200, "{finding}");
    let finding_id = finding["id"].as_str().unwrap();
    let (status, read) = h
        .send("flight-read", "/tool/read_findings", json!({}))
        .await;
    assert_eq!(status, 200, "{read}");
    let before = h.kernel.state().await.unwrap().ledger.accounts["global"]
        .total
        .spent;
    let model = h.send("flight-paid", "/model", inference("__delayed_receipt__"));
    let retract = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while h.upstream.lock().await.len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        h.send("flight-retract", "/tool/retract_finding", json!({"finding_id":finding_id,"reason":"evidence withdrawn after confirmed receiver contact"})).await
    };
    let ((status, response), (retract_status, retracted)) = tokio::join!(model, retract);
    assert_eq!(retract_status, 200, "{retracted}");
    assert_eq!(status, 403, "{response}");
    assert!(response.get("choices").is_none() && response.get("evidence").is_none());
    assert!(!response.to_string().contains("synthetic answer"));
    let state = h.kernel.state().await.unwrap();
    assert_eq!(state.ledger.accounts["global"].total.spent, before + 7);
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    assert_eq!(
        state.ledger.intents["model-flight-paid"].state,
        IntentState::Settled
    );
    assert!(state.ledger.intents["model-flight-paid"].receipt.is_some());
    assert_eq!(state.effects["model-flight-paid"].state, "done");
    assert!(
        !state
            .domain
            .planning_rounds
            .contains_key("g/model-flight-paid")
    );
    assert_eq!(h.upstream.lock().await.len(), 2);
    assert_eq!(
        h.send("flight-paid", "/model", inference("__delayed_receipt__"))
            .await
            .0,
        403
    );
    assert_eq!(h.upstream.lock().await.len(), 2);
    h.close().await;
}

#[tokio::test]
async fn declared_audit_capability_returns_only_goal_metadata_and_signed_measured_flows() {
    let audit = super::ToolDefinition {
        name: "audit_read".into(),
        description: "Read scoped audit evidence".into(),
        parameters: json!({"type":"object","properties":{"after_index":{"type":"integer"},"limit":{"type":"integer"}},"additionalProperties":false}),
        operation: "audit_read".into(),
        kind: crate::security::CapabilityKind::ToolCall,
        resource: "audit/g".into(),
        class: crate::security::OperationClass::ReadOnly,
    };
    let h = Harness::open_with_tools(
        std::sync::Arc::new(super::receiver_tests::DenyTools),
        vec![audit],
    )
    .await;
    assert_eq!(
        h.send("audit-claim", "/tool/claim_task", json!({})).await.0,
        200
    );
    let (status, response) = h
        .send(
            "audit-model",
            "/model",
            inference("measure the approved receiver"),
        )
        .await;
    assert_eq!(status, 200, "{response}");
    let (status, view) = h
        .send(
            "audit-view",
            "/tool/audit_read",
            json!({"after_index":0,"limit":32}),
        )
        .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["goal_id"], "g");
    let key = h.kernel.signer.public_key();
    let head: crate::audit::SignedTreeHead = serde_json::from_value(view["head"].clone()).unwrap();
    head.verify("audit-tree-head", &key).unwrap();
    let entries = view["entries"].as_array().unwrap();
    assert!(!entries.is_empty());
    assert!(entries.len() <= 32);
    for entry in entries {
        let event: crate::audit::AuditEvent =
            serde_json::from_value(entry["event"].clone()).unwrap();
        let proof: crate::audit::InclusionProof =
            serde_json::from_value(entry["proof"].clone()).unwrap();
        assert_eq!(event.goal.as_deref(), Some("g"));
        crate::audit::verify_inclusion(&event, &proof, &head.payload).unwrap();
    }
    let flows: crate::audit::Signed<crate::flow::GoalFlows> =
        serde_json::from_value(view["egress"].clone()).unwrap();
    flows.verify("goal-egress-view", &key).unwrap();
    assert_eq!(flows.payload.records.len(), 1);
    let flow = &flows.payload.records[0];
    assert_eq!(flow.goal_id, "g");
    assert_eq!(flow.holder, h.fixture.subject.id());
    assert_eq!(flow.route, crate::flow::RouteKind::Provider);
    assert!(flow.contacted);
    assert!(flow.payload_request_bytes > 0 && flow.payload_response_bytes > 0);
    assert!(flows.payload.flags.is_empty());
    assert_eq!(h.upstream.lock().await.len(), 1);
    assert!(!view.to_string().contains("synthetic answer"));
    assert_eq!(
        h.send("audit-overflow", "/tool/audit_read", json!({"limit":33}))
            .await
            .0,
        403
    );
    h.close().await;
}

#[tokio::test]
async fn concurrent_claims_and_paid_receipt_cross_second_without_backwards_audit() {
    let h = Harness::open().await;
    assert_eq!(
        h.send("clock-initial-claim", "/tool/claim_task", json!({}))
            .await
            .0,
        200
    );
    // Keep the real network receipt in flight across the next wall-clock second.
    let fraction = crate::cluster::now_ms() % 1000;
    let wait = if fraction <= 800 {
        800 - fraction
    } else {
        1800 - fraction
    };
    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
    let started_ms = crate::cluster::now_ms();
    let model = h.send("clock-model", "/model", inference("__delayed_receipt__"));
    let claims = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while h.upstream.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tokio::join!(
            h.send("clock-claim-a", "/tool/claim_task", json!({})),
            h.send("clock-claim-b", "/tool/claim_task", json!({})),
        )
    };
    let ((model_status, receipt), ((a_status, a), (b_status, b))) = tokio::join!(model, claims);
    assert_eq!(a_status, 200, "{a}");
    assert_eq!(b_status, 200, "{b}");
    assert_eq!(model_status, 200, "{receipt}");
    let stopped_ms = crate::cluster::now_ms();
    assert!(stopped_ms / 1000 > started_ms / 1000);
    let state = h.kernel.state().await.unwrap();
    assert_eq!(state.ledger.accounts["global"].total.spent, 7);
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    assert_eq!(h.upstream.lock().await.len(), 1);
    state.audit.validate().unwrap();
    assert!(
        state
            .audit
            .events()
            .windows(2)
            .all(|events| events[0].timestamp <= events[1].timestamp)
    );
    let flows = state.egress.goal_view("g", 32);
    assert_eq!(flows.records.len(), 1);
    assert!(flows.records[0].closed_ms.unwrap() / 1000 > flows.records[0].opened_ms / 1000);
    h.close().await;
}
