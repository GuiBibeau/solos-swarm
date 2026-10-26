//! Evidence-driven broker behaviour: coordinator routing, budget notices and the
//! finalization allowance, partial findings, host synthesis, cache-aware ledger.
use super::*;

fn setup(task_budget: Option<u64>) -> (Goal, Store, PathBuf) {
    let tasks: Vec<Value> = [("a00", "coordinator"), ("a01", "worker"), ("a02", "worker")]
        .iter()
        .map(|(id, role)| json!({"id":id,"budget_microusd":task_budget,"input":{"title":id,"role":role,"seed_urls":["https://e.test/x"],"output_contract":{"result_key":"papers"}}}))
        .collect();
    let goal: Goal = serde_json::from_value(json!({"schema_version":1,"intent":"test","worker_prompt":"test","synthesis_prompt":"test","scope":"synthetic-fixtures-only","model":"deepseek-flash","agent_count":3,"deadline_seconds":10,"budget_microusd":3_000_000,"max_calls_per_agent":100,"max_input_bytes":65536,"max_output_tokens":2048,"input_price_microusd_per_million":300000,"output_price_microusd_per_million":1200000,"cache_hit_price_microusd_per_million":30000,"grants":["claim_task","board_read","board_post","submit_finding","read_findings","submit_artifact"],"tasks":tasks,"seed":1,"catalog_version":"test","scorer_version":"1"})).unwrap();
    goal.validate().unwrap();
    let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&dir).unwrap();
    let mut store = Store::new(&dir, &goal).unwrap();
    store.goal_status = "running".into();
    for id in ["agent-00", "agent-01", "agent-02"] {
        store.admit(id).unwrap();
    }
    (goal, store, dir)
}
fn claim(store: &mut Store, goal: &Goal, id: &str) -> String {
    store.tool(goal, id, "claim_task", json!({})).unwrap()["id"]
        .as_str()
        .unwrap()
        .into()
}

#[test]
fn coordinator_task_goes_to_agent_00_whatever_the_claim_order() {
    let (goal, mut store, dir) = setup(None);
    assert_eq!(claim(&mut store, &goal, "agent-01"), "a01");
    assert_eq!(claim(&mut store, &goal, "agent-02"), "a02");
    assert_eq!(claim(&mut store, &goal, "agent-00"), "a00");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cache_hits_settle_cheaper_but_never_above_the_hold() {
    let (goal, mut store, dir) = setup(None);
    claim(&mut store, &goal, "agent-01");
    let index = store.reserve(&goal, "agent-01", 4000).unwrap();
    store.intents[index].state = "dispatched".into();
    let usage =
        json!({"prompt_tokens":100_000,"completion_tokens":100,"prompt_cache_hit_tokens":99_000});
    let cost = store
        .settle(&goal, index, &json!({"id":"r1","usage":usage}))
        .unwrap();
    assert_eq!(
        cost,
        2_970 + 300 + 120,
        "99k hit at 0.03, 1k miss at 0.3, 100 out"
    );
    assert_eq!(goal.settled_cost(100_000, 0, 100), 30_000 + 120);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn notices_start_at_70_percent_and_run_code_is_refused_at_90() {
    let (goal, mut store, dir) = setup(Some(1_000_000));
    claim(&mut store, &goal, "agent-01");
    let result = json!({"exit_code":0});
    assert!(
        store
            .with_notice(&goal, "agent-01", "run_code", result.clone())
            .get("budget_notice")
            .is_none()
    );
    let spend = |store: &mut Store, microusd: u64| {
        store.intents.clear();
        store.intents.push(Intent {
            id: "i".into(),
            agent_id: "agent-01".into(),
            state: "completed".into(),
            hold_microusd: 0,
            cost_microusd: microusd,
            usage: None,
            provider: "deepseek".into(),
            request_id: None,
        });
    };
    spend(&mut store, 750_000);
    let soft = store.with_notice(&goal, "agent-01", "run_code", result.clone());
    assert!(soft["budget_notice"].as_str().unwrap().contains("WARNING"));
    assert!(
        store
            .budget_gate(&goal, "agent-01", "run_code")
            .unwrap()
            .is_none()
    );
    spend(&mut store, 920_000);
    let gate = store
        .budget_gate(&goal, "agent-01", "run_code")
        .unwrap()
        .unwrap();
    assert_eq!(gate["blocked"], true);
    assert!(
        gate["budget_notice"]
            .as_str()
            .unwrap()
            .contains("submit_finding NOW")
    );
    assert!(
        store.reserve(&goal, "agent-01", 200).is_ok(),
        "the allowance still buys the filing turn"
    );
    assert!(
        fs::read_to_string(dir.join("events.jsonl"))
            .unwrap()
            .contains("tool_budget_blocked")
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn exhausted_agent_with_work_files_a_partial_not_an_empty_defer() {
    let (goal, mut store, dir) = setup(Some(1_000_000));
    claim(&mut store, &goal, "agent-01");
    store
        .tool(
            &goal,
            "agent-01",
            "board_post",
            json!({"message":"candidate: RMT cleaning beats raw covariance"}),
        )
        .unwrap();
    store.agents.get_mut("agent-01").unwrap().model_calls = 120;
    store.note_work(
        "agent-01",
        "python proof.py",
        &json!({"exit_code":0,"stdout":"kill criterion met"}),
    );
    store
        .defer_task(&goal, "agent-01", "Task budget envelope exhausted")
        .unwrap();
    let record = &store.findings[0].result["papers"][0];
    assert_eq!(record["severity"], "defer");
    assert_eq!(record["output"], "DEFERRED: Task budget envelope exhausted");
    let text = record["description"].as_str().unwrap();
    assert!(
        text.contains("PARTIAL")
            && text.contains("RMT cleaning")
            && text.contains("python proof.py"),
        "{text}"
    );
    assert_eq!(record["evidence"], record["description"]);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn host_files_unfiled_tasks_and_synthesizes_the_exact_union() {
    let (goal, mut store, dir) = setup(None);
    for id in ["agent-00", "agent-01", "agent-02"] {
        claim(&mut store, &goal, id);
    }
    store
        .tool(
            &goal,
            "agent-01",
            "submit_finding",
            json!({"task_id":"a01","result":{"papers":[{"id":"x"},{"id":"y"}]}}),
        )
        .unwrap();
    assert_eq!(store.file_partial_findings(&goal, "deadline").unwrap(), 2);
    assert_eq!(store.file_partial_findings(&goal, "deadline").unwrap(), 0);
    assert!(store.synthesize_artifact("deadline").unwrap());
    assert!(!store.synthesize_artifact("again").unwrap());
    let artifact = store.artifact.clone().unwrap();
    assert_eq!(artifact["papers"].as_array().unwrap().len(), 4);
    let (key, expected) = finding_records(&store.findings).unwrap();
    assert!(same_records(&artifact, &key, &expected));
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(dir.join("artifacts/final.json")).unwrap())
            .unwrap(),
        artifact
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn oversized_response_headers_are_trimmed_not_fatal() {
    let mut map = reqwest::header::HeaderMap::new();
    map.insert("a-small", "1".parse().unwrap());
    let (kept, truncated) = bounded_headers(&map);
    assert!(!truncated && kept.len() == 1);
    for i in 0..40 {
        map.insert(
            reqwest::header::HeaderName::from_bytes(format!("x-h{i}").as_bytes()).unwrap(),
            "v".repeat(300).parse().unwrap(),
        );
    }
    let (kept, truncated) = bounded_headers(&map);
    assert!(truncated && serde_json::to_vec(&kept).unwrap().len() <= FETCH_HEADER_BYTES);
}
