//! `solos-swarm agent publish`: a sidecar that makes the agent's tickets
//! readable from a dashboard on the internet while the agent's box accepts no
//! inbound connection. Every few seconds it looks at `agent.db` (read-only,
//! see [`source`]), builds one JSON snapshot per ticket and an index of them
//! ([`snapshot`]), and uploads the snapshots whose content changed to a
//! private Vercel Blob store over HTTPS ([`blob`]). The dashboard
//! (`web/agent-dashboard`) reads them from there behind its own sign-in.
//!
//! It only ever reads the Store. What it remembers is one small file in the
//! state directory, [`STATE_FILE`]: the hash of what each blob last held, so
//! a restart uploads nothing that is already there. An upload is a put at a
//! fixed pathname that replaces the blob, so doing one twice is harmless.
//!
//! One more thing is read, and it is not the Store: the file where the
//! Watchdog keeps what it found about the external jobs it watches
//! ([`external_jobs::STATE_FILE`]). It goes up as [`JOBS_PATH`] unchanged,
//! when its content changes and on every heartbeat.

mod blob;
pub mod snapshot;
mod source;

pub use blob::{API_VERSION, Backoff, BlobClient, BlobToken, Put, PutError};
pub use source::{Built, ENDED_IN_INDEX, Source};

use super::config::{AgentConfig, PublishConfig};
use super::external_jobs;
use crate::broker::now_ms;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use snapshot::{Index, TicketSnapshot, iso};
use solos_agent_core::model::Ms;
use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What the publisher remembers, in the state directory.
pub const STATE_FILE: &str = "publish-state.json";
/// The index's pathname in the Blob store.
pub const INDEX_PATH: &str = "tickets/index.json";
/// The external jobs' pathname: what [`external_jobs::State`] holds.
pub const JOBS_PATH: &str = "jobs/index.json";
const CONTENT_TYPE: &str = "application/json";
/// A failing publisher waits longer between looks, up to this.
const MAX_WAIT: Duration = Duration::from_secs(300);

/// The pathname of one ticket's snapshot.
pub fn ticket_path(id: &str) -> String {
    format!("tickets/{id}.json")
}

/// Ids the Store makes are `tk_` and hex; one that is anything else is not
/// made into a pathname.
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// How the publisher runs.
#[derive(Clone, Debug)]
pub struct Options {
    pub every: Duration,
    pub heartbeat: Duration,
    pub api_url: String,
    pub backoff: Backoff,
}

impl Options {
    pub fn of(config: &PublishConfig) -> Self {
        Self {
            every: Duration::from_secs(config.every_secs),
            heartbeat: Duration::from_secs(config.heartbeat_secs),
            api_url: config.api_url.clone(),
            backoff: Backoff::default(),
        }
    }
}

/// Why a look failed.
#[derive(Debug)]
pub enum CycleError {
    /// The token is refused: the publisher stops, since a restart with the
    /// same token fails the same way.
    Fatal(anyhow::Error),
    /// The next look tries again.
    Retry(anyhow::Error),
}

impl std::fmt::Display for CycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fatal(err) | Self::Retry(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for CycleError {}

/// What one look did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// The Store had changed, so the snapshots were built again.
    pub built: bool,
    /// Pathnames uploaded, tickets first and the index last.
    pub uploaded: Vec<String>,
    /// Snapshots whose content was already in the store.
    pub unchanged: usize,
    /// Pathnames the API refused, which are not tried again until their
    /// content changes.
    pub rejected: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    v: u32,
    /// The Blob store the hashes are about; another store has none of them.
    store: String,
    /// SHA-256 of what each pathname last held (the index's without its
    /// `generated_at`).
    hashes: BTreeMap<String, String>,
    /// When the index was last uploaded.
    index_ms: Ms,
    /// When the external jobs were last uploaded.
    #[serde(default)]
    jobs_ms: Ms,
}

impl State {
    fn load(path: &Path, store: &str) -> Self {
        let fresh = Self {
            v: 1,
            store: store.to_owned(),
            ..Self::default()
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return fresh;
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(state) if state.v == 1 && state.store == store => state,
            _ => fresh,
        }
    }

    /// Written whole or not at all, readable by the agent's user only.
    fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))
    }
}

/// A blob to put.
struct Upload {
    path: String,
    /// What the state records once it is in.
    hash: String,
    body: Vec<u8>,
    index: bool,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The state directory's Store, looked at and uploaded.
pub struct Publisher {
    state_dir: PathBuf,
    client: BlobClient,
    options: Options,
    state: State,
    source: Option<Source>,
    /// The latest build, which the index's heartbeat is made from.
    built: Option<Built>,
    /// Uploads not done yet, kept across looks so a failure is retried even
    /// when the Store has not changed.
    queue: Vec<Upload>,
    /// The hash of the external jobs the API refused, which are not sent
    /// again until they change.
    refused_jobs: Option<String>,
}

impl Publisher {
    pub fn new(state_dir: &Path, token: BlobToken, options: Options) -> Result<Self> {
        let state = State::load(&state_dir.join(STATE_FILE), token.store_id());
        let client = BlobClient::new(&options.api_url, token, options.backoff)?;
        Ok(Self {
            state_dir: state_dir.to_owned(),
            client,
            options,
            state,
            source: None,
            built: None,
            queue: Vec::new(),
            refused_jobs: None,
        })
    }

    /// One look: build the snapshots if the Store changed, then upload what
    /// is queued.
    pub async fn cycle(&mut self) -> Result<Report, CycleError> {
        let now = now_ms();
        let mut report = Report::default();
        if self.source.as_ref().is_some_and(Source::replaced) {
            self.source = None;
            self.built = None;
            self.queue.clear();
        }
        if self.source.is_none() {
            self.source = Some(Source::open(&self.state_dir).map_err(CycleError::Retry)?);
        }
        let source = self.source.as_mut().expect("opened above");
        if source.changed().map_err(CycleError::Retry)? {
            let built = source.build().map_err(CycleError::Retry)?;
            report.built = true;
            report.unchanged = self.plan(&built, now);
            self.built = Some(built);
        }
        self.plan_jobs(now);
        self.heartbeat(now);
        self.flush(now, &mut report).await?;
        Ok(report)
    }

    /// Queue what `built` holds that the store does not. Returns how many
    /// snapshots were already in it.
    fn plan(&mut self, built: &Built, now: Ms) -> usize {
        self.queue.clear();
        let mut unchanged = 0;
        for (id, snapshot) in &built.tickets {
            if !safe_id(id.as_str()) {
                eprintln!("publish: skipping ticket {id}: its id is not safe in a pathname");
                continue;
            }
            let path = ticket_path(id.as_str());
            let Some(upload) = self.upload_of(path, snapshot) else {
                continue;
            };
            if self.state.hashes.get(&upload.path) == Some(&upload.hash) {
                unchanged += 1;
            } else {
                self.queue.push(upload);
            }
        }
        let index = index_upload(&built.index, now);
        if self.state.hashes.get(INDEX_PATH) != Some(&index.hash) {
            self.queue.push(index);
        } else {
            unchanged += 1;
        }
        unchanged
    }

    fn upload_of(&self, path: String, snapshot: &TicketSnapshot) -> Option<Upload> {
        match serde_json::to_vec(snapshot) {
            Ok(body) => Some(Upload {
                hash: hash(&body),
                path,
                body,
                index: false,
            }),
            Err(err) => {
                eprintln!("publish: cannot serialise {path}: {err}");
                None
            }
        }
    }

    fn heartbeat_ms(&self) -> Ms {
        u64::try_from(self.options.heartbeat.as_millis()).unwrap_or(u64::MAX)
    }

    /// Queue the Watchdog's external jobs when their content changed, and
    /// again each heartbeat: the state says when the Watchdog last looked, so
    /// the dashboard can tell a quiet Watchdog from a stopped one. Nothing is
    /// queued where the Watchdog has written no state.
    fn plan_jobs(&mut self, now: Ms) {
        let Some(state) = external_jobs::State::load(&self.state_dir) else {
            return;
        };
        let upload = jobs_upload(&state);
        let changed = self.state.hashes.get(JOBS_PATH) != Some(&upload.hash);
        let due = now.saturating_sub(self.state.jobs_ms) >= self.heartbeat_ms();
        self.queue.retain(|queued| queued.path != JOBS_PATH);
        if (changed || due) && self.refused_jobs.as_ref() != Some(&upload.hash) {
            self.queue.push(upload);
        }
    }

    /// An index not uploaded for a heartbeat is uploaded again, so the
    /// dashboard can tell a quiet agent from a dead publisher.
    fn heartbeat(&mut self, now: Ms) {
        let due = now.saturating_sub(self.state.index_ms) >= self.heartbeat_ms();
        if !due || self.queue.iter().any(|upload| upload.index) {
            return;
        }
        if let Some(built) = &self.built {
            self.queue.push(index_upload(&built.index, now));
        }
    }

    async fn flush(&mut self, now: Ms, report: &mut Report) -> Result<(), CycleError> {
        // Tickets before the index, so the index never names a blob that is
        // not there yet.
        let mut queue = std::mem::take(&mut self.queue);
        queue.sort_by_key(|upload| upload.index);
        let state_path = self.state_dir.join(STATE_FILE);
        let mut uploads = queue.into_iter();
        while let Some(upload) = uploads.next() {
            let put = self
                .client
                .put(&upload.path, CONTENT_TYPE, upload.body.clone())
                .await;
            match put {
                Ok(_) => {
                    self.state
                        .hashes
                        .insert(upload.path.clone(), upload.hash.clone());
                    if upload.index {
                        self.state.index_ms = now;
                    }
                    if upload.path == JOBS_PATH {
                        self.state.jobs_ms = now;
                    }
                    if let Err(err) = self.state.save(&state_path) {
                        self.queue.extend(uploads);
                        return Err(CycleError::Retry(err));
                    }
                    report.uploaded.push(upload.path);
                }
                Err(PutError::Unauthorized(why)) => {
                    self.queue.push(upload);
                    self.queue.extend(uploads);
                    return Err(CycleError::Fatal(anyhow::anyhow!(
                        "the Blob token was refused ({why}); replace it and restart the publisher"
                    )));
                }
                Err(PutError::Transient(why)) => {
                    self.queue.push(upload);
                    self.queue.extend(uploads);
                    return Err(CycleError::Retry(anyhow::anyhow!(why)));
                }
                Err(PutError::Rejected(why)) => {
                    eprintln!("publish: {why}");
                    if upload.path == JOBS_PATH {
                        self.refused_jobs = Some(upload.hash);
                    }
                    report.rejected.push(upload.path);
                }
            }
        }
        Ok(())
    }
}

/// The index as it is uploaded, stamped with `now`; its hash leaves the
/// stamp out.
fn index_upload(index: &Index, now: Ms) -> Upload {
    let unstamped = Index {
        generated_at: None,
        ..index.clone()
    };
    let stamped = Index {
        generated_at: Some(iso(now)),
        ..index.clone()
    };
    Upload {
        path: INDEX_PATH.to_owned(),
        hash: hash(&serde_json::to_vec(&unstamped).unwrap_or_default()),
        body: serde_json::to_vec(&stamped).unwrap_or_default(),
        index: true,
    }
}

/// The external jobs as they are uploaded. The time of the Watchdog's last
/// look moves every minute, so the hash leaves it out.
fn jobs_upload(state: &external_jobs::State) -> Upload {
    let unstamped = external_jobs::State {
        checked_ms: 0,
        ..state.clone()
    };
    Upload {
        path: JOBS_PATH.to_owned(),
        hash: hash(&serde_json::to_vec(&unstamped).unwrap_or_default()),
        body: serde_json::to_vec(state).unwrap_or_default(),
        index: false,
    }
}

/// `solos-swarm agent publish`. With `once`, one look, a summary on stdout,
/// and an error if anything was refused or failed; otherwise it looks until
/// SIGINT or SIGTERM.
pub async fn run(config: &Path, state_dir: &Path, once: bool) -> Result<()> {
    run_until(config, state_dir, once, super::shutdown()).await
}

/// [`run`] until `stop` resolves instead of a signal.
pub async fn run_until(
    config: &Path,
    state_dir: &Path,
    once: bool,
    stop: impl Future<Output = ()>,
) -> Result<()> {
    let config = AgentConfig::load(config)?;
    let Some(publish) = config.web.as_ref().and_then(|web| web.publish.as_ref()) else {
        bail!("the agent config has no web.publish section: there is nothing to publish to");
    };
    let token = BlobToken::take(&publish.token_file).context("web.publish.token_file")?;
    let mut publisher = Publisher::new(state_dir, token, Options::of(publish))?;
    let every = publisher.options.every;
    if once {
        let report = publisher.cycle().await?;
        println!("{}", serde_json::to_string(&report)?);
        if !report.rejected.is_empty() {
            bail!("the Blob API refused {}", report.rejected.join(", "));
        }
        return Ok(());
    }
    tokio::pin!(stop);
    let mut failures = 0u32;
    eprintln!(
        "publish: store {} every {}s, from {}",
        publisher.client.store_id(),
        every.as_secs(),
        state_dir.display()
    );
    loop {
        let cycle = tokio::select! {
            () = &mut stop => return Ok(()),
            cycle = publisher.cycle() => cycle,
        };
        let wait = match cycle {
            Ok(report) => {
                failures = 0;
                if !report.uploaded.is_empty() {
                    eprintln!(
                        "publish: uploaded {} ({} unchanged)",
                        summary(&report.uploaded),
                        report.unchanged
                    );
                }
                every
            }
            Err(CycleError::Fatal(err)) => return Err(err),
            Err(CycleError::Retry(err)) => {
                failures = failures.saturating_add(1);
                let wait = every
                    .saturating_mul(2u32.saturating_pow(failures.min(10)))
                    .min(MAX_WAIT);
                eprintln!(
                    "publish: {err:#}; looking again in {}s",
                    wait.as_secs().max(1)
                );
                wait
            }
        };
        tokio::select! {
            () = &mut stop => return Ok(()),
            () = tokio::time::sleep(wait) => {}
        }
    }
}

fn summary(paths: &[String]) -> String {
    const SHOWN: usize = 4;
    let shown: Vec<&str> = paths.iter().take(SHOWN).map(String::as_str).collect();
    match paths.len().checked_sub(SHOWN) {
        Some(more) if more > 0 => format!("{} and {more} more", shown.join(", ")),
        _ => shown.join(", "),
    }
}
