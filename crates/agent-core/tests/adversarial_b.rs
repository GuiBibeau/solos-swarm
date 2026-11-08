//! Regressions from the adversarial review of P0. Each test failed at 80e2fbe.

use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, ChannelRef, Effect, Plan, ReportIn, ReportStatus,
    ThreadKey, TicketId, Trigger, TriggerId, Verdict,
};
use solos_agent_core::{AllowlistPolicy, Engine, MemoryRepositories};
use std::collections::BTreeMap;

type TestEngine = Engine<MemoryRepositories, AllowlistPolicy, Box<dyn Fn() -> u64>>;

struct World {
    engine: TestEngine,
}

impl World {
    fn new(policy: AllowlistPolicy) -> Self {
        let clock = Box::new(|| 1_000_u64) as Box<dyn Fn() -> u64>;
        Self {
            engine: Engine::new(MemoryRepositories::default(), policy, clock),
        }
    }
    fn effects(&mut self) -> Vec<Effect> {
        self.engine.repos_mut().take_effects()
    }
    fn assess(&mut self, trigger: Trigger) -> solos_agent_core::model::EvalId {
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

fn live(w: &mut World) -> (TicketId, solos_agent_core::model::AssignmentId) {
    let eval = w.assess(slack(
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
    w.effects();
    (ticket.clone(), assignment.clone())
}
fn rep(status: ReportStatus) -> ReportIn {
    ReportIn {
        status,
        text: "t".into(),
        question: None,
        spent_microusd: 0,
        harvest: None,
    }
}
fn ci(id: &str) -> Trigger {
    let mut t = slack(
        id,
        Actor::bot("ci-bot"),
        Addressed::Unaddressed,
        "th1",
        "CI",
    );
    t.source = "github".into();
    t
}

#[test]
fn unlisted_person_resumes_ended_ticket() {
    let mut w = World::new(policy());
    let (t, a) = live(&mut w);
    w.engine.report(&t, &a, rep(ReportStatus::Ready)).unwrap();
    w.effects();
    w.engine
        .receive(slack(
            "m1",
            Actor::person("mallory"),
            Addressed::Unaddressed,
            "th1",
            "again",
        ))
        .unwrap();
    let eff = w.effects();
    assert!(
        launched(&eff).is_empty(),
        "unlisted person must not relaunch work: {eff:?}"
    );
}

#[test]
fn unlisted_person_releases_held_inputs() {
    let mut w = World::new(policy());
    let (t, a) = live(&mut w);
    w.engine.report(&t, &a, rep(ReportStatus::Failed)).unwrap();
    w.engine.receive(ci("c1")).unwrap();
    w.engine
        .receive(slack(
            "m1",
            Actor::person("mallory"),
            Addressed::Unaddressed,
            "th1",
            "hi",
        ))
        .unwrap();
    let eff = w.effects();
    assert!(
        !eff.iter().any(|e| matches!(e, Effect::Deliver { .. })),
        "{eff:?}"
    );
}

#[test]
fn handback_of_queued_trigger_is_not_judged_twice() {
    let mut w = World::new(policy());
    let (t, a) = live(&mut w);
    let _ = a;
    w.engine.receive(ci("c1")).unwrap();
    w.engine
        .handback(&t, &TriggerId::new("c1"), "not mine")
        .unwrap();
    w.engine.evaluate_due().unwrap();
    let eff = w.effects();
    let Some(Effect::Assess(c)) = eff.iter().find(|e| matches!(e, Effect::Assess(_))) else {
        panic!("{eff:?}")
    };
    let n = c.triggers.iter().filter(|x| x.id.as_str() == "c1").count();
    assert_eq!(n, 1, "c1 appears {n} times in the batch");
}
