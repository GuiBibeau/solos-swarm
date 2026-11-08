//! Lives at: crates/agent-core/src/model.rs and engine.rs (additions). PRs 4
//! and 6. Pure, as the rest of the core.
//!
//! A detached job's end must reach its Ticket when no worker runs. Today a
//! Note waits for the next Assignment, and only an allowed person wakes an
//! idle Ticket. `job_ended` adds the end as an Input and, within a bound,
//! starts the next Assignment itself. The bound resets only on an allowed
//! person's Input, so a worker that chains short jobs stops and asks.

use crate::model::{MicroUsd, TicketId};

// model.rs: a new Input kind. Data; grants nothing.
pub enum InputBody {
    // ... Trigger, Note, Approved, Secret as today ...
    /// A detached job of this Ticket ended: how, and the end of its output.
    Job { job: String, text: String },
}

// model.rs: Ticket gains
pub struct TicketAdditions {
    /// Assignments that job ends started since the last allowed person's
    /// Input.
    pub job_wakes: u32,
}

// model.rs: Mandate gains, fixed at accept from the repository's `external`
// list and the meter's config, never from a model's words.
pub struct MandateAdditions {
    pub external: Vec<ExternalTerms>,
}

pub struct ExternalTerms {
    /// A meter in agent.yaml's `external`, such as `modal`.
    pub meter: String,
    /// The meter's per-Ticket cap at accept. `None`: each job only to its
    /// own hold.
    pub ticket_cap_microusd: Option<MicroUsd>,
}

pub enum Woke {
    /// Delivered to the running Assignment.
    Delivered,
    /// Started the next Assignment.
    Started,
    /// Kept for the next Assignment: the Ticket waits for a person, or is
    /// delivering.
    Kept,
    /// Past `max_wakes`: kept, and the Ticket asks a person.
    Asked,
}

pub struct Engine;
pub struct Error;

impl Engine {
    /// A detached job of `ticket` ended. Adds `InputBody::Job`. On an open
    /// Ticket with no live Assignment, waiting for no person and not
    /// delivering, it starts the next Assignment (through `start`, which
    /// checks approval) while `job_wakes < max_wakes`, and otherwise sets
    /// the attention and sends it. Records `JobEnded`, and `JobWakeRefused`
    /// past the bound.
    pub fn job_ended(
        &mut self,
        ticket: &TicketId,
        job: &str,
        text: String,
        max_wakes: u32,
    ) -> Result<Woke, Error> {
        todo!()
    }
}

// engine.rs: `add_input` sets `job_wakes = 0` when `from_allowed_person`.

#[cfg(test)]
mod tests {
    // a_job_end_is_delivered_to_a_running_assignment
    // a_job_end_wakes_an_idle_ticket_and_counts
    // a_job_end_does_not_wake_a_ticket_that_waits_for_a_person
    // a_job_end_does_not_wake_a_delivering_ticket
    // past_max_wakes_the_ticket_asks_a_person_once
    // an_allowed_persons_input_resets_the_count_and_an_outsiders_does_not
    // a_job_end_on_an_ended_ticket_is_refused
    // the_differential_walk_covers_input_body_job
}
