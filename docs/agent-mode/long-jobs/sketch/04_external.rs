//! Lives at: src/agent/external/ (new): meter.rs, modal.rs, guard.rs,
//! tripwire.rs. PRs 6 and 7.
//!
//! The host runs every Modal command here, in a sandbox it builds with the
//! host network, an empty environment, `modal` read-only and the meter
//! token bound read-only, as the dependency install runs. The commands come
//! from agent.yaml; no worker reaches this sandbox, its token or its
//! output, and the output is parsed into integers here and never read by a
//! model.

use solos_agent_core::model::{MicroUsd, Ms, TicketId};

/// One row of `modal billing report --resolution h --json`: one App in one
/// closed hour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeterRow {
    pub interval_ms: Ms,
    /// The App id (`object_id`).
    pub object: String,
    pub environment: String,
    /// Modal's Decimal `cost`, parsed from its text and rounded up.
    pub cost_microusd: MicroUsd,
    /// The `solos_job` tag, when the App set one.
    pub job_tag: Option<String>,
}

/// An App as `modal app list --json` and `modal container list --json` show
/// it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveApp {
    pub id: String,
    pub running: bool,
    /// Each running container's price per hour: from its GPU and the rates,
    /// or `container_ceiling_microusd_per_hour` when the listing does not
    /// name its GPU.
    pub containers: Vec<MicroUsd>,
}

/// An external meter. Modal is the one adapter; another provider is
/// another `kind`.
pub trait ExternalMeter: Send + Sync {
    /// Closed hours from `from_ms` to `to_ms`, every App in the workspace.
    fn report(&self, from_ms: Ms, to_ms: Ms) -> anyhow::Result<Vec<MeterRow>>;
    fn live(&self) -> anyhow::Result<Vec<LiveApp>>;
    /// `modal app stop <id> --yes`.
    fn stop(&self, app: &str) -> anyhow::Result<()>;
}

/// `"12.3456789"` in USD to micro-USD, rounded up, with no float. Refuses
/// a negative, an exponent, or more than 18 integer digits.
pub fn usd_text_to_microusd(text: &str) -> Result<MicroUsd, String> {
    todo!()
}

/// burn × timeout / 3600, rounded up, plus the overhead. Saturates.
pub fn hold_microusd(burn_microusd_per_hour: MicroUsd, timeout_s: u64, overhead: MicroUsd) -> MicroUsd {
    todo!()
}

/// How long a job at `burn` could run in `room`, for a refusal.
pub fn room_hours(room_microusd: MicroUsd, burn_microusd_per_hour: MicroUsd, overhead: MicroUsd) -> f64 {
    todo!()
}

/// What the tripwire reads, for one Modal job, at one poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    pub hold_microusd: MicroUsd,
    pub declared_burn_microusd_per_hour: MicroUsd,
    /// The cost of its Apps in hours the meter reported.
    pub metered_microusd: MicroUsd,
    /// Observed burn integrated since the last reported hour.
    pub estimated_microusd: MicroUsd,
    /// The running containers of its Apps, each at its price, now.
    pub observed_burn_microusd_per_hour: MicroUsd,
    pub stop_grace_ms: Ms,
    pub deadline_ms: Ms,
    pub shape_tolerance_percent: u64,
    pub now_ms: Ms,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trip {
    /// metered + estimated + observed burn × stop grace reached the hold.
    Hold,
    /// The deadline, less the stop grace, has come.
    Deadline,
    /// It burns more than it declared, past the tolerance.
    Shape,
}

/// Pure: whether to stop the job now, and why.
pub fn tripwire(reading: &Reading) -> Option<Trip> {
    todo!()
}

/// Who owns each App. One holder at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub ticket: TicketId,
    pub since_ms: Ms,
    /// Live Modal jobs of the holder, with their launch windows.
    pub jobs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placed {
    /// The App is one of the holder's jobs': by its tag, by the one launcher
    /// that ran, or else the holder's first live job.
    Job(String),
    /// No holder, or the holder released: stop it, latch the meter, alert.
    Unknown,
}

/// Pure: place a newly seen App.
pub fn place(app: &LiveApp, tag: Option<&str>, holder: Option<&Holder>) -> Placed {
    todo!()
}

/// Pure: for one reported hour, each row's owner, and the unattributed rest,
/// so the workspace total equals the sum.
pub fn attribute(rows: &[MeterRow], registered: &[(String, String)]) -> Attribution {
    todo!()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attribution {
    /// (job, cost) for each job with rows in the hour.
    pub jobs: Vec<(String, MicroUsd)>,
    /// Rows no live or recent job owns: charged to `external:<meter>`.
    pub unattributed: Vec<MeterRow>,
}

#[cfg(test)]
mod tests {
    // usd_text_rounds_up_and_refuses_floats_exponents_and_negatives
    // hold_is_burn_times_hours_plus_overhead_and_saturates
    // tripwire_table: under, at the hold, deadline, shape over tolerance,
    //   a reported hour replaces its estimate
    // place: tagged, one launcher, holder released, no holder
    // attribute: handover inside one hour, conservation, late rows
    // parsers against recorded fixtures of report, app list, container list
    //   and rates (numbers redacted), including a field Modal adds later
}
