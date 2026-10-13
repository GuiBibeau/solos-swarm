//! Arbitrary Wasm compilation and execution run outside the broker in a bounded owned process.
use crate::{
    cluster::now_ms,
    sandbox::{InnerPolicy, ToolLimits, ToolResult},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::watch,
};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    module: String,
    input: String,
    limits: ToolLimits,
    expires_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    ok: bool,
    error: Option<String>,
    tier: String,
    elapsed_ms: u64,
    stdout: String,
    stderr: String,
    fuel_consumed: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Plan {
    unit: String,
    owned_dir: PathBuf,
    expires_ms: u64,
    module_sha256: String,
    input_sha256: String,
    limits: ToolLimits,
    memory_max: u64,
    pids_max: u64,
    cpu_millicores: u64,
}
pub fn unit_for_directory(path: &Path) -> Result<String> {
    ensure!(path.is_absolute(), "absolute WASI directory required");
    Ok(format!(
        "solos-wasi-{}.service",
        &hex(&crate::audit::hash(path.as_os_str().as_encoded_bytes()))[..24]
    ))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    Ok(())
}
/// CLI only: the namespace/cgroup exist before module bytes enter this process.
pub fn worker_entry(policy_path: &Path) -> Result<()> {
    let policy: InnerPolicy = serde_json::from_slice(&fs::read(policy_path)?)?;
    crate::sandbox::install_inner_guard(&policy, false)?;
    // WASI exposes memory streams and timers, so its host executor needs no signal/network I/O.
    // Build after sealing: every executor thread inherits the same filesystem/syscall guard.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build()
        .context("WASI timer executor startup failed")?;
    let _runtime_scope = runtime.enter();
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 8 * 1024 * 1024,
        "WASI worker ingress exceeded bound"
    );
    let job: Job = serde_json::from_slice(&bytes)?;
    job.limits.validate()?;
    ensure!(
        job.expires_ms > now_ms(),
        "WASI absolute deadline expired before compilation"
    );
    let module = STANDARD.decode(job.module)?;
    let input = STANDARD.decode(job.input)?;
    let mut limits = job.limits.clone();
    limits.deadline_ms = limits
        .deadline_ms
        .min(job.expires_ms.saturating_sub(now_ms()));
    let response = match crate::sandbox::run_wasi(&module, &input, &limits) {
        Ok(result) => Response {
            ok: true,
            error: None,
            tier: result.tier,
            elapsed_ms: result.elapsed_ms,
            stdout: STANDARD.encode(result.stdout),
            stderr: STANDARD.encode(result.stderr),
            fuel_consumed: result.fuel_consumed,
        },
        Err(_) => Response {
            ok: false,
            error: Some("WASI compilation or execution refused".into()),
            tier: "W".into(),
            elapsed_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
            fuel_consumed: None,
        },
    };
    std::io::stdout().write_all(&serde_json::to_vec(&response)?)?;
    std::io::stdout().flush()?;
    Ok(())
}
async fn stopped(plan: &Plan) -> Result<bool> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            &plan.unit,
            "--property=ActiveState,LoadState",
            "--no-pager",
        ])
        .output()
        .await?;
    let text = String::from_utf8_lossy(&output.stdout);
    ensure!(
        output.status.success() || text.contains("LoadState=not-found"),
        "WASI status unavailable"
    );
    Ok(text
        .lines()
        .any(|line| matches!(line, "ActiveState=inactive" | "ActiveState=failed")))
}
async fn cleanup(plan: &Plan) -> Result<()> {
    if !stopped(plan).await? {
        ensure!(
            Command::new("systemctl")
                .args(["--user", "stop", &plan.unit])
                .status()
                .await?
                .success(),
            "WASI owned unit stop failed"
        );
    }
    ensure!(stopped(plan).await?, "WASI cleanup unconfirmed");
    let path = plan.owned_dir.join("cleanup.json");
    if !path.exists() {
        save(
            &path,
            &json!({"verdict":"PASS","unit":plan.unit,"stopped_ms":now_ms(),"evidence_preserved":true}),
        )?;
    }
    Ok(())
}
struct CleanupGuard {
    plan: Plan,
    done: bool,
}
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if !self.done
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let plan = self.plan.clone();
            handle.spawn(async move {
                let _ = cleanup(&plan).await;
            });
        }
    }
}
async fn bounded<R: tokio::io::AsyncRead + Unpin>(mut reader: R, max: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut reader)
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= max, "WASI worker output exceeded bound");
    Ok(bytes)
}
#[allow(clippy::too_many_arguments)] // Execution limits, owned resources and cancellation remain explicit.
pub async fn run_isolated(
    module: &[u8],
    input: &[u8],
    limits: &ToolLimits,
    owned_dir: &Path,
    expires_ms: u64,
    cancel: watch::Receiver<bool>,
) -> Result<ToolResult> {
    run_with_path(
        module,
        input,
        limits,
        owned_dir,
        expires_ms,
        cancel,
        &std::env::current_exe()?.canonicalize()?,
    )
    .await
}
/// Explicit installed/test worker. Its executable digest must be approved before use.
#[allow(clippy::too_many_arguments)]
pub async fn run_isolated_with_executable(
    module: &[u8],
    input: &[u8],
    limits: &ToolLimits,
    owned_dir: &Path,
    expires_ms: u64,
    cancel: watch::Receiver<bool>,
    worker: &crate::sandbox::PinnedAsset,
) -> Result<ToolResult> {
    worker.verify()?;
    run_with_path(
        module,
        input,
        limits,
        owned_dir,
        expires_ms,
        cancel,
        &worker.path.canonicalize()?,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
async fn run_with_path(
    module: &[u8],
    input: &[u8],
    limits: &ToolLimits,
    owned_dir: &Path,
    expires_ms: u64,
    mut cancel: watch::Receiver<bool>,
    executable: &Path,
) -> Result<ToolResult> {
    let started = std::time::Instant::now();
    ensure!(
        cfg!(target_os = "linux"),
        "isolated WASI compilation requires Linux"
    );
    limits.validate()?;
    ensure!(
        module.len() <= 4 * 1024 * 1024 && input.len() <= 1_048_576 && owned_dir.is_absolute(),
        "WASI ingress or owned directory invalid"
    );
    ensure!(
        expires_ms > now_ms() && !*cancel.borrow(),
        "WASI invocation already expired or cancelled"
    );
    fs::create_dir_all(owned_dir)?;
    use std::os::unix::fs::PermissionsExt;
    ensure!(
        !fs::symlink_metadata(owned_dir)?.file_type().is_symlink(),
        "WASI owned directory symlink denied"
    );
    fs::set_permissions(owned_dir, fs::Permissions::from_mode(0o700))?;
    let plan = Plan {
        unit: unit_for_directory(owned_dir)?,
        owned_dir: owned_dir.into(),
        expires_ms: expires_ms.min(now_ms().saturating_add(limits.deadline_ms)),
        module_sha256: hex(&crate::audit::hash(module)),
        input_sha256: hex(&crate::audit::hash(input)),
        limits: limits.clone(),
        memory_max: (limits.memory_bytes as u64)
            .checked_add(512 * 1024 * 1024)
            .context("WASI compiler memory overflow")?,
        pids_max: 64,
        cpu_millicores: 1000,
    };
    save(&owned_dir.join("wasi-plan.json"), &plan)?; // A duplicate invocation can recover evidence; it cannot execute again.
    let policy = InnerPolicy {
        read_only: vec!["/usr/lib".into(), "/usr/share".into(), "/proc".into()],
        writable: vec!["/work".into(), "/tmp".into()],
        executable: vec![],
        allow_degraded_landlock: false,
        attestation_path: "/work/isolation.json".into(),
    };
    save(&owned_dir.join("guard.json"), &policy)?;
    let mut guard = CleanupGuard {
        plan: plan.clone(),
        done: false,
    };
    let deadline = plan.expires_ms.saturating_sub(now_ms());
    ensure!(deadline > 0, "WASI setup exhausted deadline");
    let mut args = vec![
        "--user".to_string(),
        "--pipe".into(),
        "--wait".into(),
        "--collect".into(),
        format!("--unit={}", plan.unit),
        "--property=Type=exec".into(),
        "--property=ExitType=cgroup".into(),
        "--property=Restart=no".into(),
        "--property=NoNewPrivileges=yes".into(),
        "--property=LimitCORE=0".into(),
        "--property=UMask=0077".into(),
        "--property=KillMode=control-group".into(),
        "--property=TimeoutStopSec=5".into(),
        format!("--property=RuntimeMaxSec={deadline}ms"),
        "--property=CPUQuota=100%".into(),
        format!("--property=MemoryMax={}", plan.memory_max),
        format!("--property=TasksMax={}", plan.pids_max),
        "bwrap".into(),
        "--unshare-all".into(),
        "--die-with-parent".into(),
        "--new-session".into(),
        "--ro-bind".into(),
        "/usr".into(),
        "/usr".into(),
        "--symlink".into(),
        "usr/lib".into(),
        "/lib".into(),
        "--symlink".into(),
        "usr/lib64".into(),
        "/lib64".into(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--bind".into(),
        owned_dir.to_string_lossy().into_owned(),
        "/work".into(),
        "--ro-bind".into(),
        executable.to_string_lossy().into_owned(),
        "/worker".into(),
        "--ro-bind".into(),
        owned_dir.join("guard.json").to_string_lossy().into_owned(),
        "/guard.json".into(),
        "--clearenv".into(),
        "--setenv".into(),
        "RAYON_NUM_THREADS".into(),
        "1".into(),
        "--setenv".into(),
        "TOKIO_WORKER_THREADS".into(),
        "1".into(),
        "--chdir".into(),
        "/work".into(),
        "/worker".into(),
        "wasi-worker".into(),
        "--policy".into(),
        "/guard.json".into(),
    ];
    // No host identity, broker socket, configuration, home or module file is mounted.
    let job = serde_json::to_vec(&Job {
        module: STANDARD.encode(module),
        input: STANDARD.encode(input),
        limits: limits.clone(),
        expires_ms: plan.expires_ms,
    })?;
    ensure!(
        job.len() <= 8 * 1024 * 1024,
        "WASI encoded ingress too large"
    );
    save(
        &owned_dir.join("dispatch.json"),
        &json!({"unit":plan.unit,"submitted_ms":now_ms(),"expires_ms":plan.expires_ms}),
    )?;
    let mut child = Command::new("systemd-run")
        .args(args.drain(..))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().context("WASI input pipe absent")?;
    let stdout = child.stdout.take().context("WASI output pipe absent")?;
    let stderr = child.stderr.take().context("WASI error pipe absent")?;
    let stdout = tokio::spawn(bounded(
        stdout,
        limits.output_bytes.saturating_mul(3) + 8192,
    ));
    let stderr = tokio::spawn(bounded(stderr, 65536));
    let operation = async {
        stdin.write_all(&job).await?;
        stdin.shutdown().await?;
        drop(stdin);
        let status = child.wait().await?;
        let raw_stdout = stdout.await??;
        let raw_stderr = stderr.await??;
        if !status.success() {
            let digest = retain_failure(
                owned_dir,
                "worker_exit",
                status.code(),
                &raw_stdout,
                &raw_stderr,
                None,
            )?;
            anyhow::bail!(
                "isolated WASI worker failed or exceeded resource limits; private evidence {digest}"
            );
        }
        let response: Response = match serde_json::from_slice(&raw_stdout) {
            Ok(response) => response,
            Err(error) => {
                let digest = retain_failure(
                    owned_dir,
                    "worker_protocol",
                    status.code(),
                    &raw_stdout,
                    &raw_stderr,
                    Some(&error.to_string()),
                )?;
                anyhow::bail!("isolated WASI worker response invalid; private evidence {digest}");
            }
        };
        if !response.ok {
            let digest = retain_failure(
                owned_dir,
                "worker_execution",
                status.code(),
                &raw_stdout,
                &raw_stderr,
                response.error.as_deref(),
            )?;
            anyhow::bail!("isolated WASI worker refused execution; private evidence {digest}");
        }
        let output = STANDARD.decode(response.stdout)?;
        let error = STANDARD.decode(response.stderr)?;
        ensure!(
            output.len() <= limits.output_bytes && error.len() <= limits.output_bytes,
            "WASI result exceeds bound"
        );
        Ok::<_, anyhow::Error>(ToolResult {
            tier: response.tier,
            elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            stdout: output,
            stderr: error,
            fuel_consumed: response.fuel_consumed,
        })
    };
    let result = tokio::select! {result=tokio::time::timeout(Duration::from_millis(deadline+1000),operation)=>result.context("isolated WASI compile/run deadline reached").and_then(|value|value),_=async{loop {
        if *cancel.borrow() {break;}
        if cancel.changed().await.is_err() {break;}
    }}=>Err(anyhow::anyhow!("isolated WASI cancelled"))};
    cleanup(&plan).await?;
    guard.done = true;
    result
}

fn retain_failure(
    owned_dir: &Path,
    phase: &str,
    exit_code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    worker_error: Option<&str>,
) -> Result<String> {
    let evidence = json!({
        "schema_version":1,"phase":phase,"exit_code":exit_code,
        "stdout_sha256":hex(&crate::audit::hash(stdout)),"stdout_bytes":stdout.len(),
        "stderr_sha256":hex(&crate::audit::hash(stderr)),"stderr_base64":STANDARD.encode(stderr),
        "worker_error":worker_error.map(|error| error.chars().take(8192).collect::<String>())
    });
    let digest = hex(&crate::audit::content_hash(&evidence)?);
    save(&owned_dir.join("failure.json"), &evidence)?;
    Ok(digest)
}

#[cfg(all(test, unix))]
mod failure_evidence_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn private_failure_is_bounded_immutable_and_does_not_return_payload() {
        let root =
            std::env::temp_dir().join(format!("solos-wasi-failure-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let sensitive = b"synthetic-private-output";
        let message = "synthetic-private-error".repeat(1000);
        let digest = retain_failure(
            &root,
            "worker_exit",
            Some(101),
            sensitive,
            b"synthetic-stderr",
            Some(&message),
        )
        .unwrap();
        assert_eq!(digest.len(), 64);
        assert!(!digest.contains("synthetic"));
        let path = root.join("failure.json");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let bytes = fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-private-output"));
        let evidence: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            evidence["worker_error"].as_str().unwrap().chars().count(),
            8192
        );
        assert_eq!(digest, hex(&crate::audit::content_hash(&evidence).unwrap()));
        assert!(retain_failure(&root, "replace", None, b"changed", b"", None).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::remove_dir_all(root).unwrap();
    }
}
