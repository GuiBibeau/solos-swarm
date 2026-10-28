//! Owned host resources and capability-free WASI; broker authorization precedes launch.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::process::Command;
use wasmtime::{Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{
    WasiCtxBuilder,
    p1::WasiP1Ctx,
    p2::pipe::{MemoryInputPipe, MemoryOutputPipe},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLimits {
    pub fuel: u64,
    pub memory_bytes: usize,
    pub deadline_ms: u64,
    pub output_bytes: usize,
}
impl ToolLimits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.fuel > 0
                && (65536..=4 * 1024 * 1024 * 1024_u64 as usize).contains(&self.memory_bytes)
                && (1..=60_000).contains(&self.deadline_ms)
                && (1..=1_048_576).contains(&self.output_bytes),
            "finite fuel/memory/time/output limits required"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolResult {
    pub tier: String,
    pub elapsed_ms: u64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub fuel_consumed: Option<u64>,
}
struct WasiState {
    ctx: WasiP1Ctx,
    limits: StoreLimits,
}
pub fn run_wasi(bytes: &[u8], input: &[u8], limits: &ToolLimits) -> Result<ToolResult> {
    limits.validate()?;
    ensure!(
        bytes.len() <= 4 * 1024 * 1024 && input.len() <= 1_048_576,
        "WASI module/input exceeds declared ingress limit"
    );
    let started = Instant::now();
    let mut config = wasmtime::Config::new();
    config
        .consume_fuel(true)
        .epoch_interruption(true)
        .wasm_memory64(false);
    let engine = Engine::new(&config)?;
    let module = Module::new(&engine, bytes)?;
    ensure!(
        started.elapsed().as_millis() < limits.deadline_ms as u128,
        "WASI compilation exceeded deadline"
    );
    let stdout = MemoryOutputPipe::new(limits.output_bytes);
    let stderr = MemoryOutputPipe::new(limits.output_bytes);
    let mut wasi = WasiCtxBuilder::new();
    wasi.stdin(MemoryInputPipe::new(input.to_vec()))
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .allow_tcp(false)
        .allow_udp(false)
        .allow_ip_name_lookup(false);
    // No preopens, inherited environment, arguments, host streams, sockets or custom host I/O.
    let state = WasiState {
        ctx: wasi.build_p1(),
        limits: StoreLimitsBuilder::new()
            .memory_size(limits.memory_bytes)
            .memories(1)
            .tables(1)
            .table_elements(4096)
            .instances(1)
            .trap_on_grow_failure(true)
            .build(),
    };
    let mut store = Store::new(&engine, state);
    store.limiter(|state| &mut state.limits);
    store.set_fuel(limits.fuel)?;
    store.set_epoch_deadline(1);
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |state: &mut WasiState| &mut state.ctx)?;
    let instance = linker.instantiate(&mut store, &module)?;
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
    let remaining = Duration::from_millis(limits.deadline_ms).saturating_sub(started.elapsed());
    let (completed, receiver) = std::sync::mpsc::sync_channel(1);
    let timer_engine = engine.clone();
    let timer = std::thread::spawn(move || {
        if receiver.recv_timeout(remaining).is_err() {
            timer_engine.increment_epoch();
        }
    });
    let outcome = start.call(&mut store, ());
    let _ = completed.send(());
    let _ = timer.join();
    match outcome {
        Ok(()) => (),
        Err(error)
            if error
                .downcast_ref::<wasmtime_wasi::I32Exit>()
                .is_some_and(|exit| exit.0 == 0) => {}
        Err(error) => {
            return Err(error
                .context("WASI execution trapped or exited unsuccessfully")
                .into());
        }
    }
    ensure!(
        stdout.contents().len() < limits.output_bytes
            && stderr.contents().len() < limits.output_bytes,
        "WASI output capacity exhausted"
    );
    Ok(ToolResult {
        tier: "W".into(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        stdout: stdout.contents().to_vec(),
        stderr: stderr.contents().to_vec(),
        fuel_consumed: Some(limits.fuel - store.get_fuel()?),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessLimits {
    pub cpu_millicores: u64,
    pub memory_bytes: u64,
    pub pids: u64,
    pub max_output_bytes: u64,
}
impl ProcessLimits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=128_000).contains(&self.cpu_millicores)
                && self.memory_bytes >= 32 * 1024 * 1024
                && (4..=4096).contains(&self.pids)
                && (1..=16 * 1024 * 1024).contains(&self.max_output_bytes),
            "invalid process resource limits"
        );
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct ProcessSpec {
    pub run_id: String,
    pub agent_id: String,
    pub incarnation: u64,
    pub expires_ms: u64,
    pub limits: ProcessLimits,
    pub node_binary: PathBuf,
    pub pi_root: PathBuf,
    pub guard_binary: PathBuf,
    pub broker_socket: PathBuf,
    pub identity_dir: PathBuf,
    pub broker_server_name: String,
    pub work_dir: PathBuf,
    pub allow_degraded_landlock: bool,
    pub args: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InnerPolicy {
    pub read_only: Vec<PathBuf>,
    pub writable: Vec<PathBuf>,
    pub executable: Vec<PathBuf>,
    pub allow_degraded_landlock: bool,
    pub attestation_path: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessPlan {
    pub unit: String,
    pub expires_ms: u64,
    pub work_dir: PathBuf,
    pub policy_path: PathBuf,
    pub command: Vec<String>,
    pub limits: ProcessLimits,
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        && id != "."
        && id != ".."
}
fn private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(temporary, path)?;
    Ok(())
}
pub fn prepare_process(spec: &ProcessSpec) -> Result<ProcessPlan> {
    ensure!(cfg!(target_os = "linux"), "Tier P requires Linux");
    ensure!(
        valid_id(&spec.run_id) && valid_id(&spec.agent_id) && spec.incarnation > 0,
        "invalid owned sandbox identity"
    );
    spec.limits.validate()?;
    ensure!(
        spec.expires_ms > crate::cluster::now_ms(),
        "sandbox deadline already expired"
    );
    for path in [&spec.node_binary, &spec.guard_binary, &spec.broker_socket] {
        ensure!(
            path.is_absolute() && path.exists(),
            "required process asset missing"
        );
    }
    ensure!(
        spec.pi_root
            .join("node_modules/@earendil-works/pi-coding-agent/dist/cli.js")
            .is_file()
            && spec.pi_root.join("extensions/mvp.ts").is_file(),
        "pinned Pi runtime missing"
    );
    ensure!(
        spec.identity_dir.is_absolute() && spec.identity_dir.is_dir(),
        "workload identity mount missing"
    );
    fs::create_dir_all(&spec.work_dir)?;
    let work_dir = spec.work_dir.canonicalize()?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&work_dir, fs::Permissions::from_mode(0o700))?;
    let writable = work_dir.join("workspace");
    fs::create_dir_all(&writable)?;
    let config = work_dir.join("config");
    fs::create_dir_all(&config)?;
    private_json(
        &config.join("settings.json"),
        &json!({"retry":{"enabled":false},"compaction":{"enabled":false},"quietStartup":true}),
    )?;
    private_json(&config.join("auth.json"), &json!({}))?;
    private_json(&config.join("models.json"), &json!({"providers":{}}))?;
    let policy = InnerPolicy {
        read_only: vec![
            "/usr/lib".into(),
            "/usr/share".into(),
            "/proc".into(),
            "/etc/ssl/certs".into(),
            "/runtime/node_modules".into(),
            "/runtime/extensions".into(),
            "/identity".into(),
            "/config".into(),
            "/guard-policy.json".into(),
        ],
        writable: vec![
            "/work".into(),
            "/tmp".into(),
            "/config".into(),
            "/dev/null".into(),
            "/dev/zero".into(),
            "/dev/urandom".into(),
        ],
        executable: vec![
            "/runtime/node".into(),
            "/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2".into(),
            "/usr/lib64/ld-linux-x86-64.so.2".into(),
        ],
        allow_degraded_landlock: spec.allow_degraded_landlock,
        attestation_path: "/work/.sandbox-attestation.json".into(),
    };
    let policy_path = work_dir.join("policy.json");
    private_json(&policy_path, &policy)?;
    let identity = format!("{}:{}:{}", spec.run_id, spec.agent_id, spec.incarnation);
    use sha2::Digest;
    let suffix = format!("{:x}", sha2::Sha256::digest(identity.as_bytes()));
    let unit = format!("solos-agent-{}.service", &suffix[..24]);
    let remaining = spec
        .expires_ms
        .checked_sub(crate::cluster::now_ms())
        .context("sandbox deadline elapsed during preparation")?
        .div_ceil(1000);
    let cpu_quota = format!(
        "{}.{:01}%",
        spec.limits.cpu_millicores / 10,
        spec.limits.cpu_millicores % 10
    );
    let mut command = vec![
        "systemd-run".into(),
        "--user".into(),
        "--quiet".into(),
        "--pipe".into(),
        "--wait".into(),
        format!("--unit={unit}"),
        "--property=Type=exec".into(),
        "--property=ExitType=cgroup".into(),
        "--property=Restart=no".into(),
        "--property=KillMode=control-group".into(),
        "--property=NoNewPrivileges=yes".into(),
        "--property=LimitCORE=0".into(),
        "--property=LimitNOFILE=65536".into(),
        "--property=UMask=0077".into(),
        "--property=TimeoutStopSec=5".into(),
        format!("--property=RuntimeMaxSec={remaining}"),
        format!("--property=CPUQuota={cpu_quota}"),
        format!("--property=MemoryMax={}", spec.limits.memory_bytes),
        format!("--property=TasksMax={}", spec.limits.pids),
        "--setenv=TOKIO_WORKER_THREADS=1".into(),
        "--".into(),
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
        "--tmpfs".into(),
        "/config".into(),
        "--dir".into(),
        "/runtime".into(),
        "--dir".into(),
        "/etc/ssl".into(),
    ];
    for (source, destination) in [
        (&spec.node_binary, "/runtime/node"),
        (&spec.guard_binary, "/guard"),
        (&spec.pi_root.join("node_modules"), "/runtime/node_modules"),
        (&spec.pi_root.join("extensions"), "/runtime/extensions"),
        (&spec.broker_socket, "/broker.sock"),
        (&spec.identity_dir, "/identity"),
        (&config.join("settings.json"), "/config/settings.json"),
        (&config.join("auth.json"), "/config/auth.json"),
        (&config.join("models.json"), "/config/models.json"),
        (&policy_path, "/guard-policy.json"),
    ] {
        command.extend([
            "--ro-bind".into(),
            source.to_string_lossy().into_owned(),
            destination.into(),
        ]);
    }
    if Path::new("/etc/ssl/certs").is_dir() {
        command.extend([
            "--ro-bind".into(),
            "/etc/ssl/certs".into(),
            "/etc/ssl/certs".into(),
        ]);
    }
    command.extend([
        "--bind".into(),
        writable.to_string_lossy().into_owned(),
        "/work".into(),
        "--clearenv".into(),
        "--setenv".into(),
        "HOME".into(),
        "/work".into(),
        "--setenv".into(),
        "PATH".into(),
        "/usr/bin:/bin".into(),
        "--setenv".into(),
        "TOKIO_WORKER_THREADS".into(),
        "1".into(),
        "--setenv".into(),
        "PI_CODING_AGENT_DIR".into(),
        "/config".into(),
        "--setenv".into(),
        "SWARM_SOCKET".into(),
        "/broker.sock".into(),
        "--setenv".into(),
        "SWARM_IDENTITY_DIR".into(),
        "/identity".into(),
        "--setenv".into(),
        "SWARM_BROKER_NAME".into(),
        spec.broker_server_name.clone(),
        "--chdir".into(),
        "/work".into(),
        "/guard".into(),
        "sandbox-enter".into(),
        "--policy".into(),
        "/guard-policy.json".into(),
        "--node".into(),
        "/runtime/node".into(),
        "--".into(),
        "/runtime/node_modules/@earendil-works/pi-coding-agent/dist/cli.js".into(),
        "--offline".into(),
        "--mode".into(),
        "rpc".into(),
        "--no-session".into(),
        "--no-builtin-tools".into(),
        "--no-extensions".into(),
        "--no-mcp".into(),
        "--no-skills".into(),
        "--no-prompt-templates".into(),
        "--no-themes".into(),
        "--no-context-files".into(),
        "--no-approve".into(),
        "--extension".into(),
        "/runtime/extensions/mvp.ts".into(),
    ]);
    ensure!(
        spec.args.len() == 6
            && spec.args[0] == "--provider"
            && spec.args[1] == "swarm"
            && spec.args[2] == "--model"
            && !spec.args[3].starts_with('-')
            && spec.args[4] == "--thinking"
            && spec.args[5] == "off",
        "only the broker registered model selection is admitted"
    );
    command.extend(spec.args.clone());
    let plan = ProcessPlan {
        unit,
        expires_ms: spec.expires_ms,
        work_dir,
        policy_path,
        command,
        limits: spec.limits.clone(),
    };
    private_json(&plan.work_dir.join("process-plan.json"), &plan)?;
    Ok(plan)
}
pub fn process_command(plan: &ProcessPlan) -> Result<Command> {
    ensure!(
        plan.expires_ms > crate::cluster::now_ms(),
        "cannot restart an expired agent"
    );
    let args = refreshed_deadline(&plan.command, plan.expires_ms)?;
    let mut command = Command::new(&args[0]);
    command
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}
fn refreshed_deadline(command: &[String], expires_ms: u64) -> Result<Vec<String>> {
    let remaining = expires_ms
        .checked_sub(crate::cluster::now_ms())
        .context("sandbox deadline elapsed at dispatch")?;
    ensure!(remaining > 0, "sandbox expired at dispatch");
    let mut args = command.to_vec();
    let property = args
        .iter_mut()
        .find(|arg| arg.starts_with("--property=RuntimeMaxSec="))
        .context("owned service deadline missing")?;
    *property = format!("--property=RuntimeMaxSec={remaining}ms");
    Ok(args)
}
pub async fn cleanup_process(plan: &ProcessPlan) -> Result<()> {
    ensure!(
        plan.unit.starts_with("solos-agent-")
            && plan.unit.ends_with(".service")
            && plan
                .unit
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-.".contains(&byte)),
        "cleanup unit is not owned"
    );
    let status = Command::new("systemctl")
        .args([
            "--user",
            "show",
            &plan.unit,
            "--property=ActiveState",
            "--no-pager",
        ])
        .output()
        .await?;
    ensure!(status.status.success(), "sandbox status unavailable");
    if !String::from_utf8_lossy(&status.stdout)
        .lines()
        .any(|line| matches!(line, "ActiveState=inactive" | "ActiveState=failed"))
    {
        let stopped = Command::new("systemctl")
            .args(["--user", "stop", &plan.unit])
            .status()
            .await?;
        ensure!(stopped.success(), "sandbox stop unconfirmed");
    }
    let final_status = Command::new("systemctl")
        .args([
            "--user",
            "show",
            &plan.unit,
            "--property=ActiveState",
            "--no-pager",
        ])
        .output()
        .await?;
    ensure!(
        final_status.status.success()
            && String::from_utf8_lossy(&final_status.stdout)
                .lines()
                .any(|line| matches!(line, "ActiveState=inactive" | "ActiveState=failed")),
        "owned sandbox remains active"
    );
    private_json(
        &plan.work_dir.join("cleanup.json"),
        &json!({"verdict":"PASS","unit":plan.unit,"evidence_preserved":true,"stopped_ms":crate::cluster::now_ms()}),
    )?;
    Ok(())
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn seccomp(allow_exec: bool) -> Result<()> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule,
    };
    use std::collections::BTreeMap;
    ensure!(
        cfg!(target_arch = "x86_64"),
        "this pinned Tier P syscall profile requires x86_64"
    );
    let allowed = [
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_openat,
        libc::SYS_close,
        libc::SYS_fstat,
        libc::SYS_newfstatat,
        libc::SYS_statx,
        libc::SYS_lseek,
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_munmap,
        libc::SYS_brk,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_ioctl,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_access,
        libc::SYS_faccessat,
        libc::SYS_faccessat2,
        libc::SYS_pipe2,
        libc::SYS_dup,
        libc::SYS_dup2,
        libc::SYS_dup3,
        libc::SYS_fcntl,
        libc::SYS_futex,
        libc::SYS_clock_gettime,
        libc::SYS_clock_getres,
        libc::SYS_nanosleep,
        libc::SYS_clock_nanosleep,
        libc::SYS_getpid,
        libc::SYS_getppid,
        libc::SYS_gettid,
        libc::SYS_getuid,
        libc::SYS_geteuid,
        libc::SYS_getgid,
        libc::SYS_getegid,
        libc::SYS_getgroups,
        libc::SYS_set_tid_address,
        libc::SYS_set_robust_list,
        libc::SYS_rseq,
        libc::SYS_madvise,
        libc::SYS_getrandom,
        libc::SYS_prlimit64,
        libc::SYS_getrlimit,
        libc::SYS_getrusage,
        libc::SYS_tgkill,
        libc::SYS_kill,
        libc::SYS_sched_yield,
        libc::SYS_sched_getaffinity,
        libc::SYS_sched_getparam,
        libc::SYS_sched_getscheduler,
        libc::SYS_sched_getattr,
        libc::SYS_getcpu,
        libc::SYS_uname,
        libc::SYS_sysinfo,
        libc::SYS_readlink,
        libc::SYS_readlinkat,
        libc::SYS_getdents64,
        libc::SYS_epoll_create1,
        libc::SYS_epoll_ctl,
        libc::SYS_epoll_wait,
        libc::SYS_epoll_pwait,
        libc::SYS_eventfd2,
        libc::SYS_poll,
        libc::SYS_ppoll,
        libc::SYS_connect,
        libc::SYS_shutdown,
        libc::SYS_getsockname,
        libc::SYS_getpeername,
        libc::SYS_setsockopt,
        libc::SYS_getsockopt,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_fsync,
        libc::SYS_fdatasync,
        libc::SYS_ftruncate,
        libc::SYS_unlinkat,
        libc::SYS_renameat,
        libc::SYS_renameat2,
        libc::SYS_mkdirat,
        libc::SYS_mkdir,
        libc::SYS_rmdir,
        libc::SYS_utimensat,
        libc::SYS_chdir,
        libc::SYS_fchdir,
        libc::SYS_getcwd,
        libc::SYS_umask,
        libc::SYS_execve,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_arch_prctl,
        libc::SYS_clone3,
    ];
    let mut rules = allowed
        .into_iter()
        .map(|number| (number, vec![]))
        .collect::<BTreeMap<_, _>>();
    if !allow_exec {
        rules.remove(&libc::SYS_execve);
        for number in [
            libc::SYS_sigaltstack,
            libc::SYS_membarrier,
            libc::SYS_mremap,
            libc::SYS_memfd_create,
        ] {
            rules.insert(number, vec![]);
        }
    }
    rules.insert(
        libc::SYS_socket,
        vec![SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::AF_UNIX as u64,
        )?])?],
    );
    let namespace_flags = (libc::CLONE_NEWUSER
        | libc::CLONE_NEWNS
        | libc::CLONE_NEWNET
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWCGROUP) as u64;
    rules.insert(
        libc::SYS_clone,
        vec![SeccompRule::new(vec![
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Qword,
                SeccompCmpOp::MaskedEq(namespace_flags),
                0,
            )?,
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Qword,
                SeccompCmpOp::MaskedEq(libc::CLONE_THREAD as u64),
                libc::CLONE_THREAD as u64,
            )?,
        ])?],
    );
    let filter: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Errno(libc::EPERM as u32),
        SeccompAction::Allow,
        std::env::consts::ARCH.try_into()?,
    )?
    .try_into()?;
    let clone3: BpfProgram = SeccompFilter::new(
        BTreeMap::from([(libc::SYS_clone3, vec![])]),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        std::env::consts::ARCH.try_into()?,
    )?
    .try_into()?;
    seccompiler::apply_filter(&clone3)?;
    seccompiler::apply_filter(&filter)?;
    Ok(())
}
#[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
fn seccomp(allow_exec: bool) -> Result<()> {
    anyhow::bail!("this vetted Tier P syscall profile requires x86_64")
}
#[cfg(target_os = "linux")]
pub(crate) fn install_inner_guard(policy: &InnerPolicy, allow_exec: bool) -> Result<()> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
        RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope,
    };
    // SAFETY: landlock_create_ruleset with a null attr and size 0 only queries the supported ABI version.
    let raw = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0,
            1,
        )
    };
    ensure!(
        raw >= 6 || policy.allow_degraded_landlock,
        "Tier P requires Landlock FS/network/abstract-IPC/signal support"
    );
    ensure!(
        raw <= 9 || policy.allow_degraded_landlock,
        "kernel Landlock ABI newer than vetted profile"
    );
    let abi = match raw {
        1 => ABI::V1,
        2 => ABI::V2,
        3 => ABI::V3,
        4 => ABI::V4,
        5 => ABI::V5,
        6 => ABI::V6,
        7 => ABI::V7,
        8 => ABI::V8,
        _ => ABI::V9,
    };
    let mut ruleset = Ruleset::default()
        .set_compatibility(if policy.allow_degraded_landlock {
            CompatLevel::BestEffort
        } else {
            CompatLevel::HardRequirement
        })
        .handle_access(AccessFs::from_all(abi))?;
    if raw >= 4 {
        ruleset = ruleset.handle_access(AccessNet::from_all(abi))?;
    }
    if raw >= 6 {
        ruleset = ruleset.scope(Scope::from_all(abi))?;
    }
    let mut created = ruleset.create()?;
    for path in &policy.read_only {
        if path.exists() {
            let access = if path.is_dir() {
                AccessFs::ReadFile | AccessFs::ReadDir
            } else {
                AccessFs::ReadFile.into()
            };
            created = created.add_rule(PathBeneath::new(PathFd::new(path)?, access))?;
        }
    }
    for path in &policy.writable {
        if path.exists() {
            let mut access = if path.is_dir() {
                AccessFs::from_all(abi)
            } else {
                AccessFs::from_file(abi)
            };
            access.remove(AccessFs::Execute);
            created = created.add_rule(PathBeneath::new(PathFd::new(path)?, access))?;
        }
    }
    for path in &policy.executable {
        if path.exists() {
            created = created.add_rule(PathBeneath::new(
                PathFd::new(path)?,
                AccessFs::Execute | AccessFs::ReadFile,
            ))?;
        }
    }
    let status = created.restrict_self()?;
    ensure!(
        status.no_new_privs
            && (status.ruleset == RulesetStatus::FullyEnforced || policy.allow_degraded_landlock),
        "sandbox restriction incomplete"
    );
    private_json(
        &policy.attestation_path,
        &json!({"tier":"P","landlock_abi":raw,"landlock":format!("{:?}",status.ruleset),"no_new_privs":status.no_new_privs,"seccomp":"architecture-checked-default-deny","network":"Unix broker only; TCP/UDP sockets denied","abstract_ipc_scoped":raw>=6,"signals_scoped":raw>=6,"degraded":policy.allow_degraded_landlock&&raw<6}),
    )?;
    seccomp(allow_exec)?;
    Ok(())
}
#[cfg(target_os = "linux")]
pub fn enter_process(policy: &InnerPolicy, node: &Path, args: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    install_inner_guard(policy, true)?;
    Err(std::process::Command::new(node).args(args).exec().into())
}
#[cfg(not(target_os = "linux"))]
pub(crate) fn install_inner_guard(_policy: &InnerPolicy, _allow_exec: bool) -> Result<()> {
    anyhow::bail!("strict worker guard requires Linux")
}
#[cfg(not(target_os = "linux"))]
pub fn enter_process(_policy: &InnerPolicy, _node: &Path, _args: &[String]) -> Result<()> {
    anyhow::bail!("Tier P requires Linux")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn seccomp_supports_read_only_working_directory_and_scheduler_queries() {
        const CHILD: &str = "SOLOS_SECCOMP_QUERY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "sandbox::tests::seccomp_supports_read_only_working_directory_and_scheduler_queries",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        // SAFETY: prctl with PR_SET_NO_NEW_PRIVS takes only integer arguments.
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
            0
        );
        seccomp(false).unwrap();
        let mut cwd = [0_i8; 4096];
        let mut parameter = libc::sched_param { sched_priority: 0 };
        let mut attributes = [0_u64; 8];
        // SAFETY: every pointer passed (cwd, parameter, attributes) refers to a live local buffer of the stated length.
        let queries_allowed = unsafe {
            !libc::getcwd(cwd.as_mut_ptr(), cwd.len()).is_null()
                && libc::sched_getparam(0, &mut parameter) == 0
                && libc::sched_getscheduler(0) >= 0
                && libc::syscall(libc::SYS_sched_getattr, 0, attributes.as_mut_ptr(), 64, 0) == 0
        };
        // SAFETY: sched_setscheduler reads the live `parameter` struct; errno is thread-local.
        let mutation_denied = unsafe {
            libc::sched_setscheduler(0, libc::SCHED_OTHER, &parameter) == -1
                && *libc::__errno_location() == libc::EPERM
        };
        // SAFETY: socket takes only integer arguments; errno is read from the calling thread.
        let network_denied = unsafe {
            libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) == -1
                && *libc::__errno_location() == libc::EPERM
        };
        // SAFETY: _exit takes no pointers and never returns; it is the sole exit path of this forked test child.
        unsafe {
            libc::_exit(if queries_allowed && mutation_denied && network_denied {
                0
            } else {
                1
            })
        };
    }
    #[test]
    fn actual_wasi_enforces_fuel_memory_and_default_denied_files() {
        let limits = ToolLimits {
            fuel: 1_000_000,
            memory_bytes: 1024 * 1024,
            deadline_ms: 1000,
            output_bytes: 1024,
        };
        let result = run_wasi(br#"(module (func (export "_start")))"#, b"", &limits).unwrap();
        assert_eq!(result.tier, "W");
        assert!(result.fuel_consumed.is_some());
        assert!(
            run_wasi(
                br#"(module (func (export "_start") (loop br 0)))"#,
                b"",
                &limits
            )
            .is_err()
        );
        assert!(
            run_wasi(
                br#"(module (memory 100) (func (export "_start")))"#,
                b"",
                &limits
            )
            .is_err()
        );
        let denied=br#"(module (import "wasi_snapshot_preview1" "path_open" (func $open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32))) (memory (export "memory") 1) (data (i32.const 0) "/etc/passwd") (func (export "_start") (if (i32.eqz (call $open (i32.const 3) (i32.const 0) (i32.const 0) (i32.const 11) (i32.const 0) (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 100))) (then unreachable))))"#;
        let denied_result = run_wasi(denied, b"", &limits);
        assert!(denied_result.is_ok(), "{denied_result:?}");
    }
    #[test]
    fn pinned_rootfs_rejects_changed_code_and_deadline_is_refreshed_at_dispatch() {
        let root =
            std::env::temp_dir().join(format!("solos-rootfs-pin-{}", crate::cluster::now_ms()));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("tool"), b"admitted").unwrap();
        let pinned = PinnedTree {
            path: root.clone(),
            sha256: PinnedTree::digest(&root).unwrap(),
        };
        pinned.verify().unwrap();
        fs::write(root.join("tool"), b"changed").unwrap();
        assert!(pinned.verify().is_err());
        let command = vec!["systemd-run".into(), "--property=RuntimeMaxSec=600".into()];
        let refreshed = refreshed_deadline(&command, crate::cluster::now_ms() + 1000).unwrap();
        let remaining = refreshed[1]
            .strip_prefix("--property=RuntimeMaxSec=")
            .unwrap()
            .strip_suffix("ms")
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(remaining > 0 && remaining <= 1000);
        assert!(refreshed_deadline(&command, 0).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn owned_directory_fd_supports_real_unix_sockets_beyond_sun_path_limit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = std::env::temp_dir()
            .join(format!("solos-socket-{}", crate::cluster::now_ms()))
            .join("a".repeat(70))
            .join("b".repeat(70));
        fs::create_dir_all(&root).unwrap();
        let socket = root.join("job.vsock");
        assert!(socket.as_os_str().as_encoded_bytes().len() > 108);
        let (_directory, short) = short_unix_path(&socket).unwrap();
        let listener = tokio::net::UnixListener::bind(&short).unwrap();
        let mut client = tokio::net::UnixStream::connect(&short).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        client.write_all(b"bounded").await.unwrap();
        let mut bytes = [0; 7];
        server.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"bounded");
        drop(listener);
        fs::remove_dir_all(root.parent().unwrap().parent().unwrap()).unwrap();
    }
}

/// Pinned assets are admitted by the operator, never selected by generated tool code.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedAsset {
    pub path: PathBuf,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedTree {
    pub path: PathBuf,
    pub sha256: String,
}
impl PinnedTree {
    pub fn digest(path: &Path) -> Result<String> {
        use sha2::Digest;
        ensure!(
            path.is_absolute() && path.is_dir(),
            "admitted rootfs directory missing"
        );
        let mut pending = vec![path.to_owned()];
        let mut files = vec![];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let metadata = entry.file_type()?;
                ensure!(
                    files.len() + pending.len() < 100000,
                    "rootfs inventory exceeds bound"
                );
                if metadata.is_dir() {
                    pending.push(entry.path());
                    files.push(entry.path());
                } else {
                    ensure!(
                        metadata.is_file() || metadata.is_symlink(),
                        "rootfs special devices must be runtime mounts"
                    );
                    files.push(entry.path());
                }
            }
        }
        files.sort();
        let mut digest = sha2::Sha256::new();
        for file in files {
            use std::os::unix::fs::PermissionsExt;
            let name = file.strip_prefix(path)?.as_os_str().as_encoded_bytes();
            digest.update((name.len() as u64).to_be_bytes());
            digest.update(name);
            let metadata = fs::symlink_metadata(&file)?;
            if metadata.is_symlink() {
                digest.update(b"link");
                digest.update(fs::read_link(&file)?.as_os_str().as_encoded_bytes());
            } else if metadata.is_dir() {
                digest.update(b"directory");
                digest.update((metadata.permissions().mode() & 0o777).to_be_bytes());
            } else {
                digest.update(b"file");
                digest.update((metadata.permissions().mode() & 0o777).to_be_bytes());
                digest.update(file_hash(&file)?);
            }
        }
        Ok(format!("{:x}", digest.finalize()))
    }
    fn verify(&self) -> Result<()> {
        ensure!(
            Self::digest(&self.path)? == self.sha256,
            "admitted rootfs digest changed"
        );
        Ok(())
    }
}
fn file_hash(path: &Path) -> Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
impl PinnedAsset {
    pub(crate) fn verify(&self) -> Result<()> {
        ensure!(
            self.path.is_absolute() && self.path.is_file(),
            "sandbox asset missing"
        );
        ensure!(
            self.sha256 == file_hash(&self.path)?,
            "sandbox asset digest changed"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeToolSpec {
    pub run_id: String,
    pub tool_id: String,
    pub incarnation: u64,
    pub expires_ms: u64,
    pub owned_dir: PathBuf,
    pub limits: ProcessLimits,
    /// Guest argv is already authorized by the effect broker; no host shell interprets it.
    #[serde(skip_serializing, default)]
    pub argv: Vec<String>,
    #[serde(skip_serializing, default)]
    pub input: Vec<u8>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MicroVmAssets {
    pub firecracker: PinnedAsset,
    pub jailer: PinnedAsset,
    pub kernel: PinnedAsset,
    pub rootfs: PinnedAsset,
    pub uid: u32,
    pub gid: u32,
    pub guest_cid: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmBrokerBinding {
    pub own_identity_dir: PathBuf,
    pub broker_socket: PathBuf,
    pub server_name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePlan {
    pub tier: String,
    pub unit: String,
    pub spec: NativeToolSpec,
    pub command: Vec<String>,
    pub job_socket: Option<PathBuf>,
    pub runtime_id: String,
    pub assets: Vec<PinnedAsset>,
    #[serde(default)]
    pub broker_binding: Option<VmBrokerBinding>,
    #[serde(default)]
    pub rootfs_tree: Option<PinnedTree>,
}
pub fn native_unit(spec: &NativeToolSpec, tier: &str) -> Result<String> {
    ensure!(tier == "V" || tier == "G", "unsupported native unit tier");
    Ok(format!(
        "solos-tool-{}-{}.service",
        tier.to_ascii_lowercase(),
        native_identity(spec)?
    ))
}
fn native_identity(spec: &NativeToolSpec) -> Result<String> {
    use sha2::Digest;
    ensure!(
        valid_id(&spec.run_id) && valid_id(&spec.tool_id) && spec.incarnation > 0,
        "invalid tool identity"
    );
    ensure!(
        spec.owned_dir.is_absolute(),
        "absolute owned directory required"
    );
    spec.limits.validate()?;
    Ok(format!(
        "{:x}",
        sha2::Sha256::digest(format!(
            "{}:{}:{}:{}",
            spec.run_id,
            spec.tool_id,
            spec.incarnation,
            spec.owned_dir.display()
        ))
    )[..24]
        .into())
}
fn validate_invocation(spec: &NativeToolSpec) -> Result<()> {
    ensure!(
        !spec.argv.is_empty()
            && spec.argv.len() <= 128
            && spec
                .argv
                .iter()
                .all(|arg| arg.len() <= 8192 && !arg.contains('\0'))
            && spec.input.len() <= 1_048_576,
        "native invocation exceeds bounds"
    );
    Ok(())
}
fn absolute_deadline_prefix(expires_ms: u64) -> Vec<String> {
    vec!["/usr/bin/python3".into(),"-I".into(),"-c".into(),"import os,sys,time; deadline=int(sys.argv[1]); sys.exit(124) if time.time_ns()//1000000>=deadline else os.execv(sys.argv[2],sys.argv[2:])".into(),expires_ms.to_string()]
}
fn native_properties(spec: &NativeToolSpec, unit: &str) -> Result<Vec<String>> {
    let remaining = spec
        .expires_ms
        .checked_sub(crate::cluster::now_ms())
        .context("tool expired during setup")?
        .div_ceil(1000);
    Ok(vec![
        "--quiet".into(),
        "--pipe".into(),
        "--wait".into(),
        format!("--unit={unit}"),
        "--property=Type=exec".into(),
        "--property=ExitType=cgroup".into(),
        "--property=Restart=no".into(),
        "--property=KillMode=control-group".into(),
        "--property=LimitCORE=0".into(),
        "--property=UMask=0077".into(),
        "--property=TimeoutStopSec=5".into(),
        format!("--property=RuntimeMaxSec={remaining}"),
        format!(
            "--property=CPUQuota={}.{:01}%",
            spec.limits.cpu_millicores / 10,
            spec.limits.cpu_millicores % 10
        ),
        format!("--property=MemoryMax={}", spec.limits.memory_bytes),
        format!("--property=TasksMax={}", spec.limits.pids),
        "--".into(),
    ])
}
/// KVM availability is hardware/device availability, not the unprivileged caller's file access.
/// A scoped privileged jailer can open the device; permission failure cannot authorize Tier G.
pub fn kvm_present() -> bool {
    cfg!(target_os = "linux") && Path::new("/dev/kvm").exists()
}
pub fn prepare_microvm(spec: &NativeToolSpec, assets: &MicroVmAssets) -> Result<NativePlan> {
    prepare_microvm_with_broker(spec, assets, None)
}
pub fn prepare_microvm_with_broker(
    spec: &NativeToolSpec,
    assets: &MicroVmAssets,
    binding: Option<VmBrokerBinding>,
) -> Result<NativePlan> {
    ensure!(cfg!(target_os = "linux"), "native tool tiers require Linux");
    let id = native_identity(spec)?;
    validate_invocation(spec)?;
    ensure!(
        spec.expires_ms > crate::cluster::now_ms(),
        "native deadline expired"
    );
    ensure!(kvm_present(), "Tier V needs an admitted KVM node");
    ensure!(
        assets.uid > 0 && assets.gid > 0 && assets.guest_cid >= 3,
        "nonroot jailer identity and unique guest CID required"
    );
    let pinned = vec![
        assets.firecracker.clone(),
        assets.jailer.clone(),
        assets.kernel.clone(),
        assets.rootfs.clone(),
    ];
    for asset in &pinned {
        asset.verify()?;
    }
    ensure!(
        spec.limits.memory_bytes >= 256 * 1024 * 1024,
        "Tier V includes guest and VMM memory"
    );
    fs::create_dir_all(&spec.owned_dir)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&spec.owned_dir, fs::Permissions::from_mode(0o700))?;
    let base = spec.owned_dir.canonicalize()?.join("jails");
    let binary = assets
        .firecracker
        .path
        .file_name()
        .context("Firecracker executable basename missing")?;
    let jail = base.join(binary).join(&id).join("root");
    ensure!(
        !jail.exists(),
        "existing microVM identity cannot be restarted implicitly"
    );
    fs::create_dir_all(&jail)?;
    for (source, name) in [
        (&assets.kernel.path, "kernel"),
        (&assets.rootfs.path, "rootfs.ext4"),
    ] {
        fs::copy(source, jail.join(name))?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(jail.join(name), fs::Permissions::from_mode(0o444))?;
    }
    let memory_mib = (spec.limits.memory_bytes / (1024 * 1024)).saturating_sub(64);
    let mut config = json!({"boot-source":{"kernel_image_path":"/kernel","boot_args":"console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=on init=/sbin/solos-guest-agent"},"drives":[{"drive_id":"rootfs","path_on_host":"/rootfs.ext4","is_root_device":true,"is_read_only":true}],"machine-config":{"vcpu_count":spec.limits.cpu_millicores.div_ceil(1000).clamp(1,32),"mem_size_mib":memory_mib,"smt":false},"vsock":{"guest_cid":assets.guest_cid,"uds_path":"/job.vsock"}});
    config["entropy"] =
        json!({"rate_limiter":{"bandwidth":{"size":65536,"one_time_burst":0,"refill_time":1000}}});
    if let Some(binding) = &binding {
        ensure!(
            binding.own_identity_dir.is_absolute()
                && binding.broker_socket.is_absolute()
                && binding.broker_socket.exists()
                && !binding.server_name.is_empty(),
            "tool broker binding incomplete"
        );
        // The VM receives no private credential material; the host signs only for this admitted tool.
        ensure!(
            binding.own_identity_dir.join("identity.json").is_file(),
            "own tool SVID missing"
        );
    }
    private_json(&jail.join("config.json"), &config)?;
    let unit = format!("solos-tool-v-{id}.service");
    let mut command = vec!["sudo".into(), "-n".into(), "systemd-run".into()];
    command.extend(native_properties(spec, &unit)?);
    command.extend(absolute_deadline_prefix(spec.expires_ms));
    command.extend([
        assets.jailer.path.to_string_lossy().into_owned(),
        "--id".into(),
        id.clone(),
        "--exec-file".into(),
        assets.firecracker.path.to_string_lossy().into_owned(),
        "--uid".into(),
        assets.uid.to_string(),
        "--gid".into(),
        assets.gid.to_string(),
        "--chroot-base-dir".into(),
        base.to_string_lossy().into_owned(),
        "--cgroup-version".into(),
        "2".into(),
        "--new-pid-ns".into(),
        "--resource-limit".into(),
        "no-file=4096".into(),
        "--".into(),
        "--no-api".into(),
        "--config-file".into(),
        "/config.json".into(),
    ]);
    let plan = NativePlan {
        tier: "V".into(),
        unit,
        spec: spec.clone(),
        command,
        job_socket: Some(jail.join("job.vsock")),
        runtime_id: id,
        assets: pinned,
        broker_binding: binding,
        rootfs_tree: None,
    };
    private_json(&spec.owned_dir.join("native-plan.json"), &plan)?;
    Ok(plan)
}
/// G is explicit policy admission on a node without KVM, never an automatic launch retry.
pub fn prepare_gvisor(
    spec: &NativeToolSpec,
    runsc: &PinnedAsset,
    rootfs: &PinnedTree,
    allow_no_kvm_fallback: bool,
) -> Result<NativePlan> {
    prepare_gvisor_with_broker(spec, runsc, rootfs, allow_no_kvm_fallback, None)
}
pub fn prepare_gvisor_with_broker(
    spec: &NativeToolSpec,
    runsc: &PinnedAsset,
    rootfs: &PinnedTree,
    allow_no_kvm_fallback: bool,
    binding: Option<VmBrokerBinding>,
) -> Result<NativePlan> {
    ensure!(cfg!(target_os = "linux"), "native tool tiers require Linux");
    let id = native_identity(spec)?;
    validate_invocation(spec)?;
    ensure!(
        spec.expires_ms > crate::cluster::now_ms(),
        "native deadline expired"
    );
    ensure!(
        allow_no_kvm_fallback && !kvm_present(),
        "Tier G requires explicit no-KVM fallback admission"
    );
    runsc.verify()?;
    rootfs.verify()?;
    ensure!(
        rootfs.path.is_absolute() && rootfs.path.is_dir(),
        "admitted OCI rootfs missing"
    );
    fs::create_dir_all(&spec.owned_dir)?;
    let bundle = spec.owned_dir.join("oci");
    ensure!(
        !bundle.exists(),
        "existing gVisor invocation cannot be replayed implicitly"
    );
    fs::create_dir(&bundle)?;
    use std::os::unix::fs::MetadataExt;
    let owner = fs::metadata(&spec.owned_dir)?;
    let mut config = json!({"ociVersion":"1.0.2","process":{"terminal":false,"user":{"uid":65534,"gid":65534},"args":spec.argv,"env":["PATH=/usr/bin:/bin","HOME=/work"],"cwd":"/work","noNewPrivileges":true,"capabilities":{"bounding":[],"effective":[],"inheritable":[],"permitted":[],"ambient":[]},"rlimits":[{"type":"RLIMIT_NOFILE","hard":4096,"soft":4096}]},"root":{"path":rootfs.path,"readonly":true},"mounts":[{"destination":"/proc","type":"proc","source":"proc","options":["nosuid","noexec","nodev"]},{"destination":"/work","type":"tmpfs","source":"tmpfs","options":["nosuid","nodev","mode=1777",format!("size={}",spec.limits.memory_bytes/4)]}],"linux":{"namespaces":[{"type":"pid"},{"type":"mount"},{"type":"network"},{"type":"ipc"},{"type":"uts"},{"type":"user"}],"uidMappings":[{"containerID":65534,"hostID":owner.uid(),"size":1}],"gidMappings":[{"containerID":65534,"hostID":owner.gid(),"size":1}],"resources":{"memory":{"limit":spec.limits.memory_bytes},"pids":{"limit":spec.limits.pids},"cpu":{"quota":spec.limits.cpu_millicores*100,"period":100000}}}});
    if let Some(binding) = &binding {
        ensure!(
            binding.own_identity_dir.is_absolute() && binding.broker_socket.is_absolute(),
            "G SDK binding must be host-owned absolute paths"
        );
        ensure!(
            binding.own_identity_dir.join("identity.json").is_file(),
            "host signer identity missing"
        );
        let relay_directory = spec.owned_dir.join("guest-broker");
        fs::create_dir(&relay_directory)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&relay_directory, fs::Permissions::from_mode(0o700))?;
        config["mounts"].as_array_mut().context("OCI mounts absent")?.push(json!({"destination":"/broker","type":"bind","source":relay_directory,"options":["rbind","ro","nosuid","nodev","noexec"]}));
        config["process"]["env"]
            .as_array_mut()
            .context("OCI env absent")?
            .push(json!("SWARM_SOCKET=/broker/sdk.sock"));
    }
    private_json(&bundle.join("config.json"), &config)?;
    let unit = format!("solos-tool-g-{id}.service");
    let mut command = vec!["systemd-run".into(), "--user".into(), "--pipe".into()];
    command.extend(native_properties(spec, &unit)?);
    command.extend(absolute_deadline_prefix(spec.expires_ms));
    command.extend([
        runsc.path.to_string_lossy().into_owned(),
        format!("--root={}", spec.owned_dir.join("runsc").display()),
        "--rootless=true".into(),
        "--network=none".into(),
        "--platform=systrap".into(),
        "run".into(),
        format!("--bundle={}", bundle.display()),
        id.clone(),
    ]);
    let plan = NativePlan {
        tier: "G".into(),
        unit,
        spec: spec.clone(),
        command,
        job_socket: None,
        runtime_id: id,
        assets: vec![runsc.clone()],
        broker_binding: binding,
        rootfs_tree: Some(rootfs.clone()),
    };
    private_json(&spec.owned_dir.join("native-plan.json"), &plan)?;
    Ok(plan)
}
async fn bounded_read<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    (&mut reader)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= limit, "sandbox output exceeded allowance");
    Ok(bytes)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestResult {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}
#[cfg(target_os = "linux")]
fn short_unix_path(path: &Path) -> Result<(fs::File, PathBuf)> {
    use std::os::fd::AsRawFd;
    let directory = fs::File::open(path.parent().context("Unix socket parent missing")?)?;
    let name = path.file_name().context("Unix socket name missing")?;
    let short = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name);
    Ok((directory, short))
}
#[cfg(not(target_os = "linux"))]
fn short_unix_path(path: &Path) -> Result<(fs::File, PathBuf)> {
    Ok((
        fs::File::open(path.parent().context("socket parent missing")?)?,
        path.into(),
    ))
}
pub async fn execute_native(plan: &NativePlan) -> Result<ToolResult> {
    for asset in &plan.assets {
        asset.verify()?;
    }
    if let Some(tree) = &plan.rootfs_tree {
        tree.verify()?;
    }
    ensure!(
        plan.spec.expires_ms > crate::cluster::now_ms(),
        "tool deadline expired"
    );
    // A resumed verifier or duplicated delivery can inspect evidence, never replay a native effect.
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut dispatch = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(plan.spec.owned_dir.join("dispatch.json"))
        .context("native invocation already dispatched or outcome unknown")?;
    dispatch.write_all(&serde_json::to_vec(&json!({"unit":plan.unit,"dispatched_ms":crate::cluster::now_ms(),"incarnation":plan.spec.incarnation}))?)?;
    dispatch.sync_all()?;
    let args = refreshed_deadline(&plan.command, plan.spec.expires_ms)?;
    let mut relay = if let Some(binding) = &plan.broker_binding {
        Some(OwnedRelay(start_vm_relay(plan, binding).await?))
    } else {
        None
    };
    let child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    if child.is_err() {
        if let Some(relay) = relay.take() {
            relay.0.abort();
        }
        remove_vm_identity(plan)?;
        private_json(
            &plan.spec.owned_dir.join("cleanup.json"),
            &json!({"verdict":"PASS","tier":plan.tier,"unit":plan.unit,"stopped_ms":crate::cluster::now_ms(),"service_never_started":true,"evidence_preserved":true}),
        )?;
    }
    let mut child = child?;
    let started = Instant::now();
    let deadline = Duration::from_millis(
        plan.spec
            .expires_ms
            .checked_sub(crate::cluster::now_ms())
            .context("tool expired at dispatch")?,
    );
    let stdout = child.stdout.take().context("tool stdout missing")?;
    let stderr = child.stderr.take().context("tool stderr missing")?;
    let output_limit = plan.spec.limits.max_output_bytes as usize;
    let mut outputs = Some(tokio::spawn(async move {
        tokio::try_join!(
            bounded_read(stdout, output_limit),
            bounded_read(stderr, output_limit)
        )
    }));
    let operation = async {
        if plan.tier == "V" {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let socket = plan.job_socket.as_ref().context("VM job socket missing")?;
            let (_socket_directory, socket) = short_unix_path(socket)?;
            let mut stream = loop {
                let handshake = async {
                    let mut stream = tokio::net::UnixStream::connect(&socket).await?;
                    stream.write_all(b"CONNECT 8000\n").await?;
                    let mut acknowledgement = Vec::new();
                    loop {
                        let byte = stream.read_u8().await?;
                        if byte == b'\n' {
                            break;
                        }
                        acknowledgement.push(byte);
                        ensure!(acknowledgement.len() < 64, "invalid vsock acknowledgement");
                    }
                    ensure!(
                        acknowledgement.starts_with(b"OK "),
                        "vsock guest connection refused"
                    );
                    Ok::<_, anyhow::Error>(stream)
                };
                if let Ok(Ok(stream)) =
                    tokio::time::timeout(Duration::from_secs(2), handshake).await
                {
                    break stream;
                }
                // No invocation bytes have been sent, so retrying guest readiness cannot repeat an effect.
                ensure!(child.try_wait()?.is_none(), "VMM exited before guest ready");
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            let mut request = json!({"argv":plan.spec.argv,"input":plan.spec.input,"timeout_ms":deadline.as_millis().min(60000),"output_bytes":output_limit,"pids":plan.spec.limits.pids});
            if let Some(binding) = &plan.broker_binding {
                request["broker_server_name"] =
                    serde_json::Value::String(binding.server_name.clone());
            }
            let body = serde_json::to_vec(&request)?;
            ensure!(
                body.len() <= 8 * 1024 * 1024,
                "native invocation frame exceeds bound"
            );
            stream.write_u32(body.len() as u32).await?;
            stream.write_all(&body).await?;
            let length = stream.read_u32().await? as usize;
            ensure!(
                length <= output_limit.saturating_mul(10) + 4096,
                "guest result exceeds frame allowance"
            );
            let mut bytes = vec![0; length];
            stream.read_exact(&mut bytes).await?;
            let result: GuestResult = serde_json::from_slice(&bytes)?;
            ensure!(
                result.exit_code == 0,
                "guest tool failed: exit {}",
                result.exit_code
            );
            ensure!(
                result.stdout.len() <= output_limit && result.stderr.len() <= output_limit,
                "guest output exceeded allowance"
            );
            Ok(ToolResult {
                tier: "V".into(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                stdout: result.stdout,
                stderr: result.stderr,
                fuel_consumed: None,
            })
        } else {
            use tokio::io::AsyncWriteExt;
            child
                .stdin
                .take()
                .context("tool input missing")?
                .write_all(&plan.spec.input)
                .await?;
            ensure!(
                child.wait().await?.success(),
                "gVisor tool execution failed"
            );
            let (stdout, stderr) = outputs.take().context("output readers missing")?.await??;
            Ok(ToolResult {
                tier: "G".into(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                stdout,
                stderr,
                fuel_consumed: None,
            })
        }
    };
    let result = tokio::time::timeout(deadline, operation)
        .await
        .context("native tool deadline reached")
        .and_then(|value| value);
    let cleanup = cleanup_native(plan).await;
    if let Some(relay) = relay {
        relay.0.abort();
    }
    let _ = tokio::time::timeout(Duration::from_secs(6), child.wait()).await;
    if let Some(outputs) = outputs {
        match tokio::time::timeout(Duration::from_secs(1), outputs).await {
            Ok(Ok(Ok((_stdout, _stderr)))) => {}
            Ok(Ok(Err(error))) => {
                cleanup?;
                return Err(error);
            }
            _ => {
                cleanup?;
                anyhow::bail!("VMM output drain unconfirmed");
            }
        }
    }
    cleanup?;
    result
}
pub async fn cleanup_native(plan: &NativePlan) -> Result<()> {
    ensure!(
        (plan.tier == "V" || plan.tier == "G")
            && plan.unit
                == format!(
                    "solos-tool-{}-{}.service",
                    plan.tier.to_lowercase(),
                    native_identity(&NativeToolSpec {
                        expires_ms: crate::cluster::now_ms() + 1,
                        ..plan.spec.clone()
                    })?
                ),
        "native cleanup identity invalid"
    );
    let before = native_unit_state(plan).await?;
    if !matches!(before.as_str(), "inactive" | "failed") {
        let mut command = native_systemctl(plan);
        ensure!(
            command.args(["stop", &plan.unit]).status().await?.success(),
            "native sandbox stop unconfirmed"
        );
    }
    ensure!(
        matches!(
            native_unit_state(plan).await?.as_str(),
            "inactive" | "failed"
        ),
        "native sandbox stopped state unconfirmed"
    );
    remove_vm_identity(plan)?;
    private_json(
        &plan.spec.owned_dir.join("cleanup.json"),
        &json!({"verdict":"PASS","tier":plan.tier,"unit":plan.unit,"stopped_ms":crate::cluster::now_ms(),"evidence_preserved":true}),
    )?;
    Ok(())
}
fn native_systemctl(plan: &NativePlan) -> Command {
    if plan.tier == "V" {
        let mut command = Command::new("sudo");
        command.args(["-n", "systemctl"]);
        command
    } else {
        let mut command = Command::new("systemctl");
        command.arg("--user");
        command
    }
}
async fn native_unit_state(plan: &NativePlan) -> Result<String> {
    let output = native_systemctl(plan)
        .args([
            "show",
            &plan.unit,
            "--property=ActiveState,LoadState",
            "--no-pager",
        ])
        .output()
        .await?;
    let text = String::from_utf8_lossy(&output.stdout);
    ensure!(
        output.status.success()
            || (text.lines().any(|line| line == "LoadState=not-found")
                && text.lines().any(|line| line == "ActiveState=inactive")),
        "native sandbox status unavailable"
    );
    Ok(text
        .lines()
        .find_map(|line| line.strip_prefix("ActiveState="))
        .context("native active state missing")?
        .to_owned())
}
fn remove_vm_identity(plan: &NativePlan) -> Result<()> {
    if plan.tier == "G" {
        let socket = plan.spec.owned_dir.join("guest-broker/sdk.sock");
        if socket.exists() {
            fs::remove_file(socket)?;
        }
    }
    if let Some(socket) = &plan.job_socket {
        let image = socket
            .parent()
            .context("VM jail missing")?
            .join("own-identity.ext4");
        if image.exists() {
            fs::remove_file(image)?;
        }
        let relay = PathBuf::from(format!("{}_5001", socket.display()));
        if relay.exists() {
            fs::remove_file(relay)?;
        }
    }
    Ok(())
}

struct OwnedRelay(tokio::task::JoinHandle<()>);
impl Drop for OwnedRelay {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn start_vm_relay(
    plan: &NativePlan,
    binding: &VmBrokerBinding,
) -> Result<tokio::task::JoinHandle<()>> {
    let path = if plan.tier == "G" {
        plan.spec.owned_dir.join("guest-broker/sdk.sock")
    } else {
        PathBuf::from(format!(
            "{}_5001",
            plan.job_socket
                .as_ref()
                .context("VM relay requires owned vsock")?
                .display()
        ))
    };
    let (directory, short_path) = short_unix_path(&path)?;
    let listener = tokio::net::UnixListener::bind(&short_path)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let binding = binding.clone();
    let expiry = plan.spec.expires_ms;
    Ok(tokio::spawn(async move {
        let _directory = directory;
        let mut children = tokio::task::JoinSet::new();
        let mut admitted_requests = 0usize;
        loop {
            let accepted = tokio::select! {accepted=listener.accept()=>accepted,_=children.join_next(),if !children.is_empty()=>continue,_=tokio::time::sleep(Duration::from_millis(expiry.saturating_sub(crate::cluster::now_ms())))=>break};
            let Ok((incoming, _)) = accepted else { break };
            if children.len() >= 8 || admitted_requests >= 8 {
                drop(incoming);
                continue;
            }
            admitted_requests += 1;
            let binding = binding.clone();
            children.spawn(async move {
                let operation = host_signed_request(incoming, &binding);
                let _ = tokio::time::timeout(
                    Duration::from_millis(
                        expiry.saturating_sub(crate::cluster::now_ms()).min(60000),
                    ),
                    operation,
                )
                .await;
            });
        }
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestBrokerRequest {
    request_id: String,
    path: String,
    payload: serde_json::Value,
}
async fn host_signed_request(
    mut incoming: tokio::net::UnixStream,
    binding: &VmBrokerBinding,
) -> Result<()> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use ed25519_dalek::{Signer, SigningKey, pkcs8::DecodePrivateKey};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use zeroize::Zeroizing;
    let size = incoming.read_u32().await? as usize;
    ensure!(size <= 1024 * 1024, "guest broker frame exceeds bound");
    let mut body = vec![0; size];
    incoming.read_exact(&mut body).await?;
    let request: GuestBrokerRequest = serde_json::from_slice(&body)?;
    ensure!(
        !request.request_id.is_empty()
            && request.request_id.len() <= 128
            && !request.request_id.contains(['\n', '\r'])
            && request.path.starts_with('/')
            && request.path.len() <= 128
            && !request.path.contains(['\n', '\r']),
        "invalid guest broker request"
    );
    let bytes = Zeroizing::new(fs::read(binding.own_identity_dir.join("identity.json"))?);
    ensure!(bytes.len() <= 32768, "tool identity exceeds bound");
    let mut identity: serde_json::Value = serde_json::from_slice(&bytes)?;
    let private_pem = match identity
        .as_object_mut()
        .context("tool identity malformed")?
        .remove("private_key_pem")
    {
        Some(serde_json::Value::String(value)) => Zeroizing::new(value),
        _ => anyhow::bail!("tool key missing"),
    };
    let key = rustls_pemfile::private_key(&mut std::io::Cursor::new(private_pem.as_bytes()))?
        .context("tool PKCS8 key absent")?;
    let signing = SigningKey::from_pkcs8_der(key.secret_der())?;
    let certs = rustls_pemfile::certs(&mut std::io::Cursor::new(
        identity["certificate_pem"]
            .as_str()
            .context("tool certificate missing")?
            .as_bytes(),
    ))
    .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut roots = rustls::RootCertStore::empty();
    for ca in rustls_pemfile::certs(&mut std::io::Cursor::new(
        identity["ca_pem"]
            .as_str()
            .context("tool CA missing")?
            .as_bytes(),
    )) {
        roots.add(ca?)?;
    }
    let mut tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_root_certificates(roots)
    .with_client_auth_cert(certs, key)?;
    tls.enable_early_data = false;
    let payload = serde_json::to_string(&request.payload)?;
    let issued_ms = crate::cluster::now_ms();
    let signature = STANDARD.encode(
        signing
            .sign(
                format!(
                    "solos-swarm-request-v1\n{}\n{}\n{}\n{}",
                    request.request_id, issued_ms, request.path, payload
                )
                .as_bytes(),
            )
            .to_bytes(),
    );
    let body = serde_json::to_vec(
        &json!({"request_id":request.request_id,"issued_ms":issued_ms,"path":request.path,"payload":payload,"signature":signature}),
    )?;
    let socket = tokio::net::UnixStream::connect(&binding.broker_socket).await?;
    let name = rustls::pki_types::ServerName::try_from(binding.server_name.clone())?;
    let mut secure = tokio_rustls::TlsConnector::from(std::sync::Arc::new(tls))
        .connect(name, socket)
        .await?;
    secure.write_all(format!("POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",request.path,binding.server_name,body.len()).as_bytes()).await?;
    secure.write_all(&body).await?;
    let mut header = Vec::new();
    loop {
        header.push(secure.read_u8().await?);
        ensure!(header.len() <= 16384, "broker header exceeds bound");
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header = std::str::from_utf8(&header)?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("broker status missing")?
        .parse::<u16>()?;
    let length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then_some(value.trim())
        })
        .context("broker bounded Content-Length missing")?
        .parse::<usize>()?;
    ensure!(length <= 4 * 1024 * 1024, "broker response exceeds bound");
    let mut response = vec![0; length];
    secure.read_exact(&mut response).await?;
    let result = serde_json::to_vec(
        &json!({"status":status,"body":serde_json::from_slice::<serde_json::Value>(&response)?}),
    )?;
    incoming.write_u32(result.len() as u32).await?;
    incoming.write_all(&result).await?;
    incoming.shutdown().await?;
    Ok(())
}
/// Explicit scoped cancellation plus a Drop fallback for an interrupted executor future.
pub async fn execute_native_scoped(
    plan: &NativePlan,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<ToolResult> {
    struct Cleanup {
        plan: NativePlan,
        done: bool,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if !self.done
                && let Ok(runtime) = tokio::runtime::Handle::try_current()
            {
                let plan = self.plan.clone();
                runtime.spawn(async move {
                    let _ = cleanup_native(&plan).await;
                });
            }
        }
    }
    let mut cleanup = Cleanup {
        plan: plan.clone(),
        done: false,
    };
    let outcome = tokio::select! {result=execute_native(plan)=>result,_=async {loop {if *cancel.borrow(){break;}
    if cancel.changed().await.is_err(){break;}}}=>Err(anyhow::anyhow!("native tool cancelled; external outcome requires receipt reconciliation"))};
    cleanup_native(plan).await?;
    cleanup.done = true;
    outcome
}
