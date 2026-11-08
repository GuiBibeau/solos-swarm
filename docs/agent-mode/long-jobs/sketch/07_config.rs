//! Lives at: src/agent/config.rs and src/agent/config/ (additions). PRs 3
//! and 6. Every field is `deny_unknown_fields`, as the rest of agent.yaml.
//!
//! Load refuses: `external` without `executor.detached`; a repository's
//! `external` entry that names no meter; either token file under a path a
//! sandbox can read; `max_burn_microusd_per_hour` or a cap of 0; a
//! `stop_grace` longer than `poll` times 10.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use solos_agent_core::model::MicroUsd;

/// `executor.detached`. Absent: no detached jobs, and `job_start` refuses
/// `detach`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetachedConfig {
    /// On the box, all Tickets. 1 to 32. Default 6.
    pub max_running: usize,
    /// Per Ticket. 1 to 8. Default 2.
    pub max_per_ticket: usize,
    /// The longest `timeout_s`, in hours. 1 to 72. Default 12.
    pub max_hours: u32,
    /// MemoryMax per job. 256 MiB to the slice's. Default 32 GiB.
    pub memory_bytes: u64,
    /// CPUQuota per job. 100 to 12800. Default 800.
    pub cpu_quota_percent: u32,
    /// TasksMax per job. Default 512.
    pub tasks_max: u32,
    /// prlimit --fsize. 1 MiB to 4 GiB. Default 256 MiB.
    pub output_bytes: u64,
    /// The stall threshold of a job that names none. Default 30m.
    pub quiet: Duration,
    /// Job ends that may relaunch an idle Ticket per person's message.
    /// 0 turns waking off. Default 8.
    pub max_wakes: u32,
}

/// `external`: meters by name.
pub type ExternalConfig = std::collections::BTreeMap<String, MeterConfig>;

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MeterConfig {
    Modal(ModalMeterConfig),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModalMeterConfig {
    /// The `modal` CLI, an absolute path, bound read-only in the meter's
    /// sandbox.
    pub modal: PathBuf,
    /// The meter's token, a one-use runtime copy. Never in a job sandbox.
    pub meter_token_file: PathBuf,
    /// The workers' token, given to a Modal job's sandbox as #99's sandbox
    /// secret.
    pub worker_token_file: PathBuf,
    /// Each Ticket's Modal spend, recorded in the Mandate at accept.
    /// Absent: each job only to its own hold.
    pub ticket_cap_microusd: Option<MicroUsd>,
    /// All Tickets' Modal spend per UTC day. Absent: none.
    pub daily_microusd: Option<MicroUsd>,
    pub max_burn_microusd_per_hour: MicroUsd,
    pub container_ceiling_microusd_per_hour: MicroUsd,
    /// Default 500000.
    pub overhead_microusd: MicroUsd,
    /// Default 1m.
    pub poll: Duration,
    /// Default 5m.
    pub stop_grace: Duration,
    /// Default 2h.
    pub settle_lag: Duration,
    /// Default 10.
    pub shape_tolerance_percent: u64,
}

/// `forge.repos.<repo>` gains:
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoAdditions {
    /// Meters this repository's Tickets may spend on. Default empty.
    #[serde(default)]
    pub external: Vec<String>,
}

/// `watchdog.thresholds` gains `job_quiet` (default: `executor.detached.quiet`)
/// and `meter_stale` (default 3h).
pub struct ThresholdAdditions {
    pub job_quiet: Option<Duration>,
    pub meter_stale: Duration,
}
