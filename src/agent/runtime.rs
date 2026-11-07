//! The long-running loop. One task owns the Engine, so state has one writer.
//! Adapters run concurrently and send facts and Effect outcomes back to it.

use super::earlier;
use super::ports::{
    Answer, Applied, BoxFuture, Channel, Command, Delivered, Delivery, Execute, Executor, Fact,
    FactSink, Failure, Forge, Inbound, Launch, MemoCommand, MemoDecision, MemoOrder, Order,
    Publish, Router, SendOptions, SendOutcome, Source,
};
use super::watchdog;
use crate::broker::now_ms;
use crate::gateway;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use solos_agent_core::change::{BranchName, CommitSha, RepoName};
use solos_agent_core::delivery::{ForgeJob, Observed};
use solos_agent_core::ids::key;
use solos_agent_core::memo::{
    MemoActor, MemoAudience, MemoAuthor, MemoIn, MemoReview, MemoScope, MemoSource,
};
use solos_agent_core::model::{
    Actor, AssignmentId, AssignmentState, Candidate, Effect, EvalId, Input, InputBody, MemoId,
    MessageId, MessageKind, Ms, OutboundMessage, Plan, Stage, Ticket, TicketId, Trigger,
};
use solos_agent_core::{AllowlistPolicy, Engine, Error, Intake, Policy, Repositories};
use solos_agent_store::{EffectId, QueuedEffect, SqliteStore};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::task::{self, JoinError, JoinSet};
use tokio::time::MissedTickBehavior;

pub type AgentEngine = Engine<SqliteStore, AllowlistPolicy, fn() -> Ms>;

const MAX_IN_FLIGHT: usize = 64;
/// How often proposals no review decided are looked at for expiry.
const EXPIRY_CHECK: Duration = Duration::from_secs(60);
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
    /// Makes the forge match the Store for `Effect::Forge`.
    forge: Option<Arc<dyn Forge>>,
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
    /// Bumped whenever a global memo changes, so a Router session started
    /// before it starts again with the new index.
    global_memos: Option<watch::Sender<u64>>,
    /// The repositories a memo may be about; `None` takes any.
    memo_repos: Option<BTreeSet<RepoName>>,
    /// When unreviewed proposals were last expired.
    memos_expired: Option<Instant>,
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
                forge: None,
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
            global_memos: None,
            memo_repos: None,
            memos_expired: None,
        }
    }

    /// Before `run` returns, run `closing` while the facts it sends are
    /// still applied, such as each open browser's signed Close.
    pub fn with_closing(mut self, closing: impl Future<Output = ()> + Send + 'static) -> Self {
        self.closing = Some(Box::pin(closing));
        self
    }

    /// The forge that performs `Effect::Forge`. Without one, such an Effect
    /// fails and is retried, so a misconfigured agent says so instead of
    /// dropping a pull request.
    pub fn with_forge(mut self, forge: Option<Arc<dyn Forge>>) -> Self {
        self.shared.forge = forge;
        self
    }

    /// Pause while `dir`'s pause gate is shut, as a broker does.
    pub fn with_control_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.control_dir = Some(dir.into());
        self
    }

    /// Bump `changes` whenever a global memo is added, retired or forgotten.
    pub fn with_global_memo_changes(mut self, changes: watch::Sender<u64>) -> Self {
        self.global_memos = Some(changes);
        self
    }

    /// Refuse a memo about any repository but `repos`, the ones a Ticket may
    /// name, so a typo is told instead of kept where no worker reads it.
    pub fn with_memo_repos(mut self, repos: BTreeSet<RepoName>) -> Self {
        self.memo_repos = Some(repos);
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
            self.expire_memos()?;
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
            Fact::Memo(command) => {
                return self.memo_command(command).map(|()| Ok(Applied::Done));
            }
            Fact::ShowMemos { to } => (
                format!("memo index for {to:?}"),
                self.engine.show_memos(to).map(Applied::Memos),
            ),
            Fact::ProposeMemo(proposal) => {
                let what = match &proposal.author {
                    MemoAuthor::Worker { assignment, .. } => {
                        format!("memo proposed by {assignment}")
                    }
                    MemoAuthor::Operator { source, by } => {
                        format!("memo proposed by {source}:{}", by.id)
                    }
                };
                (what, self.propose_memo(proposal))
            }
            Fact::ReviewMemo {
                memo,
                review,
                decision,
            } => (
                format!("review of {memo}"),
                self.review_memo(&memo, review, decision),
            ),
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
            Fact::Observed(observed) => (
                format!("observation of the forge on {}", observed_ticket(&observed)),
                self.engine.forge(observed).map(done),
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
        let outcome = match &order {
            Order::Approve => told(
                self.engine.approve(&ticket, &source, by.clone()),
                format!("Approved by {who}. The work starts now."),
            )?,
            Order::Stop | Order::StopBecause(_) => {
                let reason = match &order {
                    Order::StopBecause(why) => format!("stopped by {who} on {source}: {why}"),
                    _ => format!("stopped by {who} on {source}"),
                };
                told(
                    self.engine
                        .stop_ticket(&ticket, &source, by.clone(), &reason),
                    format!("Stopped by {who}."),
                )?
            }
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

    /// Apply an operator's order about a memo and answer in the thread it
    /// came from. What the operator writes is their own word, so the core
    /// makes it active at once.
    fn memo_command(&mut self, command: MemoCommand) -> Result<()> {
        let MemoCommand {
            id,
            order,
            source,
            by,
            reply_to,
        } = command;
        let scope_of = |runtime: &Self, memo| runtime.engine.repos().memo(memo).map(|m| m.scope);
        let (result, done, scope) = match &order {
            MemoOrder::Add { scope, text } => {
                let elsewhere = match (scope.repo(), &self.memo_repos) {
                    (Some(repo), Some(repos)) if !repos.contains(repo) => Some(repo),
                    _ => None,
                };
                let added = match elsewhere {
                    Some(repo) => Err(Error::Refused(format!(
                        "{repo} is not a repository this agent works on"
                    ))),
                    None => self.engine.propose_memo(MemoIn {
                        scope: scope.clone(),
                        text: text.clone(),
                        sources: vec![MemoSource::Operator],
                        author: MemoAuthor::Operator {
                            source: source.clone(),
                            by: by.clone(),
                        },
                    }),
                };
                let done = match &added {
                    Ok(memo) => format!("Remembered as {memo} ({scope})."),
                    Err(_) => String::new(),
                };
                (added.map(drop), done, Some(scope.clone()))
            }
            MemoOrder::Retire { memo, reason } => {
                let scope = scope_of(self, memo);
                let operator = MemoActor::Operator {
                    source: source.clone(),
                    by: by.clone(),
                };
                let retired = self.engine.retire_memo(memo, operator, reason);
                (retired, format!("Retired {memo}."), scope)
            }
            MemoOrder::Forget { memo } => {
                let scope = scope_of(self, memo);
                let forgot = self.engine.forget_memo(memo, &source, by.clone());
                (forgot, format!("Forgot {memo}: its text is erased."), scope)
            }
        };
        let changed = result.is_ok();
        let text = told(result, done)?.unwrap_or_else(|refused| {
            eprintln!("agent: {order:?} by {source}:{} refused: {refused}", by.id);
            refused
        });
        if changed && scope == Some(MemoScope::Global) {
            self.global_memos_changed();
        }
        self.replies.push(OutboundMessage {
            id: MessageId(key("rp", &[&id])),
            channel: reply_to,
            kind: MessageKind::Acknowledgement,
            text,
            ticket: None,
            ask: None,
        });
        Ok(())
    }

    /// The global index moved, so a Router session started before starts
    /// again with the new one.
    fn global_memos_changed(&self) {
        if let Some(changes) = &self.global_memos {
            changes.send_modify(|n| *n += 1);
        }
    }

    /// A worker's memo, as the core keeps it. One that is active at once
    /// (a person's words, quoted whole) and global moves the Router's index.
    fn propose_memo(&mut self, proposal: MemoIn) -> Result<Applied, Error> {
        let id = self.engine.propose_memo(proposal)?;
        let memo = self.engine.repos().memo(&id).ok_or(Error::Missing {
            kind: "memo",
            id: id.to_string(),
        })?;
        if memo.is_active() && memo.scope == MemoScope::Global {
            self.global_memos_changed();
        }
        Ok(Applied::Memo(Box::new(memo)))
    }

    /// Apply the review's decision through the Engine, which takes it only
    /// for a memo still proposed: keep it, keep it in other words (and in
    /// place of the memo it merges), or retire it.
    fn review_memo(
        &mut self,
        memo: &MemoId,
        review: MemoReview,
        decision: MemoDecision,
    ) -> Result<Applied, Error> {
        let scope = self.engine.repos().memo(memo).map(|m| m.scope);
        let kept = match decision {
            MemoDecision::Keep => self
                .engine
                .activate_memo(memo, review)
                .map(|()| Some(memo.clone())),
            MemoDecision::Rewrite { text } => {
                self.engine.revise_memo(memo, review, text, None).map(Some)
            }
            MemoDecision::Merge { into, text } => self
                .engine
                .revise_memo(memo, review, text, Some(&into))
                .map(Some),
            MemoDecision::Reject => {
                let by = MemoActor::Review {
                    by: review.by.clone(),
                };
                self.engine
                    .retire_memo(memo, by, &review.reason)
                    .map(|()| None)
            }
        }?;
        if kept.is_some() && scope == Some(MemoScope::Global) {
            self.global_memos_changed();
        }
        Ok(Applied::Reviewed(kept))
    }

    /// Retire the proposals no review decided within a week, at most once
    /// a minute. A Store error stops the runtime.
    fn expire_memos(&mut self) -> Result<()> {
        if self
            .memos_expired
            .is_some_and(|at| at.elapsed() < EXPIRY_CHECK)
        {
            return Ok(());
        }
        self.memos_expired = Some(Instant::now());
        match self.engine.expire_memos() {
            Ok(expired) => {
                for memo in expired {
                    eprintln!("agent: {memo} expired: no review decided it within a week");
                }
                Ok(())
            }
            Err(Error::Store(why)) => bail!("memo expiry: {why}"),
            Err(err) => {
                eprintln!("agent: memo expiry: {err}");
                Ok(())
            }
        }
    }

    /// A session's worker gets the memo index the core picks as its Launch
    /// is performed, and the core records which memos it was shown. A swarm's
    /// agents get none.
    fn with_memos(&mut self, job: Job) -> Result<Job> {
        let Job::Launch(mut launch) = job else {
            return Ok(job);
        };
        if matches!(launch.plan, Plan::Session { .. }) {
            let to = MemoAudience::Worker {
                ticket: launch.ticket.clone(),
                assignment: launch.assignment.clone(),
            };
            match self.engine.show_memos(to) {
                Ok(index) => launch.memos = (!index.text.is_empty()).then_some(index.text),
                Err(Error::Store(why)) => bail!("memos for {}: {why}", launch.assignment),
                Err(err) => eprintln!(
                    "agent: no memos for {} of {}: {err}",
                    launch.assignment, launch.ticket
                ),
            }
        }
        Ok(Job::Launch(launch))
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
                Ok(job) => self.with_memos(job)?,
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
                let earlier = earlier::of(&found.assignments);
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
                    repo: found.mandate.repo,
                    recovery: recovery || repeat,
                    inputs,
                    earlier,
                    memos: None,
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
            Effect::Forge(ForgeJob::Publish { ticket, head }) => {
                let found = store.ticket(&ticket).context("no such ticket")?;
                match self.publish_job(found, &head)? {
                    Ok(job) => Job::Forge(Box::new(job)),
                    Err(why) => Job::Skip(why),
                }
            }
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

    /// What publishing `head` needs, read from the Store now. `Err(why)` is an
    /// Effect with nothing left to do: its Ticket was stopped, or a newer head
    /// replaced it, or the forge already holds it, or the forge said no.
    fn publish_job(
        &self,
        ticket: Ticket,
        head: &CommitSha,
    ) -> Result<std::result::Result<PublishJob, String>> {
        let id = ticket.id.clone();
        let Some(change) = ticket.change.clone() else {
            return Ok(Err(format!("{id} has no change to publish")));
        };
        let skip = if ticket.stopped_by_person() {
            Some("was stopped")
        } else if change.head != *head {
            Some("has a newer head, which has its own publish")
        } else if change.delivered.as_ref().is_some_and(|d| d.head == *head) {
            Some("is already on the forge")
        } else if change.held.is_some() {
            Some("is held: the forge said no")
        } else {
            None
        };
        if let Some(why) = skip {
            return Ok(Err(format!(
                "{id}: publishing {} is skipped: the ticket {why}",
                head.short()
            )));
        }
        let repo = ticket
            .mandate
            .repo
            .clone()
            .context("a change names no repository")?;
        let origin = self
            .engine
            .repos()
            .trigger(&ticket.origin)
            .with_context(|| format!("no trigger {}", ticket.origin))?;
        Ok(Ok(PublishJob {
            ticket: id,
            repo,
            branch: change.branch,
            base: change.base,
            head: head.clone(),
            lease: change.delivered.map(|delivered| delivered.head),
            title: ticket.title,
            text: change.text,
            requester: ticket.mandate.requester,
            source: ticket.mandate.source,
            origin,
        }))
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
            let owed = store.ticket(&id).is_some_and(|t| {
                matches!(
                    t.stage(),
                    Stage::Queued | Stage::Working | Stage::Delivering
                )
            });
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
        Err(Error::Forbidden(why)) => Ok(Err(why)),
        Err(Error::Refused(why) | Error::Invalid(why)) => {
            Ok(Err(format!("Nothing changed: {why}.")))
        }
        Err(other) => Ok(Err(format!("Nothing changed: {other}."))),
    }
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

/// What `Execute::Publish` borrows, owned so an adapter task can hold it.
struct PublishJob {
    ticket: TicketId,
    repo: RepoName,
    branch: BranchName,
    base: String,
    head: CommitSha,
    lease: Option<CommitSha>,
    title: String,
    text: String,
    requester: Actor,
    source: String,
    origin: Trigger,
}

fn observed_ticket(observed: &Observed) -> &TicketId {
    match observed {
        Observed::Published { ticket, .. } | Observed::Withheld { ticket, .. } => ticket,
    }
}

enum Job {
    Assess(Candidate),
    Launch(Box<Launch>),
    Deliver(Delivery),
    /// Make the forge hold a head and a pull request for it.
    Forge(Box<PublishJob>),
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
            Job::Forge(job) => {
                let forge = shared
                    .forge
                    .as_ref()
                    .context("this agent has no forge to publish through")?;
                let observed = forge
                    .execute(Execute::Publish(Publish {
                        ticket: &job.ticket,
                        repo: &job.repo,
                        branch: &job.branch,
                        base: &job.base,
                        head: &job.head,
                        lease: job.lease.as_ref(),
                        title: &job.title,
                        text: &job.text,
                        requester: &job.requester,
                        source: &job.source,
                        origin: &job.origin,
                    }))
                    .await?;
                facts.send(Fact::Observed(observed)).await
            }
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
    use crate::agent::ports::{Applied, Fact, MemoCommand, MemoOrder};
    use crate::agent::scripted::{ScriptedExecutor, ScriptedRouter};
    use solos_agent_core::change::RepoName;
    use solos_agent_core::memo::{
        Memo, MemoAudience, MemoAuthor, MemoIn, MemoScope, MemoSource, MemoState, MemoTrust,
        PROPOSAL_TTL_MS,
    };
    use solos_agent_core::model::{
        Accept, Acknowledge, Actor, Addressed, Ask, AssignmentId, ChangeSet, ChannelRef,
        DomainEvent, Effect, MemoId, Plan, SecretId, ThreadKey, TicketId, Trigger, TriggerId,
        Verdict,
    };
    use solos_agent_core::trail::Digest;
    use solos_agent_core::{AllowlistPolicy, Repositories};
    use solos_agent_store::SqliteStore;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::watch;

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
                method: None,
            },
            budget_microusd: Some(budget_microusd),
            acknowledge: Acknowledge::Standard,
            rules: Vec::new(),
            repo: None,
            forge: None,
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
        runtime
            .engine
            .stop_ticket(&ticket, "slack", Actor::person("alice"), "stopped")
            .unwrap();
        assert_eq!(offered(&runtime), [None], "a stopped Ticket asks nothing");
    }

    fn repo(name: &str) -> MemoScope {
        MemoScope::Repo(RepoName::try_from(name.to_owned()).unwrap())
    }

    #[test]
    fn a_launch_carries_the_memos_the_core_picks_as_it_is_performed_and_records_them() {
        let (mut runtime, ticket) = runtime_with_a_ticket(500_000);
        // Written after the Launch was committed, before it is performed.
        let memo = runtime
            .engine
            .propose_memo(MemoIn {
                scope: MemoScope::Global,
                text: "Answer in French.".into(),
                sources: vec![MemoSource::Operator],
                author: MemoAuthor::Operator {
                    source: "slack".into(),
                    by: Actor::person("alice"),
                },
            })
            .unwrap();
        let queued = runtime.engine.repos().pending_effects().unwrap();
        let effect = queued
            .into_iter()
            .find(|q| matches!(q.effect, Effect::Launch { .. }))
            .unwrap()
            .effect;
        let job = runtime.job(effect, false).unwrap();
        let Job::Launch(launch) = runtime.with_memos(job).unwrap() else {
            panic!("a Launch stays a Launch")
        };
        let memos = launch.memos.clone().expect("an index");
        assert!(
            memos.contains(&format!("> [{memo} · person] Answer in French.")),
            "{memos}"
        );
        let shown: Vec<_> = runtime
            .engine
            .repos()
            .events()
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.event {
                DomainEvent::MemosShown { to, memos, .. } => Some((to, memos)),
                _ => None,
            })
            .collect();
        let to = MemoAudience::Worker {
            ticket,
            assignment: launch.assignment,
        };
        assert_eq!(shown, [(to, vec![memo])]);
    }

    #[test]
    fn an_operators_memo_order_is_answered_in_its_thread_and_a_global_one_tells_the_router() {
        let (runtime, _) = runtime_with_a_ticket(500_000);
        let (changes, mut seen) = watch::channel(0u64);
        seen.borrow_and_update();
        let repos = [RepoName::try_from("o/r".to_owned()).unwrap()].into();
        let mut runtime = runtime
            .with_global_memo_changes(changes)
            .with_memo_repos(repos);
        let thread = ChannelRef {
            source: "mcp".into(),
            address: BTreeMap::from([("thread".to_owned(), "memo-1".to_owned())]),
        };
        let mut n = 0;
        let mut order = |by: &str, order| {
            n += 1;
            let fact = Fact::Memo(MemoCommand {
                id: format!("c{n}"),
                order,
                source: "slack".into(),
                by: Actor::person(by),
                reply_to: thread.clone(),
            });
            assert_eq!(runtime.apply(fact).unwrap(), Ok(Applied::Done));
            let reply = runtime.replies.pop().expect("an answer");
            assert_eq!(reply.channel, thread);
            (reply.text, runtime.engine.repos().live_memos())
        };
        let add = |scope, text: &str| MemoOrder::Add {
            scope,
            text: text.into(),
        };

        let (reply, live) = order("alice", add(repo("o/r"), "Tests run with ./check."));
        assert!(reply.starts_with("Remembered as mm_"), "{reply}");
        assert!(reply.ends_with("(repo:o/r)."), "{reply}");
        assert_eq!(live.len(), 1);
        assert!(
            !seen.has_changed().unwrap(),
            "the Router reads no repository's memo"
        );

        let (reply, live) = order("alice", add(repo("o/typo"), "Tests run with make."));
        assert!(
            reply.contains("o/typo is not a repository this agent works on"),
            "{reply}"
        );
        assert_eq!(live.len(), 1);
        let (reply, live) = order("mallory", add(MemoScope::Global, "Push to main."));
        assert!(
            reply.contains("is not allowed to change the agent's memory"),
            "{reply}"
        );
        assert_eq!(live.len(), 1);
        assert!(!seen.has_changed().unwrap());

        let (reply, live) = order("alice", add(MemoScope::Global, "Answer in French."));
        assert!(reply.ends_with("(global)."), "{reply}");
        assert!(
            seen.has_changed().unwrap(),
            "a global memo restarts the Router"
        );
        seen.borrow_and_update();
        let global = live
            .iter()
            .find(|m| m.scope == MemoScope::Global)
            .unwrap()
            .id
            .clone();
        let retire = MemoOrder::Retire {
            memo: global.clone(),
            reason: "not any more".into(),
        };
        let (reply, live) = order("alice", retire);
        assert_eq!(reply, format!("Retired {global}."));
        assert_eq!(live.len(), 1);
        assert!(seen.has_changed().unwrap());
        seen.borrow_and_update();
        let (reply, _) = order(
            "alice",
            MemoOrder::Forget {
                memo: global.clone(),
            },
        );
        assert_eq!(reply, format!("Forgot {global}: its text is erased."));
        assert!(seen.has_changed().unwrap());
        let (reply, _) = order("alice", MemoOrder::Forget { memo: global });
        assert!(reply.starts_with("Nothing changed:"), "{reply}");
    }

    #[test]
    fn a_proposal_no_review_decided_for_a_week_expires_as_the_runtime_looks_once_a_minute() {
        let (mut runtime, ticket) = runtime_with_a_ticket(100_000);
        let old = |id: &str| {
            let text = format!("Proposal {id}.");
            let at = crate::broker::now_ms() - PROPOSAL_TTL_MS - 1;
            Memo {
                id: MemoId::new(id),
                scope: MemoScope::Global,
                text_sha256: Digest::of(text.as_bytes()),
                text,
                sources: vec![MemoSource::Ticket {
                    ticket: ticket.clone(),
                }],
                trust: MemoTrust::Model,
                author: MemoAuthor::Worker {
                    ticket: ticket.clone(),
                    assignment: AssignmentId::new("as_gone"),
                },
                state: MemoState::Proposed,
                created_ms: at,
                verified_ms: at,
            }
        };
        let save = |runtime: &mut Runtime, memo: Memo| {
            let memos = BTreeMap::from([(memo.id.clone(), memo)]);
            let changes = ChangeSet {
                memos,
                ..ChangeSet::default()
            };
            runtime.engine.repos_mut().commit(changes).unwrap();
        };
        let state = |runtime: &Runtime, id: &str| {
            let memo = runtime.engine.repos().memo(&MemoId::new(id)).unwrap();
            memo.state.name()
        };
        save(&mut runtime, old("mm_0000000000000001"));

        runtime.expire_memos().unwrap();
        assert_eq!(state(&runtime, "mm_0000000000000001"), "retired");

        save(&mut runtime, old("mm_0000000000000002"));
        runtime.expire_memos().unwrap();
        assert_eq!(
            state(&runtime, "mm_0000000000000002"),
            "proposed",
            "looked for once a minute"
        );
        runtime.memos_expired = Some(Instant::now() - super::EXPIRY_CHECK);
        runtime.expire_memos().unwrap();
        assert_eq!(state(&runtime, "mm_0000000000000002"), "retired");
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
