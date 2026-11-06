//! A pull request is not something a worker does: it is something a Ticket
//! owes and the host delivers. The worker's whole vote is `ready`, which the
//! host may refuse (`ready_gate`); after it, the Engine commits one record of
//! what the host admitted and the Effect that makes the forge match it, and
//! the host reports what it read back (`Observed`).
//!
//! Nothing here is a claim a model made: a branch the host named, a head the
//! host admitted, an observation the host made.

use crate::change::{
    BranchName, Change, ChangeState, CommitSha, Deliverable, ForgeTerms, Harvest, PrRef, Withheld,
};
use crate::model::{ChannelRef, Plan, ThreadKey, Ticket, TicketId};
use serde::{Deserialize, Serialize};

/// What a decision owes the forge. Committed in the same transaction as the
/// decision that owes it, so the commit is the write-ahead record: the
/// runtime's retry (with `repeat`) is the crash protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum ForgeJob {
    /// Make the forge hold `head` on the Ticket's branch, with a ready pull
    /// request for it.
    Publish { ticket: TicketId, head: CommitSha },
}

impl ForgeJob {
    pub fn ticket(&self) -> &TicketId {
        match self {
            Self::Publish { ticket, .. } => ticket,
        }
    }
}

/// What the host observed of the forge, applied by `Engine::forge`. Every
/// variant is idempotent: applied twice, the second changes nothing and
/// appends no event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observed {
    /// A Publish converged: the forge holds `head` on the Ticket's branch
    /// and a ready pull request for it.
    Published {
        ticket: TicketId,
        head: CommitSha,
        pr: PrRef,
        url: String,
        /// The pull request's conversation, named by the adapter (the core
        /// does not know how a Source names Threads).
        thread: ThreadKey,
        reply_to: ChannelRef,
        /// Whether this call made the pull request. Recorded as it was
        /// observed; the Result's "opened" or "updated" comes from the
        /// Store, which is right after a crash.
        created: bool,
    },
    /// The forge said no, and said it for good.
    Withheld {
        ticket: TicketId,
        head: CommitSha,
        why: Withheld,
    },
}

/// What `ready` meant, once the gate has looked at the Ticket and what the
/// host harvested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// The Ticket owes only an answer: `ready` is as it always was.
    Answer,
    /// `Deliverable::Change`: the head is admitted (so `ready` needs at least
    /// one commit), the Result says so in a host line, and the Ticket ends.
    /// No Effect.
    Committed(Harvest),
    /// A new head to deliver.
    Propose(Harvest),
    /// The worker answered (a reviewer's question) and changed nothing: the
    /// text is the reply, the head is as delivered.
    Reply,
}

/// Why `ready` is refused. Nothing is committed; the sentence reaches the
/// worker as the tool's error, so it corrects course in the same turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotReady {
    /// The host found no commits to propose.
    NothingToDeliver,
    /// The previous proposal is still being delivered.
    DeliveryInFlight {
        head: CommitSha,
    },
    Held {
        why: Withheld,
    },
    PushLimit {
        max: u32,
    },
    /// The pull request is merged or closed; new work is a new Ticket.
    Finished,
}

impl NotReady {
    /// The sentence the worker reads. Each says what to do next.
    pub fn tell(&self) -> String {
        match self {
            Self::NothingToDeliver => {
                "Not accepted: this ticket owes commits and there are none in /work since the \
                 clone started. Commit your change, then report ready again, or report dropped \
                 or blocked if there is nothing to change."
                    .into()
            }
            Self::DeliveryInFlight { head } => format!(
                "Not accepted: the host is still publishing {}. Report waiting; you will be \
                 told when it lands.",
                head.short()
            ),
            Self::Held { why } => format!(
                "Not accepted: the host could not publish this ticket's change ({}). Report \
                 blocked; a person decides.",
                match why {
                    Withheld::BranchMoved { .. } => "someone else pushed to its branch",
                    Withheld::PrClosed => "its pull request was closed",
                    Withheld::Forbidden { .. } => "GitHub refused it",
                    Withheld::ReadOnly => "this agent is read-only",
                }
            ),
            Self::PushLimit { max } => format!(
                "Not accepted: this ticket has reached its limit of {max} published heads. \
                 Report blocked; a person must raise it."
            ),
            Self::Finished => "Not accepted: this ticket's pull request is merged or closed, so \
                               new work is a new ticket. Report dropped."
                .into(),
        }
    }
}

/// What the Ticket's Plan owes.
pub fn owes(ticket: &Ticket) -> Deliverable {
    deliverable(&ticket.plan)
}

/// What `plan` owes: an answer unless its method says more.
pub fn deliverable(plan: &Plan) -> Deliverable {
    match plan {
        Plan::Session {
            method: Some(method),
            ..
        } => method.deliverable,
        Plan::Session { method: None, .. } | Plan::Swarm { .. } => Deliverable::Answer,
    }
}

/// The `ready` gate: pure, and the first failure refuses. `terms` come from
/// the Ticket's Mandate; `harvest` is what the host measured and admitted of
/// the worker's tree, `None` where the host found nothing to propose.
///
/// A Ticket whose Mandate carries no forge terms (one accepted before
/// pull requests were delivered) owes an answer whatever its Plan says: its
/// `ready` ends it as it always did.
pub fn ready_gate(
    ticket: &Ticket,
    terms: Option<&ForgeTerms>,
    harvest: Option<&Harvest>,
) -> Result<Admission, NotReady> {
    let deliverable = owes(ticket);
    let Some(terms) = terms else {
        return Ok(Admission::Answer);
    };
    match deliverable {
        Deliverable::Answer => Ok(Admission::Answer),
        Deliverable::Change => harvest
            .cloned()
            .map(Admission::Committed)
            .ok_or(NotReady::NothingToDeliver),
        Deliverable::PullRequest => {
            let change = ticket.change.as_ref();
            if let Some(change) = change {
                if let Some(why) = &change.held {
                    return Err(NotReady::Held { why: why.clone() });
                }
                if change.state != ChangeState::Open {
                    return Err(NotReady::Finished);
                }
                if change.delivery_owed() {
                    return Err(NotReady::DeliveryInFlight {
                        head: change.head.clone(),
                    });
                }
            }
            let harvest = harvest.ok_or(NotReady::NothingToDeliver)?;
            if change.is_some_and(|change| change.head == harvest.head) {
                return Ok(Admission::Reply);
            }
            let pushes = change.map_or(0, |change| change.pushes);
            if pushes >= terms.max_pushes {
                return Err(NotReady::PushLimit {
                    max: terms.max_pushes,
                });
            }
            Ok(Admission::Propose(harvest.clone()))
        }
    }
}

/// The branch the Ticket's change is, or will be, pushed to.
pub fn branch_of(ticket: &Ticket) -> BranchName {
    match &ticket.change {
        Some(Change { branch, .. }) => branch.clone(),
        None => BranchName::for_ticket(&ticket.id, &ticket.title),
    }
}
