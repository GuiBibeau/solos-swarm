use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, Approval, ChannelRef, DecisionKind, Effect, MessageKind,
    Plan, ReportIn, ReportStatus, Stage, ThreadKey, TicketId, TicketOutcome, Trigger, TriggerId,
    Verdict,
};
use solos_agent_core::{
    AcceptMode, AllowlistPolicy, Engine, Intake, MAX_EXPIRED_EVALS, MemoryRepositories,
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
    fn receive_and_start_eval(&mut self, trigger: Trigger) -> solos_agent_core::model::EvalId {
        self.engine.receive(trigger).unwrap();
        let started = self.engine.evaluate_due().unwrap();
        assert_eq!(started.len(), 1, "one eval starts");
        started[0].clone()
    }
}

fn policy() -> AllowlistPolicy {
    AllowlistPolicy {
        people: ["slack:alice".to_owned()].into(),
        bots: ["github:ci-bot".to_owned()].into(),
        approvers: ["slack:alice".to_owned()].into(),
        rules: ["reproduce-bugs".to_owned()].into(),
        ..AllowlistPolicy::default()
    }
}

fn slack(id: &str, actor: Actor, addressed: Addressed, thread: &str, text: &str) -> Trigger {
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
            address: BTreeMap::from([
                ("channel".to_owned(), "C1".to_owned()),
                ("thread_ts".to_owned(), thread.to_owned()),
            ]),
        }],
        meta: serde_json::Value::Null,
    }
}

fn accept(plan: Plan, budget: u64) -> Verdict {
    Verdict::Accept(Accept {
        title: "Fix the flaky test".into(),
        scope: "Reproduce and fix tests/broker_http.rs flake".into(),
        reason: "alice asked".into(),
        plan,
        budget_microusd: budget,
        acknowledge: Acknowledge::Standard,
        rules: Vec::new(),
    })
}

fn session() -> Plan {
    Plan::Session {
        harness: None,
        environment: None,
    }
}

fn launched(effects: &[Effect]) -> Vec<(TicketId, solos_agent_core::model::AssignmentId, Plan)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Launch {
                ticket,
                assignment,
                plan,
                ..
            } => Some((ticket.clone(), assignment.clone(), plan.clone())),
            _ => None,
        })
        .collect()
}

fn sent(effects: &[Effect]) -> Vec<(MessageKind, String)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Send(m) => Some((m.kind, m.text.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn explicit_request_runs_to_a_result_in_the_same_thread() {
    let mut w = World::new(policy());
    let alice = Actor::person("alice");
    let eval = w.receive_and_start_eval(slack(
        "t1",
        alice,
        Addressed::Mention,
        "th1",
        "@agent fix the flake",
    ));
    let assess = w.effects();
    assert!(matches!(&assess[..], [Effect::Assess(c)] if c.triggers.len() == 1));

    let decision = w
        .engine
        .decide(&eval, accept(session(), 1_000_000))
        .unwrap();
    let effects = w.effects();
    let [(ticket, assignment, plan)] = &launched(&effects)[..] else {
        panic!("one launch: {effects:?}")
    };
    assert_eq!(*plan, session());
    assert_eq!(
        sent(&effects),
        vec![(
            MessageKind::Acknowledgement,
            "On it: Fix the flaky test.".to_owned()
        )]
    );
    let record = w.engine.repos().decisions[&decision].clone();
    assert_eq!(record.kind, DecisionKind::Accept);
    assert_eq!(record.ticket.as_ref(), Some(ticket));

    w.engine.assignment_started(ticket, assignment).unwrap();
    assert_eq!(w.engine.repos().tickets[ticket].stage(), Stage::Working);
    let report = |status, text: &str| ReportIn {
        status,
        text: text.into(),
        question: None,
        spent_microusd: 0,
    };
    w.engine
        .report(
            ticket,
            assignment,
            report(ReportStatus::Progress, "reproduced"),
        )
        .unwrap();
    assert!(sent(&w.effects()).is_empty(), "progress stays quiet");
    w.engine
        .report(ticket, assignment, report(ReportStatus::Ready, "PR #18"))
        .unwrap();
    let effects = w.effects();
    assert_eq!(
        sent(&effects),
        vec![(MessageKind::Result, "PR #18".to_owned())]
    );
    assert!(effects.iter().any(|e| matches!(e, Effect::Stop { .. })));
    let t = &w.engine.repos().tickets[ticket];
    assert_eq!(
        t.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
}

#[test]
fn a_redelivered_trigger_is_stored_once() {
    let mut w = World::new(policy());
    let t = slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "hi",
    );
    assert!(matches!(
        w.engine.receive(t.clone()).unwrap(),
        Intake::Stored { .. }
    ));
    assert_eq!(w.engine.receive(t).unwrap(), Intake::Duplicate);
    assert_eq!(
        w.engine.repos().threads[&ThreadKey::new("th1")]
            .queued
            .len(),
        1
    );
}

#[test]
fn follow_ups_reach_the_ticket_without_an_eval_and_bots_are_context_only() {
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "do x",
    ));
    w.engine
        .decide(&eval, accept(session(), 1_000_000))
        .unwrap();
    let [(ticket, assignment, _)] = &launched(&w.effects())[..] else {
        panic!()
    };
    w.engine.assignment_started(ticket, assignment).unwrap();

    let intake = w
        .engine
        .receive(slack(
            "t2",
            Actor::person("alice"),
            Addressed::Unaddressed,
            "th1",
            "also y",
        ))
        .unwrap();
    assert_eq!(
        intake,
        Intake::Stored {
            reached: vec![ticket.clone()],
            queued: false
        }
    );
    assert!(matches!(&w.effects()[..], [Effect::Deliver { inputs, .. }] if inputs.len() == 1));

    let mut bot = slack(
        "t3",
        Actor::bot("other-bot"),
        Addressed::Unaddressed,
        "th1",
        "CI red",
    );
    bot.source = "github".into();
    let intake = w.engine.receive(bot).unwrap();
    assert_eq!(
        intake,
        Intake::Stored {
            reached: vec![ticket.clone()],
            queued: true
        }
    );
    w.effects();
    assert!(w.engine.evaluate_due().unwrap().is_empty());
    let ignored = w
        .engine
        .repos()
        .decisions
        .values()
        .find(|d| d.eval.is_none())
        .unwrap();
    assert_eq!(ignored.kind, DecisionKind::Ignore);
    assert!(
        ignored.reason.contains("bot_not_listed"),
        "{}",
        ignored.reason
    );
}

#[test]
fn an_unlisted_person_never_reaches_the_router() {
    let mut w = World::new(policy());
    w.engine
        .receive(slack(
            "t1",
            Actor::person("mallory"),
            Addressed::Mention,
            "th1",
            "@agent rm -rf",
        ))
        .unwrap();
    assert!(w.engine.evaluate_due().unwrap().is_empty());
    assert!(w.effects().iter().all(|e| !matches!(e, Effect::Assess(_))));
}

#[test]
fn triggers_that_arrive_during_an_eval_form_the_next_batch() {
    let mut w = World::new(policy());
    let alice = || Actor::person("alice");
    let first = w.receive_and_start_eval(slack("t1", alice(), Addressed::Mention, "th1", "one"));
    w.engine
        .receive(slack("t2", alice(), Addressed::Mention, "th1", "two"))
        .unwrap();
    w.engine
        .receive(slack("t3", alice(), Addressed::Mention, "th1", "three"))
        .unwrap();
    assert!(
        w.engine.evaluate_due().unwrap().is_empty(),
        "one eval per thread at a time"
    );
    w.engine
        .decide(
            &first,
            Verdict::Ignore {
                reason: "chatter".into(),
            },
        )
        .unwrap();
    w.effects();
    let next = w.engine.evaluate_due().unwrap();
    assert_eq!(next.len(), 1);
    let effects = w.effects();
    let [Effect::Assess(candidate)] = &effects[..] else {
        panic!("{effects:?}")
    };
    let ids: Vec<_> = candidate.triggers.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["t2", "t3"]);
}

#[test]
fn a_swarm_over_the_auto_budget_waits_for_an_approver() {
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "survey",
    ));
    let swarm = Plan::Swarm {
        agents: 20,
        goal_template: Some("research".into()),
    };
    w.engine
        .decide(&eval, accept(swarm.clone(), 25_000_000))
        .unwrap();
    let effects = w.effects();
    assert!(
        launched(&effects).is_empty(),
        "nothing runs before approval"
    );
    let [(MessageKind::Attention, ask)] = &sent(&effects)[..] else {
        panic!("{effects:?}")
    };
    assert!(
        ask.contains("a swarm of 20 agents") && ask.contains("$25.00"),
        "{ask}"
    );
    let ticket = w.engine.repos().tickets.keys().next().unwrap().clone();
    assert_eq!(w.engine.repos().tickets[&ticket].stage(), Stage::Approval);

    assert!(
        w.engine
            .approve(&ticket, "slack", Actor::person("bob"))
            .is_err()
    );
    w.engine
        .approve(&ticket, "slack", Actor::person("alice"))
        .unwrap();
    let effects = w.effects();
    assert_eq!(launched(&effects)[0].2, swarm);
    let t = &w.engine.repos().tickets[&ticket];
    assert!(matches!(t.approval, Approval::Granted { .. }));
    assert_eq!(t.stage(), Stage::Queued);
}

#[test]
fn a_session_plan_names_its_environment_and_one_stored_without_it_still_reads() {
    for stored in [
        r#"{"kind":"session"}"#,
        r#"{"kind":"session","harness":"pi"}"#,
    ] {
        let plan: Plan = serde_json::from_str(stored).unwrap();
        let Plan::Session { environment, .. } = &plan else {
            panic!("{stored} is not a session")
        };
        assert_eq!(*environment, None, "{stored}");
        assert_eq!(
            serde_json::to_string(&plan).unwrap(),
            stored,
            "it writes back unchanged"
        );
    }

    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "run it on my laptop",
    ));
    let on_laptop = Plan::Session {
        harness: None,
        environment: Some("laptop".into()),
    };
    w.engine
        .decide(&eval, accept(on_laptop.clone(), 500_000))
        .unwrap();
    assert_eq!(launched(&w.effects())[0].2, on_laptop);
    assert_eq!(
        serde_json::to_string(&on_laptop).unwrap(),
        r#"{"kind":"session","environment":"laptop"}"#
    );
}

#[test]
fn swarm_size_is_bounded_like_a_swarm_goal() {
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "big",
    ));
    let plan = Plan::Swarm {
        agents: 31,
        goal_template: None,
    };
    assert!(w.engine.decide(&eval, accept(plan, 1_000_000)).is_err());
}

#[test]
fn a_refused_command_commits_nothing() {
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "x",
    ));
    w.effects();
    let before = w.engine.repos().clone();
    assert!(
        w.engine.decide(&eval, accept(session(), 0)).is_err(),
        "a zero budget is refused"
    );
    let after = w.engine.repos();
    assert_eq!(
        format!("{:?}", before.threads),
        format!("{:?}", after.threads)
    );
    assert!(after.tickets.is_empty() && after.decisions.is_empty() && after.effects.is_empty());
    assert!(after.events.len() == before.events.len());
    w.engine
        .decide(
            &eval,
            Verdict::Ignore {
                reason: "still running".into(),
            },
        )
        .unwrap();
}

#[test]
fn inferred_work_needs_auto_mode_and_enabled_rules() {
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Unaddressed,
        "th1",
        "the deploy script crashes",
    ));
    let mut verdict = accept(session(), 1_000_000);
    if let Verdict::Accept(a) = &mut verdict {
        a.rules = vec!["reproduce-bugs".into()];
    }
    let decision = w.engine.decide(&eval, verdict.clone()).unwrap();
    let record = &w.engine.repos().decisions[&decision];
    assert_eq!(
        (record.kind, record.would_accept),
        (DecisionKind::Ignore, true)
    );
    assert!(w.engine.repos().tickets.is_empty());

    let mut w = World::new(AllowlistPolicy {
        accept_mode: AcceptMode::Auto,
        ..policy()
    });
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Unaddressed,
        "th1",
        "x",
    ));
    w.engine.decide(&eval, verdict).unwrap();
    assert_eq!(w.engine.repos().tickets.len(), 1);
}

#[test]
fn an_eval_expires_twice_then_records_unavailable() {
    let mut w = World::new(policy());
    let timeout = policy().eval_timeout_ms;
    w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "hello?",
    ));
    for round in 1..MAX_EXPIRED_EVALS {
        w.advance(timeout);
        assert_eq!(
            w.engine.evaluate_due().unwrap().len(),
            1,
            "requeued, round {round}"
        );
    }
    w.advance(timeout);
    assert!(w.engine.evaluate_due().unwrap().is_empty());
    let kinds: Vec<_> = w
        .engine
        .repos()
        .decisions
        .values()
        .map(|d| d.kind)
        .collect();
    assert_eq!(kinds, [DecisionKind::Unavailable]);
}

#[test]
fn a_person_resumes_ended_work_and_a_stopped_ticket_hears_nothing() {
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "do x",
    ));
    w.engine
        .decide(&eval, accept(session(), 1_000_000))
        .unwrap();
    let [(ticket, assignment, _)] = &launched(&w.effects())[..] else {
        panic!()
    };
    w.engine.assignment_started(ticket, assignment).unwrap();
    let ready = ReportIn {
        status: ReportStatus::Ready,
        text: "done".into(),
        question: None,
        spent_microusd: 400_000,
    };
    w.engine.report(ticket, assignment, ready).unwrap();
    w.effects();

    let back = slack(
        "t2",
        Actor::person("alice"),
        Addressed::Unaddressed,
        "th1",
        "one more",
    );
    w.engine.receive(back).unwrap();
    let effects = w.effects();
    let [(again, second, _)] = &launched(&effects)[..] else {
        panic!("{effects:?}")
    };
    assert_eq!(again, ticket);
    assert_ne!(second, assignment);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Launch { recovery: true, .. }))
    );

    w.engine.stop_ticket(ticket, "not needed").unwrap();
    let late = slack(
        "t3",
        Actor::person("alice"),
        Addressed::Unaddressed,
        "th1",
        "hello",
    );
    let intake = w.engine.receive(late).unwrap();
    assert_eq!(
        intake,
        Intake::Stored {
            reached: vec![],
            queued: true
        }
    );
}

#[test]
fn a_blocked_ticket_holds_bot_inputs_until_a_person_writes() {
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "do x",
    ));
    w.engine
        .decide(&eval, accept(session(), 1_000_000))
        .unwrap();
    let [(ticket, assignment, _)] = &launched(&w.effects())[..] else {
        panic!()
    };
    w.engine.assignment_started(ticket, assignment).unwrap();
    let blocked = ReportIn {
        status: ReportStatus::Blocked,
        text: "Which branch should I target?".into(),
        question: Some("Which branch should I target?".into()),
        spent_microusd: 0,
    };
    w.engine.report(ticket, assignment, blocked).unwrap();
    let effects = w.effects();
    assert_eq!(sent(&effects).len(), 1, "one question, asked once");
    assert_eq!(w.engine.repos().tickets[ticket].stage(), Stage::Answer);

    let mut ci = slack(
        "t2",
        Actor::bot("ci-bot"),
        Addressed::Unaddressed,
        "th1",
        "CI green",
    );
    ci.source = "github".into();
    w.engine.receive(ci).unwrap();
    assert!(
        !w.effects()
            .iter()
            .any(|e| matches!(e, Effect::Deliver { .. })),
        "held"
    );

    let answer = slack(
        "t3",
        Actor::person("alice"),
        Addressed::Unaddressed,
        "th1",
        "main",
    );
    w.engine.receive(answer).unwrap();
    let effects = w.effects();
    let [Effect::Deliver { inputs, .. }] = &effects[..] else {
        panic!("{effects:?}")
    };
    assert_eq!(
        inputs.len(),
        2,
        "the held bot input and the answer, in order"
    );
    assert_eq!(w.engine.repos().tickets[ticket].stage(), Stage::Working);
}

#[test]
fn a_policy_change_during_an_eval_rejudges_the_batch() {
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "do x",
    ));
    w.engine.set_policy(AllowlistPolicy {
        accept_mode: AcceptMode::Auto,
        ..policy()
    });
    let decision = w
        .engine
        .decide(&eval, accept(session(), 1_000_000))
        .unwrap();
    assert_eq!(
        w.engine.repos().decisions[&decision].kind,
        DecisionKind::Unavailable
    );
    assert!(w.engine.repos().tickets.is_empty());
    assert_eq!(w.engine.evaluate_due().unwrap().len(), 1, "judged again");
}

#[test]
fn a_stop_in_the_thread_is_handed_back_and_only_a_person_stops_work() {
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "do x",
    ));
    w.engine
        .decide(&eval, accept(session(), 1_000_000))
        .unwrap();
    let [(ticket, assignment, _)] = &launched(&w.effects())[..] else {
        panic!()
    };
    w.engine.assignment_started(ticket, assignment).unwrap();

    let mut bot = slack(
        "t2",
        Actor::bot("ci-bot"),
        Addressed::Unaddressed,
        "th1",
        "stop it",
    );
    bot.source = "github".into();
    w.engine.receive(bot).unwrap();
    let eval = w.engine.evaluate_due().unwrap().pop().unwrap();
    let stop = |reason: &str| Verdict::Stop {
        ticket: ticket.clone(),
        reason: reason.into(),
    };
    assert!(w.engine.decide(&eval, stop("bot")).is_err());
    w.engine
        .decide(
            &eval,
            Verdict::Ignore {
                reason: "a bot".into(),
            },
        )
        .unwrap();

    let please = slack(
        "t3",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "@agent stop",
    );
    let intake = w.engine.receive(please).unwrap();
    assert_eq!(
        intake,
        Intake::Stored {
            reached: vec![ticket.clone()],
            queued: false
        }
    );
    let t3 = TriggerId::new("t3");
    assert!(w.engine.handback(ticket, &t3, "a stop request").unwrap());
    assert!(!w.engine.handback(ticket, &t3, "again").unwrap(), "once");
    let eval = w.engine.evaluate_due().unwrap().pop().unwrap();
    w.engine.decide(&eval, stop("alice asked")).unwrap();
    let t = &w.engine.repos().tickets[ticket];
    assert!(t.stopped_by_person());
    assert!(w.effects().iter().any(|e| matches!(e, Effect::Stop { .. })));
}

#[test]
fn a_trigger_on_two_threads_reaches_each_ticket_once() {
    let mut w = World::new(policy());
    let mut first = slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "fix it",
    );
    first.threads = vec![ThreadKey::new("th1"), ThreadKey::new("pr-7")];
    let eval = w.receive_and_start_eval(first);
    w.engine
        .decide(&eval, accept(session(), 1_000_000))
        .unwrap();
    let ticket = w.engine.repos().tickets.keys().next().unwrap().clone();
    assert!(
        w.engine.repos().threads[&ThreadKey::new("pr-7")]
            .subscriptions
            .contains(&ticket)
    );

    let mut both = slack(
        "t2",
        Actor::person("alice"),
        Addressed::Unaddressed,
        "th1",
        "see PR",
    );
    both.threads = vec![
        ThreadKey::new("pr-7"),
        ThreadKey::new("th1"),
        ThreadKey::new("th1"),
    ];
    let intake = w.engine.receive(both).unwrap();
    assert_eq!(
        intake,
        Intake::Stored {
            reached: vec![ticket.clone()],
            queued: false
        }
    );
    let inputs = &w.engine.repos().tickets[&ticket].inputs;
    assert_eq!(inputs.len(), 1, "one Input, not one per Thread");
}

#[test]
fn retry_replaces_a_failed_assignment_and_late_reports_are_refused() {
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "do x",
    ));
    w.engine
        .decide(&eval, accept(session(), 1_000_000))
        .unwrap();
    let [(ticket, first, _)] = &launched(&w.effects())[..] else {
        panic!()
    };
    w.engine.assignment_started(ticket, first).unwrap();
    let failed = ReportIn {
        status: ReportStatus::Failed,
        text: "provider outage".into(),
        question: None,
        spent_microusd: 0,
    };
    w.engine.report(ticket, first, failed.clone()).unwrap();
    assert_eq!(w.engine.repos().tickets[ticket].stage(), Stage::Failed);
    w.effects();

    let second = w.engine.retry(ticket).unwrap().unwrap();
    let effects = w.effects();
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Stop { assignment, .. } if assignment == first))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Launch { recovery: true, .. }))
    );
    assert!(
        w.engine.report(ticket, first, failed).is_err(),
        "the old assignment is gone"
    );
    w.engine.assignment_started(ticket, &second).unwrap();
    assert_eq!(w.engine.repos().tickets[ticket].stage(), Stage::Working);
}

fn started_ticket(
    w: &mut World,
    thread: &str,
) -> (TicketId, solos_agent_core::model::AssignmentId) {
    let eval = w.receive_and_start_eval(slack(
        &format!("t0-{thread}"),
        Actor::person("alice"),
        Addressed::Mention,
        thread,
        "do x",
    ));
    w.engine
        .decide(&eval, accept(session(), 1_000_000))
        .unwrap();
    let [(ticket, assignment, _)] = &launched(&w.effects())[..] else {
        panic!()
    };
    let (ticket, assignment) = (ticket.clone(), assignment.clone());
    w.engine.assignment_started(&ticket, &assignment).unwrap();
    w.effects();
    (ticket, assignment)
}

fn report(status: ReportStatus) -> ReportIn {
    ReportIn {
        status,
        text: "t".into(),
        question: None,
        spent_microusd: 0,
    }
}

#[test]
fn a_restart_ends_the_lost_assignment_and_launches_a_recovery_in_one_commit() {
    let mut w = World::new(policy());
    let (ticket, first) = started_ticket(&mut w, "th1");

    let second = w
        .engine
        .restart(&ticket, &first, "host restarted")
        .unwrap()
        .unwrap();

    let effects = w.effects();
    let stops: Vec<_> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::Stop {
                assignment, reason, ..
            } => Some((assignment.clone(), reason.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(stops, [(first.clone(), "host restarted".to_owned())]);
    assert!(
        effects.iter().any(|e| matches!(e,
            Effect::Launch { assignment, recovery: true, .. } if *assignment == second)),
        "{effects:?}"
    );
    let t = &w.engine.repos().tickets[&ticket];
    assert_eq!(
        t.assignments[0].ended_reason.as_deref(),
        Some("host restarted")
    );
    assert_eq!(t.stage(), Stage::Queued);
    assert!(
        w.engine.restart(&ticket, &first, "again").is_err(),
        "only the latest assignment restarts"
    );
    w.engine
        .assignment_ended(&ticket, &second, "host restarted")
        .unwrap();
    assert!(
        w.engine.restart(&ticket, &second, "again").is_err(),
        "an ended assignment does not restart"
    );
    w.engine.stop_ticket(&ticket, "stopped").unwrap();
    assert!(w.engine.restart(&ticket, &second, "again").is_err());
}

#[test]
fn an_unlisted_follow_up_neither_resumes_work_nor_ends_a_hold() {
    let mut w = World::new(policy());
    let (ticket, assignment) = started_ticket(&mut w, "th1");
    w.engine
        .report(&ticket, &assignment, report(ReportStatus::Failed))
        .unwrap();
    let mut ci = slack(
        "c1",
        Actor::bot("ci-bot"),
        Addressed::Unaddressed,
        "th1",
        "CI red",
    );
    ci.source = "github".into();
    w.engine.receive(ci).unwrap();
    let nudge = slack(
        "m1",
        Actor::person("mallory"),
        Addressed::Mention,
        "th1",
        "@agent go on",
    );
    let intake = w.engine.receive(nudge).unwrap();
    assert_eq!(
        intake,
        Intake::Stored {
            reached: vec![ticket.clone()],
            queued: true
        }
    );
    assert!(
        !w.effects()
            .iter()
            .any(|e| matches!(e, Effect::Deliver { .. })),
        "still held"
    );
    assert_eq!(w.engine.repos().tickets[&ticket].stage(), Stage::Failed);

    let judged = w.engine.evaluate_due().unwrap();
    assert_eq!(
        judged.len(),
        1,
        "judged on its own, as an inferred candidate"
    );
    w.engine
        .decide(
            &judged[0],
            Verdict::Ignore {
                reason: "not listed".into(),
            },
        )
        .unwrap();
    w.engine.stop_ticket(&ticket, "reset").unwrap();
    let (done, assignment) = started_ticket(&mut w, "th2");
    w.engine
        .report(&done, &assignment, report(ReportStatus::Ready))
        .unwrap();
    w.effects();
    let again = slack(
        "m2",
        Actor::person("mallory"),
        Addressed::Mention,
        "th2",
        "@agent again",
    );
    w.engine.receive(again).unwrap();
    assert!(
        launched(&w.effects()).is_empty(),
        "an unlisted person relaunches nothing"
    );
    let again = slack(
        "a2",
        Actor::person("alice"),
        Addressed::Unaddressed,
        "th2",
        "again",
    );
    w.engine.receive(again).unwrap();
    assert_eq!(
        launched(&w.effects()).len(),
        1,
        "an allowed person resumes it"
    );
}

#[test]
fn handing_back_a_bot_trigger_queues_nothing() {
    let mut w = World::new(policy());
    let (ticket, _) = started_ticket(&mut w, "th1");
    let mut ci = slack(
        "c1",
        Actor::bot("ci-bot"),
        Addressed::Unaddressed,
        "th1",
        "CI red",
    );
    ci.source = "github".into();
    w.engine.receive(ci).unwrap();
    assert!(
        !w.engine
            .handback(&ticket, &TriggerId::new("c1"), "not mine")
            .unwrap()
    );
    let queued = &w.engine.repos().threads[&ThreadKey::new("th1")].queued;
    assert_eq!(
        queued,
        &vec![TriggerId::new("c1")],
        "judged once, on its own"
    );
}

#[test]
fn a_pause_starts_no_evals_but_still_records_work() {
    let mut w = World::new(policy());
    let (ticket, assignment) = started_ticket(&mut w, "th1");
    w.engine
        .report(&ticket, &assignment, report(ReportStatus::Ready))
        .unwrap();
    w.effects();
    w.engine.set_policy(AllowlistPolicy {
        paused: true,
        ..policy()
    });
    let back = slack(
        "a1",
        Actor::person("alice"),
        Addressed::Unaddressed,
        "th1",
        "one more",
    );
    let intake = w.engine.receive(back).unwrap();
    assert_eq!(
        intake,
        Intake::Stored {
            reached: vec![ticket.clone()],
            queued: false
        }
    );
    assert_eq!(
        launched(&w.effects()).len(),
        1,
        "the effect runner holds it"
    );
    let nudge = slack(
        "m1",
        Actor::person("mallory"),
        Addressed::Mention,
        "th2",
        "@agent hi",
    );
    w.engine.receive(nudge).unwrap();
    assert!(
        w.engine.evaluate_due().unwrap().is_empty(),
        "no Eval while paused"
    );
    w.engine.set_policy(policy());
    let nudge = slack(
        "a2",
        Actor::person("alice"),
        Addressed::Mention,
        "th3",
        "@agent hi",
    );
    w.engine.receive(nudge).unwrap();
    assert_eq!(
        w.engine.evaluate_due().unwrap().len(),
        1,
        "judged once the pause lifts"
    );
}
