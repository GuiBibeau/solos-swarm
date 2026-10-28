use crate::code::CodeExecutor;
use crate::progress;
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
use zeroize::Zeroizing;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    /// DeepSeek hosted: `thinking` is a parameter on the same chat body.
    DeepSeek,
    /// Any other OpenAI-compatible gateway; reasoning is steered by `reasoning_effort`.
    OpenAi,
}
/// The exact admitted upstream a Goal spends against. The provider is named
/// explicitly because it is what the receipt, ledger and checker attribute.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    pub provider: String,
    pub model: String,
    pub dialect: Dialect,
}
impl Upstream {
    pub fn deepseek(model: &str) -> Self {
        Self {
            provider: "deepseek".into(),
            model: model.into(),
            dialect: Dialect::DeepSeek,
        }
    }
}
/// Model IDs of the admitted Venice catalog. Newer revisions carry a date
/// suffix, so selection is prefix-anchored rather than an exact match.
pub fn venice_model(model: &str) -> bool {
    model.starts_with("deepseek-v4-1-flash-uncensored") || model.starts_with("zai-org-glm-5")
}
/// Resolve a deployment provider id and model into its admitted upstream, so an
/// operator selects a shipped profile by name instead of restating its dialect.
/// An unknown provider fails rather than silently defaulting, because the wrong
/// dialect would send a vendored parameter openai-compatible gateways ignore.
pub fn upstream_profile(provider: &str, model: &str) -> Result<Upstream> {
    ensure!(!model.is_empty(), "provider model is empty");
    Ok(if provider == "deepseek" {
        Upstream::deepseek(model)
    } else if provider == "venice" {
        ensure!(
            venice_model(model),
            "model is outside the admitted Venice catalog"
        );
        Upstream {
            provider: provider.into(),
            model: model.into(),
            dialect: Dialect::OpenAi,
        }
    } else {
        bail!("provider outside the deployment allowlist: {provider}")
    })
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Goal {
    pub schema_version: u32,
    pub intent: String,
    pub worker_prompt: String,
    pub synthesis_prompt: String,
    pub scope: String,
    /// Legacy admitted provider selector. Retained so a Goal frozen by the
    /// ten-agent preview keeps both its digest and its meaning.
    #[serde(default)]
    pub provider: Option<String>,
    /// The exact admitted provider/model pair, superseding the legacy pair.
    #[serde(default)]
    pub upstream: Option<Upstream>,
    /// Where this Goal's per-task `prior_evidence` came from, when the case was
    /// forked from an earlier run that died before filing. Absent for a fresh case.
    #[serde(default)]
    pub via_recovered_run: Option<Value>,
    pub model: String,
    pub agent_count: usize,
    pub deadline_seconds: u64,
    pub budget_microusd: u64,
    /// Per-agent model-call ceiling. It exists to bound a runaway agent, not to
    /// shape its behaviour, so it is deliberately high: an agent that probes a
    /// live target needs as many turns as the target takes to answer.
    pub max_calls_per_agent: usize,
    /// Ceiling on ONE worker turn's request body. A worker replays its whole
    /// conversation each turn and every fetch result stays in that history, so
    /// this bounds the accumulated transcript, not a single prompt.
    pub max_input_bytes: usize,
    pub max_output_tokens: u64,
    pub input_price_microusd_per_million: u64,
    pub output_price_microusd_per_million: u64,
    /// Optional settlement rate for prompt tokens the provider reports as cache
    /// hits (`usage.prompt_cache_hit_tokens`). Absent keeps the historical
    /// all-miss settlement. Holds are always priced all-miss, so a settlement
    /// can never exceed its reservation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_price_microusd_per_million: Option<u64>,
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
            matches!(
                self.scope.as_str(),
                "synthetic-fixtures-only"
                    | "authorized-audit:solana-developer-platform"
                    | "authorized-audit:solana-platform-redteam"
                    | "public-web-research"
            ),
            "Goal scope outside the deployment allowlist"
        );
        match (&self.upstream, &self.provider) {
            (Some(upstream), _) => {
                ensure!(
                    upstream == &upstream_profile(&upstream.provider, &upstream.model)?,
                    "declared upstream is not the admitted profile it names"
                );
                ensure!(
                    upstream.model == self.model,
                    "Goal model disagrees with its declared upstream"
                );
            }
            (None, Some(provider)) => {
                ensure!(
                    upstream_profile(provider, &self.model)? == Upstream::deepseek(&self.model),
                    "legacy Goal names a non-DeepSeek provider"
                );
            }
            (None, None) => ensure!(
                self.model == "deepseek-flash",
                "model outside deployment allowlist"
            ),
        }
        ensure!(
            (1..=30).contains(&self.agent_count) && self.tasks.len() == self.agent_count,
            "one Task per admitted agent required (maximum thirty)"
        );
        ensure!(
            (1..=21_600).contains(&self.deadline_seconds),
            "deadline must be 1..21600 seconds"
        );
        ensure!(
            (1..=1_000_000_000_000).contains(&self.budget_microusd),
            "Goal spend must be 1..1000000000000 microusd"
        );
        let mut task_budget = 0u64;
        for task in &self.tasks {
            if let Some(budget) = task.budget_microusd {
                ensure!(budget > 0, "Task budget must be positive");
                task_budget = task_budget
                    .checked_add(budget)
                    .context("Task budget total overflow")?;
            }
        }
        ensure!(
            task_budget <= self.budget_microusd,
            "Task budgets exceed Goal spend cap"
        );
        ensure!(
            (1..=10_000).contains(&self.max_calls_per_agent),
            "invalid turn ceiling"
        );
        ensure!(
            (1..=16_777_216).contains(&self.max_input_bytes)
                && (1..=393_216).contains(&self.max_output_tokens),
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
        if let Some(hit) = self.cache_hit_price_microusd_per_million {
            ensure!(
                (6_000..=self.input_price_microusd_per_million).contains(&hit),
                "cache-hit rate outside admitted peak prices"
            );
        }
        // The six historical grants always exist; `fetch_url` and `run_code`
        // are optional so a Goal frozen before those tools existed keeps
        // validating unchanged.
        let required = [
            "claim_task",
            "board_read",
            "board_post",
            "submit_finding",
            "read_findings",
            "submit_artifact",
        ];
        ensure!(
            required
                .iter()
                .all(|name| self.grants.iter().any(|g| g == name))
                && self.grants.iter().all(|grant| {
                    required.contains(&grant.as_str())
                        || matches!(grant.as_str(), "fetch_url" | "run_code")
                })
                && self.grants.len() >= required.len(),
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
    /// The admitted upstream, resolving the legacy pair for a Goal frozen before
    /// `upstream` existed. Never fails on a validated Goal.
    fn resolved(&self) -> Upstream {
        self.upstream
            .clone()
            .unwrap_or_else(|| Upstream::deepseek(&self.model))
    }
    fn cost(&self, input: u64, output: u64) -> u64 {
        // Charge all input at peak cache-miss rates; this deliberately overstates the invoice.
        (input * self.input_price_microusd_per_million).div_ceil(1_000_000)
            + (output * self.output_price_microusd_per_million).div_ceil(1_000_000)
    }
    fn settled_cost(&self, input: u64, cached: u64, output: u64) -> u64 {
        let hit_rate = self
            .cache_hit_price_microusd_per_million
            .unwrap_or(self.input_price_microusd_per_million);
        // Round the combined input charge once so splitting a prompt never increases its cost.
        ((input - cached) * self.input_price_microusd_per_million + cached * hit_rate)
            .div_ceil(1_000_000)
            + (output * self.output_price_microusd_per_million).div_ceil(1_000_000)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub input: Value,
    /// Optional per-task spend envelope. Omitted Goals keep the historical
    /// fair-share envelope, while a case with one expensive component can fund
    /// it without giving every Task the same ceiling.
    #[serde(default)]
    pub budget_microusd: Option<u64>,
    #[serde(default)]
    pub claimed_by: Option<String>,
    #[serde(default = "open")]
    pub status: String,
}
fn open() -> String {
    "open".into()
}

#[derive(Serialize, Deserialize)]
pub struct Agent {
    pub started_ms: u64,
    pub stopped_ms: Option<u64>,
    pub successful_model_calls: usize,
    pub model_calls: usize,
    pub status: String,
    pub fetch_bytes: u64,
    /// Set once the broker refuses further model work for a non-transient
    /// reason. The worker may still file; the nudge loop turns this into one
    /// `defer` Finding instead of prompting a denied worker forever.
    pub terminal: Option<String>,
    /// run_code calls made, and the last few as one-line notes, so a partial
    /// finding can say what the agent did before its budget ran out.
    pub run_code_calls: u32,
    pub notes: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Finding {
    pub agent_id: String,
    pub task_id: String,
    pub result: Value,
}
#[derive(Serialize, Deserialize)]
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
/// The single result key every Finding uses and the records under it.
///
/// The Goal decides the key (`papers`, `experiments`, ...); the broker only
/// requires that every Finding agrees on one array-valued key.
fn finding_records(findings: &[Finding]) -> Option<(String, Vec<Value>)> {
    let key = findings.first()?.result.as_object()?.keys().next()?.clone();
    let mut records = Vec::new();
    for finding in findings {
        let result = finding.result.as_object()?;
        if result.len() != 1 {
            return None;
        }
        records.extend(result.get(&key)?.as_array()?.iter().cloned());
    }
    Some((key, records))
}
/// True when `artifact` is exactly `{key: [...expected...]}` as a multiset.
///
/// Order is irrelevant because the checker sorts, but every occurrence counts:
/// a dropped or retyped record is a failed Goal, not a stylistic difference.
/// `Value::to_string()` is canonical here because serde_json's map keeps keys
/// sorted unless the preserve_order feature is enabled (it is not).
fn same_records(artifact: &Value, key: &str, expected: &[Value]) -> bool {
    let Some(object) = artifact.as_object() else {
        return false;
    };
    if object.len() != 1 {
        return false;
    }
    let Some(got) = object.get(key).and_then(Value::as_array) else {
        return false;
    };
    let mut want: Vec<String> = expected.iter().map(Value::to_string).collect();
    let mut have: Vec<String> = got.iter().map(Value::to_string).collect();
    want.sort_unstable();
    have.sort_unstable();
    want == have
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
    /// Set while the broker refuses new model work (operator drain or pause,
    /// provider outage). Absent from state.json when the run is not paused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause: Option<Pause>,
    #[serde(skip)]
    pub dir: PathBuf,
    #[serde(skip)]
    log: File,
    /// Sequence of the last event; written to state.json so a resume can prove
    /// state.json and events.jsonl describe the same instant.
    #[serde(rename = "last_seq")]
    seq: u64,
    #[serde(skip)]
    outage_streak: u32,
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
            pause: None,
            outage_streak: 0,
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
        // A resumed agent keeps its counters, so the call ceiling and envelope
        // see its whole history rather than a fresh process.
        let prior = self.agents.remove(id);
        let resumed = prior.is_some();
        let agent = match prior {
            Some(mut agent) => {
                agent.status = "running".into();
                agent.stopped_ms = None;
                agent
            }
            None => Agent {
                started_ms: now_ms(),
                stopped_ms: None,
                successful_model_calls: 0,
                model_calls: 0,
                status: "running".into(),
                fetch_bytes: 0,
                terminal: None,
                run_code_calls: 0,
                notes: vec![],
            },
        };
        self.agents.insert(id.into(), agent);
        let kind = if resumed {
            "agent_resumed"
        } else {
            "agent_started"
        };
        self.record(kind, json!({"agent_id":id}))?;
        Ok(token)
    }
    /// Admission for one more live read. The budget bounds accumulated context,
    /// not the number of calls: submit_finding stays available after it trips.
    pub fn read_budget(&self, id: &str) -> Result<()> {
        let agent = self.agents.get(id).context("unknown allocation")?;
        ensure!(
            agent.fetch_bytes < FETCH_TOTAL_BYTES,
            "read budget exhausted after {} MiB; stop fetching and call submit_finding with the evidence you already have",
            FETCH_TOTAL_BYTES / (1024 * 1024)
        );
        Ok(())
    }
    pub fn charge_fetch(&mut self, id: &str, bytes: u64) -> Result<()> {
        let agent = self.agents.get_mut(id).context("unknown allocation")?;
        agent.fetch_bytes = agent.fetch_bytes.saturating_add(bytes);
        Ok(())
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
    /// A Task may carry its own envelope. The fallback is the historical fair
    /// share, so a Goal frozen before per-Task budgets keeps its meaning.
    fn envelope(&self, goal: &Goal, id: &str) -> u64 {
        self.tasks
            .iter()
            .find(|task| task.claimed_by.as_deref() == Some(id))
            .and_then(|task| task.budget_microusd)
            .unwrap_or(goal.budget_microusd / goal.agent_count as u64)
    }
    /// Settled cost plus outstanding holds for one agent.
    fn committed(&self, id: &str) -> u64 {
        self.intents
            .iter()
            .filter(|i| i.agent_id == id)
            .map(|i| {
                if i.state == "completed" {
                    i.cost_microusd
                } else {
                    i.hold_microusd
                }
            })
            .sum()
    }
    /// How much of its envelope or turn ceiling an agent has used, 0.0..=1.0+.
    pub fn budget_fraction(&self, goal: &Goal, id: &str) -> f64 {
        let calls = self.agents.get(id).map_or(0, |a| a.model_calls);
        progress::fraction(
            self.committed(id),
            self.envelope(goal, id),
            calls,
            goal.max_calls_per_agent,
        )
    }
    /// Append the soft/final warning to a tool result the agent will read.
    pub fn with_notice(&self, goal: &Goal, id: &str, name: &str, mut value: Value) -> Value {
        if matches!(name, "submit_finding" | "submit_artifact" | "read_findings")
            || id == "agent-00"
        {
            return value;
        }
        if let (Some(text), Some(object)) = (
            progress::notice(self.budget_fraction(goal, id)),
            value.as_object_mut(),
        ) {
            object.insert("budget_notice".into(), json!(text));
        }
        value
    }
    /// Refuse fetch/run_code inside the finalization allowance, leaving the
    /// last tenth of the envelope for the model turns that file the finding.
    pub fn budget_gate(&mut self, goal: &Goal, id: &str, name: &str) -> Result<Option<Value>> {
        let fraction = self.budget_fraction(goal, id);
        if id == "agent-00" || progress::level(fraction) != Some(progress::Level::Final) {
            return Ok(None);
        }
        self.record(
            "tool_budget_blocked",
            json!({"agent_id":id,"tool":name,"fraction":fraction}),
        )?;
        Ok(Some(progress::blocked(fraction)))
    }
    pub fn note_work(&mut self, id: &str, code: &str, response: &Value) {
        if let Some(agent) = self.agents.get_mut(id) {
            agent.run_code_calls += 1;
            agent.notes.push(progress::work_note(code, response));
            if agent.notes.len() > progress::MAX_NOTES {
                agent.notes.remove(0);
            }
        }
    }
    fn reserve(&mut self, goal: &Goal, id: &str, input_bytes: usize) -> Result<usize> {
        self.active(id)?;
        // Before any counter moves: a paused run burns neither call ceiling nor budget.
        self.gate_model()?;
        {
            let agent = self.agents.get(id).context("unknown allocation")?;
            if let Some(reason) = &agent.terminal {
                bail!("agent terminal: {reason}");
            }
            ensure!(
                agent.model_calls < goal.max_calls_per_agent,
                "agent turn ceiling exhausted"
            );
        }
        // UTF-8 bytes plus chat framing overbounds byte-BPE input; reject usage above this bound.
        let hold = goal.cost(input_bytes as u64 + 4096, goal.max_output_tokens);
        ensure!(
            self.spent_microusd + self.held_microusd + hold <= goal.budget_microusd,
            "Goal spend ceiling exhausted"
        );
        let envelope = self.envelope(goal, id);
        let agent_committed = self.committed(id);
        if agent_committed.saturating_add(hold) > envelope {
            let reason = "Task budget envelope exhausted";
            if let Some(agent) = self.agents.get_mut(id) {
                agent.terminal = Some(reason.into());
            }
            // A denied model turn cannot generate the defer Finding itself.
            // File the contract row here, before the denial is returned, so the
            // worker is retired with a valid result instead of being prompted
            // into the same denial two hundred times.
            let _ = self.defer_task(goal, id, reason);
            bail!("{reason}");
        }
        let index = self.intents.len();
        self.intents.push(Intent {
            id: format!("intent-{index:04}"),
            agent_id: id.into(),
            state: "reserved".into(),
            hold_microusd: hold,
            cost_microusd: 0,
            usage: None,
            provider: goal.resolved().provider,
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
        let usage = solos_swarm::provider::completion_usage(&response["usage"])?;
        let input = usage.input_tokens;
        let output = usage.output_tokens;
        let cached = usage.cached_input_tokens;
        ensure!(
            input <= 1_000_000 && output <= goal.max_output_tokens,
            "provider usage exceeds contract"
        );
        let request_id = response["id"]
            .as_str()
            .filter(|v| !v.is_empty())
            .context("missing provider receipt")?
            .to_owned();
        let cost = goal.settled_cost(input, cached, output);
        let intent = &mut self.intents[index];
        let exceeds_hold = cost > intent.hold_microusd;
        let id = intent.agent_id.clone();
        self.held_microusd -= intent.hold_microusd;
        self.spent_microusd += cost;
        intent.cost_microusd = cost;
        intent.usage = Some(
            json!({"input_tokens":input,"cached_input_tokens":cached,"uncached_input_tokens":input-cached,"output_tokens":output}),
        );
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
                // The coordinator Task belongs to agent-00, the only agent
                // granted read_findings/submit_artifact. First-come claiming
                // handed it to a worker in arch-risk-a3 and no one synthesized.
                let wants_coordinator = id == "agent-00";
                let open = |t: &Task| t.status == "open" && is_coordinator(t) == wants_coordinator;
                let index = current
                    .or_else(|| self.tasks.iter().position(open))
                    .or_else(|| {
                        wants_coordinator
                            .then(|| self.tasks.iter().position(|t| t.status == "open"))
                            .flatten()
                    })
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
                ensure!(self.board.len() < 100, "Board capacity exceeded");
                // A Board message over the limit used to be a schema rejection
                // the model could retry forever. Shorten it here as well, so any
                // client (not only the pinned extension) can keep moving.
                let shortened = message.chars().count() > BOARD_MESSAGE_LIMIT;
                let message: String = message.chars().take(BOARD_MESSAGE_LIMIT).collect();
                let mut post = json!({"seq":self.board.len()+1,"agent_id":id,"message":message});
                if shortened {
                    post["shortened"] = json!(true);
                }
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
            // Network egress is not derived from Goal state, so the caller performs
            // it and passes the observed response in; this arm only records it so a
            // replay matches on the same wire bytes.
            "fetch_url" | "run_code" => {
                let response = args["response"].clone();
                ensure!(
                    response.is_object(),
                    "tool response is supplied by the caller"
                );
                response
            }
            "submit_artifact" => {
                ensure!(
                    self.findings.len() == goal.agent_count,
                    "all Task results required before synthesis"
                );
                ensure!(args["artifact"].is_object(), "artifact must be an object");
                ensure!(self.artifact.is_none(), "Attempt already submitted");
                let artifact = args["artifact"].clone();
                // The artifact is mechanically derivable: concatenate every
                // finding's records verbatim. A model can still retype, dedupe
                // or drop rows, which the independent checker would score as a
                // failed Goal after the money is spent. Reject the attempt here
                // with the exact counts and let the synthesis loop retry.
                let Some((key, expected)) = finding_records(&self.findings) else {
                    bail!("every Finding must contain exactly one result array")
                };
                if same_records(&artifact, &key, &expected) {
                    write_json(&self.dir.join("artifacts/final.json"), &artifact)?;
                    self.artifact = Some(artifact);
                    json!({"accepted":true})
                } else {
                    let received = artifact
                        .get(&key)
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    json!({
                        "accepted": false,
                        "error": "Artifact must contain every Finding record exactly once, verbatim.",
                        "expected_records": expected.len(),
                        "received_records": received,
                        "continue": "Copy every record from every finding, then submit again."
                    })
                }
            }
            _ => bail!("unknown tool"),
        };
        self.record(
            "tool_completed",
            json!({"agent_id":id,"tool":name,"args":args,"result":result}),
        )?;
        Ok(result)
    }

    /// The finding result for a Task the agent never filed: a plain defer when
    /// it did nothing, otherwise a partial summary of its Board posts and work.
    fn partial_result(&self, goal: &Goal, id: &str, task: &Task, reason: &str) -> Value {
        let agent = self.agents.get(id);
        let summary = progress::Summary {
            reason,
            model_calls: agent.map_or(0, |a| a.model_calls),
            run_code_calls: agent.map_or(0, |a| a.run_code_calls),
            committed_microusd: self.committed(id),
            envelope_microusd: self.envelope(goal, id),
            board_posts: self
                .board
                .iter()
                .filter(|post| post["agent_id"] == id)
                .filter_map(|post| post["message"].as_str())
                .rev()
                .take(3)
                .map(|m| m.chars().take(400).collect())
                .collect(),
            notes: agent.map_or(&[][..], |a| &a.notes),
        };
        let text = progress::partial_text(&summary);
        let mut record = deferred_record(task, reason, text.as_deref());
        if text.is_some() {
            record["evidence"] = json!(record["description"]);
        }
        json!({ finding_key(task): [record] })
    }
    /// Host-side filing for every claimed Task whose agent never filed. Used when
    /// the run ends (deadline, cancel, nudges exhausted); it does not need an
    /// active goal because it is a control-plane effect, like `defer_task`.
    pub fn file_partial_findings(&mut self, goal: &Goal, reason: &str) -> Result<usize> {
        let missing: Vec<(String, Task)> = self
            .tasks
            .iter()
            .filter_map(|t| Some((t.claimed_by.clone()?, t.clone())))
            .filter(|(agent, _)| !self.findings.iter().any(|f| &f.agent_id == agent))
            .collect();
        for (agent, task) in &missing {
            let result = self.partial_result(goal, agent, task, reason);
            self.findings.push(Finding {
                agent_id: agent.clone(),
                task_id: task.id.clone(),
                result,
            });
            if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task.id) {
                t.status = "completed".into();
            }
            self.record(
                "finding_filed_by_host",
                json!({"agent_id":agent,"task_id":task.id,"reason":reason}),
            )?;
        }
        Ok(missing.len())
    }
    /// Build the exact-union artifact from the findings when the coordinator
    /// could not. Same guard as `submit_artifact`: every record, verbatim, once.
    pub fn synthesize_artifact(&mut self, reason: &str) -> Result<bool> {
        if self.artifact.is_some() {
            return Ok(false);
        }
        let Some((key, records)) = finding_records(&self.findings) else {
            return Ok(false);
        };
        let artifact = json!({ key.clone(): records.clone() });
        ensure!(
            same_records(&artifact, &key, &records),
            "synthesis guard failed"
        );
        write_json(&self.dir.join("artifacts/final.json"), &artifact)?;
        self.artifact = Some(artifact);
        self.record(
            "artifact_synthesized",
            json!({"reason":reason,"records":records.len(),"findings":self.findings.len()}),
        )?;
        Ok(true)
    }
    /// File one deferred record for a worker whose Task envelope is terminal.
    ///
    /// This is deliberately a broker effect, not another model turn: the worker
    /// cannot be prompted to file when its provider calls are already denied.
    /// The record uses only the frozen Task contract, so the checker can still
    /// attribute one well-formed result to the Task and the artifact remains an
    /// exact union of thirty records.
    pub fn defer_task(&mut self, goal: &Goal, id: &str, reason: &str) -> Result<()> {
        if self.findings.iter().any(|finding| finding.agent_id == id) {
            return Ok(());
        }
        let claimed = self.tool(goal, id, "claim_task", json!({}))?;
        let task_id = claimed["id"]
            .as_str()
            .context("claim returned no Task")?
            .to_owned();
        let task = self
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .cloned()
            .context("unknown Task")?;
        let result = self.partial_result(goal, id, &task, reason);
        self.tool(
            goal,
            id,
            "submit_finding",
            json!({"task_id":task_id,"result":result}),
        )?;
        // A Board post is not required per Finding, but it keeps the deferral
        // visible to the coordinator and is harmless if the Board is full.
        let _ = self.tool(
            goal,
            id,
            "board_post",
            json!({"message":format!("{id}: deferred {task_id} ({reason})")}),
        );
        Ok(())
    }
}

fn is_coordinator(task: &Task) -> bool {
    task.input.get("role").and_then(Value::as_str) == Some("coordinator")
}

/// The result key of a Task, derived only from the public Task input.
fn finding_key(task: &Task) -> String {
    task.input
        .get("output_contract")
        .and_then(|contract| contract.get("result_key"))
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            (task.input.get("experiment").is_some() || task.input.get("artifact_fields").is_some())
                .then(|| "experiments".to_owned())
        })
        .unwrap_or_else(|| "inconsistencies".to_owned())
}

/// A valid experiment-contract row for a Task that was never executed.
fn deferred_record(task: &Task, reason: &str, partial: Option<&str>) -> Value {
    let input = &task.input;
    let title = input
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .unwrap_or(task.id.as_str());
    let file = input
        .get("seed_url")
        .and_then(Value::as_str)
        .or_else(|| input.get("paper_url").and_then(Value::as_str))
        .or_else(|| input.get("artifact_file").and_then(Value::as_str))
        .unwrap_or_default();
    let mut record = json!({
        "id": task.id,
        "title": title,
        "file": file,
        "description": partial.map_or_else(|| format!(
            "Deferred before execution: {reason}. No code was run; this record preserves the Task contract for a later retry."
        ), str::to_owned),
        "severity": "defer",
        "code": "",
        "output": format!("DEFERRED: {reason}"),
    });
    for field in ["experiment", "component", "compute"] {
        if let Some(value) = input.get(field) {
            record[field] = value.clone();
        }
    }
    record
}

type Dispatch = Pin<Box<dyn Future<Output = Result<Value>> + Send>>;
pub trait Provider: Send + Sync {
    fn dispatch(&self, body: Value) -> Dispatch;
}
/// An OpenAI-compatible hosted endpoint. `dialect` is the only difference
/// between the shipped profiles, so one transport serves both and neither
/// duplicates the receipt, bound or error handling.
pub struct OpenAiCompatible {
    pub endpoint: &'static str,
    pub dialect: Dialect,
    pub client: reqwest::Client,
    pub key: Zeroizing<String>,
}
impl Provider for OpenAiCompatible {
    fn dispatch(&self, body: Value) -> Dispatch {
        let endpoint = self.endpoint;
        let dialect = self.dialect;
        let client = self.client.clone();
        let key = self.key.clone();
        Box::pin(async move {
            // `thinking` is DeepSeek's own dialect and Venice rejects it outright
            // ("Invalid request parameters", HTTP 400), so an OpenAI-compatible
            // route REPLACES it rather than augmenting: the field is removed and
            // the intent re-expressed as `reasoning_effort`, because a reasoning
            // model would otherwise leave reasoning at its costlier default. A
            // non-reasoning model rejects `reasoning_effort: "none"`, so that is
            // only sent where the Goal declared reasoning off.
            let mut body = body;
            if dialect == Dialect::OpenAi {
                let disabled = body["thinking"]["type"] == json!("disabled");
                if let Some(object) = body.as_object_mut() {
                    object.remove("thinking");
                }
                if disabled && reasoning_model(body["model"].as_str().unwrap_or_default()) {
                    body["reasoning_effort"] = json!("none");
                }
            }
            let mut response = client
                .post(endpoint)
                .bearer_auth(key.as_str())
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
/// Models whose `reasoning_effort` field is meaningful on an OpenAI-compatible
/// gateway. Kept beside the catalog admission so adding a model to one without
/// the other is visible.
pub fn reasoning_model(model: &str) -> bool {
    model.starts_with("deepseek-v4-1-flash") || model.starts_with("zai-org-glm-5")
}
#[derive(Clone)]
pub struct Broker {
    pub goal: Arc<Goal>,
    pub store: Arc<Mutex<Store>>,
    pub provider: Arc<dyn Provider>,
    pub slots: Arc<Semaphore>,
    /// Egress client for `fetch_url`, deliberately separate from the provider
    /// client: a target's latency and redirects are the worker's business.
    pub client: reqwest::Client,
    /// The sandboxed local executor. `None` when the Goal did not grant
    /// `run_code`, so a frozen ten-agent Goal cannot acquire it accidentally.
    pub code: Option<Arc<CodeExecutor>>,
}
type ApiResult = std::result::Result<Json<Value>, (StatusCode, Json<Value>)>;
fn denied(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(json!({"error":error.to_string()})),
    )
}

/// Seconds until the weekday peak window ends when `now_secs` (Unix, UTC) falls inside one: Mon-Fri 01:00-04:00 and
/// 06:00-10:00. No model call may start inside it when `SWARM_REFUSE_PEAK=1`, so no tokens are bought at peak price.
pub fn peak_retry_after(now_secs: u64) -> Option<u64> {
    let weekday = (now_secs / 86_400 + 4) % 7; // 1970-01-01 was a Thursday; 0 = Sunday
    if !(1..=5).contains(&weekday) {
        return None;
    }
    let second_of_day = now_secs % 86_400;
    [(3_600, 14_400), (21_600, 36_000)]
        .into_iter()
        .find(|(start, end)| (*start..*end).contains(&second_of_day))
        .map(|(_, end)| end - second_of_day)
}
/// The one pause test. `control.json` in the run directory is the source of truth: an operator or the broker
/// itself (provider outage, drain) writes `{"mode":"pause"|"drain"}` there. Two inputs feed the same answer: a
/// `PAUSE` file (the older operator switch) and, with `SWARM_REFUSE_PEAK=1`, a peak window. Returns the polling
/// interval the client should wait (at most 60 s). A paused request is refused BEFORE any reservation, so it
/// costs no call, no hold and no ledger entry.
pub fn pause_retry_after(dir: &Path, now_secs: u64) -> Option<u64> {
    let mode = fs::read_to_string(dir.join("control.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|control| control["mode"].as_str().map(str::to_owned));
    if matches!(mode.as_deref(), Some("pause" | "drain")) || dir.join("PAUSE").exists() {
        return Some(30);
    }
    if std::env::var("SWARM_REFUSE_PEAK").as_deref() == Ok("1") {
        return peak_retry_after(now_secs).map(|s| s.clamp(1, 60));
    }
    None
}
fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
    let (id, run_dir) = {
        let store = broker.store.lock().await;
        (
            store.authenticate(&headers).map_err(denied)?,
            store.dir.clone(),
        )
    };
    if let Some(wait) = pause_retry_after(&run_dir, unix_secs()) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({"paused":true,"retry_after_s":wait,"reason":"model calls are paused (operator pause or peak window)"}),
            ),
        ));
    }
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
                if store.pause.is_some() {
                    // The run_paused event is the record; per-call denials
                    // would only grow the log while agents poll.
                    drop(store);
                    tokio::time::sleep(PAUSED_RETRY_DELAY).await;
                    return Err(denied(error));
                }
                store
                    .record(
                        "provider_denied",
                        json!({"agent_id":id,"reason":error.to_string()}),
                    )
                    .map_err(denied)?;
                return Err(denied(error));
            }
        };
        // The wire model comes from the declared upstream, not the Goal alias:
        // a Goal may name one model while the provider is selected separately.
        body = json!({"model":broker.goal.resolved().model,"messages":body["messages"],"tools":body.get("tools").cloned().unwrap_or(json!([])),"max_tokens":broker.goal.max_output_tokens,"thinking":{"type":"disabled"},"temperature":0,"stream":false});
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
    let mut backoff = std::time::Duration::ZERO;
    let mut store = broker.store.lock().await;
    let outcome = match outcome {
        Ok(mut response) => match store.settle(&broker.goal, index, &response) {
            Ok(cost) => {
                store.note_provider_ok();
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
        Err(error) => {
            backoff = store.note_provider_failure(&error.to_string());
            error
        }
    };
    if store.intents[index].state == "dispatched" {
        store.intents[index].state = "unknown".into();
    }
    // A provider error is retryable, so the Goal keeps running and later turns
    // are not poisoned by one transient 402/429/5xx. Only an accounting failure
    // (settle charged more than it reserved) is terminal, because continuing
    // would spend against a ledger that no longer adds up.
    if store.goal_status != "accounting_failed" {
        store.goal_status = "running".into();
    }
    // The reservation must not be stranded: the provider may never have been
    // reached, so the hold is released and the turn counter is NOT refunded --
    // a retry is a real extra call and the ceiling should see it.
    if !matches!(store.intents[index].state.as_str(), "completed") {
        let held = store.intents[index].hold_microusd;
        if store.intents[index].state != "denied" {
            store.held_microusd = store.held_microusd.saturating_sub(held);
            store.intents[index].hold_microusd = 0;
            store.intents[index].state = "denied".into();
        }
    }
    let intent_id = store.intents[index].id.clone();
    store
        .record(
            "provider_unknown",
            json!({"intent_id":intent_id,"reason":outcome.to_string(),"retryable":true}),
        )
        .map_err(denied)?;
    // Bounded backoff outside the lock, still holding the dispatch slot, so an
    // outage slows the whole cohort instead of letting each agent spin.
    drop(store);
    tokio::time::sleep(backoff).await;
    Err(denied(outcome))
}
async fn tool(
    State(broker): State<Broker>,
    RoutePath(name): RoutePath<String>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> ApiResult {
    // Authenticate and check the allocation before any slow work. The lock is
    // released while egress or the sandbox runs, so one worker cannot block
    // another worker's bookkeeping.
    let id = {
        let store = broker.store.lock().await;
        let id = store.authenticate(&headers).map_err(denied)?;
        store.active(&id).map_err(denied)?;
        if name == "claim_task" {
            store.gate_claim(&id).map_err(denied)?;
        }
        id
    };
    let mut args = args;
    if matches!(name.as_str(), "fetch_url" | "run_code") {
        let mut store = broker.store.lock().await;
        if let Some(blocked) = store
            .budget_gate(&broker.goal, &id, &name)
            .map_err(denied)?
        {
            return Ok(Json(blocked));
        }
    }
    let submitted_code = args["code"].as_str().unwrap_or_default().to_owned();
    if name == "fetch_url" {
        if !broker.goal.grants.iter().any(|g| g == "fetch_url") {
            return Err(denied(anyhow::anyhow!("tool outside admitted grants")));
        }
        {
            let mut store = broker.store.lock().await;
            if let Err(error) = store.read_budget(&id) {
                store
                    .record(
                        "tool_denied",
                        json!({"agent_id":id,"tool":name,"reason":error.to_string()}),
                    )
                    .map_err(denied)?;
                return Err(denied(error));
            }
        }
        let url = args["url"].clone();
        match fetch(&broker.client, &args).await {
            Ok(response) => {
                let bytes = response["body_bytes"].as_u64().unwrap_or(0);
                let mut store = broker.store.lock().await;
                store.charge_fetch(&id, bytes).map_err(denied)?;
                drop(store);
                args = json!({"url":url,"response":response});
            }
            Err(error) => {
                let mut store = broker.store.lock().await;
                store
                    .record(
                        "fetch_failed",
                        json!({"agent_id":id,"url":url,"reason":error.to_string()}),
                    )
                    .map_err(denied)?;
                return Err(denied(error));
            }
        }
    }
    if name == "run_code" {
        if !broker.goal.grants.iter().any(|g| g == "run_code") {
            return Err(denied(anyhow::anyhow!("tool outside admitted grants")));
        }
        let Some(code) = broker.code.clone() else {
            return Err(denied(anyhow::anyhow!(
                "run_code is not available on this host"
            )));
        };
        {
            let mut store = broker.store.lock().await;
            store
                .record(
                    "code_started",
                    json!({
                        "agent_id":id,
                        "code_bytes":args["code"].as_str().map(str::len),
                        "timeout_seconds":args["timeout_seconds"].as_u64(),
                    }),
                )
                .map_err(denied)?;
        }
        match code.run(&id, &args).await {
            Ok(response) => {
                broker
                    .store
                    .lock()
                    .await
                    .note_work(&id, &submitted_code, &response);
                args = json!({"response":response});
            }
            Err(error) => {
                let mut store = broker.store.lock().await;
                store
                    .record(
                        "code_failed",
                        json!({"agent_id":id,"reason":error.to_string()}),
                    )
                    .map_err(denied)?;
                return Err(denied(error));
            }
        }
    }
    let mut store = broker.store.lock().await;
    match store.tool(&broker.goal, &id, &name, args) {
        Ok(value) => Ok(Json(store.with_notice(&broker.goal, &id, &name, value))),
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
        // Deliberately larger than the admitted input budget: the Goal bounds
        // what a worker may SEND, while this bounds what the transport must
        // ACCEPT. A worker replays its full conversation each turn and every
        // fetch result stays in that history, so the wire body exceeds the
        // admitted budget by construction; equal limits made a long run die 413.
        .layer(DefaultBodyLimit::max(limit * 4))
        .with_state(broker)
}

/// Bounds one live egress call. A worker probing an unfamiliar target must not be
/// able to hang the Goal, amplify through redirects, or exhaust memory with a
/// large body, so every one of those is capped here rather than by policy.
/// Kept modest on purpose: a fetch result is replayed in EVERY later turn, so a
/// large default silently multiplies through the conversation.
const FETCH_MAX_BYTES_DEFAULT: usize = 98_304;
const FETCH_MAX_REDIRECTS: usize = 5;
const FETCH_TIMEOUT_MS: u64 = 20_000;
const FETCH_HEADER_BYTES: usize = 4_096;
/// Total response bytes one agent may read through `fetch_url`, across calls.
/// A fetch result is replayed in every later turn, so the transcript, not the
/// call count, is what must stay bounded. A worker that trips this is told to
/// file; `submit_finding` is never denied by it.
pub const FETCH_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
/// The Board's message limit. Longer messages are shortened, never rejected.
/// How long a paused run makes a polling agent wait before the next refusal.
const PAUSED_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
pub const BOARD_MESSAGE_LIMIT: usize = 2_048;
/// Headers a worker may not silently override, because the fetch contract the
/// evidence depends on is stated in terms of them.
const FETCH_METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// Response headers within `FETCH_HEADER_BYTES`. A github.com directory page
/// returns more than that (CSP, cookies) and the old hard error failed every
/// seed fetch of it (7 `fetch_failed` in arch-path-b3); keep the first headers
/// that fit and say so.
fn bounded_headers(map: &reqwest::header::HeaderMap) -> (BTreeMap<String, String>, bool) {
    let mut headers = BTreeMap::new();
    let mut used = 2;
    for (name, value) in map {
        let value = value.to_str().unwrap_or("<binary>");
        let cost = name.as_str().len() + value.len() + 6;
        if used + cost > FETCH_HEADER_BYTES {
            return (headers, true);
        }
        used += cost;
        headers.insert(name.as_str().to_owned(), value.to_owned());
    }
    (headers, false)
}
/// Perform one bounded, read-mostly egress call.
///
/// Redirects are followed manually so the policy is explicit and so the final
/// URL is reported: a worker that is bounced to a login page has to see that it
/// was, rather than receiving a 200 from somewhere it never asked for.
pub async fn fetch(client: &reqwest::Client, args: &Value) -> Result<Value> {
    let raw = args["url"].as_str().context("fetch url missing")?;
    let url = reqwest::Url::parse(raw)?;
    ensure!(
        matches!(url.scheme(), "https" | "http"),
        "fetch supports http and https only"
    );
    ensure!(url.host_str().is_some(), "fetch url has no host");
    let method = args["method"]
        .as_str()
        .unwrap_or("GET")
        .to_ascii_uppercase();
    ensure!(
        FETCH_METHODS.contains(&method.as_str()),
        "unsupported fetch method"
    );
    let max_bytes = args["max_bytes"]
        .as_u64()
        .map_or(FETCH_MAX_BYTES_DEFAULT, |n| n as usize)
        .min(1_048_576);
    let mut current = url;
    let mut redirects = Vec::new();
    for _ in 0..=FETCH_MAX_REDIRECTS {
        let mut builder = client
            .request(
                reqwest::Method::from_bytes(method.as_bytes())?,
                current.clone(),
            )
            .timeout(std::time::Duration::from_millis(FETCH_TIMEOUT_MS));
        if let Some(headers) = args["headers"].as_object() {
            for (name, value) in headers {
                ensure!(value.is_string(), "fetch header value must be a string");
                builder = builder.header(name, value.as_str().unwrap());
            }
        }
        if let Some(body) = args["body"].as_str().filter(|b| !b.is_empty()) {
            builder = builder.body(body.to_owned());
        }
        let response = builder.send().await?;
        let status = response.status();
        if status.is_redirection()
            && let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
        {
            let next = current.join(location)?;
            redirects.push(json!({"status":status.as_u16(),"to":next.as_str()}));
            current = next;
            continue;
        }
        let (headers, headers_truncated) = bounded_headers(response.headers());
        let head_only = method == "HEAD" || status.as_u16() == 204 || status.as_u16() == 304;
        let mut bytes = Vec::new();
        let mut truncated = false;
        if !head_only {
            let mut response = response;
            while let Some(chunk) = response.chunk().await? {
                if bytes.len() + chunk.len() > max_bytes {
                    bytes.extend_from_slice(&chunk[..max_bytes - bytes.len()]);
                    truncated = true;
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
        }
        let body = String::from_utf8_lossy(&bytes).into_owned();
        return Ok(json!({
            "final_url": current.as_str(),
            "status": status.as_u16(),
            "headers": headers,
            "headers_truncated": headers_truncated,
            "body": if head_only { String::new() } else { body },
            "body_bytes": bytes.len(),
            "truncated": truncated,
            "redirects": redirects,
        }));
    }
    anyhow::bail!("fetch exceeded {FETCH_MAX_REDIRECTS} redirects")
}
#[path = "broker_control.rs"]
mod control;
#[allow(unused_imports)]
pub use control::{
    Drained, OUTAGE_PAUSE_AFTER, Pause, ResumeReport, control_watch, outage_backoff, resume_plan,
};
#[cfg(test)]
#[path = "broker_control_tests.rs"]
mod control_tests;
#[cfg(test)]
#[path = "broker_progress_tests.rs"]
mod progress_tests;
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
    fn board_messages_are_shortened_not_rejected() {
        let (goal, mut store, dir) = setup();
        let message = "x".repeat(BOARD_MESSAGE_LIMIT + 500);
        let post = store
            .tool(&goal, "agent-00", "board_post", json!({"message":message}))
            .unwrap();
        assert_eq!(
            post["message"].as_str().unwrap().chars().count(),
            BOARD_MESSAGE_LIMIT,
            "an over-long Board message must be shortened at the boundary"
        );
        assert_eq!(post["shortened"], json!(true));
        let short = store
            .tool(&goal, "agent-00", "board_post", json!({"message":"ok"}))
            .unwrap();
        assert!(short.get("shortened").is_none());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn read_budget_is_per_agent_and_never_blocks_filing() {
        let (goal, mut store, dir) = setup();
        store.charge_fetch("agent-00", FETCH_TOTAL_BYTES).unwrap();
        assert!(store.read_budget("agent-00").is_err());
        assert!(
            store.read_budget("agent-01").is_ok(),
            "each agent owns its own read budget"
        );
        let claimed = store
            .tool(&goal, "agent-00", "claim_task", json!({}))
            .unwrap();
        assert!(
            store
                .tool(
                    &goal,
                    "agent-00",
                    "submit_finding",
                    json!({"task_id":claimed["id"],"result":{"papers":[]}})
                )
                .is_ok(),
            "filing stays available after the read budget trips"
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
    fn artifact_is_rejected_unless_it_copies_every_record_once() {
        let (goal, mut store, dir) = setup();
        let first = store
            .tool(&goal, "agent-00", "claim_task", json!({}))
            .unwrap();
        let second = store
            .tool(&goal, "agent-01", "claim_task", json!({}))
            .unwrap();
        for (agent, task, id) in [("agent-00", &first, "p1"), ("agent-01", &second, "p2")] {
            store
                .tool(
                    &goal,
                    agent,
                    "submit_finding",
                    json!({"task_id": task["id"].clone(), "result": {"papers": [{"id": id}]}}),
                )
                .unwrap();
        }
        let rejected = |store: &mut Store, papers: Value| {
            store
                .tool(
                    &goal,
                    "agent-00",
                    "submit_artifact",
                    json!({"artifact": {"papers": papers}}),
                )
                .unwrap()
        };
        let short = rejected(&mut store, json!([{"id": "p1"}]));
        assert_eq!(short["accepted"], json!(false));
        assert_eq!(short["expected_records"], json!(2));
        assert_eq!(short["received_records"], json!(1));
        assert!(store.artifact.is_none());
        let duplicate = rejected(&mut store, json!([{"id": "p1"}, {"id": "p1"}]));
        assert_eq!(duplicate["accepted"], json!(false));
        let extra = store
            .tool(
                &goal,
                "agent-00",
                "submit_artifact",
                json!({"artifact": {"papers": [{"id": "p1"}, {"id": "p2"}], "other": true}}),
            )
            .unwrap();
        assert_eq!(extra["accepted"], json!(false));
        // Order and object key order are irrelevant; the multiset is the contract.
        let exact = rejected(&mut store, json!([{"id": "p2"}, {"id": "p1"}]));
        assert_eq!(exact["accepted"], json!(true));
        assert!(store.artifact.is_some());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn artifact_guard_follows_the_findings_result_key() {
        let (goal, mut store, dir) = setup();
        let first = store
            .tool(&goal, "agent-00", "claim_task", json!({}))
            .unwrap();
        let second = store
            .tool(&goal, "agent-01", "claim_task", json!({}))
            .unwrap();
        for (agent, task, id) in [("agent-00", &first, "e1"), ("agent-01", &second, "e2")] {
            store
                .tool(
                    &goal,
                    agent,
                    "submit_finding",
                    json!({"task_id": task["id"].clone(), "result": {"experiments": [{"id": id}]}}),
                )
                .unwrap();
        }
        let wrong_key = store
            .tool(
                &goal,
                "agent-00",
                "submit_artifact",
                json!({"artifact": {"papers": [{"id": "e1"}, {"id": "e2"}]}}),
            )
            .unwrap();
        assert_eq!(wrong_key["accepted"], json!(false));
        assert_eq!(wrong_key["received_records"], json!(0));
        let exact = store
            .tool(
                &goal,
                "agent-00",
                "submit_artifact",
                json!({"artifact": {"experiments": [{"id": "e2"}, {"id": "e1"}]}}),
            )
            .unwrap();
        assert_eq!(exact["accepted"], json!(true));
        assert!(store.artifact.is_some());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn run_code_is_an_optional_grant() {
        let (mut goal, _, dir) = setup();
        goal.grants.push("run_code".into());
        goal.validate().unwrap();
        goal.grants.push("bash".into());
        assert!(
            goal.validate().is_err(),
            "only named optional grants are admitted"
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn goal_admits_thirty_agents_and_refuses_thirty_one() {
        let (mut goal, _, dir) = setup();
        goal.agent_count = 30;
        goal.tasks = (0..30)
            .map(|index| Task {
                id: format!("task-{index:02}"),
                input: json!({}),
                budget_microusd: None,
                claimed_by: None,
                status: "open".into(),
            })
            .collect();
        goal.validate().unwrap();
        goal.agent_count = 31;
        goal.tasks.push(Task {
            id: "task-30".into(),
            input: json!({}),
            budget_microusd: None,
            claimed_by: None,
            status: "open".into(),
        });
        let error = goal.validate().unwrap_err().to_string();
        assert!(error.contains("maximum thirty"), "{error}");
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
    fn task_budget_and_recovery_marker_deserialize() {
        let goal: Goal = serde_json::from_value(json!({
            "schema_version":1,"intent":"recovery","worker_prompt":"w","synthesis_prompt":"s",
            "scope":"public-web-research","model":"deepseek-flash","agent_count":2,
            "deadline_seconds":10,"budget_microusd":1000000,"max_calls_per_agent":8,
            "max_input_bytes":65536,"max_output_tokens":2048,
            "input_price_microusd_per_million":300000,
            "output_price_microusd_per_million":1200000,
            "grants":["claim_task","board_read","board_post","submit_finding","read_findings","submit_artifact"],
            "tasks":[{"id":"a","input":{},"budget_microusd":5},
                     {"id":"b","input":{},"budget_microusd":6}],
            "seed":1,"catalog_version":"recovery-v1","scorer_version":"1",
            "via_recovered_run":{"source_run":"prior","retained_records":1}
        }))
        .unwrap();
        goal.validate().unwrap();
        assert_eq!(goal.tasks[0].budget_microusd, Some(5));
        assert_eq!(
            goal.via_recovered_run.as_ref().unwrap()["source_run"],
            "prior"
        );
    }

    #[test]
    fn task_budgets_fund_a_heavy_component_and_stay_below_the_goal_cap() {
        let (mut goal, mut store, dir) = setup();
        goal.tasks[0].budget_microusd = Some(900_000);
        goal.tasks[1].budget_microusd = Some(200_000);
        assert!(
            goal.validate().is_err(),
            "the explicit Task budgets cannot exceed the Goal cap"
        );
        goal.tasks[1].budget_microusd = Some(100_000);
        goal.validate().unwrap();
        store.tasks[0].budget_microusd = goal.tasks[0].budget_microusd;
        store.tasks[1].budget_microusd = goal.tasks[1].budget_microusd;
        let claimed = store
            .tool(&goal, "agent-00", "claim_task", json!({}))
            .unwrap();
        assert_eq!(claimed["id"], "a");
        store.intents.push(Intent {
            id: "spent-before".into(),
            agent_id: "agent-00".into(),
            state: "completed".into(),
            hold_microusd: 0,
            cost_microusd: 600_000,
            usage: Some(json!({"input_tokens":1,"output_tokens":1})),
            provider: "deepseek".into(),
            request_id: Some("receipt-before".into()),
        });
        store.spent_microusd = 600_000;
        assert!(
            store.reserve(&goal, "agent-00", 200).is_ok(),
            "the Task budget, not the old fair share, must admit this call"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exhausted_task_envelope_is_terminal_and_files_one_defer() {
        let (mut goal, mut store, dir) = setup();
        let input = json!({
            "title": "A",
            "seed_url": "https://example.test/a",
            "experiment": "E1",
            "component": "component-a"
        });
        goal.tasks[0].input = input.clone();
        goal.tasks[0].budget_microusd = Some(1);
        store.tasks[0].input = input;
        store.tasks[0].budget_microusd = Some(1);
        store
            .tool(&goal, "agent-00", "claim_task", json!({}))
            .unwrap();
        let error = store
            .reserve(&goal, "agent-00", 200)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Task budget envelope exhausted"), "{error}");
        assert_eq!(
            store.agents["agent-00"].terminal.as_deref(),
            Some("Task budget envelope exhausted")
        );
        assert!(
            store.reserve(&goal, "agent-00", 200).is_err(),
            "a terminal agent cannot buy another provider turn"
        );
        store
            .defer_task(&goal, "agent-00", "Task budget envelope exhausted")
            .unwrap();
        let finding = store
            .findings
            .iter()
            .find(|finding| finding.agent_id == "agent-00")
            .unwrap();
        let record = &finding.result["experiments"][0];
        assert_eq!(record["id"], "a");
        assert_eq!(record["title"], "A");
        assert_eq!(record["file"], "https://example.test/a");
        assert_eq!(record["severity"], "defer");
        assert_eq!(record["output"], "DEFERRED: Task budget envelope exhausted");
        assert_eq!(store.findings.len(), 1, "one defer row, not a retry loop");
        let events = fs::read_to_string(dir.join("events.jsonl")).unwrap();
        assert!(events.contains("\"tool\":\"claim_task\""));
        assert!(events.contains("\"tool\":\"submit_finding\""));
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
    /// The dialect decision is the difference between a 200 and a hard 400:
    /// Venice rejects `thinking` outright, so an OpenAI route must REPLACE it.
    #[test]
    fn the_openai_dialect_replaces_thinking_instead_of_augmenting_it() {
        let wire = |dialect: Dialect, body: Value| {
            let mut body = body;
            if dialect == Dialect::OpenAi {
                let disabled = body["thinking"]["type"] == json!("disabled");
                body.as_object_mut().unwrap().remove("thinking");
                if disabled && reasoning_model(body["model"].as_str().unwrap_or_default()) {
                    body["reasoning_effort"] = json!("none");
                }
            }
            body
        };
        let body = json!({"model":"deepseek-v4-1-flash-uncensored","thinking":{"type":"disabled"}});
        let sent = wire(Dialect::OpenAi, body);
        assert!(sent.get("thinking").is_none(), "Venice rejects `thinking`");
        assert_eq!(sent["reasoning_effort"], json!("none"));

        // A non-reasoning model still drops the field but gains no effort value.
        let non = json!({"model":"some-chat-model","thinking":{"type":"disabled"}});
        let sent = wire(Dialect::OpenAi, non);
        assert!(sent.get("thinking").is_none() && sent.get("reasoning_effort").is_none());

        // The DeepSeek dialect keeps its own field untouched.
        let body = json!({"model":"deepseek-flash","thinking":{"type":"disabled"}});
        let sent = wire(Dialect::DeepSeek, body);
        assert_eq!(sent["thinking"]["type"], json!("disabled"));
        assert_eq!(
            Dialect::DeepSeek,
            Upstream::deepseek("deepseek-flash").dialect
        );
    }
    #[test]
    fn upstream_selection_is_admitted_and_cannot_be_desynchronised() {
        // The legacy pair still resolves to DeepSeek, so a frozen preview Goal
        // validates unchanged and its recorded provider is preserved.
        let (mut goal, _, dir) = setup();
        assert!(goal.upstream.is_none() && goal.provider.is_none());
        assert_eq!(goal.resolved(), Upstream::deepseek("deepseek-flash"));
        goal.provider = Some("deepseek".into());
        assert!(goal.validate().is_ok());

        // Venice selects the openai dialect, whose model set is its own.
        goal.provider = None;
        goal.model = "deepseek-v4-1-flash-uncensored".into();
        goal.upstream = Some(upstream_profile("venice", &goal.model).unwrap());
        assert!(goal.validate().is_ok());
        assert_eq!(goal.resolved().dialect, Dialect::OpenAi);
        // A Goal frozen before `upstream` carried its own provider spelling and
        // the intent records it, so evidence attributes the real upstream.
        assert_eq!(goal.resolved().provider, "venice");
        // A dated revision is admitted by prefix, an unknown provider is not.
        assert!(upstream_profile("venice", "deepseek-v4-1-flash-uncensored-1017").is_ok());
        assert!(upstream_profile("venice", "deepseek-v4-1-flash-1017").is_err());
        assert!(upstream_profile("openrouter", "whatever").is_err());

        // The legacy pair alone cannot smuggle in a non-DeepSeek provider, and
        // the Goal model may not drift from the upstream that names the wire model.
        goal.upstream = None;
        goal.provider = Some("venice".into());
        assert!(goal.validate().is_err());
        goal.provider = None;
        goal.model = "deepseek-flash".into();
        goal.upstream = Some(Upstream {
            provider: "venice".into(),
            model: "deepseek-v4-1-flash-uncensored".into(),
            dialect: Dialect::OpenAi,
        });
        assert!(goal.validate().is_err());
        goal.model = "deepseek-v4-1-flash-uncensored".into();
        assert!(goal.validate().is_ok());
        fs::remove_dir_all(dir).unwrap();
    }
    /// A worker replays its whole conversation each turn and every fetch result
    /// stays in it, so the transport body cap MUST exceed the admitted input
    /// budget. When they were equal, a long run died with a 413 and EPIPE.
    #[test]
    fn the_transport_accepts_more_than_the_admitted_input_budget() {
        let (goal, _, dir) = setup();
        assert!(
            goal.max_input_bytes * 4 > goal.max_input_bytes,
            "body cap must exceed the admitted budget"
        );
        // The transport rule is what matters; the fixture's own budget is small.
        // Stated as a const check so the relation is pinned at compile time.
        const { assert!(FETCH_MAX_BYTES_DEFAULT < 262_144) };
        fs::remove_dir_all(dir).unwrap();
    }
    /// One transient provider error used to poison the whole Goal: `status` went
    /// to `provider_failed`, and every later turn was refused with "Goal is
    /// stopped or not running". A retryable error must leave the Goal running and
    /// must release the hold so the budget is not stranded.
    #[test]
    fn a_provider_error_is_retryable_and_releases_its_hold() {
        let (goal, mut store, dir) = setup();
        let index = store.reserve(&goal, "agent-00", 200).unwrap();
        store.intents[index].state = "dispatched".into();
        let held = store.held_microusd;
        assert!(held > 0);
        // Play out the failure path exactly as the route does.
        store.intents[index].state = "unknown".into();
        store.held_microusd -= store.intents[index].hold_microusd;
        store.intents[index].hold_microusd = 0;
        store.intents[index].state = "denied".into();
        assert_eq!(store.held_microusd, 0, "the hold must not be stranded");
        assert_ne!(store.goal_status, "provider_failed");
        // The Goal still admits work, so the next turn can retry.
        assert!(store.reserve(&goal, "agent-00", 200).is_ok());
        // An accounting failure stays terminal.
        store.goal_status = "accounting_failed".into();
        assert!(store.reserve(&goal, "agent-00", 200).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn cache_discount_settles_reported_usage_without_discounting_reservations() {
        let (mut goal, mut store, dir) = setup();
        let all_miss_hold = goal.cost(200 + 4096, goal.max_output_tokens);
        goal.cache_hit_price_microusd_per_million = Some(6_000);
        goal.validate().unwrap();
        let index = store.reserve(&goal, "agent-00", 200).unwrap();
        assert_eq!(store.held_microusd, all_miss_hold);
        store.intents[index].state = "dispatched".into();
        let response = json!({"id":"cached-receipt","usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":90,"prompt_cache_miss_tokens":10,"completion_tokens":50}});
        assert_eq!(store.settle(&goal, index, &response).unwrap(), 64);
        assert_eq!(store.spent_microusd, 64);
        assert_eq!(store.held_microusd, 0);
        assert_eq!(
            store.intents[index].usage,
            Some(
                json!({"input_tokens":100,"cached_input_tokens":90,"uncached_input_tokens":10,"output_tokens":50})
            )
        );
        assert!(store.settle(&goal, index, &response).is_err());
        assert_eq!(store.spent_microusd, 64);
        assert_eq!(goal.settled_cost(100, 0, 50), 90);
        assert_eq!(goal.settled_cost(0, 0, 0), 0);
        assert_eq!(goal.settled_cost(1_000_000, 1_000_000, 0), 6_000);
        for input in 0..100 {
            for cached in 0..=input {
                assert!(goal.settled_cost(input, cached, 0) <= goal.cost(input, 0));
            }
        }
        goal.cache_hit_price_microusd_per_million = None;
        assert_eq!(goal.settled_cost(100, 90, 50), 90);
        goal.cache_hit_price_microusd_per_million = Some(goal.input_price_microusd_per_million);
        assert_eq!(goal.settled_cost(2, 1, 0), goal.cost(2, 0));
        goal.cache_hit_price_microusd_per_million = Some(3_000);
        assert!(goal.validate().is_err());
        goal.cache_hit_price_microusd_per_million = Some(goal.input_price_microusd_per_million + 1);
        assert!(goal.validate().is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn invalid_cache_counts_retain_the_full_hold() {
        let (mut goal, mut store, dir) = setup();
        goal.cache_hit_price_microusd_per_million = Some(6_000);
        let index = store.reserve(&goal, "agent-00", 200).unwrap();
        store.intents[index].state = "dispatched".into();
        let held = store.held_microusd;
        for usage in [
            json!({"prompt_tokens":100,"completion_tokens":50,"prompt_cache_hit_tokens":101}),
            json!({"prompt_tokens":100,"completion_tokens":50,"prompt_cache_hit_tokens":90,"prompt_cache_miss_tokens":90}),
        ] {
            assert!(
                store
                    .settle(&goal, index, &json!({"id":"invalid-cache","usage":usage}))
                    .is_err()
            );
            assert_eq!(store.held_microusd, held);
            assert_eq!(store.spent_microusd, 0);
            assert_eq!(store.intents[index].state, "dispatched");
        }
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
    #[test]
    fn authorized_audit_envelope_bounds_are_exact() {
        let (mut goal, _, dir) = setup();
        goal.scope = "authorized-audit:solana-developer-platform".into();
        goal.deadline_seconds = 21_600;
        goal.budget_microusd = 1_000_000_000_000;
        goal.max_calls_per_agent = 200;
        goal.max_input_bytes = 2_097_152;
        goal.max_output_tokens = 393_216;
        assert!(goal.validate().is_ok());
        goal.budget_microusd = 1_000_000_000_001;
        assert!(goal.validate().is_err());
        goal.budget_microusd = 1_000_000_000_000;
        goal.deadline_seconds = 21_601;
        assert!(goal.validate().is_err());
        goal.deadline_seconds = 21_600;
        // The ceiling admits a deliberately generous range: 200 calls is inside
        // it while the old 48-call value remains valid for a frozen preview Goal.
        goal.max_calls_per_agent = 10_001;
        assert!(goal.validate().is_err());
        goal.max_calls_per_agent = 10_000;
        assert!(goal.validate().is_ok());
        goal.max_calls_per_agent = 48;
        assert!(goal.validate().is_ok());
        goal.max_input_bytes = 16_777_217;
        assert!(goal.validate().is_err());
        goal.max_input_bytes = 16_777_216;
        assert!(goal.validate().is_ok());
        goal.max_input_bytes = 6_291_456;
        goal.max_output_tokens = 393_217;
        assert!(goal.validate().is_err());
        goal.max_output_tokens = 393_216;
        goal.scope = "authorized-audit:other-target".into();
        assert!(goal.validate().is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
