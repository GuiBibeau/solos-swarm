use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as RoutePath, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, Semaphore};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn file_digest(path: &Path) -> Result<String> {
    Ok(digest(&fs::read(path)?))
}
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    File::open(path.parent().context("missing parent")?)?.sync_all()?;
    Ok(())
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Goal {
    pub schema_version: u32,
    pub intent: String,
    pub worker_prompt: String,
    pub synthesis_prompt: String,
    pub scope: String,
    pub model: String,
    pub agent_count: usize,
    pub deadline_seconds: u64,
    pub budget_microusd: u64,
    pub max_calls_per_agent: usize,
    pub max_input_bytes: usize,
    pub max_output_tokens: u64,
    pub input_price_microusd_per_million: u64,
    pub output_price_microusd_per_million: u64,
    pub grants: Vec<String>,
    pub tasks: Vec<Task>,
    pub seed: u64,
    pub catalog_version: String,
    pub scorer_version: String,
}

impl Goal {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && !self.intent.is_empty(),
            "invalid Goal contract"
        );
        ensure!(
            !self.worker_prompt.is_empty()
                && !self.synthesis_prompt.is_empty()
                && self.worker_prompt.len() + self.synthesis_prompt.len() <= 8192,
            "invalid Goal prompts"
        );
        ensure!(
            self.scope == "synthetic-fixtures-only",
            "preview permits synthetic fixtures only"
        );
        ensure!(
            self.model == "deepseek-flash",
            "model outside deployment allowlist"
        );
        ensure!(
            (1..=10).contains(&self.agent_count) && self.tasks.len() == self.agent_count,
            "one Task per admitted agent required (maximum ten)"
        );
        ensure!(
            (1..=600).contains(&self.deadline_seconds),
            "deadline must be 1..600 seconds"
        );
        ensure!(
            (1..=1_000_000).contains(&self.budget_microusd),
            "Goal spend must be 1..1000000 microusd"
        );
        ensure!(
            (1..=12).contains(&self.max_calls_per_agent),
            "invalid turn ceiling"
        );
        ensure!(
            (1..=65_536).contains(&self.max_input_bytes)
                && (1..=4096).contains(&self.max_output_tokens),
            "invalid request limits"
        );
        ensure!(
            self.input_price_microusd_per_million >= 300_000
                && self.output_price_microusd_per_million >= 1_200_000,
            "price table below admitted conservative peak rates"
        );
        ensure!(
            self.input_price_microusd_per_million <= 10_000_000
                && self.output_price_microusd_per_million <= 10_000_000,
            "invalid price table"
        );
        let allowed = [
            "claim_task",
            "board_read",
            "board_post",
            "submit_finding",
            "read_findings",
            "submit_artifact",
        ];
        ensure!(
            self.grants.len() == allowed.len()
                && allowed
                    .iter()
                    .all(|name| self.grants.iter().any(|g| g == name)),
            "missing or invalid agent definition"
        );
        let mut ids = BTreeSet::new();
        for task in &self.tasks {
            ensure!(
                !task.id.is_empty() && task.id.len() <= 128 && ids.insert(&task.id),
                "invalid or duplicate Task"
            );
            ensure!(
                task.claimed_by.is_none() && task.status == "open",
                "operator cannot import claimed Tasks"
            );
            ensure!(
                serde_json::to_vec(&task.input)?.len() <= self.max_input_bytes / 2,
                "Task input exceeds admitted limit"
            );
        }
        Ok(())
    }
    fn cost(&self, input: u64, output: u64) -> u64 {
        // Charge all input at peak cache-miss rates; this deliberately overstates the invoice.
        (input * self.input_price_microusd_per_million).div_ceil(1_000_000)
            + (output * self.output_price_microusd_per_million).div_ceil(1_000_000)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub input: Value,
    #[serde(default)]
    pub claimed_by: Option<String>,
    #[serde(default = "open")]
    pub status: String,
}
fn open() -> String {
    "open".into()
}

#[derive(Serialize)]
pub struct Agent {
    pub started_ms: u64,
    pub stopped_ms: Option<u64>,
    pub successful_model_calls: usize,
    pub model_calls: usize,
    pub status: String,
}
#[derive(Clone, Serialize)]
pub struct Finding {
    pub agent_id: String,
    pub task_id: String,
    pub result: Value,
}
#[derive(Serialize)]
pub struct Intent {
    pub id: String,
    pub agent_id: String,
    pub state: String,
    pub hold_microusd: u64,
    pub cost_microusd: u64,
    pub usage: Option<Value>,
    pub provider: String,
    pub request_id: Option<String>,
}
#[derive(Serialize)]
pub struct Store {
    pub schema_version: u32,
    pub goal_status: String,
    pub agents: BTreeMap<String, Agent>,
    pub tasks: Vec<Task>,
    pub findings: Vec<Finding>,
    pub intents: Vec<Intent>,
    pub board: Vec<Value>,
    pub spent_microusd: u64,
    pub held_microusd: u64,
    pub artifact: Option<Value>,
    #[serde(skip)]
    pub dir: PathBuf,
    #[serde(skip)]
    log: File,
    #[serde(skip)]
    seq: u64,
    #[serde(skip)]
    previous_hash: String,
    #[serde(skip)]
    tokens: BTreeMap<String, String>,
}
impl Store {
    pub fn new(dir: &Path, goal: &Goal) -> Result<Self> {
        fs::create_dir_all(dir.join("artifacts"))?;
        Ok(Self {
            schema_version: 1,
            goal_status: "registered".into(),
            agents: BTreeMap::new(),
            tasks: goal.tasks.clone(),
            findings: vec![],
            intents: vec![],
            board: vec![],
            spent_microusd: 0,
            held_microusd: 0,
            artifact: None,
            dir: dir.into(),
            log: OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(dir.join("events.jsonl"))?,
            seq: 0,
            previous_hash: "0".repeat(64),
            tokens: BTreeMap::new(),
        })
    }
    pub fn record(&mut self, kind: &str, data: Value) -> Result<()> {
        self.seq += 1;
        let mut event = json!({"seq":self.seq,"time_ms":now_ms(),"kind":kind,"data":data});
        let hash =
            digest(format!("{}{}", self.previous_hash, serde_json::to_string(&event)?).as_bytes());
        event["prev_hash"] = json!(self.previous_hash);
        event["hash"] = json!(hash);
        let result = (|| -> Result<()> {
            serde_json::to_writer(&mut self.log, &event)?;
            self.log.write_all(b"\n")?;
            self.log.sync_data()?;
            self.previous_hash = hash;
            write_json(&self.dir.join("state.json"), self)
        })();
        if result.is_err() {
            self.goal_status = "storage_failed".into();
        }
        result
    }
    pub fn admit(&mut self, id: &str) -> Result<String> {
        let token = uuid::Uuid::new_v4().to_string();
        self.tokens.insert(token.clone(), id.into());
        self.agents.insert(
            id.into(),
            Agent {
                started_ms: now_ms(),
                stopped_ms: None,
                successful_model_calls: 0,
                model_calls: 0,
                status: "running".into(),
            },
        );
        self.record("agent_started", json!({"agent_id":id}))?;
        Ok(token)
    }
    fn authenticate(&self, headers: &HeaderMap) -> Result<String> {
        let token = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .context("missing identity")?;
        self.tokens.get(token).cloned().context("invalid identity")
    }
    fn active(&self, id: &str) -> Result<()> {
        ensure!(
            self.goal_status == "running",
            "Goal is stopped or not running"
        );
        ensure!(
            self.agents.get(id).is_some_and(|a| a.status == "running"),
            "allocation is revoked"
        );
        Ok(())
    }
    fn reserve(&mut self, goal: &Goal, id: &str, input_bytes: usize) -> Result<usize> {
        self.active(id)?;
        let agent = self.agents.get(id).context("unknown allocation")?;
        ensure!(
            agent.model_calls < goal.max_calls_per_agent,
            "agent turn ceiling exhausted"
        );
        // UTF-8 bytes plus chat framing overbounds byte-BPE input; reject usage above this bound.
        let hold = goal.cost(input_bytes as u64 + 4096, goal.max_output_tokens);
        ensure!(
            self.spent_microusd + self.held_microusd + hold <= goal.budget_microusd,
            "Goal spend ceiling exhausted"
        );
        let agent_committed: u64 = self
            .intents
            .iter()
            .filter(|i| i.agent_id == id)
            .map(|i| {
                if i.state == "completed" {
                    i.cost_microusd
                } else {
                    i.hold_microusd
                }
            })
            .sum();
        ensure!(
            agent_committed + hold <= goal.budget_microusd / goal.agent_count as u64,
            "Task budget envelope exhausted"
        );
        let index = self.intents.len();
        self.intents.push(Intent {
            id: format!("intent-{index:04}"),
            agent_id: id.into(),
            state: "reserved".into(),
            hold_microusd: hold,
            cost_microusd: 0,
            usage: None,
            provider: "deepseek".into(),
            request_id: None,
        });
        self.held_microusd += hold;
        self.agents.get_mut(id).unwrap().model_calls += 1;
        self.record(
            "provider_reserved",
            json!({"agent_id":id,"intent_id":self.intents[index].id,"hold_microusd":hold}),
        )?;
        Ok(index)
    }
    fn settle(&mut self, goal: &Goal, index: usize, response: &Value) -> Result<u64> {
        ensure!(
            self.intents[index].state == "dispatched",
            "intent already settled or not dispatched"
        );
        let input = response["usage"]["prompt_tokens"]
            .as_u64()
            .context("missing input usage")?;
        let output = response["usage"]["completion_tokens"]
            .as_u64()
            .context("missing output usage")?;
        ensure!(
            input <= 1_000_000 && output <= goal.max_output_tokens,
            "provider usage exceeds contract"
        );
        let request_id = response["id"]
            .as_str()
            .filter(|v| !v.is_empty())
            .context("missing provider receipt")?
            .to_owned();
        let cost = goal.cost(input, output);
        let intent = &mut self.intents[index];
        let exceeds_hold = cost > intent.hold_microusd;
        let id = intent.agent_id.clone();
        self.held_microusd -= intent.hold_microusd;
        self.spent_microusd += cost;
        intent.cost_microusd = cost;
        intent.usage = Some(json!({"input_tokens":input,"output_tokens":output}));
        intent.request_id = Some(request_id);
        intent.state = "completed".into();
        self.agents.get_mut(&id).unwrap().successful_model_calls += 1;
        if exceeds_hold || self.spent_microusd + self.held_microusd > goal.budget_microusd {
            self.goal_status = "accounting_failed".into();
        }
        self.record("provider_settled", json!({"intent_id":self.intents[index].id,"cost_microusd":cost,"usage":self.intents[index].usage,"request_id":self.intents[index].request_id}))?;
        ensure!(
            !exceeds_hold && self.goal_status != "accounting_failed",
            "provider exceeded reservation; admission stopped"
        );
        Ok(cost)
    }
    fn tool(&mut self, goal: &Goal, id: &str, name: &str, args: Value) -> Result<Value> {
        self.active(id)?;
        ensure!(
            goal.grants.iter().any(|g| g == name),
            "tool outside admitted grants"
        );
        if name == "read_findings" || name == "submit_artifact" {
            ensure!(id == "agent-00", "coordinator grant required");
        }
        let result = match name {
            "claim_task" => {
                ensure!(
                    args.as_object().is_some_and(|o| o.is_empty()),
                    "claim takes no parameters"
                );
                let current = self
                    .tasks
                    .iter()
                    .position(|t| t.claimed_by.as_deref() == Some(id));
                let index = current
                    .or_else(|| self.tasks.iter().position(|t| t.status == "open"))
                    .context("no open Task")?;
                let task = &mut self.tasks[index];
                if current.is_none() {
                    task.claimed_by = Some(id.into());
                    task.status = "claimed".into();
                }
                json!({"id":task.id,"input":task.input})
            }
            "board_read" => json!({"posts":self.board}),
            "board_post" => {
                let message = args["message"].as_str().context("missing Board message")?;
                ensure!(
                    message.len() <= 4096 && self.board.len() < 100,
                    "Board capacity exceeded"
                );
                let post = json!({"seq":self.board.len()+1,"agent_id":id,"message":message});
                self.board.push(post.clone());
                post
            }
            "submit_finding" => {
                let task_id = args["task_id"].as_str().context("missing Task ID")?;
                let task = self
                    .tasks
                    .iter_mut()
                    .find(|t| t.id == task_id)
                    .context("unknown Task")?;
                ensure!(
                    task.claimed_by.as_deref() == Some(id),
                    "Task belongs to another agent"
                );
                ensure!(args["result"].is_object(), "Finding must be an object");
                if let Some(existing) = self.findings.iter().find(|f| f.task_id == task_id) {
                    ensure!(
                        existing.result == args["result"],
                        "immutable Finding conflict"
                    );
                } else {
                    task.status = "completed".into();
                    self.findings.push(Finding {
                        agent_id: id.into(),
                        task_id: task_id.into(),
                        result: args["result"].clone(),
                    });
                }
                json!({"accepted":true,"task_id":task_id})
            }
            "read_findings" => json!({"findings":self.findings}),
            "submit_artifact" => {
                ensure!(
                    self.findings.len() == goal.agent_count,
                    "all Task results required before synthesis"
                );
                ensure!(args["artifact"].is_object(), "artifact must be an object");
                ensure!(self.artifact.is_none(), "Attempt already submitted");
                write_json(&self.dir.join("artifacts/final.json"), &args["artifact"])?;
                self.artifact = Some(args["artifact"].clone());
                json!({"accepted":true})
            }
            _ => bail!("unknown tool"),
        };
        self.record(
            "tool_completed",
            json!({"agent_id":id,"tool":name,"args":args,"result":result}),
        )?;
        Ok(result)
    }
}

type Dispatch = Pin<Box<dyn Future<Output = Result<Value>> + Send>>;
pub trait Provider: Send + Sync {
    fn dispatch(&self, body: Value) -> Dispatch;
}
pub struct DeepSeek {
    pub client: reqwest::Client,
    pub key: String,
}
impl Provider for DeepSeek {
    fn dispatch(&self, body: Value) -> Dispatch {
        let client = self.client.clone();
        let key = self.key.clone();
        Box::pin(async move {
            let mut response = client
                .post("https://api.deepseek.com/chat/completions")
                .bearer_auth(key)
                .json(&body)
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("provider transport failure; outcome unknown"))?;
            ensure!(
                response.status().is_success(),
                "provider returned HTTP {}; outcome requires reconciliation",
                response.status().as_u16()
            );
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| anyhow::anyhow!("provider response interrupted"))?
            {
                ensure!(
                    bytes.len() + chunk.len() <= 262_144,
                    "provider response exceeds limit"
                );
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes).context("invalid provider response")
        })
    }
}
#[derive(Clone)]
pub struct Broker {
    pub goal: Arc<Goal>,
    pub store: Arc<Mutex<Store>>,
    pub provider: Arc<dyn Provider>,
    pub slots: Arc<Semaphore>,
}
type ApiResult = std::result::Result<Json<Value>, (StatusCode, Json<Value>)>;
fn denied(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(json!({"error":error.to_string()})),
    )
}

async fn model(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> ApiResult {
    let input_bytes = serde_json::to_vec(&body)
        .map_err(|e| denied(e.into()))?
        .len();
    if input_bytes > broker.goal.max_input_bytes {
        return Err(denied(anyhow::anyhow!("request exceeds Goal limit")));
    }
    let id = {
        let store = broker.store.lock().await;
        store.authenticate(&headers).map_err(denied)?
    };
    if body["model"] != broker.goal.model
        || !body["messages"].is_array()
        || body["messages"].as_array().unwrap().is_empty()
    {
        return Err(denied(anyhow::anyhow!(
            "model or message shape outside contract"
        )));
    }
    if let Some(tools) = body.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| denied(anyhow::anyhow!("invalid tool schema")))?;
        for tool in tools {
            let name = tool["function"]["name"]
                .as_str()
                .ok_or_else(|| denied(anyhow::anyhow!("invalid tool name")))?;
            if !broker.goal.grants.iter().any(|g| g == name) {
                return Err(denied(anyhow::anyhow!("unadmitted provider tool")));
            }
        }
    }
    let _permit = broker.slots.acquire().await.map_err(|e| denied(e.into()))?;
    let index = {
        let mut store = broker.store.lock().await;
        let index = match store.reserve(&broker.goal, &id, input_bytes) {
            Ok(index) => index,
            Err(error) => {
                store
                    .record(
                        "provider_denied",
                        json!({"agent_id":id,"reason":error.to_string()}),
                    )
                    .map_err(denied)?;
                return Err(denied(error));
            }
        };
        body = json!({"model":broker.goal.model,"messages":body["messages"],"tools":body.get("tools").cloned().unwrap_or(json!([])),"max_tokens":broker.goal.max_output_tokens,"thinking":{"type":"disabled"},"temperature":0,"stream":false});
        let intent_id = store.intents[index].id.clone();
        store
            .record(
                "provider_request",
                json!({"agent_id":id,"intent_id":intent_id,"body":body}),
            )
            .map_err(denied)?;
        store.intents[index].state = "dispatched".into();
        store
            .record("provider_dispatched", json!({"intent_id":intent_id}))
            .map_err(denied)?;
        index
    };
    let outcome = broker.provider.dispatch(body).await;
    let mut store = broker.store.lock().await;
    let outcome = match outcome {
        Ok(mut response) => match store.settle(&broker.goal, index, &response) {
            Ok(cost) => {
                response["swarm_cost_microusd"] = json!(cost);
                let intent_id = store.intents[index].id.clone();
                store
                    .record(
                        "provider_response",
                        json!({"intent_id":intent_id,"body":response}),
                    )
                    .map_err(denied)?;
                return Ok(Json(response));
            }
            Err(error) => error,
        },
        Err(error) => error,
    };
    if store.intents[index].state == "dispatched" {
        store.intents[index].state = "unknown".into();
    }
    store.goal_status = "provider_failed".into();
    let intent_id = store.intents[index].id.clone();
    store
        .record(
            "provider_unknown",
            json!({"intent_id":intent_id,"reason":outcome.to_string()}),
        )
        .map_err(denied)?;
    Err(denied(outcome))
}
async fn tool(
    State(broker): State<Broker>,
    RoutePath(name): RoutePath<String>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> ApiResult {
    let mut store = broker.store.lock().await;
    let id = store.authenticate(&headers).map_err(denied)?;
    match store.tool(&broker.goal, &id, &name, args) {
        Ok(value) => Ok(Json(value)),
        Err(error) => {
            store
                .record(
                    "tool_denied",
                    json!({"agent_id":id,"tool":name,"reason":error.to_string()}),
                )
                .map_err(denied)?;
            Err(denied(error))
        }
    }
}
pub fn router(broker: Broker) -> Router {
    let limit = broker.goal.max_input_bytes;
    Router::new()
        .route("/model", post(model))
        .route("/tool/{name}", post(tool))
        .layer(DefaultBodyLimit::max(limit))
        .with_state(broker)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (Goal, Store, PathBuf) {
        let goal: Goal = serde_json::from_value(json!({"schema_version":1,"intent":"test","worker_prompt":"test","synthesis_prompt":"test","scope":"synthetic-fixtures-only","model":"deepseek-flash","agent_count":2,"deadline_seconds":10,"budget_microusd":1000000,"max_calls_per_agent":8,"max_input_bytes":65536,"max_output_tokens":2048,"input_price_microusd_per_million":300000,"output_price_microusd_per_million":1200000,"grants":["claim_task","board_read","board_post","submit_finding","read_findings","submit_artifact"],"tasks":[{"id":"a","input":{}},{"id":"b","input":{}}],"seed":1,"catalog_version":"test","scorer_version":"1"})).unwrap();
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&dir).unwrap();
        let mut store = Store::new(&dir, &goal).unwrap();
        store.goal_status = "running".into();
        store.admit("agent-00").unwrap();
        store.admit("agent-01").unwrap();
        (goal, store, dir)
    }
    #[test]
    fn claims_are_exclusive_and_idempotent() {
        let (goal, mut store, dir) = setup();
        let first = store
            .tool(&goal, "agent-00", "claim_task", json!({}))
            .unwrap();
        assert_eq!(
            first,
            store
                .tool(&goal, "agent-00", "claim_task", json!({}))
                .unwrap()
        );
        assert_ne!(
            first["id"],
            store
                .tool(&goal, "agent-01", "claim_task", json!({}))
                .unwrap()["id"]
        );
        assert!(
            store
                .tool(
                    &goal,
                    "agent-01",
                    "submit_finding",
                    json!({"task_id":"a","result":{}})
                )
                .is_err()
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn stopped_and_ungranted_effects_are_denied() {
        let (goal, mut store, dir) = setup();
        assert!(
            store
                .tool(&goal, "agent-01", "submit_artifact", json!({"artifact":{}}))
                .is_err()
        );
        assert!(store.tool(&goal, "agent-00", "bash", json!({})).is_err());
        store.goal_status = "stopped".into();
        assert!(store.reserve(&goal, "agent-00", 200).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn ceiling_and_settlement_are_once_only() {
        let (mut goal, mut store, dir) = setup();
        goal.budget_microusd = 1;
        assert!(store.reserve(&goal, "agent-00", 200).is_err());
        assert!(store.intents.is_empty());
        goal.budget_microusd = 1000000;
        let index = store.reserve(&goal, "agent-00", 200).unwrap();
        store.intents[index].state = "dispatched".into();
        let response =
            json!({"id":"receipt1","usage":{"prompt_tokens":100,"completion_tokens":50}});
        assert_eq!(store.settle(&goal, index, &response).unwrap(), 90);
        assert_eq!(store.held_microusd, 0);
        assert!(store.settle(&goal, index, &response).is_err());
        assert_eq!(store.spent_microusd, 90);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn missing_usage_keeps_hold() {
        let (goal, mut store, dir) = setup();
        let index = store.reserve(&goal, "agent-00", 200).unwrap();
        store.intents[index].state = "dispatched".into();
        let held = store.held_microusd;
        assert!(
            store
                .settle(&goal, index, &json!({"id":"receipt"}))
                .is_err()
        );
        assert_eq!(store.held_microusd, held);
        assert_eq!(store.spent_microusd, 0);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn malformed_and_modified_definitions_fail_closed() {
        let (mut goal, _, dir) = setup();
        assert!(goal.validate().is_ok());
        goal.grants.clear();
        assert!(goal.validate().is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
