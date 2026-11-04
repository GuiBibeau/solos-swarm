#![allow(dead_code)]

use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, AssignmentId, ChannelRef, Decision, Effect, Event, Plan,
    Thread, ThreadKey, Ticket, TicketId, Trigger, TriggerId, Verdict,
};
use solos_agent_core::trail::{
    Digest, PageSeen, STEP_VERSION, Signature, SignedStep, StepBody, StepOutcome, TrailAction,
    TrailCursor, TrailKey,
};
use solos_agent_core::{AllowlistPolicy, Engine, MemoryRepositories, Repositories};
use solos_agent_store::SqliteStore;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

pub type Clock = Box<dyn Fn() -> u64>;
pub type SqlEngine = Engine<SqliteStore, AllowlistPolicy, Clock>;
pub type MemEngine = Engine<MemoryRepositories, AllowlistPolicy, Clock>;

pub fn clock(now: &Rc<Cell<u64>>) -> Clock {
    let now = now.clone();
    Box::new(move || now.get())
}

/// A database file in its own temporary directory, removed on drop.
pub struct TempDb {
    pub path: PathBuf,
    dir: PathBuf,
}

impl TempDb {
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "solos-agent-store-{name}-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            path: dir.join("agent.db"),
            dir,
        }
    }
    /// A second, raw connection, as an operator or an attacker would have.
    pub fn raw(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.path).unwrap()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Everything a store holds, in the order both stores list it.
#[derive(Debug, PartialEq)]
pub struct Contents {
    pub triggers: Vec<Trigger>,
    pub threads: Vec<Thread>,
    pub tickets: Vec<Ticket>,
    pub decisions: Vec<Decision>,
    pub trails: Vec<TrailCursor>,
    pub effects: Vec<Effect>,
    pub events: Vec<Event>,
}

pub fn sqlite_contents(s: &SqliteStore) -> Contents {
    Contents {
        triggers: s.triggers().unwrap(),
        threads: s.threads().unwrap(),
        tickets: s.tickets().unwrap(),
        decisions: s.decisions().unwrap(),
        trails: s.trails().unwrap(),
        effects: s.effects().unwrap().into_iter().map(|q| q.effect).collect(),
        events: s.events().unwrap(),
    }
}

pub fn memory_contents(m: &MemoryRepositories) -> Contents {
    Contents {
        triggers: m.triggers.values().cloned().collect(),
        threads: m.threads.values().cloned().collect(),
        tickets: m.tickets.values().cloned().collect(),
        decisions: m.decisions.values().cloned().collect(),
        trails: m.trails.values().cloned().collect(),
        effects: m.effects.clone(),
        events: m.events.clone(),
    }
}

/// The first place two snapshots differ, for a readable failure.
pub fn first_difference(a: &Contents, b: &Contents) -> Option<String> {
    fn list<T: PartialEq + std::fmt::Debug>(name: &str, a: &[T], b: &[T]) -> Option<String> {
        if a.len() != b.len() {
            return Some(format!("{name}: {} vs {} items", a.len(), b.len()));
        }
        let i = a.iter().zip(b).position(|(x, y)| x != y)?;
        Some(format!("{name}[{i}]:\n  {:?}\n  {:?}", a[i], b[i]))
    }
    list("triggers", &a.triggers, &b.triggers)
        .or_else(|| list("threads", &a.threads, &b.threads))
        .or_else(|| list("tickets", &a.tickets, &b.tickets))
        .or_else(|| list("decisions", &a.decisions, &b.decisions))
        .or_else(|| list("trails", &a.trails, &b.trails))
        .or_else(|| list("effects", &a.effects, &b.effects))
        .or_else(|| list("events", &a.events, &b.events))
}

/// What the port answers for the queries the Engine makes outside a command.
pub fn port_view(r: &impl Repositories, threads: &[Thread]) -> String {
    let evals: Vec<_> = threads
        .iter()
        .filter_map(|t| t.eval.as_ref())
        .map(|e| (e.id.clone(), r.thread_of_eval(&e.id)))
        .collect();
    format!("active {:?}; evals {evals:?}", r.active_threads())
}

pub fn page_seen() -> StepOutcome {
    StepOutcome::Page(PageSeen {
        url: "https://example.com/".into(),
        status: Some(200),
        title: "Example".into(),
        snapshot: Digest::of(b"snapshot"),
        view: Digest::of(b"view"),
        target: None,
        filled: Vec::new(),
        keyed: Vec::new(),
    })
}

pub fn signed_step(
    ticket: &TicketId,
    assignment: &AssignmentId,
    n: u32,
    action: TrailAction,
    outcome: StepOutcome,
) -> SignedStep {
    SignedStep {
        body: StepBody {
            v: STEP_VERSION,
            ticket: ticket.clone(),
            assignment: assignment.clone(),
            n,
            environment: "local".into(),
            session: "7a3c0f9e12d45b60".into(),
            session_step: n.saturating_sub(1),
            at_ms: 1_000,
            provenance: None,
            action,
            outcome,
        },
        signer: TrailKey::try_from(format!("ed25519:{}", "9c".repeat(32))).unwrap(),
        signature: Signature::try_from("ab".repeat(64)).unwrap(),
    }
}

pub fn policy() -> AllowlistPolicy {
    AllowlistPolicy {
        people: ["slack:alice".to_owned()].into(),
        bots: ["github:ci-bot".to_owned()].into(),
        approvers: ["slack:alice".to_owned()].into(),
        ..AllowlistPolicy::default()
    }
}

pub fn slack(id: &str, actor: Actor, thread: &str, text: &str) -> Trigger {
    Trigger {
        id: TriggerId::new(id),
        source: "slack".into(),
        at_ms: 1_000,
        actor,
        addressed: Addressed::Mention,
        text: text.into(),
        threads: vec![ThreadKey::new(thread)],
        reply_to: vec![channel(thread)],
        meta: serde_json::json!({"ts": "1730000000.000100", "float": 0.1}),
    }
}

pub fn channel(thread: &str) -> ChannelRef {
    ChannelRef {
        source: "slack".into(),
        address: BTreeMap::from([
            ("channel".to_owned(), "C1".to_owned()),
            ("thread_ts".to_owned(), thread.to_owned()),
        ]),
    }
}

pub fn accept(title: &str, plan: Plan, budget: u64) -> Verdict {
    Verdict::Accept(Accept {
        title: title.into(),
        scope: format!("scope of {title}"),
        reason: "alice asked".into(),
        plan,
        budget_microusd: budget,
        acknowledge: Acknowledge::Standard,
        rules: Vec::new(),
        repo: None,
    })
}
