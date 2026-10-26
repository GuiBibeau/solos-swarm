//! The narrow code-execution tool.
//!
//! A worker never gets a host shell. `run_code` starts one separate bwrap
//! process per call with an empty environment, no network namespace, no broker
//! socket, no provider credential and no host home. Only the decoded dataset,
//! the regimes checkout and its Python environment are mounted read-only; the
//! only writable mount is that agent's scratch directory.
//!
//! The tool is deliberately single-purpose: one `/usr/bin/bash -c` command,
//! bounded input and output, a wall-clock timeout, and a small JSON receipt.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Semaphore,
    task::JoinHandle,
};

pub const DEFAULT_TIMEOUT_SECONDS: u64 = 120;
pub const MAX_TIMEOUT_SECONDS: u64 = 300;
pub const MAX_CODE_BYTES: usize = 1_048_576;
pub const MAX_FILES: usize = 64;
pub const MAX_FILE_BYTES: usize = 1_048_576;
pub const MAX_FILES_BYTES: usize = 4_194_304;
pub const MAX_STDIN_BYTES: usize = 1_048_576;
pub const STDOUT_LIMIT: usize = 65_536;
pub const STDERR_LIMIT: usize = 32_768;
pub const DEFAULT_CONCURRENCY: usize = 4;
pub const DEFAULT_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const TMPFS_BYTES: u64 = 268_435_456;
const FILE_SIZE_BYTES: u64 = 268_435_456;
const MAX_OPEN_FILES: u64 = 512;
const DEFAULT_DATASET_ROOT: &str = "/home/debian/.local/share/solos-data/phoenix_decoded";
const DEFAULT_REPO_ROOT: &str = "/home/debian/research/regimes/repo";
const DEFAULT_VENV_ROOT: &str = "/home/debian/research/regimes/repo/.venv";
const DEFAULT_UV_ROOT: &str = "/home/debian/.local/share/uv/python";
const DEFAULT_BWRAP: &str = "/usr/bin/bwrap";
const DEFAULT_PRLIMIT: &str = "/usr/bin/prlimit";

#[derive(Clone, Debug)]
pub struct CodeConfig {
    pub dataset_root: PathBuf,
    pub repo_root: PathBuf,
    pub venv_root: PathBuf,
    pub uv_root: Option<PathBuf>,
    pub scratch_root: PathBuf,
    pub concurrency: usize,
    pub memory_bytes: u64,
    pub bwrap: PathBuf,
    pub prlimit: PathBuf,
}
impl CodeConfig {
    fn from_env(run_dir: &Path) -> Result<Self> {
        let dataset_root = env_path("SWARM_CODE_DATASET_ROOT", DEFAULT_DATASET_ROOT);
        let repo_root = env_path("SWARM_CODE_REPO_ROOT", DEFAULT_REPO_ROOT);
        let venv_root = env_path("SWARM_CODE_VENV", DEFAULT_VENV_ROOT);
        let uv_root = env_path("SWARM_CODE_UV_ROOT", DEFAULT_UV_ROOT);
        let scratch_root = env_path(
            "SWARM_CODE_SCRATCH_ROOT",
            &run_dir.join("code").to_string_lossy(),
        );
        let concurrency = env_u64("SWARM_CODE_CONCURRENCY", DEFAULT_CONCURRENCY as u64)?;
        ensure!(
            (1..=8).contains(&concurrency),
            "SWARM_CODE_CONCURRENCY must be 1..=8"
        );
        let memory_bytes = env_u64("SWARM_CODE_MEMORY_BYTES", DEFAULT_MEMORY_BYTES)?;
        ensure!(
            (256 * 1024 * 1024..=16 * 1024 * 1024 * 1024).contains(&memory_bytes),
            "SWARM_CODE_MEMORY_BYTES must be 256 MiB..=16 GiB"
        );
        Ok(Self {
            dataset_root,
            repo_root,
            venv_root,
            uv_root: uv_root.is_dir().then_some(uv_root),
            scratch_root,
            concurrency: concurrency as usize,
            memory_bytes,
            bwrap: env_path("SWARM_CODE_BWRAP", DEFAULT_BWRAP),
            prlimit: env_path("SWARM_CODE_PRLIMIT", DEFAULT_PRLIMIT),
        })
    }
}
fn env_path(name: &str, default: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}
fn env_u64(name: &str, default: u64) -> Result<u64> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .with_context(|| format!("{name} is not an integer")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug)]
struct CodeRequest {
    code: String,
    files: Vec<(String, String)>,
    timeout: Duration,
    stdin: Option<String>,
}
impl CodeRequest {
    fn parse(args: &Value) -> Result<Self> {
        let object = args.as_object().context("run_code takes an object")?;
        ensure!(
            object
                .keys()
                .all(|key| matches!(key.as_str(), "code" | "files" | "timeout_seconds" | "stdin")),
            "run_code has an unknown parameter"
        );
        let code = object
            .get("code")
            .and_then(Value::as_str)
            .context("run_code code is required")?
            .to_owned();
        ensure!(!code.trim().is_empty(), "run_code code is empty");
        ensure!(
            code.len() <= MAX_CODE_BYTES,
            "run_code code exceeds {MAX_CODE_BYTES} bytes"
        );
        let mut files = Vec::new();
        let mut file_bytes = 0usize;
        if let Some(value) = object.get("files") {
            let map = value
                .as_object()
                .context("run_code files must be an object")?;
            ensure!(
                map.len() <= MAX_FILES,
                "run_code accepts at most {MAX_FILES} files"
            );
            for (name, content) in map {
                validate_file_name(name)?;
                let content = content
                    .as_str()
                    .with_context(|| format!("run_code file {name} must contain text"))?;
                ensure!(
                    content.len() <= MAX_FILE_BYTES,
                    "run_code file {name} exceeds {MAX_FILE_BYTES} bytes"
                );
                file_bytes = file_bytes.saturating_add(content.len());
                ensure!(
                    file_bytes <= MAX_FILES_BYTES,
                    "run_code files exceed {MAX_FILES_BYTES} bytes"
                );
                files.push((name.clone(), content.to_owned()));
            }
        }
        let timeout_seconds = match object.get("timeout_seconds") {
            None => DEFAULT_TIMEOUT_SECONDS,
            Some(value) => value
                .as_u64()
                .context("run_code timeout_seconds must be an integer")?,
        };
        ensure!(
            (1..=MAX_TIMEOUT_SECONDS).contains(&timeout_seconds),
            "run_code timeout_seconds must be 1..={MAX_TIMEOUT_SECONDS}"
        );
        let stdin = match object.get("stdin") {
            None => None,
            Some(value) => {
                let text = value
                    .as_str()
                    .context("run_code stdin must be a string")?
                    .to_owned();
                ensure!(
                    text.len() <= MAX_STDIN_BYTES,
                    "run_code stdin exceeds {MAX_STDIN_BYTES} bytes"
                );
                Some(text)
            }
        };
        Ok(Self {
            code,
            files,
            timeout: Duration::from_secs(timeout_seconds),
            stdin,
        })
    }
}
fn validate_file_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= 128,
        "run_code file name must be 1..=128 bytes"
    );
    ensure!(
        !name.starts_with('/') && !name.contains('\\') && !name.contains('\0'),
        "run_code file name must be a relative Linux path"
    );
    ensure!(
        !name.split('/').any(str::is_empty),
        "run_code file name has an empty path component"
    );
    for component in Path::new(name).components() {
        ensure!(
            matches!(component, Component::Normal(_)),
            "run_code file name may only use ordinary path components"
        );
    }
    Ok(())
}

pub struct CodeExecutor {
    config: CodeConfig,
    permits: Arc<Semaphore>,
}
impl CodeExecutor {
    pub fn new(run_dir: &Path) -> Result<Self> {
        Self::from_config(CodeConfig::from_env(run_dir)?)
    }
    fn from_config(config: CodeConfig) -> Result<Self> {
        ensure!(
            config.dataset_root.is_dir(),
            "run_code dataset root is missing: {}",
            config.dataset_root.display()
        );
        ensure!(
            config.repo_root.is_dir(),
            "run_code regimes root is missing: {}",
            config.repo_root.display()
        );
        ensure!(
            config.venv_root.is_dir(),
            "run_code Python environment is missing: {}",
            config.venv_root.display()
        );
        ensure!(
            config.venv_root.join("bin/python").exists(),
            "run_code Python interpreter is missing: {}",
            config.venv_root.join("bin/python").display()
        );
        ensure!(
            config.bwrap.is_file(),
            "run_code bubblewrap is missing: {}",
            config.bwrap.display()
        );
        ensure!(
            config.prlimit.is_file(),
            "run_code prlimit is missing: {}",
            config.prlimit.display()
        );
        fs::create_dir_all(&config.scratch_root)?;
        fs::set_permissions(&config.scratch_root, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            permits: Arc::new(Semaphore::new(config.concurrency)),
            config,
        })
    }
    /// Run one request for one agent. Non-zero exits and timeouts are normal
    /// results; only a launch or sandbox failure is an error.
    pub async fn run(&self, agent_id: &str, args: &Value) -> Result<Value> {
        let request = CodeRequest::parse(args)?;
        let scratch = self.scratch(agent_id)?;
        let queued = std::time::Instant::now();
        let _permit = self
            .permits
            .acquire()
            .await
            .context("run_code scheduler closed")?;
        let queue_wait_ms = queued.elapsed().as_millis() as u64;
        write_files(&scratch, &request.files)?;
        let command = self.command(&scratch, &request.code);
        let mut receipt = execute(command, request.stdin, request.timeout).await?;
        // `duration_ms` is sandbox time only; queue_wait_ms is the time spent
        // waiting for one of `concurrency` slots (78% of run_code wall time in
        // arch-path-b3 with 4 slots for 20 agents).
        receipt["queue_wait_ms"] = json!(queue_wait_ms);
        Ok(receipt)
    }
    fn scratch(&self, agent_id: &str) -> Result<PathBuf> {
        ensure!(
            agent_id.starts_with("agent-")
                && agent_id.len() > 6
                && agent_id[6..].chars().all(|char| char.is_ascii_digit()),
            "invalid agent id"
        );
        let path = self.config.scratch_root.join(agent_id);
        fs::create_dir_all(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(path)
    }
    fn command(&self, scratch: &Path, code: &str) -> Command {
        let mut command = Command::new(&self.config.prlimit);
        command.env_clear();
        command
            .arg(format!("--as={}", self.config.memory_bytes))
            .arg(format!("--fsize={FILE_SIZE_BYTES}"))
            .arg(format!("--cpu={MAX_TIMEOUT_SECONDS}"))
            .arg(format!("--nofile={MAX_OPEN_FILES}"))
            .arg("--")
            .arg(&self.config.bwrap)
            .args(self.bwrap_args(scratch, code));
        command
    }
    /// The bwrap argument vector is separated from the process launch so the
    /// isolation contract can be asserted without a Linux host.
    fn bwrap_args(&self, scratch: &Path, code: &str) -> Vec<OsString> {
        let mut args: Vec<OsString> = [
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind-try",
            "/etc",
            "/etc",
            "--ro-bind-try",
            "/lib",
            "/lib",
            "--ro-bind-try",
            "/lib64",
            "/lib64",
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/sbin",
            "/sbin",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--size",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        args.push(OsString::from(TMPFS_BYTES.to_string()));
        args.extend(
            ["--tmpfs", "/tmp", "--dir", "/work", "--bind"]
                .into_iter()
                .map(OsString::from),
        );
        args.push(scratch.into());
        args.push("/work".into());
        bound_alias(&mut args, "/dataset", &self.config.dataset_root);
        bound_alias(&mut args, "/regimes", &self.config.repo_root);
        bound_alias(&mut args, "/venv", &self.config.venv_root);
        if let Some(root) = &self.config.uv_root {
            // The venv's interpreter is an absolute symlink into uv's Python
            // store. Bind the whole store at its real path so that link resolves.
            create_parent_dirs(&mut args, root);
            args.push("--ro-bind".into());
            args.push(root.into());
            args.push(root.into());
        }
        args.extend(
            [
                "--chdir",
                "/work",
                "--clearenv",
                "--setenv",
                "PATH",
                "/venv/bin:/usr/local/bin:/usr/bin:/bin",
                "--setenv",
                "HOME",
                "/work",
                "--setenv",
                "TMPDIR",
                "/tmp",
                "--setenv",
                "LANG",
                "C.UTF-8",
                "--setenv",
                "LC_ALL",
                "C.UTF-8",
                "--setenv",
                "PYTHONNOUSERSITE",
                "1",
                "--setenv",
                "PYTHONDONTWRITEBYTECODE",
                "1",
                "--setenv",
                "PYTHONUNBUFFERED",
                "1",
                "--setenv",
                "PYTHONPATH",
                "/regimes/src:/regimes",
                "--setenv",
                "MPLBACKEND",
                "Agg",
                // Bound BLAS thread fan-out: the box has 128 cores, but four
                // concurrent sandboxes each starting 128 OpenBLAS threads
                // exhausts the per-process address-space cap before the user
                // code allocates anything.
                "--setenv",
                "OPENBLAS_NUM_THREADS",
                "4",
                "--setenv",
                "OMP_NUM_THREADS",
                "4",
                "--setenv",
                "MKL_NUM_THREADS",
                "4",
                "--setenv",
                "NUMEXPR_NUM_THREADS",
                "4",
                "--setenv",
                "VECLIB_MAXIMUM_THREADS",
                "4",
                "--setenv",
                "RAYON_NUM_THREADS",
                "4",
                // CPU-only deployment: even if a library probes for a device,
                // this sandbox never exposes one.
                "--setenv",
                "CUDA_VISIBLE_DEVICES",
                "",
                "/usr/bin/bash",
                "-c",
            ]
            .into_iter()
            .map(OsString::from),
        );
        args.push(code.into());
        args
    }
}
fn bound_alias(args: &mut Vec<OsString>, alias: &str, source: &Path) {
    args.push("--dir".into());
    args.push(alias.into());
    args.push("--ro-bind".into());
    args.push(source.into());
    args.push(alias.into());
}
fn create_parent_dirs(args: &mut Vec<OsString>, path: &Path) {
    let mut parents: Vec<&Path> = path.ancestors().skip(1).collect();
    parents.reverse();
    for parent in parents {
        if parent == Path::new("/") {
            continue;
        }
        args.push("--dir".into());
        args.push(parent.into());
    }
}
fn write_files(scratch: &Path, files: &[(String, String)]) -> Result<()> {
    for (name, content) in files {
        let path = scratch.join(name);
        ensure!(
            path.starts_with(scratch),
            "run_code file escaped the scratch directory"
        );
        if let Some(parent) = path.parent() {
            create_dirs(scratch, parent)?;
        }
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            ensure!(
                metadata.file_type().is_file() || metadata.file_type().is_symlink(),
                "run_code file path is a directory"
            );
            fs::remove_file(&path)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("run_code could not create {name}"))?;
        file.write_all(content.as_bytes())?;
    }
    Ok(())
}
fn create_dirs(scratch: &Path, directory: &Path) -> Result<()> {
    ensure!(
        directory.starts_with(scratch),
        "run_code directory escaped the scratch directory"
    );
    let relative = directory
        .strip_prefix(scratch)
        .context("run_code directory prefix")?;
    let mut current = scratch.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            bail!("run_code directory has an invalid component");
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "run_code scratch directory contains a symlink"
                );
                ensure!(
                    metadata.is_dir(),
                    "run_code scratch path is not a directory"
                );
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                fs::set_permissions(&current, fs::Permissions::from_mode(0o700))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

async fn execute(mut command: Command, stdin: Option<String>, timeout: Duration) -> Result<Value> {
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let started = Instant::now();
    let mut child = command.spawn().context("run_code sandbox launch failed")?;
    let stdout = child
        .stdout
        .take()
        .context("run_code sandbox stdout missing")?;
    let stderr = child
        .stderr
        .take()
        .context("run_code sandbox stderr missing")?;
    let stdout_task = tokio::spawn(read_bounded(stdout, STDOUT_LIMIT));
    let stderr_task = tokio::spawn(read_bounded(stderr, STDERR_LIMIT));
    let stdin_task = stdin.map(|text| {
        let mut pipe = child.stdin.take().expect("piped stdin");
        tokio::spawn(async move {
            pipe.write_all(text.as_bytes()).await?;
            pipe.shutdown().await
        })
    });
    let mut timed_out = false;
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(error)) => return Err(error.into()),
        Err(_) => {
            timed_out = true;
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            None
        }
    };
    let (stdout, stdout_truncated) = finish_reader(stdout_task).await;
    let (stderr, stderr_truncated) = finish_reader(stderr_task).await;
    if let Some(task) = stdin_task {
        let abort = task.abort_handle();
        if tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .is_err()
        {
            abort.abort();
        }
    }
    Ok(json!({
        "exit_code": status.and_then(|status| status.code()).unwrap_or(-1),
        "stdout": stdout,
        "stderr": stderr,
        "timed_out": timed_out,
        "duration_ms": started.elapsed().as_millis() as u64,
        "truncated": stdout_truncated || stderr_truncated,
    }))
}
async fn finish_reader(mut task: JoinHandle<io::Result<(String, bool)>>) -> (String, bool) {
    let abort = task.abort_handle();
    match tokio::time::timeout(Duration::from_secs(5), &mut task).await {
        Ok(Ok(Ok(value))) => value,
        Ok(Ok(Err(_))) | Ok(Err(_)) => (String::new(), true),
        Err(_) => {
            abort.abort();
            (String::new(), true)
        }
    }
}
async fn read_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> io::Result<(String, bool)> {
    let mut kept = Vec::with_capacity(limit.min(8_192));
    let mut buffer = [0u8; 8_192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let room = limit.saturating_sub(kept.len());
        let take = room.min(read);
        kept.extend_from_slice(&buffer[..take]);
        if take < read {
            truncated = true;
        }
    }
    Ok((String::from_utf8_lossy(&kept).into_owned(), truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(dir: &Path) -> CodeConfig {
        CodeConfig {
            dataset_root: PathBuf::from("/usr"),
            repo_root: PathBuf::from("/usr"),
            venv_root: PathBuf::from("/usr"),
            uv_root: None,
            scratch_root: dir.to_path_buf(),
            concurrency: 1,
            memory_bytes: DEFAULT_MEMORY_BYTES,
            bwrap: PathBuf::from("/usr/bin/bwrap"),
            prlimit: PathBuf::from("/usr/bin/prlimit"),
        }
    }
    fn executor(dir: &Path) -> CodeExecutor {
        // The unit tests exercise the argument and I/O contract without a
        // Linux bwrap binary or a real Python installation.
        CodeExecutor {
            config: test_config(dir),
            permits: Arc::new(Semaphore::new(1)),
        }
    }
    #[test]
    fn request_rejects_escapes_and_bounds() {
        assert!(CodeRequest::parse(&json!({"code":"echo ok"})).is_ok());
        for bad in [
            json!({"code":"echo ok","files":{"../escape":"x"}}),
            json!({"code":"echo ok","files":{"/absolute":"x"}}),
            json!({"code":"echo ok","files":{"a/../../b":"x"}}),
            json!({"code":"echo ok","timeout_seconds":0}),
            json!({"code":"echo ok","timeout_seconds":301}),
            json!({"code":"echo ok","unknown":1}),
        ] {
            assert!(CodeRequest::parse(&bad).is_err(), "accepted {bad}");
        }
    }
    #[test]
    fn files_stay_inside_scratch_and_replace_old_links() {
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&dir).unwrap();
        let outside = dir.join("outside");
        fs::write(&outside, "secret").unwrap();
        let scratch = dir.join("work");
        fs::create_dir_all(&scratch).unwrap();
        write_files(
            &scratch,
            &[
                ("nested/data.txt".into(), "hello".into()),
                ("replace.txt".into(), "new".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(scratch.join("nested/data.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            fs::read_to_string(scratch.join("replace.txt")).unwrap(),
            "new"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn sandbox_has_no_network_broker_or_provider_environment() {
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&dir).unwrap();
        let args = executor(&dir).bwrap_args(Path::new("/tmp/scratch"), "python -c pass");
        let text = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("--unshare-all"));
        assert!(text.contains("--die-with-parent"));
        assert!(text.contains("--ro-bind\n/usr\n/usr"));
        assert!(!text.contains("--ro-bind\n/tmp/scratch\n/work"));
        assert!(text.contains("--bind\n/tmp/scratch\n/work"));
        assert!(text.contains("--setenv\nHOME\n/work"));
        assert!(text.contains("--setenv\nCUDA_VISIBLE_DEVICES\n"));
        assert!(!text.contains("SWARM_TOKEN"));
        assert!(!text.contains("SWARM_SOCKET"));
        assert!(!text.contains("broker.sock"));
        assert!(!text.contains("--share-net"));
        fs::remove_dir_all(&dir).unwrap();
    }
    #[tokio::test]
    async fn bounded_reader_truncates_and_drains() {
        let input = [b'x'; 10];
        let (text, truncated) = read_bounded(&input[..], 4).await.unwrap();
        assert_eq!(text, "xxxx");
        assert!(truncated);
        let (text, truncated) = read_bounded(&input[..], 20).await.unwrap();
        assert_eq!(text.len(), 10);
        assert!(!truncated);
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_sandbox_runs_bash_with_a_file_and_clean_environment() {
        if !Path::new("/usr/bin/bwrap").is_file() || !Path::new("/usr/bin/prlimit").is_file() {
            return;
        }
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&dir).unwrap();
        let executor = executor(&dir);
        let result = executor
            .run(
                "agent-00",
                &json!({
                    "code": "cat input.txt && printf ' %s' \"$HOME\" && test -z \"$SWARM_TOKEN\" && test ! -S /broker.sock",
                    "files": {"input.txt": "hello"},
                    "timeout_seconds": 10
                }),
            )
            .await
            .unwrap();
        assert_eq!(result["exit_code"], json!(0), "{result}");
        assert_eq!(result["stdout"], json!("hello /work"));
        assert_eq!(result["timed_out"], json!(false));
        fs::remove_dir_all(&dir).unwrap();
    }
}
