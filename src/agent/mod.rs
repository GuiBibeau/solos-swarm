//! Agent mode's shell: the async ports, their first adapters, and the
//! runtime that drives `solos_agent_core::Engine` on the SQLite Store.
//! [`serve`] is the composition root, the one place that names concrete
//! adapters.

pub mod broker;
pub mod config;
pub mod jsonl;
pub mod pi_executor;
pub mod pi_router;
pub mod ports;
pub mod runtime;
pub mod scripted;
pub mod slack;

pub use config::AgentConfig;
pub use runtime::{Adapters, RunOptions, Runtime, Summary};

use crate::broker::{now_ms, write_json};
use crate::gateway::{Provider, provider_for, upstream_profile};
use crate::harness::{AgentHarness, PiHarness, Role, SessionSpec};
use anyhow::{Context, Result, bail, ensure};
use broker::{AgentBroker, BrokerParts, DecideTerms, Grant, ModelTerms, Principal, ROUTER_TOOLS};
use config::{ChannelConfig, ExecutorConfig, RouterConfig, SourceConfig};
use jsonl::{JsonlChannel, JsonlSource};
use pi_router::PiRouter;
use ports::{Channel, Executor, Router, Source};
use scripted::{ScriptedExecutor, ScriptedRouter};
use serde_json::json;
use slack::{SlackApi, SlackChannel, SlackSource, Token};
use solos_agent_store::SqliteStore;
use std::collections::BTreeMap;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zeroize::Zeroizing;

/// The Store's file in the state directory.
pub const STORE_FILE: &str = "agent.db";
/// The Router's transcripts, stderr and Pi settings, in the state directory.
pub const ROUTER_DIR: &str = "router";

/// What `agent serve` takes besides its config file. A Pi Router needs the
/// last three; a scripted one needs none.
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

/// Where a Pi Router's sessions run, and the provider its broker calls.
pub struct RouterHost<H> {
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
    let io = Io::open(&config, !until_idle)?;
    let host = match &config.router {
        RouterConfig::Scripted {} => None,
        RouterConfig::Pi {
            provider, model, ..
        } => {
            let (Some(pi_root), Some(node), Some(key)) = (pi_root, node, credential) else {
                bail!("a pi router needs --pi-root, --node and --credential-file");
            };
            ensure!(
                std::env::consts::OS == "linux",
                "the router's isolation requires Linux"
            );
            let upstream = upstream_profile(provider, model)?;
            let provider = provider_for(&upstream, crate::swarm::http_client()?, key)?;
            Some(RouterHost {
                harness: PiHarness,
                provider: Arc::new(provider),
                pi_root,
                node,
            })
        }
    };
    serve_io(config, state_dir, until_idle, io, host).await
}

/// [`serve`] with the Router's harness and provider given, so tests can
/// script them. A Pi Router config needs `host`; a scripted one ignores it.
pub async fn serve_with<H>(
    config: AgentConfig,
    state_dir: &Path,
    until_idle: bool,
    host: Option<RouterHost<H>>,
) -> Result<Summary>
where
    H: AgentHarness + Send + Sync + 'static,
    H::Session: Send + 'static,
{
    let io = Io::open(&config, !until_idle)?;
    serve_io(config, state_dir, until_idle, io, host).await
}

async fn serve_io<H>(
    config: AgentConfig,
    state_dir: &Path,
    until_idle: bool,
    io: Io,
    host: Option<RouterHost<H>>,
) -> Result<Summary>
where
    H: AgentHarness + Send + Sync + 'static,
    H::Session: Send + 'static,
{
    private_dir(state_dir)?;
    let store = SqliteStore::open(state_dir.join(STORE_FILE)).context("cannot open the store")?;
    let (router, pi) = match (&config.router, host) {
        (RouterConfig::Scripted {}, _) => {
            let scripted: Arc<dyn Router> =
                Arc::new(ScriptedRouter::new(config.budgets.session_microusd));
            (scripted, None)
        }
        (RouterConfig::Pi { .. }, None) => bail!("a pi router needs a session host"),
        (RouterConfig::Pi { .. }, Some(host)) => {
            let pi = PiParts::new(&config, state_dir, host)?;
            (pi.router.clone() as Arc<dyn Router>, Some(pi))
        }
    };
    let runtime_options = RunOptions {
        until_idle,
        ..RunOptions::default()
    };
    let runtime = Runtime::new(
        store,
        config.policy.clone(),
        io.adapters(&config, router),
        runtime_options,
    )
    .with_control_dir(state_dir);
    let served = match pi {
        Some(pi) => Some(pi.serve(state_dir, runtime.facts())?),
        None => None,
    };
    let summary = runtime.run(shutdown()).await;
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

/// The Pi Router before its broker serves: the session adapter, the token
/// it was given, and what the broker admits that token to.
struct PiParts {
    router: Arc<PiRouter>,
    token: String,
    grant: Grant,
    provider: Arc<dyn Provider>,
    decide: DecideTerms,
}

impl PiParts {
    fn new<H>(config: &AgentConfig, state_dir: &Path, host: RouterHost<H>) -> Result<Self>
    where
        H: AgentHarness + Send + Sync + 'static,
        H::Session: Send + 'static,
    {
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
        let decide = DecideTerms::for_config(config);
        let dir = state_dir.join(ROUTER_DIR);
        private_dir(&dir)?;
        let settings = dir.join("pi-settings.json");
        // Compaction keeps a long-lived session inside the model's window.
        write_json(
            &settings,
            &json!({"retry":{"enabled":false},"compaction":{"enabled":true,"reserveTokens":65_536},"quietStartup":true}),
        )?;
        let token = broker::new_token();
        let spec = SessionSpec {
            id: format!("router-{}", now_ms()),
            role: Role::Router,
            grants: ROUTER_TOOLS.map(String::from).to_vec(),
            token: token.clone(),
            model: upstream.model.clone(),
            run_dir: dir,
            broker_socket: state_dir.join(broker::SOCKET_FILE),
            pi_settings: settings,
            node: canonical(&host.node)?,
            pi_root: canonical(&host.pi_root)?,
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
                decide,
            )),
        };
        Ok(Self {
            router: Arc::new(PiRouter::start(host.harness, spec)),
            token,
            grant,
            provider: host.provider,
            decide,
        })
    }

    /// Admit the Router's token and serve the broker on the state
    /// directory's socket. The Router starts its first session only when a
    /// card arrives, by which time the socket is bound.
    fn serve(self, state_dir: &Path, facts: ports::FactSink) -> Result<Served> {
        let broker = AgentBroker::new(BrokerParts {
            dir: state_dir.to_owned(),
            store: SqliteStore::open(state_dir.join(STORE_FILE))
                .context("cannot open the store for the broker")?,
            provider: self.provider,
            facts,
            decide: self.decide,
            workers: None,
        })?;
        broker.admit(self.token, self.grant)?;
        let (listener, socket) = broker::bind(state_dir)?;
        let app = broker.router()?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        Ok(Served {
            router: self.router,
            server,
            socket,
        })
    }
}

fn canonical(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("cannot find {}", path.display()))
}

struct Served {
    router: Arc<PiRouter>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    socket: PathBuf,
}

impl Served {
    async fn stop(self) {
        self.router.shutdown().await;
        self.server.abort();
        let _ = self.server.await;
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// The Sources and Channels the config names, opened before anything else
/// can fail, since opening them reads and deletes the Slack token files.
struct Io {
    sources: Vec<Box<dyn Source>>,
    channels: BTreeMap<String, Arc<dyn Channel>>,
}

impl Io {
    /// `follow`: Sources keep reading what arrives later.
    fn open(config: &AgentConfig, follow: bool) -> Result<Self> {
        let mut sources: Vec<Box<dyn Source>> = Vec::new();
        let mut slack_api = None;
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
            };
            channels.insert(channel.source().to_owned(), adapter);
        }
        Ok(Self { sources, channels })
    }

    /// The runtime's adapters: these, the Executor the config names, and
    /// `router`.
    fn adapters(self, config: &AgentConfig, router: Arc<dyn Router>) -> Adapters {
        let executor: Arc<dyn Executor> = match config.executor {
            ExecutorConfig::Scripted {} => Arc::new(ScriptedExecutor),
        };
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
