//! Verification of the budget, approval and Ticket-lifecycle claims of PR #18.
//! Every test here is expected to pass; failures are listed in slice_budget_issues.rs.

use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, Approval, AssignmentId, ChannelRef, Effect, EvalId,
    InputBody, MessageKind, Plan, ReportIn, ReportStatus, Stage, ThreadKey, Ticket, TicketId,
    TicketOutcome, Trigger, TriggerId, Verdict,
};
use solos_agent_core::{AllowlistPolicy, Engine, Error, MAX_SWARM_AGENTS, MemoryRepositories};
use std::collections::BTreeMap;

type TestEngine = Engine<MemoryRepositories, AllowlistPolicy, Box<dyn Fn() -> u64>>;

struct World {
    engine: TestEngine,
    n: u32,
}

const THRESHOLD: u64 = 2_000_000;

fn policy() -> AllowlistPolicy {
    AllowlistPolicy {
        people: ["slack:alice".to_owned(), "slack:bob".to_owned()].into(),
        bots: ["github:ci-bot".to_owned()].into(),
        approvers: ["slack:alice".to_owned()].into(),
        auto_approve_microusd: THRESHOLD,
        ..AllowlistPolicy::default()
    }
}

fn msg(id: &str, actor: Actor, addressed: Addressed, thread: &str, text: &str) -> Trigger {
    Trigger {
        id: TriggerId::new(id),
        source: "slack".into(),
        at_ms: 1_000,
        actor,
        addressed,
        text: text.into(),
        threads: vec![ThreadKey::new(thread)],
        reply_to: vec![ChannelRef {
            source: "slack".into(),
            address: BTreeMap::from([("thread_ts".to_owned(), thread.to_owned())]),
        }],
        meta: serde_json::Value::Null,
    }
}

fn ci(id: &str, thread: &str) -> Trigger {
    let mut t = msg(
        id,
        Actor::bot("ci-bot"),
        Addressed::Unaddressed,
        thread,
        "CI",
    );
    t.source = "github".into();
    t
}

fn verdict(plan: Plan, budget: u64) -> Verdict {
    Verdict::Accept(Accept {
        title: "Task".into(),
        scope: "Do the task".into(),
        reason: "asked".into(),
        plan,
        budget_microusd: Some(budget),
        acknowledge: Acknowledge::Standard,
        rules: Vec::new(),
        repo: None,
        forge: None,
        upstream: None,
    })
}

fn session() -> Plan {
    Plan::Session {
        harness: None,
        environment: None,
        method: None,
    }
}

fn rep(status: ReportStatus, question: Option<&str>) -> ReportIn {
    ReportIn {
        status,
        text: format!("{status:?} text"),
        question: question.map(str::to_owned),
        spent_microusd: 0,
        harvest: None,
    }
}

type Launch = (TicketId, AssignmentId, Plan, Option<u64>, bool);

fn launches(effects: &[Effect]) -> Vec<Launch> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Launch {
                ticket,
                assignment,
                plan,
                budget_microusd,
                recovery,
            } => Some((
                ticket.clone(),
                assignment.clone(),
                plan.clone(),
                *budget_microusd,
                *recovery,
            )),
            _ => None,
        })
        .collect()
}

fn sends(effects: &[Effect]) -> Vec<(MessageKind, String)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Send(m) => Some((m.kind, m.text.clone())),
            _ => None,
        })
        .collect()
}

fn attention(effects: &[Effect]) -> usize {
    sends(effects)
        .iter()
        .filter(|(k, _)| *k == MessageKind::Attention)
        .count()
}

impl World {
    fn new() -> Self {
        let clock = Box::new(|| 1_000_u64) as Box<dyn Fn() -> u64>;
        Self {
            engine: Engine::new(MemoryRepositories::default(), policy(), clock),
            n: 0,
        }
    }
    fn effects(&mut self) -> Vec<Effect> {
        self.engine.repos_mut().take_effects()
    }
    fn ticket(&self, id: &TicketId) -> Ticket {
        self.engine.repos().tickets[id].clone()
    }
    fn threshold(&mut self, t: u64) {
        self.engine.set_policy(AllowlistPolicy {
            auto_approve_microusd: t,
            ..policy()
        });
    }
    fn eval(&mut self, thread: &str) -> EvalId {
        let t = msg(
            &format!("ask-{thread}"),
            Actor::person("alice"),
            Addressed::Mention,
            thread,
            "@agent do it",
        );
        self.engine.receive(t).unwrap();
        let started = self.engine.evaluate_due().unwrap();
        assert_eq!(started.len(), 1);
        self.effects();
        started[0].clone()
    }
    /// Accept a Ticket on `thread`; returns it and the effects of the accept.
    fn accept(&mut self, thread: &str, plan: Plan, budget: u64) -> (TicketId, Vec<Effect>) {
        let eval = self.eval(thread);
        let d = self.engine.decide(&eval, verdict(plan, budget)).unwrap();
        let ticket = self.engine.repos().decisions[&d].ticket.clone().unwrap();
        (ticket, self.effects())
    }
    fn running(&mut self, thread: &str, budget: u64) -> (TicketId, AssignmentId) {
        let (t, eff) = self.accept(thread, session(), budget);
        let [(_, a, ..)] = &launches(&eff)[..] else {
            panic!("{eff:?}")
        };
        self.engine.assignment_started(&t, a).unwrap();
        self.effects();
        (t, a.clone())
    }
    fn person(&mut self, who: &str, thread: &str) -> Vec<Effect> {
        self.n += 1;
        let id = format!("{who}-{}", self.n);
        self.engine
            .receive(msg(
                &id,
                Actor::person(who),
                Addressed::Unaddressed,
                thread,
                "hi",
            ))
            .unwrap();
        self.effects()
    }
}

// ---------- claim 1 ----------

#[test]
fn c1_decide_over_threshold_launches_nothing_and_asks_once_at_threshold_launches() {
    let mut w = World::new();
    let (t, eff) = w.accept("th1", session(), THRESHOLD + 1);
    assert!(launches(&eff).is_empty());
    assert_eq!(attention(&eff), 1);
    assert_eq!(sends(&eff).len(), 1, "no acknowledgement either: {eff:?}");
    assert_eq!(w.ticket(&t).approval, Approval::Waiting);
    for _ in 0..3 {
        let eff = w.person("alice", "th1");
        assert!(launches(&eff).is_empty() && attention(&eff) == 0, "{eff:?}");
    }
    let eff = w.person("bob", "th1");
    assert!(launches(&eff).is_empty() && attention(&eff) == 0, "{eff:?}");

    let (_, eff) = w.accept("th2", session(), THRESHOLD);
    assert_eq!(
        launches(&eff).len(),
        1,
        "a budget equal to the threshold launches"
    );
}

#[test]
fn c1_resume_checks_current_threshold_and_asks_once() {
    let mut w = World::new();
    let (t, a) = w.running("th1", 1_500_000);
    w.engine
        .report(&t, &a, rep(ReportStatus::Ready, None))
        .unwrap();
    w.effects();
    w.threshold(1_000_000);
    let mut asks = 0;
    for _ in 0..3 {
        let eff = w.person("alice", "th1");
        assert!(launches(&eff).is_empty(), "{eff:?}");
        asks += attention(&eff);
    }
    assert_eq!(asks, 1);
    assert_eq!(w.ticket(&t).approval, Approval::Waiting);
    assert_eq!(w.ticket(&t).stage(), Stage::Approval);
    w.engine
        .approve(&t, "slack", Actor::person("alice"))
        .unwrap();
    let l = launches(&w.effects());
    assert_eq!(l.len(), 1);
    assert!(l[0].4, "recovery after an earlier Assignment");
}

#[test]
fn c1_wake_of_open_ticket_checks_current_threshold() {
    let mut w = World::new();
    let (t, a) = w.running("th1", 1_500_000);
    w.engine
        .report(&t, &a, rep(ReportStatus::Waiting, None))
        .unwrap();
    w.engine.assignment_ended(&t, &a, "host restart").unwrap();
    w.effects();
    w.threshold(1_000_000);
    let mut asks = 0;
    for _ in 0..3 {
        let eff = w.person("alice", "th1");
        assert!(launches(&eff).is_empty(), "{eff:?}");
        asks += attention(&eff);
    }
    // A bot never wakes or asks.
    w.engine.receive(ci("c1", "th1")).unwrap();
    let eff = w.effects();
    assert!(launches(&eff).is_empty() && attention(&eff) == 0);
    assert_eq!(asks, 1);
    assert_eq!(w.ticket(&t).stage(), Stage::Approval);
}

#[test]
fn c1_retry_checks_current_threshold_and_asks_once() {
    let mut w = World::new();
    let (t, a) = w.running("th1", 1_500_000);
    w.engine
        .report(&t, &a, rep(ReportStatus::Failed, None))
        .unwrap();
    w.effects();
    w.threshold(1_000_000);
    assert_eq!(w.engine.retry(&t), Ok(None));
    let eff = w.effects();
    assert!(launches(&eff).is_empty());
    assert_eq!(attention(&eff), 1);
    assert!(matches!(w.engine.retry(&t), Err(Error::Refused(_))));
    let eff = w.person("alice", "th1");
    assert!(launches(&eff).is_empty() && attention(&eff) == 0, "{eff:?}");
    w.engine
        .approve(&t, "slack", Actor::person("alice"))
        .unwrap();
    let l = launches(&w.effects());
    assert_eq!(l.len(), 1);
    assert!(l[0].4);
    // Retry after approval launches with no further ask, even below a lower threshold.
    w.threshold(1);
    let next = w.engine.retry(&t).unwrap();
    let eff = w.effects();
    assert!(next.is_some() && launches(&eff).len() == 1 && attention(&eff) == 0);
}

#[test]
fn c1_granted_approval_covers_later_resumes() {
    let mut w = World::new();
    let (t, _) = w.accept("th1", session(), 5_000_000);
    w.engine
        .approve(&t, "slack", Actor::person("alice"))
        .unwrap();
    let [(_, a, ..)] = &launches(&w.effects())[..] else {
        panic!()
    };
    w.engine.assignment_started(&t, a).unwrap();
    w.engine
        .report(&t, a, rep(ReportStatus::Ready, None))
        .unwrap();
    w.effects();
    w.threshold(1);
    let eff = w.person("alice", "th1");
    assert_eq!(launches(&eff).len(), 1);
    assert_eq!(attention(&eff), 0);
}

// ---------- claim 2 ----------

#[test]
fn c2_approve_only_waiting_only_listed_approver_then_launches() {
    let mut w = World::new();
    let (cheap, _) = w.accept("th0", session(), 1_000_000);
    assert!(matches!(
        w.engine.approve(&cheap, "slack", Actor::person("alice")),
        Err(Error::Refused(_))
    ));

    let swarm = Plan::Swarm {
        agents: 12,
        goal_template: None,
    };
    let (t, _) = w.accept("th1", swarm.clone(), 9_000_000);
    let refusals = [
        ("slack", Actor::person("bob")),
        ("github", Actor::person("alice")),
        ("slack", Actor::bot("alice")),
        ("slack", Actor::system("alice")),
        ("slack", Actor::person("slack:alice")),
    ];
    for (source, by) in refusals {
        let r = w.engine.approve(&t, source, by.clone());
        assert!(
            matches!(r, Err(Error::Forbidden(_))),
            "{source} {by:?}: {r:?}"
        );
        assert!(w.effects().is_empty());
        assert_eq!(w.ticket(&t).approval, Approval::Waiting);
    }
    w.engine
        .approve(&t, "slack", Actor::person("alice"))
        .unwrap();
    let l = launches(&w.effects());
    assert_eq!(l.len(), 1);
    assert_eq!(
        (l[0].2.clone(), l[0].3, l[0].4),
        (swarm, Some(9_000_000), false)
    );
    assert!(matches!(w.ticket(&t).approval, Approval::Granted { .. }));
    assert!(matches!(
        w.engine.approve(&t, "slack", Actor::person("alice")),
        Err(Error::Refused(_))
    ));

    let (stopped, _) = w.accept("th2", session(), 9_000_000);
    w.engine
        .stop_ticket(&stopped, "slack", Actor::person("alice"), "no")
        .unwrap();
    assert!(matches!(
        w.engine.approve(&stopped, "slack", Actor::person("alice")),
        Err(Error::Refused(_))
    ));
    assert!(matches!(
        w.engine
            .approve(&TicketId::new("nope"), "slack", Actor::person("alice")),
        Err(Error::Missing { .. })
    ));
}

// ---------- claim 3 ----------

#[test]
fn c3_ready_sends_result_and_completes() {
    let mut w = World::new();
    let (t, a) = w.running("th1", 1_000_000);
    w.engine
        .report(&t, &a, rep(ReportStatus::Ready, None))
        .unwrap();
    let eff = w.effects();
    assert_eq!(
        sends(&eff),
        vec![(MessageKind::Result, "Ready text".to_owned())]
    );
    let tk = w.ticket(&t);
    assert_eq!(tk.ended.unwrap().outcome, TicketOutcome::Completed);
    assert_eq!(tk.attention, None);
}

#[test]
fn c3_dropped_ends_silently() {
    let mut w = World::new();
    let (t, a) = w.running("th1", 1_000_000);
    w.engine
        .report(&t, &a, rep(ReportStatus::Dropped, None))
        .unwrap();
    let eff = w.effects();
    assert!(sends(&eff).is_empty(), "{eff:?}");
    assert_eq!(w.ticket(&t).ended.unwrap().outcome, TicketOutcome::Dropped);
}

#[test]
fn c3_question_blocked_failed_ask_once_quiet_statuses_stay_quiet() {
    let cases: [(ReportStatus, Option<&str>, usize, Stage); 8] = [
        (ReportStatus::Blocked, None, 1, Stage::Blocked),
        (ReportStatus::Failed, None, 1, Stage::Failed),
        (ReportStatus::Blocked, Some("which?"), 1, Stage::Answer),
        (ReportStatus::Failed, Some("which?"), 1, Stage::Answer),
        (ReportStatus::Progress, Some("which?"), 1, Stage::Answer),
        (ReportStatus::Waiting, Some("which?"), 1, Stage::Answer),
        (ReportStatus::Progress, None, 0, Stage::Working),
        (ReportStatus::Waiting, None, 0, Stage::Idle),
    ];
    for (status, q, asks, stage) in cases {
        let mut w = World::new();
        let (t, a) = w.running("th1", 1_000_000);
        w.engine.report(&t, &a, rep(status, q)).unwrap();
        let eff = w.effects();
        assert_eq!(attention(&eff), asks, "{status:?} {q:?}: {eff:?}");
        assert_eq!(sends(&eff).len(), asks, "{status:?} {q:?}: {eff:?}");
        if let Some(q) = q {
            assert_eq!(sends(&eff)[0].1, q, "the question is what is asked");
        }
        assert_eq!(w.ticket(&t).stage(), stage, "{status:?} {q:?}");
        assert!(w.ticket(&t).ended.is_none());
    }
}

// ---------- claim 4 ----------

fn delivered_bodies(w: &World, t: &TicketId, eff: &[Effect]) -> Vec<Vec<String>> {
    let tk = w.ticket(t);
    eff.iter()
        .filter_map(|e| match e {
            Effect::Deliver { inputs, .. } => Some(
                inputs
                    .iter()
                    .map(|id| {
                        let input = tk.inputs.iter().find(|i| i.id == *id).unwrap();
                        match &input.body {
                            InputBody::Trigger { trigger } => trigger.to_string(),
                            InputBody::Note { text } => format!("note:{text}"),
                            InputBody::Approved { by } => format!("approved:{}", by.id),
                            InputBody::Secret {
                                secret,
                                granted,
                                by,
                            } => format!("secret:{secret}:{granted}:{}", by.id),
                        }
                    })
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

#[test]
fn c4_held_while_blocked_or_failed_then_delivered_in_order() {
    for status in [ReportStatus::Blocked, ReportStatus::Failed] {
        let mut w = World::new();
        let (t, a) = w.running("th1", 1_000_000);
        w.engine.report(&t, &a, rep(status, None)).unwrap();
        w.effects();
        w.engine.receive(ci("c1", "th1")).unwrap();
        w.engine.note(&t, "n1".into()).unwrap();
        w.engine
            .receive(msg(
                "m1",
                Actor::person("mallory"),
                Addressed::Mention,
                "th1",
                "x",
            ))
            .unwrap();
        w.engine.receive(ci("c2", "th1")).unwrap();
        let eff = w.effects();
        assert!(delivered_bodies(&w, &t, &eff).is_empty(), "{eff:?}");
        assert!(launches(&eff).is_empty());
        let eff = w.person("bob", "th1");
        assert_eq!(
            delivered_bodies(&w, &t, &eff),
            vec![vec!["c1", "note:n1", "m1", "c2", "bob-1"]],
            "{status:?}"
        );
        // Not held any more.
        w.engine.receive(ci("c3", "th1")).unwrap();
        let eff = w.effects();
        assert_eq!(delivered_bodies(&w, &t, &eff), vec![vec!["c3"]]);
        // A second block holds again.
        w.engine.report(&t, &a, rep(status, None)).unwrap();
        w.engine.receive(ci("c4", "th1")).unwrap();
        let eff = w.effects();
        assert!(delivered_bodies(&w, &t, &eff).is_empty(), "{eff:?}");
        let eff = w.person("alice", "th1");
        assert_eq!(delivered_bodies(&w, &t, &eff), vec![vec!["c4", "alice-2"]]);
    }
}

#[test]
fn c4_hold_across_a_crashed_assignment_releases_on_the_next_one() {
    let mut w = World::new();
    let (t, a) = w.running("th1", 1_000_000);
    w.engine
        .report(&t, &a, rep(ReportStatus::Failed, None))
        .unwrap();
    w.engine.receive(ci("c1", "th1")).unwrap();
    w.engine.assignment_ended(&t, &a, "crash").unwrap();
    w.engine.receive(ci("c2", "th1")).unwrap();
    let eff = w.effects();
    assert!(launches(&eff).is_empty() && delivered_bodies(&w, &t, &eff).is_empty());
    let eff = w.person("alice", "th1");
    let l = launches(&eff);
    assert_eq!(l.len(), 1);
    w.engine.assignment_started(&t, &l[0].1).unwrap();
    let eff = w.effects();
    assert_eq!(
        delivered_bodies(&w, &t, &eff),
        vec![vec!["c1", "c2", "alice-1"]]
    );
}

// ---------- claim 5 ----------

#[test]
fn c5_stage_walk() {
    let mut w = World::new();
    let (t, _) = w.accept("th1", session(), 5_000_000);
    assert_eq!(w.ticket(&t).stage(), Stage::Approval);
    w.person("alice", "th1");
    assert_eq!(w.ticket(&t).stage(), Stage::Approval);
    w.engine
        .approve(&t, "slack", Actor::person("alice"))
        .unwrap();
    let a = launches(&w.effects())[0].1.clone();
    assert_eq!(w.ticket(&t).stage(), Stage::Queued);
    w.engine.assignment_started(&t, &a).unwrap();
    assert_eq!(w.ticket(&t).stage(), Stage::Working);
    let seq: [(ReportStatus, Option<&str>, Stage); 6] = [
        (ReportStatus::Progress, None, Stage::Working),
        (ReportStatus::Waiting, None, Stage::Idle),
        (ReportStatus::Progress, Some("q?"), Stage::Answer),
        (ReportStatus::Blocked, None, Stage::Answer),
        (ReportStatus::Failed, None, Stage::Answer),
        (ReportStatus::Progress, None, Stage::Answer),
    ];
    for (s, q, stage) in seq {
        w.engine.report(&t, &a, rep(s, q)).unwrap();
        assert_eq!(w.ticket(&t).stage(), stage, "{s:?} {q:?}");
    }
    w.person("alice", "th1");
    assert_eq!(w.ticket(&t).stage(), Stage::Working);
    w.engine
        .report(&t, &a, rep(ReportStatus::Blocked, None))
        .unwrap();
    assert_eq!(w.ticket(&t).stage(), Stage::Blocked);
    w.engine.receive(ci("c1", "th1")).unwrap();
    assert_eq!(w.ticket(&t).stage(), Stage::Blocked);
    w.engine
        .report(&t, &a, rep(ReportStatus::Failed, None))
        .unwrap();
    assert_eq!(w.ticket(&t).stage(), Stage::Failed);
    w.engine.assignment_ended(&t, &a, "crash").unwrap();
    assert_eq!(w.ticket(&t).stage(), Stage::Failed);
    let eff = w.person("alice", "th1");
    let b = launches(&eff)[0].1.clone();
    assert_eq!(w.ticket(&t).stage(), Stage::Queued);
    w.engine.assignment_started(&t, &b).unwrap();
    w.engine
        .report(&t, &b, rep(ReportStatus::Waiting, None))
        .unwrap();
    w.engine.assignment_ended(&t, &b, "restart").unwrap();
    assert_eq!(w.ticket(&t).stage(), Stage::Idle);
    let eff = w.person("alice", "th1");
    let c = launches(&eff)[0].1.clone();
    w.engine.assignment_started(&t, &c).unwrap();
    w.engine
        .report(&t, &c, rep(ReportStatus::Ready, None))
        .unwrap();
    assert_eq!(w.ticket(&t).stage(), Stage::Ended);
}

// ---------- claim 6 ----------

#[test]
fn c6_swarm_size_and_zero_budget() {
    for (agents, ok) in [
        (0u16, false),
        (1, true),
        (MAX_SWARM_AGENTS, true),
        (MAX_SWARM_AGENTS + 1, false),
        (u16::MAX, false),
    ] {
        let mut w = World::new();
        let eval = w.eval("th1");
        let plan = Plan::Swarm {
            agents,
            goal_template: None,
        };
        let r = w.engine.decide(&eval, verdict(plan, 1_000_000));
        assert_eq!(r.is_ok(), ok, "{agents}: {r:?}");
        if !ok {
            assert!(matches!(r, Err(Error::Invalid(_))));
            assert!(w.engine.repos().tickets.is_empty());
        }
    }
    for plan in [
        session(),
        Plan::Swarm {
            agents: 3,
            goal_template: None,
        },
    ] {
        let mut w = World::new();
        let eval = w.eval("th1");
        let r = w.engine.decide(&eval, verdict(plan, 0));
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
    }
}

// ---------- claim 7 ----------

#[test]
fn c7_launch_carries_plan_budget_and_recovery_flag() {
    let mut w = World::new();
    let plan = Plan::Swarm {
        agents: 4,
        goal_template: Some("g".into()),
    };
    let (t, eff) = w.accept("th1", plan.clone(), 1_500_000);
    let mut seen: Vec<bool> = Vec::new();
    let mut check = |w: &World, eff: &[Effect]| {
        for (tk, _, p, b, rec) in launches(eff) {
            assert_eq!((tk, p, b), (t.clone(), plan.clone(), Some(1_500_000)));
            let earlier = w.ticket(&t).assignments.len() > 1;
            assert_eq!(rec, earlier);
            seen.push(rec);
        }
    };
    check(&w, &eff);
    let a = w.ticket(&t).assignments[0].id.clone();
    w.engine.assignment_started(&t, &a).unwrap();
    w.engine
        .report(&t, &a, rep(ReportStatus::Failed, None))
        .unwrap();
    w.effects();
    w.engine.retry(&t).unwrap();
    let eff = w.effects();
    check(&w, &eff);
    let a = w.ticket(&t).assignments.last().unwrap().id.clone();
    w.engine.assignment_ended(&t, &a, "crash").unwrap();
    let eff = w.person("alice", "th1");
    check(&w, &eff);
    let a = w.ticket(&t).assignments.last().unwrap().id.clone();
    w.engine.assignment_started(&t, &a).unwrap();
    w.engine
        .report(&t, &a, rep(ReportStatus::Ready, None))
        .unwrap();
    w.effects();
    let eff = w.person("alice", "th1");
    check(&w, &eff);
    w.threshold(1_000_000);
    let a = w.ticket(&t).assignments.last().unwrap().id.clone();
    w.engine.assignment_started(&t, &a).unwrap();
    w.engine
        .report(&t, &a, rep(ReportStatus::Failed, None))
        .unwrap();
    assert_eq!(w.engine.retry(&t), Ok(None));
    w.effects();
    w.engine
        .approve(&t, "slack", Actor::person("alice"))
        .unwrap();
    let eff = w.effects();
    check(&w, &eff);
    assert_eq!(seen, [false, true, true, true, true]);

    // approve at creation: no earlier Assignment.
    let (t2, _) = w.accept("th2", session(), 9_000_000);
    w.engine
        .approve(&t2, "slack", Actor::person("alice"))
        .unwrap();
    let l = launches(&w.effects());
    assert_eq!((l.len(), l[0].3, l[0].4), (1, Some(9_000_000), false));
}

// ---------- observation (passes; documents current behaviour) ----------

/// A retry after a question keeps the old Assignment's question, so the new
/// Assignment shows `Answer` through its own Progress/Waiting reports.
#[test]
fn note_retry_keeps_the_previous_assignments_question() {
    let mut w = World::new();
    let (t, a) = w.running("th1", 1_000_000);
    w.engine
        .report(&t, &a, rep(ReportStatus::Failed, Some("Which key?")))
        .unwrap();
    let b = w.engine.retry(&t).unwrap().unwrap();
    w.engine.assignment_started(&t, &b).unwrap();
    w.engine
        .report(&t, &b, rep(ReportStatus::Waiting, None))
        .unwrap();
    let tk = w.ticket(&t);
    println!("attention={:?} stage={:?}", tk.attention, tk.stage());
    assert_eq!(tk.stage(), Stage::Answer);
}
