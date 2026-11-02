//! The agent harness port: how a mode starts an agent session, prompts it and
//! stops it, whichever program runs the session. `PiHarness` is the adapter
//! swarm runs launch.
use crate::broker::now_ms;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    future::Future,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::mpsc,
    task::JoinHandle,
};

/// What a session reports, whatever harness runs it. The end of a session's
/// stream is not an event: it arrives as an `Err` on the channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// The session finished its turn and waits for the next prompt.
    TurnEnded,
    /// The harness refused a command; the text is its reason.
    Rejected(String),
}

/// One event, tagged with the id of the session that sent it.
pub type Observation = (String, Result<SessionEvent>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Coordinator,
    Worker,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Coordinator => "coordinator",
            Role::Worker => "worker",
        }
    }
}

/// Everything one session needs to start. No `Debug`: it carries a bearer token.
#[derive(Clone)]
pub struct SessionSpec {
    pub id: String,
    pub role: Role,
    pub grants: Vec<String>,
    /// The bearer token the broker admitted this session with.
    pub token: String,
    pub model: String,
    /// Where the session's transcript and stderr are written.
    pub run_dir: PathBuf,
    pub broker_socket: PathBuf,
    pub pi_settings: PathBuf,
    /// The node binary. Its install root is what gets mounted.
    pub node: PathBuf,
    /// The checkout that holds `node_modules` and `extensions`.
    pub pi_root: PathBuf,
}

pub trait AgentHarness {
    type Session: Session;
    /// Start one session. Its events, tagged with `spec.id`, go to `events`.
    fn start(
        &self,
        spec: &SessionSpec,
        events: mpsc::Sender<Observation>,
    ) -> impl Future<Output = Result<Self::Session>> + Send;
}

pub trait Session {
    fn id(&self) -> &str;
    /// Send one prompt. The end of the turn arrives as `SessionEvent::TurnEnded`.
    fn prompt(&mut self, text: &str) -> impl Future<Output = Result<()>> + Send;
    /// Kill the session and wait up to `STOP_WAIT` for it to exit; true if it
    /// did. Readers are left to `join`, so a caller stopping several sessions
    /// kills all of them before it waits on any reader.
    fn stop(&mut self) -> impl Future<Output = Result<bool>> + Send;
    /// Wait up to `JOIN_WAIT` for each of the session's readers. Returns how
    /// many did not finish cleanly.
    fn join(self) -> impl Future<Output = usize> + Send;
}

pub const STOP_WAIT: Duration = Duration::from_secs(5);
pub const JOIN_WAIT: Duration = Duration::from_secs(2);
const RPC_LINE_LIMIT: usize = 16_777_216;
const STDERR_LINE_LIMIT: usize = 65_536;
const STDERR_LIMIT: usize = 1_048_576;

/// Pi in RPC mode inside bubblewrap.
pub struct PiHarness;

impl PiHarness {
    /// The single documented worker isolation boundary: every input is a
    /// distinct mount or identity.
    pub fn command(spec: &SessionSpec) -> Command {
        let node = spec.node.as_path();
        let mut cmd = Command::new("bwrap");
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args([
                "--unshare-all",
                "--die-with-parent",
                "--new-session",
                "--ro-bind",
                "/usr",
                "/usr",
                // Name resolution and CA material are a read-only binding rather than
                // a borrowing from the host environment, so a clearenv worker still
                // resolves and verifies a live target.
                "--ro-bind-try",
                "/etc",
                "/etc",
                // The loader directories are bound rather than symlinked: on a usr-merged
                // host /lib is itself a symlink, so replacing it with one would hide the
                // real libraries from a dynamically linked node and fail as ENOENT.
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
            ])
            // `node` is the binary path; its install root is what gets bound, so the
            // worker can exec an absolute binary path and still resolve node_modules
            // beside it under the same /runtime/node prefix.
            .arg("--ro-bind")
            .arg(node.parent().and_then(Path::parent).unwrap_or(node))
            .arg("/runtime/node")
            .arg("--ro-bind")
            .arg(spec.pi_root.join("node_modules"))
            .arg("/runtime/node_modules")
            .arg("--ro-bind")
            .arg(spec.pi_root.join("extensions"))
            .arg("/runtime/extensions")
            .arg("--ro-bind")
            .arg(&spec.broker_socket)
            .arg("/broker.sock")
            .arg("--ro-bind")
            .arg(&spec.pi_settings)
            .arg("/work/config/settings.json")
            .args([
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
            ])
            .arg(&spec.token)
            .args(["--setenv", "SWARM_ROLE"])
            .arg(spec.role.as_str())
            .args(["--setenv", "SWARM_GRANTS"])
            .arg(spec.grants.join(","))
            .args([
                "--chdir",
                "/work",
                // The explicit binary path, not the directory: a directory exec needs
                // the kernel to resolve the name through PATH inside the mount
                // namespace, which fails EACCES on some host/container kernels.
                "/runtime/node/bin/node",
                "/runtime/node_modules/@earendil-works/pi-coding-agent/dist/cli.js",
                "--offline",
                "--mode",
                "rpc",
                "--provider",
                "swarm",
                "--model",
            ])
            .arg(&spec.model)
            .args([
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
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }
}

impl AgentHarness for PiHarness {
    type Session = PiSession;

    async fn start(
        &self,
        spec: &SessionSpec,
        events: mpsc::Sender<Observation>,
    ) -> Result<PiSession> {
        let mut child = Self::command(spec).spawn()?;
        let stdout = child.stdout.take().context("missing Pi stdout")?;
        let stderr = child.stderr.take().context("missing Pi stderr")?;
        let stdin = child.stdin.take().context("missing Pi stdin")?;
        let dir = &spec.run_dir;
        let readers = vec![
            tokio::spawn(transcript(
                stdout,
                dir.join(format!("{}.rpc.jsonl", spec.id)),
                spec.id.clone(),
                events,
            )),
            tokio::spawn(stderr_log(stderr, dir.join(format!("{}.stderr", spec.id)))),
        ];
        Ok(PiSession {
            id: spec.id.clone(),
            child,
            stdin,
            readers,
        })
    }
}

pub struct PiSession {
    id: String,
    child: Child,
    stdin: ChildStdin,
    readers: Vec<JoinHandle<Result<()>>>,
}

impl Session for PiSession {
    fn id(&self) -> &str {
        &self.id
    }

    async fn prompt(&mut self, text: &str) -> Result<()> {
        let message = json!({"id":format!("prompt-{}",now_ms()),"type":"prompt","message":text});
        self.stdin.write_all(&serde_json::to_vec(&message)?).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn stop(&mut self) -> Result<bool> {
        if self.child.try_wait()?.is_none() {
            self.child.start_kill()?;
        }
        Ok(matches!(
            tokio::time::timeout(STOP_WAIT, self.child.wait()).await,
            Ok(Ok(_))
        ))
    }

    async fn join(self) -> usize {
        let mut unclean = 0;
        for mut reader in self.readers {
            match tokio::time::timeout(JOIN_WAIT, &mut reader).await {
                Ok(Ok(Ok(()))) => (),
                Err(_) => {
                    unclean += 1;
                    reader.abort();
                    let _ = reader.await;
                }
                _ => {
                    unclean += 1;
                }
            }
        }
        unclean
    }
}

/// The two Pi RPC frames a mode acts on. Everything else stays in the transcript only.
fn event(frame: &Value) -> Option<SessionEvent> {
    if frame["type"] == "response" && frame["success"] == false {
        Some(SessionEvent::Rejected(frame["error"].to_string()))
    } else if frame["type"] == "agent_settled" {
        Some(SessionEvent::TurnEnded)
    } else {
        None
    }
}

/// Copy Pi's stdout to `path` verbatim and forward its events. The end of the
/// stream, clean or not, is sent as an `Err`.
async fn transcript(
    stdout: ChildStdout,
    path: PathBuf,
    who: String,
    send: mpsc::Sender<Observation>,
) -> Result<()> {
    let result = async {
        let mut file = tokio::fs::File::create(path).await?;
        let mut reader = BufReader::new(stdout);
        while let Some(line) = bounded_line(&mut reader, RPC_LINE_LIMIT).await? {
            file.write_all(&line).await?;
            // A line that is not JSON is tolerated rather than fatal: a
            // crash-truncated or interleaved write must not cost the whole
            // worker its transcript, which is also the evidence.
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if let Some(event) = event(&value) {
                send.send((who.clone(), Ok(event))).await.ok();
            }
        }
        file.sync_all().await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    match result {
        Ok(()) => {
            send.send((who, Err(anyhow::anyhow!("Pi process exited"))))
                .await
                .ok();
            Ok(())
        }
        Err(error) => {
            send.send((who, Err(anyhow::anyhow!(error.to_string()))))
                .await
                .ok();
            Err(error)
        }
    }
}

async fn stderr_log(stderr: ChildStderr, path: PathBuf) -> Result<()> {
    let mut reader = BufReader::new(stderr);
    let mut file = tokio::fs::File::create(path).await?;
    let mut total = 0;
    while let Some(line) = bounded_line(&mut reader, STDERR_LINE_LIMIT).await? {
        total += line.len();
        ensure!(total <= STDERR_LIMIT, "Pi stderr limit");
        file.write_all(&line).await?;
    }
    file.sync_all().await?;
    Ok(())
}

/// One newline-terminated line of at most `limit` bytes, or the unterminated
/// tail at end of stream.
pub async fn bounded_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    limit: usize,
) -> Result<Option<Vec<u8>>> {
    let mut result = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(if result.is_empty() {
                None
            } else {
                Some(result)
            });
        }
        let n = buf
            .iter()
            .position(|c| *c == b'\n')
            .map_or(buf.len(), |p| p + 1);
        ensure!(
            result.len() + n <= limit,
            "Pi protocol line exceeds byte limit"
        );
        let finished = buf[n - 1] == b'\n';
        result.extend_from_slice(&buf[..n]);
        reader.consume(n);
        if finished {
            return Ok(Some(result));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_settled_agent_and_a_refused_command_become_events() {
        assert_eq!(
            event(&json!({"type":"agent_settled"})),
            Some(SessionEvent::TurnEnded)
        );
        assert_eq!(
            event(&json!({"type":"response","success":false,"error":"busy"})),
            Some(SessionEvent::Rejected(r#""busy""#.into()))
        );
        assert_eq!(
            event(&json!({"type":"response","success":false})),
            Some(SessionEvent::Rejected("null".into()))
        );
        assert_eq!(event(&json!({"type":"response","success":true})), None);
        assert_eq!(event(&json!({"type":"response"})), None);
        assert_eq!(event(&json!({"type":"agent_end"})), None);
    }
}
