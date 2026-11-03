//! `solos-swarm node`: a daemon on a user's machine. It dials out to the
//! agent, so a machine behind NAT works, over TLS where each side pins the
//! other's certificate. It runs what the agent sends in its own bwrap
//! sandbox, the same [`CodeExecutor`] the agent's host uses, with the mounts
//! and secrets of its own config, and answers with `run_code`'s receipts.
//! It holds no session state: a job lives on one connection and dies with
//! it. Secret values stay here; every copy a command prints is redacted
//! before it is sent.

use super::redact::{MIN_SECRET_BYTES, Secrets, Stream};
use super::tls::{self, Fingerprint, Identity};
use super::wire::{
    self, Answer, Call, End, Hello, MAX_WAIT_MS, PROTOCOL, Reply, Request, Welcome, valid_work,
};
use crate::code::{
    self, CodeConfig, CodeExecutor, Ended, Mount, OutputBounds, Secret, Started, Unread,
};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_rustls::TlsConnector;
use zeroize::Zeroizing;

const CONNECT_WAIT: Duration = Duration::from_secs(10);
const HELLO_WAIT: Duration = Duration::from_secs(10);
/// The agent pings every 15 s, so this long without a frame means it is gone.
const IDLE_WAIT: Duration = Duration::from_secs(60);
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// A connection that lasted this long retries from the first wait again.
const RETRY_RESET: Duration = Duration::from_secs(30);
const MAX_JOBS: usize = 64;
const MAX_CALLS: usize = 256;
const MAX_JOB_OUTPUT_BYTES: usize = 1_048_576;
/// Where file secrets appear in the sandbox.
pub const SECRETS_DIR: &str = "/secrets";
/// Variables the sandbox sets itself (`code.rs`), which a secret would not
/// survive.
const SANDBOX_ENV: [&str; 17] = [
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "PYTHONNOUSERSITE",
    "PYTHONDONTWRITEBYTECODE",
    "PYTHONUNBUFFERED",
    "PYTHONPATH",
    "MPLBACKEND",
    "OPENBLAS_NUM_THREADS",
    "OMP_NUM_THREADS",
    "MKL_NUM_THREADS",
    "NUMEXPR_NUM_THREADS",
    "VECLIB_MAXIMUM_THREADS",
    "RAYON_NUM_THREADS",
    "CUDA_VISIBLE_DEVICES",
];

/// `node.yaml`. Relative paths are relative to its directory.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// This node's name in the agent's `nodes.allow`.
    pub name: String,
    pub agent: AgentEndpoint,
    /// From `solos-swarm node init`.
    pub cert: PathBuf,
    pub key: PathBuf,
    /// Each Ticket's `/work` is a directory under it, kept between commands.
    pub work_root: PathBuf,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEndpoint {
    /// `host:port` of the agent's `nodes.listen`.
    pub address: String,
    /// The agent's certificate, as `solos-swarm agent init-nodes` printed it.
    pub fingerprint: Fingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Read-only binds, each `{target: /name, source: /absolute/path}`.
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub uv_root: Option<PathBuf>,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_memory")]
    pub memory_bytes: u64,
    /// The longest a background job may run, in seconds.
    #[serde(default = "default_job_max_secs")]
    pub job_max_secs: u64,
    #[serde(default = "default_bwrap")]
    pub bwrap: PathBuf,
    #[serde(default = "default_prlimit")]
    pub prlimit: PathBuf,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mounts: Vec::new(),
            uv_root: None,
            concurrency: default_concurrency(),
            memory_bytes: default_memory(),
            job_max_secs: default_job_max_secs(),
            bwrap: default_bwrap(),
            prlimit: default_prlimit(),
        }
    }
}

fn default_concurrency() -> usize {
    code::DEFAULT_CONCURRENCY
}
fn default_memory() -> u64 {
    code::DEFAULT_MEMORY_BYTES
}
fn default_job_max_secs() -> u64 {
    86_400
}
fn default_bwrap() -> PathBuf {
    PathBuf::from("/usr/bin/bwrap")
}
fn default_prlimit() -> PathBuf {
    PathBuf::from("/usr/bin/prlimit")
}

/// Secrets the sandbox may use, each read from a file on this machine.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    /// Variables, by name: the file's text without its trailing newline.
    #[serde(default)]
    pub env: BTreeMap<String, PathBuf>,
    /// Files bound read-only at `/secrets/<name>`.
    #[serde(default)]
    pub files: BTreeMap<String, PathBuf>,
}

fn word(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl NodeConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let mut config: Self = serde_yaml::from_slice(&bytes)
            .with_context(|| format!("invalid node config {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        for file in [&mut config.cert, &mut config.key, &mut config.work_root] {
            *file = base.join(&*file);
        }
        let secrets = &mut config.secrets;
        for file in secrets.env.values_mut().chain(secrets.files.values_mut()) {
            *file = base.join(&*file);
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            word(&self.name) && self.name != crate::agent::environment::LOCAL,
            "node name {:?} is not a short word other than local",
            self.name
        );
        ensure!(
            self.agent.address.rsplit_once(':').is_some(),
            "agent.address {:?} is not host:port",
            self.agent.address
        );
        let sandbox = &self.sandbox;
        let mut targets = BTreeSet::new();
        for mount in &sandbox.mounts {
            mount.validate().context("sandbox")?;
            ensure!(
                targets.insert(&mount.target) && mount.target != Path::new(SECRETS_DIR),
                "sandbox mounts {} twice, or over {SECRETS_DIR}",
                mount.target.display()
            );
        }
        ensure!(
            sandbox
                .uv_root
                .as_ref()
                .is_none_or(|root| root.is_absolute()),
            "sandbox.uv_root must be an absolute path"
        );
        ensure!(
            code::CONCURRENCY_RANGE.contains(&sandbox.concurrency),
            "sandbox.concurrency must be 1..=8"
        );
        ensure!(
            code::MEMORY_RANGE.contains(&sandbox.memory_bytes),
            "sandbox.memory_bytes must be 256 MiB..=16 GiB"
        );
        ensure!(
            (code::MAX_TIMEOUT_SECONDS..=86_400).contains(&sandbox.job_max_secs),
            "sandbox.job_max_secs must be {}..=86400",
            code::MAX_TIMEOUT_SECONDS
        );
        for name in self.secrets.env.keys() {
            ensure!(
                name.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
                    && name
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                "secrets.env name {name:?} is not an environment variable name"
            );
            ensure!(
                !SANDBOX_ENV.contains(&name.as_str()),
                "secrets.env {name} is a variable the sandbox sets itself"
            );
        }
        for name in self.secrets.files.keys() {
            ensure!(
                word(name),
                "secrets.files name {name:?} is not a short word"
            );
        }
        Ok(())
    }
}

/// A secret file's text, as the redaction looks for it.
fn read_secret(name: &str, path: &Path) -> Result<Zeroizing<String>> {
    let text = Zeroizing::new(
        std::fs::read_to_string(path)
            .with_context(|| format!("cannot read secret {name} from {}", path.display()))?,
    );
    let value = Zeroizing::new(text.trim_end_matches(['\n', '\r']).to_owned());
    ensure!(
        value.len() >= MIN_SECRET_BYTES,
        "secret {name} is shorter than {MIN_SECRET_BYTES} bytes, too short to redact safely"
    );
    ensure!(!value.contains('\0'), "secret {name} holds a NUL byte");
    Ok(value)
}

/// What every connection shares.
struct Host {
    code: Arc<CodeExecutor>,
    secrets: Secrets,
    work_root: PathBuf,
    job_max: Duration,
}

impl Host {
    /// The Ticket's `/work` under the work root, made on first use.
    fn work_dir(&self, work: &str) -> Result<PathBuf> {
        ensure!(valid_work(work), "{work:?} cannot name a work directory");
        let dir = self.work_root.join(work);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        Ok(dir)
    }

    fn redact_end(&self, streams: &mut Streams, ended: Ended) -> End {
        match ended {
            Ended::Ran {
                mut receipt,
                stopped,
            } => {
                for (stream, name) in [(&mut streams.out, "stdout"), (&mut streams.err, "stderr")] {
                    let text = receipt[name].as_str().unwrap_or_default().to_owned();
                    receipt[name] = stream.finish(&self.secrets, &text).into();
                }
                End::Ran { receipt, stopped }
            }
            Ended::Failed(why) => End::Failed {
                why: self.secrets.redact(&why),
            },
        }
    }
}

/// The node: its identity, its sandbox and its secrets.
pub struct Node {
    name: String,
    address: String,
    tls: Arc<rustls::ClientConfig>,
    host: Arc<Host>,
}

impl Node {
    pub fn new(config: &NodeConfig) -> Result<Self> {
        config.validate()?;
        let identity = Identity::load(&config.cert, &config.key)?;
        let tls = tls::client_config(&identity, config.agent.fingerprint)?;
        let mut values = Vec::new();
        let mut env = Vec::new();
        for (name, path) in &config.secrets.env {
            let value = read_secret(name, path)?;
            env.push((name.clone(), Secret::new(value.to_string())));
            values.push((name.clone(), value));
        }
        let mut files = Vec::new();
        for (name, path) in &config.secrets.files {
            values.push((name.clone(), read_secret(name, path)?));
            files.push(Mount {
                target: Path::new(SECRETS_DIR).join(name),
                source: path.clone(),
            });
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&config.work_root)
            .with_context(|| format!("cannot create {}", config.work_root.display()))?;
        let sandbox = &config.sandbox;
        let mut code = CodeConfig::new(config.work_root.clone(), sandbox.mounts.clone());
        code.uv_root = sandbox.uv_root.clone();
        code.concurrency = sandbox.concurrency;
        code.memory_bytes = sandbox.memory_bytes;
        code.bwrap = sandbox.bwrap.clone();
        code.prlimit = sandbox.prlimit.clone();
        code.files = files;
        code.env = env;
        let code = CodeExecutor::from_config(code).context("the node's sandbox")?;
        Ok(Self {
            name: config.name.clone(),
            address: config.agent.address.clone(),
            tls,
            host: Arc::new(Host {
                code: Arc::new(code),
                secrets: Secrets::new(values),
                work_root: config.work_root.clone(),
                job_max: Duration::from_secs(sandbox.job_max_secs),
            }),
        })
    }

    /// Serve the agent until `stop`, dialing again whenever the connection
    /// ends or fails, after a wait that grows while it keeps failing.
    pub async fn serve(&self, stop: impl Future<Output = ()>) -> Result<()> {
        tokio::pin!(stop);
        let mut wait = RETRY_FIRST;
        loop {
            let began = Instant::now();
            let ended = tokio::select! {
                ended = self.session() => ended,
                () = &mut stop => return Ok(()),
            };
            match ended {
                Ok(()) => eprintln!("node: the agent closed the connection"),
                Err(err) => eprintln!("node: {err:#}"),
            }
            if began.elapsed() >= RETRY_RESET {
                wait = RETRY_FIRST;
            }
            eprintln!("node: dialing the agent again in {}s", wait.as_secs());
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = &mut stop => return Ok(()),
            }
            wait = wait.saturating_mul(2).min(RETRY_MAX);
        }
    }

    /// One connection: dial, prove who each side is, and serve until it
    /// ends. An error before the agent's welcome is a refusal.
    pub async fn session(&self) -> Result<()> {
        let tcp = tokio::time::timeout(CONNECT_WAIT, TcpStream::connect(&self.address))
            .await
            .with_context(|| format!("timed out dialing {}", self.address))?
            .with_context(|| format!("cannot dial {}", self.address))?;
        tcp.set_nodelay(true)?;
        let tls = tokio::time::timeout(
            CONNECT_WAIT,
            TlsConnector::from(self.tls.clone()).connect(tls::agent_name()?, tcp),
        )
        .await
        .context("timed out in the TLS handshake")?
        .context("the TLS handshake with the agent failed")?;
        let (mut reader, mut writer) = tokio::io::split(tls);
        let hello = Hello {
            name: self.name.clone(),
            protocol: PROTOCOL,
        };
        wire::send(&mut writer, &hello).await?;
        let welcome = tokio::time::timeout(HELLO_WAIT, wire::receive(&mut reader))
            .await
            .context("the agent did not answer the hello")??;
        match welcome {
            Some(Welcome::Accepted) => {}
            Some(Welcome::Refused { why }) => bail!("the agent refused this node: {why}"),
            None => bail!("the agent closed the connection before it welcomed this node"),
        }
        eprintln!(
            "node: {} connected to the agent at {}",
            self.name, self.address
        );
        serve_connection(self.host.clone(), reader, writer).await
    }
}

/// One job, on one connection.
struct Job {
    sandbox: Started,
    streams: Mutex<Streams>,
}

#[derive(Default)]
struct Streams {
    out: Stream,
    err: Stream,
}

impl Job {
    fn streams(&self) -> MutexGuard<'_, Streams> {
        self.streams.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

type Jobs = Arc<Mutex<HashMap<u64, Arc<Job>>>>;

fn jobs(table: &Jobs) -> MutexGuard<'_, HashMap<u64, Arc<Job>>> {
    table.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn serve_connection<R, W>(host: Arc<Host>, mut reader: R, mut writer: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let table: Jobs = Arc::default();
    let (replies, mut outgoing) = mpsc::channel::<Vec<u8>>(64);
    let mut calls = JoinSet::new();
    let reading = async {
        loop {
            let request = tokio::time::timeout(IDLE_WAIT, wire::receive::<_, Request>(&mut reader))
                .await
                .context("the agent went quiet")??;
            let Some(Request { seq, call }) = request else {
                return Ok(());
            };
            while calls.try_join_next().is_some() {}
            if calls.len() >= MAX_CALLS {
                let busy = Answer::Refused {
                    why: "the node is answering its most calls at once".into(),
                };
                if let Ok(frame) = wire::encode(&Reply { seq, answer: busy }) {
                    let _ = replies.send(frame).await;
                }
                continue;
            }
            let (host, table, replies) = (host.clone(), table.clone(), replies.clone());
            calls.spawn(async move {
                let answer = answer(&host, &table, call).await;
                let frame = wire::encode(&Reply { seq, answer }).or_else(|err| {
                    let why = format!("the answer could not be sent: {err:#}");
                    wire::encode(&Reply {
                        seq,
                        answer: Answer::Refused { why },
                    })
                });
                if let Ok(frame) = frame {
                    let _ = replies.send(frame).await;
                }
            });
        }
    };
    let writing = async {
        while let Some(frame) = outgoing.recv().await {
            writer.write_all(&frame).await?;
            writer.flush().await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let ended = tokio::select! {
        ended = reading => ended,
        ended = writing => ended,
    };
    calls.abort_all();
    // No session outlives its connection: every job is stopped now.
    for job in std::mem::take(&mut *jobs(&table)).into_values() {
        job.sandbox.stop();
    }
    ended
}

async fn answer(host: &Host, table: &Jobs, call: Call) -> Answer {
    let refused = |why: String| Answer::Refused {
        why: host.secrets.redact(&why),
    };
    match call {
        Call::Ping => Answer::Pong,
        Call::Run { job, work, args } => start(host, table, job, &work, |dir| {
            host.code.start_in(dir, &args)
        })
        .unwrap_or_else(|err| refused(format!("{err:#}"))),
        Call::JobStart {
            job,
            work,
            code,
            timeout_ms,
            stdout_bytes,
            stderr_bytes,
        } => {
            let timeout = Duration::from_millis(timeout_ms);
            if timeout.is_zero() || timeout > host.job_max {
                return refused(format!(
                    "this node runs a job for at most {} s",
                    host.job_max.as_secs()
                ));
            }
            if stdout_bytes.max(stderr_bytes) > MAX_JOB_OUTPUT_BYTES {
                return refused(format!(
                    "this node keeps at most {MAX_JOB_OUTPUT_BYTES} bytes of a stream"
                ));
            }
            let bounds = OutputBounds {
                stdout_bytes,
                stderr_bytes,
            };
            start(host, table, job, &work, |dir| {
                host.code.start_job_in(dir, &code, timeout, bounds)
            })
            .unwrap_or_else(|err| refused(format!("{err:#}")))
        }
        Call::JobWait { job, wait_ms } => {
            let Some(kept) = jobs(table).get(&job).cloned() else {
                return refused(format!("no job {job} runs on this connection"));
            };
            let wait = Duration::from_millis(wait_ms.min(MAX_WAIT_MS));
            let ended = kept.sandbox.wait(wait).await;
            let mut streams = kept.streams();
            match ended {
                Some(ended) => {
                    jobs(table).remove(&job);
                    Answer::Ended(host.redact_end(&mut streams, ended))
                }
                None => {
                    let unread = kept.sandbox.read();
                    let unread = Unread {
                        stdout: streams.out.send(&host.secrets, &unread.stdout),
                        stderr: streams.err.send(&host.secrets, &unread.stderr),
                        truncated: unread.truncated,
                    };
                    Answer::progress(kept.sandbox.running(), unread)
                }
            }
        }
        Call::JobStop { job } => match jobs(table).get(&job) {
            Some(kept) => {
                kept.sandbox.stop();
                Answer::Accepted
            }
            None => refused(format!("no job {job} runs on this connection")),
        },
    }
}

fn start(
    host: &Host,
    table: &Jobs,
    job: u64,
    work: &str,
    begin: impl FnOnce(&Path) -> Result<Started>,
) -> Result<Answer> {
    let mut table = jobs(table);
    ensure!(!table.contains_key(&job), "job {job} already runs");
    ensure!(
        table.len() < MAX_JOBS,
        "this node holds {MAX_JOBS} jobs, its most"
    );
    let sandbox = begin(&host.work_dir(work)?)?;
    table.insert(
        job,
        Arc::new(Job {
            sandbox,
            streams: Mutex::default(),
        }),
    );
    Ok(Answer::Accepted)
}
