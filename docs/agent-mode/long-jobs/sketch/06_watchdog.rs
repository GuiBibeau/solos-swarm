//! Lives at: src/agent/watchdog.rs (additions), and a shared watch module
//! with #107 (src/agent/watch.rs, or wherever #107 puts its registry).
//! PRs 5 and 8.
//!
//! `assess` stays pure. Ticket jobs and #107's external jobs share one
//! shape, so `job_died` and `job_stalled` are written once.

use std::path::PathBuf;

use solos_agent_core::model::{MicroUsd, Ms, TicketId};

/// Shared with #107.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    /// Registered by the operator; only watched.
    Operator,
    /// A detached job of a Ticket; started and stopped by the agent.
    Ticket(TicketId),
}

/// Shared with #107. Ticket jobs use `Unit` and `Modal`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    Unit(String),
    Tmux(String),
    PidFile(PathBuf),
    Command(Vec<String>),
    Modal { meter: String, apps: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watched {
    pub id: String,
    pub owner: Owner,
    pub probes: Vec<Probe>,
    pub quiet_ms: Option<Ms>,
    pub last_activity_ms: Option<Ms>,
    pub ended: Option<WatchedEnd>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchedEnd {
    pub at_ms: Ms,
    /// Nobody asked for it: a signal the agent did not send, the OOM
    /// killer, a lost unit, an App stopped outside the host. Exits,
    /// timeouts and asked stops are not.
    pub unasked: bool,
    pub how: String,
}

/// What the meter tells the Watchdog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeterHealth {
    pub meter: String,
    pub holder: Option<TicketId>,
    pub latched: Option<(Ms, String)>,
    pub failures: u32,
    pub first_failure_ms: Option<Ms>,
    /// The end of the newest hour read.
    pub read_until_ms: Option<Ms>,
    pub unattributed: Vec<(Ms, String, MicroUsd)>,
}

/// New kinds beside Waiting, Stalled, Failing, Unavailable, Spend, Paused.
pub enum AlertKindAdditions {
    JobDied,
    JobStalled,
    ExternalTripped,
    ExternalStuck,
    ExternalUnattributed,
    ExternalOverrun,
    MeterFailing,
}

/// `Snapshot` gains these; `Snapshot::read` fills them from `jobs`,
/// `meter_state` and `meter_rows`.
pub struct SnapshotAdditions {
    /// Live jobs, and jobs that ended within `repeat`.
    pub watched: Vec<Watched>,
    pub meters: Vec<MeterHealth>,
}

pub struct Alert;
pub struct Thresholds;

/// Raised while the end is younger than `repeat`, with the end as its
/// start, so it is told once.
pub fn job_died(watched: &Watched, now: Ms, limits: &Thresholds) -> Option<Alert> {
    todo!()
}

/// Raised while the silence lasts, told every `repeat`; new output ends
/// it, and a later silence is a new alert.
pub fn job_stalled(watched: &Watched, default_quiet_ms: Ms, now: Ms, limits: &Thresholds) -> Option<Alert> {
    todo!()
}

pub fn meter_alerts(meter: &MeterHealth, now: Ms, limits: &Thresholds) -> Vec<Alert> {
    todo!()
}

#[cfg(test)]
mod tests {
    // a_killed_job_is_told_once_and_not_again_after_repeat
    // an_exit_a_timeout_and_a_workers_stop_are_not_alerts
    // a_stall_is_told_then_every_repeat_and_a_new_silence_is_new
    // a_job_that_died_while_the_agent_was_down_is_told_at_the_first_tick
    // an_unknown_app_and_unattributed_cost_are_told_once_each
    // a_meter_that_fails_three_times_or_goes_stale_is_told
    // the_spend_alert_counts_a_tickets_modal_cap
}
