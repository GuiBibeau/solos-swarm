//! The schedule Source on the runtime and the SQLite Store, with a clock
//! the test moves: each slot fires once, a restart in a slot fires nothing,
//! and a start after missed slots fires the latest one.

use solos_agent_core::AllowlistPolicy;
use solos_agent_core::model::{ActorKind, Addressed, ChannelRef, OutboundMessage, ThreadKey};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::github;
use solos_swarm::agent::jsonl::JsonlChannel;
use solos_swarm::agent::ports::Channel;
use solos_swarm::agent::schedule::{Clock, Schedule, ScheduleSource};
use solos_swarm::agent::scripted::{ScriptedExecutor, ScriptedRouter};
use solos_swarm::agent::utc;
use solos_swarm::agent::{Adapters, RunOptions, Runtime, STORE_FILE, Summary};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

const LIMIT: Duration = Duration::from_secs(30);
const HOUR: u64 = 3_600_000;

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agent-schedule-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        Self { root }
    }
    fn store(&self) -> SqliteStore {
        SqliteStore::open(self.root.join("state").join(STORE_FILE)).unwrap()
    }
    fn outbox(&self) -> PathBuf {
        self.root.join("outbox.jsonl")
    }
    fn sent(&self) -> Vec<OutboundMessage> {
        std::fs::read_to_string(self.outbox())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn ids(&self) -> Vec<String> {
        let store = self.store();
        store
            .triggers()
            .unwrap()
            .iter()
            .map(|t| t.id.to_string())
            .collect()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A clock the test sets, starting at `at`.
fn clock(at: &str) -> (Arc<AtomicU64>, Clock) {
    let now = Arc::new(AtomicU64::new(utc::parse(at).unwrap()));
    let read = now.clone();
    (now, Arc::new(move || read.load(Ordering::SeqCst)))
}

fn set(now: &AtomicU64, at: &str) {
    now.store(utc::parse(at).unwrap(), Ordering::SeqCst);
}

fn hourly(reply_to: Option<ChannelRef>) -> Schedule {
    let threads = reply_to
        .as_ref()
        .map(|_| github::thread("acme/widgets", 1))
        .into_iter()
        .collect();
    Schedule {
        name: "sweep".into(),
        period_ms: HOUR,
        offset_ms: 0,
        text: "Sweep the open issues.".into(),
        reply_to,
        threads,
    }
}

fn daily_at_nine() -> Schedule {
    Schedule {
        name: "standup".into(),
        period_ms: 24 * HOUR,
        offset_ms: 9 * HOUR,
        text: "Summarise yesterday.".into(),
        reply_to: None,
        threads: Vec::new(),
    }
}

fn runtime(scratch: &Scratch, source: ScheduleSource, until_idle: bool) -> Runtime {
    let outbox: Arc<dyn Channel> = Arc::new(JsonlChannel::open(scratch.outbox()).unwrap());
    let adapters = Adapters {
        sources: vec![Box::new(source)],
        channels: BTreeMap::from([("github".to_owned(), outbox)]),
        router: Arc::new(ScriptedRouter::new(500_000)),
        executor: Arc::new(ScriptedExecutor),
    };
    let options = RunOptions {
        until_idle,
        tick: Duration::from_millis(10),
        retry_base: Duration::from_millis(10),
        retry_max: Duration::from_millis(500),
    };
    Runtime::new(
        scratch.store(),
        AllowlistPolicy::default(),
        adapters,
        options,
    )
}

/// One start at `at`, run until idle.
async fn start_at(scratch: &Scratch, schedules: Vec<Schedule>, at: &str) -> Summary {
    let (_, clock) = clock(at);
    let source = ScheduleSource::new(schedules, false).with_clock(clock, Duration::from_millis(5));
    tokio::time::timeout(
        LIMIT,
        runtime(scratch, source, true).run(std::future::pending()),
    )
    .await
    .expect("the run did not go idle")
    .unwrap()
}

async fn until_triggers(scratch: &Scratch, n: usize) {
    let deadline = Instant::now() + LIMIT;
    while scratch.ids().len() < n {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {n} triggers"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_slot_fires_once_and_the_next_fires_when_it_starts() {
    let scratch = Scratch::new("fires");
    let reply = github::reply_to("acme/widgets", 1);
    let (now, clock) = clock("2026-11-02T12:00:00Z");
    let source = ScheduleSource::new(vec![hourly(Some(reply.clone()))], true)
        .with_clock(clock, Duration::from_millis(5));
    let (stop, stopped) = oneshot::channel::<()>();
    let driver = async {
        until_triggers(&scratch, 1).await;
        set(&now, "2026-11-02T12:59:59Z");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(scratch.ids().len(), 1, "the slot fired once");
        set(&now, "2026-11-02T13:00:00Z");
        until_triggers(&scratch, 2).await;
        set(&now, "2026-11-02T13:30:00Z");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = stop.send(());
    };
    let run = runtime(&scratch, source, false).run(async {
        let _ = stopped.await;
    });

    let (summary, ()) = tokio::time::timeout(LIMIT, async { tokio::join!(run, driver) })
        .await
        .expect("the run did not end");

    let summary = summary.unwrap();
    assert_eq!(
        (summary.triggers_received, summary.triggers_duplicate),
        (2, 0),
        "each slot was sent once"
    );
    assert_eq!(
        scratch.ids(),
        [
            "schedule:sweep:2026-11-02T12:00:00Z",
            "schedule:sweep:2026-11-02T13:00:00Z",
        ]
    );
    let store = scratch.store();
    let first = &store.triggers().unwrap()[0];
    assert_eq!(first.source, "schedule");
    assert_eq!(
        (first.actor.kind, first.actor.id.as_str()),
        (ActorKind::System, "sweep")
    );
    assert_eq!(first.addressed, Addressed::Direct);
    assert_eq!(first.text, "Sweep the open issues.");
    assert_eq!(first.at_ms, utc::parse("2026-11-02T12:00:00Z").unwrap());
    assert_eq!(
        first.threads,
        [
            ThreadKey::new("schedule:sweep"),
            ThreadKey::new("github:acme/widgets#1"),
        ],
        "its own Thread first, then the issue it replies on"
    );
    assert_eq!(first.reply_to, std::slice::from_ref(&reply));
    assert_eq!(first.meta["slot"], "2026-11-02T12:00:00Z");
    let sent = scratch.sent();
    assert!(!sent.is_empty());
    assert!(
        sent.iter().all(|m| m.channel == reply),
        "every answer goes where the schedule names"
    );
    assert!(
        sent.iter()
            .any(|m| m.text.starts_with("Done: Sweep the open issues."))
    );
}

#[tokio::test]
async fn a_restart_in_the_same_slot_does_not_fire_it_again() {
    let scratch = Scratch::new("restart");

    let first = start_at(&scratch, vec![hourly(None)], "2026-11-02T12:10:00Z").await;
    let again = start_at(&scratch, vec![hourly(None)], "2026-11-02T12:50:00Z").await;

    assert_eq!((first.triggers_received, first.triggers_duplicate), (1, 0));
    assert_eq!(
        (again.triggers_received, again.triggers_duplicate),
        (0, 1),
        "the slot's id was stored already"
    );
    assert_eq!(scratch.ids(), ["schedule:sweep:2026-11-02T12:00:00Z"]);
    assert_eq!(scratch.store().tickets().unwrap().len(), 1);
    assert!(scratch.sent().is_empty(), "no reply_to, so nobody is told");
}

#[tokio::test]
async fn missed_slots_fire_once_on_start() {
    let scratch = Scratch::new("missed");
    let both = || vec![hourly(None), daily_at_nine()];

    start_at(&scratch, both(), "2026-11-02T12:10:00Z").await;
    let later = start_at(&scratch, both(), "2026-11-05T08:30:00Z").await;

    assert_eq!(
        later.triggers_received, 2,
        "one slot per schedule, however many were missed"
    );
    assert_eq!(
        scratch.ids(),
        [
            "schedule:standup:2026-11-02T09:00:00Z",
            "schedule:standup:2026-11-04T09:00:00Z",
            "schedule:sweep:2026-11-02T12:00:00Z",
            "schedule:sweep:2026-11-05T08:00:00Z",
        ]
    );
}
