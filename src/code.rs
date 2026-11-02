//! The narrow code-execution tool.
//!
//! A worker never gets a host shell. `run_code` starts one separate bwrap
//! process per call with an empty environment, no network namespace, no broker
//! socket, no provider credential and no host home. Only the configured
//! mounts are read-only binds; the only writable mount is the caller's
//! scratch directory. A swarm run mounts the decoded dataset, the regimes
//! checkout and its Python environment, and gives each `agent-NN` a scratch
//! directory under the run; agent mode mounts what its config names and
//! gives each Ticket's worker that Ticket's directory.
//!
//! The tool is deliberately single-purpose: one `/usr/bin/bash -c` command,
//! bounded input and output, a wall-clock timeout, and a small JSON receipt.

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
// Through the crate's name, not `crate::`: the broker's HTTP tests compile
// this file as a module of their own.
use solos_swarm::bwrap::Bwrap;
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{Semaphore, watch},
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
/// How many sandboxes may run at once.
pub const CONCURRENCY_RANGE: std::ops::RangeInclusive<usize> = 1..=8;
/// The address-space caps a sandbox may run under.
pub const MEMORY_RANGE: std::ops::RangeInclusive<u64> = 256 * 1024 * 1024..=16 * 1024 * 1024 * 1024;
/// Root names the sandbox's own mounts use, so no bind may take them.
const RESERVED_TARGETS: [&str; 11] = [
    "bin", "dev", "etc", "lib", "lib64", "proc", "runtime", "sbin", "tmp", "usr", "work",
];

/// One read-only bind: the host's `source` appears at `target`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    /// One directory under `/`, such as `/data`.
    pub target: PathBuf,
    pub source: PathBuf,
}
impl Mount {
    pub fn validate(&self) -> Result<()> {
        let mut parts = self.target.components();
        let name = match (parts.next(), parts.next(), parts.next()) {
            (Some(Component::RootDir), Some(Component::Normal(name)), None) => name.to_str(),
            _ => None,
        };
        let Some(name) = name else {
            bail!(
                "run_code mount target {} is not one directory under /",
                self.target.display()
            );
        };
        ensure!(
            !RESERVED_TARGETS.contains(&name),
            "run_code mount target {} is a system path",
            self.target.display()
        );
        ensure!(
            self.source.is_absolute(),
            "run_code mount source {} is not an absolute path",
            self.source.display()
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CodeConfig {
    /// Read-only binds, in this order.
    pub mounts: Vec<Mount>,
    pub uv_root: Option<PathBuf>,
    /// Every scratch directory lies under it.
    pub scratch_root: PathBuf,
    pub concurrency: usize,
    pub memory_bytes: u64,
    pub bwrap: PathBuf,
    pub prlimit: PathBuf,
}
impl CodeConfig {
    /// `mounts` and the default limits and binaries.
    pub fn new(scratch_root: PathBuf, mounts: Vec<Mount>) -> Self {
        Self {
            mounts,
            uv_root: None,
            scratch_root,
            concurrency: DEFAULT_CONCURRENCY,
            memory_bytes: DEFAULT_MEMORY_BYTES,
            bwrap: PathBuf::from(DEFAULT_BWRAP),
            prlimit: PathBuf::from(DEFAULT_PRLIMIT),
        }
    }
    /// A swarm run's, from `SWARM_CODE_*`: the decoded dataset, the regimes
    /// checkout and its Python environment.
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
            MEMORY_RANGE.contains(&memory_bytes),
            "SWARM_CODE_MEMORY_BYTES must be 256 MiB..=16 GiB"
        );
        ensure!(
            dataset_root.is_dir(),
            "run_code dataset root is missing: {}",
            dataset_root.display()
        );
        ensure!(
            repo_root.is_dir(),
            "run_code regimes root is missing: {}",
            repo_root.display()
        );
        ensure!(
            venv_root.is_dir(),
            "run_code Python environment is missing: {}",
            venv_root.display()
        );
        ensure!(
            venv_root.join("bin/python").exists(),
            "run_code Python interpreter is missing: {}",
            venv_root.join("bin/python").display()
        );
        Ok(Self {
            mounts: swarm_mounts(dataset_root, repo_root, venv_root),
            uv_root: uv_root.is_dir().then_some(uv_root),
            scratch_root,
            concurrency: concurrency as usize,
            memory_bytes,
            bwrap: env_path("SWARM_CODE_BWRAP", DEFAULT_BWRAP),
            prlimit: env_path("SWARM_CODE_PRLIMIT", DEFAULT_PRLIMIT),
        })
    }
}
fn swarm_mounts(dataset: PathBuf, regimes: PathBuf, venv: PathBuf) -> Vec<Mount> {
    [
        ("/dataset", dataset),
        ("/regimes", regimes),
        ("/venv", venv),
    ]
    .into_iter()
    .map(|(target, source)| Mount {
        target: PathBuf::from(target),
        source,
    })
    .collect()
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
    /// A swarm run's executor, configured from the environment.
    pub fn new(run_dir: &Path) -> Result<Self> {
        Self::from_config(CodeConfig::from_env(run_dir)?)
    }
    pub fn from_config(config: CodeConfig) -> Result<Self> {
        for mount in &config.mounts {
            ensure!(
                mount.source.exists(),
                "run_code mount source is missing: {}",
                mount.source.display()
            );
        }
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
    /// Run one request for one swarm agent, in its scratch directory under
    /// the run. Non-zero exits and timeouts are normal results; only a launch
    /// or sandbox failure is an error.
    pub async fn run(&self, agent_id: &str, args: &Value) -> Result<Value> {
        let request = CodeRequest::parse(args)?;
        let scratch = self.scratch(agent_id)?;
        self.execute(&scratch, request).await
    }
    /// Run one request with `scratch` as `/work`: a directory the caller made
    /// under the scratch root, such as a Ticket's, kept between calls.
    pub async fn run_in(&self, scratch: &Path, args: &Value) -> Result<Value> {
        let request = CodeRequest::parse(args)?;
        let scratch = self.owned_scratch(scratch)?;
        self.execute(&scratch, request).await
    }
    /// [`run_in`](Self::run_in) on its own task: it returns at once, and
    /// the request ends with the same receipt or error.
    pub fn start_in(self: &Arc<Self>, scratch: &Path, args: &Value) -> Result<Started> {
        let request = CodeRequest::parse(args)?;
        let scratch = self.owned_scratch(scratch)?;
        Ok(self.start(scratch, request, OutputBounds::RUN_CODE))
    }
    /// One bash command with `scratch` as `/work` on its own task, with its
    /// own wall-clock `timeout` and output `bounds`, and no files or stdin.
    pub fn start_job_in(
        self: &Arc<Self>,
        scratch: &Path,
        code: &str,
        timeout: Duration,
        bounds: OutputBounds,
    ) -> Result<Started> {
        ensure!(!code.trim().is_empty(), "run_code code is empty");
        ensure!(
            code.len() <= MAX_CODE_BYTES,
            "run_code code exceeds {MAX_CODE_BYTES} bytes"
        );
        let scratch = self.owned_scratch(scratch)?;
        let request = CodeRequest {
            code: code.to_owned(),
            files: Vec::new(),
            timeout,
            stdin: None,
        };
        Ok(self.start(scratch, request, bounds))
    }
    fn start(
        self: &Arc<Self>,
        scratch: PathBuf,
        request: CodeRequest,
        bounds: OutputBounds,
    ) -> Started {
        let progress = Arc::new(Progress {
            stdout: Mutex::new(Captured::new(bounds.stdout_bytes)),
            stderr: Mutex::new(Captured::new(bounds.stderr_bytes)),
            running: AtomicBool::new(false),
        });
        let (stop, stopping) = watch::channel(false);
        let (ended, end) = watch::channel(None);
        tokio::spawn(supervise(
            self.clone(),
            scratch,
            request,
            progress.clone(),
            stopping,
            ended,
        ));
        Started {
            progress,
            stop,
            end,
        }
    }
    /// `scratch` itself, not a link, and inside the scratch root once every
    /// link and `..` above it is resolved.
    fn owned_scratch(&self, scratch: &Path) -> Result<PathBuf> {
        let metadata = fs::symlink_metadata(scratch)
            .with_context(|| format!("run_code scratch is missing: {}", scratch.display()))?;
        ensure!(
            metadata.is_dir(),
            "run_code scratch is not a directory: {}",
            scratch.display()
        );
        let root = fs::canonicalize(&self.config.scratch_root)?;
        let real = fs::canonicalize(scratch)?;
        ensure!(
            real.starts_with(&root) && real != root,
            "run_code scratch is outside its root: {}",
            scratch.display()
        );
        Ok(real)
    }
    async fn execute(&self, scratch: &Path, request: CodeRequest) -> Result<Value> {
        let queued = std::time::Instant::now();
        let _permit = self
            .permits
            .acquire()
            .await
            .context("run_code scheduler closed")?;
        let queue_wait_ms = queued.elapsed().as_millis() as u64;
        write_files(scratch, &request.files)?;
        let command = self.command(scratch, &request.code);
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
        let mut sandbox = Bwrap::new()
            .unshare_all()
            .die_with_parent()
            .new_session()
            .ro_bind("/usr", "/usr")
            .ro_bind_try("/etc", "/etc")
            .ro_bind_try("/lib", "/lib")
            .ro_bind_try("/lib64", "/lib64")
            .symlink("usr/bin", "/bin")
            .symlink("usr/sbin", "/sbin")
            .proc("/proc")
            .dev("/dev")
            .tmpfs_sized(TMPFS_BYTES, "/tmp")
            .dir("/work")
            .bind(scratch, "/work");
        for mount in &self.config.mounts {
            sandbox = sandbox
                .dir(&mount.target)
                .ro_bind(&mount.source, &mount.target);
        }
        if let Some(root) = &self.config.uv_root {
            // The venv's interpreter is an absolute symlink into uv's Python
            // store. Bind the whole store at its real path so that link resolves.
            sandbox = parent_dirs(sandbox, root).ro_bind(root, root);
        }
        sandbox
            .chdir("/work")
            .clearenv()
            .setenv("PATH", "/venv/bin:/usr/local/bin:/usr/bin:/bin")
            .setenv("HOME", "/work")
            .setenv("TMPDIR", "/tmp")
            .setenv("LANG", "C.UTF-8")
            .setenv("LC_ALL", "C.UTF-8")
            .setenv("PYTHONNOUSERSITE", "1")
            .setenv("PYTHONDONTWRITEBYTECODE", "1")
            .setenv("PYTHONUNBUFFERED", "1")
            .setenv("PYTHONPATH", "/regimes/src:/regimes")
            .setenv("MPLBACKEND", "Agg")
            // Bound BLAS thread fan-out: the box has 128 cores, but four
            // concurrent sandboxes each starting 128 OpenBLAS threads
            // exhausts the per-process address-space cap before the user
            // code allocates anything.
            .setenv("OPENBLAS_NUM_THREADS", "4")
            .setenv("OMP_NUM_THREADS", "4")
            .setenv("MKL_NUM_THREADS", "4")
            .setenv("NUMEXPR_NUM_THREADS", "4")
            .setenv("VECLIB_MAXIMUM_THREADS", "4")
            .setenv("RAYON_NUM_THREADS", "4")
            // CPU-only deployment: even if a library probes for a device,
            // this sandbox never exposes one.
            .setenv("CUDA_VISIBLE_DEVICES", "")
            .exec("/usr/bin/bash", ["-c", code])
    }
}
/// `--dir` for each directory above `path` but the root, outermost first.
fn parent_dirs(mut sandbox: Bwrap, path: &Path) -> Bwrap {
    let mut parents: Vec<&Path> = path.ancestors().skip(1).collect();
    parents.reverse();
    for parent in parents {
        if parent == Path::new("/") {
            continue;
        }
        sandbox = sandbox.dir(parent);
    }
    sandbox
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

/// What a started request keeps of each stream until it is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputBounds {
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

impl OutputBounds {
    /// `run_code`'s, so a request nobody reads early keeps what its receipt
    /// always kept.
    pub const RUN_CODE: Self = Self {
        stdout_bytes: STDOUT_LIMIT,
        stderr_bytes: STDERR_LIMIT,
    };
}

/// How a started request ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Ended {
    /// `receipt` is `run_code`'s, with the output nobody read before. A
    /// request stopped before it had a sandbox slot never ran.
    Ran { receipt: Value, stopped: bool },
    /// The sandbox could not run; `run_code` returns this as its error.
    Failed(String),
}

/// Output printed since the last read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Unread {
    pub stdout: String,
    pub stderr: String,
    /// Some output was dropped because nobody read it in time.
    pub truncated: bool,
}

/// A request running on its own task in the same sandbox, under the same
/// limits, as [`CodeExecutor::run_in`]. Its output can be read while it
/// runs, its end awaited, and it can be stopped. Dropping it stops it.
pub struct Started {
    progress: Arc<Progress>,
    stop: watch::Sender<bool>,
    end: watch::Receiver<Option<Ended>>,
}

impl Started {
    /// It holds a sandbox slot; until then it waits for one.
    pub fn running(&self) -> bool {
        self.progress.running.load(Ordering::SeqCst)
    }
    pub fn ended(&self) -> Option<Ended> {
        self.end.borrow().clone()
    }
    /// Its end, if it comes within `limit`.
    pub async fn wait(&self, limit: Duration) -> Option<Ended> {
        tokio::time::timeout(limit, self.end()).await.ok()
    }
    /// Its end, whenever it comes.
    pub fn end(&self) -> impl Future<Output = Ended> + Send + 'static {
        let mut end = self.end.clone();
        async move {
            let ended = match end.wait_for(Option::is_some).await {
                Ok(ended) => ended.clone(),
                Err(_) => None,
            };
            ended.unwrap_or_else(|| {
                Ended::Failed("the run_code task ended without a receipt".into())
            })
        }
    }
    /// Kill it, or keep it from starting. Its end follows.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }
    /// What it printed since the last read. While it runs, a character the
    /// stream has not finished waits for its last bytes.
    pub fn read(&self) -> Unread {
        let (stdout, out_dropped) = self.progress.stdout().drain(false);
        let (stderr, err_dropped) = self.progress.stderr().drain(false);
        Unread {
            stdout,
            stderr,
            truncated: out_dropped || err_dropped,
        }
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

struct Progress {
    stdout: Mutex<Captured>,
    stderr: Mutex<Captured>,
    running: AtomicBool,
}

impl Progress {
    fn stdout(&self) -> MutexGuard<'_, Captured> {
        self.stdout.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn stderr(&self) -> MutexGuard<'_, Captured> {
        self.stderr.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One stream's unread output, at most `limit` bytes. Output past it is
/// dropped until a read makes room, as [`read_bounded`] drops it.
struct Captured {
    unread: Vec<u8>,
    limit: usize,
    dropped: bool,
}

impl Captured {
    fn new(limit: usize) -> Self {
        Self {
            unread: Vec::new(),
            limit,
            dropped: false,
        }
    }
    fn push(&mut self, bytes: &[u8]) {
        let room = self.limit.saturating_sub(self.unread.len());
        let take = room.min(bytes.len());
        self.unread.extend_from_slice(&bytes[..take]);
        if take < bytes.len() {
            self.dropped = true;
        }
    }
    /// The unread text, and whether output was dropped since the last read.
    fn drain(&mut self, whole: bool) -> (String, bool) {
        let cut = if whole {
            self.unread.len()
        } else {
            complete_prefix(&self.unread)
        };
        let text = String::from_utf8_lossy(&self.unread[..cut]).into_owned();
        self.unread.drain(..cut);
        (text, std::mem::take(&mut self.dropped))
    }
    /// What [`finish_reader`] reports for a reader that failed or hung.
    fn lost(&mut self) -> (String, bool) {
        self.unread.clear();
        self.dropped = false;
        (String::new(), true)
    }
}

/// `bytes` up to a character the stream has not finished.
fn complete_prefix(bytes: &[u8]) -> usize {
    // A UTF-8 character is at most four bytes, so only the last three can
    // start one that is not finished.
    for back in 1..=bytes.len().min(3) {
        let at = bytes.len() - back;
        let byte = bytes[at];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let length = match byte {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        return if back < length { at } else { bytes.len() };
    }
    bytes.len()
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

async fn supervise(
    executor: Arc<CodeExecutor>,
    scratch: PathBuf,
    request: CodeRequest,
    progress: Arc<Progress>,
    mut stop: watch::Receiver<bool>,
    end: watch::Sender<Option<Ended>>,
) {
    let ended = attend(&executor, &scratch, request, &progress, &mut stop)
        .await
        .unwrap_or_else(|err| Ended::Failed(format!("{err:#}")));
    end.send_replace(Some(ended));
}

/// [`CodeExecutor::execute`] for a started request: a stop while it waits
/// for a slot ends it unrun, and a stop while it runs kills it.
async fn attend(
    executor: &CodeExecutor,
    scratch: &Path,
    request: CodeRequest,
    progress: &Arc<Progress>,
    stop: &mut watch::Receiver<bool>,
) -> Result<Ended> {
    let queued = Instant::now();
    let permit = tokio::select! {
        permit = executor.permits.clone().acquire_owned() => {
            permit.context("run_code scheduler closed")?
        }
        () = stopped(stop) => {
            let receipt = json!({
                "exit_code": -1,
                "stdout": "",
                "stderr": "",
                "timed_out": false,
                "duration_ms": 0,
                "truncated": false,
                "queue_wait_ms": queued.elapsed().as_millis() as u64,
            });
            return Ok(Ended::Ran { receipt, stopped: true });
        }
    };
    let queue_wait_ms = queued.elapsed().as_millis() as u64;
    progress.running.store(true, Ordering::SeqCst);
    write_files(scratch, &request.files)?;
    let command = executor.command(scratch, &request.code);
    let (mut receipt, was_stopped) =
        execute_watched(command, request.stdin, request.timeout, progress, stop).await?;
    receipt["queue_wait_ms"] = json!(queue_wait_ms);
    drop(permit);
    Ok(Ended::Ran {
        receipt,
        stopped: was_stopped,
    })
}

/// A stop was asked for, or every handle on the request is gone.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

/// [`execute`] with output anyone may read as it comes, and a stop that
/// kills the sandbox like a timeout does. The receipt is the same.
async fn execute_watched(
    mut command: Command,
    stdin: Option<String>,
    timeout: Duration,
    progress: &Arc<Progress>,
    stop: &mut watch::Receiver<bool>,
) -> Result<(Value, bool)> {
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
    let stdout_task = tokio::spawn(capture(stdout, progress.clone(), Stream::Stdout));
    let stderr_task = tokio::spawn(capture(stderr, progress.clone(), Stream::Stderr));
    let stdin_task = match stdin {
        None => None,
        Some(text) => {
            let mut pipe = child
                .stdin
                .take()
                .context("run_code sandbox stdin missing")?;
            Some(tokio::spawn(async move {
                pipe.write_all(text.as_bytes()).await?;
                pipe.shutdown().await
            }))
        }
    };
    let (mut timed_out, mut was_stopped) = (false, false);
    let status = tokio::select! {
        status = child.wait() => Some(status?),
        () = tokio::time::sleep(timeout) => {
            timed_out = true;
            None
        }
        () = stopped(stop) => {
            was_stopped = true;
            None
        }
    };
    if status.is_none() {
        kill_sandbox(&mut child).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }
    let (stdout, stdout_truncated) = finish_capture(stdout_task, progress, Stream::Stdout).await;
    let (stderr, stderr_truncated) = finish_capture(stderr_task, progress, Stream::Stderr).await;
    if let Some(task) = stdin_task {
        let abort = task.abort_handle();
        if tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .is_err()
        {
            abort.abort();
        }
    }
    let receipt = json!({
        "exit_code": status.and_then(|status| status.code()).unwrap_or(-1),
        "stdout": stdout,
        "stderr": stderr,
        "timed_out": timed_out,
        "duration_ms": started.elapsed().as_millis() as u64,
        "truncated": stdout_truncated || stderr_truncated,
    });
    Ok((receipt, was_stopped))
}

/// Kill a sandbox and everything it started. bwrap ties its namespace's
/// life to its own only once setup is done, so a plain kill in its first
/// milliseconds leaves the namespace running. Freeze it first, so it
/// clones nothing more, then kill what it cloned, the namespace's init,
/// whose death ends every process in the namespace, and then it.
#[cfg(target_os = "linux")]
async fn kill_sandbox(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: kill(2) takes no pointers. `pid` is our child and tokio
        // has not reaped it (it clears the id when it does), so the pid
        // names no other process.
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        for _ in 0..200 {
            if frozen_or_gone(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        for inner in cloned(pid) {
            // SAFETY: kill(2) takes no pointers. Only its parent, our
            // stopped child, can reap `inner`, so the pid still names it.
            unsafe { libc::kill(inner, libc::SIGKILL) };
        }
    }
    let _ = child.start_kill();
}

#[cfg(not(target_os = "linux"))]
async fn kill_sandbox(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}

#[cfg(target_os = "linux")]
fn frozen_or_gone(pid: libc::pid_t) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    // The state follows the command name, which may hold spaces or parentheses.
    let state = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next());
    matches!(state, None | Some("T" | "t" | "Z" | "X"))
}

#[cfg(target_os = "linux")]
fn cloned(pid: libc::pid_t) -> Vec<libc::pid_t> {
    fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|inner| inner.parse().ok())
        .collect()
}

async fn capture<R: AsyncRead + Unpin>(
    mut reader: R,
    progress: Arc<Progress>,
    stream: Stream,
) -> io::Result<()> {
    let mut buffer = [0u8; 8_192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        let mut captured = match stream {
            Stream::Stdout => progress.stdout(),
            Stream::Stderr => progress.stderr(),
        };
        captured.push(&buffer[..read]);
    }
}

/// [`finish_reader`] for a captured stream: what nobody read yet.
async fn finish_capture(
    mut task: JoinHandle<io::Result<()>>,
    progress: &Progress,
    stream: Stream,
) -> (String, bool) {
    let abort = task.abort_handle();
    let finished = match tokio::time::timeout(Duration::from_secs(5), &mut task).await {
        Ok(Ok(Ok(()))) => true,
        Ok(Ok(Err(_))) | Ok(Err(_)) => false,
        Err(_) => {
            abort.abort();
            false
        }
    };
    let mut captured = match stream {
        Stream::Stdout => progress.stdout(),
        Stream::Stderr => progress.stderr(),
    };
    if finished {
        captured.drain(true)
    } else {
        captured.lost()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A swarm run's mounts, with the system binaries standing in for the
    /// research trees so the sandbox runs on any Linux host.
    fn swarm_config(dir: &Path) -> CodeConfig {
        let usr = || PathBuf::from("/usr");
        CodeConfig {
            mounts: swarm_mounts(usr(), usr(), usr()),
            uv_root: None,
            scratch_root: dir.to_path_buf(),
            concurrency: 1,
            memory_bytes: DEFAULT_MEMORY_BYTES,
            bwrap: PathBuf::from("/usr/bin/bwrap"),
            prlimit: PathBuf::from("/usr/bin/prlimit"),
        }
    }
    /// An agent worker's: one configured mount, nothing else.
    fn worker_config(dir: &Path) -> CodeConfig {
        let mut config = CodeConfig::new(
            dir.to_path_buf(),
            vec![Mount {
                target: PathBuf::from("/data"),
                source: PathBuf::from("/usr/share"),
            }],
        );
        config.concurrency = 1;
        config
    }
    fn executor(config: CodeConfig) -> CodeExecutor {
        // The unit tests exercise the argument and I/O contract without a
        // Linux bwrap binary or a real Python installation.
        CodeExecutor {
            config,
            permits: Arc::new(Semaphore::new(1)),
        }
    }
    fn scratch_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&dir).unwrap();
        dir
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
        let dir = scratch_dir();
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
        let dir = scratch_dir();
        for (mode, config) in [
            ("swarm", swarm_config(&dir)),
            ("worker", worker_config(&dir)),
        ] {
            let args = executor(config).bwrap_args(Path::new("/tmp/scratch"), "python -c pass");
            let text = args
                .iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("--unshare-all"), "{mode}");
            assert!(text.contains("--die-with-parent"), "{mode}");
            assert!(text.contains("--ro-bind\n/usr\n/usr"), "{mode}");
            assert!(!text.contains("--ro-bind\n/tmp/scratch\n/work"), "{mode}");
            assert!(text.contains("--bind\n/tmp/scratch\n/work"), "{mode}");
            assert_eq!(
                text.matches("\n--bind\n").count(),
                1,
                "{mode}: one writable mount"
            );
            assert!(text.contains("--setenv\nHOME\n/work"), "{mode}");
            assert!(text.contains("--setenv\nCUDA_VISIBLE_DEVICES\n"), "{mode}");
            assert!(!text.contains("SWARM_TOKEN"), "{mode}");
            assert!(!text.contains("SWARM_SOCKET"), "{mode}");
            assert!(!text.contains("broker.sock"), "{mode}");
            assert!(!text.contains("--share-net"), "{mode}");
        }
        let worker = executor(worker_config(&dir)).bwrap_args(Path::new("/s"), "true");
        let worker: Vec<String> = worker
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let at = worker.iter().position(|arg| arg == "/data").unwrap();
        assert_eq!(
            worker[at - 1..at + 4],
            ["--dir", "/data", "--ro-bind", "/usr/share", "/data"]
        );
        assert!(!worker.iter().any(|arg| arg == "/dataset" || arg == "/venv"));
        fs::remove_dir_all(&dir).unwrap();
    }
    /// The whole command a swarm run's `run_code` starts, as it was before
    /// the mounts became configurable.
    #[test]
    fn the_swarm_sandbox_command_is_unchanged() {
        let mut config = swarm_config(Path::new("/runs/r1/code"));
        config.mounts = swarm_mounts(
            PathBuf::from("/data/phoenix"),
            PathBuf::from("/srv/regimes"),
            PathBuf::from("/srv/regimes/.venv"),
        );
        config.uv_root = Some(PathBuf::from("/opt/uv/python"));
        let command = executor(config).command(Path::new("/runs/r1/code/agent-03"), "echo ok");
        let command = command.as_std();
        assert_eq!(command.get_program(), "/usr/bin/prlimit");
        let args: Vec<&str> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        let golden = [
            "--as=4294967296",
            "--fsize=268435456",
            "--cpu=300",
            "--nofile=512",
            "--",
            "/usr/bin/bwrap",
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
            "268435456",
            "--tmpfs",
            "/tmp",
            "--dir",
            "/work",
            "--bind",
            "/runs/r1/code/agent-03",
            "/work",
            "--dir",
            "/dataset",
            "--ro-bind",
            "/data/phoenix",
            "/dataset",
            "--dir",
            "/regimes",
            "--ro-bind",
            "/srv/regimes",
            "/regimes",
            "--dir",
            "/venv",
            "--ro-bind",
            "/srv/regimes/.venv",
            "/venv",
            "--dir",
            "/opt",
            "--dir",
            "/opt/uv",
            "--ro-bind",
            "/opt/uv/python",
            "/opt/uv/python",
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
            "--setenv",
            "CUDA_VISIBLE_DEVICES",
            "",
            "/usr/bin/bash",
            "-c",
            "echo ok",
        ];
        assert_eq!(args, golden);
        assert_eq!(command.get_envs().count(), 0);
        assert!(format!("{command:?}").starts_with("env -i "), "{command:?}");
    }
    #[test]
    fn a_swarm_scratch_is_an_agent_id_and_a_worker_scratch_is_its_own_directory_under_the_root() {
        let dir = scratch_dir();
        let swarm = executor(swarm_config(&dir));
        assert_eq!(swarm.scratch("agent-07").unwrap(), dir.join("agent-07"));
        for bad in ["agent-", "agent-x1", "../agent-01", "tk_0123"] {
            assert!(swarm.scratch(bad).is_err(), "accepted {bad}");
        }
        let root = dir.join("tickets");
        let work = root.join("tk_1/work");
        fs::create_dir_all(&work).unwrap();
        let outside = dir.join("elsewhere");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("tk_1/linked")).unwrap();
        let worker = executor(worker_config(&root));
        assert_eq!(
            worker.owned_scratch(&work).unwrap(),
            fs::canonicalize(&work).unwrap()
        );
        for (case, path) in [
            ("missing", root.join("tk_2/work")),
            ("the root itself", root.clone()),
            ("outside the root", outside.clone()),
            ("a dotted escape", root.join("tk_1/work/../../../elsewhere")),
            ("a link", root.join("tk_1/linked")),
        ] {
            assert!(worker.owned_scratch(&path).is_err(), "{case}: accepted");
        }
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn a_mount_is_one_directory_under_the_root_that_no_system_path_takes() {
        let mount = |target: &str, source: &str| Mount {
            target: PathBuf::from(target),
            source: PathBuf::from(source),
        };
        mount("/data", "/srv/data").validate().unwrap();
        mount("/repo", "/home/me/repo").validate().unwrap();
        for (target, source) in [
            ("/work", "/srv"),
            ("/usr", "/srv"),
            ("/tmp", "/srv"),
            ("/runtime", "/srv"),
            ("/", "/srv"),
            ("data", "/srv"),
            ("/data/inner", "/srv"),
            ("/../data", "/srv"),
            ("/data", "relative/source"),
        ] {
            assert!(
                mount(target, source).validate().is_err(),
                "accepted {target} <- {source}"
            );
        }
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
    #[test]
    fn captured_output_keeps_unread_bytes_up_to_its_limit_and_whole_characters() {
        let mut captured = Captured::new(4);
        captured.push(b"abcdef");
        assert_eq!(captured.drain(false), ("abcd".into(), true));
        assert_eq!(captured.drain(false), (String::new(), false));
        captured.push("é".as_bytes());
        captured.push(&"ü".as_bytes()[..1]);
        assert_eq!(captured.drain(false), ("é".into(), false), "ü waits");
        captured.push(&"ü".as_bytes()[1..]);
        assert_eq!(captured.drain(false), ("ü".into(), false));
        captured.push(&"€".as_bytes()[..2]);
        assert_eq!(captured.drain(true), ("\u{fffd}".into(), false));
        assert_eq!(complete_prefix(&[b'a', 0xFF, 0xFE]), 3);
        assert_eq!(complete_prefix(&[0xF0, 0x9F, 0x98]), 0);
    }
    /// A started request that nobody reads early ends with `run_in`'s
    /// receipt, whatever its output and outcome.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_a_started_request_ends_with_the_receipt_run_in_gives() {
        if !Path::new("/usr/bin/bwrap").is_file() || !Path::new("/usr/bin/prlimit").is_file() {
            return;
        }
        let dir = scratch_dir();
        let work = dir.join("tk_1/work");
        fs::create_dir_all(&work).unwrap();
        let worker = Arc::new(executor(worker_config(&dir)));
        for args in [
            json!({"code": "cat in.txt; printf 'é%.0s' $(seq 40000); echo oops >&2; exit 3", "files": {"in.txt": "x"}}),
            json!({"code": "read line; echo \"got $line\"", "stdin": "hello\n"}),
            json!({"code": "sleep 5", "timeout_seconds": 1}),
        ] {
            let mut ran = worker.run_in(&work, &args).await.unwrap();
            let started = worker.start_in(&work, &args).unwrap();
            let Ended::Ran {
                receipt: mut kept,
                stopped: false,
            } = started.end().await
            else {
                panic!("{args} did not run")
            };
            for receipt in [&mut ran, &mut kept] {
                receipt["duration_ms"] = json!(0);
                receipt["queue_wait_ms"] = json!(0);
            }
            assert_eq!(kept, ran, "{args}");
        }
        let started = worker
            .start_in(&work, &json!({"code": "sleep 30"}))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        started.stop();
        let Some(Ended::Ran { receipt, stopped }) = started.wait(Duration::from_secs(10)).await
        else {
            panic!("the stop did not end it")
        };
        assert!(stopped);
        assert_eq!(
            (receipt["exit_code"].clone(), receipt["timed_out"].clone()),
            (json!(-1), json!(false))
        );
        fs::remove_dir_all(&dir).unwrap();
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_sandbox_runs_bash_with_a_file_and_clean_environment() {
        if !Path::new("/usr/bin/bwrap").is_file() || !Path::new("/usr/bin/prlimit").is_file() {
            return;
        }
        let code = json!({
            "code": "cat input.txt && printf ' %s' \"$HOME\" && test -z \"$SWARM_TOKEN\" && test ! -S /broker.sock",
            "files": {"input.txt": "hello"},
            "timeout_seconds": 10
        });
        let dir = scratch_dir();
        let swarm = executor(swarm_config(&dir));
        let result = swarm.run("agent-00", &code).await.unwrap();
        assert_eq!(result["exit_code"], json!(0), "{result}");
        assert_eq!(result["stdout"], json!("hello /work"));
        assert_eq!(result["timed_out"], json!(false));

        let work = dir.join("tk_1/work");
        fs::create_dir_all(&work).unwrap();
        let worker = executor(worker_config(&dir));
        let result = worker.run_in(&work, &code).await.unwrap();
        assert_eq!(result["exit_code"], json!(0), "{result}");
        assert_eq!(result["stdout"], json!("hello /work"));
        let kept = json!({"code": "echo kept > note && test -d /data && test ! -e /dataset"});
        let result = worker.run_in(&work, &kept).await.unwrap();
        assert_eq!(result["exit_code"], json!(0), "{result}");
        assert_eq!(fs::read_to_string(work.join("note")).unwrap(), "kept\n");
        fs::remove_dir_all(&dir).unwrap();
    }
}
