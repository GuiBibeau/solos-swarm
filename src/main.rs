mod mvp;
mod smoke;

use solos_swarm::{
    broker,
    gateway::provider_for,
    harness::{PiHarness, Role, SessionSpec, bounded_line},
    swarm::{self, GoalRun, GoalStatus},
};

use anyhow::{Context, Result, ensure};
use broker::{Goal, Upstream, write_json};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
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
    /// Agent mode: a long-running agent that takes work from its Sources.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
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

#[derive(Subcommand)]
enum AgentCommand {
    /// Take Triggers from the configured Sources, judge them, run the work and
    /// reply. Serves until SIGINT or SIGTERM.
    Serve {
        /// The Policy, Sources, Channels, Router, Executor and budgets.
        #[arg(long)]
        config: PathBuf,
        /// Holds the agent's SQLite Store; created if missing.
        #[arg(long)]
        state_dir: PathBuf,
        /// Read the Sources once, finish what they started, print a summary
        /// and exit. For tests and dry runs.
        #[arg(long)]
        until_idle: bool,
        /// A Pi Router or Pi workers: the checkout that holds `node_modules`
        /// and `extensions`.
        #[arg(long)]
        pi_root: Option<PathBuf>,
        /// A Pi Router or Pi workers: the node binary.
        #[arg(long)]
        node: Option<PathBuf>,
        /// A Pi Router or Pi workers: the model key both use. Read once and
        /// deleted; it stays in this process and never enters a sandbox.
        #[arg(long)]
        credential_file: Option<PathBuf>,
    },
    /// What the agent is doing and why: open Tickets, recent Decisions,
    /// pending and failing Effects, the Router's spend today, the pause, and
    /// the alerts the Watchdog would raise now. Reads the Store and writes
    /// nothing, so it is safe while the agent serves.
    Status {
        /// The agent's state directory.
        #[arg(long)]
        state_dir: PathBuf,
        /// The agent's config, for the Router's daily cap, the Watchdog's
        /// thresholds and a pause set in it.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

/// Read a one-use credential file and delete it, whether or not the read
/// worked, so no later error can leave the key on disk.
fn consume_credential(path: &Path) -> Result<Zeroizing<String>> {
    let result = fs::read_to_string(path);
    let _ = fs::remove_file(path);
    let raw = Zeroizing::new(result.context("cannot read broker credential")?);
    Ok(Zeroizing::new(raw.trim().to_owned()))
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
        consume_credential(&path)?
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
        (None, None) => Upstream::deepseek(&goal.model),
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
    let provider = Arc::new(provider_for(&resolved, swarm::http_client()?, key)?);
    let run = GoalRun {
        goal,
        config,
        run_dir,
        deadline,
        idle_probe: idle,
        pi_root: root,
        node,
    };
    let outcome = swarm::run_goal(run, &PiHarness, provider).await?;
    println!(
        "{}",
        json!({"runtime_verdict":if outcome.runtime_pass {"PASS"}else{"FAIL"},"run_dir":outcome.run_dir,"cleanup_verdict":if outcome.remaining_workers==0 {"PASS"}else{"INCONCLUSIVE"}})
    );
    match outcome.status {
        GoalStatus::Stopped(error) => Err(error),
        GoalStatus::Completed | GoalStatus::Drained => Ok(()),
    }
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
        Action::Agent {
            command:
                AgentCommand::Serve {
                    config,
                    state_dir,
                    until_idle,
                    pi_root,
                    node,
                    credential_file,
                },
        } => {
            // Before the config is read, so no config error can leave the key on disk.
            let credential = credential_file
                .as_deref()
                .map(consume_credential)
                .transpose()?;
            let options = solos_swarm::agent::ServeOptions {
                until_idle,
                pi_root,
                node,
                credential,
            };
            let summary = solos_swarm::agent::serve(&config, &state_dir, options).await?;
            println!("{}", json!(summary));
            Ok(())
        }
        Action::Agent {
            command:
                AgentCommand::Status {
                    state_dir,
                    config,
                    json,
                },
        } => {
            use solos_swarm::agent::{AgentConfig, status};
            let config = config.as_deref().map(AgentConfig::load).transpose()?;
            let report = status::read(&state_dir, config.as_ref(), broker::now_ms())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", status::render(&report));
            }
            Ok(())
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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

    /// A fresh directory with a Goal file, an empty run dir and a stand-in Pi
    /// tree, whose extension and lock file `run_goal` digests into the manifest.
    fn scripted_run(deadline: u64, idle_probe: bool) -> (PathBuf, GoalRun) {
        let dir = std::env::temp_dir().join(format!(
            "ss-e2e-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        let pi_root = dir.join("pi");
        fs::create_dir_all(pi_root.join("extensions")).unwrap();
        fs::write(pi_root.join("extensions/broker.ts"), "// stand-in").unwrap();
        fs::write(pi_root.join("package-lock.json"), "{}").unwrap();
        fs::write(dir.join("node"), "").unwrap();
        fs::create_dir(dir.join("run")).unwrap();
        let config = dir.join("goal.yaml");
        let goal = json!({
            "schema_version":1,"intent":"scripted end to end","worker_prompt":"file one record",
            "synthesis_prompt":"submit the exact union","scope":"synthetic-fixtures-only",
            "model":"deepseek-flash","agent_count":3,"deadline_seconds":60,"budget_microusd":1_000_000,
            "max_calls_per_agent":8,"max_input_bytes":65536,"max_output_tokens":2048,
            "input_price_microusd_per_million":300_000,"output_price_microusd_per_million":1_200_000,
            "grants":["claim_task","board_read","board_post","submit_finding","read_findings","submit_artifact"],
            "tasks":[{"id":"synthesis","input":{"role":"coordinator"}},{"id":"lens-1","input":{}},{"id":"lens-2","input":{}}],
            "seed":1,"catalog_version":"e2e","scorer_version":"1"
        });
        fs::write(&config, serde_json::to_vec(&goal).unwrap()).unwrap();
        // What `run` hands over: the file's Goal, upstream resolved, validated.
        let mut goal: Goal = serde_yaml::from_slice(&fs::read(&config).unwrap()).unwrap();
        goal.upstream = Some(Upstream::deepseek(&goal.model));
        goal.validate().unwrap();
        let run = GoalRun {
            goal,
            config,
            run_dir: dir.join("run"),
            deadline,
            idle_probe,
            pi_root,
            node: dir.join("node"),
        };
        (dir, run)
    }

    #[tokio::test]
    async fn three_scripted_agents_complete_a_goal_through_run_goal_and_the_real_broker() {
        use solos_swarm::harness::scripted::{Call, ScriptedHarness};
        let (dir, run) = scripted_run(60, false);
        let ids = ["agent-00", "agent-01", "agent-02"];
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
        let outcome = tokio::time::timeout(
            Duration::from_secs(30),
            swarm::run_goal(
                run,
                &ScriptedHarness::new(scripts),
                Arc::new(smoke::Scripted),
            ),
        )
        .await
        .expect("run_goal did not finish")
        .unwrap();
        assert!(
            matches!(outcome.status, GoalStatus::Completed),
            "{:?}",
            outcome.status
        );
        let run_dir = outcome.run_dir.clone();
        assert_eq!(run_dir, dir.join("run").canonicalize().unwrap());
        assert!(outcome.runtime_pass);
        assert_eq!(outcome.remaining_workers, 0);
        assert_eq!(outcome.artifact, Some(run_dir.join("artifacts/final.json")));
        assert_eq!(outcome.findings, 3);
        let read = |name: &str| -> Value {
            serde_json::from_slice(&fs::read(run_dir.join(name)).unwrap()).unwrap()
        };

        let state = read("state.json");
        let filed: BTreeMap<&str, &Value> = state["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|finding| (finding["agent_id"].as_str().unwrap(), &finding["result"]))
            .collect();
        assert_eq!(filed.len(), 3);
        for id in ids {
            assert_eq!(filed[id], &json!({"papers":[record(id)]}), "{id}");
        }
        let artifact = read("artifacts/final.json");
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
        assert_eq!(state["artifact"], artifact);
        assert_eq!(state["held_microusd"], 0);
        let intents = state["intents"].as_array().unwrap();
        assert_eq!(intents.len(), 4, "one model call per turn");
        assert!(intents.iter().all(|intent| intent["state"] == "completed"));
        let settled: u64 = intents
            .iter()
            .map(|intent| intent["cost_microusd"].as_u64().unwrap())
            .sum();
        assert!(settled > 0);
        assert_eq!(state["spent_microusd"], settled);
        assert_eq!(outcome.spent_microusd, settled);
        assert_eq!(state["goal_status"], "completed");
        let agents = state["agents"].as_object().unwrap();
        assert_eq!(agents.keys().collect::<Vec<_>>(), ids);
        assert!(agents.values().all(|agent| agent["status"] == "stopped"));

        let config = dir.join("goal.yaml");
        assert_eq!(
            fs::read(run_dir.join("config.yaml")).unwrap(),
            fs::read(&config).unwrap()
        );
        let config_sha256 = broker::file_digest(&config).unwrap();
        let manifest = read("manifest.json");
        assert_eq!(manifest["live"], true);
        assert_eq!(manifest["provider"], "deepseek");
        assert_eq!(
            manifest["model_revision"],
            "deepseek/deepseek-flash (API alias, weights unavailable)"
        );
        assert_eq!(manifest["artifacts"]["config.yaml"], config_sha256);
        assert_eq!(
            manifest["extension_sha256"],
            broker::file_digest(&dir.join("pi/extensions/broker.ts")).unwrap()
        );
        assert_eq!(manifest["deadline_seconds"], 60);
        assert!(manifest.get("upstream_provider").is_none());
        assert_eq!(
            read("pi-settings.json"),
            json!({"retry":{"enabled":false},"compaction":{"enabled":true,"reserveTokens":65_536},"quietStartup":true})
        );
        let cleanup = read("cleanup.json");
        assert_eq!(cleanup["verdict"], "PASS");
        assert_eq!(cleanup["remaining_workers"], 0);
        let report = read("runtime-report.json");
        assert_eq!(report["runtime_verdict"], "PASS");
        assert_eq!(report["reason"], Value::Null);
        assert_eq!(report["idle_probe"], false);
        assert!(!run_dir.join("resume.json").exists());
        assert!(!run_dir.join("broker.sock").exists());

        // resume_plan re-verifies every event's sequence and hash, and that
        // state.json was written at the last event.
        let plan = broker::resume_plan(&run_dir).unwrap();
        let events: Vec<Value> = fs::read_to_string(run_dir.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!plan.truncated_tail);
        assert_eq!(plan.last_seq, events.len() as u64);
        assert_eq!(plan.held_microusd, 0);
        assert_eq!(plan.finished_agents, ids);

        // Every agent is admitted before any is prompted. Shutdown records the
        // stop, then each agent, then the cleanup, last.
        let entries: Vec<Value> = events
            .iter()
            .map(|event| json!([event["kind"], event["data"]]))
            .collect();
        assert_eq!(
            entries[..4],
            [
                json!(["goal_registered",{"intent":"scripted end to end","config_sha256":config_sha256}]),
                json!(["agent_started",{"agent_id":"agent-00"}]),
                json!(["agent_started",{"agent_id":"agent-01"}]),
                json!(["agent_started",{"agent_id":"agent-02"}]),
            ]
        );
        assert_eq!(
            entries[entries.len() - 5..],
            [
                json!(["goal_stopped",{"reason":"completed"}]),
                json!(["agent_stopped",{"agent_id":"agent-00"}]),
                json!(["agent_stopped",{"agent_id":"agent-01"}]),
                json!(["agent_stopped",{"agent_id":"agent-02"}]),
                json!(["cleanup_completed",{"remaining_workers":0}]),
            ]
        );

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
        assert_eq!(
            timeline.keys().collect::<Vec<_>>(),
            ["agent-00", "agent-01", "agent-02", "host"]
        );
        assert_eq!(timeline["agent-00"], coordinator);
        assert_eq!(timeline["agent-01"], worker);
        assert_eq!(timeline["agent-02"], worker);
        assert_eq!(
            timeline["host"],
            ["goal_registered", "goal_stopped", "cleanup_completed"]
        );
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

    #[tokio::test]
    async fn an_idle_run_stops_at_its_deadline_and_still_writes_its_files() {
        use solos_swarm::harness::scripted::ScriptedHarness;
        let (dir, run) = scripted_run(1, true);
        let outcome = tokio::time::timeout(
            Duration::from_secs(30),
            swarm::run_goal(
                run,
                &ScriptedHarness::new(BTreeMap::new()),
                Arc::new(smoke::Scripted),
            ),
        )
        .await
        .expect("run_goal did not finish")
        .unwrap();
        let GoalStatus::Stopped(error) = &outcome.status else {
            panic!("{:?}", outcome.status);
        };
        assert_eq!(error.to_string(), "host-side Goal deadline expired");
        assert!(!outcome.runtime_pass);
        assert_eq!(outcome.remaining_workers, 0);
        assert_eq!(outcome.artifact, None);
        assert_eq!((outcome.findings, outcome.spent_microusd), (0, 0));
        let read = |name: &str| -> Value {
            serde_json::from_slice(&fs::read(outcome.run_dir.join(name)).unwrap()).unwrap()
        };
        assert_eq!(read("manifest.json")["live"], false);
        assert_eq!(read("state.json")["goal_status"], "stopped");
        assert_eq!(read("state.json")["held_microusd"], 0);
        assert_eq!(read("cleanup.json")["verdict"], "PASS");
        let report = read("runtime-report.json");
        assert_eq!(report["runtime_verdict"], "FAIL");
        assert_eq!(report["reason"], "host-side Goal deadline expired");
        assert_eq!(report["idle_probe"], true);
        let kinds: Vec<String> = fs::read_to_string(outcome.run_dir.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| {
                let event: Value = serde_json::from_str(line).unwrap();
                event["kind"].as_str().unwrap().to_owned()
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "goal_registered",
                "agent_started",
                "agent_started",
                "agent_started",
                "goal_stopped",
                "agent_stopped",
                "agent_stopped",
                "agent_stopped",
                "cleanup_completed",
            ]
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
