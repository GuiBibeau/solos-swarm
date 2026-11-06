//! Work that only takes time, for `agent bench`: the article's work tool. An
//! Assignment starts when a slot is free, waits its request's duration on a
//! scaled clock and reports `ready`. [`HeldRouter`] makes the Router wait
//! while any of that work runs, which is how the `serial` design stands in
//! for one session that runs its subagent in its own turn.

use super::ports::{BoxFuture, Delivery, Executor, Fact, FactSink, Launch, Router};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use solos_agent_core::model::{AssignmentId, Candidate, ReportIn, ReportStatus, TicketId};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

/// Wall time per unit of workload time: at 0.01 a workload minute lasts
/// 0.6 seconds, at 1 it lasts a minute.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct TimeFactor(f64);

/// Parsing refuses NaN, so equality is reflexive.
impl Eq for TimeFactor {}

impl TimeFactor {
    pub const REAL_TIME: Self = Self(1.0);
    /// A workload minute lasts a wall second.
    pub const MINUTE_A_SECOND: Self = Self(1.0 / 60.0);

    pub fn new(factor: f64) -> Result<Self> {
        ensure!(
            factor.is_finite() && factor > 0.0 && factor <= 1_000.0,
            "a time factor is above 0 and at most 1000, not {factor}"
        );
        Ok(Self(factor))
    }

    pub fn get(self) -> f64 {
        self.0
    }

    /// How long `workload_ms` of workload time lasts on the wall clock.
    pub fn wall(self, workload_ms: u64) -> Duration {
        Duration::from_secs_f64(workload_ms as f64 / 1_000.0 * self.0)
    }

    /// `wall_ms` of wall time, in workload minutes.
    pub fn workload_minutes(self, wall_ms: u64) -> f64 {
        wall_ms as f64 / self.0 / 60_000.0
    }
}

impl TryFrom<f64> for TimeFactor {
    type Error = anyhow::Error;
    fn try_from(factor: f64) -> Result<Self> {
        Self::new(factor)
    }
}

impl From<TimeFactor> for f64 {
    fn from(factor: TimeFactor) -> Self {
        factor.0
    }
}

/// The work an Assignment simulates, in workload milliseconds: its Ticket's
/// origin names it as `meta.bench.work_ms`, which the bench's workload
/// writes on every request.
pub fn work_ms(launch: &Launch) -> Option<u64> {
    launch
        .origin
        .meta
        .pointer("/bench/work_ms")
        .and_then(serde_json::Value::as_u64)
}

pub struct SimulatedParts {
    pub max_workers: usize,
    pub time_factor: TimeFactor,
    /// The work of a Ticket whose origin names none, such as chatter a
    /// model Router accepted.
    pub default_work_ms: u64,
}

pub struct SimulatedExecutor {
    time_factor: TimeFactor,
    default_work_ms: u64,
    slots: Arc<Semaphore>,
    /// Assignments launched and not finished, waiting for a slot or not.
    busy: Arc<watch::Sender<usize>>,
    work: Mutex<BTreeMap<AssignmentId, JoinHandle<()>>>,
}

/// Counts an Assignment as busy until its task ends, however it ends.
struct Busy(Arc<watch::Sender<usize>>);

impl Busy {
    fn start(busy: &Arc<watch::Sender<usize>>) -> Self {
        busy.send_modify(|n| *n += 1);
        Self(busy.clone())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n = n.saturating_sub(1));
    }
}

impl SimulatedExecutor {
    pub fn new(parts: SimulatedParts) -> Result<Self> {
        ensure!(
            parts.max_workers > 0,
            "a simulated executor needs a worker slot"
        );
        Ok(Self {
            time_factor: parts.time_factor,
            default_work_ms: parts.default_work_ms,
            slots: Arc::new(Semaphore::new(parts.max_workers)),
            busy: Arc::new(watch::Sender::new(0)),
            work: Mutex::new(BTreeMap::new()),
        })
    }

    /// `router`, holding each Candidate until no work runs.
    pub fn hold(&self, router: Arc<dyn Router>) -> HeldRouter {
        HeldRouter {
            inner: router,
            busy: self.busy.subscribe(),
        }
    }

    fn work(&self) -> MutexGuard<'_, BTreeMap<AssignmentId, JoinHandle<()>>> {
        self.work.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for SimulatedExecutor {
    fn drop(&mut self) {
        for (_, task) in std::mem::take(&mut *self.work()) {
            task.abort();
        }
    }
}

impl Executor for SimulatedExecutor {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        let mut work = self.work();
        work.retain(|_, task| !task.is_finished());
        if !work.contains_key(&launch.assignment) {
            let busy = Busy::start(&self.busy);
            let wait = self
                .time_factor
                .wall(work_ms(&launch).unwrap_or(self.default_work_ms));
            let slots = self.slots.clone();
            let id = launch.assignment.clone();
            let task = tokio::spawn(async move {
                let _busy = busy;
                if let Err(err) = simulate(launch, wait, &slots, &facts).await {
                    eprintln!("agent: simulated work stopped: {err:#}");
                }
            });
            work.insert(id, task);
        }
        Box::pin(async { Ok(()) })
    }

    fn deliver(&self, _: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn stop(&self, _: TicketId, assignment: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        if let Some(task) = self.work().remove(&assignment) {
            task.abort();
        }
        Box::pin(async { Ok(()) })
    }
}

async fn simulate(
    launch: Launch,
    wait: Duration,
    slots: &Semaphore,
    facts: &FactSink,
) -> Result<()> {
    let _slot = slots
        .acquire()
        .await
        .map_err(|_| anyhow!("the worker slots are closed"))?;
    let Launch {
        ticket,
        assignment,
        title,
        ..
    } = launch;
    facts
        .send(Fact::Started {
            ticket: ticket.clone(),
            assignment: assignment.clone(),
        })
        .await?;
    tokio::time::sleep(wait).await;
    let report = ReportIn {
        status: ReportStatus::Ready,
        text: format!(
            "Done: {title}. (Simulated work: {:.1}s of wall time.)",
            wait.as_secs_f64()
        ),
        question: None,
        spent_microusd: 0,
        harvest: None,
    };
    facts
        .send(Fact::Report {
            ticket,
            assignment,
            report,
        })
        .await
}

/// A Router that takes no Candidate while simulated work runs: one session
/// that runs its subagent inside its own turn reads nothing new until the
/// subagent returns.
pub struct HeldRouter {
    inner: Arc<dyn Router>,
    busy: watch::Receiver<usize>,
}

impl Router for HeldRouter {
    fn assess(&self, candidate: Candidate, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        let mut busy = self.busy.clone();
        Box::pin(async move {
            busy.wait_for(|running| *running == 0)
                .await
                .map_err(|_| anyhow!("the simulated executor has stopped"))?;
            self.inner.assess(candidate, facts).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{SimulatedExecutor, SimulatedParts, TimeFactor};
    use crate::agent::ports::{BoxFuture, Executor, Fact, FactSink, Inbound, Launch, Router};
    use serde_json::json;
    use solos_agent_core::model::{
        AssignmentId, Candidate, EvalId, Plan, ReportStatus, Strength, ThreadKey, TicketId,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn executor(max_workers: usize) -> SimulatedExecutor {
        SimulatedExecutor::new(SimulatedParts {
            max_workers,
            time_factor: TimeFactor::new(0.001).unwrap(),
            default_work_ms: 600_000,
        })
        .unwrap()
    }

    /// An Assignment whose request names 100 s of work: 100 ms here.
    fn launch(n: u32) -> Launch {
        Launch {
            ticket: TicketId::new(format!("tk_{n}")),
            assignment: AssignmentId::new(format!("as_{n}")),
            title: format!("Request {n}"),
            scope: "Simulated.".into(),
            origin: serde_json::from_value(json!({
                "id": format!("t{n}"), "source": "bench", "at_ms": 0,
                "actor": {"id": "dev-1", "kind": "person"}, "addressed": "mention",
                "text": "@agent work", "threads": ["T1"],
                "meta": {"bench": {"kind": "request", "work_ms": 100_000}}
            }))
            .unwrap(),
            rules: Vec::new(),
            plan: Plan::Session {
                harness: None,
                environment: None,
                method: None,
            },
            budget_microusd: Some(1),
            repo: None,
            recovery: false,
            inputs: Vec::new(),
        }
    }

    /// The next fact as `(assignment, started)`: started, or ready.
    async fn next(inbound: &mut mpsc::Receiver<Inbound>) -> (String, bool) {
        let message = tokio::time::timeout(Duration::from_secs(5), inbound.recv())
            .await
            .unwrap()
            .unwrap();
        match message {
            Inbound::Fact(Fact::Started { assignment, .. }) => (assignment.to_string(), true),
            Inbound::Fact(Fact::Report {
                assignment, report, ..
            }) => {
                assert_eq!(report.status, ReportStatus::Ready);
                (assignment.to_string(), false)
            }
            _ => panic!("an unexpected message"),
        }
    }

    #[tokio::test]
    async fn one_slot_runs_one_assignment_at_a_time_and_a_repeated_launch_starts_nothing() {
        let simulated = executor(1);
        let (facts, mut inbound) = FactSink::channel();
        simulated.launch(launch(1), facts.clone()).await.unwrap();
        simulated.launch(launch(1), facts.clone()).await.unwrap();
        simulated.launch(launch(2), facts.clone()).await.unwrap();
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(next(&mut inbound).await);
        }
        let expected = [
            ("as_1", true),
            ("as_1", false),
            ("as_2", true),
            ("as_2", false),
        ];
        assert_eq!(seen, expected.map(|(a, s)| (a.to_owned(), s)));
        let extra = tokio::time::timeout(Duration::from_millis(250), inbound.recv()).await;
        assert!(extra.is_err(), "as_1 ran once");
    }

    struct Counting(Arc<AtomicUsize>);

    impl Router for Counting {
        fn assess(&self, _: Candidate, _: FactSink) -> BoxFuture<'_, anyhow::Result<()>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn a_held_router_takes_a_candidate_once_the_work_has_ended() {
        let simulated = executor(4);
        let assessed = Arc::new(AtomicUsize::new(0));
        let held = Arc::new(simulated.hold(Arc::new(Counting(assessed.clone()))));
        let (facts, mut inbound) = FactSink::channel();
        simulated.launch(launch(1), facts.clone()).await.unwrap();
        let candidate = Candidate {
            eval: EvalId::new("ev_1"),
            thread: ThreadKey::new("T2"),
            strength: Strength::Explicit,
            policy_version: "pv".into(),
            triggers: Vec::new(),
            tickets: Vec::new(),
        };
        let (router, sink) = (held.clone(), facts.clone());
        let assess = tokio::spawn(async move { router.assess(candidate, sink).await });
        assert_eq!(next(&mut inbound).await, ("as_1".to_owned(), true));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            assessed.load(Ordering::SeqCst),
            0,
            "held while the work runs"
        );
        assert_eq!(next(&mut inbound).await, ("as_1".to_owned(), false));
        assess.await.unwrap().unwrap();
        assert_eq!(assessed.load(Ordering::SeqCst), 1);
    }
}
