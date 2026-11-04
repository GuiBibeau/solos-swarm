//! Review of PR #18, slice "Evals and Router verdicts": repros. Each test
//! asserts what the claim requires and fails at 25fc138.

use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, Candidate, ChannelRef, DecisionKind, Effect, EvalId,
    Plan, Thread, ThreadKey, TicketId, Trigger, TriggerId, Verdict,
};
use solos_agent_core::{AcceptMode, AllowlistPolicy, Engine, MemoryRepositories, Policy};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

type TestEngine = Engine<MemoryRepositories, AllowlistPolicy, Box<dyn Fn() -> u64>>;

struct World {
    engine: TestEngine,
    now: Rc<Cell<u64>>,
}

impl World {
    fn new(policy: AllowlistPolicy) -> Self {
        let now = Rc::new(Cell::new(1_000));
        let clock = {
            let now = now.clone();
            Box::new(move || now.get()) as Box<dyn Fn() -> u64>
        };
        Self {
            engine: Engine::new(MemoryRepositories::default(), policy, clock),
            now,
        }
    }
    fn effects(&mut self) -> Vec<Effect> {
        self.engine.repos_mut().take_effects()
    }
    fn advance(&self, ms: u64) {
        self.now.set(self.now.get() + ms);
    }
    fn thread(&self, key: &str) -> Thread {
        self.engine.repos().threads[&ThreadKey::new(key)].clone()
    }
    fn queued(&self, key: &str) -> Vec<String> {
        self.thread(key)
            .queued
            .iter()
            .map(|t| t.to_string())
            .collect()
    }
    fn recv(&mut self, trigger: Trigger) {
        self.engine.receive(trigger).unwrap();
    }
    fn due(&mut self) -> Vec<EvalId> {
        self.engine.evaluate_due().unwrap()
    }
    fn one_eval(&mut self) -> EvalId {
        let started = self.due();
        assert_eq!(started.len(), 1, "one eval starts: {started:?}");
        started[0].clone()
    }
    fn ticket_from(&mut self, eval: &EvalId) -> TicketId {
        let d = self.engine.decide(eval, accept()).unwrap();
        self.engine.repos().decisions[&d]
            .ticket
            .clone()
            .expect("a ticket")
    }
    /// Two open Tickets subscribed to thread A: one from alice, one from a
    /// schedule (a system actor's Trigger always queues for an Eval).
    fn two_tickets_on_a(&mut self) -> (TicketId, TicketId) {
        self.recv(alice("t1", Addressed::Mention, &["A"]));
        let e = self.one_eval();
        let t1 = self.ticket_from(&e);
        self.recv(system("s1", &["A"]));
        let e = self.one_eval();
        let t2 = self.ticket_from(&e);
        self.effects();
        (t1, t2)
    }
}

fn candidates(effects: &[Effect]) -> Vec<Candidate> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Assess(c) => Some(c.clone()),
            _ => None,
        })
        .collect()
}

fn policy() -> AllowlistPolicy {
    AllowlistPolicy {
        people: ["slack:alice".to_owned(), "slack:bob".to_owned()].into(),
        bots: ["github:ci-bot".to_owned()].into(),
        approvers: ["slack:alice".to_owned()].into(),
        rules: ["reproduce-bugs".to_owned()].into(),
        ..AllowlistPolicy::default()
    }
}

fn trig(id: &str, source: &str, actor: Actor, addressed: Addressed, threads: &[&str]) -> Trigger {
    Trigger {
        id: TriggerId::new(id),
        source: source.into(),
        at_ms: 1_000,
        actor,
        addressed,
        text: id.into(),
        threads: threads.iter().map(|k| ThreadKey::new(*k)).collect(),
        reply_to: vec![ChannelRef {
            source: source.into(),
            address: BTreeMap::from([("thread".to_owned(), threads[0].to_owned())]),
        }],
        meta: serde_json::Value::Null,
    }
}
fn person(id: &str, who: &str, addressed: Addressed, threads: &[&str]) -> Trigger {
    trig(id, "slack", Actor::person(who), addressed, threads)
}
fn alice(id: &str, addressed: Addressed, threads: &[&str]) -> Trigger {
    person(id, "alice", addressed, threads)
}
fn bot(id: &str, who: &str, threads: &[&str]) -> Trigger {
    trig(
        id,
        "github",
        Actor::bot(who),
        Addressed::Unaddressed,
        threads,
    )
}
fn system(id: &str, threads: &[&str]) -> Trigger {
    trig(
        id,
        "schedule",
        Actor::system("cron"),
        Addressed::Unaddressed,
        threads,
    )
}
fn accept() -> Verdict {
    Verdict::Accept(Accept {
        title: "Fix it".into(),
        scope: "Fix it".into(),
        reason: "asked".into(),
        plan: Plan::Session {
            harness: None,
            environment: None,
            method: None,
        },
        budget_microusd: 1_000_000,
        acknowledge: Acknowledge::Silent,
        rules: Vec::new(),
        repo: None,
    })
}
fn ignore() -> Verdict {
    Verdict::Ignore {
        reason: "no".into(),
    }
}

// ---------- claim 4 ----------

/// Two expiries, then the Router answers in time but the Policy changed:
/// `decide` records `Unavailable` and requeues. That Eval did not expire,
/// yet the streak is not reset, so the very next expiry (the first for the
/// new Eval) records `Unavailable` and drops the batch instead of requeuing.
#[test]
fn i1_policy_change_decide_does_not_reset_the_expiry_streak() {
    let timeout = policy().eval_timeout_ms;
    let mut w = World::new(policy());
    w.recv(alice("t1", Addressed::Mention, &["A"]));
    w.one_eval(); // E1
    w.advance(timeout);
    w.one_eval(); // E1 expired (1), E2 started
    w.advance(timeout);
    let e3 = w.one_eval(); // E2 expired (2), E3 started
    w.engine.set_policy(AllowlistPolicy {
        accept_mode: AcceptMode::Auto,
        ..policy()
    });
    let d = w.engine.decide(&e3, ignore()).unwrap(); // decided in time
    assert_eq!(
        w.engine.repos().decisions[&d].kind,
        DecisionKind::Unavailable
    );
    w.one_eval(); // E4 started on the requeued batch
    println!(
        "after policy-change decide: consecutive_expiries={}",
        w.thread("A").consecutive_expiries
    );
    w.advance(timeout);
    let restarted = w.due(); // E4's first expiry
    let decisions: Vec<_> = w
        .engine
        .repos()
        .decisions
        .values()
        .map(|d| (d.kind, d.reason.clone()))
        .collect();
    assert_eq!(
        restarted.len(),
        1,
        "E4 expired once and must be requeued; got started={restarted:?}, \
         queued={:?}, decisions={decisions:?}",
        w.queued("A")
    );
}

/// Variant: two expiries, then the requeued batch is all gate-rejected (the
/// Policy dropped alice), so no Eval follows and the streak stays at 2. A
/// later, unrelated batch is dropped on its own first expiry.
#[test]
fn i1b_a_new_batch_inherits_an_old_expiry_streak() {
    let timeout = policy().eval_timeout_ms;
    let mut w = World::new(policy());
    w.recv(alice("t1", Addressed::Mention, &["A"]));
    w.one_eval();
    w.advance(timeout);
    w.one_eval(); // expiry 1
    w.engine.set_policy(AllowlistPolicy {
        people: ["slack:bob".to_owned()].into(),
        ..policy()
    });
    w.advance(timeout);
    assert!(w.due().is_empty()); // expiry 2; t1 now rejected by the gate
    w.advance(10 * timeout);
    w.recv(person("n1", "bob", Addressed::Mention, &["A"]));
    w.one_eval(); // a new batch, never requeued
    w.advance(timeout);
    let restarted = w.due();
    let dropped: Vec<_> = w
        .engine
        .repos()
        .decisions
        .values()
        .filter(|d| d.kind == DecisionKind::Unavailable)
        .map(|d| d.triggers.clone())
        .collect();
    assert_eq!(
        restarted.len(),
        1,
        "n1's Eval expired once and must be requeued; got started={restarted:?}, \
         unavailable={dropped:?}"
    );
}

// ---------- claim 2 ----------

/// Decision records Y's rejection.
#[test]
fn i3_a_second_gate_decision_overwrites_the_first() {
    let mut w = World::new(policy());
    let (t1, t2) = w.two_tickets_on_a();
    // alice's message reaches both Tickets; both later hand it back.
    w.recv(alice("x", Addressed::Unaddressed, &["A"]));
    assert!(
        w.engine
            .handback(&t1, &TriggerId::new("x"), "not mine")
            .unwrap()
    );
    w.recv(bot("y", "evil-bot", &["A"]));
    assert_eq!(w.queued("A"), ["x", "y"]);
    w.engine.set_policy(AllowlistPolicy {
        people: ["slack:bob".to_owned()].into(),
        accept_mode: AcceptMode::Off,
        ..policy()
    });
    assert!(w.due().is_empty());
    let first: Vec<_> = w
        .engine
        .repos()
        .decisions
        .values()
        .filter(|d| d.eval.is_none())
        .map(|d| (d.id.to_string(), d.triggers.clone(), d.reason.clone()))
        .collect();
    println!("first gate decision: {first:?}");
    assert!(
        !w.engine
            .handback(&t2, &TriggerId::new("x"), "not mine")
            .unwrap(),
        "a Trigger is judged once after a handback"
    );
    assert!(w.due().is_empty());
    let gate: Vec<_> = w
        .engine
        .repos()
        .decisions
        .values()
        .filter(|d| d.eval.is_none())
        .map(|d| (d.id.to_string(), d.triggers.clone(), d.reason.clone()))
        .collect();
    let thread_decisions = w.thread("A").decisions;
    let y_recorded = w
        .engine
        .repos()
        .decisions
        .values()
        .any(|d| d.triggers.contains(&TriggerId::new("y")));
    assert!(
        y_recorded,
        "no Decision records y's rejection; gate decisions now={gate:?}; \
         thread A decision ids={thread_decisions:?}"
    );
}

// ---------- claim 5 ----------

/// AllowlistPolicy::version joins each set with "\n" before hashing, so two
/// different `people` sets can share a version. The change below drops alice
/// from the allowlist during her Eval, yet decide accepts.
#[test]
fn i4_a_policy_change_with_the_same_version_is_not_detected() {
    let before = AllowlistPolicy {
        people: ["slack:alice".to_owned(), "slack:bob".to_owned()].into(),
        ..policy()
    };
    let after = AllowlistPolicy {
        people: ["slack:alice\nslack:bob".to_owned()].into(),
        ..policy()
    };
    let mut w = World::new(before.clone());
    w.recv(alice("t1", Addressed::Mention, &["A"]));
    let e = w.one_eval();
    let gate_after = after.gate(&alice("probe", Addressed::Mention, &["Z"]), false);
    println!(
        "version before={} after={}; alice under the new policy: {gate_after:?}",
        before.version(),
        after.version()
    );
    w.engine.set_policy(after);
    let d = w.engine.decide(&e, accept()).unwrap();
    let rec = w.engine.repos().decisions[&d].clone();
    assert_eq!(
        rec.kind,
        DecisionKind::Unavailable,
        "decide recorded {:?} and created {} ticket(s)",
        rec.kind,
        w.engine.repos().tickets.len()
    );
}

// ---------- adjacent: one Trigger judged by two Evals ----------

/// Two Tickets on A each hand back the same alice message. It is queued once
/// per handback, so the Router judges it twice: first ignored, then
/// accepted. (Handed back together, it appears twice in one Candidate.)
#[test]
fn i5_a_trigger_handed_back_by_two_tickets_is_judged_twice() {
    let mut w = World::new(policy());
    let (t1, t2) = w.two_tickets_on_a();
    w.recv(alice("x", Addressed::Unaddressed, &["A"]));
    w.engine
        .handback(&t1, &TriggerId::new("x"), "not mine")
        .unwrap();
    let e1 = w.one_eval();
    w.engine
        .handback(&t2, &TriggerId::new("x"), "not mine")
        .unwrap();
    w.engine.decide(&e1, ignore()).unwrap();
    let e2 = w.due();
    let judged = 1 + e2.len();
    if let Some(e2) = e2.first() {
        w.engine.decide(e2, accept()).unwrap();
    }
    let tickets_from_x = w
        .engine
        .repos()
        .tickets
        .values()
        .filter(|t| t.origin == TriggerId::new("x"))
        .count();

    let mut v = World::new(policy());
    let (t1, t2) = v.two_tickets_on_a();
    v.recv(alice("x", Addressed::Unaddressed, &["A"]));
    v.engine
        .handback(&t1, &TriggerId::new("x"), "not mine")
        .unwrap();
    v.engine
        .handback(&t2, &TriggerId::new("x"), "not mine")
        .unwrap();
    v.one_eval();
    let batch: Vec<_> = candidates(&v.effects())
        .iter()
        .flat_map(|c| c.triggers.iter().map(|t| t.id.to_string()))
        .collect();
    assert!(
        judged == 1 && batch == ["x"],
        "x judged by {judged} Evals (second accepted: {tickets_from_x} ticket from x); \
         handed back together, one Candidate holds {batch:?}"
    );
}
