//! Agent mode's shell: the async ports, their first adapters, and the
//! runtime that drives `solos_agent_core::Engine` on the SQLite Store.
//! [`serve`] is the composition root, the one place that names concrete
//! adapters.

pub mod bench;
pub mod broker;
pub mod config;
pub mod discord;
pub mod environment;
pub mod forge;
pub mod github;
pub mod goal_template;
pub mod health;
pub mod jobs;
pub mod jsonl;
pub mod links;
mod markdown;
pub mod mcp;
pub mod pi_executor;
pub mod pi_router;
mod plain_answer;
pub mod ports;
pub mod prune;
pub mod pstack;
pub mod publish;
pub mod runtime;
pub mod schedule;
pub mod scripted;
pub mod simulated;
pub mod slack;
pub mod status;
pub mod stop;
pub mod swarm_executor;
pub mod trail;
pub mod utc;
pub mod watchdog;

pub use config::AgentConfig;
pub use runtime::{Adapters, HOST_RESTARTED, RunOptions, Runtime, Summary};

use crate::broker::{now_ms, write_json};
use crate::browse::{Browsers, Launcher, TrailSigner};
use crate::code::{CodeConfig, CodeExecutor};
use crate::exposure::{self, Exposure, Host};
use crate::gateway::{Provider, provider_for, upstream_profile};
use crate::harness::claude_code::ClaudeCodeHarness;
use crate::harness::{AgentHarness, PiHarness, Role, SessionSpec};
use crate::remote::Listener;
use crate::vault::Vault;
use anyhow::{Context, Result, bail, ensure};
use broker::browser::{BrowserParts, LocalBrowser};
use broker::{
    AgentBroker, BrokerParts, DecideTerms, Grant, ModelTerms, Principal, ROUTER_TOOLS, VaultParts,
    WorkerParts,
};
use config::{
    ChannelConfig, ClaudeConfig, ExecutorConfig, RouterConfig, RunCodeConfig, SourceConfig,
};
use discord::{DiscordApi, DiscordChannel, DiscordSource};
use github::{GithubApp, GithubChannel, GithubSource};
use goal_template::GoalTemplates;
use jsonl::{JsonlChannel, JsonlSource};
use links::TicketLinks;
use mcp::McpChannel;
use pi_executor::{PiExecutor, PiExecutorParts};
use pi_router::PiRouter;
use ports::{Channel, Executor, Router, Source};
use pstack::Pstack;
use schedule::ScheduleSource;
use scripted::{ScriptedExecutor, ScriptedRouter};
use serde_json::json;
use simulated::{SimulatedExecutor, SimulatedParts};
use slack::{SlackApi, SlackChannel, SlackSource, Token};
use solos_agent_store::SqliteStore;
use std::collections::BTreeMap;
use std::future::Future;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use swarm_executor::{SwarmExecutor, SwarmParts};
use zeroize::Zeroizing;

/// The Store's file in the state directory.
pub const STORE_FILE: &str = "agent.db";
/// The Router's transcripts, stderr and Pi settings, in the state directory.
pub const ROUTER_DIR: &str = "router";

/// Each Ticket's directory, in the state directory.
pub const TICKETS_DIR: &str = "tickets";
/// The browsers' session directories and sockets, unless `browser.runtime_dir` says.
pub const BROWSER_DIR: &str = "browser";
/// The trail's blobs, unless `trail.path` says.
pub const TRAIL_DIR: &str = "trail";

/// What `agent serve` takes besides its config file. A Pi Router or Pi
/// workers need the last three; scripted ones need none.
#[derive(Default)]
pub struct ServeOptions {
    /// Read the Sources once, finish what they started and exit.
    pub until_idle: bool,
    /// The checkout that holds `node_modules` and `extensions`.
    pub pi_root: Option<PathBuf>,
    /// The node binary.
    pub node: Option<PathBuf>,
    /// The model key, read once from its file. It never leaves this process.
    pub credential: Option<Zeroizing<String>>,
}

/// Where the Pi Router's and Pi workers' sessions run, and the provider
/// their broker calls.
pub struct PiHost<H> {
    pub harness: H,
    pub provider: Arc<dyn Provider>,
    pub pi_root: PathBuf,
    pub node: PathBuf,
    pub browser: Launcher,
}

/// `solos-swarm agent serve`. With `until_idle`, Sources read what is there
/// and the run ends when nothing is left to do; otherwise it serves until
/// SIGINT or SIGTERM.
pub async fn serve(config: &Path, state_dir: &Path, options: ServeOptions) -> Result<Summary> {
    serve_until(config, state_dir, options, shutdown()).await
}

/// [`serve`] until `stop` resolves instead of a signal, as `agent bench`
/// ends each run.
pub async fn serve_until(
    config: &Path,
    state_dir: &Path,
    options: ServeOptions,
    stop: impl Future<Output = ()>,
) -> Result<Summary> {
    let config = AgentConfig::load(config)?;
    let ServeOptions {
        until_idle,
        pi_root,
        node,
        credential,
    } = options;
    // The App's key is refused before it is read if a sandbox could read it.
    check_key_path(&config, state_dir, pi_root.as_deref(), node.as_deref())?;
    // First, so the token files are consumed before anything else can fail.
    let io = Io::open(&config, state_dir, !until_idle)?;
    let host = match config.pi_upstream() {
        None => None,
        Some((provider, model)) => {
            let (Some(pi_root), Some(node), Some(key)) = (pi_root, node, credential) else {
                bail!("a pi router or pi workers need --pi-root, --node and --credential-file");
            };
            ensure!(
                std::env::consts::OS == "linux",
                "the isolation of pi sessions requires Linux"
            );
            let upstream = upstream_profile(provider, model)?;
            let provider = provider_for(&upstream, crate::swarm::http_client()?, key)?;
            Some(PiHost {
                harness: PiHarness,
                provider: Arc::new(provider),
                pi_root,
                node,
                browser: Launcher::Lightpanda,
            })
        }
    };
    serve_io(config, state_dir, until_idle, io, host, stop).await
}

/// [`serve`] with the Pi sessions' harness and provider given, so tests can
/// script them. A config with a Pi Router or Pi workers needs `host`; one
/// with neither ignores it.
pub async fn serve_with<H>(
    config: AgentConfig,
    state_dir: &Path,
    until_idle: bool,
    host: Option<PiHost<H>>,
) -> Result<Summary>
where
    H: AgentHarness + Send + Sync + 'static,
    H::Session: Send + 'static,
{
    let paths = host
        .as_ref()
        .map(|host| (host.pi_root.as_path(), host.node.as_path()));
    check_key_path(
        &config,
        state_dir,
        paths.map(|(root, _)| root),
        paths.map(|(_, node)| node),
    )?;
    let io = Io::open(&config, state_dir, !until_idle)?;
    serve_io(config, state_dir, until_idle, io, host, shutdown()).await
}

async fn serve_io<H>(
    config: AgentConfig,
    state_dir: &Path,
    until_idle: bool,
    io: Io,
    host: Option<PiHost<H>>,
    stop: impl Future<Output = ()>,
) -> Result<Summary>
where
    H: AgentHarness + Send + Sync + 'static,
    H::Session: Send + 'static,
{
    private_dir(state_dir)?;
    let store = SqliteStore::open(state_dir.join(STORE_FILE)).context("cannot open the store")?;
    let (facts, inbound) = ports::FactSink::channel();
    let nodes = match &config.nodes {
        Some(nodes) => Some(Listener::start(nodes).await?),
        None => None,
    };
    let pi = match (config.pi_upstream(), host) {
        (None, _) => None,
        (Some(_), None) => bail!("a pi router or pi workers need a session host"),
        (Some(_), Some(host)) => Some(PiParts::new(
            &config,
            state_dir,
            host,
            facts.clone(),
            nodes.as_ref(),
            io.github.as_deref(),
        )?),
    };
    let mut router: Arc<dyn Router> = match pi.as_ref().and_then(|pi| pi.router.clone()) {
        Some(router) => router,
        None => Arc::new(scripted_router(&config)),
    };
    let executor: Arc<dyn Executor> = match pi.as_ref().and_then(|pi| pi.executor.clone()) {
        Some(executor) => executor,
        None => match &config.executor {
            ExecutorConfig::Simulated {
                max_workers,
                time_factor,
                default_work_ms,
                router_waits_for_work,
            } => {
                let simulated = SimulatedExecutor::new(SimulatedParts {
                    max_workers: *max_workers,
                    time_factor: *time_factor,
                    default_work_ms: *default_work_ms,
                })?;
                if *router_waits_for_work {
                    router = Arc::new(simulated.hold(router));
                }
                Arc::new(simulated)
            }
            _ => Arc::new(ScriptedExecutor),
        },
    };
    let runtime_options = RunOptions {
        until_idle,
        ..RunOptions::default()
    };
    let watchdog = watchdog::Running::start(&config, state_dir, &io.channels, &facts)?;
    let forge = pi.as_ref().and_then(|pi| pi.forge.clone());
    let runtime = Runtime::with_sink(
        store,
        config.policy.clone(),
        io.adapters(router, executor),
        runtime_options,
        (facts, inbound),
    )
    .with_control_dir(state_dir)
    .with_forge(forge)
    .with_pass_through(config.pass_through);
    let served = match pi {
        Some(pi) => Some(pi.serve(state_dir)?),
        None => None,
    };
    let runtime = match &served {
        Some(served) if served.browsers.is_some() => {
            runtime.with_closing(broker::browser::close_all(served.broker.clone()))
        }
        _ => runtime,
    };
    let summary = runtime.run(stop).await;
    if let Some(watchdog) = watchdog {
        watchdog.stop().await;
    }
    if let Some(served) = served {
        served.stop().await;
    }
    if let Some(nodes) = nodes {
        nodes.stop().await;
    }
    summary
}

fn scripted_router(config: &AgentConfig) -> ScriptedRouter {
    let ignore = matches!(
        config.router,
        RouterConfig::Scripted {
            ignore_follow_ups: true
        }
    );
    ScriptedRouter::new(config.budgets.session_microusd).ignoring_follow_ups(ignore)
}

fn private_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))
}

/// The Pi Router and the Pi workers before their broker serves: the
/// adapters, and the broker that admits the Router's token and, as each
/// Assignment starts, a worker's.
struct PiParts<H> {
    broker: AgentBroker,
    /// Clones, admits and publishes; the same forge for the executor, the
    /// broker and the runtime.
    forge: Option<Arc<dyn ports::Forge>>,
    router: Option<Arc<PiRouter>>,
    executor: Option<Arc<PiExecutor<H>>>,
    browsers: Option<Arc<Browsers>>,
}

impl<H> PiParts<H>
where
    H: AgentHarness + Send + Sync + 'static,
    H::Session: Send + 'static,
{
    fn new(
        config: &AgentConfig,
        state_dir: &Path,
        host: PiHost<H>,
        facts: ports::FactSink,
        nodes: Option<&Listener>,
        github: Option<&GithubApp>,
    ) -> Result<Self> {
        let PiHost {
            harness,
            provider,
            pi_root,
            node,
            browser: launcher,
        } = host;
        let harness = Arc::new(harness);
        let (pi_root, node) = (canonical(&pi_root)?, canonical(&node)?);
        let pstack = match &config.pstack {
            None => None,
            Some(wanted) => {
                let opened = Pstack::open(&pi_root, &wanted.entry).context("pstack")?;
                eprintln!(
                    "agent: pstack {} at {}, tree sha256:{}, adapter sha256:{}",
                    opened.version(),
                    opened.root().display(),
                    opened.digest(),
                    pstack::sha256_hex(pstack::ADAPTER.as_bytes())
                );
                Some(Arc::new(opened))
            }
        };
        let state = canonical(state_dir)?;
        let tickets_dir = state.join(TICKETS_DIR);
        let readable = Exposure::new(
            config,
            &Host {
                state_dir: Some(state),
                pi_root: Some(pi_root.clone()),
                node: Some(node.clone()),
                pstack: pstack.as_ref().map(|opened| opened.root().to_owned()),
            },
        )
        .view();
        let mut browsers = None;
        let workers = match &config.executor {
            ExecutorConfig::Scripted {} | ExecutorConfig::Simulated { .. } => None,
            ExecutorConfig::Pi {
                provider,
                model,
                prices,
                max_input_bytes,
                max_output_tokens,
                temperature,
                run_code,
                ..
            } => {
                let code = match run_code {
                    None => None,
                    Some(run_code) => {
                        let tree = pstack.as_deref().map(Pstack::root);
                        let sandbox = code_config(run_code, &tickets_dir, tree);
                        let code =
                            CodeExecutor::from_config(sandbox).context("executor.run_code")?;
                        Some(Arc::new(code))
                    }
                };
                let terms = ModelTerms {
                    upstream: upstream_profile(provider, model)?,
                    prices: *prices,
                    max_input_bytes: *max_input_bytes,
                    max_output_tokens: *max_output_tokens,
                    call_limit: None,
                    temperature: Some(*temperature),
                };
                let vault = match &config.vault {
                    None => None,
                    Some(vault) => Some(VaultParts {
                        vault: Arc::new(Vault::open(vault, &readable).context("vault")?),
                        asks_on: config.approval_sources(),
                    }),
                };
                let browser = match &config.browser {
                    None => None,
                    Some(browser) => {
                        let opened = local_browsers(
                            config,
                            browser,
                            state_dir,
                            &readable,
                            vault.as_ref().map(|parts| parts.vault.clone()),
                            launcher,
                        )?;
                        browsers = Some(opened.0.clone());
                        Some(opened.1)
                    }
                };
                Some(WorkerParts {
                    terms,
                    code,
                    vault,
                    browser,
                })
            }
        };
        let worker_model = workers
            .as_ref()
            .map(|parts| parts.terms.upstream.model.clone());
        let job_limits = match &config.executor {
            ExecutorConfig::Pi {
                run_code: Some(run_code),
                ..
            } => run_code.job_limits(),
            _ => jobs::JobLimits::default(),
        };
        let parts = BrokerParts {
            dir: state_dir.to_owned(),
            store: SqliteStore::open(state_dir.join(STORE_FILE))
                .context("cannot open the store for the broker")?,
            provider: provider.clone(),
            facts,
            decide: DecideTerms::for_config(config),
            workers,
        };
        let remote = environment::Nodes {
            default: match &config.executor {
                ExecutorConfig::Pi { environment, .. } => environment.clone(),
                ExecutorConfig::Scripted {} | ExecutorConfig::Simulated { .. } => None,
            },
            nodes: nodes.map(Listener::environments).unwrap_or_default(),
        };
        let broker = AgentBroker::with_pstack(parts, job_limits, remote, pstack)?;
        let forge = config
            .forge
            .as_ref()
            .map(|wanted| forge::open(wanted, canonical(state_dir)?.join(forge::FORGE_DIR), github))
            .transpose()?;
        if let Some(forge) = &forge {
            broker.set_forge(forge.clone());
        }
        let swarm = match &config.executor {
            ExecutorConfig::Pi {
                provider: name,
                swarm: Some(swarm),
                ..
            } => Some(SwarmExecutor::new(SwarmParts {
                harness: harness.clone(),
                provider,
                broker: broker.clone(),
                templates: GoalTemplates::load(swarm, name)?,
                tickets_dir: tickets_dir.clone(),
                state_dir: state_dir.to_owned(),
                node: node.clone(),
                pi_root: pi_root.clone(),
                max_runs: swarm.max_runs,
                progress_every: swarm_executor::progress_every(
                    config
                        .watchdog
                        .as_ref()
                        .map(|watchdog| watchdog.thresholds().stalled_ms),
                ),
            })?),
            _ => None,
        };
        let router = match &config.router {
            RouterConfig::Scripted { .. } => None,
            RouterConfig::Pi { max_batch, .. } => {
                let (spec, grant) = router_session(config, state_dir, &node, &pi_root)?;
                broker.admit(spec.token.clone(), grant)?;
                // Its first session starts with the first card, by which time the socket is bound.
                Some(Arc::new(PiRouter::start(harness.clone(), spec, *max_batch)))
            }
        };
        let executor = match (&config.executor, worker_model) {
            (
                ExecutorConfig::Pi {
                    max_workers,
                    prepare,
                    harness: default_harness,
                    claude,
                    ..
                },
                Some(model),
            ) => Some(Arc::new(PiExecutor::new(PiExecutorParts {
                harness,
                claude: claude
                    .as_deref()
                    .map(claude_harness)
                    .transpose()?
                    .map(Arc::new),
                default_harness: *default_harness,
                broker: broker.clone(),
                tickets_dir,
                broker_socket: state_dir.join(broker::SOCKET_FILE),
                node,
                pi_root,
                model,
                max_workers: *max_workers,
                prepare: prepare.clone(),
                forge: forge.clone(),
                swarm,
            })?)),
            _ => None,
        };
        Ok(Self {
            broker,
            forge,
            router,
            executor,
            browsers,
        })
    }

    /// Serve the broker on the state directory's socket.
    fn serve(self, state_dir: &Path) -> Result<Served<H>> {
        let (listener, socket) = broker::bind(state_dir)?;
        let app = self.broker.router()?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        Ok(Served {
            reaper: broker::browser::spawn_reaper(&self.broker),
            broker: self.broker,
            router: self.router,
            executor: self.executor,
            browsers: self.browsers,
            server,
            socket,
        })
    }
}

fn local_browsers(
    config: &AgentConfig,
    browser: &crate::browse::BrowserConfig,
    state_dir: &Path,
    readable: &crate::vault::SandboxView,
    vault: Option<Arc<Vault>>,
    launcher: Launcher,
) -> Result<(Arc<Browsers>, BrowserParts)> {
    let state_dir = canonical(state_dir)?;
    let signer = TrailSigner::load(&browser.audit_key, readable).context("browser.audit_key")?;
    let runtime_dir = match &browser.runtime_dir {
        Some(dir) => dir.clone(),
        None => state_dir.join(BROWSER_DIR),
    };
    readable
        .hides(&runtime_dir)
        .context("browser.runtime_dir")?;
    let blob_dir = config.trail.root(&state_dir);
    readable.hides(&blob_dir).context("trail.path")?;
    let blobs = trail::BlobStore::open(&blob_dir, &config.trail).context("trail")?;
    let browsers = Arc::new(
        Browsers::new(
            browser,
            environment::LOCAL,
            &runtime_dir,
            vault,
            signer,
            launcher,
        )
        .context("browser")?,
    );
    let place = Arc::new(LocalBrowser::new(browsers.clone()));
    let idle = Duration::from_secs(browser.idle_secs);
    Ok((browsers, BrowserParts::new(place, blobs, idle)))
}

/// The `claude-code` harness of `install`, its paths resolved. Its sandbox
/// runs this binary for the session's tools unless `install` names another.
fn claude_harness(install: &ClaudeConfig) -> Result<ClaudeCodeHarness> {
    let solos_swarm = match &install.solos_swarm {
        Some(path) => path.clone(),
        None => std::env::current_exe().context("cannot find this solos-swarm binary")?,
    };
    Ok(ClaudeCodeHarness {
        binary: canonical(&install.binary)?,
        node: install.node.as_deref().map(canonical).transpose()?,
        solos_swarm: canonical(&solos_swarm)?,
    })
}

/// The workers' `run_code` sandbox, with every scratch directory under the
/// Tickets' directory.
fn code_config(run_code: &RunCodeConfig, tickets_dir: &Path, tree: Option<&Path>) -> CodeConfig {
    let mut code = CodeConfig::new(
        tickets_dir.to_owned(),
        exposure::run_code_mounts(run_code, tree),
    );
    code.uv_root = run_code.uv_root.clone();
    code.concurrency = run_code.concurrency;
    code.memory_bytes = run_code.memory_bytes;
    code
}

/// The Router's session and what the broker admits its token to.
fn router_session(
    config: &AgentConfig,
    state_dir: &Path,
    node: &Path,
    pi_root: &Path,
) -> Result<(SessionSpec, Grant)> {
    let RouterConfig::Pi {
        provider,
        model,
        prices,
        max_input_bytes,
        max_output_tokens,
        max_batch: _,
        instructions,
        rules,
    } = &config.router
    else {
        bail!("the router is not a pi router");
    };
    let upstream = upstream_profile(provider, model)?;
    let half_an_eval = Duration::from_millis(config.policy.eval_timeout_ms / 2);
    let dir = state_dir.join(ROUTER_DIR);
    private_dir(&dir)?;
    let settings = dir.join("pi-settings.json");
    // Compaction keeps a long-lived session inside the model's window.
    write_json(
        &settings,
        &json!({"retry":{"enabled":false},"compaction":{"enabled":true,"reserveTokens":65_536},"quietStartup":true}),
    )?;
    let spec = SessionSpec {
        id: format!("router-{}", now_ms()),
        role: Role::Router,
        grants: ROUTER_TOOLS.map(String::from).to_vec(),
        token: broker::new_token(),
        model: upstream.model.clone(),
        run_dir: dir,
        broker_socket: state_dir.join(broker::SOCKET_FILE),
        pi_settings: settings,
        node: node.to_owned(),
        pi_root: pi_root.to_owned(),
        pstack: None,
    };
    let grant = Grant {
        principal: Principal::Router,
        account: broker::Account::Router {
            daily_microusd: config.budgets.router_daily_microusd,
        },
        terms: ModelTerms {
            upstream,
            prices: *prices,
            max_input_bytes: *max_input_bytes,
            max_output_tokens: *max_output_tokens,
            call_limit: Some(half_an_eval),
            temperature: None,
        },
        system_prompt: Some(
            pi_router::system_prompt(
                instructions.as_deref(),
                &config.policy,
                rules,
                &DecideTerms::for_config(config),
            ) + &config.environment_note(),
        ),
    };
    Ok((spec, grant))
}

fn canonical(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("cannot find {}", path.display()))
}

struct Served<H> {
    broker: AgentBroker,
    router: Option<Arc<PiRouter>>,
    executor: Option<Arc<PiExecutor<H>>>,
    browsers: Option<Arc<Browsers>>,
    reaper: Option<tokio::task::JoinHandle<()>>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    socket: PathBuf,
}

impl<H> Served<H>
where
    H: AgentHarness + Send + Sync + 'static,
    H::Session: Send + 'static,
{
    /// End every session, then the broker. The Engine is not told about
    /// the workers; the next start recovers their Assignments.
    async fn stop(self) {
        if let Some(reaper) = &self.reaper {
            reaper.abort();
        }
        if let Some(router) = &self.router {
            router.shutdown().await;
        }
        if let Some(executor) = &self.executor {
            executor.shutdown().await;
        }
        if let Some(browsers) = &self.browsers {
            browsers.shutdown().await;
        }
        self.server.abort();
        let _ = self.server.await;
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// The Sources and Channels the config names, opened before anything else
/// can fail, since opening them reads and deletes the Slack and Discord token
/// files and the GitHub App's key.
struct Io {
    sources: Vec<Box<dyn Source>>,
    channels: BTreeMap<String, Arc<dyn Channel>>,
    /// The App the github Source, the github Channel and the forge share.
    github: Option<Arc<GithubApp>>,
}

/// Refused: the GitHub App's key sits under a path some sandbox can read.
/// Judged before the key is read, with what is known then: the config's
/// mounts and the state, Pi and node directories.
fn check_key_path(
    config: &AgentConfig,
    state_dir: &Path,
    pi_root: Option<&Path>,
    node: Option<&Path>,
) -> Result<()> {
    let Some(github) = &config.github else {
        return Ok(());
    };
    let canonical = |path: &Path| path.canonicalize().ok();
    let host = Host {
        state_dir: canonical(state_dir),
        pi_root: pi_root.and_then(canonical),
        node: node.and_then(canonical),
        pstack: None,
    };
    Exposure::new(config, &host)
        .view()
        .hides(&github.key_file)
        .context("github.key_file")
}

impl Io {
    /// `follow`: Sources keep reading what arrives later. Sources keep their
    /// read positions in `state_dir`.
    fn open(config: &AgentConfig, state_dir: &Path, follow: bool) -> Result<Self> {
        let mut sources: Vec<Box<dyn Source>> = Vec::new();
        let mut slack_api = None;
        let mut discord_api = None;
        let github = match &config.github {
            None => None,
            Some(github) => {
                let repos = config
                    .forge
                    .iter()
                    .flat_map(|forge| forge.repos().keys().cloned())
                    .collect();
                Some(Arc::new(GithubApp::open(
                    github,
                    repos,
                    state_dir,
                    config.github_logins(),
                )?))
            }
        };
        for source in &config.sources {
            match source {
                SourceConfig::Jsonl { path } => {
                    sources.push(Box::new(JsonlSource::new(path, follow)));
                }
                SourceConfig::Slack {
                    channels,
                    app_token_file,
                    bot_token_file,
                    api_base,
                } => {
                    let app = Token::take(app_token_file, "xapp-");
                    let bot = Token::take(bot_token_file, "xoxb-");
                    let api = Arc::new(SlackApi::new(api_base, bot?)?);
                    let slack = SlackSource::new(api.clone(), app?, channels.clone(), follow);
                    sources.push(Box::new(slack));
                    slack_api = Some(api);
                }
                SourceConfig::Discord {
                    guilds,
                    channels,
                    token_file,
                    api_base,
                } => {
                    let token = discord::Token::take(token_file)?;
                    let api = Arc::new(DiscordApi::new(api_base, token)?);
                    let discord =
                        DiscordSource::new(api.clone(), guilds.clone(), channels.clone(), follow);
                    sources.push(Box::new(discord));
                    discord_api = Some(api);
                }
                SourceConfig::Github {} => {
                    let (Some(app), Some(settings)) = (&github, &config.github) else {
                        bail!("the github source needs the github: section");
                    };
                    let repos = config
                        .forge
                        .iter()
                        .flat_map(|forge| forge.repos().keys().cloned());
                    let cursor = state_dir.join(github::CURSOR_FILE);
                    let source = GithubSource::new(app.api.clone(), repos, cursor, follow)
                        .with_poll(Duration::from_secs(settings.poll_secs));
                    sources.push(Box::new(source));
                }
                SourceConfig::Schedule { schedules } => {
                    let schedules = schedules
                        .iter()
                        .map(config::ScheduleConfig::schedule)
                        .collect::<Result<_>>()?;
                    sources.push(Box::new(ScheduleSource::new(schedules, follow)));
                }
                SourceConfig::Mcp {} => {
                    let inbox = mcp::open_inbox(state_dir)?;
                    sources.push(Box::new(JsonlSource::new(inbox, follow)));
                    let commands = mcp::open_commands(state_dir)?;
                    sources.push(Box::new(mcp::CommandSource::new(commands, follow)));
                }
            }
        }
        let mut channels: BTreeMap<String, Arc<dyn Channel>> = BTreeMap::new();
        // Replies link to the Ticket's page on the dashboard, when there is one.
        let links = match &config.web {
            Some(web) => TicketLinks::new(&web.public_url)?,
            None => TicketLinks::none(),
        };
        for channel in &config.channels {
            let adapter: Arc<dyn Channel> = match channel {
                ChannelConfig::Jsonl { path, .. } => Arc::new(JsonlChannel::open(path)?),
                ChannelConfig::Slack {} => Arc::new(
                    SlackChannel::new(
                        slack_api
                            .clone()
                            .context("the slack channel needs a slack source")?,
                    )
                    .with_links(links.clone()),
                ),
                ChannelConfig::Discord {} => Arc::new(
                    DiscordChannel::new(
                        discord_api
                            .clone()
                            .context("the discord channel needs a discord source")?,
                    )
                    .with_links(links.clone()),
                ),
                ChannelConfig::Github {} => Arc::new(
                    GithubChannel::new(
                        github
                            .as_ref()
                            .map(|app| app.api.clone())
                            .context("the github channel needs the github: section")?,
                    )
                    .with_links(links.clone()),
                ),
                ChannelConfig::Mcp {} => {
                    mcp::open_dir(state_dir)?;
                    let outbox = JsonlChannel::open(mcp::outbox(state_dir))?;
                    Arc::new(McpChannel::new(outbox, links.clone()))
                }
            };
            channels.insert(channel.source().to_owned(), adapter);
        }
        Ok(Self {
            sources,
            channels,
            github,
        })
    }

    /// The runtime's adapters: these, `router` and `executor`.
    fn adapters(self, router: Arc<dyn Router>, executor: Arc<dyn Executor>) -> Adapters {
        Adapters {
            sources: self.sources,
            channels: self.channels,
            router,
            executor,
        }
    }
}

/// SIGINT or SIGTERM.
pub async fn shutdown() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        Ok(()) = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
}
