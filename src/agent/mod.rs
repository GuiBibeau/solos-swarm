//! Agent mode's shell: the async ports, their first adapters, and the
//! runtime that drives `solos_agent_core::Engine` on the SQLite Store.
//! [`serve`] is the composition root, the one place that names concrete
//! adapters.

pub mod config;
pub mod jsonl;
pub mod ports;
pub mod runtime;
pub mod scripted;

pub use config::AgentConfig;
pub use runtime::{Adapters, RunOptions, Runtime, Summary};

use anyhow::{Context, Result};
use config::{ChannelConfig, ExecutorConfig, RouterConfig, SourceConfig};
use jsonl::{JsonlChannel, JsonlSource};
use ports::{Channel, Executor, Router, Source};
use scripted::{ScriptedExecutor, ScriptedRouter};
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
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir)
        .with_context(|| format!("cannot create {}", state_dir.display()))?;
    let store = SqliteStore::open(state_dir.join(STORE_FILE)).context("cannot open the store")?;
    let adapters = adapters(&config, !until_idle)?;
    let options = RunOptions {
        until_idle,
        ..RunOptions::default()
    };
    Runtime::new(store, config.policy, adapters, options)
        .run(shutdown())
        .await
}

/// `follow`: Sources keep reading what arrives later.
pub fn adapters(config: &AgentConfig, follow: bool) -> Result<Adapters> {
    let sources = config
        .sources
        .iter()
        .map(|source| -> Box<dyn Source> {
            match source {
                SourceConfig::Jsonl { path } => Box::new(JsonlSource::new(path, follow)),
            }
        })
        .collect();
    let mut channels: BTreeMap<String, Arc<dyn Channel>> = BTreeMap::new();
    for channel in &config.channels {
        let adapter: Arc<dyn Channel> = match channel {
            ChannelConfig::Jsonl { path, .. } => Arc::new(JsonlChannel::open(path)?),
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
