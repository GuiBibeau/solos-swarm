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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires release-profile receiver stress; explicitly run with --release --ignored"]
async fn ten_overlapping_authenticated_claims_and_models_settle_once_without_cas_exhaustion() {
    use std::sync::Arc;
    use tokio::{sync::Barrier, task::JoinSet};

    let h = Arc::new(Harness::open_with_concurrency(10).await);
    let mut claims = JoinSet::new();
    for index in 0..10 {
        let h = h.clone();
        claims.spawn(async move {
            h.send(&format!("ten-claim-{index}"), "/tool/claim_task", json!({}))
                .await
        });
    }
    while let Some(result) = claims.join_next().await {
        let (status, body) = result.unwrap();
        assert_eq!(status, 200, "concurrent claim failed: {body}");
    }
    let observer_h = h.clone();
    let observer = tokio::spawn(async move {
        for _ in 0..36 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                observer_h.kernel.control_state(),
            )
            .await
            {
                Ok(Ok(state)) => eprintln!(
                    "ten-request diagnostic: upstream={} effects={:?} spent={} held={} boards={} rounds={}",
                    observer_h.upstream.lock().await.len(),
                    state
                        .effects
                        .iter()
                        .map(|(id, effect)| (id, &effect.state))
                        .collect::<Vec<_>>(),
                    state.ledger.accounts["global"].total.spent,
                    state.ledger.accounts["global"].total.held,
                    state.domain.boards.get("g").map_or(0, Vec::len),
                    state.domain.planning_rounds.len(),
                ),
                result => eprintln!("ten-request diagnostic state read failed: {result:?}"),
            }
        }
    });
    let barrier = Arc::new(Barrier::new(11));
    let mut requests = JoinSet::new();
    for index in 0..10 {
        let model_h = h.clone();
        let model_barrier = barrier.clone();
        requests.spawn(async move {
            model_barrier.wait().await;
            let id = format!("ten-model-{index}");
            let (status, body) = model_h
                .send(
                    &id,
                    "/model",
                    inference(&format!(
                        "__delayed_receipt__ ten concurrent inference {index}"
                    )),
                )
                .await;
            (id, status, body)
        });
    }
    barrier.wait().await;
    while let Some(result) = requests.join_next().await {
        let (id, status, body) = result.unwrap();
        assert_eq!(status, 200, "{id} failed: {body}");
    }
    observer.abort();
    let _ = observer.await;
    let state = h.kernel.state().await.unwrap();
    assert_eq!(h.upstream.lock().await.len(), 10);
    assert_eq!(state.ledger.accounts["global"].total.spent, 70);
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    assert!(!state.security.stopped(&crate::security::Scope::Goal {
        tenant: "t".into(),
        goal: "g".into(),
    }));
    assert_eq!(state.domain.planning_rounds.len(), 10);
    assert!(state.domain.boards.get("g").is_none_or(Vec::is_empty));
    for index in 0..10 {
        let intent = format!("model-ten-model-{index}");
        assert_eq!(state.effects[&intent].state, "done");
        assert_eq!(state.ledger.intents[&intent].state, IntentState::Settled);
        assert_eq!(state.ledger.intents[&intent].settled, 7);
        assert_eq!(state.quotas.reservations[&intent].state, "done");
        let (status, body) = h
            .send(
                &format!("ten-model-{index}"),
                "/model",
                inference(&format!(
                    "__delayed_receipt__ ten concurrent inference {index}"
                )),
            )
            .await;
        assert_eq!(status, 200, "cached model failed: {body}");
    }
    assert_eq!(h.upstream.lock().await.len(), 10);
    let final_state = h.kernel.state().await.unwrap();
    assert_eq!(final_state.ledger.accounts["global"].total.spent, 70);
    assert_eq!(final_state.ledger.accounts["global"].total.held, 0);
    final_state.audit.validate().unwrap();
    Arc::try_unwrap(h)
        .unwrap_or_else(|_| panic!("receiver fixture still shared"))
        .close()
        .await;
}

#[tokio::test]
async fn control_writer_bypasses_ten_queued_normal_writers() {
    let h = Harness::open().await;
    let cluster = h.kernel.cluster.clone();
    let inflight = cluster.kernel_write(false).await;
    let (started_tx, mut started_rx) = tokio::sync::mpsc::channel(11);
    let (order_tx, mut order_rx) = tokio::sync::mpsc::channel(11);
    let mut writers = tokio::task::JoinSet::new();
    for index in 0..10 {
        let cluster = cluster.clone();
        let started = started_tx.clone();
        let order = order_tx.clone();
        writers.spawn(async move {
            started.send(()).await.unwrap();
            let _guard = cluster.kernel_write(false).await;
            order.send(format!("normal-{index}")).await.unwrap();
            tokio::task::yield_now().await;
        });
    }
    for _ in 0..10 {
        started_rx.recv().await.unwrap();
    }
    let control_cluster = cluster.clone();
    writers.spawn(async move {
        started_tx.send(()).await.unwrap();
        let _guard = control_cluster.kernel_write(true).await;
        order_tx.send("control".into()).await.unwrap();
    });
    started_rx.recv().await.unwrap();
    // This single-thread runtime has polled each writer to its awaited gate.
    tokio::task::yield_now().await;
    drop(inflight);
    let mut order = vec![];
    while let Some(next) = tokio::time::timeout(std::time::Duration::from_secs(5), order_rx.recv())
        .await
        .expect("writer queue did not drain")
    {
        order.push(next);
    }
    while let Some(result) = writers.join_next().await {
        result.unwrap();
    }
    assert_eq!(order.len(), 11);
    assert!(
        order.iter().position(|entry| entry == "control").unwrap() <= 1,
        "control waited behind the normal queue: {order:?}"
    );
    h.close().await;
}

#[tokio::test]
async fn queued_model_waits_without_funding_then_admits_current_unrelated_authority_epoch() {
    use crate::provider::Provider;
    let h = Harness::open().await;
    assert_eq!(
        h.send("queue-claim", "/tool/claim_task", json!({})).await.0,
        200
    );
    let (_stop, receiver) = tokio::sync::watch::channel(false);
    let slot = h
        .provider
        .acquire(crate::cluster::now_ms() + 10_000, receiver)
        .await
        .unwrap();
    let id = format!("queued-model-{}", uuid::Uuid::new_v4().simple());
    let intent = format!("model-{id}");
    let payload = inference("wait for a local slot before any budget reservation");
    let model = h.send(&id, "/model", payload.clone());
    let release = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let state = h.kernel.state().await.unwrap();
                if state
                    .security
                    .inputs
                    .contains_key(&format!("context/{intent}"))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("queued model never prepared its immutable context");
        let queued = h.kernel.state().await.unwrap();
        assert!(!queued.effects.contains_key(&intent));
        assert!(!queued.ledger.intents.contains_key(&intent));
        assert!(!queued.quotas.reservations.contains_key(&intent));
        assert_eq!(queued.ledger.accounts["global"].total.held, 0);
        assert_eq!(queued.ledger.accounts["global"].total.spent, 0);
        assert!(h.upstream.lock().await.is_empty());
        let epoch = advance_authority(&h, &uuid::Uuid::new_v4().simple().to_string()).await;
        let changed = h.kernel.state().await.unwrap();
        assert!(!changed.effects.contains_key(&intent));
        assert!(!changed.ledger.intents.contains_key(&intent));
        assert!(!changed.quotas.reservations.contains_key(&intent));
        assert_eq!(changed.ledger.accounts["global"].total.held, 0);
        assert_eq!(changed.ledger.accounts["global"].total.spent, 0);
        assert!(h.upstream.lock().await.is_empty());
        drop(slot);
        epoch
    };
    let ((status, completion), epoch) = tokio::join!(model, release);
    assert_eq!(status, 200, "{completion}");
    let state = h.kernel.state().await.unwrap();
    assert_eq!(state.effects[&intent].permit.payload.grant_epoch, epoch);
    assert_eq!(state.effects[&intent].request.intent_id, intent);
    assert_eq!(
        state.effects[&intent].payload["wire_hash"],
        crate::kernel::hex(&crate::audit::content_hash(&payload).unwrap())
    );
    assert_eq!(state.ledger.intents[&intent].state, IntentState::Settled);
    assert_eq!(state.ledger.accounts["global"].total.spent, 7);
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
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
async fn queued_model_denied_by_current_stop_releases_local_slot_without_funding() {
    use crate::{
        operator::{OperatorCommand, OperatorKernel, OperatorRequest},
        provider::Provider,
        security::Scope,
    };
    let h = Harness::open().await;
    assert_eq!(
        h.send("queue-deny-claim", "/tool/claim_task", json!({}))
            .await
            .0,
        200
    );
    let (_stop, receiver) = tokio::sync::watch::channel(false);
    let held = h
        .provider
        .acquire(crate::cluster::now_ms() + 10_000, receiver)
        .await
        .unwrap();
    let id = format!("queue-denied-{}", uuid::Uuid::new_v4().simple());
    let intent = format!("model-{id}");
    let model = h.send(
        &id,
        "/model",
        inference("no reservation while waiting for a slot"),
    );
    let stop_and_release = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if h.kernel
                    .state()
                    .await
                    .unwrap()
                    .security
                    .inputs
                    .contains_key(&format!("context/{intent}"))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("queued model never prepared its context");
        h.kernel
            .apply_operator(
                h.fixture.operators[0].clone(),
                OperatorRequest {
                    request_id: format!("stop-{id}"),
                    operation: OperatorCommand::EngageStop {
                        scope: Scope::Goal {
                            tenant: "t".into(),
                            goal: "g".into(),
                        },
                        reason: "deny before current admission".into(),
                    },
                },
                &h.fixture.signer,
                crate::cluster::now_ms() / 1000,
            )
            .await
            .unwrap();
        drop(held);
    };
    let ((status, response), ()) = tokio::join!(model, stop_and_release);
    assert_eq!(status, 403, "{response}");
    let state = h.kernel.state().await.unwrap();
    assert!(!state.effects.contains_key(&intent));
    assert!(!state.ledger.intents.contains_key(&intent));
    assert!(!state.quotas.reservations.contains_key(&intent));
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    assert_eq!(state.ledger.accounts["global"].total.spent, 0);
    assert!(h.upstream.lock().await.is_empty());
    let (_stop, receiver) = tokio::sync::watch::channel(false);
    let released = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        h.provider
            .acquire(crate::cluster::now_ms() + 1000, receiver),
    )
    .await
    .expect("denied model leaked its local provider slot")
    .unwrap();
    drop(released);
    h.close().await;
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
async fn stale_c3_receiver_application_waits_for_two_fresh_humans_and_consumes_once() {
    use crate::{
        operator::{OperatorCommand, OperatorKernel, OperatorRequest},
        security::{CapabilityKind, ConfirmationReview, ConfirmationTier},
    };
    let h = Harness::open_with_confirmation(CapabilityKind::BoardPost, ConfirmationTier::C3).await;
    assert_eq!(
        h.send("c3-claim", "/tool/claim_task", json!({})).await.0,
        200
    );
    let id = format!("c3-application-{}", uuid::Uuid::new_v4().simple());
    let intent = format!("tool-{id}");
    let payload = json!({"topic":"approved-evidence","body":{"value":"one immutable application"}});
    let (status, first) = h.send(&id, "/tool/board_post", payload.clone()).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["status"], "confirmation_required");
    let review: ConfirmationReview = serde_json::from_value(first["review"].clone()).unwrap();
    assert_eq!(review.tier, ConfirmationTier::C3);
    assert_eq!(review.required_humans, 2);
    for human in 0..2 {
        h.kernel
            .apply_operator(
                h.fixture.operators[human].clone(),
                OperatorRequest {
                    request_id: format!("original-human-{human}-{id}"),
                    operation: OperatorCommand::Approve {
                        action_hash: review.action_hash,
                        evidence_hash: review.evidence_hash,
                        grant_version: review.grant_version,
                        scope: review.scope.clone(),
                        expiry: review.expiry,
                    },
                },
                &h.fixture.signer,
                crate::cluster::now_ms() / 1000,
            )
            .await
            .unwrap();
    }
    let gate = crate::kernel::admission_sync::pause_once(format!("application:apply-{id}"));
    let application = h.send(&id, "/tool/board_post", payload.clone());
    let churn = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.wait_reviewed())
            .await
            .expect("receiver never reached local application boundary");
        let before = h.kernel.state().await.unwrap();
        let original = before.effects[&intent].permit.clone();
        assert_eq!(before.effects[&intent].state, "pending");
        assert_eq!(original.payload.confirmation, ConfirmationTier::C3);
        assert!(before.domain.boards.get("g").is_none_or(Vec::is_empty));
        assert!(!before.ledger.intents.contains_key(&intent));
        assert_eq!(before.ledger.accounts["global"].total.held, 0);
        advance_authority(&h, &uuid::Uuid::new_v4().simple().to_string()).await;
        gate.release();
        original
    };
    let ((status, refreshed), original) = tokio::join!(application, churn);
    assert_eq!(status, 200, "{refreshed}");
    assert_eq!(refreshed["status"], "confirmation_required");
    let fresh: ConfirmationReview = serde_json::from_value(refreshed["review"].clone()).unwrap();
    assert_eq!(fresh.tier, ConfirmationTier::C3);
    assert_eq!(fresh.required_humans, 2);
    assert_eq!(fresh.action_hash, original.payload.action_hash);
    assert_eq!(fresh.evidence_hash, original.payload.evidence_hash);
    assert!(fresh.grant_epoch > original.payload.grant_epoch);
    assert_eq!(fresh.expiry, original.payload.expiry);
    let reviews: std::collections::BTreeMap<_, _> = h
        .kernel
        .state()
        .await
        .unwrap()
        .replies
        .iter()
        .filter(|(_, reply)| {
            reply.value["status"] == "confirmation_required" && reply.value["intent"] == intent
        })
        .map(|(id, reply)| (id.clone(), crate::audit::content_hash(reply).unwrap()))
        .collect();
    assert!(!reviews.is_empty());
    for human in 0..2 {
        h.kernel
            .apply_operator(
                h.fixture.operators[human].clone(),
                OperatorRequest {
                    request_id: format!("fresh-human-{human}-{id}"),
                    operation: OperatorCommand::Approve {
                        action_hash: fresh.action_hash,
                        evidence_hash: fresh.evidence_hash,
                        grant_version: fresh.grant_version,
                        scope: fresh.scope.clone(),
                        expiry: fresh.expiry,
                    },
                },
                &h.fixture.signer,
                crate::cluster::now_ms() / 1000,
            )
            .await
            .unwrap();
        if human == 0 {
            let (status, insufficient) = h.send(&id, "/tool/board_post", payload.clone()).await;
            assert_eq!(status, 200, "{insufficient}");
            assert_eq!(insufficient["status"], "confirmation_required");
            let state = h.kernel.state().await.unwrap();
            assert_eq!(state.effects[&intent].state, "pending");
            assert!(state.domain.boards.get("g").is_none_or(Vec::is_empty));
            let changed = json!({"topic":"changed-destination","body":{"value":"same id cannot change intent"}});
            assert_eq!(h.send(&id, "/tool/board_post", changed).await.0, 403);
            assert_eq!(
                h.kernel.state().await.unwrap().effects[&intent].payload,
                state.effects[&intent].payload
            );
            // Also reach the application's own replay identity guard through the authenticated
            // Kernel API, independent of the receiver's earlier pending-admission check.
            let view = h.kernel.cluster.view().await.unwrap();
            let allocation = &view.leases["allocation/a1"];
            let rejected = h
                .kernel
                .domain(
                    &format!("apply-{id}"),
                    &h.fixture.subject,
                    &intent,
                    crate::domain::DomainCommand::AppendBoard {
                        goal_id: "g".into(),
                        entry_id: id.clone(),
                        topic: "changed-application".into(),
                        body: json!({"value":"same application id cannot change its command"}),
                        signature: String::new(),
                    },
                    "",
                    vec![crate::cluster::LeaseCheck {
                        id: "allocation/a1".into(),
                        holder: allocation.holder.clone(),
                        fence: allocation.fence,
                    }],
                )
                .await
                .unwrap_err();
            assert!(
                rejected
                    .to_string()
                    .contains("application request identity reused for different content"),
                "{rejected}"
            );
        }
    }
    let (status, completed) = h.send(&id, "/tool/board_post", payload.clone()).await;
    assert_eq!(status, 200, "{completed}");
    assert_ne!(completed["status"], "confirmation_required");
    let state = h.kernel.state().await.unwrap();
    assert_eq!(state.effects[&intent].state, "done");
    assert_eq!(state.domain.boards["g"].len(), 1);
    assert_eq!(state.domain.boards["g"][0].entry_id, id);
    assert_eq!(
        state.effects[&intent].permit.payload.action_hash,
        original.payload.action_hash
    );
    assert_eq!(
        state.effects[&intent].permit.payload.expiry,
        original.payload.expiry
    );
    assert_eq!(
        state.effects[&intent].permit.payload.grant_epoch,
        fresh.grant_epoch
    );
    assert!(!state.pending_actions.contains_key(&intent));
    assert!(!state.ledger.intents.contains_key(&intent));
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    assert_eq!(state.ledger.accounts["global"].total.spent, 0);
    assert!(h.upstream.lock().await.is_empty());
    let refreshed_audit = format!(
        "permit-refresh/{intent}/{}",
        crate::kernel::hex(&crate::audit::content_hash(&state.effects[&intent].permit).unwrap())
    );
    assert_eq!(
        state
            .audit
            .events()
            .iter()
            .filter(|event| event.id == refreshed_audit)
            .count(),
        1
    );
    let observed = state.security.observed.get(h.fixture.subject.id()).cloned();
    let (status, replay) = h.send(&id, "/tool/board_post", payload.clone()).await;
    assert_eq!(status, 200, "{replay}");
    assert_ne!(replay["status"], "confirmation_required");
    let replayed = h.kernel.state().await.unwrap();
    assert_eq!(replayed.domain.boards["g"].len(), 1);
    assert!(!replayed.pending_actions.contains_key(&intent));
    assert_eq!(
        replayed
            .security
            .observed
            .get(h.fixture.subject.id())
            .cloned(),
        observed
    );
    assert_eq!(replayed.ledger.accounts["global"].total.spent, 0);
    assert_eq!(replayed.ledger.accounts["global"].total.held, 0);
    for (id, hash) in &reviews {
        assert_eq!(
            crate::audit::content_hash(&replayed.replies[id]).unwrap(),
            *hash
        );
    }
    h.kernel
        .apply_operator(
            h.fixture.operators[0].clone(),
            OperatorRequest {
                request_id: format!("stop-c3-{id}"),
                operation: OperatorCommand::EngageStop {
                    scope: crate::security::Scope::Goal {
                        tenant: "t".into(),
                        goal: "g".into(),
                    },
                    reason: "stop must forbid cached local delivery".into(),
                },
            },
            &h.fixture.signer,
            crate::cluster::now_ms() / 1000,
        )
        .await
        .unwrap();
    assert_eq!(h.send(&id, "/tool/board_post", payload).await.0, 403);
    assert_eq!(h.kernel.state().await.unwrap().domain.boards["g"].len(), 1);
    h.close().await;
}

#[tokio::test]
async fn completed_attempt_replay_keeps_original_round_after_new_inference_and_denies_changes() {
    let h = Harness::open().await;
    assert_eq!(
        h.send("attempt-replay-claim", "/tool/claim_task", json!({}))
            .await
            .0,
        200
    );
    assert_eq!(
        h.send(
            "attempt-first-round",
            "/model",
            inference("prepare one candidate")
        )
        .await
        .0,
        200
    );
    let (status, finding) = h
        .send(
            "attempt-replay-finding",
            "/tool/submit_finding",
            json!({"body":{"evidence":"synthetic current evidence"}}),
        )
        .await;
    assert_eq!(status, 200, "{finding}");
    let finding_id = finding["id"].as_str().unwrap();
    let packet = json!({"payload":{"candidate":"immutable candidate"},"finding_ids":[finding_id]});
    let (status, attempt) = h
        .send(
            "attempt-replay-packet",
            "/tool/submit_attempt",
            packet.clone(),
        )
        .await;
    assert_eq!(status, 200, "{attempt}");
    let original_round = attempt["planning_round_id"].as_str().unwrap().to_owned();
    assert_eq!(
        h.send(
            "attempt-next-round",
            "/model",
            inference("compare the retained candidate in a new round")
        )
        .await
        .0,
        200
    );
    let next = h
        .kernel
        .state()
        .await
        .unwrap()
        .domain
        .latest_planning_round("g", h.fixture.subject.id())
        .unwrap()
        .id
        .clone();
    assert_ne!(next, original_round);
    let (status, replay) = h
        .send(
            "attempt-replay-packet",
            "/tool/submit_attempt",
            packet.clone(),
        )
        .await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay["planning_round_id"], original_round);
    let mut changed = packet;
    changed["payload"]["candidate"] = json!("changed candidate under the same packet id");
    assert_eq!(
        h.send("attempt-replay-packet", "/tool/submit_attempt", changed)
            .await
            .0,
        403
    );
    let state = h.kernel.state().await.unwrap();
    assert_eq!(state.domain.attempts.len(), 1);
    assert_eq!(
        state
            .domain
            .attempt("g", "attempt-replay-packet")
            .unwrap()
            .planning_round_id,
        original_round
    );
    assert_eq!(
        state.domain.planning_rounds[&format!("g/{original_round}")]
            .introduced
            .len(),
        1
    );
    assert!(
        state.domain.planning_rounds[&format!("g/{next}")]
            .introduced
            .is_empty()
    );
    assert_eq!(h.upstream.lock().await.len(), 2);
    assert_eq!(state.ledger.accounts["global"].total.spent, 14);
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    h.close().await;
}

#[tokio::test]
async fn identical_read_payload_cannot_replay_findings_as_task_status_under_same_id() {
    use crate::{
        kernel::TaskContext,
        node::ToolDefinition,
        security::{CapabilityKind, OperationClass},
    };
    let descriptor = ToolDefinition {
        name: "declare_task_status".into(),
        description: "explicit fixture Task metadata permission".into(),
        parameters: json!({"type":"object"}),
        operation: "task_status".into(),
        kind: CapabilityKind::ToolCall,
        resource: "task/g".into(),
        class: OperationClass::ReadOnly,
    };
    let h = Harness::open_with_tools(
        std::sync::Arc::new(super::receiver_tests::DenyTools),
        vec![descriptor],
    )
    .await;
    assert_eq!(
        h.send("collision-claim", "/tool/claim_task", json!({}))
            .await
            .0,
        200
    );
    let view = h.kernel.cluster.view().await.unwrap();
    let allocation = &view.leases["allocation/a1"];
    h.kernel
        .observe_task_input(
            "collision-input",
            &h.fixture.subject,
            "a1-1",
            TaskContext {
                id: "task-1".into(),
                incarnation: 1,
                fence: allocation.fence,
            },
            vec![crate::cluster::LeaseCheck {
                id: "allocation/a1".into(),
                holder: allocation.holder.clone(),
                fence: allocation.fence,
            }],
        )
        .await
        .unwrap()
        .require()
        .unwrap();
    let marker = "finding-body-is-not-task-status-metadata";
    let (status, finding) = h
        .send(
            "collision-finding",
            "/tool/submit_finding",
            json!({"body":{"marker":marker}}),
        )
        .await;
    assert_eq!(status, 200, "{finding}");
    assert_eq!(
        h.send(
            "collision-status-authorized",
            "/tool/task_status",
            json!({})
        )
        .await
        .0,
        200
    );
    let id = "cross-operation-read";
    let (status, findings) = h.send(id, "/tool/read_findings", json!({})).await;
    assert_eq!(status, 200, "{findings}");
    assert!(findings.to_string().contains(marker));
    let before = h.kernel.state().await.unwrap();
    let (status, collision) = h.send(id, "/tool/task_status", json!({})).await;
    assert_eq!(status, 403, "{collision}");
    assert!(!collision.to_string().contains(marker));
    assert!(!collision.to_string().contains("task-1"));
    let after = h.kernel.state().await.unwrap();
    assert_eq!(after.effects.len(), before.effects.len());
    assert_eq!(
        after.effects["tool-cross-operation-read"]
            .request
            .action
            .kind,
        CapabilityKind::FindingRead
    );
    assert_eq!(
        after.effects["tool-cross-operation-read"]
            .request
            .action
            .operation,
        "finding_read"
    );
    assert_eq!(
        after.effects["tool-cross-operation-read"]
            .request
            .action
            .resource,
        "finding/g"
    );
    assert_eq!(after.security.observed, before.security.observed);
    assert_eq!(after.ledger.accounts["global"].total.spent, 0);
    assert_eq!(after.ledger.accounts["global"].total.held, 0);
    assert!(h.upstream.lock().await.is_empty());
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

#[tokio::test]
async fn declared_task_status_returns_bounded_current_metadata_without_task_bodies() {
    use crate::{
        node::ToolDefinition,
        security::{CapabilityKind, OperationClass},
    };
    use std::sync::Arc;
    let plain = Harness::open().await;
    assert_eq!(
        plain
            .send("status-no-cap-claim", "/tool/claim_task", json!({}))
            .await
            .0,
        200
    );
    assert_eq!(
        plain
            .send("status-no-cap", "/tool/task_status", json!({}))
            .await
            .0,
        403
    );
    assert!(plain.upstream.lock().await.is_empty());
    plain.close().await;
    let descriptor = ToolDefinition {
        name: "declare_task_status".into(),
        description: "explicit fixture metadata permission".into(),
        parameters: json!({"type":"object"}),
        kind: CapabilityKind::ToolCall,
        operation: "task_status".into(),
        resource: "task/g".into(),
        class: OperationClass::ReadOnly,
    };
    let h = Harness::open_with_tools(Arc::new(super::receiver_tests::DenyTools), vec![descriptor])
        .await;
    assert_eq!(
        h.send("status-claim", "/tool/claim_task", json!({}))
            .await
            .0,
        200
    );
    let (code, page) = h
        .send("status-page", "/tool/task_status", json!({"limit":1}))
        .await;
    assert_eq!(code, 200, "{page}");
    assert_eq!(page["goal_id"], "g");
    assert_eq!(page["goal_version"], 1);
    let rows = page["tasks"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "task-1");
    assert_eq!(rows[0]["state"], "claimed");
    for key in ["input", "body", "spec", "failure_reason"] {
        assert!(rows[0].get(key).is_none());
    }
    let (code, next) = h
        .send(
            "status-next",
            "/tool/task_status",
            json!({"after_id":"task-1","limit":1}),
        )
        .await;
    assert_eq!(code, 200, "{next}");
    assert_eq!(next["tasks"].as_array().unwrap().len(), 0);
    assert_eq!(
        h.send("status-zero", "/tool/task_status", json!({"limit":0}))
            .await
            .0,
        403
    );
    assert!(h.upstream.lock().await.is_empty());
    let state = h.kernel.state().await.unwrap();
    assert_eq!(state.ledger.accounts["global"].total.spent, 0);
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    assert!(
        state
            .security
            .inputs
            .values()
            .any(|input| input.source_type == "task_status")
    );
    h.close().await;
}
