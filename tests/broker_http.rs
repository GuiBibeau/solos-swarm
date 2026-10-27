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
use axum::{Json, Router, extract::State, routing::post};
use broker::{Broker, Goal, Provider, Store, router};
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

struct FixtureProvider {
    client: reqwest::Client,
}
impl Provider for FixtureProvider {
    fn dispatch(&self, body: Value) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> {
        let client = self.client.clone();
        Box::pin(async move {
            Ok(client
                .post("http://fixture/v1/chat/completions")
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?)
        })
    }
}

async fn receive(State(calls): State<Arc<AtomicUsize>>, Json(body): Json<Value>) -> Json<Value> {
    calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(body["model"], "deepseek-flash");
    assert_eq!(body["max_tokens"], 2048);
    assert_eq!(body["stream"], false);
    Json(
        json!({"id":"fixture-receipt-1","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":90,"prompt_cache_miss_tokens":10,"completion_tokens":50,"total_tokens":150}}),
    )
}

#[tokio::test]
async fn unix_broker_denials_dispatch_nothing_and_allowed_call_settles_once() {
    let dir = std::env::temp_dir().join(format!(
        "swarm-http-{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    ));
    fs::create_dir(&dir).unwrap();
    let goal: Goal = serde_json::from_value(json!({
        "schema_version":1,"intent":"HTTP receiver controls","worker_prompt":"test","synthesis_prompt":"test","scope":"synthetic-fixtures-only","model":"deepseek-flash",
        "agent_count":1,"deadline_seconds":10,"budget_microusd":1000000,"max_calls_per_agent":8,
        "max_input_bytes":65536,"max_output_tokens":2048,"input_price_microusd_per_million":300000,
        "output_price_microusd_per_million":1200000,
        "cache_hit_price_microusd_per_million":6000,
        "grants":["claim_task","board_read","board_post","submit_finding","read_findings","submit_artifact"],
        "tasks":[{"id":"task-0","input":{}}],"seed":1,"catalog_version":"test","scorer_version":"1"
    })).unwrap();
    goal.validate().unwrap();
    let mut state = Store::new(&dir, &goal).unwrap();
    state.goal_status = "running".into();
    let token = state.admit("agent-00").unwrap();
    let state = Arc::new(Mutex::new(state));
    let calls = Arc::new(AtomicUsize::new(0));
    let upstream_socket = dir.join("p.sock");
    let upstream_listener = UnixListener::bind(&upstream_socket).unwrap();
    let upstream_router = Router::new()
        .route("/v1/chat/completions", post(receive))
        .with_state(calls.clone());
    let upstream = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_router)
            .await
            .unwrap();
    });
    let provider = FixtureProvider {
        client: reqwest::Client::builder()
            .unix_socket(upstream_socket)
            .build()
            .unwrap(),
    };
    let socket = dir.join("b.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let app = router(Broker {
        goal: Arc::new(goal),
        store: state.clone(),
        provider: Arc::new(provider),
        slots: Arc::new(Semaphore::new(1)),
        client: reqwest::Client::new(),
        code: None,
    });
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::builder()
        .unix_socket(socket)
        .build()
        .unwrap();
    let body = json!({"model":"deepseek-flash","messages":[{"role":"user","content":"test"}],"max_tokens":4096});
    let missing_auth = client
        .post("http://broker/model")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(missing_auth.status(), 403);
    let mut wrong_model = body.clone();
    wrong_model["model"] = json!("unadmitted-model");
    let mut unknown_tool = body.clone();
    unknown_tool["tools"] =
        json!([{"type":"function","function":{"name":"bash","parameters":{"type":"object"}}}]);
    for denied in [wrong_model, unknown_tool] {
        assert_eq!(
            client
                .post("http://broker/model")
                .bearer_auth(&token)
                .json(&denied)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    assert_eq!(
        client
            .post("http://broker/tool/bash")
            .bearer_auth(&token)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    state.lock().await.spent_microusd = 1_000_000;
    assert_eq!(
        client
            .post("http://broker/model")
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    state.lock().await.spent_microusd = 0;
    state.lock().await.goal_status = "stopped".into();
    assert_eq!(
        client
            .post("http://broker/model")
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "denied requests reached the upstream receiver"
    );
    assert!(state.lock().await.intents.is_empty());
    state.lock().await.goal_status = "running".into();
    let response = client
        .post("http://broker/model")
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let receipt: Value = response.json().await.unwrap();
    assert_eq!(receipt["id"], "fixture-receipt-1");
    assert_eq!(receipt["swarm_cost_microusd"], 64);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    {
        let state = state.lock().await;
        assert_eq!(state.intents.len(), 1);
        assert_eq!(state.intents[0].state, "completed");
        assert_eq!(state.spent_microusd, 64);
        assert_eq!(
            state.intents[0].usage.as_ref().unwrap()["cached_input_tokens"],
            90
        );
        assert_eq!(state.held_microusd, 0);
    }
    server.abort();
    upstream.abort();
    let _ = server.await;
    let _ = upstream.await;
    fs::remove_dir_all(dir).unwrap();
}
