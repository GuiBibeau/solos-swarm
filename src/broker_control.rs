//! Pause, drain and resume for one broker run.
//!
//! One gate refuses new model work and new task claims. Operators reach it with
//! `control.json` in the run directory, and a provider outage reaches it on its
//! own. Draining waits for in-flight calls to settle, so a drained run holds no
//! budget. Resuming rebuilds the Store from `state.json` after proving it
//! matches the hash-chained `events.jsonl`.
use super::{Agent, Finding, Goal, Intent, Store, Task, digest, file_digest, now_ms, write_json};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

/// Consecutive provider outage errors, across all agents, before the run pauses.
pub const OUTAGE_PAUSE_AFTER: u32 = 5;
const STATE_SCHEMA: u32 = 1;
const DRAIN_GRACE_SECONDS: u64 = 120;
const DRAIN_GRACE_MAX_SECONDS: u64 = 900;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pause {
    /// `drain` (ends the run, resumable) or `pause` (run stays up, gate shut).
    pub mode: String,
    pub reason: String,
    pub since_ms: u64,
}

/// The control watcher returns this error once a drain has settled.
#[derive(Debug)]
pub struct Drained;
impl std::fmt::Display for Drained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "run drained at operator request")
    }
}
impl std::error::Error for Drained {}

/// 250 ms doubling to an 8 s ceiling.
pub fn outage_backoff(consecutive: u32) -> Duration {
    let exponent = consecutive.saturating_sub(1).min(5);
    Duration::from_millis((250u64 << exponent).min(8_000))
}

enum Failure {
    Outage,
    Billing,
    Other,
}
/// Classify a provider error by the messages `OpenAiCompatible` produces.
fn classify(message: &str) -> Failure {
    if message.contains("transport failure") || message.contains("response interrupted") {
        return Failure::Outage;
    }
    let status = message
        .split("HTTP ")
        .nth(1)
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse::<u16>().ok());
    match status {
        Some(401..=403) => Failure::Billing,
        Some(408 | 425 | 429 | 500..=599) => Failure::Outage,
        _ => Failure::Other,
    }
}

impl Store {
    /// Refuse a model reservation while the run is paused or draining.
    pub(super) fn gate_model(&self) -> Result<()> {
        if let Some(pause) = &self.pause {
            bail!(
                "run {}: {}; no model call was made",
                pause.mode,
                pause.reason
            );
        }
        Ok(())
    }
    /// Refuse a new claim while paused. An agent that already holds a task keeps it.
    pub(super) fn gate_claim(&self, id: &str) -> Result<()> {
        let holds = self
            .tasks
            .iter()
            .any(|task| task.claimed_by.as_deref() == Some(id));
        match (&self.pause, holds) {
            (Some(pause), false) => bail!("run {}: no new tasks are granted", pause.mode),
            _ => Ok(()),
        }
    }
    pub fn begin_pause(&mut self, mode: &str, reason: &str) -> Result<()> {
        self.begin_pause_from(mode, reason, true)
    }
    /// `publish` mirrors the pause into `control.json`, the one file every consumer
    /// (model handler, deadline clock, doctor) reads. A pause that arrived through
    /// that file is not written back.
    fn begin_pause_from(&mut self, mode: &str, reason: &str, publish: bool) -> Result<()> {
        // A drain supersedes a pause; nothing supersedes a drain.
        if self
            .pause
            .as_ref()
            .is_some_and(|p| p.mode == "drain" || mode == "pause")
        {
            return Ok(());
        }
        let pause = Pause {
            mode: mode.into(),
            reason: reason.chars().take(300).collect(),
            since_ms: now_ms(),
        };
        // Alert first: the file is what an operator or the status command reads.
        write_json(
            &self.dir.join("alert.json"),
            &json!({"kind":"run_paused","pause":pause}),
        )?;
        let event = json!({"mode":pause.mode,"reason":pause.reason});
        if publish {
            self.publish_control(mode, reason)?;
        }
        // The gate closes even if the log write fails.
        self.pause = Some(pause);
        self.record("run_paused", event)
    }
    /// Lift a `pause`. A `drain` is final for this process.
    pub fn lift_pause(&mut self) -> Result<bool> {
        if self.pause.as_ref().is_none_or(|p| p.mode != "pause") {
            return Ok(false);
        }
        self.pause = None;
        self.outage_streak = 0;
        let _ = fs::remove_file(self.dir.join("alert.json"));
        self.publish_control("run", "pause lifted")?;
        self.record("run_unpaused", json!({}))?;
        Ok(true)
    }
    fn publish_control(&self, mode: &str, reason: &str) -> Result<()> {
        let path = self.dir.join("control.json");
        let current = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        if current.is_some_and(|c| c["mode"] == mode) {
            return Ok(());
        }
        write_json(
            &path,
            &json!({"mode":mode,"reason":reason,"source":"broker"}),
        )
    }
    /// No call is reserved or in flight, and no budget is held.
    pub fn quiescent(&self) -> bool {
        self.held_microusd == 0
            && self
                .intents
                .iter()
                .all(|i| !matches!(i.state.as_str(), "reserved" | "dispatched"))
    }
    pub(super) fn note_provider_ok(&mut self) {
        self.outage_streak = 0;
    }
    /// Count a failed provider call and return how long the caller should back off.
    /// Outages pause the run after `OUTAGE_PAUSE_AFTER` in a row; a rejected
    /// credential or billing error pauses at once, since retrying cannot help.
    pub(super) fn note_provider_failure(&mut self, message: &str) -> Duration {
        let kind = classify(message);
        let reason = match kind {
            Failure::Outage => {
                self.outage_streak += 1;
                (self.outage_streak >= OUTAGE_PAUSE_AFTER).then(|| {
                    format!(
                        "provider outage: {} consecutive failures",
                        self.outage_streak
                    )
                })
            }
            Failure::Billing => Some("provider rejected credentials or billing".into()),
            Failure::Other => None,
        };
        if let Some(reason) = reason {
            // A failed alert write must not hide the pause; the gate still closes.
            if self.begin_pause("pause", &reason).is_err() {
                self.pause = Some(Pause {
                    mode: "pause".into(),
                    reason,
                    since_ms: now_ms(),
                });
            }
        }
        match kind {
            Failure::Outage => outage_backoff(self.outage_streak),
            _ => Duration::ZERO,
        }
    }
    /// Settle intents a dead or drained process left open. A reservation never
    /// sent releases its hold. A sent call whose outcome is unknown is charged at
    /// its ceiling, so a retry can never make the run cheaper than reality.
    pub fn close_orphans(&mut self, why: &str) -> Result<Vec<String>> {
        let mut closed = Vec::new();
        for index in 0..self.intents.len() {
            let (state, hold) = (
                self.intents[index].state.clone(),
                self.intents[index].hold_microusd,
            );
            let action = match state.as_str() {
                "reserved" => "released",
                "dispatched" | "unknown" if hold > 0 => "charged_ceiling",
                _ => continue,
            };
            self.held_microusd = self.held_microusd.saturating_sub(hold);
            let intent = &mut self.intents[index];
            if action == "released" {
                intent.state = "denied".into();
            } else {
                intent.state = "unknown_charged".into();
                intent.cost_microusd = hold;
                self.spent_microusd += hold;
            }
            self.intents[index].hold_microusd = 0;
            let id = self.intents[index].id.clone();
            self.record(
                "intent_orphaned",
                json!({"intent_id":id,"action":action,"why":why}),
            )?;
            closed.push(id);
        }
        Ok(closed)
    }
    /// Apply one `control.json` document. Returns the drain grace for a drain.
    pub fn apply_control(&mut self, text: &str) -> Result<Option<u64>> {
        let control: Value = serde_json::from_str(text).context("control.json is not JSON")?;
        let reason = control["reason"].as_str().unwrap_or("operator request");
        match control["mode"].as_str() {
            Some("pause") => self.begin_pause_from("pause", reason, false).map(|()| None),
            Some("run") => self.lift_pause().map(|_| None),
            Some("drain") => {
                self.begin_pause_from("drain", reason, false)?;
                let grace = control["grace_seconds"]
                    .as_u64()
                    .unwrap_or(DRAIN_GRACE_SECONDS);
                Ok(Some(grace.clamp(1, DRAIN_GRACE_MAX_SECONDS)))
            }
            _ => bail!("control mode must be drain, pause or run"),
        }
    }
    /// The snapshot a resume checks against. Written last, after cleanup.
    pub fn write_resume_manifest(&self) -> Result<()> {
        let state = fs::read(self.dir.join("state.json"))?;
        let runtime = fs::read(self.dir.join("manifest.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .map(|manifest| manifest["runtime_sha256"].clone())
            .unwrap_or(Value::Null);
        let filed = self.findings.len();
        write_json(
            &self.dir.join("resume.json"),
            &json!({
                "schema_version":1,"state_schema":STATE_SCHEMA,"last_seq":self.seq,
                "state_sha256":digest(&state),"goal_status":self.goal_status,
                "spent_microusd":self.spent_microusd,"held_microusd":self.held_microusd,
                "tasks_total":self.tasks.len(),"tasks_filed":filed,"runtime_sha256":runtime,
                "written_ms":now_ms()
            }),
        )
    }
}

/// Wait for a control request. After a drain request, wait for in-flight calls to
/// settle (or the grace to elapse, charging any leftover at its ceiling), then
/// return `Drained`. Pause and run requests are applied and the watch continues.
pub async fn control_watch(store: Arc<Mutex<Store>>, dir: PathBuf) -> Result<()> {
    let path = dir.join("control.json");
    let mut seen = String::new();
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if text == seen {
            continue;
        }
        seen.clone_from(&text);
        let grace = {
            let mut state = store.lock().await;
            match state.apply_control(&text) {
                Ok(Some(grace)) => grace,
                Ok(None) => continue,
                Err(error) => {
                    state.record("control_rejected", json!({"reason":error.to_string()}))?;
                    continue;
                }
            }
        };
        let until = tokio::time::Instant::now() + Duration::from_secs(grace);
        loop {
            {
                let mut state = store.lock().await;
                if state.quiescent() {
                    break;
                }
                if tokio::time::Instant::now() >= until {
                    state.close_orphans("drain grace elapsed")?;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        return Err(anyhow::Error::new(Drained));
    }
}

#[derive(Deserialize)]
pub(super) struct Image {
    schema_version: u32,
    goal_status: String,
    agents: std::collections::BTreeMap<String, Agent>,
    tasks: Vec<Task>,
    findings: Vec<Finding>,
    intents: Vec<Intent>,
    board: Vec<Value>,
    spent_microusd: u64,
    held_microusd: u64,
    artifact: Option<Value>,
    last_seq: u64,
}

pub(super) struct Chain {
    pub(super) last_seq: u64,
    last_hash: String,
    good_bytes: u64,
    truncated_tail: bool,
}
/// Verify every event's sequence and hash. A torn final line is tolerated and
/// reported; anything else wrong with the chain is an error.
pub(super) fn read_chain(dir: &Path) -> Result<Chain> {
    let text = fs::read_to_string(dir.join("events.jsonl")).context("events.jsonl unreadable")?;
    let mut chain = Chain {
        last_seq: 0,
        last_hash: "0".repeat(64),
        good_bytes: 0,
        truncated_tail: false,
    };
    let mut offset = 0u64;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    for (index, line) in lines.iter().enumerate() {
        offset += line.len() as u64;
        let parsed = serde_json::from_str::<Value>(line.trim_end());
        let mut event = match parsed {
            Ok(event) if line.ends_with('\n') => event,
            _ if index + 1 == lines.len() => {
                chain.truncated_tail = true;
                break;
            }
            _ => bail!("events.jsonl line {} is corrupt", index + 1),
        };
        ensure!(
            event["seq"] == chain.last_seq + 1,
            "event {} out of sequence",
            index + 1
        );
        ensure!(
            event["prev_hash"] == chain.last_hash,
            "event {} breaks the hash chain",
            index + 1
        );
        let claimed = event["hash"].as_str().unwrap_or_default().to_owned();
        let object = event.as_object_mut().context("event is not an object")?;
        object.remove("prev_hash");
        object.remove("hash");
        let expected =
            digest(format!("{}{}", chain.last_hash, serde_json::to_string(&event)?).as_bytes());
        ensure!(expected == claimed, "event {} hash mismatch", index + 1);
        chain.last_seq += 1;
        chain.last_hash = claimed;
        chain.good_bytes = offset;
    }
    Ok(chain)
}

pub(super) fn load_image(dir: &Path, goal: &Goal) -> Result<(Image, Chain)> {
    let chain = read_chain(dir)?;
    let image: Image = serde_json::from_slice(&fs::read(dir.join("state.json"))?)
        .context("state.json does not match this runtime's schema")?;
    ensure!(
        !matches!(
            image.goal_status.as_str(),
            "accounting_failed" | "storage_failed"
        ),
        "run stopped as {}; its ledger is not trustworthy enough to resume",
        image.goal_status
    );
    ensure!(
        image.schema_version == STATE_SCHEMA,
        "state schema {} is not resumable by this runtime (supports {STATE_SCHEMA})",
        image.schema_version
    );
    ensure!(
        image.last_seq == chain.last_seq,
        "state.json is at event {} but the log is at {}; the state write was interrupted",
        image.last_seq,
        chain.last_seq
    );
    let frozen: Vec<&str> = goal.tasks.iter().map(|t| t.id.as_str()).collect();
    let kept: Vec<&str> = image.tasks.iter().map(|t| t.id.as_str()).collect();
    ensure!(
        frozen == kept,
        "the Goal's tasks differ from the run's frozen tasks"
    );
    ensure!(
        image.spent_microusd + image.held_microusd <= goal.budget_microusd,
        "recorded spend exceeds the Goal budget"
    );
    Ok((image, chain))
}

#[derive(Debug, Serialize)]
pub struct ResumeReport {
    pub last_seq: u64,
    pub truncated_tail: bool,
    pub goal_status: String,
    pub spent_microusd: u64,
    pub held_microusd: u64,
    pub release_intents: Vec<String>,
    pub charge_intents: Vec<String>,
    pub charge_microusd: u64,
    pub respawn_agents: Vec<String>,
    pub finished_agents: Vec<String>,
}

fn report(image: &Image, chain: &Chain, goal: &Goal) -> ResumeReport {
    let mut report = ResumeReport {
        last_seq: chain.last_seq,
        truncated_tail: chain.truncated_tail,
        goal_status: image.goal_status.clone(),
        spent_microusd: image.spent_microusd,
        held_microusd: image.held_microusd,
        release_intents: vec![],
        charge_intents: vec![],
        charge_microusd: 0,
        respawn_agents: vec![],
        finished_agents: vec![],
    };
    for intent in &image.intents {
        match intent.state.as_str() {
            "reserved" => report.release_intents.push(intent.id.clone()),
            "dispatched" | "unknown" if intent.hold_microusd > 0 => {
                report.charge_intents.push(intent.id.clone());
                report.charge_microusd += intent.hold_microusd;
            }
            _ => {}
        }
    }
    for n in 0..goal.agent_count {
        let id = format!("agent-{n:02}");
        let done = image.findings.iter().any(|finding| finding.agent_id == id);
        if done {
            &mut report.finished_agents
        } else {
            &mut report.respawn_agents
        }
        .push(id);
    }
    report
}

/// What a resume of `dir` would do, without touching it.
pub fn resume_plan(dir: &Path) -> Result<ResumeReport> {
    let goal: Goal = serde_yaml::from_slice(&fs::read(dir.join("config.yaml"))?)?;
    let (image, chain) = load_image(dir, &goal)?;
    Ok(report(&image, &chain, &goal))
}

impl Store {
    /// Rebuild a Store from a stopped run's directory and settle its orphans.
    /// Agents come back `stopped`; `admit` revives each with its counters.
    pub fn resume(dir: &Path, goal: &Goal) -> Result<(Store, ResumeReport)> {
        let (image, chain) = load_image(dir, goal)?;
        let summary = report(&image, &chain, goal);
        let log_path = dir.join("events.jsonl");
        if chain.truncated_tail {
            OpenOptions::new()
                .write(true)
                .open(&log_path)?
                .set_len(chain.good_bytes)?;
        }
        let mut agents = image.agents;
        for agent in agents.values_mut() {
            agent.status = "stopped".into();
            agent.stopped_ms.get_or_insert_with(now_ms);
        }
        let mut store = Store {
            schema_version: image.schema_version,
            goal_status: "running".into(),
            agents,
            tasks: image.tasks,
            findings: image.findings,
            intents: image.intents,
            board: image.board,
            spent_microusd: image.spent_microusd,
            held_microusd: image.held_microusd,
            artifact: image.artifact,
            pause: None,
            outage_streak: 0,
            dir: dir.into(),
            log: OpenOptions::new().append(true).open(&log_path)?,
            seq: chain.last_seq,
            previous_hash: chain.last_hash,
            tokens: Default::default(),
        };
        let runtime = file_digest(&std::env::current_exe()?)?;
        store.record(
            "run_resumed",
            json!({"from_seq":chain.last_seq,"previous_status":summary.goal_status,"runtime_sha256":runtime}),
        )?;
        store.close_orphans("resume after stop")?;
        Ok((store, summary))
    }
}
