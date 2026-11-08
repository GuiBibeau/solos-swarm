//! Verification of the intake and follow-up claims of PR #18 at 25fc138.
//! Tests named `issue_*` encode a claim and fail when the claim does not hold.

use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, AssignmentId, ChannelRef, DecisionKind, Effect, EvalId,
    InputBody, Plan, ReportIn, ReportStatus, Stage, ThreadKey, TicketId, Trigger, TriggerId,
    Verdict,
};
use solos_agent_core::{AcceptMode, AllowlistPolicy, Engine, Intake, MemoryRepositories};
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
    fn tick(&self) {
        self.now.set(self.now.get() + 1);
    }
    fn snapshot(&self) -> String {
        format!("{:?}", self.engine.repos())
    }
    fn one_eval(&mut self) -> EvalId {
        let started = self.engine.evaluate_due().unwrap();
        assert_eq!(started.len(), 1, "one eval starts: {started:?}");
        started[0].clone()
    }
    /// Accept the running eval, start the Assignment it launches.
    fn accept_and_start(&mut self, eval: &EvalId) -> (TicketId, AssignmentId) {
        self.engine.decide(eval, accept(1_000_000)).unwrap();
        let effects = self.effects();
        let [(ticket, assignment)] = &launches(&effects)[..] else {
            panic!("one launch: {effects:?}")
        };
        self.engine.assignment_started(ticket, assignment).unwrap();
        self.effects();
        (ticket.clone(), assignment.clone())
    }
    /// alice asks on `thread`; the Router accepts; the Assignment runs.
    fn running(&mut self, id: &str, threads: &[&str]) -> (TicketId, AssignmentId) {
        let mut t = msg(id, alice(), Addressed::Mention, threads[0]);
        t.threads = keys(threads);
        self.engine.receive(t).unwrap();
        let eval = self.one_eval();
        self.accept_and_start(&eval)
    }
    fn ticket(&self, id: &TicketId) -> solos_agent_core::model::Ticket {
        self.engine.repos().tickets[id].clone()
    }
    fn queued(&self, thread: &str) -> Vec<String> {
        self.engine
            .repos()
            .threads
            .get(&ThreadKey::new(thread))
            .map(|t| t.queued.iter().map(|id| id.to_string()).collect())
            .unwrap_or_default()
    }
}

fn policy() -> AllowlistPolicy {
    AllowlistPolicy {
        people: ["slack:alice".to_owned(), "github:alice".to_owned()].into(),
        bots: ["github:ci-bot".to_owned()].into(),
        approvers: ["slack:alice".to_owned()].into(),
        rules: ["reproduce-bugs".to_owned()].into(),
        ..AllowlistPolicy::default()
    }
}

fn paused() -> AllowlistPolicy {
    AllowlistPolicy {
        paused: true,
        ..policy()
    }
}

fn alice() -> Actor {
    Actor::person("alice")
}

fn keys(threads: &[&str]) -> Vec<ThreadKey> {
    threads.iter().map(|k| ThreadKey::new(*k)).collect()
}

fn msg(id: &str, actor: Actor, addressed: Addressed, thread: &str) -> Trigger {
    Trigger {
        id: TriggerId::new(id),
        source: "slack".into(),
        at_ms: 1_000,
        actor,
        addressed,
        text: format!("text of {id}"),
        threads: vec![ThreadKey::new(thread)],
        reply_to: vec![ChannelRef {
            source: "slack".into(),
            address: BTreeMap::from([("thread_ts".to_owned(), thread.to_owned())]),
        }],
        meta: serde_json::Value::Null,
    }
}

fn on(mut t: Trigger, threads: &[&str]) -> Trigger {
    t.threads = keys(threads);
    t
}

fn ci(id: &str, threads: &[&str]) -> Trigger {
    let mut t = on(
        msg(id, Actor::bot("ci-bot"), Addressed::Unaddressed, threads[0]),
        threads,
    );
    t.source = "github".into();
    t
}

fn unlisted_bot(id: &str, threads: &[&str]) -> Trigger {
    let mut t = ci(id, threads);
    t.actor = Actor::bot("evil-bot");
    t
}

fn mallory(id: &str, threads: &[&str]) -> Trigger {
    on(
        msg(id, Actor::person("mallory"), Addressed::Mention, threads[0]),
        threads,
    )
}

fn schedule(id: &str, threads: &[&str]) -> Trigger {
    on(
        msg(
            id,
            Actor::system("cron"),
            Addressed::Unaddressed,
            threads[0],
        ),
        threads,
    )
}

fn from_alice(id: &str, threads: &[&str]) -> Trigger {
    on(
        msg(id, alice(), Addressed::Unaddressed, threads[0]),
        threads,
    )
}

fn accept(budget: u64) -> Verdict {
    accept_rules(budget, Vec::new())
}

fn accept_rules(budget: u64, rules: Vec<String>) -> Verdict {
    Verdict::Accept(Accept {
        title: "Fix the flaky test".into(),
        scope: "Reproduce and fix the flake".into(),
        reason: "asked".into(),
        plan: Plan::Session {
            harness: None,
            environment: None,
            method: None,
        },
        budget_microusd: Some(budget),
        acknowledge: Acknowledge::Silent,
        rules,
        repo: None,
        forge: None,
        upstream: None,
    })
}

fn launches(effects: &[Effect]) -> Vec<(TicketId, AssignmentId)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Launch {
                ticket, assignment, ..
            } => Some((ticket.clone(), assignment.clone())),
            _ => None,
        })
        .collect()
}

fn assessed(effects: &[Effect]) -> Vec<Vec<String>> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Assess(c) => Some(c.triggers.iter().map(|t| t.id.to_string()).collect()),
            _ => None,
        })
        .collect()
}

fn report(status: ReportStatus) -> ReportIn {
    ReportIn {
        status,
        text: format!("{status:?}"),
        question: None,
        spent_microusd: 0,
        harvest: None,
    }
}

/// (trigger id, from_allowed_person) of each Trigger Input of a Ticket.
fn trigger_inputs(w: &World, ticket: &TicketId) -> Vec<(String, bool)> {
    w.ticket(ticket)
        .inputs
        .iter()
        .filter_map(|i| match &i.body {
            InputBody::Trigger { trigger } => Some((trigger.to_string(), i.from_allowed_person)),
            _ => None,
        })
        .collect()
}

fn stored(reached: &[&TicketId], queued: bool) -> Intake {
    Intake::Stored {
        reached: reached.iter().map(|t| (*t).clone()).collect(),
        queued,
    }
}

// ---------- 1. a stored Trigger id changes nothing ----------

#[test]
fn c1_a_redelivery_changes_nothing_in_any_state() {
    let mut w = World::new(policy());
    let (ticket, a1) = w.running("t1", &["th1", "th2"]);
    // A follow-up that reached the ticket, a bot that reached and queued,
    // an unlisted person queued on a thread with no ticket.
    let follow = from_alice("t2", &["th1", "th2"]);
    let bot = ci("b1", &["th2", "th1"]);
    let stranger = mallory("m1", &["th3"]);
    for t in [follow.clone(), bot.clone(), stranger.clone()] {
        assert!(matches!(
            w.engine.receive(t).unwrap(),
            Intake::Stored { .. }
        ));
    }
    let before = w.snapshot();
    for t in [follow.clone(), bot.clone(), stranger.clone()] {
        w.tick();
        assert_eq!(w.engine.receive(t).unwrap(), Intake::Duplicate);
    }
    // Same id, different content: still a duplicate.
    let mut forged = follow.clone();
    forged.actor = Actor::bot("evil-bot");
    forged.threads = keys(&["th9", "th1"]);
    assert_eq!(w.engine.receive(forged).unwrap(), Intake::Duplicate);
    // The origin Trigger of the ticket.
    assert_eq!(
        w.engine
            .receive(msg("t1", alice(), Addressed::Mention, "th1"))
            .unwrap(),
        Intake::Duplicate
    );
    assert_eq!(before, w.snapshot(), "a redelivery changed state");

    // After the ticket ends, a redelivered follow-up resumes nothing.
    w.engine
        .report(&ticket, &a1, report(ReportStatus::Ready))
        .unwrap();
    w.effects();
    let before = w.snapshot();
    assert_eq!(w.engine.receive(follow).unwrap(), Intake::Duplicate);
    assert_eq!(before, w.snapshot(), "a redelivery resumed an ended ticket");

    // While paused, too.
    w.engine.set_policy(paused());
    let before = w.snapshot();
    assert_eq!(w.engine.receive(bot).unwrap(), Intake::Duplicate);
    assert_eq!(before, w.snapshot());
}

#[test]
fn c1_a_redelivery_during_an_eval_does_not_rejoin_the_queue() {
    let mut w = World::new(policy());
    let t = msg("t1", alice(), Addressed::Mention, "th1");
    w.engine.receive(t.clone()).unwrap();
    let eval = w.one_eval();
    let before = w.snapshot();
    assert_eq!(w.engine.receive(t.clone()).unwrap(), Intake::Duplicate);
    assert_eq!(before, w.snapshot());
    w.engine
        .decide(&eval, Verdict::Ignore { reason: "x".into() })
        .unwrap();
    assert_eq!(w.engine.receive(t).unwrap(), Intake::Duplicate);
    assert!(w.engine.evaluate_due().unwrap().is_empty(), "judged again");
}

// ---------- 2. allowed person on a live Ticket's Thread ----------

#[test]
fn c2_an_allowed_person_reaches_a_live_ticket_and_is_not_queued() {
    // Running assignment, addressed or not, primary Thread elsewhere, other Source.
    let mut w = World::new(policy());
    let (ticket, _) = w.running("t1", &["th1"]);
    let cases = [
        msg("a1", alice(), Addressed::Unaddressed, "th1"),
        msg("a2", alice(), Addressed::Mention, "th1"),
        from_alice("a3", &["th9", "th1"]),
        {
            let mut t = from_alice("a4", &["pr-7", "th1"]);
            t.source = "github".into();
            t
        },
    ];
    for t in cases {
        let id = t.id.to_string();
        assert_eq!(
            w.engine.receive(t).unwrap(),
            stored(&[&ticket], false),
            "{id}"
        );
        assert!(
            w.queued("th1").is_empty() && w.queued("th9").is_empty() && w.queued("pr-7").is_empty(),
            "{id}"
        );
        assert_eq!(
            trigger_inputs(&w, &ticket).last(),
            Some(&(id.clone(), true))
        );
        let effects = w.effects();
        assert!(
            effects.iter().any(|e| matches!(e, Effect::Deliver { .. })),
            "{id}: {effects:?}"
        );
    }
    assert!(w.engine.evaluate_due().unwrap().is_empty());
}

#[test]
fn c2_holds_for_pending_awaiting_approval_and_paused_tickets() {
    // Pending assignment.
    let mut w = World::new(policy());
    w.engine
        .receive(msg("t1", alice(), Addressed::Mention, "th1"))
        .unwrap();
    let eval = w.one_eval();
    w.engine.decide(&eval, accept(1_000_000)).unwrap();
    let ticket = w.engine.repos().tickets.keys().next().unwrap().clone();
    w.effects();
    assert_eq!(
        w.engine.receive(from_alice("a1", &["th1"])).unwrap(),
        stored(&[&ticket], false)
    );
    assert!(
        launches(&w.effects()).is_empty(),
        "no second launch while pending"
    );

    // Waiting for approval.
    let mut w = World::new(policy());
    w.engine
        .receive(msg("t1", alice(), Addressed::Mention, "th1"))
        .unwrap();
    let eval = w.one_eval();
    w.engine.decide(&eval, accept(50_000_000)).unwrap();
    let ticket = w.engine.repos().tickets.keys().next().unwrap().clone();
    w.effects();
    assert_eq!(
        w.engine.receive(from_alice("a1", &["th1"])).unwrap(),
        stored(&[&ticket], false)
    );
    let effects = w.effects();
    assert!(effects.is_empty(), "no second ask, no launch: {effects:?}");
    assert_eq!(w.ticket(&ticket).stage(), Stage::Approval);

    // Paused, running.
    let mut w = World::new(policy());
    let (ticket, _) = w.running("t1", &["th1"]);
    w.engine.set_policy(paused());
    assert_eq!(
        w.engine.receive(from_alice("a1", &["th1"])).unwrap(),
        stored(&[&ticket], false)
    );
    assert!(w.queued("th1").is_empty());
}

// ---------- 3. bots, CI and unlisted people: context and their own Eval ----------

#[test]
fn c3_automatic_and_unlisted_messages_reach_as_context_and_queue() {
    let mut w = World::new(policy());
    let (ticket, _) = w.running("t1", &["th1"]);
    let cases = [
        ci("b1", &["th1"]),
        unlisted_bot("b2", &["th1"]),
        mallory("m1", &["th1"]),
        {
            let mut t = from_alice("m2", &["th1"]);
            t.source = "discord".into(); // alice is not listed on discord
            t
        },
        ci("b3", &["pr-9", "th1"]), // primary elsewhere
    ];
    for t in cases {
        let id = t.id.to_string();
        let primary = t.threads[0].to_string();
        assert_eq!(
            w.engine.receive(t).unwrap(),
            stored(&[&ticket], true),
            "{id}"
        );
        assert!(w.queued(&primary).contains(&id), "{id} queued on {primary}");
        assert_eq!(trigger_inputs(&w, &ticket).last(), Some(&(id, false)));
    }
    let started = w.engine.evaluate_due().unwrap();
    let effects = w.effects();
    let judged: Vec<_> = assessed(&effects).concat();
    let ignored: Vec<_> = w
        .engine
        .repos()
        .decisions
        .values()
        .flat_map(|d| d.triggers.iter().map(|t| t.to_string()))
        .collect();
    println!("evals={started:?} assessed={judged:?} gate-ignored={ignored:?}");
    for id in ["b1", "b2", "m1", "m2", "b3"] {
        assert!(
            judged.iter().any(|j| j == id) || ignored.iter().any(|j| j == id),
            "{id} never judged"
        );
    }
}

// ---------- 4. several Threads of one Ticket: once ----------

#[test]
fn c4_a_multi_thread_trigger_reaches_a_live_or_resumed_ticket_once() {
    let mut w = World::new(policy());
    let (ticket, a1) = w.running("t1", &["th1", "th2", "th3"]);
    for (id, threads) in [
        ("a1", &["th1", "th2", "th3"][..]),
        ("a2", &["th3", "th1", "th3", "th2"][..]),
    ] {
        w.engine.receive(from_alice(id, threads)).unwrap();
    }
    w.engine.receive(ci("b1", &["th2", "th3", "th1"])).unwrap();
    let ids: Vec<_> = trigger_inputs(&w, &ticket)
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert_eq!(ids, ["a1", "a2", "b1"]);
    assert_eq!(w.queued("th2"), ["b1"], "queued once, on its primary");

    // Resume through several Threads: one Input, one Launch.
    w.engine
        .report(&ticket, &a1, report(ReportStatus::Ready))
        .unwrap();
    w.effects();
    let intake = w
        .engine
        .receive(from_alice("a3", &["th2", "th1", "th3"]))
        .unwrap();
    assert_eq!(intake, stored(&[&ticket], false));
    assert_eq!(launches(&w.effects()).len(), 1);
    let ids: Vec<_> = trigger_inputs(&w, &ticket)
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert_eq!(ids, ["a1", "a2", "b1", "a3"]);
}

/// Two Tickets each hand back the same multi-Thread follow-up while an Eval
/// runs; the Ticket that Eval accepts follows both Threads and gets the
/// Trigger twice.
#[test]
fn issue_c4_a_multi_thread_trigger_reaches_a_new_ticket_twice_via_two_handbacks() {
    let mut w = World::new(policy());
    let (a, _) = w.running("t1", &["th1"]);
    let (b, _) = w.running("t2", &["th2"]);
    // A schedule on both Threads: context for A and B, and an explicit Eval on th1.
    let intake = w.engine.receive(schedule("s1", &["th1", "th2"])).unwrap();
    assert_eq!(intake, stored(&[&a, &b], true));
    let eval = w.one_eval();
    w.effects();
    // alice follows up on both Threads; it reaches A and B, and each hands it back.
    let intake = w.engine.receive(from_alice("t3", &["th1", "th2"])).unwrap();
    assert_eq!(intake, stored(&[&a, &b], false));
    let t3 = TriggerId::new("t3");
    assert!(w.engine.handback(&a, &t3, "not mine").unwrap());
    assert!(
        !w.engine.handback(&b, &t3, "not mine").unwrap(),
        "judged once"
    );
    println!(
        "th1.queued={:?} th2.queued={:?}",
        w.queued("th1"),
        w.queued("th2")
    );
    w.engine.decide(&eval, accept(1_000_000)).unwrap();
    let c = w
        .engine
        .repos()
        .tickets
        .keys()
        .find(|k| **k != a && **k != b)
        .unwrap()
        .clone();
    println!("C threads={:?}", w.ticket(&c).threads);
    let inputs = trigger_inputs(&w, &c);
    println!("C trigger inputs={inputs:?}");
    let n = inputs.iter().filter(|(t, _)| t == "t3").count();
    assert_eq!(n, 1, "t3 on two Threads of C reached C {n} times");
}

// ---------- 5. resuming an ended Ticket; a stopped Ticket hears nothing ----------

fn completed(w: &mut World, threads: &[&str]) -> TicketId {
    let (ticket, a1) = w.running("t1", threads);
    w.engine
        .report(&ticket, &a1, report(ReportStatus::Ready))
        .unwrap();
    w.effects();
    ticket
}

#[test]
fn c5_only_an_allowed_person_resumes() {
    let mut w = World::new(AllowlistPolicy {
        accept_mode: AcceptMode::Auto,
        ..policy()
    });
    let ticket = completed(&mut w, &["th1", "th2"]);
    for t in [
        ci("b1", &["th1"]),
        unlisted_bot("b2", &["th2"]),
        mallory("m1", &["th1", "th2"]),
        schedule("s1", &["th1"]),
        {
            let mut t = from_alice("m2", &["th1"]);
            t.source = "discord".into();
            t
        },
    ] {
        let id = t.id.to_string();
        assert_eq!(w.engine.receive(t).unwrap(), stored(&[], true), "{id}");
        assert!(launches(&w.effects()).is_empty(), "{id} resumed it");
        assert!(w.ticket(&ticket).ended.is_some(), "{id}");
    }
    assert_eq!(
        w.engine.receive(from_alice("a2", &["th2", "th1"])).unwrap(),
        stored(&[&ticket], false)
    );
    assert!(w.ticket(&ticket).ended.is_none());
    assert_eq!(launches(&w.effects()).len(), 1);
}

#[test]
fn c5_no_resume_when_the_mandate_does_not_revalidate() {
    // Explicit: the requester left the allowlist; another listed person writes.
    let mut w = World::new(policy());
    let ticket = completed(&mut w, &["th1"]);
    w.engine.set_policy(AllowlistPolicy {
        people: ["slack:bob".to_owned()].into(),
        ..policy()
    });
    let bob = msg("x1", Actor::person("bob"), Addressed::Unaddressed, "th1");
    assert_eq!(w.engine.receive(bob).unwrap(), stored(&[], true));
    assert!(w.ticket(&ticket).ended.is_some());
    assert!(launches(&w.effects()).is_empty());

    // Inferred: a bot-originated Ticket in auto mode; auto mode is turned off.
    let auto = AllowlistPolicy {
        accept_mode: AcceptMode::Auto,
        ..policy()
    };
    let mut w = World::new(auto.clone());
    w.engine.receive(ci("b1", &["th1"])).unwrap();
    let eval = w.one_eval();
    w.engine
        .decide(
            &eval,
            accept_rules(1_000_000, vec!["reproduce-bugs".into()]),
        )
        .unwrap();
    let effects = w.effects();
    let [(ticket, a1)] = &launches(&effects)[..] else {
        panic!("{effects:?}")
    };
    w.engine.assignment_started(ticket, a1).unwrap();
    w.engine
        .report(ticket, a1, report(ReportStatus::Dropped))
        .unwrap();
    w.effects();
    w.engine.set_policy(policy()); // observe
    assert_eq!(
        w.engine.receive(from_alice("a1", &["th1"])).unwrap(),
        stored(&[], true)
    );
    assert!(
        w.ticket(ticket).ended.is_some(),
        "inferred mandate resumed in observe mode"
    );
    w.engine.set_policy(auto);
    assert_eq!(
        w.engine.receive(from_alice("a2", &["th1"])).unwrap(),
        stored(&[ticket], false)
    );
}

#[test]
fn c5_a_stopped_ticket_receives_nothing_more() {
    for via_router in [false, true] {
        let mut w = World::new(policy());
        let (ticket, a1) = w.running("t1", &["th1", "th2"]);
        if via_router {
            w.engine
                .receive(msg("s", alice(), Addressed::Mention, "th1"))
                .unwrap();
            assert!(
                w.engine
                    .handback(&ticket, &TriggerId::new("s"), "stop")
                    .unwrap()
            );
            let eval = w.one_eval();
            w.engine
                .decide(
                    &eval,
                    Verdict::Stop {
                        ticket: ticket.clone(),
                        reason: "asked".into(),
                    },
                )
                .unwrap();
        } else {
            w.engine
                .stop_ticket(&ticket, "slack", Actor::person("alice"), "no")
                .unwrap();
        }
        w.effects();
        let before = w.ticket(&ticket);
        for t in [
            from_alice("a1", &["th1"]),
            msg("a2", alice(), Addressed::Mention, "th2"),
            from_alice("a3", &["th2", "th1"]),
            ci("b1", &["th1"]),
            mallory("m1", &["th2"]),
            schedule("s1", &["th1"]),
        ] {
            let id = t.id.to_string();
            let intake = w.engine.receive(t).unwrap();
            assert!(
                matches!(&intake, Intake::Stored { reached, queued: true } if reached.is_empty()),
                "{id}: {intake:?}"
            );
        }
        assert!(w.engine.note(&ticket, "fyi".into()).is_err());
        assert!(w.engine.approve(&ticket, "slack", alice()).is_err());
        assert!(w.engine.retry(&ticket).is_err());
        assert!(w.engine.assignment_started(&ticket, &a1).is_err());
        assert!(
            w.engine
                .report(&ticket, &a1, report(ReportStatus::Progress))
                .is_err()
        );
        let effects = w.effects();
        assert!(
            !effects.iter().any(|e| matches!(
                e,
                Effect::Launch { ticket: t, .. } | Effect::Deliver { ticket: t, .. } if *t == ticket
            )),
            "{effects:?}"
        );
        // A later accepted Ticket on the same Thread does not revive it.
        let eval = w.engine.evaluate_due().unwrap();
        for e in eval {
            let _ = w.engine.decide(&e, accept(1_000_000));
        }
        assert_eq!(before, w.ticket(&ticket), "via_router={via_router}");
    }
}

// ---------- 6. open Ticket, latest Assignment ended ----------

fn idle(w: &mut World) -> TicketId {
    let (ticket, a1) = w.running("t1", &["th1"]);
    w.engine
        .report(&ticket, &a1, report(ReportStatus::Waiting))
        .unwrap();
    w.engine
        .assignment_ended(&ticket, &a1, "host restart")
        .unwrap();
    w.effects();
    assert_eq!(w.ticket(&ticket).stage(), Stage::Idle);
    ticket
}

#[test]
fn c6_an_allowed_person_starts_a_new_assignment() {
    let mut w = World::new(policy());
    let ticket = idle(&mut w);
    // Automatic and unlisted messages start nothing.
    w.engine.receive(ci("b1", &["th1"])).unwrap();
    w.engine.receive(mallory("m1", &["th1"])).unwrap();
    assert!(launches(&w.effects()).is_empty());
    let intake = w.engine.receive(from_alice("a1", &["th9", "th1"])).unwrap();
    assert_eq!(intake, stored(&[&ticket], false));
    let effects = w.effects();
    let [(t, a2)] = &launches(&effects)[..] else {
        panic!("{effects:?}")
    };
    assert_eq!(t, &ticket);
    w.engine.assignment_started(&ticket, a2).unwrap();
    let effects = w.effects();
    assert!(
        matches!(&effects[..], [Effect::Deliver { inputs, .. }] if inputs.len() == 3),
        "{effects:?}"
    );
}

/// A pause holds work; it should not drop it. The person's message reaches
/// the Ticket and is not queued, so once the pause lifts nothing ever acts
/// on it.
#[test]
fn issue_c6_a_follow_up_received_while_paused_never_starts_an_assignment() {
    let mut w = World::new(policy());
    let ticket = idle(&mut w);
    w.engine.set_policy(paused());
    let intake = w.engine.receive(from_alice("a1", &["th1"])).unwrap();
    let during = w.effects();
    w.engine.set_policy(policy());
    let evals = w.engine.evaluate_due().unwrap();
    let after = w.effects();
    let t = w.ticket(&ticket);
    let undelivered: Vec<_> = t
        .inputs
        .iter()
        .filter(|i| i.sent_to.is_none())
        .map(|i| &i.body)
        .collect();
    println!(
        "intake={intake:?}\nlaunches while paused={:?}\nafter unpause: evals={evals:?} launches={:?}\nassignments={} stage={:?} undelivered={undelivered:?} th1.queued={:?}",
        launches(&during),
        launches(&after),
        t.assignments.len(),
        t.stage(),
        w.queued("th1"),
    );
    assert!(
        !launches(&during).is_empty() || !launches(&after).is_empty() || !evals.is_empty(),
        "alice's message on an idle ticket never started an assignment nor reached an Eval"
    );
}

// ---------- 7. handback ----------

#[test]
fn c7_handback_queues_an_owned_follow_up_once_on_its_primary_thread() {
    let mut w = World::new(policy());
    let (ticket, _) = w.running("t1", &["th1"]);
    // Primary Thread th9 is not the ticket's; it is still judged there.
    w.engine.receive(from_alice("a1", &["th9", "th1"])).unwrap();
    let a1 = TriggerId::new("a1");
    assert!(w.engine.handback(&ticket, &a1, "beyond scope").unwrap());
    assert!(!w.engine.handback(&ticket, &a1, "again").unwrap());
    assert_eq!(w.queued("th9"), ["a1"]);
    assert!(w.queued("th1").is_empty());
    // Still once after its Eval ran.
    let eval = w.one_eval();
    w.engine
        .decide(&eval, Verdict::Ignore { reason: "x".into() })
        .unwrap();
    assert!(!w.engine.handback(&ticket, &a1, "third").unwrap());
    assert!(w.queued("th9").is_empty());

    // Not owned: nothing changes.
    for t in [
        ci("b1", &["th1"]),
        unlisted_bot("b2", &["th1"]),
        mallory("m1", &["th1"]),
        schedule("s1", &["th1"]),
    ] {
        let id = t.id.clone();
        w.engine.receive(t).unwrap();
        let before = w.snapshot();
        assert!(
            !w.engine.handback(&ticket, &id, "not mine").unwrap(),
            "{id}"
        );
        assert_eq!(before, w.snapshot(), "{id}");
    }
    // Never reached it.
    assert!(
        w.engine
            .handback(&ticket, &TriggerId::new("nope"), "x")
            .is_err()
    );
    assert!(
        w.engine
            .handback(&ticket, &TriggerId::new("t1"), "origin")
            .is_err()
    );
}

/// Two Tickets on one Thread each hand back the same follow-up; it is queued
/// twice and one Candidate carries it twice (the shape adversarial_b's
/// `handback_of_queued_trigger_is_not_judged_twice` guards against).
#[test]
fn issue_c7_one_follow_up_handed_back_by_two_tickets_is_queued_and_judged_twice() {
    let mut w = World::new(policy());
    let (a, _) = w.running("t1", &["th1"]);
    w.engine.receive(schedule("s2", &["th1"])).unwrap();
    let eval = w.one_eval();
    let (b, _) = w.accept_and_start(&eval);
    assert_ne!(a, b);
    let intake = w.engine.receive(from_alice("t3", &["th1"])).unwrap();
    assert_eq!(intake, stored(&[&a, &b], false));
    let t3 = TriggerId::new("t3");
    let first = w.engine.handback(&a, &t3, "not mine").unwrap();
    let second = w.engine.handback(&b, &t3, "not mine").unwrap();
    let queued = w.queued("th1");
    w.engine.evaluate_due().unwrap();
    let batches = assessed(&w.effects());
    println!("handback A={first} B={second}; th1.queued={queued:?}; candidates={batches:?}");
    assert_eq!(queued, ["t3"], "handed back to an Eval more than once");
    assert_eq!(batches, [vec!["t3".to_owned()]]);
}

/// The worker hands back a stop request; while it waits, the running Eval on
/// the Thread accepts a new Ticket, which adopts it as its own follow-up.
/// It never reaches an Eval (engine.rs: "It goes to an Eval on its primary
/// Thread, once").
#[test]
fn issue_c7_c8_a_handed_back_trigger_is_adopted_by_a_new_ticket_and_never_judged() {
    let mut w = World::new(policy());
    let (a, _) = w.running("t1", &["th1"]);
    w.engine.receive(schedule("s2", &["th1"])).unwrap();
    let eval = w.one_eval();
    w.effects();
    let stop = msg("t3", alice(), Addressed::Mention, "th1");
    assert_eq!(w.engine.receive(stop).unwrap(), stored(&[&a], false));
    assert!(
        w.engine
            .handback(&a, &TriggerId::new("t3"), "a stop request")
            .unwrap()
    );
    assert_eq!(w.queued("th1"), ["t3"]);
    w.engine.decide(&eval, accept(1_000_000)).unwrap();
    let c = w
        .engine
        .repos()
        .tickets
        .keys()
        .find(|k| **k != a)
        .unwrap()
        .clone();
    let evals = w.engine.evaluate_due().unwrap();
    let batches = assessed(&w.effects());
    println!(
        "th1.queued={:?}; C inputs={:?}; next evals={evals:?} candidates={batches:?}; A stage={:?}",
        w.queued("th1"),
        trigger_inputs(&w, &c),
        w.ticket(&a).stage()
    );
    assert_eq!(
        batches,
        [vec!["t3".to_owned()]],
        "the handed-back stop never reached an Eval"
    );
}

// ---------- 8. adoption of waiting Triggers ----------

#[test]
fn c8_a_new_ticket_adopts_waiting_triggers_on_all_its_threads() {
    let mut w = World::new(policy());
    let mut first = msg("t1", alice(), Addressed::Mention, "th1");
    first.threads = keys(&["th1", "pr-7"]);
    w.engine.receive(first).unwrap();
    let eval = w.one_eval();
    for t in [
        from_alice("a1", &["th1"]),
        ci("b1", &["th1"]),
        mallory("m1", &["th1"]),
        schedule("s1", &["th1"]),
        msg("a2", alice(), Addressed::Mention, "pr-7"),
        ci("b2", &["pr-7"]),
    ] {
        assert_eq!(w.engine.receive(t).unwrap(), stored(&[], true));
    }
    w.engine.decide(&eval, accept(1_000_000)).unwrap();
    let ticket = w.engine.repos().tickets.keys().next().unwrap().clone();
    let inputs = trigger_inputs(&w, &ticket);
    println!("inputs={inputs:?}");
    for (id, owner) in [
        ("a1", true),
        ("b1", false),
        ("m1", false),
        ("s1", false),
        ("a2", true),
        ("b2", false),
    ] {
        assert!(inputs.contains(&(id.to_owned(), owner)), "{id}");
    }
    assert_eq!(w.queued("th1"), ["b1", "m1", "s1"]);
    assert_eq!(w.queued("pr-7"), ["b2"]);
    // Weaker admitted Triggers left in the queue by the accepting Eval itself.
    let mut w = World::new(policy());
    w.engine.receive(ci("b0", &["th1"])).unwrap();
    w.engine.receive(from_alice("a0", &["th1"])).unwrap(); // inferred: no ticket yet
    w.engine
        .receive(msg("t1", alice(), Addressed::Mention, "th1"))
        .unwrap();
    let eval = w.one_eval();
    assert_eq!(assessed(&w.effects()), [vec!["t1".to_owned()]]);
    w.engine.decide(&eval, accept(1_000_000)).unwrap();
    let ticket = w.engine.repos().tickets.keys().next().unwrap().clone();
    assert_eq!(
        trigger_inputs(&w, &ticket),
        [("b0".to_owned(), false), ("a0".to_owned(), true)]
    );
    assert_eq!(w.queued("th1"), ["b0"]);
}

/// alice's message belongs to th1 (the new Ticket's Thread) but its primary
/// is pr-7, so it waits there. Arriving a moment later it would reach the
/// Ticket and skip the Router; arriving during the accepting Eval it never
/// reaches the Ticket and goes to the Router on pr-7 instead.
#[test]
fn issue_c8_a_waiting_trigger_on_the_tickets_thread_queued_on_another_primary_is_not_adopted() {
    let mut w = World::new(policy());
    w.engine
        .receive(msg("t1", alice(), Addressed::Mention, "th1"))
        .unwrap();
    let eval = w.one_eval();
    w.engine
        .receive(from_alice("a1", &["pr-7", "th1"]))
        .unwrap();
    w.engine.decide(&eval, accept(1_000_000)).unwrap();
    let ticket = w.engine.repos().tickets.keys().next().unwrap().clone();
    // Control: the same message after the Ticket exists.
    let later = w
        .engine
        .receive(from_alice("a2", &["pr-7", "th1"]))
        .unwrap();
    println!(
        "ticket threads={:?}; inputs={:?}; pr-7.queued={:?}; control intake={later:?}",
        w.ticket(&ticket).threads,
        trigger_inputs(&w, &ticket),
        w.queued("pr-7")
    );
    assert_eq!(later, stored(&[&ticket], false));
    assert!(
        trigger_inputs(&w, &ticket).contains(&("a1".to_owned(), true)),
        "a1 waited on th1 when the Ticket was created but never reached it"
    );
}

#[test]
fn c8_adoption_by_a_ticket_awaiting_approval_and_ended_eval_kinds() {
    let mut w = World::new(policy());
    w.engine
        .receive(msg("t1", alice(), Addressed::Mention, "th1"))
        .unwrap();
    let eval = w.one_eval();
    w.engine.receive(from_alice("a1", &["th1"])).unwrap();
    w.engine.receive(ci("b1", &["th1"])).unwrap();
    w.engine.decide(&eval, accept(50_000_000)).unwrap();
    let ticket = w.engine.repos().tickets.keys().next().unwrap().clone();
    let t = w.ticket(&ticket);
    assert_eq!(
        t.stage(),
        Stage::Approval,
        "the adopted person Input cleared the ask"
    );
    assert_eq!(
        trigger_inputs(&w, &ticket),
        [("a1".to_owned(), true), ("b1".to_owned(), false)]
    );
    assert_eq!(w.queued("th1"), ["b1"]);
    // An ignore creates nothing and adopts nothing.
    let mut w = World::new(policy());
    w.engine
        .receive(msg("t1", alice(), Addressed::Mention, "th1"))
        .unwrap();
    let eval = w.one_eval();
    w.engine.receive(from_alice("a1", &["th1"])).unwrap();
    let d = w
        .engine
        .decide(&eval, Verdict::Ignore { reason: "x".into() })
        .unwrap();
    assert_eq!(w.engine.repos().decisions[&d].kind, DecisionKind::Ignore);
    assert_eq!(w.queued("th1"), ["a1"]);
}

/// Variant: the Ticket follows th1 and pr-7; alice's pr-7 message is in a
/// running Eval on pr-7 (built before the Ticket existed) when the Ticket is
/// accepted. It never reaches the Ticket.
#[test]
fn issue_c8_variant_a_trigger_in_a_running_eval_on_another_ticket_thread_never_reaches_it() {
    let mut w = World::new(policy());
    w.engine
        .receive(on(
            msg("t1", alice(), Addressed::Mention, "th1"),
            &["th1", "pr-7"],
        ))
        .unwrap();
    w.engine
        .receive(msg("p1", alice(), Addressed::Mention, "pr-7"))
        .unwrap();
    let evals = w.engine.evaluate_due().unwrap();
    assert_eq!(evals.len(), 2);
    let effects = w.effects();
    let on_th1 = effects
        .iter()
        .find_map(|e| match e {
            Effect::Assess(c) if c.thread.as_str() == "th1" => Some(c.eval.clone()),
            _ => None,
        })
        .unwrap();
    let on_pr7 = evals.iter().find(|e| **e != on_th1).unwrap().clone();
    w.engine.decide(&on_th1, accept(1_000_000)).unwrap();
    let ticket = w.engine.repos().tickets.keys().next().unwrap().clone();
    w.engine
        .decide(
            &on_pr7,
            Verdict::Ignore {
                reason: "nothing new".into(),
            },
        )
        .unwrap();
    println!(
        "ticket threads={:?}; inputs={:?}",
        w.ticket(&ticket).threads,
        trigger_inputs(&w, &ticket)
    );
    assert!(
        trigger_inputs(&w, &ticket).iter().any(|(t, _)| t == "p1"),
        "p1, on a Thread of the Ticket, never reached it"
    );
}

/// Outside the 8 claims, on the intake path: a follow-up wakes a Ticket that
/// waits for approval once the threshold rises; Approval stays Waiting, so
/// the later approval launches a second Assignment over the first, which is
/// never stopped.
#[test]
fn issue_extra_a_follow_up_launches_a_ticket_still_waiting_for_approval() {
    let mut w = World::new(policy());
    w.engine
        .receive(msg("t1", alice(), Addressed::Mention, "th1"))
        .unwrap();
    let eval = w.one_eval();
    w.engine.decide(&eval, accept(5_000_000)).unwrap();
    let ticket = w.engine.repos().tickets.keys().next().unwrap().clone();
    w.effects();
    w.engine.set_policy(AllowlistPolicy {
        auto_approve_microusd: 10_000_000,
        ..policy()
    });
    w.engine.receive(from_alice("a1", &["th1"])).unwrap();
    let on_follow_up = w.effects();
    let t = w.ticket(&ticket);
    println!(
        "after follow-up: launches={:?} approval={:?} stage={:?}",
        launches(&on_follow_up),
        t.approval,
        t.stage()
    );
    let approved = w.engine.approve(&ticket, "slack", alice());
    let on_approve = w.effects();
    let t = w.ticket(&ticket);
    let stops = on_approve
        .iter()
        .filter(|e| matches!(e, Effect::Stop { .. }))
        .count();
    println!(
        "approve -> {approved:?}; launches={:?} stops={stops}; assignments={:?}",
        launches(&on_approve),
        t.assignments
            .iter()
            .map(|a| (&a.id, a.state))
            .collect::<Vec<_>>()
    );
    let total = launches(&on_follow_up).len() + launches(&on_approve).len();
    assert!(
        total <= 1,
        "{total} Launches for one Ticket; the first Assignment was never stopped"
    );
}
