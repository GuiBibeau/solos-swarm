use solos_agent_core::change::{Deliverable, ForgeTerms, MergeMode, RepoName};
use solos_agent_core::engine::REMIND_WITHIN_MS;
use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, Approval, Ask, ChannelRef, DecisionKind, DomainEvent,
    Effect, Mandate, MessageKind, Method, Plan, ReportIn, ReportStatus, Stage, ThreadKey, TicketId,
    TicketOutcome, Trigger, TriggerId, Verdict,
};
use solos_agent_core::{
    AcceptMode, AllowlistPolicy, Engine, Error, Intake, MAX_EXPIRED_EVALS, MemoryRepositories,
    Repositories,
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
        vec![(MessageKind::Acknowledgement, "Looking into it.".to_owned())]
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
        harvest: None,
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
fn a_declined_accept_records_a_declined_decision_and_tells_the_requester_once() {
    let told = "I can't work on acme/widgets here: its code may go only to anthropic, and this \
                agent runs no anthropic model. Nothing was started.";
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "@agent fix the widgets",
    ));
    w.effects();
    let decision = w
        .engine
        .decide(
            &eval,
            Verdict::Decline {
                reason: told.into(),
                told: told.into(),
            },
        )
        .unwrap();
    let record = w.engine.repos().decisions[&decision].clone();
    assert_eq!(record.kind, DecisionKind::Declined);
    assert_eq!(record.ticket, None);
    assert!(record.reason.contains("Nothing was started"), "{record:?}");
    assert!(w.engine.repos().tickets.is_empty(), "nothing started");
    let effects = w.effects();
    assert!(launched(&effects).is_empty());
    assert_eq!(sent(&effects), [(MessageKind::Result, told.to_owned())]);
    let [Effect::Send(message)] = &effects[..] else {
        panic!("one send: {effects:?}")
    };
    assert_eq!(message.ticket, None);
    assert_eq!(message.channel.address["thread_ts"], "th1");

    // The Eval is decided once, and no second sentence goes out.
    assert!(
        w.engine
            .decide(
                &eval,
                Verdict::Decline {
                    reason: told.into(),
                    told: told.into(),
                }
            )
            .is_err()
    );
    assert!(w.effects().is_empty());
}

#[test]
fn a_declined_inferred_accept_tells_nobody() {
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Unaddressed,
        "th1",
        "the deploy script crashes",
    ));
    w.effects();
    let decision = w
        .engine
        .decide(
            &eval,
            Verdict::Decline {
                reason: "declined".into(),
                told: "declined".into(),
            },
        )
        .unwrap();
    assert_eq!(
        w.engine.repos().decisions[&decision].kind,
        DecisionKind::Declined
    );
    assert!(
        w.effects().is_empty(),
        "an inferred request buys no message, so an outsider cannot make the agent post"
    );
}

#[test]
fn a_reply_answers_the_person_in_their_thread_and_starts_nothing() {
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "@agent what are you working on?",
    ));
    w.effects();
    let text = "Two tickets: one on the flaky test, one on the release notes.";
    let decision = w
        .engine
        .decide(
            &eval,
            Verdict::Reply {
                text: text.into(),
                reason: "a question about the agent's own work".into(),
            },
        )
        .unwrap();
    let record = w.engine.repos().decisions[&decision].clone();
    assert_eq!(record.kind, DecisionKind::Answered);
    assert_eq!(record.ticket, None);
    assert_eq!(record.reply.as_deref(), Some(text));
    assert!(w.engine.repos().tickets.is_empty(), "no ticket");
    let effects = w.effects();
    assert!(launched(&effects).is_empty());
    assert_eq!(sent(&effects), [(MessageKind::Result, text.to_owned())]);
    let [Effect::Send(message)] = &effects[..] else {
        panic!("one send: {effects:?}")
    };
    assert_eq!(message.ticket, None);
    assert_eq!(message.channel.address["thread_ts"], "th1");

    // The Eval is decided once, and no second message goes out.
    assert!(
        w.engine
            .decide(
                &eval,
                Verdict::Reply {
                    text: text.into(),
                    reason: "again".into(),
                }
            )
            .is_err()
    );
    assert!(w.effects().is_empty());
}

#[test]
fn a_reply_to_an_inferred_candidate_is_refused_and_says_nothing() {
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Unaddressed,
        "th1",
        "what is the deploy doing",
    ));
    w.effects();
    let refused = w.engine.decide(
        &eval,
        Verdict::Reply {
            text: "It is deploying.".into(),
            reason: "status".into(),
        },
    );
    assert_eq!(
        refused,
        Err(Error::Refused(
            "only an explicit request is answered".into()
        ))
    );
    assert!(w.engine.repos().decisions.is_empty(), "nothing recorded");
    assert!(w.effects().is_empty());

    // An empty reply is not an answer.
    let explicit = w.receive_and_start_eval(slack(
        "t2",
        Actor::person("alice"),
        Addressed::Mention,
        "th2",
        "@agent hello",
    ));
    w.effects();
    assert!(matches!(
        w.engine.decide(
            &explicit,
            Verdict::Reply {
                text: "  ".into(),
                reason: "chatter".into(),
            }
        ),
        Err(Error::Invalid(_))
    ));
    assert!(w.effects().is_empty());
}

#[test]
fn the_first_reply_is_a_natural_line_the_routers_own_words_or_nothing() {
    use solos_agent_core::model::STANDARD_ACKNOWLEDGEMENT;
    let first_reply = |acknowledge: Acknowledge| {
        let mut w = World::new(policy());
        let eval = w.receive_and_start_eval(slack(
            "t1",
            Actor::person("alice"),
            Addressed::Mention,
            "th1",
            "@agent what is MAX_SWARM_AGENTS?",
        ));
        w.effects();
        let Verdict::Accept(mut accept) = accept(session(), 1_000_000) else {
            unreachable!()
        };
        accept.acknowledge = acknowledge;
        w.engine.decide(&eval, Verdict::Accept(accept)).unwrap();
        sent(&w.effects())
    };

    // The standard line is a sentence a person would say. It never carries the
    // Ticket's title ("Fix the flaky test"), which read as a label.
    assert_eq!(STANDARD_ACKNOWLEDGEMENT, "Looking into it.");
    let standard = first_reply(Acknowledge::Standard);
    assert_eq!(
        standard,
        [(MessageKind::Acknowledgement, "Looking into it.".to_owned())]
    );
    assert!(!standard[0].1.contains("On it"), "{standard:?}");
    assert!(!standard[0].1.contains("flaky"), "{standard:?}");

    // The Router's own words go out as written; silence sends nothing.
    assert_eq!(
        first_reply(Acknowledge::Text(
            "Checking the constant and HEAD now.".into()
        )),
        [(
            MessageKind::Acknowledgement,
            "Checking the constant and HEAD now.".to_owned()
        )]
    );
    assert!(first_reply(Acknowledge::Silent).is_empty());
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
fn without_pass_through_a_follow_up_reaches_the_ticket_and_the_router() {
    let mut w = World::new(policy());
    w.engine.set_pass_through(false);
    let alice = || Actor::person("alice");
    w.engine
        .receive(slack("t0", alice(), Addressed::Mention, "th1", "do x"))
        .unwrap();
    w.engine
        .receive(slack("t1", alice(), Addressed::Unaddressed, "th1", "and z"))
        .unwrap();
    let started = w.engine.evaluate_due().unwrap();
    let [eval] = &started[..] else {
        panic!("{started:?}")
    };
    w.engine.decide(eval, accept(session(), 1_000_000)).unwrap();
    let [(ticket, assignment, _)] = &launched(&w.effects())[..] else {
        panic!()
    };
    w.engine.assignment_started(ticket, assignment).unwrap();
    w.effects();
    let thread = w.engine.repos().threads[&ThreadKey::new("th1")].clone();
    assert_eq!(
        thread.queued,
        [TriggerId::new("t1")],
        "an adopted follow-up stays queued"
    );

    let intake = w
        .engine
        .receive(slack(
            "t2",
            alice(),
            Addressed::Unaddressed,
            "th1",
            "also y",
        ))
        .unwrap();
    assert_eq!(
        intake,
        Intake::Stored {
            reached: vec![ticket.clone()],
            queued: true
        }
    );
    let effects = w.effects();
    assert!(
        matches!(&effects[..], [Effect::Deliver { inputs, .. }] if inputs.len() == 1),
        "the follow-up reaches the worker: {effects:?}"
    );
    let next = w.engine.evaluate_due().unwrap();
    assert_eq!(next.len(), 1, "the Router judges the follow-ups too");
    let effects = w.effects();
    let [Effect::Assess(candidate)] = &effects[..] else {
        panic!("{effects:?}")
    };
    let ids: Vec<_> = candidate.triggers.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["t1", "t2"]);
    assert_eq!(candidate.tickets.len(), 1);
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

/// What the gate said about one trigger, from the Ignore Decision it recorded.
fn gated(w: &World, trigger: &str) -> Option<String> {
    w.engine
        .repos()
        .decisions
        .values()
        .filter(|d| d.eval.is_none())
        .find(|d| d.triggers.iter().any(|t| t.as_str() == trigger))
        .map(|d| d.reason.clone())
}

#[test]
fn an_unlisted_person_on_a_thread_whose_ticket_ended_costs_no_router_call() {
    let mut w = World::new(policy());
    let (ticket, assignment) = started_ticket(&mut w, "th1");
    w.engine
        .report(&ticket, &assignment, report(ReportStatus::Ready))
        .unwrap();
    w.effects();
    assert!(w.engine.repos().tickets[&ticket].ended.is_some());

    w.engine
        .receive(slack(
            "m1",
            Actor::person("mallory"),
            Addressed::Mention,
            "th1",
            "@agent do something else",
        ))
        .unwrap();
    assert!(w.engine.evaluate_due().unwrap().is_empty());
    assert!(w.effects().iter().all(|e| !matches!(e, Effect::Assess(_))));
    let why = gated(&w, "m1").expect("the gate recorded it");
    assert!(why.contains("m1: not_listed"), "{why}");
}

#[test]
fn an_unlisted_person_on_a_thread_with_an_open_ticket_is_still_judged_as_inferred() {
    let mut w = World::new(policy());
    let (ticket, _) = started_ticket(&mut w, "th1");
    assert!(w.engine.repos().tickets[&ticket].ended.is_none());

    w.engine
        .receive(slack(
            "m1",
            Actor::person("mallory"),
            Addressed::Mention,
            "th1",
            "@agent do something else",
        ))
        .unwrap();
    let judged = w.engine.evaluate_due().unwrap();
    assert_eq!(judged.len(), 1, "one eval starts: {judged:?}");
    let assessed: Vec<_> = w
        .effects()
        .into_iter()
        .filter_map(|e| match e {
            Effect::Assess(c) => Some(c),
            _ => None,
        })
        .collect();
    let [candidate] = &assessed[..] else {
        panic!("one assessment: {assessed:?}")
    };
    assert_eq!(
        candidate.strength,
        solos_agent_core::model::Strength::Inferred
    );
    assert!(gated(&w, "m1").is_none());
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

/// An accept with no budget, as an operator who sets none gets.
fn uncapped(plan: Plan) -> Verdict {
    let Verdict::Accept(accept) = accept(plan, 1) else {
        unreachable!()
    };
    Verdict::Accept(Accept {
        budget_microusd: None,
        upstream: None,
        ..accept
    })
}

#[test]
fn a_session_with_no_budget_launches_at_once_whatever_the_approval_threshold_is() {
    for threshold in [0, 2_000_000] {
        let mut w = World::new(AllowlistPolicy {
            auto_approve_microusd: threshold,
            ..policy()
        });
        let eval = w.receive_and_start_eval(slack(
            "t1",
            Actor::person("alice"),
            Addressed::Mention,
            "th1",
            "fix the flake",
        ));
        w.engine.decide(&eval, uncapped(session())).unwrap();
        let effects = w.effects();
        let [(ticket, _, plan)] = &launched(&effects)[..] else {
            panic!("one launch at threshold {threshold}: {effects:?}")
        };
        assert_eq!(*plan, session());
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::Launch {
                    budget_microusd: None,
                    ..
                }
            )),
            "the launch carries no budget: {effects:?}"
        );
        let t = &w.engine.repos().tickets[ticket];
        assert_eq!(t.mandate.budget_microusd, None);
        assert_eq!(t.approval, Approval::NotNeeded);
        assert!(
            sent(&effects)
                .iter()
                .all(|(kind, _)| *kind != MessageKind::Attention),
            "nobody is asked to approve anything: {effects:?}"
        );
    }
}

#[test]
fn a_swarm_needs_a_budget_and_a_session_may_go_without() {
    let swarm = Plan::Swarm {
        agents: 4,
        goal_template: None,
    };
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "survey",
    ));
    w.effects();
    let refused = w.engine.decide(&eval, uncapped(swarm)).unwrap_err();
    assert!(
        matches!(&refused, Error::Invalid(why) if why.contains("swarm needs a budget")),
        "{refused:?}"
    );
    assert!(
        w.engine.repos().tickets.is_empty() && w.engine.repos().effects.is_empty(),
        "a refused accept commits nothing"
    );
    w.engine.decide(&eval, uncapped(session())).unwrap();
}

#[test]
fn a_mandate_without_a_budget_stores_without_the_field_and_reads_back() {
    let stored = r#"{"policy_version":"p1","scope":"fix it","strength":"explicit","rules":[],"requester":{"id":"alice","kind":"person"},"source":"slack"}"#;
    let mandate: Mandate = serde_json::from_str(stored).unwrap();
    assert_eq!(mandate.budget_microusd, None);
    assert_eq!(serde_json::to_string(&mandate).unwrap(), stored);
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
        method: None,
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
fn a_session_on_a_repository_keeps_it_in_its_mandate_and_what_it_owes_in_its_plan() {
    let widgets = RepoName::try_from("acme/widgets".to_owned()).unwrap();
    let owes = |deliverable| Plan::Session {
        harness: None,
        environment: None,
        method: Some(Method { deliverable }),
    };
    let on = |plan: Plan, repo: Option<&RepoName>| {
        let Verdict::Accept(mut accepted) = accept(plan, 500_000) else {
            panic!("accept() makes an accept")
        };
        accepted.repo = repo.cloned();
        let owed = matches!(
            &accepted.plan,
            Plan::Session { method: Some(m), .. } if m.deliverable > Deliverable::Answer
        );
        accepted.forge = (owed && repo.is_some()).then_some(ForgeTerms {
            merge: MergeMode::Ask,
            ci_expected: true,
            max_pushes: 6,
            max_repairs: 2,
        });
        Verdict::Accept(accepted)
    };
    let mut w = World::new(policy());
    let eval = w.receive_and_start_eval(slack(
        "t1",
        Actor::person("alice"),
        Addressed::Mention,
        "th1",
        "fix the flake in acme/widgets",
    ));
    let swarm = Plan::Swarm {
        agents: 2,
        goal_template: None,
    };
    for (refused, why) in [
        (
            on(swarm, Some(&widgets)),
            "a repository is for a session plan only",
        ),
        (
            on(owes(Deliverable::Change), None),
            "a change or a pull request needs a repository",
        ),
        (
            on(owes(Deliverable::PullRequest), None),
            "a change or a pull request needs a repository",
        ),
        (
            {
                let Verdict::Accept(mut bare) = on(owes(Deliverable::PullRequest), Some(&widgets))
                else {
                    unreachable!()
                };
                bare.forge = None;
                Verdict::Accept(bare)
            },
            "a change or a pull request needs the forge terms of its repository",
        ),
    ] {
        let err = w.engine.decide(&eval, refused).unwrap_err();
        assert_eq!(err, Error::Invalid(why.into()));
    }

    let pull_request = owes(Deliverable::PullRequest);
    let decision = w
        .engine
        .decide(&eval, on(pull_request.clone(), Some(&widgets)))
        .unwrap();
    let id = w
        .engine
        .repos()
        .decision(&decision)
        .unwrap()
        .ticket
        .unwrap();
    let ticket = w.engine.repos().ticket(&id).unwrap();
    assert_eq!(ticket.mandate.repo, Some(widgets));
    assert_eq!(launched(&w.effects())[0].2, pull_request);
    let written = serde_json::to_value(&ticket).unwrap();
    assert_eq!(written["mandate"]["repo"], "acme/widgets");
    assert_eq!(
        written["plan"],
        serde_json::json!({"kind":"session","method":{"deliverable":"pull_request"}})
    );

    let answer = w.receive_and_start_eval(slack(
        "t2",
        Actor::person("alice"),
        Addressed::Mention,
        "th2",
        "what is a flake?",
    ));
    w.engine
        .decide(&answer, on(owes(Deliverable::Answer), None))
        .unwrap();
    assert_eq!(launched(&w.effects())[0].2, owes(Deliverable::Answer));
}

#[test]
fn a_mandate_stored_before_repositories_reads_and_writes_back_unchanged() {
    let stored = r#"{"policy_version":"p1","scope":"fix it","strength":"explicit","rules":[],"requester":{"id":"alice","kind":"person"},"source":"slack","budget_microusd":500000}"#;
    let mandate: Mandate = serde_json::from_str(stored).unwrap();
    assert_eq!(mandate.repo, None);
    assert_eq!(serde_json::to_string(&mandate).unwrap(), stored);
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
        harvest: None,
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

    w.engine
        .stop_ticket(ticket, "slack", Actor::person("alice"), "not needed")
        .unwrap();
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
        harvest: None,
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
        harvest: None,
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
        harvest: None,
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
    w.engine
        .stop_ticket(&ticket, "slack", Actor::person("alice"), "stopped")
        .unwrap();
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
    w.engine
        .stop_ticket(&ticket, "slack", Actor::person("alice"), "reset")
        .unwrap();
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

/// The Engine, not the shell, decides who may stop: a refused stop commits nothing.
#[test]
fn the_engine_refuses_a_stop_from_anyone_the_policy_does_not_allow() {
    let mut w = World::new(policy());
    let (ticket, _) = started_ticket(&mut w, "th1");
    let before = format!("{:?}", w.engine.repos());
    let refused = [
        ("github", Actor::person("alice")),
        ("slack", Actor::person("mallory")),
        ("slack", Actor::bot("ci-bot")),
        ("slack", Actor::system("schedule")),
    ];
    for (source, by) in refused {
        let result = w.engine.stop_ticket(&ticket, source, by.clone(), "stopped");
        assert!(
            matches!(result, Err(Error::Forbidden(_))),
            "{source} {by:?}: {result:?}"
        );
        assert_eq!(format!("{:?}", w.engine.repos()), before, "{source} {by:?}");
    }
    assert!(w.effects().is_empty());
    w.engine
        .stop_ticket(&ticket, "slack", Actor::person("alice"), "stopped")
        .unwrap();
    assert!(
        w.engine
            .repos()
            .ticket(&ticket)
            .unwrap()
            .stopped_by_person()
    );
}

/// A listed person may ask for work and so may stop it, without approving budgets.
#[test]
fn a_listed_person_who_cannot_approve_may_still_stop_their_work() {
    let mut p = policy();
    p.people.insert("slack:bob".into());
    let mut w = World::new(p);
    let (ticket, _) = started_ticket(&mut w, "th1");
    let bob = Actor::person("bob");
    assert!(matches!(
        w.engine.approve(&ticket, "slack", bob.clone()),
        Err(Error::Forbidden(_))
    ));
    w.engine
        .stop_ticket(&ticket, "slack", bob, "enough")
        .unwrap();
    assert!(
        w.engine
            .repos()
            .ticket(&ticket)
            .unwrap()
            .stopped_by_person()
    );
}

const HOUR: u64 = 3_600_000;

fn asking(question: &str) -> ReportIn {
    ReportIn {
        status: ReportStatus::Blocked,
        text: question.into(),
        question: Some(question.into()),
        spent_microusd: 0,
        harvest: None,
    }
}

fn reminders(w: &World) -> usize {
    let events = &w.engine.repos().events;
    let told = events
        .iter()
        .filter(|e| matches!(e.event, DomainEvent::Reminded { .. }));
    told.count()
}

#[test]
fn a_ticket_that_waits_for_a_person_has_its_thread_reminded_once_per_wait() {
    let mut w = World::new(policy());
    let (ticket, assignment) = started_ticket(&mut w, "th1");
    w.engine
        .report(
            &ticket,
            &assignment,
            asking("Which branch should I target?"),
        )
        .unwrap();
    assert_eq!(sent(&w.effects()).len(), 1, "the question, asked once");

    assert!(!w.engine.remind(&ticket, 4 * HOUR).unwrap());
    w.advance(4 * HOUR - 1);
    assert!(
        !w.engine.remind(&ticket, 4 * HOUR).unwrap(),
        "a millisecond short"
    );
    assert!(w.effects().is_empty());
    w.advance(1);
    assert!(w.engine.remind(&ticket, 4 * HOUR).unwrap());

    let effects = w.effects();
    let [Effect::Send(message)] = &effects[..] else {
        panic!("{effects:?}")
    };
    assert_eq!(message.kind, MessageKind::Attention);
    assert_eq!(
        message.channel.address["thread_ts"], "th1",
        "in its own thread"
    );
    assert_eq!(message.ticket.as_ref(), Some(&ticket));
    assert_eq!(
        message.text,
        "I'm still waiting for your answer. I asked: Which branch should I target?"
    );
    assert_eq!(message.ask, None);
    assert_eq!(reminders(&w), 1);

    for later in [0, HOUR, 24 * HOUR] {
        w.advance(later);
        assert!(
            !w.engine.remind(&ticket, 4 * HOUR).unwrap(),
            "once per wait"
        );
    }
    assert!(w.effects().is_empty());
    assert_eq!(reminders(&w), 1);
}

#[test]
fn a_new_wait_is_reminded_of_again_and_an_answered_one_is_not() {
    let mut w = World::new(policy());
    let (ticket, assignment) = started_ticket(&mut w, "th1");
    w.engine
        .report(
            &ticket,
            &assignment,
            asking("Which branch should I target?"),
        )
        .unwrap();
    w.advance(5 * HOUR);
    assert!(w.engine.remind(&ticket, 4 * HOUR).unwrap());
    w.effects();

    let answer = slack(
        "t2",
        Actor::person("alice"),
        Addressed::Unaddressed,
        "th1",
        "main",
    );
    w.engine.receive(answer).unwrap();
    w.effects();
    assert_eq!(w.engine.repos().tickets[&ticket].stage(), Stage::Working);
    w.advance(10 * HOUR);
    assert!(
        !w.engine.remind(&ticket, 4 * HOUR).unwrap(),
        "a Ticket that works waits for nobody"
    );

    w.engine
        .report(&ticket, &assignment, asking("And the remote?"))
        .unwrap();
    w.effects();
    w.advance(4 * HOUR);
    assert!(w.engine.remind(&ticket, 4 * HOUR).unwrap(), "a second wait");
    assert_eq!(
        sent(&w.effects())[0].1,
        "I'm still waiting for your answer. I asked: And the remote?"
    );
    assert_eq!(reminders(&w), 2);
}

#[test]
fn a_failed_or_blocked_ticket_and_an_unapproved_budget_are_reminded_in_their_own_words() {
    let mut w = World::new(policy());
    let (ticket, assignment) = started_ticket(&mut w, "th1");
    let failed = ReportIn {
        status: ReportStatus::Failed,
        text: "provider outage".into(),
        question: None,
        spent_microusd: 0,
        harvest: None,
    };
    w.engine.report(&ticket, &assignment, failed).unwrap();
    w.effects();
    w.advance(4 * HOUR);
    assert!(w.engine.remind(&ticket, 4 * HOUR).unwrap());
    assert_eq!(
        sent(&w.effects())[0].1,
        "I'm still waiting on a person after a failure. I said: provider outage. Reply here \
         and I will try again."
    );

    let (blocked, assignment) = started_ticket(&mut w, "th2");
    let report = ReportIn {
        status: ReportStatus::Blocked,
        text: "I need the staging password".into(),
        question: None,
        spent_microusd: 0,
        harvest: None,
    };
    w.engine.report(&blocked, &assignment, report).unwrap();
    w.effects();
    w.advance(4 * HOUR);
    assert!(w.engine.remind(&blocked, 4 * HOUR).unwrap());
    assert_eq!(
        sent(&w.effects())[0].1,
        "I'm still waiting on a person. I said: I need the staging password. Reply here and I \
         will pick the work back up."
    );

    let eval = w.receive_and_start_eval(slack(
        "t9",
        Actor::person("alice"),
        Addressed::Mention,
        "th3",
        "survey",
    ));
    let swarm = Plan::Swarm {
        agents: 20,
        goal_template: Some("research".into()),
    };
    w.engine.decide(&eval, accept(swarm, 25_000_000)).unwrap();
    w.effects();
    let approval = w
        .engine
        .repos()
        .tickets
        .values()
        .find(|t| t.stage() == Stage::Approval)
        .unwrap()
        .id
        .clone();
    w.advance(4 * HOUR);
    assert!(w.engine.remind(&approval, 4 * HOUR).unwrap());
    let effects = w.effects();
    let [Effect::Send(message)] = &effects[..] else {
        panic!("{effects:?}")
    };
    assert!(
        message
            .text
            .starts_with("I'm still waiting for a person to approve this. ")
            && message.text.contains("a swarm of 20 agents"),
        "{}",
        message.text
    );
    assert_eq!(message.ask, Some(Ask::Budget), "the buttons come back");
}

#[test]
fn a_wait_nobody_can_answer_or_a_week_old_is_not_reminded_of() {
    let mut w = World::new(policy());
    let (ticket, assignment) = started_ticket(&mut w, "th1");
    w.advance(10 * HOUR);
    assert!(
        !w.engine.remind(&ticket, 4 * HOUR).unwrap(),
        "it is working"
    );

    w.engine
        .report(
            &ticket,
            &assignment,
            asking("Which branch should I target?"),
        )
        .unwrap();
    w.effects();
    w.advance(REMIND_WITHIN_MS + 1);
    assert!(
        !w.engine.remind(&ticket, 4 * HOUR).unwrap(),
        "the person has had a week"
    );

    let (ended, assignment) = started_ticket(&mut w, "th2");
    w.engine
        .report(&ended, &assignment, asking("Which branch should I target?"))
        .unwrap();
    w.engine
        .stop_ticket(&ended, "slack", Actor::person("alice"), "enough")
        .unwrap();
    w.advance(5 * HOUR);
    assert!(!w.engine.remind(&ended, 4 * HOUR).unwrap(), "it ended");
    assert!(!w.engine.remind(&TicketId::new("tk_none"), HOUR).unwrap());
    assert_eq!(reminders(&w), 0);
}
