//! Repros for budget, approval and Ticket-lifecycle claims of PR #18 that do
//! not hold at 25fc138. Each test asserts what the claim requires and fails.

use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, Approval, AssignmentId, AssignmentState, ChannelRef,
    Effect, EvalId, MessageKind, Plan, ReportIn, ReportStatus, Stage, ThreadKey, Ticket, TicketId,
    Trigger, TriggerId, Verdict,
};
use solos_agent_core::{AllowlistPolicy, Engine, MemoryRepositories};
use std::collections::BTreeMap;

type TestEngine = Engine<MemoryRepositories, AllowlistPolicy, Box<dyn Fn() -> u64>>;

struct World {
    engine: TestEngine,
}

fn policy() -> AllowlistPolicy {
    AllowlistPolicy {
        people: ["slack:alice".to_owned()].into(),
        approvers: ["slack:alice".to_owned()].into(),
        auto_approve_microusd: 2_000_000,
        ..AllowlistPolicy::default() // accept_mode: observe
    }
}

fn msg(id: &str, addressed: Addressed, thread: &str) -> Trigger {
    Trigger {
        id: TriggerId::new(id),
        source: "slack".into(),
        at_ms: 1_000,
        actor: Actor::person("alice"),
        addressed,
        text: "hi".into(),
        threads: vec![ThreadKey::new(thread)],
        reply_to: vec![ChannelRef {
            source: "slack".into(),
            address: BTreeMap::from([("thread_ts".to_owned(), thread.to_owned())]),
        }],
        meta: serde_json::Value::Null,
    }
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

fn launches(effects: &[Effect]) -> Vec<(AssignmentId, bool)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Launch {
                assignment,
                recovery,
                ..
            } => Some((assignment.clone(), *recovery)),
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

impl World {
    fn new() -> Self {
        let clock = Box::new(|| 1_000_u64) as Box<dyn Fn() -> u64>;
        Self {
            engine: Engine::new(MemoryRepositories::default(), policy(), clock),
        }
    }
    fn effects(&mut self) -> Vec<Effect> {
        self.engine.repos_mut().take_effects()
    }
    fn ticket(&self, id: &TicketId) -> Ticket {
        self.engine.repos().tickets[id].clone()
    }
    fn eval_of(&mut self, trigger: Trigger) -> EvalId {
        self.engine.receive(trigger).unwrap();
        let started = self.engine.evaluate_due().unwrap();
        assert_eq!(started.len(), 1);
        self.effects();
        started[0].clone()
    }
    fn accept_trigger(&mut self, trigger: Trigger, plan: Plan, budget: u64) -> TicketId {
        let eval = self.eval_of(trigger);
        let d = self.engine.decide(&eval, verdict(plan, budget)).unwrap();
        self.engine.repos().decisions[&d].ticket.clone().unwrap()
    }
    fn accept(&mut self, budget: u64) -> TicketId {
        self.accept_trigger(msg("ask", Addressed::Mention, "th1"), session(), budget)
    }
    fn running(&mut self) -> (TicketId, AssignmentId) {
        let t = self.accept(1_000_000);
        let [(a, _)] = &launches(&self.effects())[..] else {
            panic!()
        };
        self.engine.assignment_started(&t, a).unwrap();
        self.effects();
        (t, a.clone())
    }
    /// A Ticket accepted over the threshold waits for approval; then the
    /// operator raises the threshold above its budget and alice writes.
    fn waiting_then_threshold_raised(&mut self) -> (TicketId, Vec<Effect>, usize) {
        let t = self.accept(3_000_000);
        let eff = self.effects();
        assert!(launches(&eff).is_empty());
        assert_eq!(self.ticket(&t).approval, Approval::Waiting);
        self.engine.set_policy(AllowlistPolicy {
            auto_approve_microusd: 5_000_000,
            ..policy()
        });
        let before = self.ticket(&t).assignments.len();
        self.engine
            .receive(msg("follow-up", Addressed::Unaddressed, "th1"))
            .unwrap();
        (t, self.effects(), before)
    }
}

/// Claim 7: recovery is true exactly when an earlier Assignment existed.
#[test]
fn i1_first_launch_after_threshold_raise_says_recovery() {
    let mut w = World::new();
    let (t, eff, before) = w.waiting_then_threshold_raised();
    let l = launches(&eff);
    println!(
        "assignments before launch: {before}; launches: {l:?}; approval after: {:?}",
        w.ticket(&t).approval
    );
    assert_eq!(l.len(), 1);
    assert_eq!(
        l[0].1,
        before > 0,
        "recovery must be false: no earlier Assignment existed"
    );
}

/// Claim 5: stage() for a running Assignment whose budget no longer needs approval.
#[test]
fn i2_running_ticket_reports_stage_approval() {
    let mut w = World::new();
    let (t, eff, _) = w.waiting_then_threshold_raised();
    let a = launches(&eff)[0].0.clone();
    w.engine.assignment_started(&t, &a).unwrap();
    w.engine
        .report(&t, &a, rep(ReportStatus::Progress, None))
        .unwrap();
    let tk = w.ticket(&t);
    let retry = w.engine.retry(&t);
    println!(
        "assignment state={:?}; latest report={:?}; approval={:?}; stage={:?}; retry -> {retry:?}",
        tk.latest_assignment().map(|a| a.state),
        tk.latest_report().map(|r| r.status),
        tk.approval,
        tk.stage()
    );
    assert_eq!(tk.stage(), Stage::Working);
}

/// Issue 1, later consequence: the stale `Waiting` suppresses the ask when the
/// budget needs approval again (threshold back down, a person resumes it).
#[test]
fn i1c_stale_waiting_resumes_over_threshold_and_asks_nobody() {
    let mut w = World::new();
    let (t, eff, _) = w.waiting_then_threshold_raised();
    let a1 = launches(&eff)[0].0.clone();
    w.engine.assignment_started(&t, &a1).unwrap();
    w.engine
        .report(&t, &a1, rep(ReportStatus::Ready, None))
        .unwrap();
    w.effects();
    w.engine.set_policy(policy()); // threshold back to 2.00 < budget 3.00
    w.engine
        .receive(msg("one-more", Addressed::Unaddressed, "th1"))
        .unwrap();
    let eff = w.effects();
    let tk = w.ticket(&t);
    println!(
        "launches={:?}; sends={:?}; approval={:?}; attention={:?}; stage={:?}",
        launches(&eff),
        sends(&eff),
        tk.approval,
        tk.attention,
        tk.stage()
    );
    assert_eq!(
        sends(&eff)
            .iter()
            .filter(|(k, _)| *k == MessageKind::Attention)
            .count(),
        1,
        "over the threshold without a grant: ask a person"
    );
}

/// Claim 2: approve launches, but here over a live Assignment that it never stops.
#[test]
fn i3_approve_launches_a_second_live_assignment() {
    let mut w = World::new();
    let (t, eff, _) = w.waiting_then_threshold_raised();
    let a1 = launches(&eff)[0].0.clone();
    w.engine.assignment_started(&t, &a1).unwrap();
    w.effects();
    let r = w.engine.approve(&t, "slack", Actor::person("alice"));
    let eff = w.effects();
    let stopped: Vec<_> = eff
        .iter()
        .filter_map(|e| match e {
            Effect::Stop { assignment, .. } => Some(assignment.clone()),
            _ => None,
        })
        .collect();
    let tk = w.ticket(&t);
    let states: Vec<_> = tk
        .assignments
        .iter()
        .map(|a| (a.id.clone(), a.state))
        .collect();
    let late = w.engine.report(&t, &a1, rep(ReportStatus::Progress, None));
    println!(
        "approve -> {r:?}; launches={:?}; stops={stopped:?}; assignments={states:?}; report from a1 -> {late:?}",
        launches(&eff)
    );
    let live = tk
        .assignments
        .iter()
        .filter(|a| a.state != AssignmentState::Ended)
        .count();
    assert!(
        launches(&eff).is_empty() || stopped.contains(&a1),
        "approve launched a second Assignment while {a1} runs and sent no Stop for it \
         ({live} Assignments not ended)"
    );
}

/// Claim 3: `dropped` ends the Ticket with no message.
#[test]
fn i4_dropped_with_a_question_sends_a_message() {
    let mut w = World::new();
    let (t, a) = w.running();
    w.engine
        .report(
            &t,
            &a,
            rep(ReportStatus::Dropped, Some("Should I file it instead?")),
        )
        .unwrap();
    let eff = w.effects();
    let tk = w.ticket(&t);
    println!(
        "sends={:?}; ended={:?}; attention={:?}",
        sends(&eff),
        tk.ended.as_ref().map(|e| e.outcome),
        tk.attention
    );
    assert!(sends(&eff).is_empty(), "dropped must end silently");
}

/// Related to claim 3: a `ready` Report with a question asks it and ends the
/// Ticket in the same command, so the question is outstanding on an ended Ticket.
#[test]
fn i4b_ready_with_a_question_asks_on_an_ended_ticket() {
    let mut w = World::new();
    let (t, a) = w.running();
    w.engine
        .report(&t, &a, rep(ReportStatus::Ready, Some("Merge it?")))
        .unwrap();
    let eff = w.effects();
    let tk = w.ticket(&t);
    println!(
        "sends={:?}; ended={:?}; attention={:?}; stage={:?}",
        sends(&eff),
        tk.ended.as_ref().map(|e| e.outcome),
        tk.attention,
        tk.stage()
    );
    assert_eq!(
        sends(&eff),
        vec![(MessageKind::Result, "Ready text".to_owned())],
        "ready sends the result"
    );
}

/// Claim 6: an inferred (observe-mode) accept with an out-of-range swarm or a
/// zero budget is recorded as `would_accept` instead of refused.
#[test]
fn i5_observe_mode_records_invalid_accepts_as_would_accept() {
    let mut recorded = Vec::new();
    for (agents, budget) in [(0u16, 1_000_000u64), (31, 1_000_000), (5, 0)] {
        let mut w = World::new();
        // alice is listed but did not address the agent and the Thread has no
        // Ticket: an inferred candidate.
        let eval = w.eval_of(msg("noticed", Addressed::Unaddressed, "th1"));
        let plan = Plan::Swarm {
            agents,
            goal_template: None,
        };
        let r = w.engine.decide(&eval, verdict(plan, budget));
        let rec = r
            .as_ref()
            .ok()
            .map(|d| w.engine.repos().decisions[d].clone());
        println!(
            "agents={agents} budget={budget}: decide -> {r:?}; kind={:?} would_accept={:?} reason={:?}",
            rec.as_ref().map(|d| d.kind),
            rec.as_ref().map(|d| d.would_accept),
            rec.as_ref().map(|d| d.reason.clone())
        );
        if r.is_ok() && rec.is_some_and(|d| d.would_accept) {
            recorded.push((agents, budget));
        }
    }
    assert!(
        recorded.is_empty(),
        "must be refused, not recorded as would_accept: {recorded:?}"
    );
}
