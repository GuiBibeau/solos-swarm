use super::BrowserConfig;
use super::cdp::Cdp;
use crate::bwrap::Bwrap;
use anyhow::{Context, Result, bail};
use sha2::{Digest as _, Sha256};
use solos_agent_core::trail::{Digest, Provenance};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, Command};

const SOCKET: &str = "cdp.sock";
const SANDBOX_DIR: &str = "/s";
const ETC_BINDS: [&str; 6] = [
    "/etc/ssl",
    "/etc/ca-certificates",
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/localtime",
];
const STDERR_BYTES: usize = 16_384;
const READY_WAIT: Duration = Duration::from_secs(10);
const END_WAIT: Duration = Duration::from_secs(5);

pub(super) fn socket(session_dir: &Path) -> PathBuf {
    session_dir.join(SOCKET)
}

pub(super) fn command(config: &BrowserConfig, session_dir: &Path) -> Vec<OsString> {
    let sandbox = Bwrap::new()
        .unshare_all()
        .share_net()
        .die_with_parent()
        .new_session()
        .ro_bind("/usr", "/usr")
        .ro_bind_try("/lib", "/lib")
        .ro_bind_try("/lib64", "/lib64");
    let sandbox = ETC_BINDS
        .into_iter()
        .fold(sandbox, |sandbox, path| sandbox.ro_bind_try(path, path))
        .proc("/proc")
        .dev("/dev")
        .tmpfs("/tmp")
        .ro_bind(&config.lightpanda, "/lightpanda")
        .bind(session_dir, SANDBOX_DIR)
        .clearenv()
        .setenv("LIGHTPANDA_DISABLE_TELEMETRY", "true")
        .setenv("HOME", "/tmp");
    let resources = if config.subframes { "iframe" } else { "none" };
    let heap = config.v8_heap_mb.to_string();
    let response = super::RESPONSE_BYTES.to_string();
    let inside = format!("{SANDBOX_DIR}/{SOCKET}");
    let mut serve = vec![
        "serve",
        "--unix-socket",
        &inside,
        "--cdp-max-connections",
        "1",
        "--cdp-max-message-size",
        "1048576",
        "--disable-metrics",
        "--load-resources",
        resources,
        "--http-timeout",
        "20000",
        "--http-connect-timeout",
        "10000",
        "--http-max-response-size",
        &response,
        "--v8-max-heap-mb",
        &heap,
    ];
    if !config.allow_private_networks {
        serve.push("--block-private-networks");
    }
    let mut line = vec![
        config.prlimit.clone().into_os_string(),
        format!("--as={}", config.address_space_bytes).into(),
        "--core=0".into(),
        "--nofile=1024".into(),
        "--".into(),
        config.bwrap.clone().into_os_string(),
    ];
    line.extend(sandbox.exec("/lightpanda", serve));
    line
}

pub(super) fn argv_digest(line: &[OsString]) -> Digest {
    let mut hasher = Sha256::new();
    for arg in line {
        hasher.update(arg.as_bytes());
        hasher.update([0]);
    }
    digest(hasher)
}

fn digest(hasher: Sha256) -> Digest {
    Digest::try_from(format!("sha256:{:x}", hasher.finalize())).expect("sha256 is 64 hex digits")
}

fn file_digest(path: &Path) -> Result<Digest> {
    let mut file =
        fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(digest(hasher));
        }
        hasher.update(&buffer[..read]);
    }
}

pub(super) fn clear_stale_sessions(runtime_dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(runtime_dir)
        .with_context(|| format!("cannot make {}", runtime_dir.display()))?;
    fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(runtime_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if entry.file_type()?.is_dir() && super::session::is_id(&name.to_string_lossy()) {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

pub(super) struct Process {
    child: Child,
    dir: PathBuf,
    stderr: Arc<Mutex<VecDeque<u8>>>,
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Process {
    pub(super) async fn start(
        config: &BrowserConfig,
        session_dir: &Path,
    ) -> Result<(Self, Cdp, Provenance)> {
        let line = command(config, session_dir);
        let binary = config.lightpanda.clone();
        let binary_sha256 = tokio::task::spawn_blocking(move || file_digest(&binary)).await??;
        let provenance = Provenance {
            binary_sha256,
            argv_sha256: argv_digest(&line),
        };
        fs::DirBuilder::new()
            .mode(0o700)
            .create(session_dir)
            .with_context(|| format!("cannot make {}", session_dir.display()))?;
        let spawned = Command::new(&line[0])
            .args(&line[1..])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_dir_all(session_dir);
                return Err(error)
                    .with_context(|| format!("cannot run {}", config.prlimit.display()));
            }
        };
        let stderr = drain_stderr(child.stderr.take());
        let mut process = Self {
            child,
            dir: session_dir.to_owned(),
            stderr,
        };
        let cdp = process.connect().await?;
        Ok((process, cdp, provenance))
    }

    async fn connect(&mut self) -> Result<Cdp> {
        let socket = socket(&self.dir);
        let deadline = Instant::now() + READY_WAIT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!("it exited ({status}): {}", self.stderr_tail());
            }
            match Cdp::connect(&socket).await {
                Ok(cdp) => return Ok(cdp),
                Err(error) if Instant::now() > deadline => bail!(
                    "it did not listen within {} s ({error}): {}",
                    READY_WAIT.as_secs(),
                    self.stderr_tail()
                ),
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    }

    fn stderr_tail(&self) -> String {
        let mut ring = self.stderr.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(ring.make_contiguous()).into_owned()
    }

    pub(super) async fn end(mut self) {
        crate::code::kill_sandbox(&mut self.child).await;
        let _ = tokio::time::timeout(END_WAIT, self.child.wait()).await;
    }
}

fn drain_stderr(stderr: Option<ChildStderr>) -> Arc<Mutex<VecDeque<u8>>> {
    let ring = Arc::new(Mutex::new(VecDeque::new()));
    if let Some(mut stderr) = stderr {
        let writer = Arc::clone(&ring);
        tokio::spawn(async move {
            let mut buffer = [0u8; 4_096];
            while let Ok(read @ 1..) = stderr.read(&mut buffer).await {
                let mut ring = writer.lock().unwrap_or_else(PoisonError::into_inner);
                ring.extend(&buffer[..read]);
                let extra = ring.len().saturating_sub(STDERR_BYTES);
                ring.drain(..extra);
            }
        });
    }
    ring
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: serde_json::Value) -> BrowserConfig {
        let mut base = serde_json::json!({
            "lightpanda": "/opt/lightpanda/lightpanda",
            "audit_key": "/var/lib/solos-keys/audit.key",
        });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    fn text(line: &[OsString]) -> Vec<&str> {
        line.iter().map(|arg| arg.to_str().unwrap()).collect()
    }

    #[test]
    fn the_browser_runs_in_its_own_sandbox_with_the_network_and_little_else() {
        let line = command(
            &config(serde_json::json!({})),
            Path::new("/var/lib/solos/browser/0123456789abcdef"),
        );
        assert_eq!(
            text(&line),
            [
                "/usr/bin/prlimit",
                "--as=68719476736",
                "--core=0",
                "--nofile=1024",
                "--",
                "/usr/bin/bwrap",
                "--unshare-all",
                "--share-net",
                "--die-with-parent",
                "--new-session",
                "--ro-bind",
                "/usr",
                "/usr",
                "--ro-bind-try",
                "/lib",
                "/lib",
                "--ro-bind-try",
                "/lib64",
                "/lib64",
                "--ro-bind-try",
                "/etc/ssl",
                "/etc/ssl",
                "--ro-bind-try",
                "/etc/ca-certificates",
                "/etc/ca-certificates",
                "--ro-bind-try",
                "/etc/resolv.conf",
                "/etc/resolv.conf",
                "--ro-bind-try",
                "/etc/hosts",
                "/etc/hosts",
                "--ro-bind-try",
                "/etc/nsswitch.conf",
                "/etc/nsswitch.conf",
                "--ro-bind-try",
                "/etc/localtime",
                "/etc/localtime",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--tmpfs",
                "/tmp",
                "--ro-bind",
                "/opt/lightpanda/lightpanda",
                "/lightpanda",
                "--bind",
                "/var/lib/solos/browser/0123456789abcdef",
                "/s",
                "--clearenv",
                "--setenv",
                "LIGHTPANDA_DISABLE_TELEMETRY",
                "true",
                "--setenv",
                "HOME",
                "/tmp",
                "/lightpanda",
                "serve",
                "--unix-socket",
                "/s/cdp.sock",
                "--cdp-max-connections",
                "1",
                "--cdp-max-message-size",
                "1048576",
                "--disable-metrics",
                "--load-resources",
                "none",
                "--http-timeout",
                "20000",
                "--http-connect-timeout",
                "10000",
                "--http-max-response-size",
                "16777216",
                "--v8-max-heap-mb",
                "512",
                "--block-private-networks",
            ]
        );
    }

    #[test]
    fn frames_and_private_networks_are_opt_in() {
        let line = command(
            &config(serde_json::json!({"subframes": true, "allow_private_networks": true})),
            Path::new("/r/0123456789abcdef"),
        );
        let line = text(&line);
        let at = line
            .iter()
            .position(|arg| *arg == "--load-resources")
            .unwrap();
        assert_eq!(line[at + 1], "iframe");
        assert!(!line.contains(&"--block-private-networks"));
        assert!(!line.contains(&"/etc"), "all of /etc is never bound");
    }

    #[test]
    fn the_argv_digest_changes_with_any_argument_and_with_where_they_split() {
        let digest =
            |args: &[&str]| argv_digest(&args.iter().map(OsString::from).collect::<Vec<_>>());
        assert_eq!(digest(&["a", "bc"]), digest(&["a", "bc"]));
        assert_ne!(digest(&["a", "bc"]), digest(&["ab", "c"]));
        assert_ne!(digest(&["a", "bc"]), digest(&["a", "bd"]));
    }
}
