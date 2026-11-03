use crate::model::{
    Actor, AssignmentId, ChangeSet, Decision, DecisionId, EvalId, Mandate, MicroUsd, Ms, Thread,
    ThreadKey, Ticket, TicketId, Trigger, TriggerId,
};
use crate::trail::TrailCursor;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid(String),
    Missing { kind: &'static str, id: String },
    Refused(String),
    Store(String),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(why) => write!(f, "invalid: {why}"),
            Self::Missing { kind, id } => write!(f, "no {kind} {id}"),
            Self::Refused(why) => write!(f, "refused: {why}"),
            Self::Store(why) => write!(f, "store: {why}"),
        }
    }
}
impl std::error::Error for Error {}

/// Durable state. Reads return owned copies; `commit` applies one command's
/// [`ChangeSet`] (state, Effects and events together) or nothing.
pub trait Repositories {
    fn trigger(&self, id: &TriggerId) -> Option<Trigger>;
    fn thread(&self, key: &ThreadKey) -> Option<Thread>;
    fn ticket(&self, id: &TicketId) -> Option<Ticket>;
    fn decision(&self, id: &DecisionId) -> Option<Decision>;
    fn trail(&self, assignment: &AssignmentId) -> Option<TrailCursor>;
    /// Threads with queued Triggers or an Eval in progress.
    fn active_threads(&self) -> Vec<ThreadKey>;
    /// The Thread whose running Eval has this id.
    fn thread_of_eval(&self, eval: &EvalId) -> Option<ThreadKey>;
    fn commit(&mut self, changes: ChangeSet) -> Result<(), Error>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    Admit { strength: crate::model::Strength },
    Reject { reason: String },
}

/// Versioned, deterministic rules: who may originate work, how proactive the
/// agent may be, and how much it may spend before a person approves. Trigger
/// text cannot change any of it.
pub trait Policy {
    fn version(&self) -> String;
    /// Processing pause: no new Evals. The effect runner holds every Effect
    /// until the pause lifts, and Sources keep recording.
    fn paused(&self) -> bool;
    fn gate(&self, trigger: &Trigger, has_tickets: bool) -> Gate;
    /// Whether an inferred accept may act on the rules the Router matched.
    fn admits_inferred(&self, rules: &[String]) -> bool;
    /// Re-check a Mandate before an ended Ticket resumes.
    fn revalidate(&self, mandate: &Mandate) -> bool;
    fn eval_timeout_ms(&self) -> Ms;
    /// The largest budget a Ticket launches with on the Policy's authority;
    /// above it a person approves first.
    fn auto_approve_microusd(&self) -> MicroUsd;
    /// `source` is where the approval came from, such as `slack`.
    fn may_approve(&self, source: &str, actor: &Actor) -> bool;
}

pub trait Clock {
    fn now_ms(&self) -> Ms;
}
impl<F: Fn() -> Ms> Clock for F {
    fn now_ms(&self) -> Ms {
        self()
    }
}
