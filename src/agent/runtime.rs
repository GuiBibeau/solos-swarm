//! The long-running loop. One task owns the Engine, so state has one writer.
//! Adapters run concurrently and send facts and Effect outcomes back to it.

use super::ports::{
    Answer, Applied, BoxFuture, Channel, Command, Delivered, Delivery, Executor, Fact, FactSink,
    Failure, Inbound, Launch, Order, Router, SendOptions, SendOutcome, Source,
};
use super::watchdog;
use crate::broker::now_ms;
use crate::gateway;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use solos_agent_core::ids::key;
use solos_agent_core::model::{
    Actor, AssignmentId, AssignmentState, Candidate, Effect, EvalId, Input, InputBody, MessageId,
    MessageKind, Ms, OutboundMessage, Stage, Ticket, TicketId, Trigger,
};
use solos_agent_core::{AllowlistPolicy, Engine, Error, Intake, Policy, Repositories};
use solos_agent_store::{EffectId, QueuedEffect, SqliteStore};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::{self, JoinError, JoinSet};
use tokio::time::MissedTickBehavior;

pub type AgentEngine = Engine<SqliteStore, AllowlistPolicy, fn() -> Ms>;

const MAX_IN_FLIGHT: usize = 64;
/// Why a start ends the Assignments the last run left without a worker.
pub const HOST_RESTARTED: &str = "host restarted";

pub struct Adapters {
    pub sources: Vec<Box<dyn Source>>,
    /// By the `ChannelRef::source` each serves.
    pub channels: BTreeMap<String, Arc<dyn Channel>>,
    pub router: Arc<dyn Router>,
    pub executor: Arc<dyn Executor>,
}

#[derive(Clone, Debug)]
pub struct RunOptions {
    /// Exit once every Source has ended and nothing is left to do, instead
    /// of serving until stopped.
    pub until_idle: bool,
    /// How often expiries and retries are checked when nothing else happens.
    pub tick: Duration,
    /// The first retry waits this long, each later one twice as long as the
    /// one before, up to `retry_max`.
    pub retry_base: Duration,
    pub retry_max: Duration,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            until_idle: false,
            tick: Duration::from_millis(200),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(300),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub triggers_received: u64,
    pub triggers_duplicate: u64,
    pub triggers_refused: u64,
    pub effects_done: u64,
    pub effect_failures: u64,
    /// Effects not done when the run ended, such as those a pause holds.
    pub effects_pending: u64,
}

#[derive(Clone)]
struct Shared {
    router: Arc<dyn Router>,
    executor: Arc<dyn Executor>,
    channels: Arc<BTreeMap<String, Arc<dyn Channel>>>,
}

pub struct Runtime {
    engine: AgentEngine,
    sources: Vec<Box<dyn Source>>,
    shared: Shared,
    options: RunOptions,
    summary: Summary,
    in_flight: BTreeSet<EffectId>,
    /// Which Effect each running adapter task performs, so a panic fails it.
    tasks: HashMap<task::Id, (EffectId, u32)>,
    facts: FactSink,
    inbound: Option<mpsc::Receiver<Inbound>>,
    /// A directory whose pause gate (`control.json`, `PAUSE`) pauses the
    /// agent as the Policy's own `paused` does.
    control_dir: Option<PathBuf>,
    /// The Policy's own `paused`, which the directory's gate adds to.
    configured_pause: bool,
    gate_checked: Option<Instant>,
    /// The last Effect committed before this run. A crash may have cut any
    /// of them short after its adapter acted, so a Send among them is a
    /// possible repeat.
    recovered: Option<EffectId>,
    /// Answers to commands, waiting to go out. They are not Effects: a crash
    /// loses an unsent one, never the command's own outcome.
    replies: Vec<OutboundMessage>,
    closing: Option<BoxFuture<'static, ()>>,
}

impl Runtime {
    pub fn new(
        store: SqliteStore,
        policy: AllowlistPolicy,
        adapters: Adapters,
        options: RunOptions,
    ) -> Self {
        Self::with_sink(store, policy, adapters, options, FactSink::channel())
    }

    /// A runtime whose fact sink was made first, so adapters that need it,
    /// such as a broker, can be built before the runtime.
    pub(crate) fn with_sink(
        store: SqliteStore,
        policy: AllowlistPolicy,
        adapters: Adapters,
        options: RunOptions,
        (facts, inbound): (FactSink, mpsc::Receiver<Inbound>),
    ) -> Self {
        let clock: fn() -> Ms = now_ms;
        Self {
            configured_pause: policy.paused,
            engine: Engine::new(store, policy, clock),
            sources: adapters.sources,
            shared: Shared {
                router: adapters.router,
                executor: adapters.executor,
                channels: Arc::new(adapters.channels),
            },
            options,
            summary: Summary::default(),
            in_flight: BTreeSet::new(),
            tasks: HashMap::new(),
            facts,
            inbound: Some(inbound),
            control_dir: None,
            gate_checked: None,
            recovered: None,
            replies: Vec::new(),
            closing: None,
        }
    }

    /// Before `run` returns, run `closing` while the facts it sends are
    /// still applied, such as each open browser's signed Close.
    pub fn with_closing(mut self, closing: impl Future<Output = ()> + Send + 'static) -> Self {
        self.closing = Some(Box::pin(closing));
        self
    }

    /// Pause while `dir`'s pause gate is shut, as a broker does.
    pub fn with_control_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.control_dir = Some(dir.into());
        self
    }

    /// With `false`, an allowed person's follow-up on a subscribed Thread
    /// also goes to the Router (see `Engine::set_pass_through`).
    pub fn with_pass_through(mut self, on: bool) -> Self {
        self.engine.set_pass_through(on);
        self
    }

    /// Where facts for this runtime go, before and while it runs.
    pub fn facts(&self) -> FactSink {
        self.facts.clone()
    }

    /// Serve until `stop` resolves, or with `until_idle` until there is
    /// nothing left to do. Dropping the future stops everything at once, as
    /// a crash would; every Effect not marked done runs again on the next
    /// start.
    pub async fn run(mut self, stop: impl Future<Output = ()>) -> Result<Summary> {
        self.recovered = self.engine.repos().effects()?.last().map(|q| q.id);
        self.recover()?;
        let facts = self.facts.clone();
        let mut inbound = self.inbound.take().context("a runtime runs once")?;
        let (trigger_tx, mut triggers) = mpsc::channel::<Vec<Trigger>>(16);
        let mut sources = JoinSet::new();
        for source in std::mem::take(&mut self.sources) {
            sources.spawn(source.run(trigger_tx.clone(), facts.clone()));
        }
        drop(trigger_tx);
        let mut sources_open = true;
        let mut work = JoinSet::new();
        let mut tick = tokio::time::interval(self.options.tick);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tokio::pin!(stop);
        loop {
            self.gate();
            self.evaluate()?;
            self.dispatch(&mut work, &facts)?;
            let drained =
                !sources_open && sources.is_empty() && inbound.is_empty() && work.is_empty();
            if self.options.until_idle && drained && self.idle()? {
                break;
            }
            // A batch is stored whole before the next evaluation, so Triggers
            // read together are judged in one Eval.
            tokio::select! {
                biased;
                () = &mut stop => break,
                batch = triggers.recv(), if sources_open => match batch {
                    Some(batch) => self.receive(batch)?,
                    None => sources_open = false,
                },
                Some(ended) = sources.join_next() => {
                    ended.context("a source panicked")?.context("a source failed")?;
                }
                Some(message) = inbound.recv() => self.inbound(message)?,
                Some(joined) = work.join_next_with_id() => self.reap(joined)?,
                _ = tick.tick() => {}
            }
        }
        if let Some(mut closing) = self.closing.take() {
            loop {
                tokio::select! {
                    biased;
                    () = &mut closing => break,
                    Some(message) = inbound.recv() => self.inbound(message)?,
                }
            }
        }
        let pending = self
            .engine
            .repos()
            .effects()?
            .iter()
            .filter(|q| !q.done)
            .count();
        self.summary.effects_pending = pending as u64;
        Ok(self.summary)
    }

    /// Workers die with the process that ran them. An Assignment the last
    /// run left running, or pending with its Launch performed, ends as
    /// `host restarted`, oldest Ticket first. One that was working starts
    /// again at once with `recovery` set; one that waited for input, an
    /// answer or a person starts again with the next Input that wakes it. A
    /// pending one whose Launch is still queued is left to that Launch.
    fn recover(&mut self) -> Result<()> {
        let store = self.engine.repos();
        let queued: BTreeSet<AssignmentId> = store
            .effects()?
            .into_iter()
            .filter(|q| !q.done)
            .filter_map(|q| match q.effect {
                Effect::Launch { assignment, .. } => Some(assignment),
                _ => None,
            })
            .collect();
        let mut lost = Vec::new();
        for id in store.open_tickets()? {
            let Some(ticket) = store.ticket(&id) else {
                continue;
            };
            let Some(latest) = ticket.latest_assignment() else {
                continue;
            };
            let gone = match latest.state {
                AssignmentState::Running => true,
                AssignmentState::Pending => !queued.contains(&latest.id),
                AssignmentState::Ended => false,
            };
            if gone {
                let working = matches!(ticket.stage(), Stage::Queued | Stage::Working);
                lost.push((ticket.created_ms, id, latest.id.clone(), working));
            }
        }
        lost.sort();
        for (_, ticket, assignment, working) in lost {
            let result = if working {
                self.engine
                    .restart(&ticket, &assignment, HOST_RESTARTED)
                    .map(drop)
            } else {
                self.engine
                    .assignment_ended(&ticket, &assignment, HOST_RESTARTED)
            };
            let what = format!("recovery of {assignment} on {ticket}");
            settle(&what, result)?;
            let next = if working {
                "a recovery starts now"
            } else {
                "its next input starts a recovery"
            };
            eprintln!("agent: {assignment} of {ticket} ended: {HOST_RESTARTED}; {next}");
        }
        Ok(())
    }

    /// Follow the control directory's pause gate, reading it at most once a
    /// tick. Pausing does not change the Policy version, so running Evals
    /// stay valid.
    fn gate(&mut self) {
        let Some(dir) = &self.control_dir else {
            return;
        };
        if self
            .gate_checked
            .is_some_and(|at| at.elapsed() < self.options.tick)
        {
            return;
        }
        self.gate_checked = Some(Instant::now());
        let paused = self.configured_pause
            || gateway::effective_control(dir, gateway::unix_secs()).is_some();
        if paused != self.engine.policy().paused() {
            let mut policy = self.engine.policy().clone();
            policy.paused = paused;
            self.engine.set_policy(policy);
            let what = if paused { "paused" } else { "unpaused" };
            eprintln!("agent: {what} by {}", dir.display());
        }
    }

    fn evaluate(&mut self) -> Result<()> {
        if self.engine.repos().active_threads().is_empty() {
            return Ok(());
        }
        settle("evaluate", self.engine.evaluate_due().map(drop))
    }

    fn receive(&mut self, batch: Vec<Trigger>) -> Result<()> {
        for trigger in batch {
            let id = trigger.id.clone();
            match self.engine.receive(trigger) {
                Ok(Intake::Stored { .. }) => self.summary.triggers_received += 1,
                Ok(Intake::Duplicate) => self.summary.triggers_duplicate += 1,
                Err(Error::Store(why)) => bail!("storing trigger {id}: {why}"),
                Err(err) => {
                    self.summary.triggers_refused += 1;
                    eprintln!("agent: trigger {id} refused: {err}");
                }
            }
        }
        Ok(())
    }

    /// Apply a fact as its Engine command. The outer error is a Store
    /// failure, which stops the runtime; the answer says what the Engine did.
    fn apply(&mut self, fact: Fact) -> Result<Answer> {
        let done = |()| Applied::Done;
        let (what, result) = match fact {
            Fact::Verdict { eval, verdict } => {
                let decided = self
                    .engine
                    .decide(&eval, verdict)
                    .map(|id| {
                        self.engine
                            .repos()
                            .decision(&id)
                            .map_or(Applied::Done, Applied::Decision)
                    })
                    .map_err(|err| match err {
                        Error::Missing {
                            kind: "running eval",
                            ..
                        } => Error::Refused(self.refusal_with_open_evals_by_thread(&eval)),
                        err => err,
                    });
                (format!("verdict on {eval}"), decided)
            }
            Fact::Started { ticket, assignment } => (
                format!("start of {assignment} on {ticket}"),
                self.engine
                    .assignment_started(&ticket, &assignment)
                    .map(done),
            ),
            Fact::Report {
                ticket,
                assignment,
                report,
            } => (
                format!("report on {assignment} of {ticket}"),
                self.engine.report(&ticket, &assignment, report).map(done),
            ),
            Fact::Ended {
                ticket,
                assignment,
                reason,
            } => (
                format!("end of {assignment} on {ticket}"),
                self.engine
                    .assignment_ended(&ticket, &assignment, &reason)
                    .map(done),
            ),
            Fact::Handback {
                ticket,
                trigger,
                reason,
            } => (
                format!("handback of {trigger} by {ticket}"),
                self.engine
                    .handback(&ticket, &trigger, &reason)
                    .map(|queued| Applied::HandedBack { queued }),
            ),
            Fact::Note { ticket, text } => (
                format!("note on {ticket}"),
                self.engine.note(&ticket, text).map(done),
            ),
            Fact::Command(command) => return self.command(command).map(|()| Ok(Applied::Done)),
            Fact::TrailAsk {
                ticket,
                assignment,
                action,
            } => (
                format!("trail step asked by {assignment} on {ticket}"),
                self.engine
                    .trail_ask(&ticket, &assignment, action)
                    .map(|n| Applied::TrailAsked { n }),
            ),
            Fact::TrailDone {
                ticket,
                assignment,
                n,
                outcome,
            } => (
                format!("trail step {n} of {assignment} on {ticket}"),
                self.engine
                    .trail_done(&ticket, &assignment, n, outcome)
                    .map(done),
            ),
            Fact::SecretAsk {
                ticket,
                secret,
                label,
                answerable_by,
            } => (
                format!("secret ask of {secret} on {ticket}"),
                self.engine
                    .ask_secret(&ticket, &secret, &label, &answerable_by)
                    .map(Applied::Secret),
            ),
            Fact::KeyUsed {
                ticket,
                assignment,
                secret,
                method,
                url,
            } => (
                format!("keyed use of {secret} by {assignment} on {ticket}"),
                self.engine
                    .use_key(&ticket, &assignment, &secret, &method, &url)
                    .map(done),
            ),
            Fact::Retry {
                ticket,
                assignment,
                reports,
            } => (
                format!("watchdog retry of {assignment} on {ticket}"),
                self.retry_stalled(&ticket, &assignment, reports).map(done),
            ),
        };
        match result {
            Ok(applied) => Ok(Ok(applied)),
            Err(Error::Store(why)) => bail!("{what}: {why}"),
            Err(err) => {
                eprintln!("agent: {what}: {err}");
                Ok(Err(err.to_string()))
            }
        }
    }

    fn refusal_with_open_evals_by_thread(&self, eval: &EvalId) -> String {
        const LISTED: usize = 10;
        let repos = self.engine.repos();
        let mut open: Vec<String> = repos
            .active_threads()
            .into_iter()
            .filter_map(|key| {
                let running = repos.thread(&key)?.eval?;
                Some(format!("{} (thread {key})", running.id))
            })
            .collect();
        open.sort();
        let next = if open.is_empty() {
            "No eval is open now. End your turn: the next prompt carries any new candidates."
                .to_owned()
        } else {
            let more = open.len().saturating_sub(LISTED);
            open.truncate(LISTED);
            let more = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            format!(
                "Evals open now: {}{more}. Decide one only if its card was sent to you. The \
                 other cards come in the next prompt, so end your turn once the open evals \
                 you hold are decided.",
                open.join(", ")
            )
        };
        format!(
            "{eval} is closed: it expired or was already decided, so it cannot be decided. \
             Do not retry it. {next}"
        )
    }

    /// The Watchdog's retry, unless the Assignment moved on since it looked.
    fn retry_stalled(
        &mut self,
        ticket: &TicketId,
        assignment: &AssignmentId,
        reports: usize,
    ) -> Result<(), Error> {
        let found = self.engine.repos().ticket(ticket);
        if !found.is_some_and(|t| watchdog::still_stalled(&t, assignment, reports)) {
            return Err(Error::Refused(format!("{assignment} is no longer stalled")));
        }
        self.engine.retry(ticket).map(drop)
    }

    /// Apply a person's command and answer in the thread it came from.
    fn command(&mut self, command: Command) -> Result<()> {
        let Command {
            id,
            order,
            ticket,
            source,
            by,
            reply_to,
        } = command;
        let who = by.name.clone().unwrap_or_else(|| by.id.clone());
        let policy = self.engine.policy();
        let outcome = match &order {
            Order::Approve if !policy.may_approve(&source, &by) => {
                Err(format!("{who} is not allowed to approve budgets."))
            }
            Order::Stop if !may_stop(policy, &source, &by) => {
                Err(format!("{who} is not allowed to stop this work."))
            }
            Order::Approve => told(
                self.engine.approve(&ticket, &source, by.clone()),
                format!("Approved by {who}. The work starts now."),
            )?,
            Order::Stop => told(
                self.engine
                    .stop_ticket(&ticket, &format!("stopped by {who} on {source}")),
                format!("Stopped by {who}."),
            )?,
            Order::Secret { secret, allow } => told(
                self.engine
                    .answer_secret(&ticket, secret, &source, by.clone(), *allow),
                format!(
                    "{} by {who}: {secret} for this ticket.",
                    if *allow { "Allowed" } else { "Denied" }
                ),
            )?,
        };
        let text = outcome.unwrap_or_else(|refused| {
            eprintln!(
                "agent: {order:?} of {ticket} by {source}:{} refused: {refused}",
                by.id
            );
            refused
        });
        self.replies.push(OutboundMessage {
            id: MessageId(key("rp", &[&id])),
            channel: reply_to,
            kind: MessageKind::Acknowledgement,
            text,
            ticket: Some(ticket),
            ask: None,
        });
        Ok(())
    }

    /// Hand due Effects to their adapters. A pause holds every Effect, per
    /// the core's rule; `evaluate_due` starts no Evals meanwhile.
    fn dispatch(&mut self, work: &mut JoinSet<()>, facts: &FactSink) -> Result<()> {
        // A person's command was applied even under a pause, so its answer goes too.
        for reply in std::mem::take(&mut self.replies) {
            let Some(channel) = self.shared.channels.get(&reply.channel.source).cloned() else {
                eprintln!(
                    "agent: no channel serves {}; a command's answer was dropped",
                    reply.channel.source
                );
                continue;
            };
            work.spawn(async move {
                if let Err(err) = channel.send(reply, SendOptions::default()).await {
                    eprintln!("agent: a command's answer was not sent: {err:#}");
                }
            });
        }
        let room = MAX_IN_FLIGHT.saturating_sub(self.in_flight.len());
        if self.engine.policy().paused() || room == 0 {
            return Ok(());
        }
        // In-flight Effects are still pending, so they come back here too.
        let due = self
            .engine
            .repos()
            .due_effects(now_ms(), room + self.in_flight.len())?;
        for queued in due {
            let QueuedEffect {
                id,
                attempts,
                effect,
                ..
            } = queued;
            if self.in_flight.contains(&id) {
                continue;
            }
            let repeat = attempts > 0 || self.recovered.is_some_and(|last| id <= last);
            let job = match self.job(effect, repeat) {
                Ok(job) => job,
                Err(err) => {
                    self.failed(id, attempts, &Failure::from(err))?;
                    continue;
                }
            };
            let (shared, facts) = (self.shared.clone(), facts.clone());
            let handle = work.spawn(async move {
                let result = job
                    .perform(shared, facts.clone())
                    .await
                    .map_err(Failure::from);
                let outcome = Inbound::Outcome {
                    id,
                    attempts,
                    result,
                };
                let _ = facts.0.send(outcome).await;
            });
            self.in_flight.insert(id);
            self.tasks.insert(handle.id(), (id, attempts));
        }
        Ok(())
    }

    /// What the adapter needs beyond the Effect, read from the store now.
    /// `repeat`: an earlier attempt may have performed it.
    fn job(&self, effect: Effect, repeat: bool) -> Result<Job> {
        let store = self.engine.repos();
        Ok(match effect {
            Effect::Assess(candidate) => Job::Assess(candidate),
            Effect::Launch {
                ticket,
                assignment,
                plan,
                budget_microusd,
                recovery,
            } => {
                let found = store.ticket(&ticket).context("no such ticket")?;
                if !live(&found, &assignment) {
                    return Ok(Job::Skip(format!(
                        "{assignment} of {ticket} has ended; it is not launched"
                    )));
                }
                let inputs = found
                    .inputs
                    .iter()
                    .map(|input| self.delivered(input.clone()))
                    .collect::<Result<_>>()?;
                let origin = store
                    .trigger(&found.origin)
                    .with_context(|| format!("no trigger {}", found.origin))?;
                Job::Launch(Box::new(Launch {
                    ticket,
                    assignment,
                    title: found.title,
                    scope: found.mandate.scope,
                    origin,
                    rules: found.mandate.rules,
                    plan,
                    budget_microusd,
                    recovery: recovery || repeat,
                    inputs,
                }))
            }
            Effect::Deliver {
                ticket,
                assignment,
                inputs,
            } => {
                let found = store.ticket(&ticket).context("no such ticket")?;
                let inputs = inputs
                    .iter()
                    .map(|id| {
                        let input = found
                            .inputs
                            .iter()
                            .find(|input| input.id == *id)
                            .cloned()
                            .with_context(|| format!("{ticket} has no input {id}"))?;
                        self.delivered(input)
                    })
                    .collect::<Result<_>>()?;
                Job::Deliver(Delivery {
                    ticket,
                    assignment,
                    inputs,
                })
            }
            Effect::Stop {
                ticket,
                assignment,
                reason,
            } => Job::Stop {
                ticket,
                assignment,
                reason,
            },
            Effect::Send(message) => {
                let ask = message.ask.clone().filter(|ask| {
                    message
                        .ticket
                        .as_ref()
                        .and_then(|id| store.ticket(id))
                        .is_some_and(|t| t.awaits(ask))
                });
                Job::Send(message, SendOptions { repeat, ask })
            }
        })
    }

    /// An Input with the Trigger it names.
    fn delivered(&self, input: Input) -> Result<Delivered> {
        let trigger = match &input.body {
            InputBody::Trigger { trigger } => Some(
                self.engine
                    .repos()
                    .trigger(trigger)
                    .with_context(|| format!("no trigger {trigger}"))?,
            ),
            _ => None,
        };
        Ok(Delivered { input, trigger })
    }

    fn inbound(&mut self, message: Inbound) -> Result<()> {
        match message {
            Inbound::Fact(fact) => self.apply(fact).map(drop),
            Inbound::Ask(fact, reply) => {
                let applied = self.apply(fact);
                let answer = match &applied {
                    Ok(answer) => answer.clone(),
                    Err(err) => Err(format!("{err:#}")),
                };
                // The asker may have gone; the fact is applied either way.
                let _ = reply.send(answer);
                applied.map(drop)
            }
            Inbound::Outcome {
                id,
                attempts,
                result,
            } => {
                if !self.in_flight.remove(&id) {
                    return Ok(());
                }
                match result {
                    Ok(()) => {
                        self.engine.repos().complete_effect(id)?;
                        self.summary.effects_done += 1;
                        Ok(())
                    }
                    Err(failure) => self.failed(id, attempts, &failure),
                }
            }
        }
    }

    /// A finished adapter task already sent its outcome; one that panicked
    /// could not, so its Effect fails here.
    fn reap(&mut self, joined: Result<(task::Id, ()), JoinError>) -> Result<()> {
        let err = match joined {
            Ok((task, ())) => {
                self.tasks.remove(&task);
                return Ok(());
            }
            Err(err) => err,
        };
        let Some((id, attempts)) = self.tasks.remove(&err.id()) else {
            return Ok(());
        };
        if !self.in_flight.remove(&id) {
            return Ok(());
        }
        let failure = Failure {
            why: format!("the adapter task failed: {err}"),
            retry_after: None,
        };
        self.failed(id, attempts, &failure)
    }

    fn failed(&mut self, id: EffectId, attempts: u32, failure: &Failure) -> Result<()> {
        let wait = backoff(&self.options, attempts).max(failure.retry_after.unwrap_or_default());
        let why = &failure.why;
        eprintln!(
            "agent: effect {id} failed (attempt {}): {why}; retry in {wait:?}",
            attempts + 1
        );
        let wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        self.engine
            .repos()
            .fail_effect(id, attempts, why, now_ms().saturating_add(wait_ms))?;
        self.summary.effect_failures += 1;
        Ok(())
    }

    /// Nothing left to do: no Eval to start or decide, no Effect pending, and
    /// no Assignment the Executor still owes a fact. Under a pause, Effects
    /// wait for the pause to lift instead.
    fn idle(&self) -> Result<bool> {
        if !self.in_flight.is_empty() {
            return Ok(false);
        }
        if self.engine.policy().paused() {
            return Ok(true);
        }
        let store = self.engine.repos();
        if !store.active_threads().is_empty() || !store.due_effects(Ms::MAX >> 1, 1)?.is_empty() {
            return Ok(false);
        }
        for id in store.open_tickets()? {
            let owed = store
                .ticket(&id)
                .is_some_and(|t| matches!(t.stage(), Stage::Queued | Stage::Working));
            if owed {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// A Store error stops the runtime. Any other refusal is a stale or repeated
/// fact, such as one a redone Effect reports again, and changes nothing.
fn settle(what: &str, result: Result<(), Error>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(Error::Store(why)) => bail!("{what}: {why}"),
        Err(err) => {
            eprintln!("agent: {what}: {err}");
            Ok(())
        }
    }
}

/// What a person is told about their command: `done`, or why nothing
/// changed. A Store error stops the runtime.
fn told(result: Result<(), Error>, done: String) -> Result<Result<String, String>> {
    match result {
        Ok(()) => Ok(Ok(done)),
        Err(Error::Store(why)) => bail!("a command: {why}"),
        Err(Error::Refused(why) | Error::Invalid(why)) => {
            Ok(Err(format!("Nothing changed: {why}.")))
        }
        Err(other) => Ok(Err(format!("Nothing changed: {other}."))),
    }
}

/// The core lets any command stop a Ticket, so the shell decides who may:
/// an approver, or a person allowed to ask for work.
fn may_stop(policy: &AllowlistPolicy, source: &str, by: &Actor) -> bool {
    by.is_person()
        && (policy.may_approve(source, by)
            || policy.people.contains(&format!("{source}:{}", by.id)))
}

/// `assignment` is `ticket`'s latest and has not ended, so a worker may
/// still start for it.
fn live(ticket: &Ticket, assignment: &AssignmentId) -> bool {
    ticket
        .latest_assignment()
        .is_some_and(|a| a.id == *assignment && a.state != AssignmentState::Ended)
}

fn backoff(options: &RunOptions, attempts: u32) -> Duration {
    options
        .retry_base
        .saturating_mul(2u32.saturating_pow(attempts))
        .min(options.retry_max)
}

enum Job {
    Assess(Candidate),
    Launch(Box<Launch>),
    Deliver(Delivery),
    Stop {
        ticket: TicketId,
        assignment: AssignmentId,
        reason: String,
    },
    Send(OutboundMessage, SendOptions),
    /// Nothing to perform any more, and why.
    Skip(String),
}

impl Job {
    async fn perform(self, shared: Shared, facts: FactSink) -> Result<()> {
        match self {
            Job::Skip(why) => {
                eprintln!("agent: {why}");
                Ok(())
            }
            Job::Assess(candidate) => shared.router.assess(candidate, facts).await,
            Job::Launch(launch) => shared.executor.launch(*launch, facts).await,
            Job::Deliver(delivery) => shared.executor.deliver(delivery, facts).await,
            Job::Stop {
                ticket,
                assignment,
                reason,
            } => shared.executor.stop(ticket, assignment, reason).await,
            Job::Send(message, options) => {
                let source = message.channel.source.clone();
                // No channel will ever serve this source, so retrying cannot help.
                let Some(channel) = shared.channels.get(&source) else {
                    eprintln!("agent: no channel serves {source}; its message was dropped");
                    return Ok(());
                };
                if channel.send(message, options).await? == SendOutcome::Gone {
                    eprintln!("agent: a {source} conversation is gone; its message was dropped");
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Adapters, Job, RunOptions, Runtime, backoff};
    use crate::agent::scripted::{ScriptedExecutor, ScriptedRouter};
    use solos_agent_core::model::{
        Accept, Acknowledge, Actor, Addressed, Ask, ChannelRef, Effect, Plan, SecretId, ThreadKey,
        TicketId, Trigger, TriggerId, Verdict,
    };
    use solos_agent_core::{AllowlistPolicy, Repositories};
    use solos_agent_store::SqliteStore;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    fn runtime_with_a_ticket(budget_microusd: u64) -> (Runtime, TicketId) {
        let policy = AllowlistPolicy {
            people: ["slack:alice".to_owned()].into(),
            approvers: ["slack:alice".to_owned()].into(),
            auto_approve_microusd: 1_000_000,
            ..AllowlistPolicy::default()
        };
        let adapters = Adapters {
            sources: Vec::new(),
            channels: BTreeMap::new(),
            router: Arc::new(ScriptedRouter::new(budget_microusd)),
            executor: Arc::new(ScriptedExecutor),
        };
        let store = SqliteStore::open_in_memory().unwrap();
        let mut runtime = Runtime::new(store, policy, adapters, RunOptions::default());
        runtime
            .engine
            .receive(Trigger {
                id: TriggerId::new("t1"),
                source: "slack".into(),
                at_ms: 1_000,
                actor: Actor::person("alice"),
                addressed: Addressed::Mention,
                text: "triage my notifications".into(),
                threads: vec![ThreadKey::new("th1")],
                reply_to: vec![ChannelRef {
                    source: "slack".into(),
                    address: BTreeMap::from([("channel".to_owned(), "C1".to_owned())]),
                }],
                meta: serde_json::Value::Null,
            })
            .unwrap();
        let eval = runtime.engine.evaluate_due().unwrap().remove(0);
        let accept = Verdict::Accept(Accept {
            title: "Triage".into(),
            scope: "triage".into(),
            reason: "alice asked".into(),
            plan: Plan::Session {
                harness: None,
                environment: None,
            },
            budget_microusd,
            acknowledge: Acknowledge::Standard,
            rules: Vec::new(),
        });
        let decision = runtime.engine.decide(&eval, accept).unwrap();
        let ticket = runtime.engine.repos().decision(&decision).unwrap().ticket;
        (runtime, ticket.unwrap())
    }

    fn offered(runtime: &Runtime) -> Vec<Option<Ask>> {
        let sends = runtime.engine.repos().pending_effects().unwrap();
        sends
            .into_iter()
            .filter(|q| matches!(q.effect, Effect::Send(_)))
            .map(|q| match runtime.job(q.effect, false).unwrap() {
                Job::Send(_, options) => options.ask,
                _ => unreachable!("a Send is a Send"),
            })
            .collect()
    }

    #[test]
    fn a_secret_ask_has_buttons_only_while_its_grant_waits() {
        let (mut runtime, ticket) = runtime_with_a_ticket(500_000);
        let secret = SecretId::try_from("local/github-gui".to_owned()).unwrap();
        let ask = Some(Ask::Secret {
            secret: secret.clone(),
        });
        runtime
            .engine
            .ask_secret(&ticket, &secret, "sign in", &[])
            .unwrap();
        assert_eq!(
            offered(&runtime),
            [None, ask],
            "the acknowledgement, then the ask"
        );

        runtime
            .engine
            .answer_secret(&ticket, &secret, "slack", Actor::person("alice"), true)
            .unwrap();
        assert_eq!(
            offered(&runtime),
            [None, None],
            "the same Send, retried after the answer, posts a plain message"
        );
    }

    #[test]
    fn a_budget_ask_has_buttons_only_while_the_approval_waits() {
        let (mut runtime, ticket) = runtime_with_a_ticket(5_000_000);
        assert_eq!(offered(&runtime), [Some(Ask::Budget)]);

        runtime
            .engine
            .approve(&ticket, "slack", Actor::person("alice"))
            .unwrap();
        assert_eq!(offered(&runtime), [None]);

        let (mut runtime, ticket) = runtime_with_a_ticket(5_000_000);
        runtime.engine.stop_ticket(&ticket, "stopped").unwrap();
        assert_eq!(offered(&runtime), [None], "a stopped Ticket asks nothing");
    }

    #[test]
    fn backoff_doubles_up_to_its_cap() {
        let options = RunOptions {
            retry_base: Duration::from_millis(100),
            retry_max: Duration::from_millis(1_000),
            ..RunOptions::default()
        };
        let waits: Vec<u128> = (0..6).map(|n| backoff(&options, n).as_millis()).collect();
        assert_eq!(waits, [100, 200, 400, 800, 1_000, 1_000]);
        assert_eq!(backoff(&options, u32::MAX), options.retry_max);
    }
}
