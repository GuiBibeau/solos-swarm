//! The same seeded command walks drive an Engine over `MemoryRepositories`
//! and one over `SqliteStore`. After every step both stores hold the same
//! Triggers, Threads, Tickets, Decisions, Effects and Events, answer the
//! port's queries alike, and every command returned the same result.
//!
//! Every fourth seed uses a database file and reopens it at random steps. A
//! simulated effect runner works the SQLite queue in between (complete, fail,
//! and repeats of both), which must leave the domain state alone and never
//! let an Effect overtake an earlier one in its lane.
//!
//! `DIFF_SEEDS`, `DIFF_STEPS` and `DIFF_FIRST_SEED` override the defaults
//! (64 seeds of 200 steps, from seed 0). Generators follow agent-core's
//! `tests/fuzz_walk.rs`.

mod common;

use common::{
    MemEngine, SqlEngine, TempDb, clock, first_difference, memory_contents, port_view,
    sqlite_contents,
};
use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, Approval, AssignmentId, AssignmentState, ChannelRef,
    Effect, EvalId, InputBody, Plan, ReportIn, ReportStatus, ThreadKey, Ticket, TicketId, Trigger,
    TriggerId, Verdict,
};
use solos_agent_core::{
    AcceptMode, AllowlistPolicy, Engine, Error, MemoryRepositories, Repositories,
};
use solos_agent_store::{QueuedEffect, SqliteStore};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::Instant;

const THREADS: [&str; 3] = ["th1", "th2", "th3"];

/// Every public Engine command, by the name `Op::name` gives it.
const COMMANDS: [&str; 13] = [
    "receive",
    "evaluate_due",
    "decide(accept)",
    "decide(ignore)",
    "decide(stop)",
    "approve",
    "note",
    "stop_ticket",
    "retry",
    "handback",
    "assignment_started",
    "assignment_ended",
    "report",
];

struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        let mut rng = Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5DEE_CE66_D1CE_4E5B);
        for _ in 0..4 {
            rng.next();
        }
        rng
    }
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
    fn pick<T: Clone>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())].clone()
    }
}

#[derive(Clone, Debug)]
enum Op {
    Receive(Trigger),
    EvaluateDue,
    Decide(EvalId, Verdict),
    Approve(TicketId, String, Actor),
    Note(TicketId),
    StopTicket(TicketId),
    Retry(TicketId),
    Handback(TicketId, TriggerId),
    Started(TicketId, AssignmentId),
    Ended(TicketId, AssignmentId),
    Report(TicketId, AssignmentId, ReportIn),
    SetPolicy(AllowlistPolicy),
    Advance(u64),
    /// SQLite only: the effect runner takes up to this many due Effects.
    RunEffects(usize),
    /// SQLite only: close the file and open it again.
    Reopen,
}

impl Op {
    fn name(&self) -> &'static str {
        match self {
            Op::Receive(_) => "receive",
            Op::EvaluateDue => "evaluate_due",
            Op::Decide(_, Verdict::Accept(_)) => "decide(accept)",
            Op::Decide(_, Verdict::Ignore { .. }) => "decide(ignore)",
            Op::Decide(_, Verdict::Stop { .. }) => "decide(stop)",
            Op::Approve(..) => "approve",
            Op::Note(_) => "note",
            Op::StopTicket(_) => "stop_ticket",
            Op::Retry(_) => "retry",
            Op::Handback(..) => "handback",
            Op::Started(..) => "assignment_started",
            Op::Ended(..) => "assignment_ended",
            Op::Report(..) => "report",
            Op::SetPolicy(_) => "set_policy",
            Op::Advance(_) => "advance_clock",
            Op::RunEffects(_) => "run_effects",
            Op::Reopen => "reopen",
        }
    }
}

/// Runs an Engine command; `None` for the steps that are not one.
fn apply<R: Repositories>(
    e: &mut Engine<R, AllowlistPolicy, common::Clock>,
    op: &Op,
) -> Option<Result<String, Error>> {
    let unit = |()| String::new();
    Some(match op {
        Op::Receive(t) => e.receive(t.clone()).map(|i| format!("{i:?}")),
        Op::EvaluateDue => e.evaluate_due().map(|v| format!("{v:?}")),
        Op::Decide(eval, verdict) => e.decide(eval, verdict.clone()).map(|d| d.to_string()),
        Op::Approve(t, source, by) => e.approve(t, source, by.clone()).map(unit),
        Op::Note(t) => e.note(t, "a fact from other work".into()).map(unit),
        Op::StopTicket(t) => e.stop_ticket(t, "a person stopped it").map(unit),
        Op::Retry(t) => e.retry(t).map(|a| format!("{a:?}")),
        Op::Handback(t, trigger) => e.handback(t, trigger, "not mine").map(|b| b.to_string()),
        Op::Started(t, a) => e.assignment_started(t, a).map(unit),
        Op::Ended(t, a) => e.assignment_ended(t, a, "process exited").map(unit),
        Op::Report(t, a, r) => e.report(t, a, r.clone()).map(unit),
        Op::SetPolicy(p) => {
            e.set_policy(p.clone());
            Ok(String::new())
        }
        Op::Advance(_) | Op::RunEffects(_) | Op::Reopen => return None,
    })
}

// ---------- generation ----------

fn base_policy(rng: &mut Lcg) -> AllowlistPolicy {
    let mut people: BTreeSet<String> = ["slack:alice".to_owned()].into();
    if rng.chance(50) {
        people.insert("slack:bob".to_owned());
    }
    AllowlistPolicy {
        paused: false,
        people,
        bots: ["slack:ci-bot".to_owned()].into(),
        approvers: ["slack:alice".to_owned()].into(),
        accept_mode: rng.pick(&[AcceptMode::Off, AcceptMode::Observe, AcceptMode::Auto]),
        rules: ["r1".to_owned()].into(),
        auto_approve_microusd: rng.pick(&[1_000_000, 2_000_000, 5_000_000]),
        eval_timeout_ms: rng.pick(&[30_000, 120_000]),
    }
}

fn toggle(set: &mut BTreeSet<String>, item: &str) {
    if !set.remove(item) {
        set.insert(item.to_owned());
    }
}

fn mutate_policy(rng: &mut Lcg, current: &AllowlistPolicy) -> AllowlistPolicy {
    let mut p = current.clone();
    match rng.below(8) {
        0 | 1 => p.paused = !p.paused,
        2 | 3 => p.auto_approve_microusd = rng.pick(&[500_000, 2_000_000, 10_000_000]),
        4 => toggle(&mut p.people, "slack:bob"),
        5 => toggle(&mut p.approvers, "slack:bob"),
        6 => p.accept_mode = rng.pick(&[AcceptMode::Off, AcceptMode::Observe, AcceptMode::Auto]),
        _ => toggle(&mut p.rules, "r1"),
    }
    p
}

fn gen_actor(rng: &mut Lcg) -> (String, Actor) {
    let (source, actor) = match rng.below(14) {
        0..=5 => ("slack", Actor::person("alice")),
        6 => ("github", Actor::person("alice")),
        7 | 8 => ("slack", Actor::person("bob")),
        9 => ("slack", Actor::person("mallory")),
        10 | 11 => ("slack", Actor::bot("ci-bot")),
        _ => ("cron", Actor::system("schedule")),
    };
    (source.to_owned(), actor)
}

fn gen_trigger(rng: &mut Lcg, r: &MemoryRepositories, fresh: &mut u64, now: u64) -> Trigger {
    let existing: Vec<TriggerId> = r.triggers.keys().cloned().collect();
    let id = if !existing.is_empty() && rng.chance(4) {
        rng.pick(&existing)
    } else {
        *fresh += 1;
        TriggerId::new(format!("t{fresh}"))
    };
    let (source, actor) = gen_actor(rng);
    let n = match rng.below(100) {
        0..=1 => 0,
        2..=64 => 1,
        65..=92 => 2,
        _ => 3,
    };
    let threads = (0..n).map(|_| ThreadKey::new(rng.pick(&THREADS))).collect();
    let channels = match rng.below(100) {
        0..=14 => 0,
        15..=84 => 1,
        _ => 2,
    };
    let reply_to = (0..channels)
        .map(|c| ChannelRef {
            source: source.clone(),
            address: BTreeMap::from([("channel".to_owned(), format!("C{c}"))]),
        })
        .collect();
    let meta = if rng.chance(30) {
        serde_json::json!({"ts": format!("{now}.000100"), "score": 0.1, "big": u64::MAX})
    } else {
        serde_json::Value::Null
    };
    Trigger {
        id,
        source,
        at_ms: now,
        actor,
        addressed: rng.pick(&[
            Addressed::Mention,
            Addressed::Direct,
            Addressed::Unaddressed,
        ]),
        text: "please look at this, \"quoted\" and ünïcode".into(),
        threads,
        reply_to,
        meta,
    }
}

fn gen_accept(rng: &mut Lcg) -> Accept {
    let plan = if rng.chance(70) {
        Plan::Session {
            harness: rng.chance(20).then(|| "pi".to_owned()),
        }
    } else {
        Plan::Swarm {
            agents: rng.pick(&[0, 1, 4, 30, 31]),
            goal_template: None,
        }
    };
    Accept {
        title: if rng.chance(4) { "" } else { "task" }.into(),
        scope: "scope".into(),
        reason: "asked".into(),
        plan,
        budget_microusd: rng.pick(&[0, 400_000, 1_500_000, 3_000_000, 8_000_000]),
        acknowledge: rng.pick(&[
            Acknowledge::Standard,
            Acknowledge::Silent,
            Acknowledge::Text("ack".into()),
        ]),
        rules: rng.pick(&[vec![], vec!["r1".to_owned()]]),
    }
}

/// A Ticket id, preferring (75%) one `want` accepts; 5% a missing one.
fn pick_ticket(rng: &mut Lcg, r: &MemoryRepositories, want: impl Fn(&Ticket) -> bool) -> TicketId {
    let all: Vec<TicketId> = r.tickets.keys().cloned().collect();
    if all.is_empty() || rng.chance(5) {
        return TicketId::new("tk_missing");
    }
    let preferred: Vec<TicketId> = r
        .tickets
        .values()
        .filter(|t| want(t))
        .map(|t| t.id.clone())
        .collect();
    if !preferred.is_empty() && rng.chance(75) {
        rng.pick(&preferred)
    } else {
        rng.pick(&all)
    }
}

fn pick_assignment(rng: &mut Lcg, r: &MemoryRepositories, ticket: &TicketId) -> AssignmentId {
    let assignments = r.tickets.get(ticket).map(|t| &t.assignments[..]);
    let found = match rng.below(100) {
        0..=79 => assignments.and_then(<[_]>::last),
        80..=94 => assignments.and_then(<[_]>::first),
        _ => None,
    };
    found.map_or_else(|| AssignmentId::new("as_missing"), |a| a.id.clone())
}

fn latest_is(t: &Ticket, state: AssignmentState) -> bool {
    t.assignments.last().is_some_and(|a| a.state == state)
}

fn trigger_inputs(t: &Ticket, owned_only: bool) -> Vec<TriggerId> {
    t.inputs
        .iter()
        .filter(|i| !owned_only || i.from_allowed_person)
        .filter_map(|i| match &i.body {
            InputBody::Trigger { trigger } => Some(trigger.clone()),
            _ => None,
        })
        .collect()
}

fn gen_decide(rng: &mut Lcg, r: &MemoryRepositories) -> Op {
    let running: Vec<(ThreadKey, EvalId)> = r
        .threads
        .values()
        .filter_map(|t| t.eval.as_ref().map(|e| (t.key.clone(), e.id.clone())))
        .collect();
    let (thread, eval) = if running.is_empty() || rng.chance(6) {
        let stale: Vec<EvalId> = r
            .decisions
            .values()
            .filter_map(|d| d.eval.clone())
            .collect();
        let eval = if stale.is_empty() || rng.chance(30) {
            EvalId::new("ev_missing")
        } else {
            rng.pick(&stale)
        };
        (ThreadKey::new(rng.pick(&THREADS)), eval)
    } else {
        rng.pick(&running)
    };
    let verdict = match rng.below(10) {
        0..=5 => Verdict::Accept(gen_accept(rng)),
        6 | 7 => Verdict::Ignore {
            reason: "not for us".into(),
        },
        _ => {
            let on_thread: Vec<TicketId> = r
                .threads
                .get(&thread)
                .map(|t| t.subscriptions.iter().cloned().collect())
                .unwrap_or_default();
            let ticket = if !on_thread.is_empty() && rng.chance(80) {
                rng.pick(&on_thread)
            } else {
                pick_ticket(rng, r, |_| true)
            };
            Verdict::Stop {
                ticket,
                reason: "stop it".into(),
            }
        }
    };
    Op::Decide(eval, verdict)
}

fn gen_op(rng: &mut Lcg, w: &mut Walk) -> Op {
    let r = w.mem.repos();
    match rng.below(100) {
        0..=19 => Op::Receive(gen_trigger(rng, r, &mut w.fresh, w.now.get())),
        20..=29 => Op::EvaluateDue,
        30..=41 => gen_decide(rng, r),
        42..=48 => {
            let t = pick_ticket(rng, r, |t| latest_is(t, AssignmentState::Pending));
            let a = pick_assignment(rng, r, &t);
            Op::Started(t, a)
        }
        49..=57 => {
            let t = pick_ticket(rng, r, |t| latest_is(t, AssignmentState::Running));
            let a = pick_assignment(rng, r, &t);
            let status = rng.pick(&[
                ReportStatus::Progress,
                ReportStatus::Waiting,
                ReportStatus::Ready,
                ReportStatus::Blocked,
                ReportStatus::Failed,
                ReportStatus::Dropped,
            ]);
            let report = ReportIn {
                status,
                text: format!("{status:?}"),
                question: rng.chance(25).then(|| "which branch?".to_owned()),
                spent_microusd: rng.pick(&[0, 100_000, 900_000]),
            };
            Op::Report(t, a, report)
        }
        58..=61 => {
            let t = pick_ticket(rng, r, |t| t.approval == Approval::Waiting);
            let source = if rng.chance(85) { "slack" } else { "github" };
            let by = match rng.below(10) {
                0..=6 => Actor::person("alice"),
                7 => Actor::person("bob"),
                8 => Actor::bot("ci-bot"),
                _ => Actor::system("schedule"),
            };
            Op::Approve(t, source.into(), by)
        }
        62..=64 => Op::Note(pick_ticket(rng, r, |t| t.ended.is_none())),
        65..=67 => Op::StopTicket(pick_ticket(rng, r, |t| t.ended.is_none())),
        68..=70 => Op::Retry(pick_ticket(rng, r, |t| t.ended.is_none())),
        71..=75 => {
            let t = pick_ticket(rng, r, |t| !trigger_inputs(t, true).is_empty());
            let mut reached = r
                .tickets
                .get(&t)
                .map(|t| trigger_inputs(t, rng.chance(70)))
                .unwrap_or_default();
            if reached.is_empty() || rng.chance(10) {
                reached = r.triggers.keys().cloned().collect();
            }
            let trigger = if reached.is_empty() {
                TriggerId::new("t_missing")
            } else {
                rng.pick(&reached)
            };
            Op::Handback(t, trigger)
        }
        76..=78 => {
            let t = pick_ticket(rng, r, Ticket::has_live_assignment);
            let a = pick_assignment(rng, r, &t);
            Op::Ended(t, a)
        }
        79..=82 => Op::SetPolicy(mutate_policy(rng, w.mem.policy())),
        83..=89 => Op::Advance(rng.pick(&[1, 1_000, 40_000, 130_000])),
        90..=96 => Op::RunEffects(1 + rng.below(4)),
        _ if w.db.is_some() => Op::Reopen,
        _ => Op::EvaluateDue,
    }
}

// ---------- the walk ----------

#[derive(Default)]
struct Tally {
    /// (ok, err) per step name.
    outcomes: BTreeMap<&'static str, (u64, u64)>,
    completed: u64,
    failed: u64,
    repeats: u64,
    reopens: u64,
}

struct Walk {
    seed: u64,
    now: Rc<Cell<u64>>,
    mem: MemEngine,
    sql: SqlEngine,
    db: Option<TempDb>,
    fresh: u64,
    /// Separate from the command stream: which ids each step spot-checks.
    probe: Lcg,
}

fn lane(effect: &Effect) -> Option<String> {
    match effect {
        Effect::Send(m) => Some(format!("{:?}", m.channel)),
        _ => None,
    }
}

impl Walk {
    fn new(seed: u64, policy: AllowlistPolicy) -> Self {
        let now = Rc::new(Cell::new(1_000));
        let db = seed
            .is_multiple_of(4)
            .then(|| TempDb::new(&format!("diff-{seed}")));
        let store = match &db {
            Some(db) => SqliteStore::open(&db.path).unwrap(),
            None => SqliteStore::open_in_memory().unwrap(),
        };
        Self {
            seed,
            mem: Engine::new(MemoryRepositories::default(), policy.clone(), clock(&now)),
            sql: Engine::new(store, policy, clock(&now)),
            now,
            db,
            fresh: 0,
            probe: Lcg::new(seed ^ 0xD1FF),
        }
    }

    fn step(&mut self, rng: &mut Lcg, step: usize, tally: &mut Tally) {
        let op = gen_op(rng, self);
        let at = format!("seed {} step {step} {}: {op:?}", self.seed, op.name());
        match &op {
            Op::Advance(ms) => self.now.set(self.now.get() + ms),
            Op::RunEffects(n) => self.run_effects(rng, *n, tally, &at),
            Op::Reopen => {
                let db = self.db.as_ref().unwrap();
                let policy = self.sql.policy().clone();
                let store = SqliteStore::open(&db.path).unwrap();
                self.sql = Engine::new(store, policy, clock(&self.now));
                tally.reopens += 1;
            }
            _ => {
                let a = apply(&mut self.mem, &op).unwrap();
                let b = apply(&mut self.sql, &op).unwrap();
                assert_eq!(a, b, "{at}: the two stores returned differently");
                let slot = tally.outcomes.entry(op.name()).or_default();
                if a.is_ok() {
                    slot.0 += 1;
                } else {
                    slot.1 += 1;
                }
            }
        }
        self.compare(&at);
    }

    fn compare(&mut self, at: &str) {
        let (m, s) = (self.mem.repos(), self.sql.repos());
        let (a, b) = (memory_contents(m), sqlite_contents(s));
        if let Some(diff) = first_difference(&a, &b) {
            panic!("{at}: contents differ at {diff}");
        }
        assert_eq!(port_view(m, &a.threads), port_view(s, &a.threads), "{at}");
        let open: Vec<TicketId> = a
            .tickets
            .iter()
            .filter(|t| t.ended.is_none())
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(s.open_tickets().unwrap(), open, "{at}: open tickets");

        let p = &mut self.probe;
        let trigger = if !a.triggers.is_empty() && p.chance(80) {
            p.pick(&a.triggers).id
        } else {
            TriggerId::new("t_missing")
        };
        assert_eq!(m.trigger(&trigger), s.trigger(&trigger), "{at}: {trigger}");
        let key = ThreadKey::new(p.pick(&["th1", "th2", "th3", "th_missing"]));
        assert_eq!(m.thread(&key), s.thread(&key), "{at}: {key}");
        let ticket = if !a.tickets.is_empty() && p.chance(80) {
            p.pick(&a.tickets).id
        } else {
            TicketId::new("tk_missing")
        };
        assert_eq!(m.ticket(&ticket), s.ticket(&ticket), "{at}: {ticket}");
        if !a.decisions.is_empty() {
            let decision = p.pick(&a.decisions);
            assert_eq!(s.decision(&decision.id).as_ref(), Some(&decision), "{at}");
            if let Some(eval) = &decision.eval {
                assert_eq!(m.thread_of_eval(eval), s.thread_of_eval(eval), "{at}");
            }
        }
    }

    fn run_effects(&mut self, rng: &mut Lcg, n: usize, tally: &mut Tally, at: &str) {
        let s = self.sql.repos();
        let now = self.now.get();
        let all = s.effects().unwrap();
        let due = s.due_effects(now, n).unwrap();
        assert!(due.len() <= n, "{at}");
        assert!(
            due.windows(2).all(|w| w[0].id < w[1].id),
            "{at}: oldest first"
        );
        for q in &due {
            assert!(!q.done && q.due_ms <= now, "{at}: {q:?} is not due");
            let blocked = |p: &QueuedEffect| {
                p.id < q.id
                    && !p.done
                    && lane(&p.effect).is_some()
                    && lane(&p.effect) == lane(&q.effect)
            };
            assert!(
                !all.iter().any(blocked),
                "{at}: {:?} overtakes its lane",
                q.id
            );
        }
        for q in due {
            if rng.chance(70) {
                assert!(s.complete_effect(q.id).unwrap(), "{at}");
                tally.completed += 1;
                if rng.chance(20) {
                    assert!(!s.complete_effect(q.id).unwrap(), "{at}");
                    tally.repeats += 1;
                }
            } else {
                let retry = now + rng.pick(&[0, 1_000, 60_000]);
                assert!(
                    s.fail_effect(q.id, q.attempts, "boom", retry).unwrap(),
                    "{at}"
                );
                tally.failed += 1;
                if rng.chance(20) {
                    assert!(
                        !s.fail_effect(q.id, q.attempts, "boom", retry).unwrap(),
                        "{at}"
                    );
                    tally.repeats += 1;
                }
            }
        }
        // In each lane the done Effects are a prefix: none went before an earlier one.
        let mut pending_seen: BTreeSet<String> = BTreeSet::new();
        for q in s.effects().unwrap() {
            let Some(lane) = lane(&q.effect) else {
                continue;
            };
            if q.done {
                assert!(!pending_seen.contains(&lane), "{at}: {:?} overtook", q.id);
            } else {
                pending_seen.insert(lane);
            }
        }
    }
}

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn sqlite_store_matches_memory_store_on_seeded_walks() {
    let seeds = env_or("DIFF_SEEDS", 64);
    let steps = env_or("DIFF_STEPS", 200) as usize;
    let first = env_or("DIFF_FIRST_SEED", 0);
    let started = Instant::now();
    let mut tally = Tally::default();
    for seed in first..first + seeds {
        let mut rng = Lcg::new(seed);
        let policy = base_policy(&mut rng);
        let mut walk = Walk::new(seed, policy);
        for step in 0..steps {
            walk.step(&mut rng, step, &mut tally);
        }
        let chain = walk.sql.repos().verify_chain().unwrap();
        assert_eq!(
            chain.len,
            walk.mem.repos().events.len() as u64,
            "seed {seed}"
        );
    }

    let mut report = format!(
        "differential: seeds {first}..{} ({seeds} seeds), {steps} steps each, {:.1?}\n",
        first + seeds,
        started.elapsed()
    );
    for (name, (ok, err)) in &tally.outcomes {
        report.push_str(&format!("  {name:<20} ok={ok:<6} err={err}\n"));
    }
    report.push_str(&format!(
        "  effects completed={} failed={} repeated={}; reopens={}\n",
        tally.completed, tally.failed, tally.repeats, tally.reopens
    ));
    println!("{report}");
    if seeds * steps as u64 >= 2_000 {
        for name in COMMANDS {
            let ok = tally.outcomes.get(name).map_or(0, |o| o.0);
            assert!(ok > 0, "{name} never succeeded\n{report}");
        }
        assert!(
            tally.completed > 0 && tally.failed > 0 && tally.repeats > 0,
            "{report}"
        );
        assert!(tally.reopens > 0, "{report}");
    }
}
