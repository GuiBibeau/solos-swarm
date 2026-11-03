//! The agent's side of its nodes: a TLS listener the configured nodes dial,
//! each admitted by its pinned certificate and its name, and the live-node
//! table. Each node is an [`ExecutionEnvironment`]: a command starts as a
//! job on the node, and a task here relays its output and its end into the
//! same [`Started`] a local command gives. A node that is not connected is
//! [`Offline`]; one that drops fails its commands at once.

use super::tls::{self, Fingerprint, Identity};
use super::wire::{self, Answer, Call, Hello, MAX_FRAME, PROTOCOL, Reply, Request, Welcome};
use crate::agent::config::NodesConfig;
use crate::agent::environment::{ExecutionEnvironment, Offline, Work};
use crate::code::{Ended, Feed, MAX_CODE_BYTES, OutputBounds, Started, Unread};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;

const HANDSHAKE_WAIT: Duration = Duration::from_secs(10);
const MAX_HANDSHAKES: usize = 16;
/// The longest a node may take to answer a call that does not wait.
const CALL_WAIT: Duration = Duration::from_secs(30);
/// How long each poll of a running job waits on the node for its end.
const POLL_MS: u64 = 2_000;
const PING_EVERY: Duration = Duration::from_secs(15);
/// Pings answer at least this often, so a longer silence is a lost node.
const IDLE_WAIT: Duration = Duration::from_secs(45);
/// What a request frame adds around its call.
const FRAME_SLACK: usize = 64;

/// Listens for nodes and keeps one live connection per node.
pub struct Listener {
    addr: SocketAddr,
    nodes: BTreeMap<String, Arc<RemoteNode>>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Listener {
    pub async fn start(config: &NodesConfig) -> Result<Self> {
        let identity =
            Identity::load(&config.cert, &config.key).context("nodes.cert, nodes.key")?;
        let allowed = config.allow.values().map(|node| node.fingerprint).collect();
        let acceptor = TlsAcceptor::from(tls::server_config(&identity, allowed)?);
        let listener = TcpListener::bind(config.listen)
            .await
            .with_context(|| format!("cannot listen for nodes on {}", config.listen))?;
        let addr = listener.local_addr()?;
        let nodes: BTreeMap<String, Arc<RemoteNode>> = config
            .allow
            .iter()
            .map(|(name, allowed)| {
                let node = RemoteNode::new(name.clone(), allowed.fingerprint);
                (name.clone(), Arc::new(node))
            })
            .collect();
        let (stop, stopping) = watch::channel(false);
        let task = tokio::spawn(accept(listener, acceptor, nodes.clone(), stopping));
        Ok(Self {
            addr,
            nodes,
            stop,
            task,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Each configured node, by name, connected or not.
    pub fn environments(&self) -> BTreeMap<String, Arc<dyn ExecutionEnvironment>> {
        self.nodes
            .iter()
            .map(|(name, node)| {
                let node: Arc<dyn ExecutionEnvironment> = node.clone();
                (name.clone(), node)
            })
            .collect()
    }

    /// Stop listening and drop every node's connection, which ends what
    /// they run.
    pub async fn stop(self) {
        self.stop.send_replace(true);
        let _ = self.task.await;
        for node in self.nodes.values() {
            if let Some(link) = node.slot().take() {
                link.close("the agent is stopping");
            }
        }
    }
}

async fn accept(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    nodes: BTreeMap<String, Arc<RemoteNode>>,
    mut stopping: watch::Receiver<bool>,
) {
    let handshakes = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    let mut admitting = JoinSet::new();
    loop {
        tokio::select! {
            () = stopped(&mut stopping) => break,
            accepted = listener.accept() => {
                let (tcp, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(err) => {
                        eprintln!("agent: accepting a node failed: {err}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                // Unauthenticated peers get a bounded share of the listener.
                let Ok(permit) = handshakes.clone().try_acquire_owned() else {
                    continue;
                };
                let (acceptor, nodes) = (acceptor.clone(), nodes.clone());
                admitting.spawn(async move {
                    if let Err(err) = admit(tcp, acceptor, &nodes, permit).await {
                        eprintln!("agent: refused a node connection from {peer}: {err:#}");
                    }
                });
            }
            Some(_) = admitting.join_next(), if !admitting.is_empty() => {}
        }
    }
    admitting.abort_all();
}

/// A stop was asked for, or the listener is gone.
async fn stopped(stopping: &mut watch::Receiver<bool>) {
    let _ = stopping.wait_for(|stop| *stop).await;
}

/// Prove the peer is a configured node, by its certificate and the name it
/// gives, and make it that node's live connection.
async fn admit(
    tcp: TcpStream,
    acceptor: TlsAcceptor,
    nodes: &BTreeMap<String, Arc<RemoteNode>>,
    permit: OwnedSemaphorePermit,
) -> Result<()> {
    tcp.set_nodelay(true)?;
    let peer = tcp.peer_addr()?;
    let tls = tokio::time::timeout(HANDSHAKE_WAIT, acceptor.accept(tcp))
        .await
        .context("timed out in the TLS handshake")?
        .context("the TLS handshake failed")?;
    let fingerprint = tls::peer(tls.get_ref().1).context("the node sent no certificate")?;
    let (mut reader, mut writer) = tokio::io::split(tls);
    let hello: Hello = tokio::time::timeout(HANDSHAKE_WAIT, wire::receive(&mut reader))
        .await
        .context("the node sent no hello")??
        .context("the node closed the connection before its hello")?;
    let node = nodes
        .values()
        .find(|node| node.fingerprint == fingerprint)
        .context("no node has this certificate")?;
    let refusal = if hello.name != node.name {
        let claimed: String = hello.name.chars().take(64).collect();
        Some(format!(
            "this certificate is node {}'s, not {claimed}'s",
            node.name
        ))
    } else if hello.protocol != PROTOCOL {
        Some(format!(
            "this agent speaks protocol {PROTOCOL}, not {}",
            hello.protocol
        ))
    } else {
        None
    };
    if let Some(why) = refusal {
        let _ = wire::send(&mut writer, &Welcome::Refused { why: why.clone() }).await;
        anyhow::bail!(why);
    }
    wire::send(&mut writer, &Welcome::Accepted).await?;
    drop(permit);
    node.attach(reader, writer);
    eprintln!("agent: node {} connected from {peer}", node.name);
    Ok(())
}

/// A configured node, and its connection while it has one.
pub struct RemoteNode {
    name: String,
    fingerprint: Fingerprint,
    link: Mutex<Option<Arc<Link>>>,
    jobs: AtomicU64,
}

impl RemoteNode {
    fn new(name: String, fingerprint: Fingerprint) -> Self {
        Self {
            name,
            fingerprint,
            link: Mutex::new(None),
            jobs: AtomicU64::new(1),
        }
    }

    fn slot(&self) -> MutexGuard<'_, Option<Arc<Link>>> {
        self.link.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn live(&self) -> Result<Arc<Link>, Offline> {
        self.slot()
            .as_ref()
            .filter(|link| link.up())
            .cloned()
            .ok_or_else(|| {
                Offline(format!(
                    "node {} is offline; its commands run once it connects again",
                    self.name
                ))
            })
    }

    /// Serve a newly admitted connection. A connection it had before ends.
    fn attach<R, W>(&self, reader: R, writer: W)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (frames, outgoing) = mpsc::channel(64);
        let link = Arc::new(Link {
            name: self.name.clone(),
            frames,
            pending: Mutex::default(),
            seq: AtomicU64::new(1),
            down: watch::Sender::new(None),
        });
        tokio::spawn(read(link.clone(), reader));
        tokio::spawn(write(link.clone(), writer, outgoing));
        tokio::spawn(ping(link.clone()));
        if let Some(old) = self.slot().replace(link) {
            old.close("the node connected again");
        }
    }

    fn start(&self, call: Call, bounds: OutputBounds) -> Result<Started> {
        let link = self.live()?;
        let size = serde_json::to_vec(&call)?.len();
        ensure!(
            size.saturating_add(FRAME_SLACK) <= MAX_FRAME,
            "this request is {size} bytes; a node takes at most {MAX_FRAME}"
        );
        let (started, feed) = Started::fed(bounds);
        tokio::spawn(relay(link, call, feed));
        Ok(started)
    }

    fn job_id(&self) -> u64 {
        self.jobs.fetch_add(1, Ordering::Relaxed)
    }
}

impl ExecutionEnvironment for RemoteNode {
    fn ready(&self) -> Result<(), Offline> {
        self.live().map(drop)
    }

    fn run_code(&self, work: Work<'_>, args: &Value) -> Result<Started> {
        let call = Call::Run {
            job: self.job_id(),
            work: work.ticket.as_str().to_owned(),
            args: args.clone(),
        };
        self.start(call, OutputBounds::RUN_CODE)
    }

    fn job(
        &self,
        work: Work<'_>,
        code: &str,
        timeout: Duration,
        bounds: OutputBounds,
    ) -> Result<Started> {
        ensure!(!code.trim().is_empty(), "run_code code is empty");
        ensure!(
            code.len() <= MAX_CODE_BYTES,
            "run_code code exceeds {MAX_CODE_BYTES} bytes"
        );
        let call = Call::JobStart {
            job: self.job_id(),
            work: work.ticket.as_str().to_owned(),
            code: code.to_owned(),
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            stdout_bytes: bounds.stdout_bytes,
            stderr_bytes: bounds.stderr_bytes,
        };
        self.start(call, bounds)
    }
}

/// Start `call` on the node, then poll it, passing on its output, a stop
/// and its end.
async fn relay(link: Arc<Link>, call: Call, mut feed: Feed) {
    let ended = follow(&link, call, &mut feed).await;
    feed.end(ended);
}

async fn follow(link: &Link, call: Call, feed: &mut Feed) -> Ended {
    let job = match &call {
        Call::Run { job, .. } | Call::JobStart { job, .. } => *job,
        _ => return Ended::Failed("not a command".into()),
    };
    match link.call(call, CALL_WAIT).await {
        Ok(Answer::Accepted) => {}
        Ok(Answer::Refused { why }) => return Ended::Failed(why),
        Ok(other) => return Ended::Failed(unexpected(&link.name, &other)),
        Err(gone) => return Ended::Failed(gone),
    }
    let mut stop_sent = false;
    loop {
        let wait = Call::JobWait {
            job,
            wait_ms: POLL_MS,
        };
        let wait = link.call(wait, CALL_WAIT + Duration::from_millis(POLL_MS));
        tokio::pin!(wait);
        let answer = loop {
            tokio::select! {
                answer = &mut wait => break answer,
                () = feed.stopping(), if !stop_sent => {
                    stop_sent = true;
                    // The poll that follows takes the stopped end.
                    if let Err(gone) = link.call(Call::JobStop { job }, CALL_WAIT).await {
                        break Err(gone);
                    }
                }
            }
        };
        match answer {
            Ok(Answer::Progress {
                running,
                stdout,
                stderr,
                truncated,
            }) => {
                feed.running(running);
                feed.push(&Unread {
                    stdout,
                    stderr,
                    truncated,
                });
            }
            Ok(Answer::Ended(end)) => return end.into(),
            Ok(Answer::Refused { why }) => return Ended::Failed(why),
            Ok(other) => return Ended::Failed(unexpected(&link.name, &other)),
            Err(gone) => return Ended::Failed(gone),
        }
    }
}

fn unexpected(name: &str, answer: &Answer) -> String {
    let kind = match answer {
        Answer::Accepted => "accepted",
        Answer::Progress { .. } => "progress",
        Answer::Ended(_) => "ended",
        Answer::Refused { .. } => "refused",
        Answer::Pong => "pong",
    };
    format!("node {name} answered out of turn ({kind})")
}

/// One connection to a node. Calls wait for their own reply by `seq`; when
/// the connection ends, every waiting call fails.
struct Link {
    name: String,
    frames: mpsc::Sender<Vec<u8>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Answer>>>,
    seq: AtomicU64,
    /// Why it ended, once it has.
    down: watch::Sender<Option<String>>,
}

/// A call waiting for its reply. Dropping it forgets the call.
struct Waiting<'a> {
    link: &'a Link,
    seq: u64,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.link.pending().remove(&self.seq);
    }
}

impl Link {
    fn pending(&self) -> MutexGuard<'_, HashMap<u64, oneshot::Sender<Answer>>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn up(&self) -> bool {
        self.down.borrow().is_none()
    }

    fn gone(&self) -> String {
        let why = self.down.borrow().clone().unwrap_or_default();
        format!("node {} went offline: {why}", self.name)
    }

    /// End the connection, once, and fail every call that waits on it.
    fn close(&self, why: &str) {
        let closed = self.down.send_if_modified(|down| {
            if down.is_some() {
                return false;
            }
            *down = Some(why.to_owned());
            true
        });
        if closed {
            eprintln!("agent: node {} went offline: {why}", self.name);
            self.pending().clear();
        }
    }

    async fn ended(&self) {
        let mut down = self.down.subscribe();
        let _ = down.wait_for(Option::is_some).await;
    }

    /// Send `call` and wait up to `limit` for its answer. A node that does
    /// not answer in time is treated as gone.
    async fn call(&self, call: Call, limit: Duration) -> Result<Answer, String> {
        if !self.up() {
            return Err(self.gone());
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let frame = wire::encode(&Request { seq, call }).map_err(|err| format!("{err:#}"))?;
        let (answer, answered) = oneshot::channel();
        self.pending().insert(seq, answer);
        let _waiting = Waiting { link: self, seq };
        let exchange = async {
            self.frames.send(frame).await.map_err(|_| self.gone())?;
            answered.await.map_err(|_| self.gone())
        };
        match tokio::time::timeout(limit, exchange).await {
            Ok(answer) => answer,
            Err(_) => {
                self.close("it did not answer in time");
                Err(self.gone())
            }
        }
    }
}

async fn read<R: AsyncRead + Unpin>(link: Arc<Link>, mut reader: R) {
    let why = loop {
        let frame = tokio::select! {
            frame = tokio::time::timeout(IDLE_WAIT, wire::receive::<_, Reply>(&mut reader)) => frame,
            () = link.ended() => return,
        };
        match frame {
            Err(_) => break "it went quiet".to_owned(),
            Ok(Err(err)) => break format!("{err:#}"),
            Ok(Ok(None)) => break "it closed the connection".to_owned(),
            Ok(Ok(Some(Reply { seq, answer }))) => {
                if let Some(waiting) = link.pending().remove(&seq) {
                    let _ = waiting.send(answer);
                }
            }
        }
    };
    link.close(&why);
}

async fn write<W: AsyncWrite + Unpin>(
    link: Arc<Link>,
    mut writer: W,
    mut outgoing: mpsc::Receiver<Vec<u8>>,
) {
    loop {
        let frame = tokio::select! {
            frame = outgoing.recv() => frame,
            () = link.ended() => break,
        };
        let Some(frame) = frame else { break };
        let written = async {
            writer.write_all(&frame).await?;
            writer.flush().await
        };
        if let Err(err) = written.await {
            link.close(&format!("writing to it failed: {err}"));
            break;
        }
    }
    let _ = writer.shutdown().await;
}

async fn ping(link: Arc<Link>) {
    loop {
        tokio::select! {
            () = tokio::time::sleep(PING_EVERY) => {}
            () = link.ended() => return,
        }
        if link.call(Call::Ping, CALL_WAIT).await.is_err() {
            return;
        }
    }
}
