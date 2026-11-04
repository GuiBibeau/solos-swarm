//! Claude Code as a harness: headless `--print` in stream-json mode, inside
//! bubblewrap with no network and a throwaway home. It holds no key. Its
//! model is the broker's Messages facade, reached through `solos-swarm
//! bridge` on a loopback port of the sandbox's own network namespace, with
//! the session's bearer token as its API key. Its built-in tools are off;
//! its only tools are the broker's, served by `solos-swarm mcp` against the
//! broker socket with the session's token file.

use super::{
    AgentHarness, Observation, PiSession, Session, SessionEvent, SessionSpec, frames, stderr_log,
};
use crate::bwrap::Bwrap;
use crate::exposure::{self, CLAUDE, NODE, SOLOS_SWARM};
use crate::gateway::anthropic::MCP_SERVER;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;

/// Where the session's own pieces are inside the sandbox.
const MCP_CONFIG: &str = "/runtime/mcp.json";
const TOKEN: &str = "/runtime/token";
const SOCKET: &str = "/broker.sock";
const HOME: &str = "/home/claude";
/// The Ticket's work directory, `<run dir>/work`, when the session has one.
const WORK: &str = "work";
const SESSION_WAIT: Duration = Duration::from_secs(10);

/// Where Claude Code and the solos-swarm it runs for tools are installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeCodeHarness {
    /// The `claude` executable, or with `node`, the npm package's `cli.js`.
    pub binary: PathBuf,
    /// The node that runs `binary`; without it `binary` runs on its own, as
    /// the native build does.
    pub node: Option<PathBuf>,
    /// The solos-swarm binary the sandbox runs as its MCP server and bridge.
    pub solos_swarm: PathBuf,
}

/// A session's files on the host, bound read-only into its sandbox.
struct Files {
    mcp_config: PathBuf,
    token: PathBuf,
}

impl ClaudeCodeHarness {
    /// The sandbox and its program. Every input is a distinct read-only
    /// mount or variable; only the home and `/tmp` are writable, and both
    /// are tmpfs.
    fn command(&self, spec: &SessionSpec, files: &Files, system_prompt: Option<&str>) -> Command {
        let mut sandbox = Bwrap::new()
            .unshare_all()
            .die_with_parent()
            .new_session()
            .system_binds()
            .proc("/proc")
            .dev("/dev")
            .tmpfs("/tmp")
            .tmpfs(HOME);
        let work = spec.run_dir.join(WORK);
        sandbox = if work.is_dir() {
            sandbox.ro_bind(&work, "/work")
        } else {
            sandbox.dir("/work")
        };
        let mut program = Vec::new();
        match &self.node {
            None => program.push(CLAUDE.to_owned()),
            Some(_) => {
                let name = self
                    .binary
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                program.push(format!("{NODE}/bin/node"));
                program.push(format!("{CLAUDE}/{name}"));
            }
        }
        for bind in exposure::claude_binds(&self.binary, self.node.as_deref(), &self.solos_swarm) {
            sandbox = exposure::read_only(sandbox, &bind);
        }
        let allowed: Vec<String> = spec
            .grants
            .iter()
            .map(|tool| format!("mcp__{MCP_SERVER}__{tool}"))
            .collect();
        let allowed = allowed.join(",");
        program.extend(
            [
                "--print",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--bare",
                "--model",
                spec.model.as_str(),
                "--tools",
                "",
                "--strict-mcp-config",
                "--mcp-config",
                MCP_CONFIG,
                "--permission-mode",
                "dontAsk",
                "--allowedTools",
                allowed.as_str(),
                "--setting-sources",
                "user",
                "--disable-slash-commands",
                "--no-session-persistence",
            ]
            .map(String::from),
        );
        if let Some(prompt) = system_prompt {
            program.extend(["--system-prompt".to_owned(), prompt.to_owned()]);
        }
        let args = sandbox
            .ro_bind(&files.mcp_config, MCP_CONFIG)
            .ro_bind(&files.token, TOKEN)
            .ro_bind(&spec.broker_socket, SOCKET)
            .clearenv()
            .setenv("PATH", "/usr/bin:/bin")
            .setenv("HOME", HOME)
            .setenv("TMPDIR", "/tmp")
            .setenv(
                "ANTHROPIC_BASE_URL",
                format!("http://{}", crate::bridge::SANDBOX_ADDR),
            )
            .setenv("ANTHROPIC_API_KEY", &spec.token)
            // Background calls name the session's model too, so the
            // broker's contract admits them.
            .setenv("ANTHROPIC_DEFAULT_HAIKU_MODEL", &spec.model)
            .setenv("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .setenv("DISABLE_AUTOUPDATER", "1")
            .setenv("DISABLE_TELEMETRY", "1")
            .setenv("DISABLE_ERROR_REPORTING", "1")
            .chdir("/work")
            .exec(
                SOLOS_SWARM,
                [
                    "bridge",
                    "--listen",
                    crate::bridge::SANDBOX_ADDR,
                    "--socket",
                    SOCKET,
                    "--",
                ]
                .into_iter()
                .map(String::from)
                .chain(program),
            );
        let mut cmd = Command::new("bwrap");
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }
}

/// The MCP config: the broker's tools, as one stdio server.
fn mcp_config() -> Value {
    json!({"mcpServers": {MCP_SERVER: {
        "type": "stdio",
        "command": SOLOS_SWARM,
        "args": ["mcp", "--socket", SOCKET, "--token-file", TOKEN],
    }}})
}

/// Write the session's files in its run directory, readable by this user only.
fn write_files(spec: &SessionSpec) -> Result<Files> {
    let files = Files {
        mcp_config: spec.run_dir.join(format!("{}.mcp.json", spec.id)),
        token: spec.run_dir.join(format!("{}.token", spec.id)),
    };
    for (path, bytes) in [
        (&files.mcp_config, serde_json::to_vec(&mcp_config())?),
        (&files.token, spec.token.clone().into_bytes()),
    ] {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut file| file.write_all(&bytes))
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(files)
}

/// The system prompt the broker serves this session at `/session`, as the
/// Pi extension reads it.
async fn system_prompt(spec: &SessionSpec) -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        .unix_socket(spec.broker_socket.clone())
        .timeout(SESSION_WAIT)
        .build()?;
    let response = client
        .post("http://broker/session")
        .bearer_auth(&spec.token)
        .send()
        .await
        .context("the broker did not answer /session")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    ensure!(
        response.status().is_success(),
        "the broker refused /session: HTTP {}",
        response.status()
    );
    let body: Value = response.json().await?;
    Ok(body["system_prompt"].as_str().map(str::to_owned))
}

impl AgentHarness for ClaudeCodeHarness {
    type Session = ClaudeCodeSession;

    async fn start(
        &self,
        spec: &SessionSpec,
        events: mpsc::Sender<Observation>,
    ) -> Result<ClaudeCodeSession> {
        let prompt = system_prompt(spec).await?;
        let files = write_files(spec)?;
        let cleanup = Cleanup(vec![files.mcp_config.clone(), files.token.clone()]);
        let mut child = self.command(spec, &files, prompt.as_deref()).spawn()?;
        let stdout = child.stdout.take().context("missing Claude Code stdout")?;
        let stderr = child.stderr.take().context("missing Claude Code stderr")?;
        let stdin = child.stdin.take().context("missing Claude Code stdin")?;
        let dir = &spec.run_dir;
        let readers = vec![
            tokio::spawn(frames(
                stdout,
                dir.join(format!("{}.claude.jsonl", spec.id)),
                spec.id.clone(),
                events,
                event,
                "Claude Code exited",
            )),
            tokio::spawn(stderr_log(stderr, dir.join(format!("{}.stderr", spec.id)))),
        ];
        Ok(ClaudeCodeSession {
            process: PiSession {
                id: spec.id.clone(),
                child,
                stdin,
                readers,
            },
            _cleanup: cleanup,
        })
    }
}

/// Removes a session's files once it is gone.
struct Cleanup(Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub struct ClaudeCodeSession {
    /// The bwrap process, its stdin and its readers, as a Pi session's.
    process: PiSession,
    _cleanup: Cleanup,
}

impl Session for ClaudeCodeSession {
    fn id(&self) -> &str {
        self.process.id()
    }

    /// One stream-json user message.
    async fn prompt(&mut self, text: &str) -> Result<()> {
        let message = json!({"type": "user", "message": {"role": "user", "content": text}});
        let stdin = &mut self.process.stdin;
        stdin.write_all(&serde_json::to_vec(&message)?).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn stop(&mut self) -> Result<bool> {
        self.process.stop().await
    }

    async fn join(self) -> usize {
        self.process.join().await
    }
}

/// The stream-json frames a mode acts on: a `result` ends the turn, or, as
/// an error, is refused with its text; a session whose broker tools did not
/// connect is refused at its start. Everything else stays in the transcript.
fn event(frame: &Value) -> Option<SessionEvent> {
    match frame["type"].as_str()? {
        "result" if frame["is_error"] == true => {
            let why = frame["result"]
                .as_str()
                .or_else(|| frame["subtype"].as_str())
                .unwrap_or("an error");
            Some(SessionEvent::Rejected(why.to_owned()))
        }
        "result" => Some(SessionEvent::TurnEnded),
        "system" if frame["subtype"] == "init" => {
            let servers = frame["mcp_servers"].as_array()?;
            let status = servers
                .iter()
                .find(|server| server["name"] == MCP_SERVER)
                .map_or("missing", |server| {
                    server["status"].as_str().unwrap_or("unknown")
                });
            (!matches!(status, "connected" | "pending")).then(|| {
                SessionEvent::Rejected(format!("the broker's tools did not connect: {status}"))
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Role;
    use std::path::Path;

    fn spec(run_dir: &Path) -> SessionSpec {
        SessionSpec {
            id: "as_1".into(),
            role: Role::TicketWorker,
            grants: vec!["worker_report".into(), "fetch_url".into()],
            token: "tok-1".into(),
            model: "deepseek-v4-flash".into(),
            run_dir: run_dir.to_owned(),
            broker_socket: PathBuf::from("/state/broker.sock"),
            pi_settings: run_dir.join("pi-settings.json"),
            node: PathBuf::from("/opt/node/bin/node"),
            pi_root: PathBuf::from("/srv/pi"),
            pstack: None,
        }
    }

    fn args(harness: &ClaudeCodeHarness, spec: &SessionSpec, prompt: Option<&str>) -> Vec<String> {
        let files = Files {
            mcp_config: PathBuf::from("/state/tickets/tk_1/as_1.mcp.json"),
            token: PathBuf::from("/state/tickets/tk_1/as_1.token"),
        };
        harness
            .command(spec, &files, prompt)
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn native() -> ClaudeCodeHarness {
        ClaudeCodeHarness {
            binary: PathBuf::from("/opt/claude/versions/2.1.231"),
            node: None,
            solos_swarm: PathBuf::from("/usr/local/bin/solos-swarm"),
        }
    }

    #[test]
    fn the_launch_is_pinned_keyless_offline_and_limited_to_the_broker_tools() {
        let dir = std::env::temp_dir().join(format!("claude-launch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("work")).unwrap();
        let spec = spec(&dir);
        let work = dir.join("work").to_string_lossy().into_owned();
        #[rustfmt::skip]
        let expected: Vec<String> = [
            "--unshare-all", "--die-with-parent", "--new-session",
            "--ro-bind", "/usr", "/usr",
            "--ro-bind-try", "/etc", "/etc",
            "--ro-bind-try", "/lib", "/lib",
            "--ro-bind-try", "/lib64", "/lib64",
            "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/home/claude",
            "--ro-bind", &work, "/work",
            "--ro-bind", "/opt/claude/versions/2.1.231", "/runtime/claude",
            "--ro-bind", "/usr/local/bin/solos-swarm", "/runtime/solos-swarm",
            "--ro-bind", "/state/tickets/tk_1/as_1.mcp.json", "/runtime/mcp.json",
            "--ro-bind", "/state/tickets/tk_1/as_1.token", "/runtime/token",
            "--ro-bind", "/state/broker.sock", "/broker.sock",
            "--clearenv",
            "--setenv", "PATH", "/usr/bin:/bin",
            "--setenv", "HOME", "/home/claude",
            "--setenv", "TMPDIR", "/tmp",
            "--setenv", "ANTHROPIC_BASE_URL", "http://127.0.0.1:41300",
            "--setenv", "ANTHROPIC_API_KEY", "tok-1",
            "--setenv", "ANTHROPIC_DEFAULT_HAIKU_MODEL", "deepseek-v4-flash",
            "--setenv", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1",
            "--setenv", "DISABLE_AUTOUPDATER", "1",
            "--setenv", "DISABLE_TELEMETRY", "1",
            "--setenv", "DISABLE_ERROR_REPORTING", "1",
            "--chdir", "/work",
            "/runtime/solos-swarm", "bridge", "--listen", "127.0.0.1:41300", "--socket", "/broker.sock", "--",
            "/runtime/claude",
            "--print", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
            "--bare", "--model", "deepseek-v4-flash", "--tools", "",
            "--strict-mcp-config", "--mcp-config", "/runtime/mcp.json",
            "--permission-mode", "dontAsk", "--allowedTools", "mcp__swarm__worker_report,mcp__swarm__fetch_url",
            "--setting-sources", "user", "--disable-slash-commands", "--no-session-persistence",
            "--system-prompt", "You are a worker.",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(args(&native(), &spec, Some("You are a worker.")), expected);

        let without = args(&native(), &spec, None);
        assert_eq!(
            without[..],
            expected[..expected.len() - 2],
            "no prompt, no flag"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        let bare = args(&native(), &spec, None);
        let at = bare.iter().position(|arg| arg == "/work").unwrap();
        assert_eq!(bare[at - 1], "--dir", "no work directory, an empty one");
        assert!(!bare.contains(&work));
    }

    #[test]
    fn with_node_the_package_directory_is_mounted_and_node_runs_its_cli() {
        let harness = ClaudeCodeHarness {
            binary: PathBuf::from("/opt/npm/lib/node_modules/@anthropic-ai/claude-code/cli.js"),
            node: Some(PathBuf::from("/opt/node-v24/bin/node")),
            solos_swarm: PathBuf::from("/usr/local/bin/solos-swarm"),
        };
        let args = args(&harness, &spec(Path::new("/nowhere")), None);
        let window = |needle: &[&str]| args.windows(needle.len()).any(|w| w == needle);
        assert!(
            window(&["--ro-bind", "/opt/node-v24", "/runtime/node"]),
            "{args:?}"
        );
        assert!(window(&[
            "--ro-bind",
            "/opt/npm/lib/node_modules/@anthropic-ai/claude-code",
            "/runtime/claude"
        ]));
        assert!(window(&[
            "--",
            "/runtime/node/bin/node",
            "/runtime/claude/cli.js",
            "--print"
        ]));
    }

    #[test]
    fn the_mcp_config_runs_solos_swarm_mcp_on_the_socket_with_the_token_file() {
        assert_eq!(
            mcp_config(),
            json!({"mcpServers": {"swarm": {"type": "stdio", "command": "/runtime/solos-swarm",
                "args": ["mcp", "--socket", "/broker.sock", "--token-file", "/runtime/token"]}}})
        );
        let dir = std::env::temp_dir().join(format!("claude-files-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let files = write_files(&spec(&dir)).unwrap();
        assert_eq!(std::fs::read_to_string(&files.token).unwrap(), "tok-1");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&files.token)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(Cleanup(vec![files.token.clone(), files.mcp_config.clone()]));
        assert!(!files.token.exists() && !files.mcp_config.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_result_ends_the_turn_an_error_result_is_refused_and_so_are_missing_tools() {
        assert_eq!(
            event(&json!({"type": "result", "subtype": "success", "is_error": false})),
            Some(SessionEvent::TurnEnded)
        );
        let refused = json!({"type": "result", "subtype": "success", "is_error": true, "api_error_status": 403,
            "result": "Failed to authenticate. API Error: 403 tk_1's budget is reached: 90 of 100 microusd committed"});
        assert_eq!(
            event(&refused),
            Some(SessionEvent::Rejected(
                "Failed to authenticate. API Error: 403 tk_1's budget is reached: 90 of 100 microusd committed".into()
            ))
        );
        assert_eq!(
            event(&json!({"type": "result", "subtype": "error_max_turns", "is_error": true})),
            Some(SessionEvent::Rejected("error_max_turns".into()))
        );
        let init =
            |servers: Value| json!({"type": "system", "subtype": "init", "mcp_servers": servers});
        assert_eq!(
            event(&init(json!([{"name": "swarm", "status": "connected"}]))),
            None
        );
        assert_eq!(
            event(&init(json!([{"name": "swarm", "status": "failed"}]))),
            Some(SessionEvent::Rejected(
                "the broker's tools did not connect: failed".into()
            ))
        );
        assert_eq!(
            event(&init(json!([]))),
            Some(SessionEvent::Rejected(
                "the broker's tools did not connect: missing".into()
            ))
        );
        for quiet in [
            json!({"type": "assistant", "message": {}}),
            json!({"type": "user"}),
            json!({"type": "system", "subtype": "api_retry", "attempt": 1}),
            json!({"type": "stream_event"}),
            json!("text"),
        ] {
            assert_eq!(event(&quiet), None, "{quiet}");
        }
    }
}
