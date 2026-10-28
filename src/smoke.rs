//! Zero-spend release smoke: the candidate's own broker, ledger, pause gate,
//! drain and resume run against a scripted provider. It needs no sandbox, no
//! network and no key, so a promotion gate can run it on any host.
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use solos_swarm::broker::{self, Broker, Drained, Goal, Provider, Store};
use std::{future::Future, path::Path, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    net::UnixListener,
    sync::{Mutex, Semaphore},
};

struct Scripted;
impl Provider for Scripted {
    fn dispatch(&self, _: Value) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> {
        Box::pin(async {
            Ok(
                json!({"id":"smoke-receipt","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":0,"prompt_cache_miss_tokens":100,"completion_tokens":50,"total_tokens":150}}),
            )
        })
    }
}

pub async fn run(dir: &Path) -> Result<Value> {
    std::fs::create_dir_all(dir)?;
    let goal: Goal = serde_json::from_value(json!({
        "schema_version":1,"intent":"release smoke","worker_prompt":"smoke","synthesis_prompt":"smoke",
        "scope":"synthetic-fixtures-only","model":"deepseek-flash","agent_count":1,"deadline_seconds":60,
        "budget_microusd":1_000_000,"max_calls_per_agent":8,"max_input_bytes":65536,"max_output_tokens":2048,
        "input_price_microusd_per_million":300_000,"output_price_microusd_per_million":1_200_000,
        "grants":["claim_task","board_read","board_post","submit_finding","read_findings","submit_artifact"],"tasks":[{"id":"smoke","input":{}}],
        "seed":1,"catalog_version":"smoke","scorer_version":"1"
    }))?;
    goal.validate()?;
    std::fs::write(dir.join("config.yaml"), serde_json::to_vec(&goal)?)?;
    let mut store = Store::new(dir, &goal)?;
    store.goal_status = "running".into();
    let token = store.admit("agent-00")?;
    let store = Arc::new(Mutex::new(store));
    let socket = dir.join("smoke.sock");
    let app = broker::router(Broker {
        goal: Arc::new(goal.clone()),
        store: store.clone(),
        provider: Arc::new(Scripted),
        slots: Arc::new(Semaphore::new(1)),
        client: reqwest::Client::new(),
        code: None,
    });
    let listener = UnixListener::bind(&socket)?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let client = reqwest::Client::builder().unix_socket(socket).build()?;
    let post = |path: &'static str, body: Value| {
        let (client, token) = (client.clone(), token.clone());
        async move {
            let response = client
                .post(format!("http://broker/{path}"))
                .bearer_auth(token)
                .json(&body)
                .send()
                .await?;
            Ok::<_, anyhow::Error>((response.status().as_u16(), response.json::<Value>().await?))
        }
    };
    let body = json!({"model":"deepseek-flash","messages":[{"role":"user","content":"smoke"}]});
    let mut checks = vec![];
    let mut check = |name: &str, ok: bool| checks.push(json!({"name":name,"ok":ok}));

    check("claim", post("tool/claim_task", json!({})).await?.0 == 200);
    let (status, reply) = post("model", body.clone()).await?;
    let spent = store.lock().await.spent_microusd;
    check(
        "settle_once",
        status == 200 && spent == reply["swarm_cost_microusd"] && spent > 0,
    );
    check("no_held_budget", store.lock().await.held_microusd == 0);

    store.lock().await.begin_pause("pause", "smoke")?;
    let calls = store.lock().await.agents["agent-00"].model_calls;
    let (status, _) = post("model", body.clone()).await?;
    check(
        "pause_refuses_model",
        status == 403 && store.lock().await.agents["agent-00"].model_calls == calls,
    );
    store.lock().await.lift_pause()?;
    check("unpause_admits", post("model", body).await?.0 == 200);

    std::fs::write(
        dir.join("control.json"),
        r#"{"mode":"drain","reason":"smoke"}"#,
    )?;
    let drained = tokio::time::timeout(
        Duration::from_secs(10),
        broker::control_watch(store.clone(), dir.into()),
    )
    .await;
    check(
        "drain_settles",
        matches!(&drained, Ok(Err(e)) if e.is::<Drained>()),
    );
    let (spent, held) = {
        let state = store.lock().await;
        state.write_resume_manifest()?;
        (state.spent_microusd, state.held_microusd)
    };
    check("drain_holds_nothing", held == 0);
    server.abort();
    drop(store);

    let plan = broker::resume_plan(dir)?;
    let (resumed, _) = Store::resume(dir, &goal)?;
    check(
        "resume_keeps_ledger",
        plan.charge_microusd == 0 && resumed.spent_microusd == spent,
    );
    let pass = checks.iter().all(|c| c["ok"] == true);
    ensure!(pass, "smoke failed: {}", json!(checks));
    Ok(json!({"verdict":"PASS","checks":checks,"spent_microusd":spent,"live_spend":0}))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn the_smoke_passes_offline() {
        let dir = std::env::temp_dir().join(format!("smoke-{}", uuid::Uuid::new_v4()));
        let report = super::run(&dir).await.unwrap();
        assert_eq!(report["verdict"], "PASS");
        assert_eq!(report["live_spend"], 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
