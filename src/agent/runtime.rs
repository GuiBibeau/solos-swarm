//! The long-running loop. One task owns the Engine, so state has one writer.
//! Adapters run concurrently and send facts and Effect outcomes back to it.

use super::ports::{
    Channel, Delivered, Delivery, Executor, Fact, FactSink, Inbound, Launch, Router, SendOutcome,
    Source,
};
use crate::broker::now_ms;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use solos_agent_core::model::{
    AssignmentId, Candidate, Effect, InputBody, Ms, OutboundMessage, Stage, TicketId, Trigger,
};
use solos_agent_core::{AllowlistPolicy, Engine, Error, Intake, Policy, Repositories};
use solos_agent_store::{EffectId, QueuedEffect, SqliteStore};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::{self, JoinError, JoinSet};
use tokio::time::MissedTickBehavior;

pub type AgentEngine = Engine<SqliteStore, AllowlistPolicy, fn() -> Ms>;

const MAX_IN_FLIGHT: usize = 64;

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
}

impl Runtime {
    pub fn new(
        store: SqliteStore,
        policy: AllowlistPolicy,
        adapters: Adapters,
        options: RunOptions,
    ) -> Self {
        let clock: fn() -> Ms = now_ms;
        Self {
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
        }
    }

    /// Serve until `stop` resolves, or with `until_idle` until there is
    /// nothing left to do. Dropping the future stops everything at once, as
    /// a crash would; every Effect not marked done runs again on the next
    /// start.
    pub async fn run(mut self, stop: impl Future<Output = ()>) -> Result<Summary> {
        let (inbound_tx, mut inbound) = mpsc::channel(256);
        let facts = FactSink(inbound_tx);
        let (trigger_tx, mut triggers) = mpsc::channel::<Vec<Trigger>>(16);
        let mut sources = JoinSet::new();
        for source in std::mem::take(&mut self.sources) {
            sources.spawn(source.run(trigger_tx.clone()));
        }
        drop(trigger_tx);
        let mut sources_open = true;
        let mut work = JoinSet::new();
        let mut tick = tokio::time::interval(self.options.tick);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tokio::pin!(stop);
        loop {
            self.evaluate()?;
            self.dispatch(&mut work, &facts)?;
            let drained = !sources_open && sources.is_empty() && inbound.is_empty();
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

    fn apply(&mut self, fact: Fact) -> Result<()> {
        let (what, result) = match fact {
            Fact::Verdict { eval, verdict } => (
                format!("verdict on {eval}"),
                self.engine.decide(&eval, verdict).map(drop),
            ),
            Fact::Started { ticket, assignment } => (
                format!("start of {assignment} on {ticket}"),
                self.engine.assignment_started(&ticket, &assignment),
            ),
            Fact::Report {
                ticket,
                assignment,
                report,
            } => (
                format!("report on {assignment} of {ticket}"),
                self.engine.report(&ticket, &assignment, report),
            ),
        };
        settle(&what, result)
    }

    /// Hand due Effects to their adapters. A pause holds every Effect, per
    /// the core's rule; `evaluate_due` starts no Evals meanwhile.
    fn dispatch(&mut self, work: &mut JoinSet<()>, facts: &FactSink) -> Result<()> {
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
            let job = match self.job(effect) {
                Ok(job) => job,
                Err(err) => {
                    self.failed(id, attempts, &format!("{err:#}"))?;
                    continue;
                }
            };
            let (shared, facts) = (self.shared.clone(), facts.clone());
            let handle = work.spawn(async move {
                let result = job
                    .perform(shared, facts.clone())
                    .await
                    .map_err(|err| format!("{err:#}"));
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
    fn job(&self, effect: Effect) -> Result<Job> {
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
                Job::Launch(Launch {
                    ticket,
                    assignment,
                    title: found.title,
                    scope: found.mandate.scope,
                    plan,
                    budget_microusd,
                    recovery,
                })
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
                        let trigger = match &input.body {
                            InputBody::Trigger { trigger } => Some(
                                store
                                    .trigger(trigger)
                                    .with_context(|| format!("no trigger {trigger}"))?,
                            ),
                            _ => None,
                        };
                        Ok(Delivered { input, trigger })
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
            Effect::Send(message) => Job::Send(message),
        })
    }

    fn inbound(&mut self, message: Inbound) -> Result<()> {
        match message {
            Inbound::Fact(fact) => self.apply(fact),
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
                    Err(why) => self.failed(id, attempts, &why),
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
        self.failed(id, attempts, &format!("the adapter task failed: {err}"))
    }

    fn failed(&mut self, id: EffectId, attempts: u32, why: &str) -> Result<()> {
        let wait = backoff(&self.options, attempts);
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

fn backoff(options: &RunOptions, attempts: u32) -> Duration {
    options
        .retry_base
        .saturating_mul(2u32.saturating_pow(attempts))
        .min(options.retry_max)
}

enum Job {
    Assess(Candidate),
    Launch(Launch),
    Deliver(Delivery),
    Stop {
        ticket: TicketId,
        assignment: AssignmentId,
        reason: String,
    },
    Send(OutboundMessage),
}

impl Job {
    async fn perform(self, shared: Shared, facts: FactSink) -> Result<()> {
        match self {
            Job::Assess(candidate) => shared.router.assess(candidate, facts).await,
            Job::Launch(launch) => shared.executor.launch(launch, facts).await,
            Job::Deliver(delivery) => shared.executor.deliver(delivery, facts).await,
            Job::Stop {
                ticket,
                assignment,
                reason,
            } => shared.executor.stop(ticket, assignment, reason).await,
            Job::Send(message) => {
                let source = message.channel.source.clone();
                let channel = shared
                    .channels
                    .get(&source)
                    .with_context(|| format!("no channel serves source {source}"))?;
                if channel.send(message).await? == SendOutcome::Gone {
                    eprintln!("agent: a {source} conversation is gone; its message was dropped");
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RunOptions, backoff};
    use std::time::Duration;

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
