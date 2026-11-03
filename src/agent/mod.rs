//! Agent mode's shell: the async ports, their first adapters, and the
//! runtime that drives `solos_agent_core::Engine` on the SQLite Store.
//! [`serve`] is the composition root, the one place that names concrete
//! adapters.

pub mod broker;
pub mod config;
pub mod github;
pub mod goal_template;
pub mod jobs;
pub mod jsonl;
pub mod mcp;
pub mod pi_executor;
pub mod pi_router;
pub mod ports;
pub mod runtime;
pub mod schedule;
pub mod scripted;
pub mod slack;
pub mod status;
pub mod swarm_executor;
pub mod utc;
pub mod watchdog;

pub use config::AgentConfig;
pub use runtime::{Adapters, HOST_RESTARTED, RunOptions, Runtime, Summary};

use crate::broker::{now_ms, write_json};
use crate::code::{CodeConfig, CodeExecutor};
use crate::gateway::{Provider, provider_for, upstream_profile};
use crate::harness::claude_code::ClaudeCodeHarness;
use crate::harness::{AgentHarness, PiHarness, Role, SessionSpec};
use anyhow::{Context, Result, bail, ensure};
use broker::{
    AgentBroker, BrokerParts, DecideTerms, Grant, ModelTerms, Principal, ROUTER_TOOLS, WorkerParts,
};
use config::{
    ChannelConfig, ClaudeConfig, ExecutorConfig, RouterConfig, RunCodeConfig, SourceConfig,
};
use github::{GithubApi, GithubChannel, GithubSource};
use goal_template::GoalTemplates;
use jsonl::{JsonlChannel, JsonlSource};
use pi_executor::{PiExecutor, PiExecutorParts};
use pi_router::PiRouter;
use ports::{Channel, Executor, Router, Source};
use schedule::ScheduleSource;
use scripted::{ScriptedExecutor, ScriptedRouter};
use serde_json::json;
use slack::{SlackApi, SlackChannel, SlackSource, Token};
use solos_agent_store::SqliteStore;
use std::collections::BTreeMap;
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
}

/// `solos-swarm agent serve`. With `until_idle`, Sources read what is there
/// and the run ends when nothing is left to do; otherwise it serves until
/// SIGINT or SIGTERM.
pub async fn serve(config: &Path, state_dir: &Path, options: ServeOptions) -> Result<Summary> {
    let config = AgentConfig::load(config)?;
    let ServeOptions {
        until_idle,
        pi_root,
        node,
        credential,
    } = options;
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
            })
        }
    };
    serve_io(config, state_dir, until_idle, io, host).await
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
    let io = Io::open(&config, state_dir, !until_idle)?;
    serve_io(config, state_dir, until_idle, io, host).await
}

async fn serve_io<H>(
    config: AgentConfig,
    state_dir: &Path,
    until_idle: bool,
    io: Io,
    host: Option<PiHost<H>>,
) -> Result<Summary>
where
    H: AgentHarness + Send + Sync + 'static,
    H::Session: Send + 'static,
{
    private_dir(state_dir)?;
    let store = SqliteStore::open(state_dir.join(STORE_FILE)).context("cannot open the store")?;
    let (facts, inbound) = ports::FactSink::channel();
    let pi = match (config.pi_upstream(), host) {
        (None, _) => None,
        (Some(_), None) => bail!("a pi router or pi workers need a session host"),
        (Some(_), Some(host)) => Some(PiParts::new(&config, state_dir, host, facts.clone())?),
    };
    let router: Arc<dyn Router> = match pi.as_ref().and_then(|pi| pi.router.clone()) {
        Some(router) => router,
        None => Arc::new(ScriptedRouter::new(config.budgets.session_microusd)),
    };
    let executor: Arc<dyn Executor> = match pi.as_ref().and_then(|pi| pi.executor.clone()) {
        Some(executor) => executor,
        None => Arc::new(ScriptedExecutor),
    };
    let runtime_options = RunOptions {
        until_idle,
        ..RunOptions::default()
    };
    let watchdog = watchdog::Running::start(&config, state_dir, &io.channels, &facts)?;
    let runtime = Runtime::with_sink(
        store,
        config.policy.clone(),
        io.adapters(router, executor),
        runtime_options,
        (facts, inbound),
    )
    .with_control_dir(state_dir);
    let served = match pi {
        Some(pi) => Some(pi.serve(state_dir)?),
        None => None,
    };
    let summary = runtime.run(shutdown()).await;
    if let Some(watchdog) = watchdog {
        watchdog.stop().await;
    }
    if let Some(served) = served {
        served.stop().await;
    }
    summary
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
    router: Option<Arc<PiRouter>>,
    executor: Option<Arc<PiExecutor<H>>>,
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
    ) -> Result<Self> {
        let PiHost {
            harness,
            provider,
            pi_root,
            node,
        } = host;
        let harness = Arc::new(harness);
        let (pi_root, node) = (canonical(&pi_root)?, canonical(&node)?);
        let tickets_dir = canonical(state_dir)?.join(TICKETS_DIR);
        let workers = match &config.executor {
            ExecutorConfig::Scripted {} => None,
            ExecutorConfig::Pi {
                provider,
                model,
                prices,
                max_input_bytes,
                max_output_tokens,
                run_code,
                ..
            } => {
                let code = match run_code {
                    None => None,
                    Some(run_code) => Some(Arc::new(
                        CodeExecutor::from_config(code_config(run_code, &tickets_dir))
                            .context("executor.run_code")?,
                    )),
                };
                let terms = ModelTerms {
                    upstream: upstream_profile(provider, model)?,
                    prices: *prices,
                    max_input_bytes: *max_input_bytes,
                    max_output_tokens: *max_output_tokens,
                };
                Some(WorkerParts { terms, code })
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
        let broker = AgentBroker::with_jobs(parts, job_limits)?;
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
            RouterConfig::Scripted {} => None,
            RouterConfig::Pi { .. } => {
                let (spec, grant) = router_session(config, state_dir, &node, &pi_root)?;
                broker.admit(spec.token.clone(), grant)?;
                // Its first session starts with the first card, by which time the socket is bound.
                Some(Arc::new(PiRouter::start(harness.clone(), spec)))
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
                swarm,
            })?)),
            _ => None,
        };
        Ok(Self {
            broker,
            router,
            executor,
        })
    }

    /// Serve the broker on the state directory's socket.
    fn serve(self, state_dir: &Path) -> Result<Served<H>> {
        let (listener, socket) = broker::bind(state_dir)?;
        let app = self.broker.router()?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        Ok(Served {
            router: self.router,
            executor: self.executor,
            server,
            socket,
        })
    }
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
fn code_config(run_code: &RunCodeConfig, tickets_dir: &Path) -> CodeConfig {
    let mut code = CodeConfig::new(tickets_dir.to_owned(), run_code.mounts.clone());
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
        instructions,
        rules,
    } = &config.router
    else {
        bail!("the router is not a pi router");
    };
    let daily_microusd = config
        .budgets
        .router_daily_microusd
        .context("a pi router needs budgets.router_daily_microusd")?;
    let upstream = upstream_profile(provider, model)?;
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
    };
    let grant = Grant {
        principal: Principal::Router,
        account: broker::Account::Router { daily_microusd },
        terms: ModelTerms {
            upstream,
            prices: *prices,
            max_input_bytes: *max_input_bytes,
            max_output_tokens: *max_output_tokens,
        },
        system_prompt: Some(pi_router::system_prompt(
            instructions.as_deref(),
            &config.policy,
            rules,
            &DecideTerms::for_config(config),
        )),
    };
    Ok((spec, grant))
}

fn canonical(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("cannot find {}", path.display()))
}

struct Served<H> {
    router: Option<Arc<PiRouter>>,
    executor: Option<Arc<PiExecutor<H>>>,
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
        if let Some(router) = &self.router {
            router.shutdown().await;
        }
        if let Some(executor) = &self.executor {
            executor.shutdown().await;
        }
        self.server.abort();
        let _ = self.server.await;
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// The Sources and Channels the config names, opened before anything else
/// can fail, since opening them reads and deletes the Slack and GitHub token
/// files.
struct Io {
    sources: Vec<Box<dyn Source>>,
    channels: BTreeMap<String, Arc<dyn Channel>>,
}

impl Io {
    /// `follow`: Sources keep reading what arrives later. Sources keep their
    /// read positions in `state_dir`.
    fn open(config: &AgentConfig, state_dir: &Path, follow: bool) -> Result<Self> {
        let mut sources: Vec<Box<dyn Source>> = Vec::new();
        let mut slack_api = None;
        let mut github_api = None;
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
                SourceConfig::Github {
                    repos,
                    token_file,
                    api_base,
                    poll_secs,
                } => {
                    let token = github::Token::take(token_file)?;
                    let api = Arc::new(GithubApi::new(api_base, token)?);
                    let cursor = state_dir.join(github::CURSOR_FILE);
                    let source = GithubSource::new(api.clone(), repos.clone(), cursor, follow)
                        .with_poll(Duration::from_secs(*poll_secs));
                    sources.push(Box::new(source));
                    github_api = Some(api);
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
                }
            }
        }
        let mut channels: BTreeMap<String, Arc<dyn Channel>> = BTreeMap::new();
        for channel in &config.channels {
            let adapter: Arc<dyn Channel> = match channel {
                ChannelConfig::Jsonl { path, .. } => Arc::new(JsonlChannel::open(path)?),
                ChannelConfig::Slack {} => Arc::new(SlackChannel::new(
                    slack_api
                        .clone()
                        .context("the slack channel needs a slack source")?,
                )),
                ChannelConfig::Github {} => Arc::new(GithubChannel::new(
                    github_api
                        .clone()
                        .context("the github channel needs a github source")?,
                )),
                ChannelConfig::Mcp {} => {
                    mcp::open_dir(state_dir)?;
                    Arc::new(JsonlChannel::open(mcp::outbox(state_dir))?)
                }
            };
            channels.insert(channel.source().to_owned(), adapter);
        }
        Ok(Self { sources, channels })
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

async fn shutdown() {
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
