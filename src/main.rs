mod mvp;
mod smoke;

use solos_swarm::{
    broker,
    code::CodeExecutor,
    harness::{
        AgentHarness, Observation, PiHarness, Role, Session, SessionEvent, SessionSpec,
        bounded_line,
    },
};

use anyhow::{Context, Result, ensure};
use broker::{
    Broker, Goal, OpenAiCompatible, Store, Upstream, file_digest, now_ms, router, write_json,
};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
    sync::{Mutex, Semaphore, mpsc},
};
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(version, about = "Governed Rust/Pi swarm runtime")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    #[command(hide = true)]
    WasiWorker {
        #[arg(long)]
        policy: PathBuf,
    },
    SandboxEnter {
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        node: PathBuf,
        #[arg(last = true)]
        args: Vec<String>,
    },
    Mvp {
        #[command(subcommand)]
        command: mvp::MvpCommand,
    },
    Run {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        run_dir: PathBuf,
        #[arg(long)]
        pi_root: PathBuf,
        #[arg(long)]
        node: PathBuf,
        #[arg(long, value_parser = ["deepseek", "venice"])]
        provider: Option<String>,
        #[arg(long)]
        credential_file: Option<PathBuf>,
        #[arg(long)]
        deadline_seconds: Option<u64>,
        /// Lifecycle test only. Never qualifies as live inference.
        #[arg(long)]
        idle_probe: bool,
    },
    /// Read-only: verify a stopped run's state against its event chain and print
    /// what a resume would settle and respawn. Changes nothing.
    ResumePlan {
        #[arg(long)]
        run_dir: PathBuf,
    },
    /// Zero-spend smoke of this binary's broker, ledger, pause gate, drain and
    /// resume against a scripted provider. Needs no key, network or sandbox.
    Smoke {
        #[arg(long)]
        dir: PathBuf,
    },
    Doctor {
        #[arg(long)]
        pi_root: PathBuf,
        #[arg(long)]
        node: PathBuf,
    },
    /// Probe the sandboxed worker path without spending a token: launch one Pi
    /// worker inside bubblewrap and have it call `fetch_url`.
    FetchProbe {
        #[arg(long)]
        pi_root: PathBuf,
        #[arg(long)]
        node: PathBuf,
        #[arg(long)]
        url: String,
    },
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

/// The admitted endpoint of each shipped provider profile. A profile's dialect
/// is fixed here rather than read from the Goal, so a Goal cannot make the
/// transport speak a dialect its provider does not.
fn endpoint(provider: &str) -> Result<&'static str> {
    match provider {
        "deepseek" => Ok("https://api.deepseek.com/chat/completions"),
        "venice" => Ok("https://api.venice.ai/api/v1/chat/completions"),
        other => anyhow::bail!("provider outside the deployment allowlist: {other}"),
    }
}
/// The model revision recorded in the manifest and evidence. An API alias has
/// no weights digest to publish, so the admitted provider and model name it.
fn model_revision(upstream: &Upstream) -> String {
    format!(
        "{}/{} (API alias, weights unavailable)",
        upstream.provider, upstream.model
    )
}
/// Build the transport for a resolved upstream, so both shipped profiles share
/// one receipt, bound and error path.
fn provider_for(
    upstream: &Upstream,
    client: reqwest::Client,
    key: Zeroizing<String>,
) -> OpenAiCompatible {
    OpenAiCompatible {
        endpoint: endpoint(&upstream.provider).expect("resolved upstream endpoint"),
        dialect: upstream.dialect,
        client,
        key,
    }
}

/// One legacy preview run: a frozen Goal, the pinned Pi tree and one credential.
struct RunOptions {
    config: PathBuf,
    run_dir: PathBuf,
    root: PathBuf,
    node: PathBuf,
    provider: Option<String>,
    credential_file: Option<PathBuf>,
    deadline: Option<u64>,
    idle: bool,
}

async fn run(options: RunOptions) -> Result<()> {
    let RunOptions {
        config,
        run_dir,
        root,
        node,
        provider,
        credential_file,
        deadline,
        idle,
    } = options;
    // Consume the one-use credential before any launch or config error can retain it.
    let key = if let Some(path) = credential_file {
        let result = fs::read_to_string(&path);
        let _ = fs::remove_file(&path);
        let raw = Zeroizing::new(result.context("cannot read broker credential")?);
        Zeroizing::new(raw.trim().to_owned())
    } else {
        Zeroizing::new(String::new())
    };
    let mut goal: Goal =
        serde_yaml::from_slice(&fs::read(&config)?).context("invalid Goal YAML")?;
    // A Goal that declares an upstream is complete on its own; the flag lets an
    // operator select a shipped profile for a Goal that does not, and restating
    // a declared upstream identically is accepted.
    let resolved = match (&goal.upstream, &provider) {
        (Some(upstream), Some(supplied)) => {
            ensure!(
                &upstream.provider == supplied,
                "Goal upstream and --provider disagree"
            );
            upstream.clone()
        }
        (Some(upstream), None) => upstream.clone(),
        (None, Some(supplied)) => broker::upstream_profile(supplied, &goal.model)?,
        (None, None) => broker::Upstream::deepseek(&goal.model),
    };
    goal.upstream = Some(resolved.clone());
    goal.validate()?;
    ensure!(
        std::env::consts::OS == "linux",
        "agent isolation requires Linux"
    );
    let deadline = deadline.unwrap_or(goal.deadline_seconds);
    ensure!(
        (1..=goal.deadline_seconds).contains(&deadline),
        "deadline cannot widen the Goal"
    );
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
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let broker = Broker {
        goal: goal.clone(),
        store: store.clone(),
        provider: Arc::new(provider_for(&upstream, client.clone(), key)),
        slots: Arc::new(Semaphore::new(goal.agent_count)),
        client,
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
        match PiHarness.start(&spec, tx.clone()).await {
            Ok(worker) => workers.push(worker),
            Err(error) => {
                startup_error = Some(error);
                break;
            }
        }
    }
    drop(tx);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = if let Some(error) = startup_error {
        Err(error)
    } else {
        tokio::select! {
            result=work(&mut workers,&store,&mut rx,&goal,idle) => result,
            _=unpaused_deadline(deadline, run_dir.clone()) => Err(anyhow::anyhow!("host-side Goal deadline expired")),
            _=broker::control_watch(store.clone(),run_dir.clone()) => Err(anyhow::Error::new(broker::Drained)),
            _=term.recv() => Err(anyhow::anyhow!("operator cancelled run")),
            _=tokio::signal::ctrl_c() => Err(anyhow::anyhow!("operator interrupted run")),
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
    println!(
        "{}",
        json!({"runtime_verdict":if runtime_pass {"PASS"}else{"FAIL"},"run_dir":run_dir,"cleanup_verdict":if remaining==0 {"PASS"}else{"INCONCLUSIVE"}})
    );
    if drained { Ok(()) } else { result }
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

fn main() {
    // Guards enter their policy before creating any runtime threads.
    let result = match Cli::parse().command {
        Action::WasiWorker { policy } => solos_swarm::wasi_worker::worker_entry(&policy),
        Action::SandboxEnter { policy, node, args } => (|| -> Result<()> {
            let policy = serde_json::from_slice(&fs::read(policy)?)?;
            solos_swarm::sandbox::enter_process(&policy, &node, &args)
        })(),
        command => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("runtime startup failed")
            .and_then(|runtime| runtime.block_on(execute(command))),
    };
    if let Err(error) = result {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
async fn execute(command: Action) -> Result<()> {
    match command {
        Action::WasiWorker { .. } | Action::SandboxEnter { .. } => {
            anyhow::bail!("guard must enter before async runtime")
        }
        Action::Mvp { command } => mvp::execute(command).await,
        Action::Run {
            config,
            run_dir,
            pi_root,
            node,
            provider,
            credential_file,
            deadline_seconds,
            idle_probe,
        } => {
            run(RunOptions {
                config,
                run_dir,
                root: pi_root,
                node,
                provider,
                credential_file,
                deadline: deadline_seconds,
                idle: idle_probe,
            })
            .await
        }
        Action::Smoke { dir } => {
            println!("{}", smoke::run(&dir).await?);
            Ok(())
        }
        Action::ResumePlan { run_dir } => {
            println!("{}", json!(broker::resume_plan(&run_dir)?));
            Ok(())
        }
        Action::Doctor { pi_root, node } => doctor(&pi_root, &node).await,
        Action::FetchProbe { pi_root, node, url } => fetch_probe(&pi_root, &node, &url).await,
    }
}
async fn doctor(root: &Path, node: &Path) -> Result<()> {
    ensure!(std::env::consts::OS == "linux", "Linux host required");
    ensure!(
        root.join("extensions/broker.ts").is_file(),
        "broker extension missing"
    );
    ensure!(
        root.join("node_modules/@earendil-works/pi-coding-agent/dist/cli.js")
            .is_file(),
        "pinned Pi not installed"
    );
    let version = Command::new(node).arg("--version").output().await?;
    ensure!(version.status.success(), "Node executable failed");
    let result = Command::new("bwrap")
        .args([
            "--unshare-all",
            "--die-with-parent",
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind-try",
            "/lib",
            "/lib",
            "--ro-bind-try",
            "/lib64",
            "/lib64",
            "--ro-bind-try",
            "/etc",
            "/etc",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "/usr/bin/true",
        ])
        .status()
        .await?;
    ensure!(result.success(), "bubblewrap isolation unavailable");
    println!(
        "{}",
        json!({"verdict":"PASS","node":String::from_utf8_lossy(&version.stdout).trim(),"isolation":"bubblewrap-unshare-all"})
    );
    Ok(())
}

/// Probe the sandboxed worker path end to end without spending a token.
///
/// This exists because on a host without the pinned Linux layout a failure in
/// the worker sandbox is indistinguishable from a failure in a worker's
/// reasoning, and the second kind costs money to rediscover. The broker here is a
/// throwaway one-serving fetch endpoint, so the probe answers exactly one
/// question: can a clearenv worker inside bubblewrap reach the network and call
/// an HTTP broker over its Unix socket.
async fn fetch_probe(root: &Path, node: &Path, url: &str) -> Result<()> {
    use axum::{Json, Router, extract::State, routing::post};
    let root = root.canonicalize()?;
    let node = node.canonicalize()?;
    ensure!(
        root.join("extensions/broker.ts").is_file(),
        "broker extension missing"
    );
    let work = std::env::temp_dir().join(format!("swarm-probe-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&work)?;
    let socket = work.join("broker.sock");
    let settings = work.join("settings.json");
    write_json(
        &settings,
        &json!({"retry":{"enabled":false},"compaction":{"enabled":false},"quietStartup":true}),
    )?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let target = url.to_owned();
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let server = tokio::spawn(async move {
        let app = Router::new()
            .route(
                "/model",
                post(
                    |Json(body): Json<Value>| async move {
                        if body["messages"]
                            .as_array()
                            .is_some_and(|m| m.iter().any(|m| {
                                m["content"]
                                    .as_str()
                                    .is_some_and(|c| c.contains("<fetch_result>"))
                            }))
                        {
                            Json(json!({"id":"probe-2","created":1,"model":"probe-m1",
                                "choices":[{"index":0,"message":{"role":"assistant","content":"probe done"},"finish_reason":"stop"}],
                                "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}))
                        } else {
                            Json(json!({"id":"probe-1","created":1,"model":"probe-m1",
                                "choices":[{"index":0,"message":{"role":"assistant","content":null,
                                    "tool_calls":[{"id":"call-fetch","type":"function",
                                        "function":{"name":"fetch_url","arguments":serde_json::to_string(&json!({"url":target})).unwrap()}}]},
                                    "finish_reason":"tool_calls"}],
                                "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}))
                        }
                    },
                ),
            )
            .route(
                "/session",
                post(|| async {
                    // The probe serves the same envelope a real broker would, so the
                    // extension registers the model and the tool from the session
                    // rather than from its own hard-coded list.
                    Json(json!({
                        "workload": {"trust_domain":"probe.local","tenant":"probe","goal":"probe",
                                     "node":"probe-1","agent":"agent-00","incarnation":"1"},
                        "model": {"id":"probe-m1","name":"Probe","reasoning":false,"input":["text"],
                                  "contextWindow":131072,"maxTokens":8192,
                                  "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
                                  "compat":{"supportsStore":false,"supportsDeveloperRole":false,
                                            "maxTokensField":"max_tokens","supportsStrictMode":false}},
                        "tools": [{"name":"fetch_url",
                                   "description":"Fetch one live URL.",
                                   "parameters":{"type":"object","required":["url"],
                                                 "properties":{"url":{"type":"string"}},
                                                 "additionalProperties":false}}]
                    }))
                }),
            )
            .route(
                "/tool/fetch_url",
                post(
                    |State(client): State<reqwest::Client>, Json(args): Json<Value>| async move {
                        let url = args["url"].as_str().unwrap_or_default().to_owned();
                        match solos_swarm::broker::fetch(&client, &json!({"url":url})).await {
                            Ok(value) => Json(json!({"content":[{"type":"text","text":value.to_string()}]})),
                            Err(error) => Json(json!({"content":[{"type":"text","text":format!("error: {error}")}]})),
                        }
                    },
                ),
            )
            .with_state(client);
        axum::serve(listener, app).await
    });
    let mut child = PiHarness::command(&SessionSpec {
        id: "agent-00".into(),
        role: Role::Coordinator,
        grants: [
            "claim_task",
            "board_read",
            "board_post",
            "submit_finding",
            "read_findings",
            "submit_artifact",
            "fetch_url",
        ]
        .map(String::from)
        .to_vec(),
        token: "probe-token".into(),
        model: "probe-m1".into(),
        run_dir: work.clone(),
        broker_socket: socket,
        pi_settings: settings,
        node,
        pi_root: root,
    })
    .spawn()?;
    let mut stdin = child.stdin.take().context("missing Pi stdin")?;
    let stdout = child.stdout.take().context("missing Pi stdout")?;
    let mut stderr = child.stderr.take().context("missing Pi stderr")?;
    let stderr_task = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });
    let mut reader = BufReader::new(stdout);
    stdin
        .write_all(
            format!(
                "{}\n",
                json!({"id":"probe","type":"prompt","message":"Fetch the target URL with fetch_url."})
            )
            .as_bytes(),
        )
        .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut observed = None;
    while tokio::time::Instant::now() < deadline {
        let line = match tokio::time::timeout(
            Duration::from_secs(5),
            bounded_line(&mut reader, 4 * 1024 * 1024),
        )
        .await
        {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break,
            _ => continue,
        };
        let record: Value = match serde_json::from_slice(&line) {
            Ok(record) => record,
            Err(_) => continue,
        };
        if record["type"] == "extension_error" {
            anyhow::bail!("fetch probe extension error: {record}");
        }
        if std::env::var("PROBE_DEBUG").is_ok()
            && matches!(
                record["type"].as_str(),
                Some("tool_execution_start" | "tool_execution_end" | "agent_end")
            )
        {
            eprintln!(
                "PROBE: {}",
                &record.to_string()[..record.to_string().len().min(400)]
            );
        }
        if record["type"] == "agent_settled" || record["type"] == "agent_end" {
            observed = Some(record);
            break;
        }
    }
    let _ = child.kill().await;
    let stderr_text = stderr_task.await.unwrap_or_default();
    let _ = fs::remove_dir_all(&work);
    server.abort();
    ensure!(
        observed.is_some(),
        "probe worker did not settle; stderr: {stderr_text}"
    );
    println!(
        "{}",
        json!({"verdict":"PASS","url":url,"worker":"bubblewrap-unshare-all"})
    );
    Ok(())
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

    /// The exact bwrap + Pi argv of one swarm agent, with `role` as the only
    /// value that differs between the coordinator and a worker.
    fn golden_pi_args(role: &str) -> Vec<&str> {
        vec![
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind-try",
            "/etc",
            "/etc",
            "--ro-bind-try",
            "/lib",
            "/lib",
            "--ro-bind-try",
            "/lib64",
            "/lib64",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/work",
            "--dir",
            "/work/config",
            "--dir",
            "/runtime",
            "--ro-bind",
            "/opt/node-v24",
            "/runtime/node",
            "--ro-bind",
            "/srv/pi/node_modules",
            "/runtime/node_modules",
            "--ro-bind",
            "/srv/pi/extensions",
            "/runtime/extensions",
            "--ro-bind",
            "/runs/r1/broker.sock",
            "/broker.sock",
            "--ro-bind",
            "/runs/r1/pi-settings.json",
            "/work/config/settings.json",
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/bin",
            "--setenv",
            "HOME",
            "/work",
            "--setenv",
            "PI_CODING_AGENT_DIR",
            "/work/config",
            "--setenv",
            "SWARM_SOCKET",
            "/broker.sock",
            "--setenv",
            "SWARM_TOKEN",
            "token-0f3a",
            "--setenv",
            "SWARM_ROLE",
            role,
            "--setenv",
            "SWARM_GRANTS",
            "claim_task,board_read,board_post,submit_finding,read_findings,submit_artifact,fetch_url",
            "--chdir",
            "/work",
            "/runtime/node/bin/node",
            "/runtime/node_modules/@earendil-works/pi-coding-agent/dist/cli.js",
            "--offline",
            "--mode",
            "rpc",
            "--provider",
            "swarm",
            "--model",
            "deepseek-flash",
            "--thinking",
            "off",
            "--no-session",
            "--no-builtin-tools",
            "--no-extensions",
            "--no-mcp",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
            "--no-approve",
            "--extension",
            "/runtime/extensions/broker.ts",
        ]
    }

    #[test]
    fn the_pi_launch_keeps_its_exact_program_argv_and_env() {
        for (id, spec_role, role) in [
            ("agent-00", Role::Coordinator, "coordinator"),
            ("agent-03", Role::Worker, "worker"),
        ] {
            let command = PiHarness::command(&SessionSpec {
                id: id.into(),
                role: spec_role,
                grants: "claim_task,board_read,board_post,submit_finding,read_findings,submit_artifact,fetch_url"
                    .split(',')
                    .map(String::from)
                    .collect(),
                token: "token-0f3a".into(),
                model: "deepseek-flash".into(),
                run_dir: PathBuf::from("/runs/r1"),
                broker_socket: PathBuf::from("/runs/r1/broker.sock"),
                pi_settings: PathBuf::from("/runs/r1/pi-settings.json"),
                node: PathBuf::from("/opt/node-v24/bin/node"),
                pi_root: PathBuf::from("/srv/pi"),
            });
            let command = command.as_std();
            assert_eq!(command.get_program(), "bwrap");
            let args: Vec<&str> = command
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect();
            assert_eq!(args, golden_pi_args(role), "{id}");
            let envs: Vec<(&str, Option<&str>)> = command
                .get_envs()
                .map(|(key, value)| (key.to_str().unwrap(), value.map(|v| v.to_str().unwrap())))
                .collect();
            assert_eq!(envs, [("PATH", Some("/usr/bin:/bin"))], "{id}");
            // `env_clear` has no stable getter; the unix Debug form prints it as `env -i`.
            assert!(format!("{command:?}").starts_with("env -i "), "{command:?}");
        }
    }

    #[tokio::test]
    async fn three_scripted_agents_complete_a_goal_through_work_and_the_real_broker() {
        use solos_swarm::harness::scripted::{Call, ScriptedHarness};
        let dir = std::env::temp_dir().join(format!(
            "ss-e2e-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        fs::create_dir(&dir).unwrap();
        let goal: Goal = serde_json::from_value(json!({
            "schema_version":1,"intent":"scripted end to end","worker_prompt":"file one record",
            "synthesis_prompt":"submit the exact union","scope":"synthetic-fixtures-only",
            "model":"deepseek-flash","agent_count":3,"deadline_seconds":60,"budget_microusd":1_000_000,
            "max_calls_per_agent":8,"max_input_bytes":65536,"max_output_tokens":2048,
            "input_price_microusd_per_million":300_000,"output_price_microusd_per_million":1_200_000,
            "grants":["claim_task","board_read","board_post","submit_finding","read_findings","submit_artifact"],
            "tasks":[{"id":"synthesis","input":{"role":"coordinator"}},{"id":"lens-1","input":{}},{"id":"lens-2","input":{}}],
            "seed":1,"catalog_version":"e2e","scorer_version":"1"
        }))
        .unwrap();
        goal.validate().unwrap();
        fs::write(dir.join("config.yaml"), serde_json::to_vec(&goal).unwrap()).unwrap();
        let mut state = Store::new(&dir, &goal).unwrap();
        state.goal_status = "running".into();
        let ids = ["agent-00", "agent-01", "agent-02"];
        let tokens: Vec<String> = ids.into_iter().map(|id| state.admit(id).unwrap()).collect();
        let store = Arc::new(Mutex::new(state));
        let socket = dir.join("b.sock");
        let app = router(Broker {
            goal: Arc::new(goal.clone()),
            store: store.clone(),
            provider: Arc::new(smoke::Scripted),
            slots: Arc::new(Semaphore::new(goal.agent_count)),
            client: reqwest::Client::new(),
            code: None,
        });
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await });

        let record = |id: &str| json!({"id":format!("{id}-paper"),"severity":"useful"});
        let scripts = ids
            .into_iter()
            .map(|id| {
                let mut turns = vec![vec![
                    Call::ClaimTask,
                    Call::SubmitFinding(json!({"papers":[record(id)]})),
                ]];
                if id == "agent-00" {
                    turns.push(vec![Call::ReadFindings, Call::SubmitArtifact]);
                }
                (id.to_string(), turns)
            })
            .collect();
        let harness = ScriptedHarness::new(scripts);
        let (tx, mut rx) = mpsc::channel(256);
        let mut sessions = Vec::new();
        for (n, (id, token)) in ids.into_iter().zip(tokens).enumerate() {
            let spec = SessionSpec {
                id: id.to_string(),
                role: if n == 0 {
                    Role::Coordinator
                } else {
                    Role::Worker
                },
                grants: goal.grants.clone(),
                token,
                model: goal.model.clone(),
                run_dir: dir.clone(),
                broker_socket: socket.clone(),
                pi_settings: dir.join("pi-settings.json"),
                node: PathBuf::new(),
                pi_root: PathBuf::new(),
            };
            sessions.push(harness.start(&spec, tx.clone()).await.unwrap());
        }
        drop(tx);
        tokio::time::timeout(
            Duration::from_secs(30),
            work(&mut sessions, &store, &mut rx, &goal, false),
        )
        .await
        .expect("work() did not finish")
        .unwrap();
        assert_eq!(stop_sessions(sessions, &store).await.unwrap(), 0);
        server.abort();

        let state = store.lock().await;
        let filed: BTreeMap<&str, &Value> = state
            .findings
            .iter()
            .map(|finding| (finding.agent_id.as_str(), &finding.result))
            .collect();
        assert_eq!(filed.len(), 3);
        for id in ids {
            assert_eq!(filed[id], &json!({"papers":[record(id)]}), "{id}");
        }
        let artifact: Value =
            serde_json::from_slice(&fs::read(dir.join("artifacts/final.json")).unwrap()).unwrap();
        assert_eq!(artifact.as_object().unwrap().len(), 1);
        let mut have: Vec<String> = artifact["papers"]
            .as_array()
            .unwrap()
            .iter()
            .map(Value::to_string)
            .collect();
        let mut want: Vec<String> = ids.into_iter().map(|id| record(id).to_string()).collect();
        have.sort();
        want.sort();
        assert_eq!(have, want);
        assert_eq!(state.artifact.as_ref(), Some(&artifact));
        assert_eq!(state.held_microusd, 0);
        assert_eq!(state.intents.len(), 4, "one model call per turn");
        assert!(
            state
                .intents
                .iter()
                .all(|intent| intent.state == "completed")
        );
        let settled: u64 = state
            .intents
            .iter()
            .map(|intent| intent.cost_microusd)
            .sum();
        assert!(settled > 0);
        assert_eq!(state.spent_microusd, settled);
        drop(state);

        // resume_plan re-verifies every event's sequence and hash, and that
        // state.json was written at the last event.
        let plan = broker::resume_plan(&dir).unwrap();
        let events: Vec<Value> = fs::read_to_string(dir.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!plan.truncated_tail);
        assert_eq!(plan.last_seq, events.len() as u64);
        assert_eq!(plan.held_microusd, 0);
        assert_eq!(plan.finished_agents, ids);

        // Sessions run concurrently, so order is checked per agent, plus the
        // one cross-agent rule: synthesis is prompted after every filing.
        let mut owner = BTreeMap::new();
        let mut timeline: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for event in &events {
            let (kind, data) = (event["kind"].as_str().unwrap(), &event["data"]);
            if kind == "provider_reserved" {
                owner.insert(
                    data["intent_id"].as_str().unwrap().to_owned(),
                    data["agent_id"].as_str().unwrap().to_owned(),
                );
            }
            let agent = data["agent_id"]
                .as_str()
                .or_else(|| {
                    let intent = data["intent_id"].as_str()?;
                    owner.get(intent).map(String::as_str)
                })
                .unwrap_or("host");
            let label = data["tool"]
                .as_str()
                .map_or(kind.to_owned(), |tool| format!("{kind}:{tool}"));
            timeline.entry(agent.to_owned()).or_default().push(label);
        }
        let turn = [
            "agent_prompt",
            "provider_reserved",
            "provider_request",
            "provider_dispatched",
            "provider_settled",
            "provider_response",
        ];
        let filing = ["tool_completed:claim_task", "tool_completed:submit_finding"];
        let synthesis = [
            "tool_completed:read_findings",
            "tool_completed:submit_artifact",
        ];
        let worker: Vec<&str> =
            [&["agent_started"][..], &turn, &filing, &["agent_stopped"]].concat();
        let coordinator: Vec<&str> = [
            &["agent_started"][..],
            &turn,
            &filing,
            &turn,
            &synthesis,
            &["agent_stopped"],
        ]
        .concat();
        assert_eq!(timeline.keys().collect::<Vec<_>>(), ids);
        assert_eq!(timeline["agent-00"], coordinator);
        assert_eq!(timeline["agent-01"], worker);
        assert_eq!(timeline["agent-02"], worker);
        let last_filing = events
            .iter()
            .rposition(|event| event["data"]["tool"] == "submit_finding")
            .unwrap();
        let synthesis_prompt = events
            .iter()
            .position(|event| event["data"]["message"] == "submit the exact union")
            .unwrap();
        assert!(last_filing < synthesis_prompt);
        fs::remove_dir_all(dir).unwrap();
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
