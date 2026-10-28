//! Pause, drain, outage backoff and crash resume, offline and against the real Store.
use super::control::{load_image, read_chain};
use super::*;
use std::time::Duration;

fn goal() -> Goal {
    let tasks: Vec<Value> = ["a00", "a01", "a02"]
        .iter()
        .map(|id| json!({"id":id,"input":{"role":if *id=="a00" {"coordinator"} else {"worker"}}}))
        .collect();
    serde_json::from_value(json!({"schema_version":1,"intent":"test","worker_prompt":"test","synthesis_prompt":"test","scope":"synthetic-fixtures-only","model":"deepseek-flash","agent_count":3,"deadline_seconds":10,"budget_microusd":3_000_000,"max_calls_per_agent":100,"max_input_bytes":65536,"max_output_tokens":2048,"input_price_microusd_per_million":300000,"output_price_microusd_per_million":1200000,"grants":["claim_task","board_read","board_post","submit_finding","read_findings","submit_artifact"],"tasks":tasks,"seed":1,"catalog_version":"test","scorer_version":"1"})).unwrap()
}
fn setup() -> (Goal, Store, PathBuf) {
    let goal = goal();
    let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.yaml"),
        serde_json::to_string(&goal).unwrap(),
    )
    .unwrap();
    let mut store = Store::new(&dir, &goal).unwrap();
    store.goal_status = "running".into();
    for id in ["agent-00", "agent-01", "agent-02"] {
        store.admit(id).unwrap();
    }
    (goal, store, dir)
}
fn dispatch(store: &mut Store, goal: &Goal, id: &str) -> usize {
    let index = store.reserve(goal, id, 200).unwrap();
    store.intents[index].state = "dispatched".into();
    let id = store.intents[index].id.clone();
    store
        .record("provider_dispatched", json!({"intent_id":id}))
        .unwrap();
    index
}
fn receipt(store: &mut Store, goal: &Goal, index: usize) -> u64 {
    let usage = json!({"prompt_tokens":1000,"completion_tokens":100});
    store
        .settle(
            goal,
            index,
            &json!({"id":format!("r{index}"),"usage":usage}),
        )
        .unwrap()
}

#[test]
fn a_paused_run_refuses_model_calls_and_new_claims_without_spending_anything() {
    let (goal, mut store, dir) = setup();
    store
        .tool(&goal, "agent-01", "claim_task", json!({}))
        .unwrap();
    store.begin_pause("pause", "operator").unwrap();
    let calls = store.agents["agent-01"].model_calls;
    assert!(
        store
            .reserve(&goal, "agent-01", 200)
            .unwrap_err()
            .to_string()
            .contains("run pause")
    );
    assert_eq!(
        (store.agents["agent-01"].model_calls, store.held_microusd),
        (calls, 0)
    );
    assert!(
        store.gate_claim("agent-02").is_err(),
        "no new task while paused"
    );
    assert!(
        store.gate_claim("agent-01").is_ok(),
        "a holder keeps its task"
    );
    // Partial results stay collectable: the control plane's own claim is not gated.
    assert!(
        store
            .tool(&goal, "agent-02", "claim_task", json!({}))
            .is_ok()
    );
    assert!(store.lift_pause().unwrap());
    assert!(store.reserve(&goal, "agent-01", 200).is_ok());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_drain_cannot_be_lifted_and_settles_to_zero_held_budget() {
    let (goal, mut store, dir) = setup();
    let first = dispatch(&mut store, &goal, "agent-00");
    let second = dispatch(&mut store, &goal, "agent-01");
    store.begin_pause("drain", "update").unwrap();
    assert!(!store.quiescent());
    assert!(store.reserve(&goal, "agent-02", 200).is_err());
    assert!(!store.lift_pause().unwrap());
    store.begin_pause("pause", "later").unwrap();
    assert_eq!(store.pause.as_ref().unwrap().mode, "drain");
    let cost = receipt(&mut store, &goal, first);
    assert!(!store.quiescent(), "one call is still in flight");
    receipt(&mut store, &goal, second);
    assert!(store.quiescent());
    assert_eq!(
        (store.held_microusd, store.spent_microusd > cost),
        (0, true)
    );
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_control_file_drains_once_the_in_flight_call_settles() {
    let (goal, store, dir) = setup();
    let store = Arc::new(Mutex::new(store));
    let index = dispatch(&mut *store.lock().await, &goal, "agent-00");
    fs::write(
        dir.join("control.json"),
        r#"{"mode":"drain","reason":"candidate update"}"#,
    )
    .unwrap();
    let watcher = tokio::spawn(control_watch(store.clone(), dir.clone()));
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!watcher.is_finished(), "must wait for the in-flight call");
    receipt(&mut *store.lock().await, &goal, index);
    let error = tokio::time::timeout(Duration::from_secs(5), watcher)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.is::<Drained>());
    let state = store.lock().await;
    assert_eq!(state.held_microusd, 0);
    state.write_resume_manifest().unwrap();
    let manifest: Value =
        serde_json::from_slice(&fs::read(dir.join("resume.json")).unwrap()).unwrap();
    assert_eq!(manifest["held_microusd"], 0);
    assert!(dir.join("alert.json").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_expired_drain_grace_charges_the_unfinished_call_at_its_ceiling() {
    let (goal, store, dir) = setup();
    let store = Arc::new(Mutex::new(store));
    let index = dispatch(&mut *store.lock().await, &goal, "agent-00");
    let hold = store.lock().await.intents[index].hold_microusd;
    fs::write(
        dir.join("control.json"),
        r#"{"mode":"drain","grace_seconds":1}"#,
    )
    .unwrap();
    control_watch(store.clone(), dir.clone()).await.unwrap_err();
    let state = store.lock().await;
    assert_eq!((state.held_microusd, state.spent_microusd), (0, hold));
    assert_eq!(state.intents[index].state, "unknown_charged");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn outages_pause_after_a_bounded_streak_and_billing_errors_pause_at_once() {
    let (_goal, mut store, dir) = setup();
    let mut previous = Duration::ZERO;
    for n in 1..OUTAGE_PAUSE_AFTER {
        let wait = store
            .note_provider_failure("provider returned HTTP 503; outcome requires reconciliation");
        assert!(
            wait > previous || wait == Duration::from_secs(8),
            "backoff grows: {n}"
        );
        previous = wait;
        assert!(store.pause.is_none());
    }
    store.note_provider_ok();
    for _ in 1..OUTAGE_PAUSE_AFTER {
        store.note_provider_failure("provider transport failure; outcome unknown");
    }
    assert!(store.pause.is_none(), "a success resets the streak");
    store.note_provider_failure("provider returned HTTP 429; outcome requires reconciliation");
    assert!(
        store
            .pause
            .as_ref()
            .unwrap()
            .reason
            .contains("provider outage")
    );
    assert!(dir.join("alert.json").exists());
    store.lift_pause().unwrap();
    store.note_provider_failure("provider returned HTTP 400; outcome requires reconciliation");
    assert!(
        store.pause.is_none(),
        "a malformed request is not an outage"
    );
    store.note_provider_failure("provider returned HTTP 402; outcome requires reconciliation");
    assert!(
        store
            .pause
            .as_ref()
            .unwrap()
            .reason
            .contains("credentials or billing")
    );
    assert_eq!(outage_backoff(1), Duration::from_millis(250));
    assert_eq!(outage_backoff(99), Duration::from_secs(8));
    fs::remove_dir_all(dir).unwrap();
}

/// A process death: the Store is dropped with a settled call, a sent call and an
/// unsent reservation. Resuming must not change what was spent, must not let any
/// of them settle again, and must keep the hash chain verifiable.
#[test]
fn resume_after_a_crash_preserves_the_ledger_and_never_double_charges() {
    let (goal, mut store, dir) = setup();
    for id in ["agent-00", "agent-01", "agent-02"] {
        store.tool(&goal, id, "claim_task", json!({})).unwrap();
    }
    let settled = dispatch(&mut store, &goal, "agent-00");
    let paid = receipt(&mut store, &goal, settled);
    let sent = dispatch(&mut store, &goal, "agent-01");
    let sent_hold = store.intents[sent].hold_microusd;
    let unsent = store.reserve(&goal, "agent-02", 200).unwrap();
    let unsent_hold = store.intents[unsent].hold_microusd;
    assert_eq!(store.held_microusd, sent_hold + unsent_hold);
    let plan = resume_plan(&dir).unwrap();
    assert_eq!(
        (plan.charge_microusd, plan.release_intents.len()),
        (sent_hold, 1)
    );
    drop(store);

    let (mut store, report) = Store::resume(&dir, &goal).unwrap();
    assert_eq!(report.charge_intents.len(), 1);
    assert_eq!(
        (store.spent_microusd, store.held_microusd),
        (paid + sent_hold, 0)
    );
    assert_eq!(store.intents[sent].state, "unknown_charged");
    assert_eq!(store.intents[unsent].state, "denied");
    assert!(
        store
            .settle(
                &goal,
                sent,
                &json!({"id":"late","usage":{"prompt_tokens":1,"completion_tokens":1}})
            )
            .is_err()
    );
    assert_eq!(
        store.spent_microusd,
        paid + sent_hold,
        "a late receipt cannot charge again"
    );
    assert!(store.agents.values().all(|a| a.status == "stopped"));
    assert_eq!(
        store
            .tasks
            .iter()
            .filter(|t| t.claimed_by.is_some())
            .count(),
        3
    );

    let calls = store.agents["agent-01"].model_calls;
    store.admit("agent-01").unwrap();
    assert_eq!(
        store.agents["agent-01"].model_calls, calls,
        "counters survive the new process"
    );
    let again = dispatch(&mut store, &goal, "agent-01");
    assert_eq!(store.agents["agent-01"].model_calls, calls + 1);
    drop(store);

    // A second crash with a call in flight settles the same way, once.
    let (store, _) = Store::resume(&dir, &goal).unwrap();
    assert_eq!(store.intents[again].state, "unknown_charged");
    assert_eq!(
        store.spent_microusd,
        paid + sent_hold + store.intents[again].cost_microusd
    );
    assert_eq!(read_chain(&dir).unwrap().last_seq, store.seq);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn resume_refuses_what_it_cannot_prove() {
    let (goal, mut store, dir) = setup();
    dispatch(&mut store, &goal, "agent-00");
    drop(store);
    let log = dir.join("events.jsonl");
    let original = fs::read_to_string(&log).unwrap();
    // A torn final line is repaired and reported.
    fs::write(&log, format!("{original}{{\"seq\":99,\"ki")).unwrap();
    let (_, report) = Store::resume(&dir, &goal).unwrap();
    assert!(report.truncated_tail);
    assert!(read_chain(&dir).is_ok());
    // A rewritten event breaks the chain.
    let tampered = fs::read_to_string(&log)
        .unwrap()
        .replacen("agent-00", "agent-09", 1);
    fs::write(&log, tampered).unwrap();
    assert!(resume_plan(&dir).unwrap_err().to_string().contains("hash"));
    fs::write(&log, original).unwrap();
    // The tail repair appended a resume event, so the original log lags state.json now.
    assert!(
        resume_plan(&dir)
            .unwrap_err()
            .to_string()
            .contains("interrupted")
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn resume_refuses_an_untrustworthy_ledger_a_newer_schema_and_a_different_goal() {
    let (goal, mut store, dir) = setup();
    store.goal_status = "accounting_failed".into();
    store.record("goal_stopped", json!({})).unwrap();
    drop(store);
    assert!(
        resume_plan(&dir)
            .unwrap_err()
            .to_string()
            .contains("not trustworthy")
    );
    let state_path = dir.join("state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    state["goal_status"] = json!("stopped");
    state["schema_version"] = json!(2);
    fs::write(&state_path, state.to_string()).unwrap();
    assert!(
        resume_plan(&dir)
            .unwrap_err()
            .to_string()
            .contains("not resumable")
    );
    state["schema_version"] = json!(1);
    fs::write(&state_path, state.to_string()).unwrap();
    assert!(resume_plan(&dir).is_ok());
    let mut other = goal.clone_with_task("zz");
    other.tasks[0].id = "zz".into();
    assert!(load_image(&dir, &other).is_err());
    fs::remove_dir_all(dir).unwrap();
}

impl Goal {
    fn clone_with_task(&self, id: &str) -> Goal {
        let mut value = serde_json::to_value(self).unwrap();
        value["tasks"][0]["id"] = json!(id);
        serde_json::from_value(value).unwrap()
    }
}

#[test]
fn control_json_the_pause_file_and_the_peak_window_are_one_pause() {
    let (_goal, mut store, dir) = setup();
    let now = 1_700_000_000; // a Tuesday, outside every peak window
    assert!(pause_retry_after(&dir, now).is_none());
    store.begin_pause("pause", "operator").unwrap();
    assert!(
        pause_retry_after(&dir, now).is_some(),
        "a broker pause is visible through control.json"
    );
    store.lift_pause().unwrap();
    assert!(pause_retry_after(&dir, now).is_none());
    fs::write(dir.join("PAUSE"), "").unwrap();
    assert_eq!(
        pause_retry_after(&dir, now),
        Some(30),
        "the PAUSE file is an input to the same answer"
    );
    fs::remove_file(dir.join("PAUSE")).unwrap();
    // The operator writes control.json directly; the deadline clock and handler both see it.
    fs::write(dir.join("control.json"), r#"{"mode":"drain"}"#).unwrap();
    assert!(pause_retry_after(&dir, now).is_some());
    fs::write(dir.join("control.json"), r#"{"mode":"run"}"#).unwrap();
    assert!(pause_retry_after(&dir, now).is_none());
    // 2026-10-28 02:00 UTC is a Wednesday peak window: the env input pauses and is bounded to 60 s.
    assert_eq!(peak_retry_after(1_793_152_800).map(|s| s > 0), Some(true));
    fs::remove_dir_all(dir).unwrap();
}
