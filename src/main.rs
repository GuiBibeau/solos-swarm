mod broker;
mod mvp;

use anyhow::{Context, Result, ensure};
use broker::{Broker, DeepSeek, Goal, Store, file_digest, now_ms, router, write_json};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::{Child, ChildStdin, Command},
    sync::{Mutex, Semaphore, mpsc},
};

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
}

fn sandbox(
    node: &Path,
    root: &Path,
    socket: &Path,
    settings: &Path,
    token: &str,
    id: &str,
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
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib64",
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
        .arg("--ro-bind")
        .arg(node)
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
        .args([
            "--chdir",
            "/work",
            "/runtime/node",
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
    let ids = workers.iter().map(|w| w.id.clone()).collect();
    settled(rx, &ids).await?;
    ensure!(
        store.lock().await.findings.len() == goal.agent_count,
        "not all agents submitted a Finding"
    );
    rpc(&mut workers[0], store, &goal.synthesis_prompt).await?;
    settled(rx, &["agent-00".to_owned()].into_iter().collect()).await?;
    ensure!(
        store.lock().await.artifact.is_some(),
        "coordinator did not submit the artifact"
    );
    Ok(())
}

async fn run(
    config: PathBuf,
    run_dir: PathBuf,
    root: PathBuf,
    node: PathBuf,
    credential_file: Option<PathBuf>,
    deadline: Option<u64>,
    idle: bool,
) -> Result<()> {
    // Consume the one-use credential before any launch or config error can retain it.
    let key = if let Some(path) = credential_file {
        let result = fs::read_to_string(&path);
        let _ = fs::remove_file(&path);
        result
            .context("cannot read broker credential")?
            .trim()
            .to_owned()
    } else {
        String::new()
    };
    let goal: Goal = serde_yaml::from_slice(&fs::read(&config)?).context("invalid Goal YAML")?;
    goal.validate()?;
    ensure!(
        idle || !key.is_empty(),
        "DeepSeek broker credential required"
    );
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
    let manifest = json!({"schema_version":1,"live":!idle,"provider":"deepseek","model":goal.model,"model_revision":"DeepSeek-V4.1-Flash (API alias, weights unavailable)","pi_version":"1.1.0","topology":"single-host","isolation":"bubblewrap-unshare-all","runtime_sha256":file_digest(&executable)?,"extension_sha256":file_digest(&root.join("extensions/broker.ts"))?,"package_lock_sha256":file_digest(&root.join("package-lock.json"))?,"artifacts":{"config.yaml":file_digest(&config_path)?},"seed":goal.seed,"catalog_version":goal.catalog_version,"scorer_version":goal.scorer_version,"started_ms":now_ms(),"price_table":{"revision":"deepseek-2026-10-12-conservative-peak","input_microusd_per_million":goal.input_price_microusd_per_million,"output_microusd_per_million":goal.output_price_microusd_per_million},"deadline_seconds":deadline,"deferred":["Raft/QUIC and multi-host HA","500-agent acceptance","SVID and signed audit witness","Firecracker tools/full Tier P","local provider and air gap","policy actors/module unloading/retractions","history optimization and three-family rotation"]});
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
    let goal = Arc::new(goal);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let broker = Broker {
        goal: goal.clone(),
        store: store.clone(),
        provider: Arc::new(DeepSeek { client, key }),
        slots: Arc::new(Semaphore::new(4)),
    };
    let socket = std::env::temp_dir().join(format!("swarm-{}.sock", uuid::Uuid::new_v4()));
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let server = tokio::spawn(async move { axum::serve(listener, router(broker)).await });
    let settings = run_dir.join("pi-settings.json");
    write_json(
        &settings,
        &json!({"retry":{"enabled":false},"compaction":{"enabled":false},"quietStartup":true}),
    )?;
    let (tx, mut rx) = mpsc::channel(256);
    let mut workers = Vec::new();
    let mut readers = Vec::new();
    let mut startup_error = None;
    for n in 0..goal.agent_count {
        let id = format!("agent-{n:02}");
        let token = store.lock().await.admit(&id)?;
        let mut child = match sandbox(&node, &root, &socket, &settings, &token, &id).spawn() {
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
                while let Some(line) = bounded_line(&mut reader, 262_144).await? {
                    file.write_all(&line).await?;
                    let value: Value = serde_json::from_slice(&line).context("invalid Pi JSONL")?;
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
    let _ = fs::remove_file(&socket);
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
            credential_file,
            deadline_seconds,
            idle_probe,
        } => {
            run(
                config,
                run_dir,
                pi_root,
                node,
                credential_file,
                deadline_seconds,
                idle_probe,
            )
            .await
        }
        Action::Doctor { pi_root, node } => doctor(&pi_root, &node).await,
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
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib64",
            "/lib64",
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
