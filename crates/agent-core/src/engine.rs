//! Every command runs in one transaction: it reads through an overlay of its
//! own uncommitted changes, and commits state, Effects and events together
//! through [`Repositories::commit`]. A command that returns an error commits
//! nothing.

use crate::change::{BranchName, Change, Checks, Deliverable, Delivered};
use crate::delivery::{Admission, ForgeJob, NotReady, Observed, ready_gate};
use crate::ids::key;
use crate::memo::{
    self, Memo, MemoActor, MemoAudience, MemoAuthor, MemoIn, MemoIndex, MemoReview, MemoScope,
    MemoSource, MemoState, MemoTrust,
};
use crate::model::{
    Accept, Acknowledge, Actor, Approval, Ask, Assignment, AssignmentId, AssignmentState,
    Candidate, ChangeSet, Decision, DecisionId, DecisionKind, DomainEvent, Effect, Ended, EndedBy,
    Eval, EvalId, Event, Input, InputBody, InputId, Mandate, MemoId, MessageId, MessageKind,
    MicroUsd, Ms, OutboundMessage, Plan, Report, ReportIn, ReportStatus, SecretGrant, SecretId,
    Strength, Thread, ThreadKey, Ticket, TicketId, TicketOutcome, TicketSummary, Trigger,
    TriggerId, Verdict,
};
use crate::ports::{Clock, Error, Gate, Policy, Repositories};
use crate::trail::{self, Digest, TrailAction, TrailCursor, TrailOutcome};
use std::collections::{BTreeMap, BTreeSet};

/// The third consecutive expired Eval on a Thread records `unavailable`.
pub const MAX_EXPIRED_EVALS: u32 = 3;
pub const MAX_SWARM_AGENTS: u16 = 30;
const MAX_METHOD_CHARS: usize = 16;

/// How a person is named to other people: their display name, else their id.
fn who(by: &Actor) -> &str {
    by.name.as_deref().unwrap_or(&by.id)
}

/// What `receive` did with a Trigger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intake {
    Stored {
        /// Tickets it reached as context.
        reached: Vec<TicketId>,
        /// Whether it waits for an Eval.
        queued: bool,
    },
    Duplicate,
}

pub struct Engine<R, P, C> {
    repos: R,
    policy: P,
    clock: C,
    pass_through: bool,
}

impl<R: Repositories, P: Policy, C: Clock> Engine<R, P, C> {
    pub fn new(repos: R, policy: P, clock: C) -> Self {
        Self {
            repos,
            policy,
            clock,
            pass_through: true,
        }
    }
    /// With pass-through off, an allowed person's follow-up on a subscribed
    /// Thread still reaches its Tickets and is also queued for the Router,
    /// as every other Trigger is. On by default; off is the benchmark's
    /// ablation.
    pub fn set_pass_through(&mut self, on: bool) {
        self.pass_through = on;
    }
    pub fn repos(&self) -> &R {
        &self.repos
    }
    pub fn repos_mut(&mut self) -> &mut R {
        &mut self.repos
    }
    pub fn policy(&self) -> &P {
        &self.policy
    }
    /// Swap the rules. Evals started under the old version end `unavailable`
    /// when decided, and their Triggers are judged again.
    pub fn set_policy(&mut self, policy: P) {
        self.policy = policy;
    }

    fn run<T>(
        &mut self,
        work: impl FnOnce(&mut Tx<'_, R>, &P) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut tx = Tx {
            repos: &self.repos,
            changes: ChangeSet::default(),
            now: self.clock.now_ms(),
            pass_through: self.pass_through,
        };
        let value = work(&mut tx, &self.policy)?;
        let Tx { changes, .. } = tx;
        self.repos.commit(changes)?;
        Ok(value)
    }

    /// Store a Trigger once, fan it out to the Tickets subscribed to its
    /// Threads, and queue it for an Eval on its primary Thread unless it is a
    /// person's message that reached a Ticket, which stays that Ticket's
    /// while pass-through is on.
    pub fn receive(&mut self, trigger: Trigger) -> Result<Intake, Error> {
        let mut threads: Vec<ThreadKey> = Vec::new();
        for key in &trigger.threads {
            if !threads.contains(key) {
                threads.push(key.clone());
            }
        }
        let Some(primary) = threads.first().cloned() else {
            return Err(Error::Invalid(format!(
                "trigger {} names no thread",
                trigger.id
            )));
        };
        self.run(move |tx, policy| {
            if tx.trigger(&trigger.id).is_some() {
                return Ok(Intake::Duplicate);
            }
            let owner = allowed_person(policy, &trigger);
            let mut reached: Vec<TicketId> = Vec::new();
            for key in &threads {
                let mut thread = tx.thread(key).unwrap_or_else(|| Thread::new(key.clone()));
                thread.triggers.push(trigger.id.clone());
                for id in &thread.subscriptions {
                    if reached.contains(id) {
                        continue;
                    }
                    let Some(mut ticket) = tx.ticket(id) else {
                        continue;
                    };
                    if ticket.stopped_by_person() {
                        continue;
                    }
                    let wake = if ticket.ended.is_some() {
                        if !may_resume(policy, owner, &ticket) {
                            continue;
                        }
                        ticket.ended = None;
                        tx.event(DomainEvent::TicketResumed {
                            ticket: ticket.id.clone(),
                            trigger: trigger.id.clone(),
                        });
                        true
                    } else {
                        // A Ticket whose pull request is being made waits for it:
                        // its worker has ended, and a second one would race the
                        // delivery. The input is kept, and `forge` wakes the Ticket
                        // for it once the pull request exists.
                        owner && !ticket.has_live_assignment() && !ticket.delivery_owed()
                    };
                    if wake {
                        tx.start(&mut ticket, policy);
                    }
                    tx.add_trigger_input(&mut ticket, &trigger.id, owner);
                    reached.push(id.clone());
                    tx.save_ticket(ticket);
                }
                tx.save_thread(thread);
            }
            let queued = !owner || reached.is_empty() || !tx.pass_through;
            if queued && let Some(mut thread) = tx.thread(&primary) {
                thread.queued.push(trigger.id.clone());
                tx.save_thread(thread);
            }
            tx.event(DomainEvent::TriggerReceived {
                trigger: trigger.id.clone(),
                threads: threads.clone(),
                reached: reached.clone(),
                queued,
            });
            tx.changes.triggers.push(trigger);
            Ok(Intake::Stored { reached, queued })
        })
    }

    /// Unless paused: expire overdue Evals (requeue twice, then
    /// `unavailable`), and start one Eval per Thread with queued Triggers.
    /// An Eval batches the queued Triggers of the strongest admitted strength;
    /// weaker ones wait for the next Eval, and rejected ones are recorded.
    /// The whole pass is one transaction.
    pub fn evaluate_due(&mut self) -> Result<Vec<EvalId>, Error> {
        if self.policy.paused() {
            return Ok(Vec::new());
        }
        let keys = self.repos.active_threads();
        self.run(|tx, policy| {
            let mut started = Vec::new();
            for key in &keys {
                if let Some(eval) = tx.evaluate(key, policy)? {
                    started.push(eval);
                }
            }
            Ok(started)
        })
    }

    /// Record the Router's Verdict on a running Eval.
    pub fn decide(&mut self, eval: &EvalId, verdict: Verdict) -> Result<DecisionId, Error> {
        let key = self
            .repos
            .thread_of_eval(eval)
            .ok_or_else(|| Error::Missing {
                kind: "running eval",
                id: eval.to_string(),
            })?;
        self.run(|tx, policy| tx.decide(&key, eval, verdict, policy))
    }

    /// Give a Ticket's worker a fact from other work. Grants nothing.
    pub fn note(&mut self, ticket: &TicketId, text: String) -> Result<(), Error> {
        self.run(|tx, _| {
            let mut t = tx.open_ticket(ticket)?;
            tx.add_input(&mut t, InputBody::Note { text }, false);
            tx.save_ticket(t);
            Ok(())
        })
    }

    /// A person approves a Ticket's budget; its first Assignment launches.
    pub fn approve(&mut self, ticket: &TicketId, source: &str, by: Actor) -> Result<(), Error> {
        self.run(|tx, policy| {
            if !by.is_person() || !policy.may_approve(source, &by) {
                return Err(Error::Forbidden(format!(
                    "{} is not allowed to approve budgets.",
                    who(&by)
                )));
            }
            let mut t = tx.open_ticket(ticket)?;
            if t.approval != Approval::Waiting {
                return Err(Error::Refused(
                    "the ticket does not wait for approval".into(),
                ));
            }
            t.approval = Approval::Granted {
                by: by.clone(),
                at_ms: tx.now,
            };
            tx.event(DomainEvent::TicketApproved {
                ticket: t.id.clone(),
                by: by.clone(),
            });
            tx.add_input(&mut t, InputBody::Approved { by }, true);
            tx.launch(&mut t);
            tx.save_ticket(t);
            Ok(())
        })
    }

    /// A person's stop, from `source`: withdraws the Mandate, so nothing
    /// resumes it. The Policy decides who may stop.
    pub fn stop_ticket(
        &mut self,
        ticket: &TicketId,
        source: &str,
        by: Actor,
        reason: &str,
    ) -> Result<(), Error> {
        self.run(|tx, policy| {
            if !policy.may_stop(source, &by) {
                return Err(Error::Forbidden(format!(
                    "{} is not allowed to stop this work.",
                    who(&by)
                )));
            }
            let mut t = tx.require_ticket(ticket)?;
            if t.stopped_by_person() {
                return Err(Error::Refused("the ticket was already stopped".into()));
            }
            tx.end_ticket(&mut t, EndedBy::Person, TicketOutcome::Stopped, reason);
            tx.save_ticket(t);
            Ok(())
        })
    }

    /// End a stalled Assignment and start a new one for the same Ticket.
    /// A retry is a person's command, so it also ends a hold on automatic
    /// Inputs. Returns `None` when the budget now needs approval first.
    pub fn retry(&mut self, ticket: &TicketId) -> Result<Option<AssignmentId>, Error> {
        self.run(|tx, policy| {
            let mut t = tx.open_ticket(ticket)?;
            if t.approval == Approval::Waiting {
                return Err(Error::Refused("the ticket waits for approval".into()));
            }
            tx.end_assignment(&mut t, "retried");
            let id = tx.start(&mut t, policy);
            tx.save_ticket(t);
            Ok(id)
        })
    }

    /// The host restarted while `assignment` was pending or running, so its
    /// worker is gone: end it with `reason` and start the next one, with
    /// `recovery` set, in one transaction, so a crash between the two cannot
    /// leave the work stopped. Returns `None` when the budget now needs
    /// approval first.
    pub fn restart(
        &mut self,
        ticket: &TicketId,
        assignment: &AssignmentId,
        reason: &str,
    ) -> Result<Option<AssignmentId>, Error> {
        self.run(|tx, policy| {
            let mut t = tx.open_ticket(ticket)?;
            if latest_mut(&mut t, assignment)?.state == AssignmentState::Ended {
                return Err(Error::Refused("the assignment has ended".into()));
            }
            tx.end_assignment(&mut t, reason);
            let id = tx.start(&mut t, policy);
            tx.save_ticket(t);
            Ok(id)
        })
    }

    /// An Assignment runs; its waiting Inputs go out.
    pub fn assignment_started(
        &mut self,
        ticket: &TicketId,
        assignment: &AssignmentId,
    ) -> Result<(), Error> {
        self.run(|tx, _| {
            let mut t = tx.open_ticket(ticket)?;
            let latest = latest_mut(&mut t, assignment)?;
            if latest.state != AssignmentState::Pending {
                return Err(Error::Refused("the assignment is not pending".into()));
            }
            latest.state = AssignmentState::Running;
            tx.event(DomainEvent::AssignmentStarted {
                ticket: t.id.clone(),
                assignment: assignment.clone(),
            });
            tx.deliver_waiting(&mut t);
            tx.save_ticket(t);
            Ok(())
        })
    }

    /// The worker hands back a Trigger that reached it as a follow-up but is
    /// not its to handle: a request beyond its scope, other work, or a stop.
    /// It goes to an Eval on its primary Thread, once; the Ticket keeps its
    /// Subscription. Returns whether it was queued.
    pub fn handback(
        &mut self,
        ticket: &TicketId,
        trigger: &TriggerId,
        reason: &str,
    ) -> Result<bool, Error> {
        self.run(|tx, _| {
            let mut t = tx.require_ticket(ticket)?;
            let Some(input) = t.inputs.iter().find(
                |input| matches!(&input.body, InputBody::Trigger { trigger: id } if id == trigger),
            ) else {
                return Err(Error::Refused(format!("{trigger} did not reach {ticket}")));
            };
            // A bot's or an unlisted person's message already went to its own Eval.
            let owned = input.from_allowed_person;
            if !owned || !t.handed_back.insert(trigger.clone()) {
                return Ok(false);
            }
            let original = tx.trigger(trigger).ok_or_else(|| Error::Missing {
                kind: "trigger",
                id: trigger.to_string(),
            })?;
            let primary = original
                .threads
                .first()
                .cloned()
                .ok_or_else(|| Error::Invalid(format!("trigger {trigger} names no thread")))?;
            let mut thread = tx.thread(&primary).unwrap_or_else(|| Thread::new(primary));
            let judging = thread
                .eval
                .as_ref()
                .is_some_and(|e| e.triggers.contains(trigger));
            let queued = thread.handed_back.insert(trigger.clone())
                && !judging
                && !thread.queued.contains(trigger);
            if queued {
                thread.queued.push(trigger.clone());
            }
            tx.save_thread(thread);
            tx.event(DomainEvent::HandedBack {
                ticket: t.id.clone(),
                trigger: trigger.clone(),
                reason: reason.to_owned(),
            });
            tx.save_ticket(t);
            Ok(queued)
        })
    }

    /// An Assignment stopped without a final Report (process exit, host
    /// restart). The Ticket stays open; a person, the Watchdog or a resume
    /// starts the next one.
    pub fn assignment_ended(
        &mut self,
        ticket: &TicketId,
        assignment: &AssignmentId,
        reason: &str,
    ) -> Result<(), Error> {
        self.run(|tx, _| {
            let mut t = tx.require_ticket(ticket)?;
            latest_mut(&mut t, assignment)?;
            tx.end_assignment(&mut t, reason);
            tx.save_ticket(t);
            Ok(())
        })
    }

    /// Record a worker's Report. `ready` sends the result and ends the
    /// Ticket, unless the Ticket owes a change or a pull request: then the
    /// host must have admitted the worker's commits (`report.harvest`), and
    /// the Ticket ends only once they are delivered (see [`Engine::forge`]).
    /// A `ready` the host refuses (`ready_gate`) commits nothing and comes
    /// back as `Error::Refused` with the sentence the worker reads.
    /// `dropped` ends the Ticket silently; a question, `blocked` or `failed`
    /// asks a person; `progress` and `waiting` stay quiet.
    pub fn report(
        &mut self,
        ticket: &TicketId,
        assignment: &AssignmentId,
        report: ReportIn,
    ) -> Result<(), Error> {
        self.run(|tx, _| {
            let mut t = tx.open_ticket(ticket)?;
            let admission = if report.status == ReportStatus::Ready {
                ready_gate(&t, t.mandate.forge.as_ref(), report.harvest.as_ref())
                    .map_err(|why: NotReady| Error::Refused(why.tell()))?
            } else {
                Admission::Answer
            };
            t.seq += 1;
            let seq = t.seq;
            let latest = latest_mut(&mut t, assignment)?;
            if latest.state != AssignmentState::Running {
                return Err(Error::Refused("the assignment is not running".into()));
            }
            latest.reports.push(Report {
                seq,
                status: report.status,
                text: report.text.clone(),
                question: report.question.clone(),
                spent_microusd: report.spent_microusd,
                at_ms: tx.now,
            });
            tx.event(DomainEvent::Reported {
                ticket: t.id.clone(),
                assignment: assignment.clone(),
                status: report.status,
            });
            let ends = matches!(report.status, ReportStatus::Ready | ReportStatus::Dropped);
            if !ends && let Some(question) = &report.question {
                t.attention = Some(question.clone());
                tx.send(&mut t, MessageKind::Attention, question.clone(), None);
            }
            match report.status {
                ReportStatus::Ready => match admission {
                    Admission::Answer => {
                        tx.send(&mut t, MessageKind::Result, report.text, None);
                        tx.end_ticket(&mut t, EndedBy::Executor, TicketOutcome::Completed, "ready");
                    }
                    Admission::Committed(harvest) => {
                        let text = format!(
                            "{}\n\nCommitted {} ({} {}) in the ticket's clone; not published.",
                            report.text,
                            harvest.head.short(),
                            harvest.files,
                            if harvest.files == 1 { "file" } else { "files" }
                        );
                        tx.send(&mut t, MessageKind::Result, text, None);
                        tx.end_ticket(&mut t, EndedBy::Executor, TicketOutcome::Completed, "ready");
                    }
                    Admission::Propose(harvest) => {
                        let round = match t.change.as_mut() {
                            Some(change) => {
                                change.admit(&harvest, report.text);
                                change.pushes
                            }
                            None => {
                                let branch = BranchName::for_ticket(&t.id, &t.title);
                                t.change = Some(Change::first(branch, &harvest, report.text));
                                1
                            }
                        };
                        tx.event(DomainEvent::ProposalAdmitted {
                            ticket: t.id.clone(),
                            head: harvest.head.clone(),
                            commits: harvest.commits,
                            files: harvest.files,
                            bytes: harvest.bytes,
                            round,
                        });
                        tx.effect(Effect::Forge(ForgeJob::Publish {
                            ticket: t.id.clone(),
                            head: harvest.head,
                        }));
                        // The worker is done, the Ticket is not: it ends once
                        // the pull request exists.
                        tx.end_assignment(&mut t, "proposed");
                    }
                    Admission::Reply => {
                        let line = t
                            .change
                            .as_ref()
                            .and_then(|change| change.delivered.as_ref())
                            .map(|delivered| {
                                format!(
                                    "PR {} · head {} · unchanged",
                                    delivered.url,
                                    delivered.head.short()
                                )
                            });
                        let text = match line {
                            Some(line) => format!("{}\n\n{line}", report.text),
                            None => report.text,
                        };
                        tx.send(&mut t, MessageKind::Result, text, None);
                        tx.end_ticket(&mut t, EndedBy::Executor, TicketOutcome::Completed, "ready");
                    }
                },
                ReportStatus::Dropped => {
                    tx.end_ticket(
                        &mut t,
                        EndedBy::Executor,
                        TicketOutcome::Dropped,
                        &report.text,
                    );
                }
                ReportStatus::Blocked | ReportStatus::Failed if report.question.is_none() => {
                    tx.send(&mut t, MessageKind::Attention, report.text, None);
                }
                _ => {}
            }
            tx.save_ticket(t);
            Ok(())
        })
    }

    /// Record that `assignment` is about to act, before it acts, and return
    /// the step's number. Only the Ticket's running Assignment may act, but a
    /// session that outlives its Assignment is still closed: `Close` is
    /// admitted whatever the Assignment's or the Ticket's state, once a step
    /// has been asked.
    pub fn trail_ask(
        &mut self,
        ticket: &TicketId,
        assignment: &AssignmentId,
        action: TrailAction,
    ) -> Result<u32, Error> {
        self.run(|tx, _| {
            action.check().map_err(Error::Invalid)?;
            let t = tx.require_ticket(ticket)?;
            let mut cursor = tx
                .trail(assignment)
                .unwrap_or_else(|| TrailCursor::new(ticket.clone(), assignment.clone()));
            if cursor.ticket != *ticket {
                return Err(Error::Invalid(format!(
                    "{assignment} belongs to {}, not {ticket}",
                    cursor.ticket
                )));
            }
            match &action {
                TrailAction::Close { .. } if cursor.next == 1 => {
                    return Err(Error::Refused(format!("{assignment} has nothing to close")));
                }
                TrailAction::Close { .. } => {}
                _ if t.running_assignment().is_none_or(|a| a.id != *assignment) => {
                    return Err(Error::Refused(format!("{assignment} is not running")));
                }
                _ => {}
            }
            let n = cursor.ask(action.clone(), tx.now).map_err(Error::Refused)?;
            tx.event(DomainEvent::TrailAsked {
                ticket: ticket.clone(),
                assignment: assignment.clone(),
                n,
                action,
            });
            tx.save_trail(cursor);
            Ok(n)
        })
    }

    /// Record how step `n` ended. The Assignment's or the Ticket's state now
    /// does not matter: the action happened, so it is recorded. A signed step
    /// that does not answer the action asked is refused and the step stays
    /// pending, so the caller can record it as lost instead.
    pub fn trail_done(
        &mut self,
        ticket: &TicketId,
        assignment: &AssignmentId,
        n: u32,
        outcome: TrailOutcome,
    ) -> Result<(), Error> {
        self.run(|tx, _| {
            let mut cursor = tx.trail(assignment).ok_or_else(|| Error::Missing {
                kind: "trail",
                id: assignment.to_string(),
            })?;
            if cursor.ticket != *ticket {
                return Err(Error::Invalid(format!(
                    "{assignment} belongs to {}, not {ticket}",
                    cursor.ticket
                )));
            }
            let pending = cursor.answer(n).map_err(Error::Refused)?;
            if let TrailOutcome::Signed(step) = &outcome {
                trail::answers(&pending, ticket, assignment, step).map_err(Error::Invalid)?;
            }
            tx.event(DomainEvent::TrailDone {
                ticket: ticket.clone(),
                assignment: assignment.clone(),
                n,
                outcome: outcome.bounded(),
            });
            tx.save_trail(cursor);
            Ok(())
        })
    }

    /// Ask a person to approve the vault item `secret` for the Ticket, once.
    /// An existing grant, in any state, is returned unchanged and commits
    /// nothing, so a worker that retries while it waits cannot fill the
    /// thread. `label` and `answerable_by` come from the item's metadata.
    pub fn ask_secret(
        &mut self,
        ticket: &TicketId,
        secret: &SecretId,
        label: &str,
        answerable_by: &[String],
    ) -> Result<SecretGrant, Error> {
        self.run(|tx, _| {
            let mut t = tx.open_ticket(ticket)?;
            if let Some(grant) = t.secrets.get(secret) {
                return Ok(grant.clone());
            }
            if label.trim().is_empty() {
                return Err(Error::Invalid("a secret's label is empty".into()));
            }
            let grant = SecretGrant::Waiting {
                label: label.to_owned(),
                answerable_by: answerable_by.to_vec(),
                asked_ms: tx.now,
            };
            t.secrets.insert(secret.clone(), grant.clone());
            tx.event(DomainEvent::SecretAsked {
                ticket: t.id.clone(),
                secret: secret.clone(),
            });
            let text = format!("{} wants to {label} for the first time.", t.title);
            let ask = Ask::Secret {
                secret: secret.clone(),
            };
            tx.send(&mut t, MessageKind::Attention, text, Some(ask));
            tx.save_ticket(t);
            Ok(grant)
        })
    }

    /// A person's Allow or Deny, from a Source's own controls and never from
    /// message text. Only the item's own `answerable_by` may answer, or the
    /// Policy's approvers when that list is empty. The answer reaches the
    /// worker as an Input.
    pub fn answer_secret(
        &mut self,
        ticket: &TicketId,
        secret: &SecretId,
        source: &str,
        by: Actor,
        allow: bool,
    ) -> Result<(), Error> {
        self.run(|tx, policy| {
            let mut t = tx.open_ticket(ticket)?;
            let Some(SecretGrant::Waiting { answerable_by, .. }) = t.secrets.get(secret) else {
                return Err(Error::Refused(format!(
                    "{secret} does not wait for an answer"
                )));
            };
            let may = by.is_person()
                && if answerable_by.is_empty() {
                    policy.may_approve(source, &by)
                } else {
                    answerable_by.contains(&format!("{source}:{}", by.id))
                };
            if !may {
                let who = by.name.as_deref().unwrap_or(&by.id);
                return Err(Error::Refused(format!("{who} may not answer for {secret}")));
            }
            let at_ms = tx.now;
            let grant = if allow {
                SecretGrant::Granted {
                    by: by.clone(),
                    at_ms,
                }
            } else {
                SecretGrant::Denied {
                    by: by.clone(),
                    at_ms,
                }
            };
            t.secrets.insert(secret.clone(), grant);
            tx.event(DomainEvent::SecretAnswered {
                ticket: t.id.clone(),
                secret: secret.clone(),
                by: by.clone(),
                granted: allow,
            });
            let body = InputBody::Secret {
                secret: secret.clone(),
                granted: allow,
                by,
            };
            tx.add_input(&mut t, body, true);
            tx.save_ticket(t);
            Ok(())
        })
    }

    /// Record that `assignment` is about to send a request with the vault
    /// key `secret`, before it leaves, so a use never goes unrecorded. No
    /// state changes, and a Denied item is refused.
    pub fn use_key(
        &mut self,
        ticket: &TicketId,
        assignment: &AssignmentId,
        secret: &SecretId,
        method: &str,
        url: &str,
    ) -> Result<(), Error> {
        self.run(|tx, _| {
            let t = tx.open_ticket(ticket)?;
            if t.running_assignment().is_none_or(|a| a.id != *assignment) {
                return Err(Error::Refused(format!("{assignment} is not running")));
            }
            if matches!(t.secrets.get(secret), Some(SecretGrant::Denied { .. })) {
                return Err(Error::Refused(format!(
                    "a person denied {secret} for this ticket"
                )));
            }
            if url.chars().count() > trail::MAX_URL_CHARS {
                return Err(Error::Invalid(format!(
                    "a url is at most {} characters",
                    trail::MAX_URL_CHARS
                )));
            }
            if !(1..=MAX_METHOD_CHARS).contains(&method.len())
                || !method.bytes().all(|b| b.is_ascii_uppercase())
            {
                return Err(Error::Invalid(format!(
                    "a method is 1 to {MAX_METHOD_CHARS} uppercase ASCII letters"
                )));
            }
            tx.event(DomainEvent::KeyUsed {
                ticket: ticket.clone(),
                assignment: assignment.clone(),
                secret: secret.clone(),
                method: method.to_owned(),
                url: url.to_owned(),
            });
            Ok(())
        })
    }

    /// Apply what the host observed of the forge. Every observation is
    /// idempotent: one that repeats, or answers a head a newer proposal
    /// replaced, commits nothing.
    ///
    /// `Published` records what the host read back (the pull request, the
    /// head it holds), subscribes the Ticket to the pull request's
    /// conversation, tells whoever asked (the worker's words, then one host
    /// line, `PR <url> · head <sha> · opened`), moves the Ticket's reply
    /// channel from an origin on the same Source to that conversation, and
    /// ends the Ticket: it is done when the pull request exists. `Withheld`
    /// holds the Change and asks a person once.
    pub fn forge(&mut self, observed: Observed) -> Result<(), Error> {
        self.run(|tx, policy| match observed {
            Observed::Published {
                ticket,
                head,
                pr,
                url,
                thread,
                reply_to,
                created,
            } => {
                let mut t = tx.require_ticket(&ticket)?;
                let Some(change) = t.change.as_mut() else {
                    return Err(Error::Refused(format!("{ticket} has no change to deliver")));
                };
                let answers_the_proposal = change.head == head && change.held.is_none();
                let repeats = change.delivered.as_ref().is_some_and(|d| d.head == head);
                if !answers_the_proposal || repeats {
                    return Ok(());
                }
                let first = change.delivered.is_none();
                change.delivered = Some(Delivered {
                    pr: pr.clone(),
                    head: head.clone(),
                    url: url.clone(),
                    at_ms: tx.now,
                });
                change.checks = Checks::Pending { reported: 0 };
                let text = change.text.clone();
                tx.event(DomainEvent::Delivered {
                    ticket: t.id.clone(),
                    head: head.clone(),
                    pr,
                    created,
                });
                let mut conversation = tx
                    .thread(&thread)
                    .unwrap_or_else(|| Thread::new(thread.clone()));
                conversation.subscriptions.insert(t.id.clone());
                tx.save_thread(conversation);
                t.threads.insert(thread);
                if t.ended.is_some() {
                    // A person stopped the Ticket while the push was in
                    // flight: the pull request exists, so it is recorded, and
                    // nobody is told more.
                    tx.save_ticket(t);
                    return Ok(());
                }
                let verb = if first { "opened" } else { "updated" };
                let line = format!("PR {url} · head {} · {verb}", head.short());
                tx.send(
                    &mut t,
                    MessageKind::Result,
                    format!("{text}\n\n{line}"),
                    None,
                );
                // The first Result reaches the origin (the issue, the chat
                // thread). From then on the pull request's conversation
                // supersedes an origin on the same Source, so a review round
                // answers once on GitHub while a chat origin keeps hearing
                // everything.
                t.channels
                    .retain(|channel| channel.source != reply_to.source);
                t.channels.push(reply_to);
                tx.end_ticket(&mut t, EndedBy::Executor, TicketOutcome::Completed, "ready");
                // A follow-up that came while the pull request was being made
                // was kept, not acted on; the Ticket takes it up now.
                let waiting = t
                    .inputs
                    .iter()
                    .filter(|input| input.sent_to.is_none() && input.from_allowed_person)
                    .find_map(|input| match &input.body {
                        InputBody::Trigger { trigger } => Some(trigger.clone()),
                        _ => None,
                    });
                if let Some(trigger) = waiting
                    && policy.revalidate(&t.mandate)
                {
                    t.ended = None;
                    tx.event(DomainEvent::TicketResumed {
                        ticket: t.id.clone(),
                        trigger,
                    });
                    tx.start(&mut t, policy);
                }
                tx.save_ticket(t);
                Ok(())
            }
            Observed::Withheld { ticket, head, why } => {
                let mut t = tx.require_ticket(&ticket)?;
                let Some(change) = t.change.as_mut() else {
                    return Err(Error::Refused(format!("{ticket} has no change to deliver")));
                };
                let repeats = change.held.is_some() || change.head != head;
                let delivered = change.delivered.as_ref().is_some_and(|d| d.head == head);
                if repeats || delivered {
                    return Ok(());
                }
                change.held = Some(why.clone());
                let told = why.tell(&t.title, &change.branch, &head);
                tx.event(DomainEvent::Withheld {
                    ticket: t.id.clone(),
                    head,
                    why,
                });
                if t.ended.is_none() {
                    t.attention = Some(told.clone());
                    tx.send(&mut t, MessageKind::Attention, told, None);
                }
                tx.save_ticket(t);
                Ok(())
            }
        })
    }

    /// Propose a memo and return its id. Its trust is derived from its
    /// sources, never from its author's word, and a person's own memo (the
    /// operator's, or a person's message quoted word for word) is active at
    /// once. Anything else waits for the review's decision
    /// ([`Engine::activate_memo`]). A live memo with the same scope and text
    /// is refused, naming it.
    pub fn propose_memo(&mut self, proposal: MemoIn) -> Result<MemoId, Error> {
        self.run(|tx, policy| {
            let MemoIn {
                scope,
                text,
                sources,
                author,
                supersedes,
            } = proposal;
            let author_key = match &author {
                MemoAuthor::Operator { source, by } => {
                    may_remember(policy, source, by)?;
                    format!("{source}:{}", by.id)
                }
                MemoAuthor::Worker { ticket, assignment } => {
                    let t = tx.open_ticket(ticket)?;
                    if t.running_assignment().is_none_or(|a| a.id != *assignment) {
                        return Err(Error::Refused(format!("{assignment} is not running")));
                    }
                    assignment.to_string()
                }
            };
            memo::check_text(&text).map_err(Error::Invalid)?;
            let trust = tx.memo_trust(policy, &sources, &text, &author)?;
            if let Some(old) = &supersedes {
                tx.may_supersede(&author, &scope, old)?;
            }
            if let Some(held) = tx
                .live_memos()
                .into_iter()
                .find(|m| m.scope == scope && m.text == text)
            {
                return Err(Error::Refused(format!("{} already holds it", held.id)));
            }
            let id = MemoId(key(
                "mm",
                &[&scope.to_string(), &text, &author_key, &tx.now.to_string()],
            ));
            if tx.memo(&id).is_some() {
                return Err(Error::Refused(format!("{id} was just written")));
            }
            let active = trust == MemoTrust::Person;
            let state = if active {
                MemoState::Active {
                    since_ms: tx.now,
                    review: None,
                }
            } else {
                MemoState::Proposed
            };
            let text_sha256 = Digest::of(text.as_bytes());
            tx.event(DomainEvent::MemoProposed {
                memo: id.clone(),
                scope: scope.clone(),
                trust,
                author: author.clone(),
                text_sha256: text_sha256.clone(),
                active,
                supersedes: supersedes.clone(),
            });
            let memo = Memo {
                id: id.clone(),
                scope,
                text,
                text_sha256,
                sources,
                trust,
                author,
                state,
                created_ms: tx.now,
                verified_ms: tx.now,
                supersedes,
            };
            if active && let MemoAuthor::Worker { ticket, assignment } = &memo.author {
                let by = MemoActor::Dream {
                    ticket: ticket.clone(),
                    assignment: assignment.clone(),
                };
                tx.supersede(&memo, &by, None);
            }
            tx.save_memo(memo);
            Ok(id)
        })
    }

    /// The review keeps a proposed memo: from now on it is in the index.
    /// This is the one way a model's or an external memo becomes active.
    pub fn activate_memo(&mut self, memo: &MemoId, review: MemoReview) -> Result<(), Error> {
        self.run(|tx, _| {
            memo::check_name("a review's name", &review.by).map_err(Error::Invalid)?;
            memo::check_reason(&review.reason).map_err(Error::Invalid)?;
            let mut m = tx.require_memo(memo)?;
            if m.state != MemoState::Proposed {
                return Err(Error::Refused(format!("{memo} is {}", m.state.name())));
            }
            tx.event(DomainEvent::MemoActivated {
                memo: memo.clone(),
                by: review.by.clone(),
                reason_sha256: Digest::of(review.reason.as_bytes()),
            });
            let by = MemoActor::Review {
                by: review.by.clone(),
            };
            m.state = MemoState::Active {
                since_ms: tx.now,
                review: Some(review),
            };
            m.verified_ms = tx.now;
            tx.supersede(&m, &by, None);
            tx.save_memo(m);
            Ok(())
        })
    }

    /// The review keeps what a proposed memo says, in its own words: `text`
    /// instead of the proposal's, and with `merges`, in place of that active
    /// memo of the same scope too. A memo's text never changes, so the words
    /// kept are a new memo, active at once, by the proposal's author, that
    /// cites the proposal's sources (and the merged memo's), its trust
    /// derived from them and `text` anew. The proposal and the merged memo
    /// are retired in the same commit, each naming the new one. A person's
    /// memo is merged into nothing: only a person changes it. Returns the
    /// new memo's id.
    pub fn revise_memo(
        &mut self,
        memo: &MemoId,
        review: MemoReview,
        text: String,
        merges: Option<&MemoId>,
    ) -> Result<MemoId, Error> {
        self.run(|tx, policy| {
            memo::check_name("a review's name", &review.by).map_err(Error::Invalid)?;
            memo::check_reason(&review.reason).map_err(Error::Invalid)?;
            memo::check_text(&text).map_err(Error::Invalid)?;
            let proposal = tx.require_memo(memo)?;
            if proposal.state != MemoState::Proposed {
                return Err(Error::Refused(format!(
                    "{memo} is {}",
                    proposal.state.name()
                )));
            }
            let mut sources = proposal.sources.clone();
            let merged = match merges {
                None if text == proposal.text => {
                    return Err(Error::Invalid(format!(
                        "that is what {memo} says already; keep it instead"
                    )));
                }
                None => None,
                Some(other) => {
                    let m = tx.require_memo(other)?;
                    if m.id == proposal.id || !m.is_active() || m.scope != proposal.scope {
                        return Err(Error::Refused(format!(
                            "{other} is not an active memo of {} beside {memo}",
                            proposal.scope
                        )));
                    }
                    if m.trust == MemoTrust::Person {
                        return Err(Error::Refused(format!(
                            "{other} is a person's word, which only a person changes"
                        )));
                    }
                    for source in &m.sources {
                        if !sources.contains(source) {
                            sources.push(source.clone());
                        }
                    }
                    Some(m)
                }
            };
            let trust = tx.memo_trust(policy, &sources, &text, &proposal.author)?;
            let replaced =
                |id: &MemoId| *id == proposal.id || merged.as_ref().is_some_and(|m| m.id == *id);
            if let Some(held) = tx
                .live_memos()
                .into_iter()
                .find(|m| !replaced(&m.id) && m.scope == proposal.scope && m.text == text)
            {
                return Err(Error::Refused(format!("{} already holds it", held.id)));
            }
            let id = MemoId(key(
                "mm",
                &[
                    &proposal.scope.to_string(),
                    &text,
                    memo.as_str(),
                    &tx.now.to_string(),
                ],
            ));
            if tx.memo(&id).is_some() {
                return Err(Error::Refused(format!("{id} was just written")));
            }
            let by = MemoActor::Review {
                by: review.by.clone(),
            };
            let merged_id = merged.as_ref().map(|m| m.id.clone());
            let why = match merged {
                Some(m) => {
                    let why = format!("merged with {} into {id} by the review", m.id);
                    let other = format!("merged with {} into {id} by the review", proposal.id);
                    tx.retire(m, &by, other, Some(id.clone()));
                    why
                }
                None => format!("kept by the review in other words, as {id}"),
            };
            let (scope, author) = (proposal.scope.clone(), proposal.author.clone());
            let supersedes = proposal.supersedes.clone();
            tx.retire(proposal, &by, why, Some(id.clone()));
            let text_sha256 = Digest::of(text.as_bytes());
            tx.event(DomainEvent::MemoProposed {
                memo: id.clone(),
                scope: scope.clone(),
                trust,
                author: author.clone(),
                text_sha256: text_sha256.clone(),
                active: false,
                supersedes: supersedes.clone(),
            });
            tx.event(DomainEvent::MemoActivated {
                memo: id.clone(),
                by: review.by.clone(),
                reason_sha256: Digest::of(review.reason.as_bytes()),
            });
            let kept = Memo {
                id: id.clone(),
                scope,
                text,
                text_sha256,
                sources,
                trust,
                author,
                state: MemoState::Active {
                    since_ms: tx.now,
                    review: Some(review),
                },
                created_ms: tx.now,
                verified_ms: tx.now,
                supersedes,
            };
            tx.supersede(&kept, &by, merged_id.as_ref());
            tx.save_memo(kept);
            Ok(id)
        })
    }

    /// Retire every proposal no review decided within
    /// [`memo::PROPOSAL_TTL_MS`], under the name [`memo::EXPIRY`], and say
    /// which. Nothing is written when none is due.
    pub fn expire_memos(&mut self) -> Result<Vec<MemoId>, Error> {
        let due = |m: &Memo, now: Ms| {
            m.state == MemoState::Proposed
                && now.saturating_sub(m.created_ms) >= memo::PROPOSAL_TTL_MS
        };
        let now = self.clock.now_ms();
        if !self.repos.live_memos().iter().any(|m| due(m, now)) {
            return Ok(Vec::new());
        }
        self.run(|tx, _| {
            let by = MemoActor::Review {
                by: memo::EXPIRY.to_owned(),
            };
            let reason = format!(
                "no review decided it within {} days",
                memo::PROPOSAL_TTL_MS / 86_400_000
            );
            let mut expired = Vec::new();
            for m in tx.live_memos() {
                if due(&m, tx.now) {
                    expired.push(m.id.clone());
                    tx.retire(m, &by, reason.clone(), None);
                }
            }
            Ok(expired)
        })
    }

    /// Take a proposed or active memo out of use for good, with why. A
    /// person may retire any memo; the review only a model's or an external
    /// one, so a person's word stands until a person takes it back.
    pub fn retire_memo(&mut self, memo: &MemoId, by: MemoActor, reason: &str) -> Result<(), Error> {
        self.retire_memo_citing(memo, by, reason, Citing::Nothing)
    }

    /// [`retire_memo`](Self::retire_memo), with what the retirement rests
    /// on. The dream must cite: lines of transcripts that show the memo is
    /// wrong or stale, or the active memo of the same scope that already
    /// says it. It acts only while its Assignment runs.
    pub fn retire_memo_citing(
        &mut self,
        memo: &MemoId,
        by: MemoActor,
        reason: &str,
        citing: Citing,
    ) -> Result<(), Error> {
        self.run(|tx, policy| {
            memo_actor(policy, &by)?;
            memo::check_reason(reason).map_err(Error::Invalid)?;
            let m = tx.require_memo(memo)?;
            if !m.is_live() {
                return Err(Error::Refused(format!("{memo} is {}", m.state.name())));
            }
            let person = matches!(by, MemoActor::Operator { .. });
            if !person && m.trust == MemoTrust::Person {
                return Err(Error::Refused(format!(
                    "{memo} is a person's word, which only a person retires"
                )));
            }
            if let MemoActor::Dream { ticket, assignment } = &by {
                tx.dreaming(ticket, assignment)?;
                if matches!(citing, Citing::Nothing) {
                    return Err(Error::Invalid(
                        "the dream cites what shows a memo is wrong, stale or a duplicate".into(),
                    ));
                }
            }
            let (replaced_by, evidence) = match citing {
                Citing::Nothing => (None, Vec::new()),
                Citing::Duplicate(other) => {
                    let held = tx.require_memo(&other)?;
                    if held.id == m.id || !held.is_active() || held.scope != m.scope {
                        return Err(Error::Refused(format!(
                            "{other} is not another active memo of {}",
                            m.scope
                        )));
                    }
                    (Some(other), Vec::new())
                }
                Citing::Evidence(evidence) => {
                    if !(1..=memo::MAX_RETIRE_EVIDENCE).contains(&evidence.len()) {
                        return Err(Error::Invalid(format!(
                            "a retirement cites 1 to {} lines",
                            memo::MAX_RETIRE_EVIDENCE
                        )));
                    }
                    for source in &evidence {
                        if !matches!(source, MemoSource::Transcript { .. }) {
                            return Err(Error::Invalid(
                                "a retirement cites lines of transcripts".into(),
                            ));
                        }
                        memo::source_trust(source, "", &m.author, None).map_err(Error::Invalid)?;
                    }
                    (None, evidence)
                }
            };
            tx.retire_citing(m, &by, reason.to_owned(), replaced_by, evidence);
            Ok(())
        })
    }

    /// A person erases a memo: its text, its quotes and the reasons given
    /// for it. Its digest stays in its row and on the chain, so the chain
    /// still verifies and an audit can tell it existed.
    pub fn forget_memo(&mut self, memo: &MemoId, source: &str, by: Actor) -> Result<(), Error> {
        self.run(|tx, policy| {
            may_remember(policy, source, &by)?;
            let mut m = tx.require_memo(memo)?;
            if matches!(m.state, MemoState::Forgotten { .. }) {
                return Err(Error::Refused(format!("{memo} is already forgotten")));
            }
            let by = MemoActor::Operator {
                source: source.to_owned(),
                by,
            };
            tx.event(DomainEvent::MemoForgotten {
                memo: memo.clone(),
                by: by.clone(),
            });
            m.erase();
            m.state = MemoState::Forgotten { by, at_ms: tx.now };
            tx.save_memo(m);
            Ok(())
        })
    }

    /// The index `to` is shown now: the active global memos, and for a worker
    /// its repository's, within the audience's budget. A non-empty one is
    /// recorded as `MemosShown`, so a wrong memo can name the sessions it
    /// shaped.
    pub fn show_memos(&mut self, to: MemoAudience) -> Result<MemoIndex, Error> {
        self.run(|tx, _| {
            let mut scopes = vec![MemoScope::Global];
            if let MemoAudience::Worker { ticket, assignment } = &to {
                let t = tx.require_ticket(ticket)?;
                if !t.assignments.iter().any(|a| a.id == *assignment) {
                    return Err(Error::Refused(format!(
                        "{assignment} is not an assignment of {ticket}"
                    )));
                }
                scopes.extend(t.mandate.repo.map(MemoScope::Repo));
            }
            let index = memo::memo_index(&tx.live_memos(), &scopes, to.budget(), &to);
            if !index.text.is_empty() {
                tx.event(DomainEvent::MemosShown {
                    to,
                    memos: index.shown.clone(),
                    index_sha256: index.sha256.clone(),
                });
            }
            Ok(index)
        })
    }
}

/// Only a person the Policy lets change the agent's memory acts as its
/// operator.
fn may_remember(policy: &impl Policy, source: &str, by: &Actor) -> Result<(), Error> {
    if by.is_person() && policy.may_remember(source, by) {
        Ok(())
    } else {
        Err(Error::Forbidden(format!(
            "{} is not allowed to change the agent's memory.",
            who(by)
        )))
    }
}

fn memo_actor(policy: &impl Policy, by: &MemoActor) -> Result<(), Error> {
    match by {
        MemoActor::Operator { source, by } => may_remember(policy, source, by),
        MemoActor::Review { by } => memo::check_name("a review's name", by).map_err(Error::Invalid),
        // Checked against its Ticket where it acts.
        MemoActor::Dream { .. } => Ok(()),
    }
}

/// What a memo's retirement rests on, beside its reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Citing {
    /// A person's or the review's word.
    Nothing,
    /// Lines of transcripts that show the memo is wrong or stale.
    Evidence(Vec<MemoSource>),
    /// The active memo of the same scope that already says it.
    Duplicate(MemoId),
}

/// Only a person the Policy admits explicitly owns a follow-up: a resume
/// relaunches spending, so the allowlist decides who may cause one.
fn allowed_person(policy: &impl Policy, trigger: &Trigger) -> bool {
    trigger.actor.is_person()
        && policy.gate(trigger, true)
            == Gate::Admit {
                strength: Strength::Explicit,
            }
}

fn needs_approval(ticket: &Ticket, policy: &impl Policy) -> bool {
    !matches!(ticket.approval, Approval::Granted { .. })
        && ticket
            .mandate
            .budget_microusd
            .is_some_and(|budget| budget > policy.auto_approve_microusd())
}

fn may_resume(policy: &impl Policy, owner: bool, ticket: &Ticket) -> bool {
    owner && policy.revalidate(&ticket.mandate)
}

fn validate(accept: &Accept) -> Result<(), Error> {
    if accept.title.trim().is_empty() || accept.scope.trim().is_empty() {
        return Err(Error::Invalid("an accept needs a title and a scope".into()));
    }
    if let Plan::Swarm { agents, .. } = accept.plan
        && !(1..=MAX_SWARM_AGENTS).contains(&agents)
    {
        return Err(Error::Invalid(format!(
            "a swarm has 1 to {MAX_SWARM_AGENTS} agents, not {agents}"
        )));
    }
    match (accept.budget_microusd, &accept.plan) {
        (Some(0), _) => return Err(Error::Invalid("an accept needs a budget".into())),
        (None, Plan::Swarm { .. }) => {
            return Err(Error::Invalid("a swarm needs a budget".into()));
        }
        _ => {}
    }
    match (&accept.plan, &accept.repo) {
        (Plan::Swarm { .. }, Some(_)) => Err(Error::Invalid(
            "a repository is for a session plan only".into(),
        )),
        (
            Plan::Session {
                method: Some(method),
                ..
            },
            None,
        ) if method.deliverable > Deliverable::Answer => Err(Error::Invalid(
            "a change or a pull request needs a repository".into(),
        )),
        (
            Plan::Session {
                method: Some(method),
                ..
            },
            Some(_),
        ) if method.deliverable > Deliverable::Answer && accept.forge.is_none() => {
            Err(Error::Invalid(
                "a change or a pull request needs the forge terms of its repository".into(),
            ))
        }
        (_, None) if accept.forge.is_some() => Err(Error::Invalid(
            "forge terms are for an accept that names a repository".into(),
        )),
        _ => Ok(()),
    }
}

/// Put `triggers` back at the front of the queue, each at most once.
fn requeue(thread: &mut Thread, triggers: Vec<TriggerId>) {
    let mut queued = triggers;
    for id in std::mem::take(&mut thread.queued) {
        if !queued.contains(&id) {
            queued.push(id);
        }
    }
    thread.queued = queued;
}

fn latest_mut<'t>(
    ticket: &'t mut Ticket,
    assignment: &AssignmentId,
) -> Result<&'t mut Assignment, Error> {
    match ticket.assignments.last_mut() {
        Some(latest) if latest.id == *assignment => Ok(latest),
        _ => Err(Error::Refused(format!(
            "{assignment} is not the latest assignment of {}",
            ticket.id
        ))),
    }
}

fn usd(microusd: MicroUsd) -> String {
    format!(
        "${}.{:02}",
        microusd / 1_000_000,
        (microusd % 1_000_000) / 10_000
    )
}

fn plan_label(plan: &Plan) -> String {
    match plan {
        Plan::Session { .. } => "one worker session".into(),
        Plan::Swarm { agents, .. } => format!("a swarm of {agents} agents"),
    }
}

struct Tx<'a, R> {
    repos: &'a R,
    changes: ChangeSet,
    now: Ms,
    pass_through: bool,
}

impl<R: Repositories> Tx<'_, R> {
    fn trigger(&self, id: &TriggerId) -> Option<Trigger> {
        self.changes
            .triggers
            .iter()
            .find(|t| t.id == *id)
            .cloned()
            .or_else(|| self.repos.trigger(id))
    }
    fn thread(&self, key: &ThreadKey) -> Option<Thread> {
        self.changes
            .threads
            .get(key)
            .cloned()
            .or_else(|| self.repos.thread(key))
    }
    fn ticket(&self, id: &TicketId) -> Option<Ticket> {
        self.changes
            .tickets
            .get(id)
            .cloned()
            .or_else(|| self.repos.ticket(id))
    }
    fn require_ticket(&self, id: &TicketId) -> Result<Ticket, Error> {
        self.ticket(id).ok_or_else(|| Error::Missing {
            kind: "ticket",
            id: id.to_string(),
        })
    }
    fn open_ticket(&self, id: &TicketId) -> Result<Ticket, Error> {
        let ticket = self.require_ticket(id)?;
        if ticket.ended.is_some() {
            return Err(Error::Refused(format!("{id} has ended")));
        }
        Ok(ticket)
    }
    fn trail(&self, assignment: &AssignmentId) -> Option<TrailCursor> {
        self.changes
            .trails
            .get(assignment)
            .cloned()
            .or_else(|| self.repos.trail(assignment))
    }
    fn save_trail(&mut self, cursor: TrailCursor) {
        self.changes
            .trails
            .insert(cursor.assignment.clone(), cursor);
    }
    fn memo(&self, id: &MemoId) -> Option<Memo> {
        self.changes
            .memos
            .get(id)
            .cloned()
            .or_else(|| self.repos.memo(id))
    }
    fn require_memo(&self, id: &MemoId) -> Result<Memo, Error> {
        self.memo(id).ok_or_else(|| Error::Missing {
            kind: "memo",
            id: id.to_string(),
        })
    }
    /// `ticket` is the dream, open, and `assignment` its running one.
    fn dreaming(&self, ticket: &TicketId, assignment: &AssignmentId) -> Result<Ticket, Error> {
        let t = self.open_ticket(ticket)?;
        if !t.is_dream() {
            return Err(Error::Refused(format!("{ticket} is not the dream")));
        }
        if t.running_assignment().is_none_or(|a| a.id != *assignment) {
            return Err(Error::Refused(format!("{assignment} is not running")));
        }
        Ok(t)
    }
    /// Only the dream supersedes a memo, and only an active model's or
    /// external memo of the proposal's own scope.
    fn may_supersede(
        &self,
        author: &MemoAuthor,
        scope: &MemoScope,
        old: &MemoId,
    ) -> Result<(), Error> {
        let dream = match author {
            MemoAuthor::Worker { ticket, .. } => self.ticket(ticket).is_some_and(|t| t.is_dream()),
            MemoAuthor::Operator { .. } => false,
        };
        if !dream {
            return Err(Error::Refused("only the dream supersedes a memo".into()));
        }
        let m = self.require_memo(old)?;
        if !m.is_active() || m.scope != *scope {
            return Err(Error::Refused(format!(
                "{old} is not an active memo of {scope}"
            )));
        }
        if m.trust == MemoTrust::Person {
            return Err(Error::Refused(format!(
                "{old} is a person's word, which only a person changes"
            )));
        }
        Ok(())
    }
    /// Proposed and active memos, this transaction's changes included.
    fn live_memos(&self) -> Vec<Memo> {
        let mut all: BTreeMap<MemoId, Memo> = self
            .repos
            .live_memos()
            .into_iter()
            .map(|memo| (memo.id.clone(), memo))
            .collect();
        all.extend(self.changes.memos.clone());
        all.into_values().filter(Memo::is_live).collect()
    }
    fn save_memo(&mut self, memo: Memo) {
        self.changes.memos.insert(memo.id.clone(), memo);
    }
    /// Retire `memo` for good: its reason in its row, the reason's digest on
    /// the chain, and the memo that holds what it said now, if one does.
    fn retire(&mut self, memo: Memo, by: &MemoActor, reason: String, replaced_by: Option<MemoId>) {
        self.retire_citing(memo, by, reason, replaced_by, Vec::new());
    }
    /// [`retire`](Self::retire), with what the retirement cites.
    fn retire_citing(
        &mut self,
        mut memo: Memo,
        by: &MemoActor,
        reason: String,
        replaced_by: Option<MemoId>,
        evidence: Vec<MemoSource>,
    ) {
        self.event(DomainEvent::MemoRetired {
            memo: memo.id.clone(),
            by: by.clone(),
            reason_sha256: Digest::of(reason.as_bytes()),
        });
        memo.state = MemoState::Retired {
            by: by.clone(),
            reason,
            at_ms: self.now,
            replaced_by,
            evidence,
        };
        self.save_memo(memo);
    }
    /// `kept` is active now: retire the memo it supersedes, if that one is
    /// still live, by the same actor and naming `kept`. Nothing else is
    /// retired here.
    fn supersede(&mut self, kept: &Memo, by: &MemoActor, skip: Option<&MemoId>) {
        let Some(old) = &kept.supersedes else {
            return;
        };
        if skip == Some(old) {
            return;
        }
        if let Some(old) = self.memo(old).filter(Memo::is_live) {
            let why = format!("superseded by {}", kept.id);
            self.retire(old, by, why, Some(kept.id.clone()));
        }
    }
    /// What `sources` make a memo that says `text` by `author` worth: the
    /// least of what each gives, each checked as it is cited.
    fn memo_trust(
        &self,
        policy: &impl Policy,
        sources: &[MemoSource],
        text: &str,
        author: &MemoAuthor,
    ) -> Result<MemoTrust, Error> {
        if !(1..=memo::MAX_MEMO_SOURCES).contains(&sources.len()) {
            return Err(Error::Invalid(format!(
                "a memo cites 1 to {} sources",
                memo::MAX_MEMO_SOURCES
            )));
        }
        let mut trust = MemoTrust::Person;
        for source in sources {
            let trigger = match source {
                MemoSource::Trigger { trigger, .. } => self.trigger(trigger),
                MemoSource::Ticket { ticket } => {
                    self.require_ticket(ticket)?;
                    None
                }
                MemoSource::Operator | MemoSource::Call { .. } | MemoSource::Transcript { .. } => {
                    None
                }
            };
            let allowed = trigger.as_ref().map(|t| allowed_person(policy, t));
            let found = trigger.as_ref().zip(allowed);
            let one = memo::source_trust(source, text, author, found).map_err(Error::Invalid)?;
            trust = trust.min(one);
        }
        Ok(trust)
    }
    fn save_thread(&mut self, thread: Thread) {
        self.changes.threads.insert(thread.key.clone(), thread);
    }
    fn save_ticket(&mut self, ticket: Ticket) {
        self.changes.tickets.insert(ticket.id.clone(), ticket);
    }
    fn effect(&mut self, effect: Effect) {
        self.changes.effects.push(effect);
    }
    fn event(&mut self, event: DomainEvent) {
        self.changes.events.push(Event {
            at_ms: self.now,
            event,
        });
    }
    fn record(&mut self, thread: &mut Thread, decision: Decision) {
        thread.decisions.push(decision.id.clone());
        self.event(DomainEvent::DecisionRecorded {
            decision: decision.id.clone(),
            kind: decision.kind,
            thread: thread.key.clone(),
        });
        self.changes.decisions.push(decision);
    }

    fn add_input(&mut self, ticket: &mut Ticket, body: InputBody, from_allowed_person: bool) {
        ticket.seq += 1;
        let seq = ticket.seq;
        let id = InputId(key("in", &[ticket.id.as_str(), &seq.to_string()]));
        if from_allowed_person {
            ticket.attention = None;
        }
        ticket.inputs.push(Input {
            id,
            seq,
            body,
            from_allowed_person,
            at_ms: self.now,
            sent_to: None,
        });
        self.deliver_waiting(ticket);
    }

    fn deliver_waiting(&mut self, ticket: &mut Ticket) {
        let Some(assignment) = ticket.running_assignment().map(|a| a.id.clone()) else {
            return;
        };
        if ticket.waits_for_person() {
            return;
        }
        let inputs: Vec<InputId> = ticket
            .inputs
            .iter_mut()
            .filter(|input| input.sent_to.is_none())
            .map(|input| {
                input.sent_to = Some(assignment.clone());
                input.id.clone()
            })
            .collect();
        if !inputs.is_empty() {
            self.effect(Effect::Deliver {
                ticket: ticket.id.clone(),
                assignment,
                inputs,
            });
        }
    }

    /// Launch the next Assignment, unless the budget needs a person's
    /// approval under the current threshold. Then ask once and wait.
    fn start(&mut self, ticket: &mut Ticket, policy: &impl Policy) -> Option<AssignmentId> {
        if !needs_approval(ticket, policy) {
            if ticket.approval == Approval::Waiting {
                ticket.approval = Approval::NotNeeded;
                ticket.attention = None;
            }
            return Some(self.launch(ticket));
        }
        if ticket.approval != Approval::Waiting {
            ticket.approval = Approval::Waiting;
            let ask = format!(
                "{} needs approval first: {} with a budget of {}.",
                ticket.title,
                plan_label(&ticket.plan),
                usd(ticket.mandate.budget_microusd.unwrap_or_default())
            );
            ticket.attention = Some(ask.clone());
            self.send(ticket, MessageKind::Attention, ask, Some(Ask::Budget));
        }
        None
    }

    /// A Ticket accepted while Triggers on its Threads waited for an Eval
    /// gets them as if it had existed. An allowed person's follow-up leaves
    /// the queue, unless a Ticket handed it back to be judged or
    /// pass-through is off.
    fn adopt_waiting(&mut self, ticket: &mut Ticket, policy: &impl Policy) {
        for key_ in ticket.threads.clone() {
            let Some(on) = self.thread(&key_) else {
                continue;
            };
            for id in on.triggers {
                let Some(trigger) = self.trigger(&id) else {
                    continue;
                };
                let Some(mut waits) = trigger.threads.first().and_then(|k| self.thread(k)) else {
                    continue;
                };
                let queued = waits.queued.contains(&id);
                let judging = waits
                    .eval
                    .as_ref()
                    .is_some_and(|e| e.triggers.contains(&id));
                if !queued && !judging {
                    continue;
                }
                let owner =
                    queued && !waits.handed_back.contains(&id) && allowed_person(policy, &trigger);
                if self.add_trigger_input(ticket, &id, owner) && owner && self.pass_through {
                    waits.queued.retain(|q| *q != id);
                    self.save_thread(waits);
                }
            }
        }
    }

    /// A Ticket holds each Trigger once, however many of its Threads carry it.
    fn add_trigger_input(&mut self, ticket: &mut Ticket, trigger: &TriggerId, owner: bool) -> bool {
        let held = ticket.inputs.iter().any(
            |input| matches!(&input.body, InputBody::Trigger { trigger: id } if id == trigger),
        );
        if !held {
            let body = InputBody::Trigger {
                trigger: trigger.clone(),
            };
            self.add_input(ticket, body, owner);
        }
        !held
    }

    fn launch(&mut self, ticket: &mut Ticket) -> AssignmentId {
        let recovery = !ticket.assignments.is_empty();
        let n = ticket.assignments.len() + 1;
        let id = AssignmentId(key("as", &[ticket.id.as_str(), &n.to_string()]));
        ticket.assignments.push(Assignment {
            id: id.clone(),
            state: AssignmentState::Pending,
            created_ms: self.now,
            reports: Vec::new(),
            ended_reason: None,
        });
        self.effect(Effect::Launch {
            ticket: ticket.id.clone(),
            assignment: id.clone(),
            plan: ticket.plan.clone(),
            budget_microusd: ticket.mandate.budget_microusd,
            recovery,
        });
        id
    }

    fn end_assignment(&mut self, ticket: &mut Ticket, reason: &str) {
        let ticket_id = ticket.id.clone();
        let Some(latest) = ticket.assignments.last_mut() else {
            return;
        };
        if latest.state == AssignmentState::Ended {
            return;
        }
        latest.state = AssignmentState::Ended;
        latest.ended_reason = Some(reason.to_owned());
        let assignment = latest.id.clone();
        self.effect(Effect::Stop {
            ticket: ticket_id.clone(),
            assignment: assignment.clone(),
            reason: reason.to_owned(),
        });
        self.event(DomainEvent::AssignmentEnded {
            ticket: ticket_id,
            assignment,
            reason: reason.to_owned(),
        });
    }

    fn end_ticket(&mut self, ticket: &mut Ticket, by: EndedBy, outcome: TicketOutcome, why: &str) {
        self.end_assignment(ticket, why);
        ticket.attention = None;
        ticket.ended = Some(Ended {
            at_ms: self.now,
            by,
            outcome,
            reason: why.to_owned(),
        });
        self.event(DomainEvent::TicketEnded {
            ticket: ticket.id.clone(),
            by,
            outcome,
        });
    }

    fn send(&mut self, ticket: &mut Ticket, kind: MessageKind, text: String, ask: Option<Ask>) {
        ticket.seq += 1;
        let seq = ticket.seq.to_string();
        for (n, channel) in ticket.channels.iter().enumerate() {
            let id = MessageId(key("ms", &[ticket.id.as_str(), &seq, &n.to_string()]));
            self.effect(Effect::Send(OutboundMessage {
                id,
                channel: channel.clone(),
                kind,
                text: text.clone(),
                ticket: Some(ticket.id.clone()),
                ask: ask.clone(),
            }));
        }
    }

    fn evaluate(
        &mut self,
        key_: &ThreadKey,
        policy: &impl Policy,
    ) -> Result<Option<EvalId>, Error> {
        let Some(mut thread) = self.thread(key_) else {
            return Ok(None);
        };
        if let Some(eval) = thread.eval.clone() {
            if self.now.saturating_sub(eval.started_ms) < policy.eval_timeout_ms() {
                return Ok(None);
            }
            thread.eval = None;
            thread.consecutive_expiries += 1;
            self.event(DomainEvent::EvalExpired {
                eval: eval.id.clone(),
                thread: key_.clone(),
            });
            if thread.consecutive_expiries >= MAX_EXPIRED_EVALS {
                thread.consecutive_expiries = 0;
                let decision = Decision {
                    id: DecisionId(key("dc", &[eval.id.as_str()])),
                    thread: key_.clone(),
                    eval: Some(eval.id),
                    triggers: eval.triggers,
                    kind: DecisionKind::Unavailable,
                    reason: "the Router did not decide in time".into(),
                    ticket: None,
                    would_accept: false,
                    policy_version: eval.policy_version,
                    at_ms: self.now,
                };
                self.record(&mut thread, decision);
            } else {
                requeue(&mut thread, eval.triggers);
            }
        }
        if thread.queued.is_empty() {
            self.save_thread(thread);
            return Ok(None);
        }
        let queued = std::mem::take(&mut thread.queued);
        let has_tickets = !thread.subscriptions.is_empty();
        let mut admitted: Vec<(Strength, Trigger)> = Vec::new();
        let mut rejected: Vec<(TriggerId, String)> = Vec::new();
        for trigger in queued.iter().filter_map(|id| self.trigger(id)) {
            match policy.gate(&trigger, has_tickets) {
                Gate::Admit { strength } => admitted.push((strength, trigger)),
                Gate::Reject { reason } => rejected.push((trigger.id, reason)),
            }
        }
        let version = policy.version();
        if !rejected.is_empty() {
            let decision = Decision {
                id: DecisionId(key(
                    "dc",
                    &[key_.as_str(), "gate", &thread.decisions.len().to_string()],
                )),
                thread: key_.clone(),
                eval: None,
                triggers: rejected.iter().map(|(id, _)| id.clone()).collect(),
                kind: DecisionKind::Ignore,
                reason: format!(
                    "gate: {}",
                    rejected
                        .iter()
                        .map(|(id, why)| format!("{id}: {why}"))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                ticket: None,
                would_accept: false,
                policy_version: version.clone(),
                at_ms: self.now,
            };
            self.record(&mut thread, decision);
        }
        let Some(strength) = admitted.iter().map(|(s, _)| *s).max() else {
            thread.consecutive_expiries = 0;
            self.save_thread(thread);
            return Ok(None);
        };
        let (now, later): (Vec<_>, Vec<_>) =
            admitted.into_iter().partition(|(s, _)| *s == strength);
        thread.queued = later.into_iter().map(|(_, t)| t.id).collect();
        let triggers: Vec<Trigger> = now.into_iter().map(|(_, t)| t).collect();
        let batch: Vec<TriggerId> = triggers.iter().map(|t| t.id.clone()).collect();
        let primary = batch[0].clone();
        thread.evals_started += 1;
        let id = EvalId(key(
            "ev",
            &[key_.as_str(), &thread.evals_started.to_string()],
        ));
        let tickets = thread
            .subscriptions
            .iter()
            .filter_map(|t| self.ticket(t))
            .map(|t| TicketSummary {
                stage: t.stage(),
                id: t.id,
                title: t.title,
                scope: t.mandate.scope,
            })
            .collect();
        self.effect(Effect::Assess(Candidate {
            eval: id.clone(),
            thread: key_.clone(),
            strength,
            policy_version: version.clone(),
            triggers,
            tickets,
        }));
        self.event(DomainEvent::EvalStarted {
            eval: id.clone(),
            thread: key_.clone(),
            triggers: batch.clone(),
            strength,
        });
        thread.eval = Some(Eval {
            id: id.clone(),
            triggers: batch,
            primary,
            strength,
            policy_version: version,
            started_ms: self.now,
        });
        self.save_thread(thread);
        Ok(Some(id))
    }

    fn decide(
        &mut self,
        key_: &ThreadKey,
        eval: &EvalId,
        verdict: Verdict,
        policy: &impl Policy,
    ) -> Result<DecisionId, Error> {
        let mut thread = self.thread(key_).ok_or_else(|| Error::Missing {
            kind: "thread",
            id: key_.to_string(),
        })?;
        let Some(current) = thread.eval.take().filter(|e| e.id == *eval) else {
            return Err(Error::Refused(format!("{eval} is not running")));
        };
        let version = policy.version();
        let id = DecisionId(key("dc", &[current.id.as_str()]));
        let mut decision = Decision {
            id: id.clone(),
            thread: key_.clone(),
            eval: Some(current.id.clone()),
            triggers: current.triggers.clone(),
            kind: DecisionKind::Ignore,
            reason: String::new(),
            ticket: None,
            would_accept: false,
            policy_version: version.clone(),
            at_ms: self.now,
        };
        thread.consecutive_expiries = 0;
        if current.policy_version != version {
            requeue(&mut thread, current.triggers);
            decision.kind = DecisionKind::Unavailable;
            decision.reason = "the policy changed during the eval".into();
            self.record(&mut thread, decision);
            self.save_thread(thread);
            return Ok(id);
        }
        match verdict {
            Verdict::Ignore { reason } => decision.reason = reason,
            Verdict::Stop { ticket, reason } => {
                if current.strength != Strength::Explicit {
                    return Err(Error::Refused("only an explicit request stops work".into()));
                }
                let mut t = self.require_ticket(&ticket)?;
                let on_batch = current
                    .triggers
                    .iter()
                    .filter_map(|id| self.trigger(id))
                    .any(|trigger| trigger.threads.iter().any(|k| t.threads.contains(k)));
                if !on_batch {
                    return Err(Error::Refused(format!(
                        "{ticket} follows no Thread of this batch"
                    )));
                }
                if !t.stopped_by_person() {
                    self.end_ticket(&mut t, EndedBy::Person, TicketOutcome::Stopped, &reason);
                    self.save_ticket(t);
                }
                decision.kind = DecisionKind::Stop;
                decision.reason = reason;
                decision.ticket = Some(ticket);
            }
            Verdict::Accept(accept) => {
                validate(&accept)?;
                if current.strength == Strength::Inferred && !policy.admits_inferred(&accept.rules)
                {
                    decision.would_accept = true;
                    decision.reason = format!("would accept: {}", accept.reason);
                } else {
                    decision.kind = DecisionKind::Accept;
                    decision.reason = accept.reason.clone();
                    let ticket = self.create_ticket(&mut thread, &current, &id, accept, policy)?;
                    decision.ticket = Some(ticket);
                }
            }
        }
        self.record(&mut thread, decision);
        self.save_thread(thread);
        Ok(id)
    }

    fn create_ticket(
        &mut self,
        thread: &mut Thread,
        eval: &Eval,
        decision: &DecisionId,
        accept: Accept,
        policy: &impl Policy,
    ) -> Result<TicketId, Error> {
        let primary = self.trigger(&eval.primary).ok_or_else(|| Error::Missing {
            kind: "trigger",
            id: eval.primary.to_string(),
        })?;
        let id = TicketId(key("tk", &[eval.id.as_str()]));
        let mut threads: BTreeSet<ThreadKey> = BTreeSet::from([thread.key.clone()]);
        for trigger in eval.triggers.iter().filter_map(|t| self.trigger(t)) {
            threads.extend(trigger.threads);
        }
        thread.subscriptions.insert(id.clone());
        for key_ in threads.iter().filter(|k| **k != thread.key) {
            let mut other = self
                .thread(key_)
                .unwrap_or_else(|| Thread::new(key_.clone()));
            other.subscriptions.insert(id.clone());
            self.save_thread(other);
        }
        let mut ticket = Ticket {
            id: id.clone(),
            title: accept.title,
            origin: eval.primary.clone(),
            decision: decision.clone(),
            mandate: Mandate {
                policy_version: eval.policy_version.clone(),
                scope: accept.scope,
                strength: eval.strength,
                rules: accept.rules,
                requester: primary.actor,
                source: primary.source,
                budget_microusd: accept.budget_microusd,
                repo: accept.repo,
                forge: accept.forge,
            },
            plan: accept.plan,
            channels: primary.reply_to,
            threads,
            inputs: Vec::new(),
            assignments: Vec::new(),
            approval: Approval::NotNeeded,
            secrets: BTreeMap::new(),
            attention: None,
            ended: None,
            created_ms: self.now,
            seq: 0,
            handed_back: BTreeSet::new(),
            change: None,
        };
        self.event(DomainEvent::TicketCreated {
            ticket: id.clone(),
            decision: decision.clone(),
            plan: ticket.plan.clone(),
            budget_microusd: ticket.mandate.budget_microusd,
        });
        let ack = match accept.acknowledge {
            Acknowledge::Standard => Some(crate::model::STANDARD_ACKNOWLEDGEMENT.to_owned()),
            Acknowledge::Silent => None,
            Acknowledge::Text(text) => Some(text),
        };
        if let Some(ack) = ack
            && !needs_approval(&ticket, policy)
        {
            self.send(&mut ticket, MessageKind::Acknowledgement, ack, None);
        }
        self.save_thread(thread.clone());
        self.adopt_waiting(&mut ticket, policy);
        if let Some(saved) = self.thread(&thread.key) {
            *thread = saved;
        }
        self.start(&mut ticket, policy);
        self.save_ticket(ticket);
        Ok(id)
    }
}
