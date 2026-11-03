//! Every command runs in one transaction: it reads through an overlay of its
//! own uncommitted changes, and commits state, Effects and events together
//! through [`Repositories::commit`]. A command that returns an error commits
//! nothing.

use crate::ids::key;
use crate::model::{
    Accept, Acknowledge, Actor, Approval, Assignment, AssignmentId, AssignmentState, Candidate,
    ChangeSet, Decision, DecisionId, DecisionKind, DomainEvent, Effect, Ended, EndedBy, Eval,
    EvalId, Event, Input, InputBody, InputId, Mandate, MessageId, MessageKind, MicroUsd, Ms,
    OutboundMessage, Plan, Report, ReportIn, ReportStatus, Strength, Thread, ThreadKey, Ticket,
    TicketId, TicketOutcome, TicketSummary, Trigger, TriggerId, Verdict,
};
use crate::ports::{Clock, Error, Gate, Policy, Repositories};
use crate::trail::{self, TrailAction, TrailCursor, TrailOutcome};
use std::collections::BTreeSet;

/// The third consecutive expired Eval on a Thread records `unavailable`.
pub const MAX_EXPIRED_EVALS: u32 = 3;
pub const MAX_SWARM_AGENTS: u16 = 30;

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
                        owner && !ticket.has_live_assignment()
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
            let mut t = tx.open_ticket(ticket)?;
            if t.approval != Approval::Waiting {
                return Err(Error::Refused(
                    "the ticket does not wait for approval".into(),
                ));
            }
            if !by.is_person() || !policy.may_approve(source, &by) {
                return Err(Error::Refused(format!("{} may not approve budgets", by.id)));
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

    /// A person's stop: withdraws the Mandate, so nothing resumes it.
    pub fn stop_ticket(&mut self, ticket: &TicketId, reason: &str) -> Result<(), Error> {
        self.run(|tx, _| {
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
    /// Ticket; `dropped` ends it silently; a question, `blocked` or `failed`
    /// asks a person; `progress` and `waiting` stay quiet.
    pub fn report(
        &mut self,
        ticket: &TicketId,
        assignment: &AssignmentId,
        report: ReportIn,
    ) -> Result<(), Error> {
        self.run(|tx, _| {
            let mut t = tx.open_ticket(ticket)?;
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
                tx.send(&mut t, MessageKind::Attention, question.clone());
            }
            match report.status {
                ReportStatus::Ready => {
                    tx.send(&mut t, MessageKind::Result, report.text);
                    tx.end_ticket(&mut t, EndedBy::Executor, TicketOutcome::Completed, "ready");
                }
                ReportStatus::Dropped => {
                    tx.end_ticket(
                        &mut t,
                        EndedBy::Executor,
                        TicketOutcome::Dropped,
                        &report.text,
                    );
                }
                ReportStatus::Blocked | ReportStatus::Failed if report.question.is_none() => {
                    tx.send(&mut t, MessageKind::Attention, report.text);
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
        && ticket.mandate.budget_microusd > policy.auto_approve_microusd()
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
    if accept.budget_microusd == 0 {
        return Err(Error::Invalid("an accept needs a budget".into()));
    }
    Ok(())
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
                usd(ticket.mandate.budget_microusd)
            );
            ticket.attention = Some(ask.clone());
            self.send(ticket, MessageKind::Attention, ask);
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

    fn send(&mut self, ticket: &mut Ticket, kind: MessageKind, text: String) {
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
            },
            plan: accept.plan,
            channels: primary.reply_to,
            threads,
            inputs: Vec::new(),
            assignments: Vec::new(),
            approval: Approval::NotNeeded,
            attention: None,
            ended: None,
            created_ms: self.now,
            seq: 0,
            handed_back: BTreeSet::new(),
        };
        self.event(DomainEvent::TicketCreated {
            ticket: id.clone(),
            decision: decision.clone(),
            plan: ticket.plan.clone(),
            budget_microusd: ticket.mandate.budget_microusd,
        });
        let ack = match accept.acknowledge {
            Acknowledge::Standard => Some(format!("On it: {}.", ticket.title)),
            Acknowledge::Silent => None,
            Acknowledge::Text(text) => Some(text),
        };
        if let Some(ack) = ack
            && !needs_approval(&ticket, policy)
        {
            self.send(&mut ticket, MessageKind::Acknowledgement, ack);
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
