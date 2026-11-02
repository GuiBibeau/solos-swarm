//! Agent mode's shell: the async ports, their first adapters, and the
//! runtime that drives `solos_agent_core::Engine` on the SQLite Store.
//! [`serve`] is the composition root, the one place that names concrete
//! adapters.

pub mod config;
pub mod jsonl;
pub mod ports;
pub mod runtime;
pub mod scripted;
pub mod slack;

pub use config::AgentConfig;
pub use runtime::{Adapters, RunOptions, Runtime, Summary};

use anyhow::{Context, Result};
use config::{ChannelConfig, ExecutorConfig, RouterConfig, SourceConfig};
use jsonl::{JsonlChannel, JsonlSource};
use ports::{Channel, Executor, Router, Source};
use scripted::{ScriptedExecutor, ScriptedRouter};
use slack::{SlackApi, SlackChannel, SlackSource, Token};
use solos_agent_store::SqliteStore;
use std::collections::BTreeMap;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::sync::Arc;

/// The Store's file in the state directory.
pub const STORE_FILE: &str = "agent.db";

/// `solos-swarm agent serve`. With `until_idle`, Sources read what is there
/// and the run ends when nothing is left to do; otherwise it serves until
/// SIGINT or SIGTERM.
pub async fn serve(config: &Path, state_dir: &Path, until_idle: bool) -> Result<Summary> {
    let config = AgentConfig::load(config)?;
    // First, so the token files are consumed before anything else can fail.
    let adapters = adapters(&config, !until_idle)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir)
        .with_context(|| format!("cannot create {}", state_dir.display()))?;
    let store = SqliteStore::open(state_dir.join(STORE_FILE)).context("cannot open the store")?;
    let options = RunOptions {
        until_idle,
        ..RunOptions::default()
    };
    Runtime::new(store, config.policy, adapters, options)
        .run(shutdown())
        .await
}

/// `follow`: Sources keep reading what arrives later. Reads and deletes the
/// Slack token files.
pub fn adapters(config: &AgentConfig, follow: bool) -> Result<Adapters> {
    let mut sources: Vec<Box<dyn Source>> = Vec::new();
    let mut slack_api = None;
    for source in &config.sources {
        match source {
            SourceConfig::Jsonl { path } => sources.push(Box::new(JsonlSource::new(path, follow))),
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
    let router: Arc<dyn Router> = match config.router {
        RouterConfig::Scripted {} => Arc::new(ScriptedRouter::new(config.budgets.session_microusd)),
    };
    let executor: Arc<dyn Executor> = match config.executor {
        ExecutorConfig::Scripted {} => Arc::new(ScriptedExecutor),
    };
    Ok(Adapters {
        sources,
        channels,
        router,
        executor,
    })
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
