//! Lives at: src/agent/detached.rs (new), crates/agent-store (the `jobs`
//! table), and the job tools in src/agent/broker/registry.rs and jobs.rs.
//! PR 3.
//!
//! The supervisor is the one writer of `jobs` and `meter_rows`. It runs in
//! `agent serve`, starts and stops units through a `JobRunner`, polls them
//! every minute, and turns an end into `Fact::JobEnded`, which the runtime
//! applies as `Engine::job_ended`, so the Engine keeps its one writer.
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS jobs (
//!     id TEXT PRIMARY KEY NOT NULL,
//!     owner TEXT NOT NULL,            -- ticket:<id>; #107 may add operator
//!     state TEXT NOT NULL CHECK (state IN ('starting', 'running', 'stopping', 'ended')),
//!     body TEXT NOT NULL
//! ) STRICT;
//! CREATE INDEX IF NOT EXISTS jobs_live ON jobs (owner) WHERE state != 'ended';
//! ```

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use solos_agent_core::model::{AssignmentId, MicroUsd, Ms, TicketId};

use super::unit::{JobId, JobRunner};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// The row and any hold are committed; the unit is being started.
    Starting,
    Running,
    /// A stop was asked: by the worker, the tripwire, the Ticket's end or
    /// the operator.
    Stopping,
    Ended,
}

/// The `body` of a `jobs` row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobBody {
    pub ticket: TicketId,
    /// The Assignment that started it. A later one reads and stops it.
    pub started_by: AssignmentId,
    /// The first line of the command, short.
    pub command: String,
    pub started_ms: Ms,
    pub deadline_ms: Ms,
    pub quiet_ms: Ms,
    /// What the worker has read of each stream, so each byte reaches a
    /// worker once across sessions.
    pub read_stdout: u64,
    pub read_stderr: u64,
    pub last_output_ms: Option<Ms>,
    pub stop: Option<StopAsked>,
    pub end: Option<JobEnd>,
    /// Its end reached a worker through job_wait, job_stop or an Input.
    pub collected: bool,
    pub external: Option<ExternalPart>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopAsked {
    pub by: StoppedBy,
    pub at_ms: Ms,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoppedBy {
    Worker,
    Tripwire,
    TicketEnded,
    Operator,
    DiskFloor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "how")]
pub enum JobEnd {
    /// From the unit's end record.
    Unit { result: String, exit_code: Option<i32>, signal: Option<String>, at_ms: Ms },
    /// The unit vanished with no end record.
    Lost { at_ms: Ms },
}

/// A Modal job's part: its hold and its Apps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalPart {
    pub meter: String,
    pub hold_id: i64,
    pub hold_microusd: MicroUsd,
    pub burn_microusd_per_hour: MicroUsd,
    /// App ids registered to this job, with when each was first seen.
    pub apps: Vec<(String, Ms)>,
    pub metered_microusd: MicroUsd,
    /// The end of the newest hour the meter reported for its Apps.
    pub metered_until_ms: Option<Ms>,
    pub estimated_microusd: MicroUsd,
    pub settled: bool,
}

/// `job_start`'s arguments. `detach` and `external` are new.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartArgs {
    pub code: String,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub detach: bool,
    #[serde(default)]
    pub quiet_s: Option<u64>,
    #[serde(default)]
    pub external: Option<ExternalArgs>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalArgs {
    pub meter: String,
    pub burn_microusd_per_hour: MicroUsd,
}

/// Why a detached job call was refused, each with the sentence the worker
/// reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    BadArguments(String),
    /// No `executor.detached`, a node-bound Ticket, or a meter the Mandate
    /// does not hold.
    NotGranted(String),
    /// `max_running` or `max_per_ticket`.
    Full(String),
    /// A cap: with how long the job could run at its burn instead.
    NoRoom { room_hours: f64, why: String },
    /// Another Ticket holds the meter, or the meter is latched.
    MeterBusy(String),
    Unknown(String),
}

pub struct Supervisor {
    runner: Arc<dyn JobRunner>,
}

impl Supervisor {
    /// At start, before the broker serves anyone: reconcile every live row
    /// with its unit, take the ends that came while the agent was down,
    /// mark `lost` what vanished, and stop orphan units.
    pub async fn recover(&self) -> anyhow::Result<Recovered> {
        todo!()
    }

    /// `job_start {detach: true}`: check grants and limits, commit the row
    /// and any hold in one step, then start the unit.
    pub async fn start(
        &self,
        ticket: &TicketId,
        assignment: &AssignmentId,
        args: StartArgs,
    ) -> Result<Value, Refused> {
        todo!()
    }

    pub async fn list(&self, ticket: &TicketId) -> Value {
        todo!()
    }

    /// Unread output from the files, from the row's offsets, bounded; or the
    /// end, taken once.
    pub async fn wait(&self, ticket: &TicketId, job: &JobId, wait_s: u64) -> Result<Value, Refused> {
        todo!()
    }

    /// Stop the unit and the job's Apps. Returns `stopping` when they have
    /// not gone within 10 s; the poll finishes the stop.
    pub async fn stop(&self, ticket: &TicketId, job: &JobId, by: StoppedBy) -> Result<Value, Refused> {
        todo!()
    }

    /// The runtime committed a Ticket's end: stop its jobs.
    pub async fn ticket_ended(&self, ticket: &TicketId) {
        todo!()
    }

    /// Whether `ready` and `dropped` must be refused: a job still runs.
    pub async fn running(&self, ticket: &TicketId) -> usize {
        todo!()
    }

    /// Every minute: observe units, note output, take ends, run the meter's
    /// poll and the tripwire, and every ten minutes sweep orphans.
    pub async fn tick(&self, now: Ms) {
        todo!()
    }
}

pub struct Recovered {
    pub running: usize,
    pub ended_while_down: usize,
    pub lost: usize,
    pub orphans_stopped: usize,
}

#[cfg(test)]
mod tests {
    // a_detached_job_outlives_its_assignment_and_a_later_one_collects_it
    // each_byte_reaches_a_worker_once_across_sessions
    // recover_takes_an_end_that_came_while_the_agent_was_down
    // recover_marks_a_vanished_unit_lost_and_keeps_its_modal_apps_guarded
    // an_orphan_unit_is_stopped_at_start
    // the_tickets_end_stops_its_jobs
    // ready_and_dropped_are_refused_while_a_job_runs
    // another_tickets_job_id_is_unknown
    // detach_is_refused_without_executor_detached_and_on_a_node
}
