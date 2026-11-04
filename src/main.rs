mod mvp;
mod smoke;

use solos_swarm::{
    broker,
    bwrap::Bwrap,
    exposure::{Exposure, Host},
    gateway::provider_for,
    harness::{PiHarness, Role, SessionSpec, bounded_line},
    swarm::{self, GoalRun, GoalStatus},
    vault,
};

use anyhow::{Context, Result, ensure};
use broker::{Goal, Upstream, write_json};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
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
    /// Inside a harness's sandbox: serve a loopback TCP port as a pipe to the
    /// broker's Unix socket while the program after `--` runs, and exit with it.
    #[command(hide = true)]
    Bridge {
        #[arg(long)]
        listen: std::net::SocketAddr,
        #[arg(long)]
        socket: PathBuf,
        #[arg(last = true, required = true)]
        program: Vec<OsString>,
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
    /// A machine that runs an agent's commands: it dials out to the agent and
    /// runs what it is sent in its own sandbox.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// This machine's logins and API keys, which agent mode uses without a
    /// model ever seeing a value. No command prints a value.
    Vault {
        #[command(flatten)]
        host: HostArgs,
        #[command(subcommand)]
        command: VaultCommand,
    },
    /// This machine's headless browser, whose every step it signs.
    Browser {
        #[command(subcommand)]
        command: BrowserCommand,
    },
    /// A Model Context Protocol server on stdin and stdout, for an MCP host
    /// such as Claude Code or Codex. With --socket and --token-file it offers
    /// one principal's tools through its broker; with --state-dir and
    /// --operator, an operator's tools over an agent.
    Mcp {
        /// The broker's socket, such as an agent's `<state dir>/broker.sock`.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// The principal's bearer token. Read once and deleted.
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// How long one call waits out a paused broker before it reports the
        /// pause. Default 45.
        #[arg(long)]
        max_pause_wait_secs: Option<u64>,
        /// The agent's state directory.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Who files the requests; the Policy names them `mcp:<operator>`.
        #[arg(long)]
        operator: Option<String>,
        /// The agent's config, for `agent_status`.
        #[arg(long)]
        config: Option<PathBuf>,
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

/// The paths `agent serve` adds to its sandboxes, which a vault, key or
/// trail path must stay out of. Give the ones `agent serve` got, or the
/// check covers only the config's mounts.
#[derive(clap::Args)]
struct HostArgs {
    /// The agent's state directory.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// A Pi Router or Pi workers: the checkout that holds `node_modules`.
    #[arg(long, global = true)]
    pi_root: Option<PathBuf>,
    /// A Pi Router or Pi workers: the node binary.
    #[arg(long, global = true)]
    node: Option<PathBuf>,
}

impl HostArgs {
    fn host(&self) -> Host {
        Host {
            state_dir: self.state_dir.clone(),
            pi_root: self.pi_root.clone(),
            node: self.node.clone(),
        }
    }
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
    /// Check and replay what the agent's browsers did: the event chain, each
    /// step's signature under the key trusted for the machine that ran it,
    /// and every page and typed text the steps name. Reads the Store and the
    /// trail's blobs and writes nothing, so it is safe while the agent
    /// serves. Exits 1 on any problem.
    Trail {
        /// The agent's state directory.
        #[arg(long)]
        state_dir: PathBuf,
        /// agent.yaml: the local trail key (`browser.audit_key`) and where
        /// the blobs are (`trail.path`).
        #[arg(long)]
        config: Option<PathBuf>,
        /// One Ticket, printed step by step.
        #[arg(long)]
        ticket: Option<String>,
        /// With --ticket: print one step's blob, as stored, instead of the
        /// trail.
        #[arg(long, requires = "ticket")]
        step: Option<u32>,
        /// With --step: the Assignment, when more than one of the Ticket's
        /// has that step.
        #[arg(long, requires = "step")]
        assignment: Option<String>,
        /// With --step: `view`, the exact bytes the model got, or
        /// `snapshot`, the page as captured.
        #[arg(long, value_enum, default_value_t = solos_swarm::agent::trail::Shown::View, requires = "step")]
        show: solos_swarm::agent::trail::Shown,
        /// `<environment>=ed25519:<hex>`: verify that environment's steps
        /// with this key instead of the config's. Repeatable. A verifier
        /// that holds only public keys needs no config.
        #[arg(long)]
        trust: Vec<solos_swarm::agent::trail::Trusted>,
        /// Print JSON instead of text.
        #[arg(long, conflicts_with = "step")]
        json: bool,
        /// A step that was asked and never answered, or answered with no
        /// signed step, is a problem, not a warning.
        #[arg(long)]
        strict: bool,
    },
    /// Measure the agent on a generated stream of chat and CI messages with
    /// work requests among them, as DeepSeek's super-agent ablation did:
    /// each run serves a fresh agent with simulated work. Writes
    /// report.json and report.md into --out.
    Bench {
        /// A workload spec (YAML); every field it leaves out has its default.
        #[arg(long)]
        workload: Option<PathBuf>,
        /// super-agent, no-pass-through or serial; several separated by
        /// commas.
        #[arg(long, value_delimiter = ',', required = true)]
        design: Vec<solos_swarm::agent::bench::Design>,
        /// Runs per design; run n uses the spec's seed plus n.
        #[arg(long, default_value_t = 3)]
        runs: usize,
        /// A new or empty directory for the runs and the reports.
        #[arg(long)]
        out: PathBuf,
        /// `scripted` measures scheduling only and spends nothing; `pi` calls
        /// a real provider and needs --max-spend-usd.
        #[arg(long, default_value = "scripted", value_parser = ["scripted", "pi"])]
        router: String,
        /// Wall time per workload time, instead of the spec's (0.01: a
        /// workload minute lasts 0.6 s).
        #[arg(long)]
        time_factor: Option<f64>,
        /// Stop a run after this many workload minutes.
        #[arg(long)]
        deadline_minutes: Option<f64>,
        /// --router pi: an agent.yaml whose `router` section is the Pi Router.
        #[arg(long)]
        router_config: Option<PathBuf>,
        /// --router pi: the checkout that holds `node_modules` and `extensions`.
        #[arg(long)]
        pi_root: Option<PathBuf>,
        /// --router pi: the node binary.
        #[arg(long)]
        node: Option<PathBuf>,
        /// --router pi: the model key. Read once and deleted.
        #[arg(long)]
        credential_file: Option<PathBuf>,
        /// --router pi: the most the whole bench may spend, split evenly over
        /// its runs. Required: a paid bench refuses to start without it.
        #[arg(long)]
        max_spend_usd: Option<f64>,
    },
    /// Write a bench workload's seeded Triggers file, in workload time from 0.
    Workload {
        /// A workload spec (YAML); the defaults otherwise.
        #[arg(long)]
        spec: Option<PathBuf>,
        /// Instead of the spec's.
        #[arg(long)]
        seed: Option<u64>,
        /// The Triggers file to write, one JSON value per line.
        #[arg(long)]
        out: PathBuf,
    },
    /// Write the key and self-signed certificate the agent's node listener
    /// serves, and print the fingerprint each node's config pins.
    InitNodes {
        /// Created if missing; an existing key is never replaced.
        #[arg(long)]
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum NodeCommand {
    /// Write this machine's key and self-signed certificate, and print the
    /// fingerprint the agent's `nodes.allow` pins.
    Init {
        /// Created if missing; an existing key is never replaced.
        #[arg(long)]
        dir: PathBuf,
    },
    /// Dial the agent and run what it sends until SIGINT or SIGTERM, dialing
    /// again whenever the connection ends.
    Serve {
        /// The node's name, the agent's address and fingerprint, its own
        /// certificate, sandbox mounts and secrets.
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum BrowserCommand {
    /// Write a new Ed25519 trail key, where no sandbox can read it, and
    /// print its public half (`ed25519:<hex>`) to pin wherever the trail
    /// is verified. An existing file is never replaced.
    InitKey {
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum VaultCommand {
    /// Write the key file and an empty vault where the config's `vault:`
    /// names them. An existing key or vault is never replaced.
    Init {
        /// agent.yaml or node.yaml.
        #[arg(long)]
        config: PathBuf,
    },
    /// Add a website login, or with --replace change one. The password comes
    /// on stdin or from --password-file.
    AddLogin {
        /// agent.yaml or node.yaml.
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        name: vault::SecretName,
        /// An exact origin it is filled on, such as https://github.com.
        /// Repeat for more.
        #[arg(long = "site", required = true)]
        sites: Vec<vault::Origin>,
        #[arg(long)]
        username: String,
        /// Shown to the model and to the person who approves its use.
        #[arg(long, default_value = "")]
        hint: String,
        /// Whose Tickets may use it, as the Policy names people, such as
        /// slack:U024BE7LH. Repeat for more; without it, any requester.
        #[arg(long = "for")]
        users: Vec<String>,
        /// first-use: a person approves the first use on each Ticket. none:
        /// unattended use.
        #[arg(long, default_value = "first-use")]
        approval: vault::Approval,
        /// Read once and deleted; `-` reads stdin.
        #[arg(long, default_value = "-")]
        password_file: PathBuf,
        #[arg(long)]
        replace: bool,
    },
    /// Add an API key, or with --replace change one. The value comes on
    /// stdin or from --value-file.
    AddKey {
        /// agent.yaml or node.yaml.
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        name: vault::SecretName,
        /// An exact host it is sent to, such as api.openai.com. Repeat for
        /// more.
        #[arg(long = "host", required = true)]
        hosts: Vec<vault::HostSpec>,
        /// The request header that carries it, such as authorization.
        #[arg(long)]
        header: vault::HeaderField,
        /// bearer: the header says `Bearer <value>`. raw: the value alone.
        #[arg(long)]
        scheme: vault::KeyScheme,
        /// Shown to the model and to the person who approves its use.
        #[arg(long, default_value = "")]
        about: String,
        /// Whose Tickets may use it, as the Policy names people, such as
        /// slack:U024BE7LH. Repeat for more; without it, any requester.
        #[arg(long = "for")]
        users: Vec<String>,
        /// first-use: a person approves the first use on each Ticket. none:
        /// unattended use.
        #[arg(long, default_value = "first-use")]
        approval: vault::Approval,
        /// Read once and deleted; `-` reads stdin.
        #[arg(long, default_value = "-")]
        value_file: PathBuf,
        #[arg(long)]
        replace: bool,
    },
    /// Each item's name, sites or hosts, username, hint, approval and who
    /// may use it.
    List {
        /// agent.yaml or node.yaml.
        #[arg(long)]
        config: PathBuf,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    Remove {
        /// agent.yaml or node.yaml.
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        name: vault::SecretName,
    },
}

/// Write an identity in `dir` and say what to do with its fingerprint.
fn init_identity(dir: &Path, subject: &str, peer: &str) -> Result<()> {
    let fingerprint = solos_swarm::remote::tls::init(dir, subject)?;
    let (cert, key) = solos_swarm::remote::tls::files(dir);
    eprintln!(
        "wrote {} and {}; pin this fingerprint in {peer}:",
        cert.display(),
        key.display()
    );
    println!("{fingerprint}");
    Ok(())
}

/// Read a one-use credential file and delete it, whether or not the read
/// worked, so no later error can leave the key on disk.
fn consume_credential(path: &Path) -> Result<Zeroizing<String>> {
    let result = fs::read_to_string(path);
    let _ = fs::remove_file(path);
    let raw = Zeroizing::new(result.context("cannot read broker credential")?);
    Ok(Zeroizing::new(raw.trim().to_owned()))
}

fn vault_command(host: &Host, command: VaultCommand) -> Result<()> {
    use vault::{ItemKind, NewItem, SecretValue, Vault};
    match command {
        VaultCommand::Init { config } => {
            let (config, view) = vault_config(&config, host)?;
            Vault::init(&config, &view)?;
            eprintln!(
                "the vault {} is ready, under the key {}",
                config.path.display(),
                config.key_file.display()
            );
            Ok(())
        }
        VaultCommand::AddLogin {
            config,
            name,
            sites,
            username,
            hint,
            users,
            approval,
            password_file,
            replace,
        } => {
            let value = SecretValue::read_once(&password_file)?;
            let item = NewItem {
                name,
                kind: ItemKind::Login {
                    sites,
                    username,
                    hint,
                },
                users: users.into_iter().collect(),
                approval,
                value,
            };
            add_to_vault(&config, item, replace, host)
        }
        VaultCommand::AddKey {
            config,
            name,
            hosts,
            header,
            scheme,
            about,
            users,
            approval,
            value_file,
            replace,
        } => {
            let value = SecretValue::read_once(&value_file)?;
            let item = NewItem {
                name,
                kind: ItemKind::Key {
                    hosts,
                    header,
                    scheme,
                    about,
                },
                users: users.into_iter().collect(),
                approval,
                value,
            };
            add_to_vault(&config, item, replace, host)
        }
        VaultCommand::List { config, json } => {
            let items = open_vault(&config, host)?.list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                print!("{}", vault::table(&items));
            }
            Ok(())
        }
        VaultCommand::Remove { config, name } => {
            ensure!(
                open_vault(&config, host)?.remove(&name)?,
                "the vault holds no item {name}"
            );
            eprintln!("removed {name}");
            Ok(())
        }
    }
}

fn add_to_vault(
    config: &Path,
    item: vault::NewItem,
    replace: bool,
    host: &Host,
) -> Result<()> {
    let name = item.name.clone();
    open_vault(config, host)?.add(item, replace)?;
    eprintln!("saved {name}");
    Ok(())
}

fn open_vault(config: &Path, host: &Host) -> Result<vault::Vault> {
    let (config, view) = vault_config(config, host)?;
    vault::Vault::open(&config, &view)
}

fn vault_config(path: &Path, host: &Host) -> Result<(vault::VaultConfig, vault::SandboxView)> {
    let shown = path.display();
    let bytes = fs::read(path).with_context(|| format!("cannot read {shown}"))?;
    let yaml: serde_yaml::Value =
        serde_yaml::from_slice(&bytes).with_context(|| format!("{shown} is not YAML"))?;
    let (config, view) = if yaml.get("work_root").is_some() {
        let node = solos_swarm::remote::NodeConfig::load(path)?;
        let view = node.sandbox_view();
        (node.vault, view)
    } else {
        let agent = solos_swarm::agent::AgentConfig::load(path)?;
        let view = Exposure::new(&agent, host).view();
        (agent.vault, view)
    };
    let config = config.with_context(|| format!("{shown} has no vault: section"))?;
    Ok((config, view))
}

/// `bridge`: exit with the program's status, as if it ran alone.
async fn bridge(
    listen: std::net::SocketAddr,
    socket: PathBuf,
    program: Vec<OsString>,
) -> Result<()> {
    let (program, args) = program
        .split_first()
        .context("the bridge names no program")?;
    let bridge = solos_swarm::bridge::Bridge::bind(listen, socket).await?;
    let status = bridge.run(program, args).await?;
    std::process::exit(status.code().unwrap_or(1))
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
        // One thread: it only pipes bytes, inside every harness sandbox.
        Action::Bridge {
            listen,
            socket,
            program,
        } => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("runtime startup failed")
            .and_then(|runtime| runtime.block_on(bridge(listen, socket, program))),
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
        Action::WasiWorker { .. } | Action::SandboxEnter { .. } | Action::Bridge { .. } => {
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
        Action::Agent {
            command:
                AgentCommand::Trail {
                    state_dir,
                    config,
                    ticket,
                    step,
                    assignment,
                    show,
                    trust,
                    json,
                    strict,
                },
        } => {
            use solos_agent_core::model::{AssignmentId, TicketId};
            use solos_swarm::agent::{AgentConfig, trail};
            let config = config.as_deref().map(AgentConfig::load).transpose()?;
            let trust = trail::Trust::new(config.as_ref(), trust)?;
            let blobs = config
                .map(|config| config.trail)
                .unwrap_or_default()
                .root(&state_dir);
            let ticket = ticket.map(TicketId::new);
            let report = trail::verify(&state_dir, &blobs, &trust, ticket.as_ref(), strict)?;
            if let Some(step) = step {
                let assignment = assignment.map(AssignmentId::new);
                let bytes = trail::shown(&report, &blobs, assignment.as_ref(), step, show)?;
                std::io::Write::write_all(&mut std::io::stdout().lock(), &bytes)?;
                for found in &report.problems {
                    eprintln!("{found}");
                }
            } else if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", trail::render(&report, ticket.as_ref()));
            }
            let problems = report.problems.iter().filter(|found| found.fatal).count();
            ensure!(report.ok, "the trail has problems ({problems})");
            Ok(())
        }
        Action::Agent {
            command:
                AgentCommand::Bench {
                    workload,
                    design,
                    runs,
                    out,
                    router,
                    time_factor,
                    deadline_minutes,
                    router_config,
                    pi_root,
                    node,
                    credential_file,
                    max_spend_usd,
                },
        } => {
            use solos_swarm::agent::bench::{self, BenchArgs};
            let options = BenchArgs {
                workload,
                designs: design,
                runs,
                out,
                router,
                time_factor,
                deadline_minutes,
                router_config,
                pi_root,
                node,
                credential_file,
                max_spend_usd,
            }
            .into_options()?;
            let report = bench::run(options).await?;
            print!("{}", report.markdown());
            Ok(())
        }
        Action::Agent {
            command: AgentCommand::Workload { spec, seed, out },
        } => {
            use solos_swarm::agent::bench::workload::{WorkloadSpec, generate};
            let spec = match spec {
                Some(path) => WorkloadSpec::load(&path)?,
                None => WorkloadSpec::default(),
            };
            let workload = generate(&spec, seed.unwrap_or(spec.seed))?;
            workload.write_jsonl(&out)?;
            println!(
                "{}",
                json!({"triggers": workload.triggers.len(), "requests": workload.requests, "out": out})
            );
            Ok(())
        }
        Action::Agent {
            command: AgentCommand::InitNodes { dir },
        } => init_identity(
            &dir,
            solos_swarm::remote::tls::AGENT_NAME,
            "each node's agent.fingerprint",
        ),
        Action::Node {
            command: NodeCommand::Init { dir },
        } => init_identity(
            &dir,
            solos_swarm::remote::tls::NODE_NAME,
            "the agent's nodes.allow.<name>.fingerprint",
        ),
        Action::Vault { host, command } => vault_command(&host.host(), command),
        Action::Browser {
            command: BrowserCommand::InitKey { out },
        } => solos_swarm::browse::TrailSigner::init(&out, &vault::SandboxView::new([]))
            .map(|key| println!("{key}")),
        Action::Node {
            command: NodeCommand::Serve { config },
        } => {
            ensure!(
                std::env::consts::OS == "linux",
                "a node's sandbox requires Linux"
            );
            let config = solos_swarm::remote::NodeConfig::load(&config)?;
            let node = solos_swarm::remote::Node::new(&config)?;
            node.serve(solos_swarm::agent::shutdown()).await
        }
        Action::Mcp {
            socket,
            token_file,
            max_pause_wait_secs,
            state_dir,
            operator,
            config,
        } => {
            use solos_swarm::agent::AgentConfig;
            use solos_swarm::mcp::{self, operator::OperatorTools, proxy};
            // First, so no usage error can leave the token on disk.
            let token = token_file.as_deref().map(consume_credential).transpose()?;
            match (socket, token, state_dir, operator) {
                (Some(socket), Some(token), None, None) if config.is_none() => {
                    ensure!(!token.is_empty(), "the token file is empty");
                    let wait =
                        max_pause_wait_secs.map_or(proxy::DEFAULT_PAUSE_WAIT, Duration::from_secs);
                    mcp::serve_stdio(proxy::BrokerTools::new(&socket, token, wait)?).await
                }
                (None, None, Some(state_dir), Some(operator)) if max_pause_wait_secs.is_none() => {
                    let config = config.as_deref().map(AgentConfig::load).transpose()?;
                    mcp::serve_stdio(OperatorTools::new(state_dir, operator, config)?).await
                }
                _ => anyhow::bail!(
                    "mcp takes --socket and --token-file [--max-pause-wait-secs], or --state-dir \
                     and --operator [--config]"
                ),
            }
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
    let result = Command::new("bwrap").args(doctor_probe()).status().await?;
    ensure!(result.success(), "bubblewrap isolation unavailable");
    println!(
        "{}",
        json!({"verdict":"PASS","node":String::from_utf8_lossy(&version.stdout).trim(),"isolation":"bubblewrap-unshare-all"})
    );
    Ok(())
}

/// The sandbox `doctor` starts to prove bubblewrap isolates on this host.
fn doctor_probe() -> Vec<OsString> {
    Bwrap::new()
        .unshare_all()
        .die_with_parent()
        .ro_bind("/usr", "/usr")
        .ro_bind_try("/lib", "/lib")
        .ro_bind_try("/lib64", "/lib64")
        .ro_bind_try("/etc", "/etc")
        .proc("/proc")
        .dev("/dev")
        .exec("/usr/bin/true", std::iter::empty::<&str>())
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

    /// The Pi agent config `agent serve` judges a vault against.
    const PI_AGENT: &str = "policy: {people: [slack:alice]}
sources: [{kind: jsonl, path: triggers.jsonl}]
channels: [{kind: jsonl, source: slack, path: outbox.jsonl}]
router: {kind: scripted}
executor: {kind: pi, provider: deepseek, model: deepseek-flash, max_output_tokens: 2048, prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}}
vault: {path: VAULT, key_file: KEY}
budgets: {session_microusd: 500000}
";

    #[test]
    fn the_vault_cli_refuses_the_paths_agent_serve_refuses_under_its_mounts() {
        let dir = std::env::temp_dir().join(format!("swarm-vault-cli-{}", uuid::Uuid::new_v4()));
        let (state, pi_root) = (dir.join("state"), dir.join("pi"));
        let host = Host {
            state_dir: Some(state.clone()),
            pi_root: Some(pi_root.clone()),
            node: None,
        };
        let cases = [
            (state.join("tickets/vault.yaml"), dir.join("keys/vault.key")),
            (dir.join("keys/vault.yaml"), pi_root.join("vault.key")),
        ];
        for (vault, key) in cases {
            let path = dir.join("agent.yaml");
            std::fs::create_dir_all(&dir).unwrap();
            let yaml = PI_AGENT
                .replace("VAULT", &vault.display().to_string())
                .replace("KEY", &key.display().to_string());
            std::fs::write(&path, yaml).unwrap();
            let (config, view) = vault_config(&path, &host).unwrap();
            assert!(
                vault::Vault::init(&config, &view).is_err(),
                "{} is under a mount serve uses",
                vault.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_doctor_probe_keeps_its_exact_argv() {
        let args = doctor_probe();
        let args: Vec<&str> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
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
            ]
        );
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

    #[tokio::test]
    async fn a_host_cancel_stops_the_run_like_an_operator_cancel_and_cleans_up() {
        use solos_swarm::harness::scripted::ScriptedHarness;
        let (dir, run) = scripted_run(60, true);
        let cancel = swarm::Cancel::on(async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            "the ticket was stopped".to_owned()
        });
        let outcome = tokio::time::timeout(
            Duration::from_secs(30),
            swarm::run_goal_until(
                run,
                &ScriptedHarness::new(BTreeMap::new()),
                Arc::new(smoke::Scripted),
                cancel,
            ),
        )
        .await
        .expect("the cancel did not end the run")
        .unwrap();
        let GoalStatus::Stopped(error) = &outcome.status else {
            panic!("{:?}", outcome.status);
        };
        assert_eq!(error.to_string(), "the ticket was stopped");
        assert_eq!((outcome.remaining_workers, outcome.held_microusd), (0, 0));
        let read = |name: &str| -> Value {
            serde_json::from_slice(&fs::read(outcome.run_dir.join(name)).unwrap()).unwrap()
        };
        assert_eq!(read("cleanup.json")["verdict"], "PASS");
        assert_eq!(read("state.json")["goal_status"], "stopped");
        assert_eq!(
            read("runtime-report.json")["reason"],
            "the ticket was stopped"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
