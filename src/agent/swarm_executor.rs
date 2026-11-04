//! The swarm arm of the executor: each Assignment of a `Plan::Swarm` Ticket
//! runs one governed swarm through `swarm::run_goal_until`, with a Goal
//! rendered from a template under what the Ticket has left to spend. A run
//! has its own directory under the Ticket's, which stays as evidence, and its
//! cost is charged on the Ticket's account once it ends. At most `max_runs`
//! swarms run at once.

use super::broker::{AgentBroker, clip};
use super::goal_template::{Fill, GoalTemplates};
use super::pi_executor::{dir_name, private_dir};
use super::ports::{BoxFuture, Delivery, Executor, Fact, FactSink, Launch};
use crate::gateway::{self, Provider};
use crate::harness::AgentHarness;
use crate::swarm::{self, Cancel, GoalOutcome, GoalRun, GoalStatus};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use solos_agent_core::model::{AssignmentId, MicroUsd, Ms, Plan, ReportIn, ReportStatus, TicketId};
use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

/// Each Ticket's swarm runs, inside its directory.
pub const SWARM_DIR: &str = "swarm";
const REASON_CHARS: usize = 400;
const NAMED_FINDINGS: usize = 10;
const PAUSE_TICK: Duration = Duration::from_secs(1);
const MOST_PROGRESS_EVERY: Duration = Duration::from_secs(300);
/// How many directories one Assignment's repeated launches may take.
const RUN_DIRS: usize = 100;

/// Where swarms run and what they run on.
pub struct SwarmParts<H> {
    pub harness: Arc<H>,
    /// The workers' provider, behind each swarm's own broker.
    pub provider: Arc<dyn Provider>,
    /// The Tickets' ledger.
    pub broker: AgentBroker,
    pub templates: GoalTemplates,
    pub tickets_dir: PathBuf,
    /// Its pause gate holds every running swarm too.
    pub state_dir: PathBuf,
    pub node: PathBuf,
    pub pi_root: PathBuf,
    pub max_runs: usize,
    /// How often a running swarm sends a quiet `progress` Report.
    pub progress_every: Duration,
}

/// Often enough that the Watchdog, which takes an Assignment with no
/// Report for `stalled_ms` for stalled, never takes a running swarm for one.
pub fn progress_every(stalled_ms: Option<Ms>) -> Duration {
    stalled_ms.map_or(MOST_PROGRESS_EVERY, |ms| {
        Duration::from_millis(ms / 2).clamp(Duration::from_secs(1), MOST_PROGRESS_EVERY)
    })
}

pub struct SwarmExecutor<H> {
    inner: Arc<Inner<H>>,
}

struct Inner<H> {
    parts: SwarmParts<H>,
    slots: Arc<Semaphore>,
    runs: Mutex<BTreeMap<AssignmentId, Run>>,
    /// Run directories whose swarm has not been charged yet.
    unbooked: Mutex<BTreeSet<PathBuf>>,
}

struct Run {
    /// Set once, with why the swarm is cancelled.
    cancel: watch::Sender<Option<String>>,
    task: JoinHandle<()>,
}

impl<H> SwarmExecutor<H>
where
    H: AgentHarness + Send + Sync + 'static,
    H::Session: Send + 'static,
{
    pub fn new(parts: SwarmParts<H>) -> Result<Self> {
        ensure!(parts.max_runs > 0, "a swarm executor needs a run slot");
        private_dir(&parts.tickets_dir)?;
        Ok(Self {
            inner: Arc::new(Inner {
                slots: Arc::new(Semaphore::new(parts.max_runs)),
                parts,
                runs: Mutex::new(BTreeMap::new()),
                unbooked: Mutex::new(BTreeSet::new()),
            }),
        })
    }

    /// Whether `assignment` is a swarm this executor runs or will run.
    pub fn has(&self, assignment: &AssignmentId) -> bool {
        self.inner.runs().contains_key(assignment)
    }

    /// Cancel every swarm, as the process ends, and wait until each has
    /// written its evidence. The Engine is not told: the next start
    /// recovers their Assignments in new runs.
    pub async fn shutdown(&self) {
        let runs = std::mem::take(&mut *self.inner.runs());
        for run in runs.values() {
            let _ = run.cancel.send(Some("the agent is shutting down".into()));
        }
        for run in runs.into_values() {
            let _ = run.task.await;
        }
    }
}

impl<H> Executor for SwarmExecutor<H>
where
    H: AgentHarness + Send + Sync + 'static,
    H::Session: Send + 'static,
{
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        self.inner.start(launch, facts);
        Box::pin(async { Ok(()) })
    }

    fn deliver(&self, delivery: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        eprintln!(
            "agent: the swarm of {} on {} takes no new input; it waits for the next assignment",
            delivery.assignment, delivery.ticket
        );
        Box::pin(async { Ok(()) })
    }

    /// Cancel the swarm and wait until it has stopped every session and
    /// written its evidence.
    fn stop(
        &self,
        _: TicketId,
        assignment: AssignmentId,
        reason: String,
    ) -> BoxFuture<'_, Result<()>> {
        let run = self.inner.runs().remove(&assignment);
        Box::pin(async move {
            if let Some(run) = run {
                let _ = run
                    .cancel
                    .send(Some(format!("its assignment ended: {reason}")));
                let _ = run.task.await;
            }
            Ok(())
        })
    }
}

impl<H> Inner<H>
where
    H: AgentHarness + Send + Sync + 'static,
    H::Session: Send + 'static,
{
    fn runs(&self) -> MutexGuard<'_, BTreeMap<AssignmentId, Run>> {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn unbooked(&self) -> MutexGuard<'_, BTreeSet<PathBuf>> {
        self.unbooked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Start a swarm for `launch`, once per Assignment. The table stays
    /// locked until it holds the run, so a run that ends at once still
    /// finds itself there to remove.
    fn start(self: &Arc<Self>, launch: Launch, facts: FactSink) {
        let mut runs = self.runs();
        if runs.contains_key(&launch.assignment) {
            return;
        }
        let id = launch.assignment.clone();
        let (cancel, cancelled) = watch::channel(None);
        let task = tokio::spawn(self.clone().work(launch, facts, cancelled));
        runs.insert(id, Run { cancel, task });
    }

    async fn work(
        self: Arc<Self>,
        launch: Launch,
        facts: FactSink,
        mut cancel: watch::Receiver<Option<String>>,
    ) {
        let slot = tokio::select! {
            slot = self.slots.clone().acquire_owned() => slot.ok(),
            _ = cancel.wait_for(Option::is_some) => None,
        };
        if let Some(_slot) = slot
            && let Err(err) = self.run(&launch, &facts, cancel).await
        {
            eprintln!(
                "agent: the swarm of {} on {} was not told: {err:#}",
                launch.assignment, launch.ticket
            );
        }
        self.runs().remove(&launch.assignment);
    }

    /// One swarm, from its Goal to its Report. `Err` is a fact the runtime
    /// did not take.
    async fn run(
        &self,
        launch: &Launch,
        facts: &FactSink,
        cancel: watch::Receiver<Option<String>>,
    ) -> Result<()> {
        let prepared = self.prepare(launch).await;
        facts
            .send(Fact::Started {
                ticket: launch.ticket.clone(),
                assignment: launch.assignment.clone(),
            })
            .await?;
        let run = match prepared {
            Ok(run) => run,
            Err(err) => {
                let (why, _) = clip(&format!("{err:#}"), REASON_CHARS);
                let text = format!("The swarm did not start: {why}");
                report(facts, launch, ReportStatus::Failed, text, 0).await?;
                return ended(facts, launch, format!("swarm failed: {why}")).await;
            }
        };
        let run_dir = run.run_dir.clone();
        self.unbooked().insert(run_dir.clone());
        let outcome = self.swarm(run, cancel.clone(), launch, facts).await;
        let cost = match &outcome {
            Ok(outcome) => outcome.spent_microusd.saturating_add(outcome.held_microusd),
            Err(_) => run_cost(&run_dir).unwrap_or(0),
        };
        // Before any Report, so the next Assignment's budget counts it.
        self.book(&launch.ticket, &run_dir, cost).await;
        if cancel.borrow().is_some() {
            // The Engine ended the Assignment already, or the process is ending.
            return Ok(());
        }
        match outcome {
            Ok(outcome) => tell(facts, launch, &outcome).await,
            Err(err) => {
                let (why, _) = clip(&format!("{err:#}"), REASON_CHARS);
                let text = format!(
                    "The swarm could not run: {why}. It spent {cost} microusd; its run is in {}.",
                    run_dir.display()
                );
                report(facts, launch, ReportStatus::Failed, text, cost).await?;
                ended(facts, launch, format!("swarm failed: {why}")).await
            }
        }
    }

    /// Charge the Ticket's account what the run in `run_dir` cost. One that
    /// fails is told here, and the Ticket's next launch charges it.
    async fn book(&self, ticket: &TicketId, run_dir: &Path, cost: MicroUsd) {
        if let Err(err) = self.charge(ticket, run_dir, cost).await {
            eprintln!(
                "agent: the {cost} microusd the swarm in {} spent are not charged yet: {err:#}",
                run_dir.display()
            );
        }
        self.unbooked().remove(run_dir);
    }

    /// Charge a run once, under its directory's name.
    async fn charge(&self, ticket: &TicketId, run_dir: &Path, cost: MicroUsd) -> Result<bool> {
        let name = run_dir
            .file_name()
            .and_then(|name| name.to_str())
            .context("a run directory has no name")?;
        let provider = self.parts.templates.provider();
        self.parts
            .broker
            .charge_ticket(ticket, provider, cost, format!("swarm:{name}"))
            .await
    }

    /// Charge what earlier runs of `ticket` cost and were not charged, such
    /// as a run a crash cut short.
    async fn book_earlier(&self, ticket: &TicketId) -> Result<()> {
        let base = self.swarm_dir(ticket);
        let entries = match std::fs::read_dir(&base) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err).with_context(|| format!("cannot list {}", base.display())),
        };
        for entry in entries {
            let dir = entry?.path();
            if self.unbooked().contains(&dir) {
                continue;
            }
            if let Some(cost) = run_cost(&dir)
                && self.charge(ticket, &dir, cost).await?
            {
                eprintln!(
                    "agent: charged {ticket} {cost} microusd for the swarm in {}, which was cut short",
                    dir.display()
                );
            }
        }
        Ok(())
    }

    fn swarm_dir(&self, ticket: &TicketId) -> PathBuf {
        self.parts.tickets_dir.join(ticket.as_str()).join(SWARM_DIR)
    }

    /// The Goal for what the Ticket has left to spend, and a new run
    /// directory holding it as `config.yaml`.
    async fn prepare(&self, launch: &Launch) -> Result<GoalRun> {
        let Plan::Swarm {
            agents,
            goal_template,
        } = &launch.plan
        else {
            bail!("{} is not a swarm plan", launch.assignment);
        };
        let parts = &self.parts;
        dir_name(launch.ticket.as_str())?;
        self.book_earlier(&launch.ticket).await?;
        let committed = parts.broker.ticket_committed(&launch.ticket).await?;
        let remaining = launch.budget_microusd.saturating_sub(committed);
        ensure!(
            remaining > 0,
            "the ticket's budget of {} microusd is spent",
            launch.budget_microusd
        );
        let fill = Fill {
            ticket: launch.ticket.as_str(),
            title: &launch.title,
            scope: &launch.scope,
            agents: *agents,
            budget_microusd: remaining,
        };
        let (goal, text) = parts.templates.goal(goal_template.as_deref(), &fill)?;
        let run_dir = self.run_dir(&launch.ticket, &launch.assignment)?;
        let config = run_dir.join("config.yaml");
        std::fs::write(&config, text)
            .with_context(|| format!("cannot write {}", config.display()))?;
        Ok(GoalRun {
            deadline: goal.deadline_seconds,
            goal,
            config,
            run_dir,
            idle_probe: false,
            pi_root: parts.pi_root.clone(),
            node: parts.node.clone(),
        })
    }

    /// A directory no run has used: `<ticket>/swarm/<assignment>`, or with
    /// `-2`, `-3`, ... after it when a launch repeats.
    fn run_dir(&self, ticket: &TicketId, assignment: &AssignmentId) -> Result<PathBuf> {
        dir_name(ticket.as_str())?;
        let base = self.swarm_dir(ticket);
        private_dir(&base)?;
        let name = dir_name(assignment.as_str())?;
        for n in 1..=RUN_DIRS {
            let dir = if n == 1 {
                base.join(name)
            } else {
                base.join(format!("{name}-{n}"))
            };
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(dir),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => {
                    return Err(err).with_context(|| format!("cannot create {}", dir.display()));
                }
            }
        }
        bail!("{assignment} has used {RUN_DIRS} run directories");
    }

    /// The swarm itself, held while the agent is paused and reporting its
    /// progress, until it ends or `cancel` is set.
    async fn swarm(
        &self,
        run: GoalRun,
        mut cancel: watch::Receiver<Option<String>>,
        launch: &Launch,
        facts: &FactSink,
    ) -> Result<GoalOutcome> {
        let request = async move {
            match cancel.wait_for(Option::is_some).await {
                Ok(reason) => (*reason).clone().unwrap_or_default(),
                Err(_) => "the agent stopped".to_owned(),
            }
        };
        let paused = mirror_pause(self.parts.state_dir.clone(), run.run_dir.clone());
        let alive = heartbeat(
            facts.clone(),
            launch.clone(),
            run.run_dir.clone(),
            self.parts.progress_every,
        );
        tokio::select! {
            outcome = swarm::run_goal_until(
                run,
                &*self.parts.harness,
                self.parts.provider.clone(),
                Cancel::on(request),
            ) => outcome,
            never = paused => match never {},
            never = alive => match never {},
        }
    }
}

/// What a run's `state.json` says it committed: its spend, and the holds
/// still open, at their hold, as the ledger charges a call a crash cut
/// short. `None` before its first event, when it has spent nothing.
fn run_cost(run_dir: &Path) -> Option<MicroUsd> {
    let state = run_state(run_dir)?;
    state["spent_microusd"]
        .as_u64()?
        .checked_add(state["held_microusd"].as_u64()?)
}

fn run_state(run_dir: &Path) -> Option<Value> {
    let bytes = std::fs::read(run_dir.join("state.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A quiet `progress` Report every `every`, with what the run has filed and
/// spent so far. A Report the runtime does not take changes nothing.
async fn heartbeat(
    facts: FactSink,
    launch: Launch,
    run_dir: PathBuf,
    every: Duration,
) -> Infallible {
    loop {
        tokio::time::sleep(every).await;
        let filed = run_state(&run_dir)
            .and_then(|state| state["findings"].as_array().map(Vec::len))
            .unwrap_or(0);
        let spent = run_cost(&run_dir).unwrap_or(0);
        let text = format!(
            "The swarm is running; it has filed {filed} of its findings and spent {spent} microusd."
        );
        let _ = report(&facts, &launch, ReportStatus::Progress, text, spent).await;
    }
}

/// The outcome as the Ticket's Report. A swarm that did not complete also
/// ends its Assignment, so the next message starts a new run.
async fn tell(facts: &FactSink, launch: &Launch, outcome: &GoalOutcome) -> Result<()> {
    let spent = outcome.spent_microusd.saturating_add(outcome.held_microusd);
    let filed = filed(outcome);
    match &outcome.status {
        GoalStatus::Completed => {
            let artifact = outcome.artifact.as_deref().unwrap_or(&outcome.run_dir);
            let text = format!(
                "The swarm {filed} and spent {spent} microusd. Its artifact is {}.",
                artifact.display()
            );
            report(facts, launch, ReportStatus::Ready, text, spent).await
        }
        GoalStatus::Stopped(why) => {
            let (why, _) = clip(&format!("{why:#}"), REASON_CHARS);
            let text = format!(
                "The swarm stopped: {why}. It {filed} and spent {spent} microusd; its run is in {}.",
                outcome.run_dir.display()
            );
            report(facts, launch, ReportStatus::Failed, text, spent).await?;
            ended(facts, launch, format!("swarm stopped: {why}")).await
        }
        GoalStatus::Drained => {
            let text = format!(
                "The swarm was drained before it finished. It {filed} and spent {spent} microusd; \
                 a new message starts it again."
            );
            report(facts, launch, ReportStatus::Waiting, text, spent).await?;
            ended(facts, launch, "swarm drained".into()).await
        }
    }
}

/// "filed 2 findings (part-0, part-1)", from the run's `state.json`.
fn filed(outcome: &GoalOutcome) -> String {
    let count = match outcome.findings {
        1 => "filed 1 finding".to_owned(),
        n => format!("filed {n} findings"),
    };
    let tasks: Vec<String> = run_state(&outcome.run_dir)
        .and_then(|state| {
            let findings = state["findings"].as_array()?;
            Some(
                findings
                    .iter()
                    .filter_map(|finding| finding["task_id"].as_str())
                    .map(|task| clip(task, 60).0)
                    .collect(),
            )
        })
        .unwrap_or_default();
    if tasks.is_empty() {
        return count;
    }
    let shown = tasks.len().min(NAMED_FINDINGS);
    let mut names = tasks[..shown].join(", ");
    if tasks.len() > shown {
        names.push_str(&format!(" and {} more", tasks.len() - shown));
    }
    format!("{count} ({names})")
}

async fn report(
    facts: &FactSink,
    launch: &Launch,
    status: ReportStatus,
    text: String,
    spent_microusd: MicroUsd,
) -> Result<()> {
    facts
        .send(Fact::Report {
            ticket: launch.ticket.clone(),
            assignment: launch.assignment.clone(),
            report: ReportIn {
                status,
                text,
                question: None,
                spent_microusd,
            },
        })
        .await
}

async fn ended(facts: &FactSink, launch: &Launch, reason: String) -> Result<()> {
    facts
        .send(Fact::Ended {
            ticket: launch.ticket.clone(),
            assignment: launch.assignment.clone(),
            reason,
        })
        .await
}

/// A `PAUSE` this run wrote in its directory, removed when the run ends.
struct PauseFlag {
    path: PathBuf,
    ours: bool,
}

impl Drop for PauseFlag {
    fn drop(&mut self) {
        if self.ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// While the agent is paused, a `PAUSE` file in the run's directory, which
/// the swarm's broker and deadline read. An operator's own `PAUSE` there is
/// left alone.
async fn mirror_pause(state_dir: PathBuf, run_dir: PathBuf) -> Infallible {
    let mut flag = PauseFlag {
        path: run_dir.join("PAUSE"),
        ours: false,
    };
    loop {
        let paused = gateway::effective_control(&state_dir, gateway::unix_secs()).is_some();
        if paused && !flag.ours && !flag.path.exists() {
            flag.ours = std::fs::write(&flag.path, b"the agent is paused\n").is_ok();
        } else if !paused && flag.ours && std::fs::remove_file(&flag.path).is_ok() {
            flag.ours = false;
        }
        tokio::time::sleep(PAUSE_TICK).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{SWARM_DIR, SwarmExecutor, SwarmParts, mirror_pause, progress_every};
    use crate::agent::broker::{AgentBroker, BrokerParts, DecideTerms};
    use crate::agent::config::SwarmConfig;
    use crate::agent::goal_template::GoalTemplates;
    use crate::agent::ports::{Executor, Fact, FactSink, Inbound, Launch};
    use crate::gateway::{Dispatch, Provider};
    use crate::harness::scripted::{Call, ScriptedHarness};
    use crate::harness::{AgentHarness, Observation, Session, SessionSpec};
    use serde_json::{Value, json};
    use solos_agent_core::model::{AssignmentId, Plan, ReportIn, ReportStatus, TicketId};
    use solos_agent_store::SqliteStore;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::mpsc;

    const TEMPLATE: &str = include_str!("../../tests/fixtures/goal-template.yaml");

    /// `smoke::Scripted`: every call answers with text and costs 90 microusd.
    struct Scripted;
    impl Provider for Scripted {
        fn dispatch(&self, _: Value) -> Dispatch {
            Box::pin(async {
                Ok(
                    json!({"id":"swarm-receipt","created":1,"model":"deepseek-flash",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":0,"completion_tokens":50}}),
                )
            })
        }
    }

    /// Sessions that take prompts and never end a turn, so a swarm on them
    /// runs until it is cancelled, with no model call in flight.
    #[derive(Clone, Default)]
    struct Idle {
        events: Arc<Mutex<Vec<mpsc::Sender<Observation>>>>,
        live: Arc<AtomicUsize>,
        most: Arc<AtomicUsize>,
    }

    struct IdleSession {
        id: String,
        live: Arc<AtomicUsize>,
        stopped: bool,
    }

    impl AgentHarness for Idle {
        type Session = IdleSession;
        async fn start(
            &self,
            spec: &SessionSpec,
            events: mpsc::Sender<Observation>,
        ) -> anyhow::Result<IdleSession> {
            self.events.lock().unwrap().push(events);
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(live, Ordering::SeqCst);
            Ok(IdleSession {
                id: spec.id.clone(),
                live: self.live.clone(),
                stopped: false,
            })
        }
    }

    impl Session for IdleSession {
        fn id(&self) -> &str {
            &self.id
        }
        async fn prompt(&mut self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<bool> {
            if !std::mem::replace(&mut self.stopped, true) {
                self.live.fetch_sub(1, Ordering::SeqCst);
            }
            Ok(true)
        }
        async fn join(self) -> usize {
            0
        }
    }

    struct World<H> {
        executor: SwarmExecutor<H>,
        broker: AgentBroker,
        facts: FactSink,
        inbound: mpsc::Receiver<Inbound>,
        dir: PathBuf,
    }

    fn dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("swarm-executor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("pi/extensions")).unwrap();
        std::fs::write(dir.join("pi/extensions/broker.ts"), "// stand-in").unwrap();
        std::fs::write(dir.join("pi/extensions/agent-tools.json"), "{}").unwrap();
        std::fs::write(dir.join("pi/package-lock.json"), "{}").unwrap();
        std::fs::write(dir.join("node"), "").unwrap();
        std::fs::create_dir_all(dir.join("state")).unwrap();
        dir
    }

    impl<H> World<H>
    where
        H: AgentHarness + Send + Sync + 'static,
        H::Session: Send + 'static,
    {
        fn new(harness: H, max_runs: usize, template: &str) -> Self {
            Self::with_progress(harness, max_runs, template, Duration::from_secs(3_600))
        }
        fn with_progress(harness: H, max_runs: usize, template: &str, every: Duration) -> Self {
            let dir = dir();
            std::fs::write(dir.join("goal.yaml"), template).unwrap();
            let config = SwarmConfig {
                templates: BTreeMap::from([("research".to_owned(), dir.join("goal.yaml"))]),
                default_template: Some("research".into()),
                max_runs,
            };
            let (facts, inbound) = FactSink::channel();
            let broker = AgentBroker::new(BrokerParts {
                dir: dir.join("state"),
                store: SqliteStore::open_in_memory().unwrap(),
                provider: Arc::new(Scripted),
                facts: facts.clone(),
                decide: DecideTerms {
                    default_budget_microusd: 1,
                    max_budget_microusd: None,
                    swarm: None,
                    repos: BTreeMap::new(),
                },
                workers: None,
            })
            .unwrap();
            let executor = SwarmExecutor::new(SwarmParts {
                harness: Arc::new(harness),
                provider: Arc::new(Scripted),
                broker: broker.clone(),
                templates: GoalTemplates::load(&config, "deepseek").unwrap(),
                tickets_dir: dir.join("state/tickets"),
                state_dir: dir.join("state"),
                node: dir.join("node"),
                pi_root: dir.join("pi"),
                max_runs,
                progress_every: every,
            })
            .unwrap();
            Self {
                executor,
                broker,
                facts,
                inbound,
                dir,
            }
        }
        async fn launch(&self, launch: Launch) {
            self.executor
                .launch(launch, self.facts.clone())
                .await
                .unwrap();
        }
        async fn stop(&self, ticket: &str, assignment: &str) {
            self.executor
                .stop(
                    TicketId::new(ticket),
                    AssignmentId::new(assignment),
                    "stopped by alice on slack".into(),
                )
                .await
                .unwrap();
        }
        async fn fact(&mut self) -> Fact {
            let next = tokio::time::timeout(Duration::from_secs(30), self.inbound.recv());
            match next.await {
                Ok(Some(Inbound::Fact(fact))) => fact,
                Ok(_) => panic!("not a fact"),
                Err(_) => panic!("no fact came"),
            }
        }
        async fn no_fact(&mut self) {
            let next = tokio::time::timeout(Duration::from_millis(300), self.inbound.recv());
            if let Ok(Some(Inbound::Fact(fact))) = next.await {
                panic!("an unexpected fact: {fact:?}");
            }
        }
        async fn report(&mut self) -> ReportIn {
            match self.fact().await {
                Fact::Report { report, .. } => report,
                other => panic!("not a report: {other:?}"),
            }
        }
        async fn committed(&self, ticket: &str) -> u64 {
            self.broker
                .ticket_committed(&TicketId::new(ticket))
                .await
                .unwrap()
        }
        /// Whether `run` of `ticket` was charged already: a charge again
        /// would then charge nothing.
        async fn charged(&self, ticket: &str, run: &str) -> bool {
            let again = self
                .broker
                .charge_ticket(
                    &TicketId::new(ticket),
                    "deepseek",
                    0,
                    format!("swarm:{run}"),
                )
                .await
                .unwrap();
            !again
        }
        fn run_dir(&self, ticket: &str, assignment: &str) -> PathBuf {
            self.dir
                .join("state/tickets")
                .join(ticket)
                .join(SWARM_DIR)
                .join(assignment)
        }
    }

    impl<H> Drop for World<H> {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn launch(ticket: &str, assignment: &str, agents: u16) -> Launch {
        Launch {
            ticket: TicketId::new(ticket),
            assignment: AssignmentId::new(assignment),
            title: "Map who cites the paper".into(),
            scope: "Find every paper that cites X.".into(),
            origin: serde_json::from_value(json!({
                "id": "t1", "source": "slack", "at_ms": 1_000,
                "actor": {"id": "alice", "kind": "person"}, "addressed": "mention",
                "text": "Who cites X?", "threads": ["T1"]
            }))
            .unwrap(),
            rules: Vec::new(),
            plan: Plan::Swarm {
                agents,
                goal_template: None,
            },
            budget_microusd: 500_000,
            repo: None,
            recovery: false,
            inputs: Vec::new(),
        }
    }

    fn started(fact: &Fact, assignment: &str) -> bool {
        matches!(fact, Fact::Started { assignment: a, .. } if a.as_str() == assignment)
    }

    fn read(dir: &Path, name: &str) -> Value {
        serde_json::from_slice(&std::fs::read(dir.join(name)).unwrap()).unwrap()
    }

    fn filing() -> ScriptedHarness {
        let record =
            |id: &str| json!({"papers":[{"id":format!("{id}-paper"),"severity":"useful"}]});
        let scripts = BTreeMap::from([
            (
                "agent-00".to_owned(),
                vec![
                    vec![Call::ClaimTask, Call::SubmitFinding(record("agent-00"))],
                    vec![Call::ReadFindings, Call::SubmitArtifact],
                ],
            ),
            (
                "agent-01".to_owned(),
                vec![vec![
                    Call::ClaimTask,
                    Call::SubmitFinding(record("agent-01")),
                ]],
            ),
        ]);
        ScriptedHarness::new(scripts)
    }

    #[tokio::test]
    async fn a_swarm_runs_its_tickets_goal_and_reports_ready_with_its_artifact() {
        let mut w = World::new(filing(), 1, TEMPLATE);
        w.launch(launch("tk_1", "as_1", 2)).await;
        w.launch(launch("tk_1", "as_1", 2)).await;
        assert!(started(&w.fact().await, "as_1"));
        let report = w.report().await;
        assert_eq!(report.status, ReportStatus::Ready, "{}", report.text);
        let run = w.run_dir("tk_1", "as_1");
        let artifact = run.join("artifacts/final.json");
        assert_eq!(
            report.text,
            format!(
                "The swarm filed 2 findings (part-0, part-1) and spent 270 microusd. Its \
                 artifact is {}.",
                artifact.canonicalize().unwrap().display()
            )
        );
        assert_eq!(report.spent_microusd, 270);
        assert_eq!(
            w.committed("tk_1").await,
            270,
            "the run is charged on the ticket"
        );
        let artifact_json: Value =
            serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
        assert_eq!(artifact_json["papers"].as_array().map(Vec::len), Some(2));
        let config = std::fs::read_to_string(run.join("config.yaml")).unwrap();
        assert!(config.contains("budget_microusd: 500000\n"), "{config}");
        assert!(config.contains("agent_count: 2\n"), "{config}");
        assert_eq!(read(&run, "cleanup.json")["verdict"], "PASS");
        assert_eq!(read(&run, "state.json")["held_microusd"], 0);
        w.no_fact().await;
        assert!(
            !run.with_file_name("as_1-2").exists(),
            "a repeated launch starts no second run"
        );
    }

    #[tokio::test]
    async fn a_stop_cancels_a_running_swarm_and_leaves_no_session_and_nothing_held() {
        let idle = Idle::default();
        let mut w = World::new(idle.clone(), 1, TEMPLATE);
        w.launch(launch("tk_1", "as_1", 3)).await;
        assert!(started(&w.fact().await, "as_1"));
        for _ in 0..500 {
            if idle.live.load(Ordering::SeqCst) == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(idle.live.load(Ordering::SeqCst), 3);
        assert!(w.executor.has(&AssignmentId::new("as_1")));

        w.stop("tk_1", "as_1").await;

        assert_eq!(idle.live.load(Ordering::SeqCst), 0, "no agent runs");
        assert!(!w.executor.has(&AssignmentId::new("as_1")));
        let run = w.run_dir("tk_1", "as_1");
        assert_eq!(read(&run, "cleanup.json")["verdict"], "PASS");
        let state = read(&run, "state.json");
        assert_eq!(
            (state["held_microusd"].clone(), state["goal_status"].clone()),
            (json!(0), json!("stopped"))
        );
        assert_eq!(
            read(&run, "runtime-report.json")["reason"],
            "its assignment ended: stopped by alice on slack"
        );
        assert!(!run.join("broker.sock").exists());
        assert!(
            w.charged("tk_1", "as_1").await,
            "a stopped run is charged too"
        );
        w.no_fact().await;
    }

    #[tokio::test]
    async fn the_goal_gets_what_the_ticket_has_left_after_earlier_work_and_runs_cut_short() {
        let mut w = World::new(filing(), 1, TEMPLATE);
        let tk = TicketId::new("tk_1");
        w.broker
            .charge_ticket(&tk, "deepseek", 100_000, "a worker's calls".into())
            .await
            .unwrap();
        let cut = w.run_dir("tk_1", "as_1");
        std::fs::create_dir_all(&cut).unwrap();
        let state = json!({"goal_status":"running","spent_microusd":20_000,"held_microusd":3_456});
        std::fs::write(cut.join("state.json"), state.to_string()).unwrap();

        w.launch(launch("tk_1", "as_2", 2)).await;
        assert!(started(&w.fact().await, "as_2"));
        assert_eq!(w.report().await.status, ReportStatus::Ready);
        let config =
            std::fs::read_to_string(w.run_dir("tk_1", "as_2").join("config.yaml")).unwrap();
        assert!(
            config.contains(&format!(
                "budget_microusd: {}\n",
                500_000 - 100_000 - 23_456
            )),
            "{config}"
        );
        assert_eq!(w.committed("tk_1").await, 100_000 + 23_456 + 270);
        assert!(w.charged("tk_1", "as_1").await && w.charged("tk_1", "as_2").await);
        assert!(cut.join("state.json").is_file(), "the run cut short stays");

        w.launch(launch("tk_1", "as_3", 2)).await;
        assert!(started(&w.fact().await, "as_3"));
        assert_eq!(w.report().await.status, ReportStatus::Ready);
        assert_eq!(
            w.committed("tk_1").await,
            100_000 + 23_456 + 270 + 270,
            "each run is charged once"
        );

        w.broker
            .charge_ticket(&tk, "deepseek", 500_000, "the rest".into())
            .await
            .unwrap();
        w.launch(launch("tk_1", "as_4", 2)).await;
        assert!(started(&w.fact().await, "as_4"));
        let report = w.report().await;
        assert_eq!(report.status, ReportStatus::Failed);
        assert!(
            report
                .text
                .ends_with("the ticket's budget of 500000 microusd is spent"),
            "{}",
            report.text
        );
    }

    #[tokio::test]
    async fn a_goal_the_template_cannot_make_is_a_failed_report_not_a_crash() {
        let two = TEMPLATE.replace("{% for n in range(agents) %}", "{% for n in range(2) %}");
        let mut w = World::new(filing(), 1, &two);
        w.launch(launch("tk_1", "as_1", 3)).await;
        assert!(started(&w.fact().await, "as_1"));
        let report = w.report().await;
        assert_eq!(report.status, ReportStatus::Failed);
        assert!(
            report.text.starts_with(
                "The swarm did not start: goal template research renders an invalid Goal"
            ),
            "{}",
            report.text
        );
        let Fact::Ended { reason, .. } = w.fact().await else {
            panic!("the assignment did not end")
        };
        assert!(reason.starts_with("swarm failed: "), "{reason}");
        assert!(!w.run_dir("tk_1", "as_1").exists(), "no run started");

        let mut named = launch("tk_2", "as_2", 2);
        named.plan = Plan::Swarm {
            agents: 2,
            goal_template: Some("audit".into()),
        };
        w.launch(named).await;
        assert!(started(&w.fact().await, "as_2"));
        let report = w.report().await;
        assert!(
            report.text.contains("there is no goal template audit"),
            "{}",
            report.text
        );
        let Fact::Ended { .. } = w.fact().await else {
            panic!("the assignment did not end")
        };
        w.launch(launch("tk_3", "as_3", 2)).await;
        assert!(started(&w.fact().await, "as_3"));
        assert_eq!(
            w.report().await.status,
            ReportStatus::Ready,
            "the executor still runs swarms"
        );
    }

    #[tokio::test]
    async fn a_running_swarm_reports_progress_quietly_so_it_is_not_taken_for_stalled() {
        let idle = Idle::default();
        let mut w = World::with_progress(idle, 1, TEMPLATE, Duration::from_millis(200));
        w.launch(launch("tk_1", "as_1", 2)).await;
        assert!(started(&w.fact().await, "as_1"));
        let progress = w.report().await;
        assert_eq!(progress.status, ReportStatus::Progress);
        assert_eq!(
            progress.text,
            "The swarm is running; it has filed 0 of its findings and spent 0 microusd."
        );
        w.stop("tk_1", "as_1").await;
        let minute = 60_000;
        assert_eq!(progress_every(None), Duration::from_secs(300));
        assert_eq!(progress_every(Some(30 * minute)), Duration::from_secs(300));
        assert_eq!(progress_every(Some(4 * minute)), Duration::from_secs(120));
        assert_eq!(progress_every(Some(10)), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn no_more_swarms_run_than_the_limit_and_a_queued_one_takes_a_freed_slot() {
        let idle = Idle::default();
        let mut w = World::new(idle.clone(), 1, TEMPLATE);
        w.launch(launch("tk_a", "as_a", 2)).await;
        w.launch(launch("tk_b", "as_b", 2)).await;
        assert!(started(&w.fact().await, "as_a"));
        w.no_fact().await;
        assert!(!w.run_dir("tk_b", "as_b").exists());

        w.stop("tk_a", "as_a").await;
        assert!(started(&w.fact().await, "as_b"));
        w.stop("tk_b", "as_b").await;
        assert_eq!(
            idle.most.load(Ordering::SeqCst),
            2,
            "one swarm of two at a time"
        );
        assert_eq!(idle.live.load(Ordering::SeqCst), 0);
        w.executor.shutdown().await;
    }

    #[tokio::test]
    async fn the_agents_pause_holds_a_running_swarm_and_leaves_an_operators_own_alone() {
        let dir = dir();
        let (state, run) = (dir.join("state"), dir.join("run"));
        std::fs::create_dir_all(&run).unwrap();
        let mirror = tokio::spawn(mirror_pause(state.clone(), run.clone()));
        let until = |want: bool| {
            let flag = run.join("PAUSE");
            async move {
                for _ in 0..50 {
                    if flag.exists() == want {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                panic!("PAUSE never became {want}");
            }
        };
        std::fs::write(state.join("PAUSE"), "").unwrap();
        until(true).await;
        std::fs::remove_file(state.join("PAUSE")).unwrap();
        until(false).await;

        std::fs::write(run.join("PAUSE"), "an operator's").unwrap();
        std::fs::write(state.join("PAUSE"), "").unwrap();
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        std::fs::remove_file(state.join("PAUSE")).unwrap();
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert_eq!(
            std::fs::read_to_string(run.join("PAUSE")).unwrap(),
            "an operator's"
        );
        mirror.abort();
        let _ = std::fs::remove_dir_all(dir);
    }
}
