//! Review of PR #18, slice "Evals and Router verdicts": the claims that hold,
//! exercised with combinations and orderings.

use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, Candidate, ChannelRef, DecisionKind, Effect, EvalId,
    Plan, Strength, Thread, ThreadKey, TicketId, Trigger, TriggerId, Verdict,
};
use solos_agent_core::{
    AcceptMode, AllowlistPolicy, Engine, Gate, MAX_EXPIRED_EVALS, MemoryRepositories, Policy,
};
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
    fn running(&self, key: &str) -> Option<Vec<String>> {
        self.thread(key)
            .eval
            .map(|e| e.triggers.iter().map(|t| t.to_string()).collect())
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
    fn ticket_from(&mut self, eval: &EvalId, rules: Vec<String>) -> TicketId {
        let d = self.engine.decide(eval, accept(rules)).unwrap();
        self.engine.repos().decisions[&d]
            .ticket
            .clone()
            .expect("a ticket")
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

fn ids(c: &Candidate) -> Vec<String> {
    c.triggers.iter().map(|t| t.id.to_string()).collect()
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

fn accept(rules: Vec<String>) -> Verdict {
    Verdict::Accept(Accept {
        title: "Fix it".into(),
        scope: "Fix it".into(),
        reason: "asked".into(),
        plan: Plan::Session {
            harness: None,
            environment: None,
            method: None,
        },
        budget_microusd: Some(1_000_000),
        acknowledge: Acknowledge::Silent,
        rules,
        repo: None,
        forge: None,
        upstream: None,
    })
}
fn ignore() -> Verdict {
    Verdict::Ignore {
        reason: "no".into(),
    }
}

const M: Addressed = Addressed::Mention;
const U: Addressed = Addressed::Unaddressed;

// ---------- claim 1 ----------

#[test]
fn c1_one_eval_per_thread_and_arrivals_wait() {
    let mut w = World::new(policy());
    w.recv(alice("t1", M, &["A"]));
    w.recv(alice("u1", M, &["B"]));
    let first = w.due();
    assert_eq!(first.len(), 2, "one per thread, both threads");
    let ea = w.thread("A").eval.unwrap().id;
    w.effects();

    // Arrivals on A while its Eval runs, including one naming A and B.
    w.recv(alice("t2", M, &["A"]));
    w.recv(alice("t3", M, &["A", "B"]));
    w.recv(bot("b1", "ci-bot", &["A"]));
    for _ in 0..3 {
        w.advance(1_000);
        assert!(w.due().is_empty(), "no second eval while one runs");
    }
    assert!(candidates(&w.effects()).is_empty());
    assert_eq!(w.running("A").unwrap(), ["t1"]);
    assert_eq!(w.queued("A"), ["t2", "t3", "b1"]);
    assert!(w.queued("B").is_empty(), "t3 queues on its primary only");

    w.engine.decide(&ea, ignore()).unwrap();
    let next = w.one_eval();
    let c = candidates(&w.effects());
    assert_eq!(c.len(), 1);
    assert_eq!(ids(&c[0]), ["t2", "t3"]);
    assert_eq!(w.queued("A"), ["b1"]);
    assert!(w.due().is_empty());

    // A stale eval id is refused once it expired and was replaced
    // (B's Eval expires in the same pass, so two restart).
    w.advance(policy().eval_timeout_ms);
    assert_eq!(w.due().len(), 2);
    assert_ne!(w.thread("A").eval.unwrap().id, next);
    assert!(w.engine.decide(&next, ignore()).is_err());
    assert_eq!(w.running("A").unwrap(), ["t2", "t3"]);
    assert_eq!(w.running("B").unwrap(), ["u1"]);
}

// ---------- claim 2 ----------

#[test]
fn c2_strongest_batch_and_weaker_keep_order_through_expiry_and_ignore() {
    let mut w = World::new(policy()); // observe
    w.recv(bot("b1", "ci-bot", &["A"])); // inferred
    w.recv(alice("a1", U, &["A"])); // inferred (no tickets)
    w.recv(alice("t1", M, &["A"])); // explicit
    w.recv(person("m1", "mallory", M, &["A"])); // rejected
    w.recv(bot("b2", "ci-bot", &["A"])); // inferred
    w.recv(system("s1", &["A"])); // explicit
    w.recv(person("a2", "bob", U, &["A"])); // inferred
    w.one_eval();
    let c = candidates(&w.effects());
    assert_eq!(ids(&c[0]), ["t1", "s1"]);
    assert_eq!(c[0].strength, Strength::Explicit);
    assert_eq!(w.thread("A").eval.unwrap().primary, TriggerId::new("t1"));
    assert_eq!(w.queued("A"), ["b1", "a1", "b2", "a2"]);

    w.recv(bot("b3", "ci-bot", &["A"]));
    w.advance(policy().eval_timeout_ms);
    let e2 = w.one_eval();
    assert_eq!(ids(&candidates(&w.effects())[0]), ["t1", "s1"]);
    assert_eq!(w.queued("A"), ["b1", "a1", "b2", "a2", "b3"]);

    w.engine.decide(&e2, ignore()).unwrap();
    w.one_eval();
    let c = candidates(&w.effects());
    assert_eq!(ids(&c[0]), ["b1", "a1", "b2", "a2", "b3"]);
    assert_eq!(c[0].strength, Strength::Inferred);
}

// ---------- claim 3 ----------

#[test]
fn c3_rejected_triggers_never_reach_the_router_or_ticket_threads() {
    let mut w = World::new(policy());
    w.recv(person("m1", "mallory", M, &["A", "evil1"]));
    w.recv(bot("e1", "evil-bot", &["A", "evil2"]));
    w.recv(alice("t1", M, &["A", "side"]));
    let eval = w.one_eval();
    let c = candidates(&w.effects());
    assert_eq!(c.len(), 1);
    assert_eq!(ids(&c[0]), ["t1"]);
    assert_eq!(w.running("A").unwrap(), ["t1"]);
    let gate: Vec<_> = w
        .engine
        .repos()
        .decisions
        .values()
        .filter(|d| d.eval.is_none())
        .cloned()
        .collect();
    assert_eq!(gate.len(), 1);
    assert_eq!(gate[0].kind, DecisionKind::Ignore);
    assert_eq!(
        gate[0].triggers,
        [TriggerId::new("m1"), TriggerId::new("e1")]
    );
    assert!(
        gate[0].reason.contains("m1: not_listed"),
        "{}",
        gate[0].reason
    );
    assert!(
        gate[0].reason.contains("e1: bot_not_listed"),
        "{}",
        gate[0].reason
    );

    // A rejected arrival during the Eval is recorded and kept out too.
    w.recv(bot("e2", "evil-bot", &["A", "evil3"]));
    let ticket = w.ticket_from(&eval, vec![]);
    let t = &w.engine.repos().tickets[&ticket];
    let threads: Vec<_> = t.threads.iter().map(|k| k.to_string()).collect();
    assert_eq!(threads, ["A", "side"]);
    assert!(w.due().is_empty(), "e2 alone starts nothing");
    assert!(candidates(&w.effects()).is_empty());
    let all: Vec<_> = w.engine.repos().decisions.values().cloned().collect();
    assert!(all.iter().any(|d| d.kind == DecisionKind::Ignore
        && d.triggers == [TriggerId::new("e2")]
        && d.reason.contains("e2: bot_not_listed")));
    for evil in ["evil1", "evil2", "evil3"] {
        let sub = w
            .engine
            .repos()
            .threads
            .get(&ThreadKey::new(evil))
            .map(|t| t.subscriptions.len())
            .unwrap_or(0);
        assert_eq!(sub, 0, "{evil}");
    }
}

#[test]
fn c3_off_mode_and_requeue_after_policy_change_are_gated_again() {
    let mut w = World::new(AllowlistPolicy {
        accept_mode: AcceptMode::Off,
        ..policy()
    });
    w.recv(alice("a1", U, &["A"]));
    w.recv(bot("b1", "ci-bot", &["A"]));
    assert!(w.due().is_empty());
    assert!(candidates(&w.effects()).is_empty());
    let d: Vec<_> = w.engine.repos().decisions.values().cloned().collect();
    assert_eq!(d.len(), 1);
    assert!(d[0].reason.contains("a1: accept_mode_off"));
    assert!(d[0].reason.contains("b1: accept_mode_off"));

    // Admitted at start, rejected after the policy drops alice; the expiry
    // requeue is gated again and nothing reaches the Router.
    let mut w = World::new(policy());
    w.recv(alice("t1", M, &["A"]));
    w.one_eval();
    w.effects();
    w.engine.set_policy(AllowlistPolicy {
        people: Default::default(),
        ..policy()
    });
    w.advance(policy().eval_timeout_ms);
    assert!(w.due().is_empty());
    assert!(candidates(&w.effects()).is_empty());
    let gate = w
        .engine
        .repos()
        .decisions
        .values()
        .find(|d| d.eval.is_none())
        .cloned()
        .unwrap();
    assert_eq!(gate.triggers, [TriggerId::new("t1")]);
    assert!(gate.reason.contains("t1: not_listed"));
}

// ---------- claim 4 ----------

#[test]
fn c4_two_requeues_then_unavailable_and_a_decide_resets_the_streak() {
    let timeout = policy().eval_timeout_ms;
    let mut w = World::new(policy());
    w.recv(alice("t1", M, &["A"]));
    w.one_eval();
    w.advance(timeout - 1);
    assert!(w.due().is_empty(), "not expired before the timeout");
    w.advance(1);
    w.one_eval(); // expiry 1 -> requeued
    w.recv(alice("t2", M, &["A"]));
    w.advance(timeout);
    w.one_eval(); // expiry 2 -> requeued; t2 joins
    assert_eq!(w.running("A").unwrap(), ["t1", "t2"]);
    w.recv(bot("b1", "ci-bot", &["A"]));
    w.advance(timeout);
    let after = w.due(); // expiry 3 -> unavailable; b1 starts its own eval
    assert_eq!(after.len(), 1);
    assert_eq!(w.running("A").unwrap(), ["b1"]);
    let un: Vec<_> = w
        .engine
        .repos()
        .decisions
        .values()
        .filter(|d| d.kind == DecisionKind::Unavailable)
        .cloned()
        .collect();
    assert_eq!(un.len(), 1);
    assert_eq!(un[0].triggers, [TriggerId::new("t1"), TriggerId::new("t2")]);
    assert_eq!(MAX_EXPIRED_EVALS, 3);

    // A decided Eval between expiries resets the streak.
    let mut w = World::new(policy());
    w.recv(alice("t1", M, &["A"]));
    w.one_eval();
    w.advance(timeout);
    w.one_eval();
    w.advance(timeout);
    let e3 = w.one_eval();
    w.engine.decide(&e3, ignore()).unwrap();
    w.recv(alice("t2", M, &["A"]));
    w.one_eval();
    w.advance(timeout);
    assert_eq!(w.due().len(), 1, "requeued after a decide");
    assert_eq!(w.engine.repos().decisions.len(), 1, "only the ignore");
}

// ---------- claim 5 ----------

#[test]
fn c5_any_verdict_after_a_version_change_is_unavailable_and_requeued() {
    for kind in ["ignore", "accept", "stop"] {
        let mut w = World::new(policy());
        // A ticket on A for the stop case.
        w.recv(alice("t0", M, &["A"]));
        let e0 = w.one_eval();
        let ticket = w.ticket_from(&e0, vec![]);
        w.recv(system("s1", &["A"]));
        w.recv(bot("b1", "ci-bot", &["A"]));
        let eval = w.one_eval();
        w.recv(system("s2", &["A"]));
        w.engine.set_policy(AllowlistPolicy {
            bots: ["github:ci-bot".to_owned(), "github:other".to_owned()].into(),
            ..policy()
        });
        let verdict = match kind {
            "ignore" => ignore(),
            "accept" => accept(vec![]),
            _ => Verdict::Stop {
                ticket: ticket.clone(),
                reason: "stop".into(),
            },
        };
        let tickets = w.engine.repos().tickets.len();
        let d = w.engine.decide(&eval, verdict).unwrap();
        let rec = w.engine.repos().decisions[&d].clone();
        assert_eq!(rec.kind, DecisionKind::Unavailable, "{kind}");
        assert_eq!(w.engine.repos().tickets.len(), tickets, "{kind}");
        assert!(w.engine.repos().tickets[&ticket].ended.is_none(), "{kind}");
        assert_eq!(w.queued("A"), ["s1", "b1", "s2"], "{kind}");
        assert!(w.thread("A").eval.is_none());
        w.effects();
        w.one_eval();
        assert_eq!(ids(&candidates(&w.effects())[0]), ["s1", "s2"], "{kind}");
    }

    // Pause, approvers and timeout do not change the version.
    let mut w = World::new(policy());
    w.recv(alice("t1", M, &["A"]));
    let e = w.one_eval();
    w.engine.set_policy(AllowlistPolicy {
        approvers: Default::default(),
        eval_timeout_ms: 5,
        paused: true,
        ..policy()
    });
    let d = w.engine.decide(&e, ignore()).unwrap();
    assert_eq!(w.engine.repos().decisions[&d].kind, DecisionKind::Ignore);
}

// ---------- claim 6 ----------

#[test]
fn c6_only_explicit_evals_stop_and_only_subscribed_tickets() {
    let mut w = World::new(AllowlistPolicy {
        accept_mode: AcceptMode::Auto,
        ..policy()
    });
    w.recv(alice("t1", M, &["A"]));
    let e = w.one_eval();
    let ta = w.ticket_from(&e, vec![]);
    w.recv(alice("t2", M, &["B", "C"]));
    let e = w.one_eval();
    let tb = w.ticket_from(&e, vec![]);
    w.recv(alice("t3", M, &["D"]));
    let e = w.one_eval();
    let td = w.ticket_from(&e, vec![]);
    let stop = |t: &TicketId| Verdict::Stop {
        ticket: t.clone(),
        reason: "stop".into(),
    };

    // Inferred Evals (listed bot, unlisted person on a ticketed thread).
    for t in [
        bot("b1", "ci-bot", &["A"]),
        person("m1", "mallory", M, &["A"]),
    ] {
        w.recv(t);
        let e = w.one_eval();
        assert_eq!(w.thread("A").eval.unwrap().strength, Strength::Inferred);
        assert!(w.engine.decide(&e, stop(&ta)).is_err());
        assert!(w.engine.repos().tickets[&ta].ended.is_none());
        assert!(w.running("A").is_some(), "refusal commits nothing");
        w.engine.decide(&e, ignore()).unwrap();
    }

    // Explicit Eval on A cannot stop tickets on B/C or D.
    w.recv(system("s1", &["A"]));
    let e = w.one_eval();
    assert!(w.engine.decide(&e, stop(&tb)).is_err());
    assert!(w.engine.decide(&e, stop(&td)).is_err());
    w.engine.decide(&e, stop(&ta)).unwrap();
    assert!(w.engine.repos().tickets[&ta].stopped_by_person());

    // Explicit Eval on C (tb's secondary thread) stops tb.
    w.recv(system("s2", &["C"]));
    let e = w.one_eval();
    w.engine.decide(&e, stop(&tb)).unwrap();
    assert!(w.engine.repos().tickets[&tb].stopped_by_person());
    assert!(w.engine.repos().tickets[&td].ended.is_none());
}

// ---------- claim 7 ----------

#[test]
fn c7_inferred_accept_matrix() {
    let rule_sets: [&[&str]; 5] = [
        &[],
        &["reproduce-bugs"],
        &["reproduce-bugs", "other"],
        &["other"],
        &["reproduce-bugs", "reproduce-bugs"],
    ];
    for mode in [AcceptMode::Observe, AcceptMode::Auto] {
        for rules in rule_sets {
            for source in ["bot", "person"] {
                let mut w = World::new(AllowlistPolicy {
                    accept_mode: mode,
                    ..policy()
                });
                if source == "bot" {
                    w.recv(bot("x", "ci-bot", &["A"]));
                } else {
                    w.recv(alice("x", U, &["A"]));
                }
                let e = w.one_eval();
                assert_eq!(w.thread("A").eval.unwrap().strength, Strength::Inferred);
                let rules: Vec<String> = rules.iter().map(|r| r.to_string()).collect();
                let d = w.engine.decide(&e, accept(rules.clone())).unwrap();
                let rec = w.engine.repos().decisions[&d].clone();
                let creates = mode == AcceptMode::Auto
                    && !rules.is_empty()
                    && rules.iter().all(|r| r == "reproduce-bugs");
                let label = format!("{mode:?} {rules:?} {source}");
                assert_eq!(
                    w.engine.repos().tickets.len(),
                    usize::from(creates),
                    "{label}"
                );
                if creates {
                    assert_eq!(rec.kind, DecisionKind::Accept, "{label}");
                    assert!(!rec.would_accept, "{label}");
                } else {
                    assert_eq!(rec.kind, DecisionKind::Ignore, "{label}");
                    assert!(rec.would_accept, "{label}");
                }
            }
        }
    }
}

// ---------- claim 8 ----------

#[test]
fn c8_gate_table() {
    let addressings = [
        Addressed::Mention,
        Addressed::Direct,
        Addressed::Unaddressed,
    ];
    for mode in [AcceptMode::Off, AcceptMode::Observe, AcceptMode::Auto] {
        let p = AllowlistPolicy {
            accept_mode: mode,
            ..policy()
        };
        for has_tickets in [false, true] {
            for who in ["alice", "mallory"] {
                for addressed in addressings {
                    let listed = who == "alice";
                    let got = p.gate(&person("x", who, addressed, &["A"]), has_tickets);
                    let want = if listed && (addressed.is_addressed() || has_tickets) {
                        Some(Strength::Explicit)
                    } else if (!listed && !has_tickets) || mode == AcceptMode::Off {
                        None
                    } else {
                        Some(Strength::Inferred)
                    };
                    let label = format!("{mode:?} tickets={has_tickets} {who} {addressed:?}");
                    match (want, &got) {
                        (Some(s), Gate::Admit { strength }) => assert_eq!(*strength, s, "{label}"),
                        (None, Gate::Reject { .. }) => {}
                        _ => panic!("{label}: got {got:?}, want {want:?}"),
                    }
                }
            }
            for name in ["ci-bot", "evil-bot"] {
                let got = p.gate(&bot("x", name, &["A"]), has_tickets);
                let want = name == "ci-bot" && mode != AcceptMode::Off;
                assert_eq!(
                    got == Gate::Admit {
                        strength: Strength::Inferred
                    },
                    want,
                    "{mode:?} {name} {got:?}"
                );
            }
            for addressed in addressings {
                let mut s = system("x", &["A"]);
                s.addressed = addressed;
                assert_eq!(
                    p.gate(&s, has_tickets),
                    Gate::Admit {
                        strength: Strength::Explicit
                    }
                );
            }
        }
    }
}
