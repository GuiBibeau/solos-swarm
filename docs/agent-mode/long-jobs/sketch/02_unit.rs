//! Lives at: src/agent/detached/unit.rs (new). PR 2.
//!
//! A detached job is a transient systemd user unit in `solos-jobs.slice`.
//! systemd holds its cgroup, so the job outlives `agent serve`, and systemd
//! enforces its wall time, memory, CPU and tasks even when the agent is
//! down. The main process is `run_code`'s prlimit and bwrap line, built by
//! the same builder, minus `--cpu` and `--die-with-parent`.
//!
//! Nothing a worker wrote reaches an argument: its code is a file the host
//! wrote, bound read-only at `/job/command.sh`, because systemd expands `%`
//! and `$` in ExecStart. The end record is written by ExecStopPost, outside
//! the sandbox, so a job cannot report its own success.

use std::path::PathBuf;
use std::time::Duration;

use crate::code::Extras;
use solos_agent_core::model::{Ms, TicketId};

/// `jd_` and a hash of the Ticket and a count. The unit is
/// `solos-job-<id>.service`, so a worker never names a unit.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub String);

impl JobId {
    pub fn unit(&self) -> String {
        todo!()
    }
}

/// Everything one unit needs. No `Debug` of a secret: the Modal token is a
/// path to a file bound read-only, never a value.
#[derive(Clone, Debug)]
pub struct UnitSpec {
    pub job: JobId,
    pub ticket: TicketId,
    /// The Ticket's `/work`.
    pub work_dir: PathBuf,
    /// `<state>/tickets/<ticket>/jobs/<job>/`: command.sh, stdout, stderr,
    /// end. Never mounted in the sandbox.
    pub job_dir: PathBuf,
    pub extras: Extras,
    pub limits: UnitLimits,
    /// What #99 grants the job's sandbox. Empty: no network, as run_code.
    pub egress: Option<EgressGrant>,
    /// `SOLOS_JOB`, so a Modal app can tag itself.
    pub env: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnitLimits {
    /// RuntimeMaxSec.
    pub wall: Duration,
    /// MemoryMax, and prlimit --as.
    pub memory_bytes: u64,
    /// CPUQuota, in percent of one core.
    pub cpu_quota_percent: u32,
    pub tasks_max: u32,
    /// prlimit --fsize: each output file and each file the job writes.
    pub output_bytes: u64,
}

/// Opaque here: #99 decides its shape (a network namespace and allowlist,
/// or a proxy in the job's own unit), and the secret file it binds.
#[derive(Clone, Debug)]
pub struct EgressGrant;

/// What systemd and the end record say of a unit now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observed {
    Running {
        /// The newest change to stdout or stderr, by size.
        last_output_ms: Option<Ms>,
        memory_peak_bytes: Option<u64>,
        cpu_usage_ns: Option<u64>,
    },
    /// The unit is gone, and its end record says how it ended.
    Ended(End),
    /// The unit is gone, and there is no end record: a reboot, or the user
    /// manager restarted.
    Lost,
}

/// From ExecStopPost's `$SERVICE_RESULT`, `$EXIT_CODE` and `$EXIT_STATUS`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct End {
    pub at_ms: Ms,
    /// `success`, `exit-code`, `signal`, `timeout`, `oom-kill`, ...
    pub result: String,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
}

/// Starts, reads and stops job units. `Systemd` runs `systemd-run --user`
/// and `systemctl --user`; `Fake` runs a plain process and writes the same
/// end record, so the supervisor's tests run on macOS.
pub trait JobRunner: Send + Sync {
    fn start(&self, spec: &UnitSpec) -> anyhow::Result<()>;
    fn observe(&self, job: &JobId, job_dir: &std::path::Path) -> anyhow::Result<Observed>;
    /// SIGTERM to the cgroup, SIGKILL after systemd's stop timeout.
    fn stop(&self, job: &JobId) -> anyhow::Result<()>;
    /// Every `solos-job-*` unit systemd has loaded, for the orphan sweep.
    fn loaded(&self) -> anyhow::Result<Vec<JobId>>;
}

pub struct Systemd {
    pub systemd_run: PathBuf,
    pub systemctl: PathBuf,
    /// The binary whose `agent job-ended` ExecStopPost runs, from a path a
    /// deploy does not move, or a fixed `sh -c` with `$$` escapes.
    pub end_record: PathBuf,
}

impl Systemd {
    /// The whole argv, for the golden test: no worker text, no `--cpu`, no
    /// secret value, `--collect`, the slice, every limit.
    pub fn argv(&self, spec: &UnitSpec) -> Vec<std::ffi::OsString> {
        todo!()
    }
}

impl JobRunner for Systemd {
    fn start(&self, spec: &UnitSpec) -> anyhow::Result<()> {
        todo!()
    }
    fn observe(&self, job: &JobId, job_dir: &std::path::Path) -> anyhow::Result<Observed> {
        todo!()
    }
    fn stop(&self, job: &JobId) -> anyhow::Result<()> {
        todo!()
    }
    fn loaded(&self) -> anyhow::Result<Vec<JobId>> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    // golden_argv_has_no_worker_text_and_no_cpu_limit
    // a_percent_or_dollar_in_the_code_reaches_only_command_sh
    // the_end_record_parses_exit_signal_timeout_and_oom
    // (Linux, on the box, #[ignore]) a_sleep_unit_outlives_its_starter,
    //   its_end_record_is_written_on_exit_signal_and_timeout,
    //   the_prefix_drop_in_reaches_a_transient_unit,
    //   bwrap_starts_under_no_new_privileges
}
