//! The shell side of agent mode. The Engine's ports are synchronous; these
//! are async because their real adapters talk to people, models and workers.
//! Adapters never write state: they report facts, and the runtime applies
//! them as Engine commands.

use anyhow::{Result, anyhow};
use solos_agent_core::model::{
    AssignmentId, Candidate, EvalId, Input, MicroUsd, OutboundMessage, Plan, ReportIn, TicketId,
    Trigger, Verdict,
};
use solos_agent_store::EffectId;
use std::{future::Future, pin::Pin};
use tokio::sync::mpsc;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where a Source sends Triggers, in batches.
pub type TriggerSink = mpsc::Sender<Vec<Trigger>>;

/// Yields Triggers. A Source may yield a Trigger again (after a restart, say);
/// the Engine stores each id once.
pub trait Source: Send {
    /// Send batches to `sink`, oldest first, until the Source ends or the sink
    /// closes.
    fn run(self: Box<Self>, sink: TriggerSink) -> BoxFuture<'static, Result<()>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    /// The conversation no longer exists; retrying cannot help.
    Gone,
}

pub trait Channel: Send + Sync {
    /// Send at most once per `message.id`: a repeat reports `Sent` and sends
    /// nothing.
    fn send(&self, message: OutboundMessage) -> BoxFuture<'_, Result<SendOutcome>>;
}

pub trait Router: Send + Sync {
    /// Take a Candidate. Its Verdict arrives on `facts` as [`Fact::Verdict`],
    /// now or after a model turn; `Ok` means the Router has the Candidate.
    fn assess(&self, candidate: Candidate, facts: FactSink) -> BoxFuture<'_, Result<()>>;
}

/// What a worker needs to start one Assignment.
#[derive(Clone, Debug, PartialEq)]
pub struct Launch {
    pub ticket: TicketId,
    pub assignment: AssignmentId,
    pub title: String,
    pub scope: String,
    pub plan: Plan,
    pub budget_microusd: MicroUsd,
    /// A resume or restart: check the working directory before repeating
    /// outside actions.
    pub recovery: bool,
}

/// Inputs for a running Assignment, in order.
#[derive(Clone, Debug, PartialEq)]
pub struct Delivery {
    pub ticket: TicketId,
    pub assignment: AssignmentId,
    pub inputs: Vec<Delivered>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Delivered {
    pub input: Input,
    /// The Trigger an `InputBody::Trigger` names.
    pub trigger: Option<Trigger>,
}

/// Runs Assignments. A launch, delivery or stop can be repeated after a
/// crash, so each is keyed by its Assignment and must not start a second
/// worker or hand an Input over twice.
pub trait Executor: Send + Sync {
    /// Start an Assignment. Its facts (started, then Reports for as long as
    /// it runs) go to `facts`.
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>>;
    fn deliver(&self, delivery: Delivery, facts: FactSink) -> BoxFuture<'_, Result<()>>;
    /// End an Assignment's work. Stopping one that does not run succeeds.
    fn stop(
        &self,
        ticket: TicketId,
        assignment: AssignmentId,
        reason: String,
    ) -> BoxFuture<'_, Result<()>>;
}

/// What an adapter observed, applied by the runtime as an Engine command.
#[derive(Clone, Debug, PartialEq)]
pub enum Fact {
    Verdict {
        eval: EvalId,
        verdict: Verdict,
    },
    Started {
        ticket: TicketId,
        assignment: AssignmentId,
    },
    Report {
        ticket: TicketId,
        assignment: AssignmentId,
        report: ReportIn,
    },
}

/// Facts and Effect outcomes share one queue, so the facts an adapter sends
/// before its call returns are applied before the Effect is marked done.
pub(crate) enum Inbound {
    Fact(Fact),
    Outcome {
        id: EffectId,
        attempts: u32,
        result: Result<(), String>,
    },
}

/// Where adapters send facts. It outlives the call it came with.
#[derive(Clone)]
pub struct FactSink(pub(crate) mpsc::Sender<Inbound>);

impl FactSink {
    pub async fn send(&self, fact: Fact) -> Result<()> {
        self.0
            .send(Inbound::Fact(fact))
            .await
            .map_err(|_| anyhow!("the agent runtime has stopped"))
    }
}
