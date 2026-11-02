//! `agent serve` end to end on the SQLite Store with the scripted adapters:
//! intake, judging, launch, delivery, replies, crashes, restarts, the
//! recovery of lost Assignments, retries and pause.

use anyhow::{Result, anyhow};
use solos_agent_core::AllowlistPolicy;
use solos_agent_core::model::{
    Actor, Addressed, AssignmentId, AssignmentState, ChannelRef, DecisionKind, DomainEvent, Effect,
    InputBody, MessageKind, OutboundMessage, ReportIn, ReportStatus, ThreadKey, TicketId,
    TicketOutcome, Trigger, TriggerId,
};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::jsonl::{JsonlChannel, JsonlSource};
use solos_swarm::agent::ports::{
    BoxFuture, Channel, Delivery, Executor, Fact, FactSink, Launch, SendOptions, SendOutcome,
};
use solos_swarm::agent::scripted::{ScriptedExecutor, ScriptedRouter};
use solos_swarm::agent::{Adapters, RunOptions, Runtime, STORE_FILE, ServeOptions, Summary, serve};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const BUDGET: u64 = 500_000;
const LIMIT: Duration = Duration::from_secs(30);

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agent-serve-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        Self { root }
    }
    fn triggers(&self) -> PathBuf {
        self.root.join("triggers.jsonl")
    }
    fn outbox(&self) -> PathBuf {
        self.root.join("outbox.jsonl")
    }
    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    fn store(&self) -> SqliteStore {
        SqliteStore::open(self.state().join(STORE_FILE)).unwrap()
    }
    fn append(&self, triggers: &[Trigger]) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.triggers())
            .unwrap();
        for trigger in triggers {
            writeln!(file, "{}", serde_json::to_string(trigger).unwrap()).unwrap();
        }
    }
    fn sent(&self) -> Vec<OutboundMessage> {
        std::fs::read_to_string(self.outbox())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn config(&self, paused: bool) -> PathBuf {
        let yaml = format!(
            "policy:
  paused: {paused}
  people: [slack:alice]
  bots: [slack:deploy-bot]
  approvers: [slack:alice]
sources:
  - kind: jsonl
    path: triggers.jsonl
channels:
  - kind: jsonl
    source: slack
    path: outbox.jsonl
router: {{kind: scripted}}
executor: {{kind: scripted}}
budgets: {{session_microusd: {BUDGET}}}
"
        );
        let path = self.root.join("agent.yaml");
        std::fs::write(&path, yaml).unwrap();
        path
    }
    async fn serve(&self, paused: bool) -> Summary {
        let config = self.config(paused);
        let options = ServeOptions {
            until_idle: true,
            ..ServeOptions::default()
        };
        tokio::time::timeout(LIMIT, serve(&config, &self.state(), options))
            .await
            .expect("serve did not go idle")
            .unwrap()
    }
    fn runtime(
        &self,
        paused: bool,
        executor: Arc<dyn Executor>,
        channel: Arc<dyn Channel>,
    ) -> Runtime {
        self.runtime_until(paused, executor, channel, true)
    }
    fn runtime_until(
        &self,
        paused: bool,
        executor: Arc<dyn Executor>,
        channel: Arc<dyn Channel>,
        until_idle: bool,
    ) -> Runtime {
        let adapters = Adapters {
            sources: vec![Box::new(JsonlSource::new(self.triggers(), false))],
            channels: BTreeMap::from([("slack".to_owned(), channel)]),
            router: Arc::new(ScriptedRouter::new(BUDGET)),
            executor,
        };
        let options = RunOptions {
            until_idle,
            tick: Duration::from_millis(10),
            retry_base: Duration::from_millis(50),
            retry_max: Duration::from_secs(1),
        };
        Runtime::new(self.store(), policy(paused), adapters, options)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn policy(paused: bool) -> AllowlistPolicy {
    AllowlistPolicy {
        paused,
        people: ["slack:alice".to_owned()].into(),
        bots: ["slack:deploy-bot".to_owned()].into(),
        approvers: ["slack:alice".to_owned()].into(),
        ..AllowlistPolicy::default()
    }
}

fn slack(id: &str, actor: Actor, thread: &str, text: &str) -> Trigger {
    Trigger {
        id: TriggerId::new(id),
        source: "slack".into(),
        at_ms: 1_000,
        actor,
        addressed: Addressed::Mention,
        text: text.into(),
        threads: vec![ThreadKey::new(thread)],
        reply_to: vec![ChannelRef {
            source: "slack".into(),
            address: BTreeMap::from([
                ("channel".to_owned(), "C1".to_owned()),
                ("thread_ts".to_owned(), thread.to_owned()),
            ]),
        }],
        meta: serde_json::json!({}),
    }
}

fn request(id: &str, thread: &str, text: &str) -> Trigger {
    slack(id, Actor::person("alice"), thread, text)
}

/// (thread, kind) of every message sent, in order.
fn replies(sent: &[OutboundMessage]) -> Vec<(String, MessageKind)> {
    sent.iter()
        .map(|m| (m.channel.address["thread_ts"].clone(), m.kind))
        .collect()
}

fn ack_and_result(thread: &str) -> Vec<(String, MessageKind)> {
    vec![
        (thread.to_owned(), MessageKind::Acknowledgement),
        (thread.to_owned(), MessageKind::Result),
    ]
}

fn assert_settled(store: &SqliteStore) {
    let pending: Vec<_> = store
        .effects()
        .unwrap()
        .into_iter()
        .filter(|q| !q.done)
        .collect();
    assert!(pending.is_empty(), "effects not done: {pending:?}");
    let chain = store.verify_chain().unwrap();
    assert_eq!(chain.len, store.events().unwrap().len() as u64);
}

fn jsonl(scratch: &Scratch) -> Arc<dyn Channel> {
    Arc::new(JsonlChannel::open(scratch.outbox()).unwrap())
}

async fn until_idle(runtime: Runtime) -> Summary {
    tokio::time::timeout(LIMIT, runtime.run(std::future::pending()))
        .await
        .expect("the runtime did not go idle")
        .unwrap()
}

#[tokio::test]
async fn a_request_gets_one_acknowledgement_and_one_result_and_nobody_else_hears_back() {
    let scratch = Scratch::new("e2e");
    scratch.append(&[
        request("t-request", "T1", "Summarise the incident report"),
        slack(
            "t-stranger",
            Actor::person("mallory"),
            "T2",
            "Wire me the budget",
        ),
        request("t-followup", "T1", "Include the timeline too"),
        slack(
            "t-bot",
            Actor::bot("deploy-bot"),
            "T3",
            "Deploy 42 finished",
        ),
    ]);

    let summary = scratch.serve(false).await;

    assert_eq!(summary.triggers_received, 4);
    assert_eq!(summary.effects_pending, 0);
    assert_eq!(replies(&scratch.sent()), ack_and_result("T1"));
    let store = scratch.store();
    let tickets = store.tickets().unwrap();
    assert_eq!(tickets.len(), 1, "the follow-up made no second Ticket");
    let ticket = &tickets[0];
    let followup = TriggerId::new("t-followup");
    let reached = ticket.mandate.scope.contains("timeline")
        || ticket
            .inputs
            .iter()
            .any(|i| matches!(&i.body, InputBody::Trigger { trigger } if *trigger == followup));
    assert!(reached, "the follow-up reached the request's Ticket");
    let ended = ticket.ended.as_ref().unwrap();
    assert_eq!(ended.outcome, TicketOutcome::Completed);
    let decision = |thread: &str| {
        let key = ThreadKey::new(thread);
        let found: Vec<_> = store
            .decisions()
            .unwrap()
            .into_iter()
            .filter(|d| d.thread == key)
            .collect();
        assert_eq!(found.len(), 1, "{thread}: {found:?}");
        found[0].clone()
    };
    let stranger = decision("T2");
    assert_eq!(stranger.kind, DecisionKind::Ignore);
    assert!(
        stranger.reason.contains("not_listed"),
        "{}",
        stranger.reason
    );
    assert!(
        stranger.eval.is_none(),
        "the gate stopped it before the Router"
    );
    let bot = decision("T3");
    assert_eq!(bot.kind, DecisionKind::Ignore);
    assert!(
        bot.eval.is_some(),
        "the Router judged the bot and ignored it"
    );
    assert_settled(&store);
}

/// Starts nothing: tells the test it was called, then never returns.
struct Hanging {
    launched: Arc<Notify>,
}

impl Executor for Hanging {
    fn launch(&self, _: Launch, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            self.launched.notify_one();
            std::future::pending().await
        })
    }
    fn deliver(&self, _: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// A JSONL channel that tells the test after each message it wrote.
struct Observed {
    inner: JsonlChannel,
    sent: Arc<Notify>,
}

impl Channel for Observed {
    fn send(
        &self,
        message: OutboundMessage,
        options: SendOptions,
    ) -> BoxFuture<'_, Result<SendOutcome>> {
        Box::pin(async move {
            let outcome = self.inner.send(message, options).await?;
            self.sent.notify_one();
            Ok(outcome)
        })
    }
}

/// Run until the Launch is handed to the executor and the acknowledgement is
/// written, then drop the runtime mid-flight, as a crash would.
async fn crash_after_launch(scratch: &Scratch) {
    let launched = Arc::new(Notify::new());
    let sent = Arc::new(Notify::new());
    let executor = Arc::new(Hanging {
        launched: launched.clone(),
    });
    let channel = Arc::new(Observed {
        inner: JsonlChannel::open(scratch.outbox()).unwrap(),
        sent: sent.clone(),
    });
    let runtime = scratch.runtime(false, executor, channel);
    let crash = async { tokio::join!(launched.notified(), sent.notified()) };
    tokio::select! {
        ended = runtime.run(std::future::pending()) => panic!("the runtime ended: {ended:?}"),
        waited = tokio::time::timeout(LIMIT, crash) => {
            waited.expect("the Launch reached the executor and the acknowledgement went out");
        }
    }
    let store = scratch.store();
    let launch = store
        .effects()
        .unwrap()
        .into_iter()
        .find(|q| matches!(q.effect, Effect::Launch { .. }))
        .expect("the Launch was committed");
    assert!(
        !launch.done,
        "the crash came before the Launch was marked done"
    );
    let ticket = &store.tickets().unwrap()[0];
    assert_eq!(ticket.assignments[0].state, AssignmentState::Pending);
}

#[tokio::test]
async fn a_launch_cut_short_by_a_crash_runs_again_and_each_reply_goes_out_once() {
    let scratch = Scratch::new("crash");
    scratch.append(&[request("t-request", "T1", "Fix the flaky test")]);
    crash_after_launch(&scratch).await;
    assert_eq!(
        replies(&scratch.sent()),
        [("T1".to_owned(), MessageKind::Acknowledgement)]
    );

    let restarted = scratch.runtime(false, Arc::new(ScriptedExecutor), jsonl(&scratch));
    let summary = until_idle(restarted).await;

    assert_eq!(summary.triggers_duplicate, 1);
    assert_eq!(replies(&scratch.sent()), ack_and_result("T1"));
    let store = scratch.store();
    let ticket = &store.tickets().unwrap()[0];
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    assert_eq!(
        ticket.assignments.len(),
        1,
        "the redone Launch started the same Assignment"
    );
    assert_settled(&store);
}

#[tokio::test]
async fn a_pause_holds_every_effect_until_it_lifts() {
    let scratch = Scratch::new("pause");
    scratch.append(&[request("t-request", "T1", "Fix the flaky test")]);
    crash_after_launch(&scratch).await;
    scratch.append(&[request("t-second", "T4", "Rotate the staging keys")]);

    let held = scratch.runtime(true, Arc::new(ScriptedExecutor), jsonl(&scratch));
    let summary = until_idle(held).await;

    assert_eq!((summary.triggers_received, summary.effects_done), (1, 0));
    assert!(summary.effects_pending >= 1, "{summary:?}");
    assert_eq!(
        replies(&scratch.sent()),
        [("T1".to_owned(), MessageKind::Acknowledgement)]
    );
    {
        let store = scratch.store();
        let ticket = &store.tickets().unwrap()[0];
        assert_eq!(ticket.assignments[0].state, AssignmentState::Pending);
        assert_eq!(
            store.tickets().unwrap().len(),
            1,
            "no Eval started under the pause"
        );
    }

    let lifted = scratch.runtime(false, Arc::new(ScriptedExecutor), jsonl(&scratch));
    until_idle(lifted).await;

    // The two Threads are separate lanes, so only each Thread's own order is fixed.
    let on = |thread: &str| -> Vec<_> {
        replies(&scratch.sent())
            .into_iter()
            .filter(|(t, _)| t == thread)
            .collect()
    };
    assert_eq!(scratch.sent().len(), 4);
    assert_eq!(on("T1"), ack_and_result("T1"));
    assert_eq!(on("T4"), ack_and_result("T4"));
    let store = scratch.store();
    assert!(store.tickets().unwrap().iter().all(|t| t.ended.is_some()));
    assert_settled(&store);
}

#[tokio::test]
async fn a_restart_skips_the_triggers_it_has_and_takes_the_new_ones() {
    let scratch = Scratch::new("restart");
    scratch.append(&[request("t-first", "T1", "Draft the release notes")]);
    let first = scratch.serve(false).await;
    assert_eq!((first.triggers_received, first.triggers_duplicate), (1, 0));
    assert_eq!(replies(&scratch.sent()), ack_and_result("T1"));

    scratch.append(&[request("t-second", "T4", "Rotate the staging keys")]);
    let second = scratch.serve(false).await;

    assert_eq!(
        (second.triggers_received, second.triggers_duplicate),
        (1, 1)
    );
    assert_eq!(
        replies(&scratch.sent()),
        [ack_and_result("T1"), ack_and_result("T4")].concat()
    );
    let store = scratch.store();
    let received: Vec<TriggerId> = store
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.event {
            DomainEvent::TriggerReceived { trigger, .. } => Some(trigger),
            _ => None,
        })
        .collect();
    assert_eq!(
        received,
        [TriggerId::new("t-first"), TriggerId::new("t-second")]
    );
    assert_eq!(store.tickets().unwrap().len(), 2);
    assert_eq!(store.decisions().unwrap().len(), 2);
    assert_settled(&store);
}

/// Refuses the first `failures` sends, then writes through.
struct Flaky {
    inner: JsonlChannel,
    failures: AtomicU32,
    calls: Mutex<Vec<Instant>>,
}

impl Channel for Flaky {
    fn send(
        &self,
        message: OutboundMessage,
        options: SendOptions,
    ) -> BoxFuture<'_, Result<SendOutcome>> {
        self.calls.lock().unwrap().push(Instant::now());
        let refuse = self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        Box::pin(async move {
            if refuse {
                return Err(anyhow!("rate limited"));
            }
            self.inner.send(message, options).await
        })
    }
}

#[tokio::test]
async fn a_channel_that_fails_twice_sends_once_after_backing_off() {
    let scratch = Scratch::new("backoff");
    scratch.append(&[request("t-request", "T1", "Fix the flaky test")]);
    let channel = Arc::new(Flaky {
        inner: JsonlChannel::open(scratch.outbox()).unwrap(),
        failures: AtomicU32::new(2),
        calls: Mutex::new(Vec::new()),
    });

    let runtime = scratch.runtime(false, Arc::new(ScriptedExecutor), channel.clone());
    let summary = until_idle(runtime).await;

    assert_eq!(summary.effect_failures, 2);
    assert_eq!(replies(&scratch.sent()), ack_and_result("T1"));
    let calls = channel.calls.lock().unwrap().clone();
    assert_eq!(
        calls.len(),
        4,
        "three tries for the acknowledgement, one for the result"
    );
    // Retry times are whole wall-clock milliseconds, so allow for rounding.
    assert!(
        calls[1] - calls[0] >= Duration::from_millis(48),
        "{calls:?}"
    );
    assert!(
        calls[2] - calls[1] >= Duration::from_millis(98),
        "the second wait doubles: {calls:?}"
    );
    let store = scratch.store();
    let ack = store
        .effects()
        .unwrap()
        .into_iter()
        .find(|q| matches!(&q.effect, Effect::Send(m) if m.kind == MessageKind::Acknowledgement))
        .unwrap();
    assert_eq!((ack.attempts, ack.done), (2, true));
    assert_eq!(ack.last_error.as_deref(), Some("rate limited"));
    assert_settled(&store);
}

/// The scripted executor, keeping every Delivery it is handed.
struct Recording {
    inner: ScriptedExecutor,
    deliveries: Mutex<Vec<Delivery>>,
}

impl Executor for Recording {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        self.inner.launch(launch, facts)
    }
    fn deliver(&self, delivery: Delivery, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        self.deliveries.lock().unwrap().push(delivery.clone());
        self.inner.deliver(delivery, facts)
    }
    fn stop(
        &self,
        ticket: TicketId,
        assignment: AssignmentId,
        reason: String,
    ) -> BoxFuture<'_, Result<()>> {
        self.inner.stop(ticket, assignment, reason)
    }
}

#[tokio::test]
async fn a_bot_message_on_the_requests_thread_reaches_the_worker_as_context() {
    let scratch = Scratch::new("deliver");
    let ci = "CI failed on main: test_retry timed out";
    scratch.append(&[
        request("t-request", "T1", "Fix the flaky test"),
        slack("t-ci", Actor::bot("deploy-bot"), "T1", ci),
    ]);
    let executor = Arc::new(Recording {
        inner: ScriptedExecutor,
        deliveries: Mutex::new(Vec::new()),
    });

    until_idle(scratch.runtime(false, executor.clone(), jsonl(&scratch))).await;

    let deliveries = executor.deliveries.lock().unwrap().clone();
    let [delivery] = &deliveries[..] else {
        panic!("{deliveries:?}")
    };
    let [delivered] = &delivery.inputs[..] else {
        panic!("{delivery:?}")
    };
    assert!(!delivered.input.from_allowed_person);
    let trigger = delivered.trigger.as_ref().unwrap();
    assert_eq!((trigger.id.as_str(), trigger.text.as_str()), ("t-ci", ci));
    assert_eq!(replies(&scratch.sent()), ack_and_result("T1"));
    let store = scratch.store();
    let judged: Vec<_> = store
        .decisions()
        .unwrap()
        .into_iter()
        .filter(|d| d.triggers == [TriggerId::new("t-ci")])
        .collect();
    assert_eq!(
        judged.len(),
        1,
        "the bot's message was also judged on its own"
    );
    assert_eq!(judged[0].kind, DecisionKind::Ignore);
    assert_settled(&store);
}

#[tokio::test]
async fn a_reply_to_a_source_with_no_channel_is_dropped_and_the_runtime_still_goes_idle() {
    let scratch = Scratch::new("no-channel");
    let mut asked = request("t-github", "T9", "Summarise the incident report");
    for address in &mut asked.reply_to {
        address.source = "github".into();
    }
    scratch.append(&[asked]);

    let summary = scratch.serve(false).await;

    assert_eq!(summary.effects_pending, 0, "nothing retries forever");
    assert!(scratch.sent().is_empty(), "no channel serves github");
    let store = scratch.store();
    let ended = store.tickets().unwrap()[0].ended.clone();
    assert!(ended.is_some(), "the work itself still completed");
}

/// Starts every Assignment, reports `status` on it if given, and keeps
/// every Launch it is handed.
#[derive(Default)]
struct Starts {
    status: Option<ReportStatus>,
    launches: Mutex<Vec<Launch>>,
}

impl Executor for Starts {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        self.launches.lock().unwrap().push(launch.clone());
        let status = self.status;
        Box::pin(async move {
            let (ticket, assignment) = (launch.ticket, launch.assignment);
            facts
                .send(Fact::Started {
                    ticket: ticket.clone(),
                    assignment: assignment.clone(),
                })
                .await?;
            if let Some(status) = status {
                let report = ReportIn {
                    status,
                    text: format!("{status:?}"),
                    question: None,
                    spent_microusd: 0,
                };
                facts
                    .send(Fact::Report {
                        ticket,
                        assignment,
                        report,
                    })
                    .await?;
            }
            Ok(())
        })
    }
    fn deliver(&self, _: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// Run until the first Ticket's Assignment runs with `reports` Reports and
/// the acknowledgement is out, then drop the runtime, as a crash would.
async fn crash_while_running(scratch: &Scratch, executor: Arc<Starts>, reports: usize) {
    let runtime = scratch.runtime_until(false, executor, jsonl(scratch), false);
    let running = async {
        for _ in 0..1_000 {
            let ticket = scratch.store().tickets().unwrap().into_iter().next();
            let ran = ticket.is_some_and(|t| {
                t.running_assignment()
                    .is_some_and(|a| a.reports.len() == reports)
            });
            if ran && !scratch.sent().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the assignment never ran");
    };
    tokio::select! {
        ended = runtime.run(std::future::pending()) => panic!("the runtime ended: {ended:?}"),
        () = running => {}
    }
}

#[tokio::test]
async fn a_running_assignment_cut_short_by_a_crash_restarts_as_a_recovery_and_replies_once() {
    let scratch = Scratch::new("recover");
    scratch.append(&[request("t-request", "T1", "Fix the flaky test")]);
    crash_while_running(&scratch, Arc::new(Starts::default()), 0).await;
    assert_eq!(
        replies(&scratch.sent()),
        [("T1".to_owned(), MessageKind::Acknowledgement)]
    );

    let after = Arc::new(Starts {
        status: Some(ReportStatus::Ready),
        ..Starts::default()
    });
    until_idle(scratch.runtime(false, after.clone(), jsonl(&scratch))).await;

    assert_eq!(replies(&scratch.sent()), ack_and_result("T1"));
    let launches = after.launches.lock().unwrap().clone();
    let [launch] = &launches[..] else {
        panic!("{launches:?}")
    };
    assert!(launch.recovery, "the new Assignment is a recovery");
    let store = scratch.store();
    let ticket = &store.tickets().unwrap()[0];
    let reasons: Vec<_> = ticket
        .assignments
        .iter()
        .map(|a| (a.state, a.ended_reason.clone()))
        .collect();
    assert_eq!(
        reasons,
        [
            (AssignmentState::Ended, Some("host restarted".to_owned())),
            (AssignmentState::Ended, Some("ready".to_owned()))
        ]
    );
    assert_eq!(launch.assignment, ticket.assignments[1].id);
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    assert_settled(&store);
}

#[tokio::test]
async fn a_waiting_assignment_ends_at_a_restart_and_the_next_input_starts_a_recovery() {
    let scratch = Scratch::new("recover-idle");
    scratch.append(&[request("t-request", "T1", "Fix the flaky test")]);
    let waits = Arc::new(Starts {
        status: Some(ReportStatus::Waiting),
        ..Starts::default()
    });
    crash_while_running(&scratch, waits, 1).await;

    let quiet = Arc::new(Starts::default());
    until_idle(scratch.runtime(false, quiet.clone(), jsonl(&scratch))).await;
    assert!(quiet.launches.lock().unwrap().is_empty(), "nothing to redo");
    let ticket = scratch.store().tickets().unwrap()[0].clone();
    assert_eq!(ticket.assignments.len(), 1);
    assert_eq!(
        ticket.assignments[0].ended_reason.as_deref(),
        Some("host restarted")
    );
    assert!(ticket.ended.is_none(), "the Ticket stays open");

    scratch.append(&[request("t-more", "T1", "It fails on CI too")]);
    let woken = Arc::new(Starts {
        status: Some(ReportStatus::Ready),
        ..Starts::default()
    });
    until_idle(scratch.runtime(false, woken.clone(), jsonl(&scratch))).await;
    let launches = woken.launches.lock().unwrap().clone();
    let [launch] = &launches[..] else {
        panic!("{launches:?}")
    };
    assert!(launch.recovery);
    let carried: Vec<_> = launch
        .inputs
        .iter()
        .filter_map(|d| d.trigger.as_ref().map(|t| t.id.as_str().to_owned()))
        .collect();
    assert_eq!(
        carried,
        ["t-more"],
        "the launch carries the Ticket's inputs"
    );
    assert_eq!(replies(&scratch.sent()), ack_and_result("T1"));
    assert_settled(&scratch.store());
}
