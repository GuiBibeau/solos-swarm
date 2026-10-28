//! A provider outage must pause the run, not burn the agents' call ceilings.
#[allow(dead_code)]
#[path = "../src/broker.rs"]
mod broker;
#[allow(dead_code)]
#[path = "../src/code.rs"]
mod code;
#[allow(dead_code)]
#[path = "../src/progress.rs"]
mod progress;

use anyhow::Result;
use broker::{Broker, Goal, OUTAGE_PAUSE_AFTER, Provider, Store, router};
use serde_json::{Value, json};
use std::{
    fs,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    net::UnixListener,
    sync::{Mutex, Semaphore},
};

struct Down(Arc<AtomicUsize>);
impl Provider for Down {
    fn dispatch(&self, _: Value) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(anyhow::anyhow!(
                "provider returned HTTP 503; outcome requires reconciliation"
            ))
        })
    }
}

#[tokio::test]
async fn five_outage_errors_pause_the_run_and_the_sixth_call_is_never_reserved() {
    let dir =
        std::env::temp_dir().join(format!("outage-{}", &uuid::Uuid::new_v4().to_string()[..8]));
    fs::create_dir(&dir).unwrap();
    let goal: Goal = serde_json::from_value(json!({
        "schema_version":1,"intent":"outage","worker_prompt":"t","synthesis_prompt":"t","scope":"synthetic-fixtures-only","model":"deepseek-flash",
        "agent_count":2,"deadline_seconds":10,"budget_microusd":1000000,"max_calls_per_agent":8,
        "max_input_bytes":65536,"max_output_tokens":2048,"input_price_microusd_per_million":300000,
        "output_price_microusd_per_million":1200000,
        "grants":["claim_task","submit_finding"],
        "tasks":[{"id":"a","input":{}},{"id":"b","input":{}}],"seed":1,"catalog_version":"test","scorer_version":"1"
    })).unwrap();
    let mut state = Store::new(&dir, &goal).unwrap();
    state.goal_status = "running".into();
    let token = state.admit("agent-00").unwrap();
    let state = Arc::new(Mutex::new(state));
    let calls = Arc::new(AtomicUsize::new(0));
    let socket = dir.join("b.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let app = router(Broker {
        goal: Arc::new(goal),
        store: state.clone(),
        provider: Arc::new(Down(calls.clone())),
        slots: Arc::new(Semaphore::new(1)),
        client: reqwest::Client::new(),
        code: None,
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder()
        .unix_socket(socket)
        .build()
        .unwrap();
    let body = json!({"model":"deepseek-flash","messages":[{"role":"user","content":"x"}]});
    let mut last = String::new();
    // The agent retries as hard as it can; the broker, not the agent, decides when to stop.
    for _ in 0..(OUTAGE_PAUSE_AFTER + 2) {
        let response = client
            .post("http://broker/model")
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        last = response.json::<Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .to_owned();
    }
    assert!(last.contains("run pause"), "{last}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        OUTAGE_PAUSE_AFTER as usize,
        "the provider was contacted only until the pause"
    );
    let store = state.lock().await;
    assert_eq!(
        store.agents["agent-00"].model_calls, OUTAGE_PAUSE_AFTER as usize,
        "paused refusals do not burn the call ceiling"
    );
    assert_eq!((store.held_microusd, store.spent_microusd), (0, 0));
    assert_eq!(store.pause.as_ref().unwrap().mode, "pause");
    assert!(dir.join("alert.json").exists());
    drop(store);
    // An operator lifts the pause and the next call reaches the provider again.
    state.lock().await.lift_pause().unwrap();
    client
        .post("http://broker/model")
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        OUTAGE_PAUSE_AFTER as usize + 1
    );
    fs::remove_dir_all(dir).unwrap();
}
