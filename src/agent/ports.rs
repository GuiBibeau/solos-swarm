//! The shell side of agent mode. The Engine's ports are synchronous; these
//! are async because their real adapters talk to people, models and workers.
//! Adapters never write state: they report facts, and the runtime applies
//! them as Engine commands.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use solos_agent_core::change::RepoName;
use solos_agent_core::model::{
    Actor, Ask, AssignmentId, Candidate, ChannelRef, Decision, EvalId, Input, MicroUsd,
    OutboundMessage, Plan, ReportIn, SecretGrant, SecretId, TicketId, Trigger, TriggerId, Verdict,
};
use solos_agent_core::trail::{TrailAction, TrailOutcome};
use solos_agent_store::EffectId;
use std::path::Path;
use std::time::Duration;
use std::{future::Future, pin::Pin};
use tokio::sync::{mpsc, oneshot};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where a Source sends Triggers, in batches.
pub type TriggerSink = mpsc::Sender<Vec<Trigger>>;

/// Yields Triggers. A Source may yield a Trigger again (after a restart, say);
/// the Engine stores each id once.
pub trait Source: Send {
    /// Send batches to `sink`, oldest first, until the Source ends or the sink
    /// closes. Commands people give through the Source's own controls go to
    /// `facts` as [`Fact::Command`].
    fn run(self: Box<Self>, sink: TriggerSink, facts: FactSink) -> BoxFuture<'static, Result<()>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    /// The conversation no longer exists; retrying cannot help.
    Gone,
}

/// What the runtime knows about a message beyond its content.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SendOptions {
    /// An earlier attempt may have sent it: one failed, or a crash ended the
    /// last run before the message was marked sent.
    pub repeat: bool,
    /// What a Channel with buttons offers for the message, filtered by the
    /// runtime to the asks that still stand, so a Send retried after the
    /// answer posts a plain message. Channels without buttons ignore it.
    pub ask: Option<Ask>,
}

pub trait Channel: Send + Sync {
    /// Send at most once per `message.id`: a repeat reports `Sent` and sends
    /// nothing.
    fn send(
        &self,
        message: OutboundMessage,
        options: SendOptions,
    ) -> BoxFuture<'_, Result<SendOutcome>>;
}

/// An adapter error that says when to try again, such as an HTTP 429's
/// `Retry-After`. The runtime waits at least this long before the retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryAfter(pub Duration);

impl std::fmt::Display for RetryAfter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rate limited; retry after {}s", self.0.as_secs())
    }
}

impl std::error::Error for RetryAfter {}

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
    /// The Trigger the Ticket was accepted from.
    pub origin: Trigger,
    /// The proactive rules the Router matched, for inferred work.
    pub rules: Vec<String>,
    pub plan: Plan,
    /// The Ticket's whole budget, or `None` where it has no cap.
    pub budget_microusd: Option<MicroUsd>,
    /// The repository the Mandate names, which the worker's `/work` is a
    /// clone of.
    pub repo: Option<RepoName>,
    /// A resume or restart, or a Launch an earlier run may have performed:
    /// check the working directory before repeating outside actions.
    pub recovery: bool,
    /// Every Input the Ticket holds so far, in order. Those not yet handed
    /// to an Assignment also come in a Delivery once this one starts.
    pub inputs: Vec<Delivered>,
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

/// Where repository Tickets' code comes from. The adapter holds the token
/// and its mirrors; callers get domain types back, never a forge's wire
/// objects.
pub trait Forge: Send + Sync {
    /// Make `dirs.work` a clone of `repo` at its base's tip, with its
    /// history and the agent's commit identity, then write
    /// `dirs.host/repo.json`. It runs before any sandbox has touched `work`.
    /// Once `repo.json` exists it returns what that says and touches
    /// nothing, so a recovery keeps the tree as the worker left it; until
    /// then no sandbox ran, so a half-made `work` is made again.
    fn prepare<'a>(
        &'a self,
        repo: &'a RepoName,
        dirs: TicketDirs<'a>,
    ) -> BoxFuture<'a, Result<Workspace>>;
}

/// A repository Ticket's directories on the host.
#[derive(Clone, Copy, Debug)]
pub struct TicketDirs<'a> {
    /// `<ticket>/work`, the clone, writable by sandboxes once prepared.
    pub work: &'a Path,
    /// `<ticket>/host`, written by the host only.
    pub host: &'a Path,
}

/// What `/host/repo.json` holds, and what a worker's prompt says of its
/// repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub repo: RepoName,
    pub base: String,
    /// The base's tip in the host's mirror when the clone was made.
    pub base_commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
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
    /// The worker is gone without a final Report; the Ticket stays open.
    Ended {
        ticket: TicketId,
        assignment: AssignmentId,
        reason: String,
    },
    /// A worker gives back a Trigger that reached its Ticket but is not its
    /// to handle, so the Router judges it.
    Handback {
        ticket: TicketId,
        trigger: TriggerId,
        reason: String,
    },
    /// A fact from the Ticket's own work, such as a background job's end,
    /// that its worker reads as new input. Grants nothing.
    Note {
        ticket: TicketId,
        text: String,
    },
    Command(Command),
    /// A worker's browser is about to act. Asked, so the answer carries the
    /// step's number, and the browser acts only after the commit.
    TrailAsk {
        ticket: TicketId,
        assignment: AssignmentId,
        action: TrailAction,
    },
    TrailDone {
        ticket: TicketId,
        assignment: AssignmentId,
        n: u32,
        outcome: TrailOutcome,
    },
    /// A worker's tool needs a person to allow a vault item for the Ticket.
    /// Asked, so the answer says whether the item waits, is allowed or was
    /// denied; asking again changes nothing and posts nothing.
    SecretAsk {
        ticket: TicketId,
        secret: SecretId,
        /// From the item's metadata, never model or page text.
        label: String,
        /// `source:id`s that may answer; empty: the Policy's approvers.
        answerable_by: Vec<String>,
    },
    /// A keyed request is about to leave. Asked, so it leaves only after the
    /// commit.
    KeyUsed {
        ticket: TicketId,
        assignment: AssignmentId,
        secret: SecretId,
        method: String,
        url: String,
    },
    /// The Watchdog retries a stalled Assignment, unless it has moved on
    /// since it had `reports` Reports.
    Retry {
        ticket: TicketId,
        assignment: AssignmentId,
        reports: usize,
    },
}

/// A person's command through a Source's own controls, such as a Slack
/// button, never through message text. The Source vouches for who gave it;
/// the Policy decides whether they may.
#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    /// Stable for one click, so a redelivered click names the same reply.
    pub id: String,
    pub order: Order,
    pub ticket: TicketId,
    /// The Source it came through, such as `slack`.
    pub source: String,
    pub by: Actor,
    /// Where the outcome is told.
    pub reply_to: ChannelRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Order {
    /// Approve the Ticket's budget, so its work launches.
    Approve,
    /// Stop the Ticket for good.
    Stop,
    /// Allow or deny one vault item for the Ticket.
    Secret { secret: SecretId, allow: bool },
}

/// What the Engine did with an asked fact.
#[derive(Clone, Debug, PartialEq)]
pub enum Applied {
    /// The Decision a Verdict recorded.
    Decision(Decision),
    /// A handback: whether its Trigger now waits for an Eval. It does not
    /// when it was handed back before or already went to its own Eval.
    HandedBack {
        queued: bool,
    },
    TrailAsked {
        n: u32,
    },
    /// The grant for a vault item after an ask: waiting, or already answered.
    Secret(SecretGrant),
    Done,
}

/// The Engine's answer to an asked fact, or why it refused the fact.
pub type Answer = std::result::Result<Applied, String>;

/// Facts and Effect outcomes share one queue, so the facts an adapter sends
/// before its call returns are applied before the Effect is marked done.
pub(crate) enum Inbound {
    Fact(Fact),
    /// A fact whose sender waits for the Engine's answer.
    Ask(Fact, oneshot::Sender<Answer>),
    Outcome {
        id: EffectId,
        attempts: u32,
        result: Result<(), Failure>,
    },
}

pub(crate) struct Failure {
    pub(crate) why: String,
    pub(crate) retry_after: Option<Duration>,
}

impl From<anyhow::Error> for Failure {
    fn from(err: anyhow::Error) -> Self {
        let retry_after = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<RetryAfter>())
            .map(|after| after.0);
        Self {
            why: format!("{err:#}"),
            retry_after,
        }
    }
}

/// Where adapters send facts. It outlives the call it came with.
#[derive(Clone)]
pub struct FactSink(pub(crate) mpsc::Sender<Inbound>);

impl FactSink {
    pub(crate) fn channel() -> (Self, mpsc::Receiver<Inbound>) {
        let (sender, receiver) = mpsc::channel(256);
        (Self(sender), receiver)
    }

    pub async fn send(&self, fact: Fact) -> Result<()> {
        self.0
            .send(Inbound::Fact(fact))
            .await
            .map_err(|_| anyhow!("the agent runtime has stopped"))
    }

    /// Send a fact and wait until the runtime has applied it.
    pub async fn ask(&self, fact: Fact) -> Result<Answer> {
        let (reply, answer) = oneshot::channel();
        self.0
            .send(Inbound::Ask(fact, reply))
            .await
            .map_err(|_| anyhow!("the agent runtime has stopped"))?;
        answer
            .await
            .map_err(|_| anyhow!("the agent runtime stopped before it answered"))
    }
}
