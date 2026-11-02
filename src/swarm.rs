//! One swarm run of a Goal: its agents start through an `AgentHarness`, work
//! through the broker until the artifact is filed, then stop, and the run
//! directory keeps the evidence. `solos-swarm run` is a thin wrapper over
//! `run_goal`.
use crate::{
    broker::{
        self, Broker, Goal, Provider, Store, Upstream, file_digest, now_ms, router, write_json,
    },
    code::CodeExecutor,
    harness::{AgentHarness, Observation, Role, Session, SessionEvent, SessionSpec},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    future::Future,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::UnixListener,
    signal::unix::{Signal, SignalKind, signal},
    sync::{Mutex, Semaphore, mpsc},
};

/// One run of an admitted Goal.
pub struct GoalRun {
    /// Validated, with `upstream` resolved: the broker and the manifest read
    /// the upstream from here, so it is not repeated beside the Goal.
    pub goal: Goal,
    /// The file the Goal was read from, copied into the run as `config.yaml`.
    pub config: PathBuf,
    pub run_dir: PathBuf,
    /// Unpaused seconds, at most `goal.deadline_seconds`.
    pub deadline: u64,
    /// Lifecycle test only: the agents start but are never prompted.
    pub idle_probe: bool,
    /// The Pi checkout holding `node_modules` and `extensions`.
    pub pi_root: PathBuf,
    pub node: PathBuf,
}

/// The Goal's final `goal_status`.
#[derive(Debug)]
pub enum GoalStatus {
    Completed,
    /// An operator drain. `resume.json` says where a resume starts.
    Drained,
    /// Deadline, cancel, or a run that failed. The run's files are written.
    Stopped(anyhow::Error),
}

/// What a caller needs once the run has ended and its files are written.
#[derive(Debug)]
pub struct GoalOutcome {
    pub status: GoalStatus,
    /// `runtime_verdict` in `runtime-report.json`.
    pub runtime_pass: bool,
    /// Sessions and readers that did not end cleanly; `cleanup.json` passes at zero.
    pub remaining_workers: usize,
    pub run_dir: PathBuf,
    /// `artifacts/final.json`, when the run has an artifact.
    pub artifact: Option<PathBuf>,
    pub spent_microusd: u64,
    /// Holds still open when the run ended: calls whose cost is unknown.
    pub held_microusd: u64,
    pub findings: usize,
}

/// What cancels a run before its end, as an operator's cancel does: the
/// unfiled Tasks get partial findings, what was filed becomes the artifact,
/// and every session stops.
pub struct Cancel {
    /// SIGTERM and Ctrl-C to this process.
    signals: bool,
    /// Resolves with the reason the host cancels the run.
    request: Option<Pin<Box<dyn Future<Output = String> + Send>>>,
}

impl Cancel {
    /// SIGTERM or Ctrl-C to this process, as `solos-swarm run` has.
    pub fn signals() -> Self {
        Self {
            signals: true,
            request: None,
        }
    }

    /// `request` resolving, with its reason. The process's signals are left
    /// to the host, which owns them and cancels its runs itself.
    pub fn on(request: impl Future<Output = String> + Send + 'static) -> Self {
        Self {
            signals: false,
            request: Some(Box::pin(request)),
        }
    }
}

/// The client the model transport and the broker's `fetch_url` both use.
pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// Run `run.goal` to its end. An `Err` means the run could not start or its
/// evidence could not be written; a run that started and stopped reports
/// `GoalStatus::Stopped`. SIGTERM and Ctrl-C cancel it.
pub async fn run_goal<H: AgentHarness>(
    run: GoalRun,
    harness: &H,
    provider: Arc<dyn Provider>,
) -> Result<GoalOutcome> {
    run_goal_until(run, harness, provider, Cancel::signals()).await
}

/// [`run_goal`], cancelled by `cancel`.
pub async fn run_goal_until<H: AgentHarness>(
    run: GoalRun,
    harness: &H,
    provider: Arc<dyn Provider>,
    cancel: Cancel,
) -> Result<GoalOutcome> {
    let Cancel { signals, request } = cancel;
    let GoalRun {
        goal,
        config,
        run_dir,
        deadline,
        idle_probe: idle,
        pi_root: root,
        node,
    } = run;
    let root = root.canonicalize()?;
    let node = node.canonicalize()?;
    let run_dir = run_dir.canonicalize()?;
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))?;
    let config_path = run_dir.join("config.yaml");
    if config.canonicalize()? != config_path {
        fs::copy(&config, &config_path)?;
    }
    let executable = std::env::current_exe()?;
    let upstream = goal.upstream.clone().context("resolved upstream absent")?;
    // A Goal frozen before `upstream` existed carries neither the extra key nor
    // the derived revision, so its digest and manifest are unchanged; a Goal
    // that names its upstream records both.
    let mut manifest = json!({"schema_version":1,"live":!idle,"provider":upstream.provider,"model":goal.model,"model_revision":model_revision(&upstream),"pi_version":"1.1.0","topology":"single-host","isolation":"bubblewrap-unshare-all","runtime_sha256":file_digest(&executable)?,"extension_sha256":file_digest(&root.join("extensions/broker.ts"))?,"package_lock_sha256":file_digest(&root.join("package-lock.json"))?,"artifacts":{"config.yaml":file_digest(&config_path)?},"seed":goal.seed,"catalog_version":goal.catalog_version,"scorer_version":goal.scorer_version,"started_ms":now_ms(),"price_table":{"revision":format!("{}-2026-10-27-conservative-peak-cache-aware",upstream.provider),"input_microusd_per_million":goal.input_price_microusd_per_million,"cache_hit_microusd_per_million":goal.cache_hit_price_microusd_per_million.unwrap_or(goal.input_price_microusd_per_million),"output_microusd_per_million":goal.output_price_microusd_per_million},"deadline_seconds":deadline,"deferred":["Raft/QUIC and multi-host HA","500-agent acceptance","SVID and signed audit witness","Firecracker tools/full Tier P","local provider and air gap","policy actors/module unloading/retractions","history optimization and three-family rotation"]});
    // The checker's legacy provider check reads this key; present it only when
    // it is accurate, so an old manifest stays byte-identical and a Venice one
    // is never mislabelled.
    if upstream.provider != "deepseek" {
        manifest["upstream_provider"] = json!(upstream.provider);
    }
    ensure!(
        !run_dir.join("events.jsonl").exists(),
        "run already exists; inspect its retained evidence instead of replaying"
    );
    write_json(&run_dir.join("manifest.json"), &manifest)?;
    let mut state = Store::new(&run_dir, &goal)?;
    state.record(
        "goal_registered",
        json!({"intent":goal.intent,"config_sha256":file_digest(&config_path)?}),
    )?;
    state.goal_status = "running".into();
    let store = Arc::new(Mutex::new(state));
    // A Goal that did not grant run_code must not even initialize the executor,
    // so the capability cannot be acquired by a frozen older case.
    let code = if goal.grants.iter().any(|grant| grant == "run_code") {
        Some(Arc::new(CodeExecutor::new(&run_dir)?))
    } else {
        None
    };
    let goal = Arc::new(goal);
    let broker = Broker {
        goal: goal.clone(),
        store: store.clone(),
        provider,
        slots: Arc::new(Semaphore::new(goal.agent_count)),
        client: http_client()?,
        code,
    };
    // Bind inside the 0700 run directory. A world-writable temp socket plus a
    // later chmod leaves a window where another user can connect.
    let broker_socket = prepare_broker_socket(&run_dir)?;
    let listener = UnixListener::bind(&broker_socket.path).context("broker socket bind failed")?;
    let server = tokio::spawn(async move { axum::serve(listener, router(broker)).await });
    let settings = run_dir.join("pi-settings.json");
    write_json(
        &settings,
        // Compaction is the context-overflow defence, so it is on even though a
        // worker may legitimately read for a long time. Reserve the model's
        // output room, not the default 16K, so the next request still fits.
        &json!({"retry":{"enabled":false},"compaction":{"enabled":true,"reserveTokens":65_536},"quietStartup":true}),
    )?;
    let (tx, mut rx) = mpsc::channel(256);
    let mut workers = Vec::new();
    let mut startup_error = None;
    for n in 0..goal.agent_count {
        let id = format!("agent-{n:02}");
        let token = store.lock().await.admit(&id)?;
        let spec = SessionSpec {
            // The broker grants the coordinator tools to agent-00 only.
            role: if n == 0 {
                Role::Coordinator
            } else {
                Role::Worker
            },
            id,
            grants: goal.grants.clone(),
            token,
            // The worker's Pi model id is the provider's wire model, because the
            // broker transcript echoes whichever id Pi asks for.
            model: upstream.model.clone(),
            run_dir: run_dir.clone(),
            broker_socket: broker_socket.path.clone(),
            pi_settings: settings.clone(),
            node: node.clone(),
            pi_root: root.clone(),
        };
        match harness.start(&spec, tx.clone()).await {
            Ok(worker) => workers.push(worker),
            Err(error) => {
                startup_error = Some(error);
                break;
            }
        }
    }
    drop(tx);
    let mut term = if signals {
        Some(signal(SignalKind::terminate())?)
    } else {
        None
    };
    let result = if let Some(error) = startup_error {
        Err(error)
    } else {
        tokio::select! {
            result=work(&mut workers,&store,&mut rx,&goal,idle) => result,
            _=unpaused_deadline(deadline, run_dir.clone()) => Err(anyhow::anyhow!("host-side Goal deadline expired")),
            _=broker::control_watch(store.clone(),run_dir.clone()) => Err(anyhow::Error::new(broker::Drained)),
            () = terminated(term.as_mut()) => Err(anyhow::anyhow!("operator cancelled run")),
            _=tokio::signal::ctrl_c(), if signals => Err(anyhow::anyhow!("operator interrupted run")),
            reason = requested(request) => Err(anyhow::anyhow!(reason)),
        }
    };
    let drained = matches!(&result, Err(error) if error.is::<broker::Drained>());
    {
        let mut state = store.lock().await;
        if let Err(error) = &result {
            // Deadline or cancel: keep what the agents did. Partial findings for
            // unfiled Tasks, then the exact-union artifact of whatever was filed.
            // A drain files nothing: the unfinished tasks belong to the resumed run.
            if !drained {
                let reason = error.to_string();
                let _ = state.file_partial_findings(&goal, &reason);
                let _ = state.synthesize_artifact(&reason);
            }
        }
        state.goal_status = if result.is_ok() {
            "completed"
        } else if drained {
            "drained"
        } else {
            "stopped"
        }
        .into();
        state.record("goal_stopped",json!({"reason":result.as_ref().err().map(|e|e.to_string()).unwrap_or_else(||"completed".into())}))?;
    }
    let remaining = stop_sessions(workers, &store).await?;
    server.abort();
    let _ = server.await;
    broker_socket.cleanup();
    {
        let mut state = store.lock().await;
        for intent in &mut state.intents {
            if intent.state == "dispatched" {
                intent.state = "unknown".into();
            }
        }
        state.record("cleanup_completed", json!({"remaining_workers":remaining}))?;
    }
    write_json(
        &run_dir.join("cleanup.json"),
        &json!({"verdict":if remaining==0 {"PASS"}else{"INCONCLUSIVE"},"remaining_workers":remaining,"stopped_ms":now_ms()}),
    )?;
    let runtime_pass = (result.is_ok() || drained) && store.lock().await.held_microusd == 0;
    if drained {
        // Last write: it names the exact event and state a resume must find.
        store.lock().await.write_resume_manifest()?;
    }
    write_json(
        &run_dir.join("runtime-report.json"),
        &json!({"runtime_verdict":if runtime_pass {"PASS"}else{"FAIL"},"reason":result.as_ref().err().map(|e|e.to_string()),"stopped_ms":now_ms(),"idle_probe":idle}),
    )?;
    let state = store.lock().await;
    Ok(GoalOutcome {
        status: match result {
            Ok(()) => GoalStatus::Completed,
            Err(_) if drained => GoalStatus::Drained,
            Err(error) => GoalStatus::Stopped(error),
        },
        runtime_pass,
        remaining_workers: remaining,
        artifact: state
            .artifact
            .is_some()
            .then(|| run_dir.join("artifacts/final.json")),
        spent_microusd: state.spent_microusd,
        held_microusd: state.held_microusd,
        findings: state.findings.len(),
        run_dir,
    })
}

/// SIGTERM, when the run listens for it.
async fn terminated(term: Option<&mut Signal>) {
    match term {
        Some(term) => {
            term.recv().await;
        }
        None => std::future::pending().await,
    }
}

async fn requested(request: Option<Pin<Box<dyn Future<Output = String> + Send>>>) -> String {
    match request {
        Some(request) => request.await,
        None => std::future::pending().await,
    }
}

async fn rpc(worker: &mut impl Session, store: &Arc<Mutex<Store>>, prompt: &str) -> Result<()> {
    store.lock().await.record(
        "agent_prompt",
        json!({"agent_id":worker.id(),"message":prompt}),
    )?;
    worker.prompt(prompt).await
}

/// Two escalation points, counted PER AGENT. A cohort-wide round counter let a
/// fast worker (agent-07 in math-swarm-20261025-02) make 58 calls and exhaust
/// its envelope before the cohort ever reached the mandatory round, so the one
/// instruction that could have saved the run was sent only after every call was
/// already denied.
const NUDGE_FILING_AT: usize = 16;
const NUDGE_MANDATORY_AT: usize = 32;

/// The exact filing call this Task admits. Built from the Task's own
/// `output_contract`, never from a guessed shape or a fabricated record.
fn filing_contract(task: Option<&broker::Task>) -> Option<(String, String, String)> {
    let task = task?;
    let contract = task.input.get("output_contract")?;
    let key = contract.get("result_key").and_then(Value::as_str)?;
    let required = contract
        .get("required_fields")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let severities = contract
        .get("severities")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" | ");
    Some((
        task.id.clone(),
        key.to_owned(),
        format!("{required}; severity one of {severities}"),
    ))
}

/// The escalating instruction sent to a worker that has not filed. Earlier
/// rounds still allow exploration; later rounds make the tool call mandatory and
/// name the exact call, because the most common failure is a worker that keeps
/// researching and never submits.
fn nudge(round: usize, id: &str, task: Option<&broker::Task>) -> String {
    if round < NUDGE_FILING_AT {
        format!(
            "You are {id}. Keep researching. Use fetch_url to read the actual sources in your \
             assigned block: the manuscript map, the paper README, and the LaTeX sections under \
             build/sections/. Read several papers before you decide. Post useful intermediates to \
             the Board. Do not file yet unless you have already covered your block and have at \
             least three well-evidenced records. Keep going."
        )
    } else if round < NUDGE_MANDATORY_AT {
        format!(
            "You are {id}. You have enough evidence to begin assembling your records. Post your \
             top candidates to the Board now, then prepare one submit_finding call. Filing becomes \
             mandatory in a few turns; do not open a new line of inquiry."
        )
    } else {
        match filing_contract(task) {
            Some((task_id, key, fields)) => format!(
                "You are {id}. Filing is now the ONLY remaining action. Call submit_finding exactly \
                 once, now:\n\
                 submit_finding({{\"task_id\":\"{task_id}\",\"result\":{{\"{key}\":[]}}}})\n\
                 Replace the empty list with one object per paper you have already read in this \
                 conversation. Each object needs {fields}. Every file must be one of your seed \
                 URLs. Never invent a paper and never copy this instruction into a record. If you \
                 have no defensible record, submit the empty list: an empty finding is \
                 recoverable, a missing one blocks the whole Goal. Do not call fetch_url again. \
                 After submit_finding returns accepted, call board_post once and end the turn."
            ),
            None => format!(
                "You are {id}. Filing is now the ONLY remaining action. Call claim_task if you \
                 have not already, then call submit_finding exactly once with your claimed task_id \
                 and the result object your worker prompt defines, built from evidence already in \
                 this conversation. A filing with no records is better than no filing; never invent \
                 a record. Do not call fetch_url again. After submit_finding returns accepted, end \
                 the turn."
            ),
        }
    }
}
async fn settled(rx: &mut mpsc::Receiver<Observation>, required: &BTreeSet<String>) -> Result<()> {
    let mut done = BTreeSet::new();
    while done.len() < required.len() {
        let (id, event) = rx
            .recv()
            .await
            .context("worker observation channel closed")?;
        match event.with_context(|| format!("{id} RPC failed"))? {
            SessionEvent::Rejected(error) => {
                anyhow::bail!("{id} RPC rejected command: {error}")
            }
            SessionEvent::TurnEnded => {
                if required.contains(&id) {
                    done.insert(id);
                }
            }
        }
    }
    Ok(())
}

async fn work(
    workers: &mut [impl Session],
    store: &Arc<Mutex<Store>>,
    rx: &mut mpsc::Receiver<Observation>,
    goal: &Goal,
    idle: bool,
) -> Result<()> {
    if idle {
        std::future::pending::<()>().await;
    }
    for worker in workers.iter_mut() {
        let prompt = format!(
            "You are {} in one cooperative swarm. Goal: {}\n{}",
            worker.id(),
            goal.intent,
            goal.worker_prompt
        );
        rpc(worker, store, &prompt).await?;
    }
    let ids: BTreeSet<String> = workers.iter().map(|w| w.id().to_owned()).collect();
    // Nudge rounds recover a turn that ended without the required tool call. The
    // count is per worker and the events are consumed as they arrive: a worker
    // that has already used many turns must be steered to file even while the
    // rest of the cohort is still early, because its own budget is what runs out.
    // The bound exists only so a worker that never complies cannot livelock the
    // run; the broker's own call ceiling is the hard stop.
    let mut pending = ids.clone();
    let mut nudges: BTreeMap<String, usize> = ids.iter().map(|id| (id.clone(), 0)).collect();
    while !pending.is_empty() {
        let (who, event) = rx
            .recv()
            .await
            .context("worker observation channel closed")?;
        match event.with_context(|| format!("{who} RPC failed"))? {
            SessionEvent::Rejected(error) => {
                anyhow::bail!("{who} RPC rejected command: {error}")
            }
            SessionEvent::TurnEnded => {}
        }
        if !pending.contains(&who) {
            continue;
        }
        let (filed, task, terminal) = {
            let store = store.lock().await;
            (
                store.findings.iter().any(|finding| finding.agent_id == who),
                store
                    .tasks
                    .iter()
                    .find(|task| task.claimed_by.as_deref() == Some(who.as_str()))
                    .cloned(),
                store
                    .agents
                    .get(&who)
                    .and_then(|agent| agent.terminal.clone()),
            )
        };
        if filed {
            pending.remove(&who);
            continue;
        }
        // A denied provider call cannot be fixed by another prompt: the model
        // turn that would read the prompt is itself refused. File the contract's
        // defer row from the control plane and retire this worker instead of
        // spending the next two hundred rounds on identical denials.
        if let Some(reason) = terminal {
            let mut store = store.lock().await;
            store.defer_task(goal, &who, &reason)?;
            pending.remove(&who);
            continue;
        }
        let count = nudges
            .get_mut(&who)
            .context("settled worker was not admitted")?;
        if *count >= 200 {
            // Stop steering a worker that never complies; the finding-count
            // check below turns this into a clean, reported failure.
            pending.remove(&who);
            continue;
        }
        *count += 1;
        let prompt = nudge(*count, &who, task.as_ref());
        let worker = workers
            .iter_mut()
            .find(|worker| worker.id() == who)
            .context("settled worker has no process")?;
        rpc(worker, store, &prompt).await?;
    }
    // An agent that never filed (nudges exhausted) still has a claimed Task:
    // file its partial summary from the host so synthesis can proceed.
    store
        .lock()
        .await
        .file_partial_findings(goal, "agent ended without filing")?;
    ensure!(
        store.lock().await.findings.len() == goal.agent_count,
        "not all agents submitted a Finding"
    );
    rpc(&mut workers[0], store, &goal.synthesis_prompt).await?;
    pending = BTreeSet::from(["agent-00".to_owned()]);
    for _ in 0..12 {
        settled(rx, &pending).await?;
        if store.lock().await.artifact.is_some() {
            pending.clear();
            break;
        }
        let prompt = "Continue synthesis. You have NOT yet submitted the artifact. Call \
                      read_findings, then submit_artifact exactly as the Goal instructs. Do \
                      not end this turn without the submit_artifact tool call."
            .to_owned();
        rpc(&mut workers[0], store, &prompt).await?;
        pending = BTreeSet::from(["agent-00".to_owned()]);
    }
    if !pending.is_empty() {
        settled(rx, &pending).await?;
    }
    // The coordinator failed to submit (it never called read_findings or
    // submit_artifact in several runs). The artifact is mechanically the exact
    // union of the findings, so build it host-side under the same guard.
    store
        .lock()
        .await
        .synthesize_artifact("coordinator did not submit the artifact")?;
    ensure!(
        store.lock().await.artifact.is_some(),
        "no artifact could be synthesized"
    );
    Ok(())
}

/// The model revision recorded in the manifest and evidence. An API alias has
/// no weights digest to publish, so the admitted provider and model name it.
fn model_revision(upstream: &Upstream) -> String {
    format!(
        "{}/{} (API alias, weights unavailable)",
        upstream.provider, upstream.model
    )
}

/// Stop every session, then join their readers, and count what did not end
/// cleanly: a session still alive after its wait, or a reader that failed or
/// timed out.
async fn stop_sessions(mut workers: Vec<impl Session>, store: &Arc<Mutex<Store>>) -> Result<usize> {
    let mut remaining = 0;
    for worker in &mut workers {
        if !worker.stop().await? {
            remaining += 1;
        }
        let mut state = store.lock().await;
        if let Some(agent) = state.agents.get_mut(worker.id()) {
            agent.stopped_ms = Some(now_ms());
            agent.status = "stopped".into();
        }
        state.record("agent_stopped", json!({"agent_id":worker.id()}))?;
    }
    for worker in workers {
        remaining += worker.join().await;
    }
    Ok(remaining)
}

/// `sockaddr_un.sun_path` is 108 bytes and must also store a trailing NUL.
const SUN_PATH_LIMIT: usize = 108;

struct BrokerSocket {
    path: PathBuf,
    private_dir: Option<PathBuf>,
}

impl BrokerSocket {
    fn cleanup(&self) {
        let _ = fs::remove_file(&self.path);
        if let Some(dir) = &self.private_dir {
            let _ = fs::remove_dir(dir);
        }
    }
}

impl Drop for BrokerSocket {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn sun_path_fits(path: &Path) -> bool {
    path.as_os_str().as_encoded_bytes().len() < SUN_PATH_LIMIT
}

fn prepare_broker_socket(run_dir: &Path) -> Result<BrokerSocket> {
    let path = run_dir.join("broker.sock");
    if sun_path_fits(&path) {
        return Ok(BrokerSocket {
            path,
            private_dir: None,
        });
    }
    let private_dir = std::env::temp_dir().join(format!("ss-{}", uuid::Uuid::new_v4().as_simple()));
    let path = private_dir.join("socket");
    ensure!(sun_path_fits(&path), "broker socket path exceeds SUN_LEN");
    fs::DirBuilder::new().mode(0o700).create(&private_dir)?;
    Ok(BrokerSocket {
        path,
        private_dir: Some(private_dir),
    })
}

/// The Goal deadline counts only time in which model calls are allowed: a pause (operator file or peak window) does not
/// consume it. `RuntimeMaxSec` on the service remains the wall-clock backstop.
async fn unpaused_deadline(deadline: u64, run_dir: PathBuf) {
    let tick = Duration::from_secs(5);
    let mut left = Duration::from_secs(deadline);
    while !left.is_zero() {
        let step = left.min(tick);
        tokio::time::sleep(step).await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if broker::effective_control(&run_dir, now).is_none() {
            left -= step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener; // std, not the tokio listener the daemon binds

    #[tokio::test]
    async fn a_pause_stops_the_deadline_clock_and_resuming_restarts_it() {
        let dir = std::env::temp_dir().join(format!("swarm-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("PAUSE"), "").unwrap();
        let paused =
            tokio::time::timeout(Duration::from_secs(3), unpaused_deadline(1, dir.clone())).await;
        assert!(paused.is_err(), "deadline must not expire while paused");
        std::fs::remove_file(dir.join("PAUSE")).unwrap();
        let resumed =
            tokio::time::timeout(Duration::from_secs(4), unpaused_deadline(1, dir.clone())).await;
        assert!(resumed.is_ok(), "deadline runs once the pause is lifted");
    }

    fn task() -> broker::Task {
        serde_json::from_value(json!({
            "id": "lens-04",
            "input": {"output_contract": {
                "result_key": "papers",
                "required_fields": ["id", "title", "file", "description", "severity"],
                "severities": ["breakthrough", "useful", "background"],
            }},
            "claimed_by": "agent-07",
            "status": "claimed",
        }))
        .unwrap()
    }

    #[test]
    fn the_mandatory_nudge_names_the_goal_contract_call_without_inventing_rows() {
        let task = task();
        let text = nudge(NUDGE_MANDATORY_AT, "agent-07", Some(&task));
        assert!(
            text.contains(r#"submit_finding({"task_id":"lens-04","result":{"papers":[]}})"#),
            "the exact call must be literal: {text}"
        );
        assert!(text.contains("id, title, file, description, severity"));
        assert!(text.contains("breakthrough | useful | background"));
        assert!(text.contains("Never invent a paper"));
        assert!(text.contains("Do not call fetch_url again"));
        // The skeleton never carries a made-up record.
        assert!(!text.contains("family-"));
    }

    #[test]
    fn early_nudges_still_allow_research_and_an_unclaimed_worker_gets_no_fake_task() {
        let task = task();
        assert!(nudge(1, "agent-07", Some(&task)).contains("Keep researching"));
        assert!(!nudge(1, "agent-07", Some(&task)).contains("submit_finding({"));
        let unclaimed = nudge(NUDGE_MANDATORY_AT, "agent-07", None);
        assert!(unclaimed.contains("submit_finding exactly once"));
        assert!(!unclaimed.contains("lens-04"));
    }

    // Broker socket placement (main #5).
    #[test]
    fn broker_socket_binds_inside_private_run_dir() {
        let run_dir = std::env::temp_dir().join(format!("ss-run-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&run_dir).unwrap();
        fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let candidate = run_dir.join("broker.sock");
        let socket = prepare_broker_socket(&run_dir).unwrap();
        assert!(sun_path_fits(&socket.path));
        if sun_path_fits(&candidate) {
            assert!(socket.private_dir.is_none());
            assert_eq!(socket.path, candidate);
        } else {
            let private_dir = socket.private_dir.clone().unwrap();
            assert_eq!(socket.path, private_dir.join("socket"));
            assert_eq!(
                fs::metadata(&private_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let listener = UnixListener::bind(&socket.path).unwrap();
        drop(listener);
        let private_dir = socket.private_dir.clone();
        socket.cleanup();
        assert!(!socket.path.exists());
        if let Some(dir) = private_dir {
            assert!(!dir.exists());
        }
        fs::remove_dir_all(run_dir).unwrap();
    }

    #[test]
    fn long_run_dir_creates_mode_700_dir_before_bind() {
        let parent =
            std::env::temp_dir().join(format!("ss-long-{}", uuid::Uuid::new_v4().as_simple()));
        let run_dir = parent.join("r".repeat(80));
        fs::create_dir_all(&run_dir).unwrap();
        let candidate = run_dir.join("broker.sock");
        assert!(
            !sun_path_fits(&candidate),
            "fixture path must exceed SUN_LEN"
        );
        let socket = prepare_broker_socket(&run_dir).unwrap();
        let private_dir = socket.private_dir.clone().expect("fallback directory");
        assert_eq!(socket.path, private_dir.join("socket"));
        assert!(sun_path_fits(&socket.path));
        assert_eq!(
            fs::metadata(&private_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let listener = UnixListener::bind(&socket.path).unwrap();
        drop(listener);
        socket.cleanup();
        assert!(!socket.path.exists());
        assert!(!private_dir.exists());
        fs::remove_dir_all(parent).unwrap();
    }
}
