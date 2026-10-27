mod mvp;

use solos_swarm::{broker, code::CodeExecutor};

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
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::{Child, ChildStdin, Command},
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

// Eight arguments are deliberate: this is the single documented worker
// isolation boundary, and every one is a distinct mount or identity input.
#[allow(clippy::too_many_arguments)]
fn sandbox(
    node: &Path,
    root: &Path,
    socket: &Path,
    settings: &Path,
    grants: &str,
    token: &str,
    id: &str,
    model: &str,
) -> Command {
    let mut cmd = Command::new("bwrap");
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .args([
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--ro-bind",
            "/usr",
            "/usr",
            // Name resolution and CA material are a read-only binding rather than
            // a borrowing from the host environment, so a clearenv worker still
            // resolves and verifies a live target.
            "--ro-bind-try",
            "/etc",
            "/etc",
            // The loader directories are bound rather than symlinked: on a usr-merged
            // host /lib is itself a symlink, so replacing it with one would hide the
            // real libraries from a dynamically linked node and fail as ENOENT.
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
        ])
        // `node` is the binary path; its install root is what gets bound, so the
        // worker can exec an absolute binary path and still resolve node_modules
        // beside it under the same /runtime/node prefix.
        .arg("--ro-bind")
        .arg(node.parent().and_then(Path::parent).unwrap_or(node))
        .arg("/runtime/node")
        .arg("--ro-bind")
        .arg(root.join("node_modules"))
        .arg("/runtime/node_modules")
        .arg("--ro-bind")
        .arg(root.join("extensions"))
        .arg("/runtime/extensions")
        .arg("--ro-bind")
        .arg(socket)
        .arg("/broker.sock")
        .arg("--ro-bind")
        .arg(settings)
        .arg("/work/config/settings.json")
        .args([
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
        ])
        .arg(token)
        .args(["--setenv", "SWARM_ROLE"])
        .arg(if id == "agent-00" {
            "coordinator"
        } else {
            "worker"
        })
        .args(["--setenv", "SWARM_GRANTS"])
        .arg(grants)
        .args([
            "--chdir",
            "/work",
            // The explicit binary path, not the directory: a directory exec needs
            // the kernel to resolve the name through PATH inside the mount
            // namespace, which fails EACCES on some host/container kernels.
            "/runtime/node/bin/node",
            "/runtime/node_modules/@earendil-works/pi-coding-agent/dist/cli.js",
            "--offline",
            "--mode",
            "rpc",
            "--provider",
            "swarm",
            "--model",
        ])
        .arg(model)
        .args([
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
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

async fn bounded_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    limit: usize,
) -> Result<Option<Vec<u8>>> {
    let mut result = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(if result.is_empty() {
                None
            } else {
                Some(result)
            });
        }
        let n = buf
            .iter()
            .position(|c| *c == b'\n')
            .map_or(buf.len(), |p| p + 1);
        ensure!(
            result.len() + n <= limit,
            "Pi protocol line exceeds byte limit"
        );
        let finished = buf[n - 1] == b'\n';
        result.extend_from_slice(&buf[..n]);
        reader.consume(n);
        if finished {
            return Ok(Some(result));
        }
    }
}

struct Worker {
    id: String,
    child: Child,
    stdin: ChildStdin,
}
async fn rpc(worker: &mut Worker, store: &Arc<Mutex<Store>>, prompt: &str) -> Result<()> {
    store.lock().await.record(
        "agent_prompt",
        json!({"agent_id":worker.id,"message":prompt}),
    )?;
    let message = json!({"id":format!("prompt-{}",now_ms()),"type":"prompt","message":prompt});
    worker
        .stdin
        .write_all(&serde_json::to_vec(&message)?)
        .await?;
    worker.stdin.write_all(b"\n").await?;
    worker.stdin.flush().await?;
    Ok(())
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
async fn settled(
    rx: &mut mpsc::Receiver<(String, Result<Value>)>,
    required: &BTreeSet<String>,
) -> Result<()> {
    let mut done = BTreeSet::new();
    while done.len() < required.len() {
        let (id, event) = rx
            .recv()
            .await
            .context("worker observation channel closed")?;
        let event = event.with_context(|| format!("{id} RPC failed"))?;
        if event["type"] == "response" && event["success"] == false {
            anyhow::bail!("{id} RPC rejected command: {}", event["error"]);
        }
        if event["type"] == "agent_settled" && required.contains(&id) {
            done.insert(id);
        }
    }
    Ok(())
}

async fn work(
    workers: &mut [Worker],
    store: &Arc<Mutex<Store>>,
    rx: &mut mpsc::Receiver<(String, Result<Value>)>,
    goal: &Goal,
    idle: bool,
) -> Result<()> {
    if idle {
        std::future::pending::<()>().await;
    }
    for worker in workers.iter_mut() {
        let prompt = format!(
            "You are {} in one cooperative swarm. Goal: {}\n{}",
            worker.id, goal.intent, goal.worker_prompt
        );
        rpc(worker, store, &prompt).await?;
    }
    let ids: BTreeSet<String> = workers.iter().map(|w| w.id.clone()).collect();
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
        let event = event.with_context(|| format!("{who} RPC failed"))?;
        if event["type"] == "response" && event["success"] == false {
            anyhow::bail!("{who} RPC rejected command: {}", event["error"]);
        }
        if event["type"] != "agent_settled" || !pending.contains(&who) {
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
            .find(|worker| worker.id == who)
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
    let mut readers = Vec::new();
    let mut startup_error = None;
    for n in 0..goal.agent_count {
        let id = format!("agent-{n:02}");
        let token = store.lock().await.admit(&id)?;
        // The worker's Pi model id is the provider's wire model, because the
        // broker transcript echoes whichever id Pi asks for.
        let mut child = match sandbox(
            &node,
            &root,
            &broker_socket.path,
            &settings,
            &goal.grants.join(","),
            &token,
            &id,
            &upstream.model,
        )
        .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                startup_error = Some(anyhow::Error::new(error));
                break;
            }
        };
        let stdout = child.stdout.take().context("missing Pi stdout")?;
        let stderr = child.stderr.take().context("missing Pi stderr")?;
        let stdin = child.stdin.take().context("missing Pi stdin")?;
        let send = tx.clone();
        let who = id.clone();
        let dir = run_dir.clone();
        readers.push(tokio::spawn(async move {
            let result = async {
                let mut file =
                    tokio::fs::File::create(dir.join(format!("{who}.rpc.jsonl"))).await?;
                let mut reader = BufReader::new(stdout);
                while let Some(line) = bounded_line(&mut reader, 16_777_216).await? {
                    file.write_all(&line).await?;
                    // A line that is not JSON is tolerated rather than fatal: a
                    // crash-truncated or interleaved write must not cost the whole
                    // worker its transcript, which is also the evidence.
                    let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                        continue;
                    };
                    if value["type"] == "agent_settled" || value["type"] == "response" {
                        send.send((who.clone(), Ok(value))).await.ok();
                    }
                }
                file.sync_all().await?;
                Ok::<(), anyhow::Error>(())
            }
            .await;
            match result {
                Ok(()) => {
                    send.send((who, Err(anyhow::anyhow!("Pi process exited"))))
                        .await
                        .ok();
                    Ok(())
                }
                Err(error) => {
                    send.send((who, Err(anyhow::anyhow!(error.to_string()))))
                        .await
                        .ok();
                    Err(error)
                }
            }
        }));
        let who = id.clone();
        let dir = run_dir.clone();
        readers.push(tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut file = tokio::fs::File::create(dir.join(format!("{who}.stderr"))).await?;
            let mut total = 0;
            while let Some(line) = bounded_line(&mut reader, 65_536).await? {
                total += line.len();
                ensure!(total <= 1_048_576, "Pi stderr limit");
                file.write_all(&line).await?;
            }
            file.sync_all().await?;
            Ok::<(), anyhow::Error>(())
        }));
        workers.push(Worker { id, child, stdin });
    }
    drop(tx);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = if let Some(error) = startup_error {
        Err(error)
    } else {
        tokio::select! {
            result=work(&mut workers,&store,&mut rx,&goal,idle) => result,
            _=tokio::time::sleep(Duration::from_secs(deadline)) => Err(anyhow::anyhow!("host-side Goal deadline expired")),
            _=term.recv() => Err(anyhow::anyhow!("operator cancelled run")),
            _=tokio::signal::ctrl_c() => Err(anyhow::anyhow!("operator interrupted run")),
        }
    };
    {
        let mut state = store.lock().await;
        if let Err(error) = &result {
            // Deadline or cancel: keep what the agents did. Partial findings for
            // unfiled Tasks, then the exact-union artifact of whatever was filed.
            let reason = error.to_string();
            let _ = state.file_partial_findings(&goal, &reason);
            let _ = state.synthesize_artifact(&reason);
        }
        state.goal_status = if result.is_ok() {
            "completed"
        } else {
            "stopped"
        }
        .into();
        state.record("goal_stopped",json!({"reason":result.as_ref().err().map(|e|e.to_string()).unwrap_or_else(||"completed".into())}))?;
    }
    let mut remaining = 0;
    for worker in &mut workers {
        if worker.child.try_wait()?.is_none() {
            worker.child.start_kill()?;
        }
        if !matches!(
            tokio::time::timeout(Duration::from_secs(5), worker.child.wait()).await,
            Ok(Ok(_))
        ) {
            remaining += 1;
        }
        let mut state = store.lock().await;
        if let Some(agent) = state.agents.get_mut(&worker.id) {
            agent.stopped_ms = Some(now_ms());
            agent.status = "stopped".into();
        }
        state.record("agent_stopped", json!({"agent_id":worker.id}))?;
    }
    for mut reader in readers {
        match tokio::time::timeout(Duration::from_secs(2), &mut reader).await {
            Ok(Ok(Ok(()))) => (),
            Err(_) => {
                remaining += 1;
                reader.abort();
                let _ = reader.await;
            }
            _ => {
                remaining += 1;
            }
        }
    }
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
    let runtime_pass = result.is_ok() && store.lock().await.held_microusd == 0;
    write_json(
        &run_dir.join("runtime-report.json"),
        &json!({"runtime_verdict":if runtime_pass {"PASS"}else{"FAIL"},"reason":result.as_ref().err().map(|e|e.to_string()),"stopped_ms":now_ms(),"idle_probe":idle}),
    )?;
    println!(
        "{}",
        json!({"runtime_verdict":if runtime_pass {"PASS"}else{"FAIL"},"run_dir":run_dir,"cleanup_verdict":if remaining==0 {"PASS"}else{"INCONCLUSIVE"}})
    );
    result
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
    let mut child = sandbox(
        &node,
        &root,
        &socket,
        &settings,
        "claim_task,board_read,board_post,submit_finding,read_findings,submit_artifact,fetch_url",
        "probe-token",
        "agent-00",
        "probe-m1",
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener; // std, not the tokio listener the daemon binds

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
