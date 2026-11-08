//! Regressions from the adversarial review of P0. Each test failed at 80e2fbe.

use solos_agent_core::memo::Memo;
use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, AssignmentId, ChangeSet, ChannelRef, Decision,
    DecisionId, Effect, EvalId, MemoId, Plan, ReportIn, ReportStatus, Stage, Strength, Thread,
    ThreadKey, Ticket, TicketId, Trigger, TriggerId, Verdict,
};
use solos_agent_core::trail::TrailCursor;
use solos_agent_core::{AllowlistPolicy, Engine, Error, MemoryRepositories, Repositories};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

// ---------- harness (same shape as tests/engine.rs) ----------

/// MemoryRepositories that can be told to fail its Nth commit from now.
#[derive(Default)]
struct Flaky {
    inner: MemoryRepositories,
    fail_in: Option<usize>,
}
impl Repositories for Flaky {
    fn trigger(&self, id: &TriggerId) -> Option<Trigger> {
        self.inner.trigger(id)
    }
    fn thread(&self, key: &ThreadKey) -> Option<Thread> {
        self.inner.thread(key)
    }
    fn ticket(&self, id: &TicketId) -> Option<Ticket> {
        self.inner.ticket(id)
    }
    fn decision(&self, id: &DecisionId) -> Option<Decision> {
        self.inner.decision(id)
    }
    fn trail(&self, assignment: &AssignmentId) -> Option<TrailCursor> {
        self.inner.trail(assignment)
    }
    fn memo(&self, id: &MemoId) -> Option<Memo> {
        self.inner.memo(id)
    }
    fn live_memos(&self) -> Vec<Memo> {
        self.inner.live_memos()
    }
    fn active_threads(&self) -> Vec<ThreadKey> {
        self.inner.active_threads()
    }
    fn thread_of_eval(&self, eval: &EvalId) -> Option<ThreadKey> {
        self.inner.thread_of_eval(eval)
    }
    fn commit(&mut self, changes: ChangeSet) -> Result<(), Error> {
        if let Some(n) = self.fail_in.as_mut() {
            if *n == 0 {
                self.fail_in = None;
                return Err(Error::Store("disk full".into()));
            }
            *n -= 1;
        }
        self.inner.commit(changes)
    }
}

type TestEngine = Engine<Flaky, AllowlistPolicy, Box<dyn Fn() -> u64>>;

struct World {
    engine: TestEngine,
}

impl World {
    fn new(policy: AllowlistPolicy) -> Self {
        let now = Rc::new(Cell::new(1_000u64));
        let clock = Box::new(move || now.get()) as Box<dyn Fn() -> u64>;
        Self {
            engine: Engine::new(Flaky::default(), policy, clock),
        }
    }
    fn mem(&self) -> &MemoryRepositories {
        &self.engine.repos().inner
    }
    fn effects(&mut self) -> Vec<Effect> {
        self.engine.repos_mut().inner.take_effects()
    }
    fn assess(&mut self, trigger: Trigger) -> EvalId {
        self.engine.receive(trigger).unwrap();
        let started = self.engine.evaluate_due().unwrap();
        assert_eq!(started.len(), 1, "one eval starts");
        started[0].clone()
    }
    /// alice asks on th1, the Router accepts, the Assignment starts.
    fn running_ticket(&mut self, budget: u64) -> (TicketId, AssignmentId) {
        let eval = self.assess(slack("t1", alice(), Addressed::Mention, "th1", "do x"));
        self.engine.decide(&eval, accept(budget, vec![])).unwrap();
        let effects = self.effects();
        let [(ticket, assignment)] = &launches(&effects)[..] else {
            panic!("one launch: {effects:?}")
        };
        self.engine.assignment_started(ticket, assignment).unwrap();
        self.effects();
        (ticket.clone(), assignment.clone())
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

fn alice() -> Actor {
    Actor::person("alice")
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
            address: BTreeMap::from([("thread_ts".to_owned(), thread.to_owned())]),
        }],
        meta: serde_json::Value::Null,
    }
}

fn github_bot(id: &str, bot: &str, threads: &[&str], text: &str) -> Trigger {
    let mut t = slack(
        id,
        Actor::bot(bot),
        Addressed::Unaddressed,
        threads[0],
        text,
    );
    t.source = "github".into();
    t.threads = threads.iter().map(|k| ThreadKey::new(*k)).collect();
    t
}

fn accept(budget: u64, rules: Vec<String>) -> Verdict {
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

fn report(status: ReportStatus) -> ReportIn {
    ReportIn {
        status,
        text: format!("{status:?}"),
        question: None,
        spent_microusd: 0,
        harvest: None,
    }
}

// ---------- findings ----------

/// Intent 5. The threshold drops below a finished Ticket's budget; a person's
/// message resumes it and it launches with no approval.
#[test]
fn f1a_resume_launches_over_threshold_without_approval() {
    let mut w = World::new(policy()); // auto_approve 2.00 USD
    let (ticket, a1) = w.running_ticket(1_500_000);
    w.engine
        .report(&ticket, &a1, report(ReportStatus::Ready))
        .unwrap();
    w.effects();

    w.engine.set_policy(AllowlistPolicy {
        auto_approve_microusd: 1_000_000,
        ..policy()
    });
    w.engine
        .receive(slack(
            "t2",
            alice(),
            Addressed::Unaddressed,
            "th1",
            "one more",
        ))
        .unwrap();
    let effects = w.effects();
    let launched: Vec<_> = effects
        .iter()
        .filter(|e| matches!(e, Effect::Launch { .. }))
        .collect();
    println!(
        "threshold=1000000 budget={:?} approval={:?} launches={launched:?}",
        w.mem().tickets[&ticket].mandate.budget_microusd,
        w.mem().tickets[&ticket].approval
    );
    assert!(
        launched.is_empty(),
        "over-threshold ticket launched without approval"
    );
}

/// Intent 5, the retry path.
#[test]
fn f1b_retry_launches_over_threshold_without_approval() {
    let mut w = World::new(policy());
    let (ticket, a1) = w.running_ticket(1_500_000);
    w.engine
        .report(&ticket, &a1, report(ReportStatus::Failed))
        .unwrap();
    w.effects();
    w.engine.set_policy(AllowlistPolicy {
        auto_approve_microusd: 1_000_000,
        ..policy()
    });
    let result = w.engine.retry(&ticket);
    let effects = w.effects();
    println!("retry -> {result:?}; launches={:?}", launches(&effects));
    assert!(
        launches(&effects).is_empty(),
        "over-threshold ticket launched without approval"
    );
}

/// Policy::paused: "no new Evals and no launches". decide launches while paused.
#[test]
fn f3_gate_rejected_triggers_reach_the_router_and_pick_threads() {
    let mut w = World::new(policy());
    w.engine
        .receive(slack(
            "m1",
            Actor::person("mallory"),
            Addressed::Mention,
            "th1",
            "@agent rm -rf /",
        ))
        .unwrap();
    w.engine
        .receive(github_bot(
            "b1",
            "evil-bot",
            &["th1", "evil-thread"],
            "also watch evil-thread",
        ))
        .unwrap();
    let eval = w.assess(slack("t1", alice(), Addressed::Mention, "th1", "@agent hi"));
    let effects = w.effects();
    let [Effect::Assess(candidate)] = &effects[..] else {
        panic!("{effects:?}")
    };
    let ids: Vec<_> = candidate.triggers.iter().map(|t| t.id.as_str()).collect();
    println!("candidate triggers sent to the Router: {ids:?}");

    w.engine.decide(&eval, accept(1_000_000, vec![])).unwrap();
    let ticket = w.mem().tickets.values().next().unwrap().clone();
    println!("ticket threads: {:?}", ticket.threads);
    let subs = &w.mem().threads[&ThreadKey::new("evil-thread")].subscriptions;
    println!("evil-thread subscriptions: {subs:?}");

    assert_eq!(ids, ["t1"], "gate-rejected triggers reached the Router");
    assert!(
        !ticket.threads.contains(&ThreadKey::new("evil-thread")),
        "an unlisted bot chose a Ticket Thread"
    );
}

/// Intent 4 / 3. In `observe` mode a listed bot's candidate alone is only
/// recorded; batched with any explicit Trigger, an accept that names
/// non-enabled proactive rules creates and launches a Ticket.
#[test]
fn f4_a_bot_candidate_rides_an_explicit_batch_in_observe_mode() {
    // Control: the bot alone.
    let mut w = World::new(policy()); // accept_mode: observe (default)
    let eval = w.assess(github_bot(
        "b1",
        "ci-bot",
        &["th1"],
        "deploy is broken, run the fixer",
    ));
    let rules = vec!["not-an-enabled-rule".to_owned()];
    let d = w
        .engine
        .decide(&eval, accept(1_000_000, rules.clone()))
        .unwrap();
    let rec = &w.mem().decisions[&d];
    println!(
        "bot alone: kind={:?} would_accept={} tickets={}",
        rec.kind,
        rec.would_accept,
        w.mem().tickets.len()
    );

    let mut w = World::new(policy());
    w.engine
        .receive(github_bot(
            "b1",
            "ci-bot",
            &["th1"],
            "deploy is broken, run the fixer",
        ))
        .unwrap();
    let eval = w.assess(slack(
        "t1",
        alice(),
        Addressed::Mention,
        "th1",
        "@agent thanks!",
    ));
    let effects = w.effects();
    let [Effect::Assess(candidate)] = &effects[..] else {
        panic!("{effects:?}")
    };
    let ids: Vec<_> = candidate.triggers.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["t1"], "the bot does not ride the explicit batch");
    w.engine
        .decide(
            &eval,
            Verdict::Ignore {
                reason: "thanks".into(),
            },
        )
        .unwrap();
    let eval = w
        .engine
        .evaluate_due()
        .unwrap()
        .pop()
        .expect("the bot is judged on its own");
    let d = w.engine.decide(&eval, accept(1_000_000, rules)).unwrap();
    let rec = &w.mem().decisions[&d];
    assert!(rec.would_accept, "inferred and recorded only");
    assert!(
        w.mem().tickets.is_empty(),
        "observe mode + non-enabled rule created a Ticket"
    );
}

#[test]
fn f5_evaluate_due_errors_after_committing() {
    let mut w = World::new(policy());
    w.engine
        .receive(slack("a", alice(), Addressed::Mention, "th1", "one"))
        .unwrap();
    w.engine
        .receive(slack("b", alice(), Addressed::Mention, "th2", "two"))
        .unwrap();
    w.engine.repos_mut().fail_in = Some(1);
    let result = w.engine.evaluate_due();
    let running: Vec<_> = w
        .mem()
        .threads
        .values()
        .filter(|t| t.eval.is_some())
        .map(|t| t.key.as_str().to_owned())
        .collect();
    let assess = w
        .mem()
        .effects
        .iter()
        .filter(|e| matches!(e, Effect::Assess(_)))
        .count();
    match result {
        Err(_) => assert!(
            running.is_empty() && assess == 0,
            "an erroring pass committed part of its work: {running:?}, {assess} Assess"
        ),
        Ok(started) => assert!(
            started.len() == 2 && running.len() == 2 && assess == 2,
            "a pass that succeeded committed only part of its work"
        ),
    }
}

/// Intent 3. A person's follow-up sent while the accepting Eval runs stays
/// queued for an Eval after the Ticket subscribes the Thread; it never
/// reaches the Ticket.
#[test]
fn f6_follow_up_during_the_accepting_eval_never_reaches_the_ticket() {
    let mut w = World::new(policy());
    let eval = w.assess(slack(
        "t1",
        alice(),
        Addressed::Mention,
        "th1",
        "@agent fix the flake",
    ));
    w.engine
        .receive(slack(
            "t2",
            alice(),
            Addressed::Unaddressed,
            "th1",
            "it is in broker_http.rs",
        ))
        .unwrap();
    w.engine.decide(&eval, accept(1_000_000, vec![])).unwrap();
    w.effects();
    let ticket = w.mem().tickets.values().next().unwrap().clone();
    let thread = &w.mem().threads[&ThreadKey::new("th1")];
    println!(
        "th1 subscribes {:?}; th1.queued={:?}; ticket inputs={:?}",
        thread.subscriptions,
        thread.queued,
        ticket.inputs.iter().map(|i| &i.body).collect::<Vec<_>>()
    );
    let next = w.engine.evaluate_due().unwrap();
    let effects = w.effects();
    let to_router: Vec<_> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::Assess(c) => Some(
                c.triggers
                    .iter()
                    .map(|t| t.id.to_string())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .collect();
    println!("next evals={next:?}; sent to the Router: {to_router:?}");
    assert!(
        to_router.is_empty(),
        "the person's follow-up went to an Eval"
    );
}

/// README stop flow: a person's stop reaches the worker, which hands it back
/// to the Router. When it reached the Ticket through a secondary Thread, the
/// explicit Eval on its primary Thread cannot stop that Ticket.
#[test]
fn f7_handed_back_stop_from_a_secondary_thread_cannot_stop() {
    let mut w = World::new(policy());
    let (ticket, _) = w.running_ticket(1_000_000);
    let mut stop = slack(
        "t9",
        alice(),
        Addressed::Mention,
        "th9",
        "@agent stop the flake work",
    );
    stop.threads = vec![ThreadKey::new("th9"), ThreadKey::new("th1")];
    let intake = w.engine.receive(stop).unwrap();
    println!("intake: {intake:?}");
    assert!(
        w.engine
            .handback(&ticket, &TriggerId::new("t9"), "a stop request")
            .unwrap()
    );
    let eval = w.engine.evaluate_due().unwrap().pop().unwrap();
    let effects = w.effects();
    let strength = effects.iter().find_map(|e| match e {
        Effect::Assess(c) => Some(c.strength),
        _ => None,
    });
    let result = w.engine.decide(
        &eval,
        Verdict::Stop {
            ticket: ticket.clone(),
            reason: "alice asked".into(),
        },
    );
    println!(
        "eval strength={strength:?}; decide(Stop) -> {result:?}; ticket stage={:?}",
        w.mem().tickets[&ticket].stage()
    );
    assert_eq!(strength, Some(Strength::Explicit));
    assert!(
        result.is_ok(),
        "an explicit person's stop could not stop the ticket"
    );
}

/// Intent 8. Failed, a CI Input is held, then a retry (no person wrote):
/// the held automatic Input is delivered.
#[test]
fn f9_open_ticket_with_ended_assignment_never_continues() {
    let mut w = World::new(policy());
    let (ticket, a1) = w.running_ticket(1_000_000);
    w.engine
        .report(&ticket, &a1, report(ReportStatus::Waiting))
        .unwrap();
    w.engine
        .assignment_ended(&ticket, &a1, "host restart")
        .unwrap();
    w.effects();
    println!(
        "stage after restart: {:?}",
        w.mem().tickets[&ticket].stage()
    );
    assert_eq!(w.mem().tickets[&ticket].stage(), Stage::Idle);
    let intake = w
        .engine
        .receive(slack(
            "t2",
            alice(),
            Addressed::Unaddressed,
            "th1",
            "please continue",
        ))
        .unwrap();
    let effects = w.effects();
    let t = &w.mem().tickets[&ticket];
    println!(
        "intake={intake:?}; effects={effects:?}; undelivered inputs={}",
        t.inputs.iter().filter(|i| i.sent_to.is_none()).count()
    );
    assert!(
        !launches(&effects).is_empty(),
        "a person's message on an Idle ticket started nothing"
    );
}
