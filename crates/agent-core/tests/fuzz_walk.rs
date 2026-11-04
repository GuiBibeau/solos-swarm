//! From the swarm review of PR #18. Seeded random walk over every public Engine command, checking atomicity and
//! Effect integrity after each step. No dependencies: a 64-bit LCG drives it.
//! `FUZZ_SEEDS`, `FUZZ_STEPS` and `FUZZ_FIRST_SEED` override the defaults
//! (300 seeds of 200 steps, from seed 0). The PR #18 review ran 2,000.
//!
//! Checked after every step:
//! 1. A command that returned `Err` changed nothing: triggers, threads,
//!    tickets, decisions, effects and events equal their values before it.
//! 2. Every new Effect names Tickets, Assignments and Inputs (and Triggers)
//!    that exist in the committed state; `Deliver` names Inputs of that
//!    Ticket whose `sent_to` is that Assignment; `Assess` names a running Eval.
//! 3. An Input id appears in at most one `Deliver` over the whole walk. Rule
//!    applied: strict, with no exception for a later Assignment, because
//!    `Input::sent_to` holds a single Assignment and the engine never clears
//!    it, so a second delivery is never an intended re-send.
//! 4. A Ticket has at most one Assignment that is not `Ended`, and only the
//!    latest one can be live.
//! 5. No Eval id runs on two Threads; no Trigger id is queued twice on one
//!    Thread, or both queued and in the running Eval of that Thread; no
//!    running Eval names a Trigger twice.
//! 6. Replaying a seed gives byte-identical state (`Debug` of
//!    `MemoryRepositories`) and identical command results.
//!
//! 7. The trail: a step asked gets the Assignment's next number and only
//!    when nothing waits and the Assignment runs (a Close needs neither the
//!    Assignment nor the Ticket to be live); a step answered is the one that
//!    waited, with the action asked; and only trail commands change a cursor.
//!
//! 8. Secret grants: a grant leaves Waiting at most once, to Granted or
//!    Denied, and never changes or disappears after; a new one is Waiting.
//! 9. Only `answer_secret` answers: every SecretAnswered follows a SecretAsked
//!    of the same Ticket and secret, comes from a person the item names (or
//!    an approver when it names none), and adds exactly one Secret Input.
//! 10. A repeated `ask_secret` commits no event and no Effect; a first one
//!     records one SecretAsked and one ask per Channel of the Ticket.
//! 11. A KeyUsed names the Ticket's running Assignment and a secret that is
//!     not Denied, within the bounds, and changes nothing else.
//!
//! Also checked: a new Decision never reuses a recorded Decision's id, and a
//! message that asks stands: its Ticket still awaits that ask.
//!
//! A state invariant (4, 5) counts once, at the step that first breaks it for
//! that Ticket or Thread. Each distinct (invariant, command) pair is shrunk to
//! a minimal command sequence and printed.

use solos_agent_core::ids::key;
use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, Approval, Ask, AssignmentId, AssignmentState,
    ChannelRef, DomainEvent, Effect, EvalId, InputBody, InputId, MessageKind, Plan, ReportIn,
    ReportStatus, SecretGrant, SecretId, ThreadKey, Ticket, TicketId, Trigger, TriggerId, Verdict,
};
use solos_agent_core::trail::{
    ActText, CloseReason, Digest, MAX_URL_CHARS, PageAction, PageSeen, STEP_VERSION, Signature,
    SignedStep, StepBody, StepOutcome, TrailAction, TrailKey, TrailOutcome,
};
use solos_agent_core::{AcceptMode, AllowlistPolicy, Engine, Error, MemoryRepositories, Policy};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

const THREADS: [&str; 3] = ["th1", "th2", "th3"];
const SECRETS: [&str; 3] = ["local/github-gui", "local/openai", "laptop/gh"];
const ROLLS: usize = 130;

// ---------- randomness ----------

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

// ---------- world and commands ----------

type FuzzEngine = Engine<MemoryRepositories, AllowlistPolicy, Box<dyn Fn() -> u64>>;

struct World {
    engine: FuzzEngine,
    now: Rc<Cell<u64>>,
    fresh: u64,
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
            fresh: 0,
        }
    }
    fn repos(&self) -> &MemoryRepositories {
        self.engine.repos()
    }
}

/// A command as the walk chose it. References are symbolic, so a shrunk
/// sequence keeps its meaning when earlier steps are removed: the running Eval
/// of a Thread, the kth Ticket whose origin is a Trigger, the latest or first
/// Assignment.
#[derive(Clone, Debug)]
enum Cmd {
    Receive(Trigger),
    EvaluateDue,
    Decide(EvalRef, VerdictSpec),
    Approve(TicketRef, String, Actor),
    Note(TicketRef),
    StopTicket(TicketRef, String, Actor),
    Retry(TicketRef),
    Handback(TicketRef, TriggerId),
    Started(TicketRef, AssignmentRef),
    Ended(TicketRef, AssignmentRef),
    Report(TicketRef, AssignmentRef, ReportIn),
    TrailAsk(TicketRef, AssignmentRef, TrailAction),
    TrailDone(TicketRef, AssignmentRef, Reply),
    AskSecret(TicketRef, SecretId, String, Vec<String>),
    AnswerSecret(TicketRef, SecretId, String, Actor, bool),
    UseKey(TicketRef, AssignmentRef, SecretId, String, String),
    SetPolicy(AllowlistPolicy),
    Advance(u64),
}

#[derive(Clone, Copy, Debug)]
enum Reply {
    Faithful,
    Lost,
    OtherAction,
    NumberInBody,
    NumberInCommand,
    Crashed,
    ClosedWithoutAsking,
    OtherTicket,
}

#[derive(Clone, Debug)]
enum EvalRef {
    Running(ThreadKey),
    Id(EvalId),
}

#[derive(Clone, Debug)]
enum TicketRef {
    /// The kth Ticket, in creation order, whose origin is this Trigger.
    Origin(TriggerId, usize),
    Id(TicketId),
}

#[derive(Clone, Debug)]
enum AssignmentRef {
    Latest,
    First,
    WithPendingStep,
    Id(AssignmentId),
}

#[derive(Clone, Debug)]
enum VerdictSpec {
    Accept(Accept),
    Ignore,
    Stop(TicketRef),
}

/// The same command with concrete ids, as the Engine receives it.
#[derive(Clone, Debug)]
enum Op {
    Receive(Trigger),
    EvaluateDue,
    Decide(EvalId, Verdict),
    Approve(TicketId, String, Actor),
    Note(TicketId),
    StopTicket(TicketId, String, Actor),
    Retry(TicketId),
    Handback(TicketId, TriggerId),
    Started(TicketId, AssignmentId),
    Ended(TicketId, AssignmentId),
    Report(TicketId, AssignmentId, ReportIn),
    TrailAsk(TicketId, AssignmentId, TrailAction),
    TrailDone(TicketId, AssignmentId, u32, TrailOutcome),
    AskSecret(TicketId, SecretId, String, Vec<String>),
    AnswerSecret(TicketId, SecretId, String, Actor, bool),
    UseKey(TicketId, AssignmentId, SecretId, String, String),
    SetPolicy(AllowlistPolicy),
    Advance(u64),
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
            Op::StopTicket(..) => "stop_ticket",
            Op::Retry(_) => "retry",
            Op::Handback(..) => "handback",
            Op::Started(..) => "assignment_started",
            Op::Ended(..) => "assignment_ended",
            Op::Report(..) => "report",
            Op::TrailAsk(..) => "trail_ask",
            Op::TrailDone(..) => "trail_done",
            Op::AskSecret(..) => "ask_secret",
            Op::AnswerSecret(..) => "answer_secret",
            Op::UseKey(..) => "use_key",
            Op::SetPolicy(_) => "set_policy",
            Op::Advance(_) => "advance_clock",
        }
    }
}

fn ticket_order(r: &MemoryRepositories) -> Vec<TicketId> {
    r.events
        .iter()
        .filter_map(|e| match &e.event {
            DomainEvent::TicketCreated { ticket, .. } => Some(ticket.clone()),
            _ => None,
        })
        .collect()
}

fn resolve(w: &World, cmd: &Cmd) -> Op {
    let r = w.repos();
    let order = ticket_order(r);
    let ticket = |t: &TicketRef| match t {
        TicketRef::Origin(origin, k) => order
            .iter()
            .filter(|id| r.tickets.get(*id).is_some_and(|t| t.origin == *origin))
            .nth(*k)
            .cloned()
            .unwrap_or_else(|| TicketId::new("tk_missing")),
        TicketRef::Id(id) => id.clone(),
    };
    let assignment = |t: &TicketId, a: &AssignmentRef| {
        let ticket = r.tickets.get(t);
        let found = match a {
            AssignmentRef::Latest => ticket.and_then(|t| t.assignments.last()),
            AssignmentRef::First => ticket.and_then(|t| t.assignments.first()),
            AssignmentRef::WithPendingStep => ticket.and_then(|t| {
                t.assignments
                    .iter()
                    .find(|a| r.trails.get(&a.id).is_some_and(|c| c.pending.is_some()))
            }),
            AssignmentRef::Id(id) => return id.clone(),
        };
        found
            .map(|a| a.id.clone())
            .unwrap_or_else(|| AssignmentId::new("as_missing"))
    };
    match cmd {
        Cmd::Receive(t) => Op::Receive(t.clone()),
        Cmd::EvaluateDue => Op::EvaluateDue,
        Cmd::Decide(eval, verdict) => {
            let eval = match eval {
                EvalRef::Running(key) => r
                    .threads
                    .get(key)
                    .and_then(|t| t.eval.as_ref())
                    .map(|e| e.id.clone())
                    .unwrap_or_else(|| EvalId::new("ev_missing")),
                EvalRef::Id(id) => id.clone(),
            };
            let verdict = match verdict {
                VerdictSpec::Accept(a) => Verdict::Accept(a.clone()),
                VerdictSpec::Ignore => Verdict::Ignore {
                    reason: "not for us".into(),
                },
                VerdictSpec::Stop(t) => Verdict::Stop {
                    ticket: ticket(t),
                    reason: "stop it".into(),
                },
            };
            Op::Decide(eval, verdict)
        }
        Cmd::Approve(t, source, by) => Op::Approve(ticket(t), source.clone(), by.clone()),
        Cmd::Note(t) => Op::Note(ticket(t)),
        Cmd::StopTicket(t, source, by) => Op::StopTicket(ticket(t), source.clone(), by.clone()),
        Cmd::Retry(t) => Op::Retry(ticket(t)),
        Cmd::Handback(t, trigger) => Op::Handback(ticket(t), trigger.clone()),
        Cmd::Started(t, a) => {
            let t = ticket(t);
            let a = assignment(&t, a);
            Op::Started(t, a)
        }
        Cmd::Ended(t, a) => {
            let t = ticket(t);
            let a = assignment(&t, a);
            Op::Ended(t, a)
        }
        Cmd::Report(t, a, report) => {
            let t = ticket(t);
            let a = assignment(&t, a);
            Op::Report(t, a, report.clone())
        }
        Cmd::TrailAsk(t, a, action) => {
            let t = ticket(t);
            let a = assignment(&t, a);
            Op::TrailAsk(t, a, action.clone())
        }
        Cmd::TrailDone(t, a, reply) => {
            let t = ticket(t);
            let a = assignment(&t, a);
            let waiting = r.trails.get(&a).and_then(|c| c.pending.clone());
            let (n, asked) =
                waiting.map_or((1, trail_open("https://example.com/")), |p| (p.n, p.action));
            let (n, outcome) = answer(*reply, &t, &a, n, &asked);
            Op::TrailDone(t, a, n, outcome)
        }
        Cmd::AskSecret(t, secret, label, answerable_by) => Op::AskSecret(
            ticket(t),
            secret.clone(),
            label.clone(),
            answerable_by.clone(),
        ),
        Cmd::AnswerSecret(t, secret, source, by, allow) => Op::AnswerSecret(
            ticket(t),
            secret.clone(),
            source.clone(),
            by.clone(),
            *allow,
        ),
        Cmd::UseKey(t, a, secret, method, url) => {
            let t = ticket(t);
            let a = assignment(&t, a);
            Op::UseKey(t, a, secret.clone(), method.clone(), url.clone())
        }
        Cmd::SetPolicy(p) => Op::SetPolicy(p.clone()),
        Cmd::Advance(ms) => Op::Advance(*ms),
    }
}

fn apply(w: &mut World, op: &Op) -> Result<String, Error> {
    let e = &mut w.engine;
    let unit = |()| String::new();
    match op {
        Op::Receive(t) => e.receive(t.clone()).map(|i| format!("{i:?}")),
        Op::EvaluateDue => e.evaluate_due().map(|v| format!("{v:?}")),
        Op::Decide(eval, verdict) => e.decide(eval, verdict.clone()).map(|d| d.to_string()),
        Op::Approve(t, source, by) => e.approve(t, source, by.clone()).map(unit),
        Op::Note(t) => e.note(t, "a fact from other work".into()).map(unit),
        Op::StopTicket(t, source, by) => e
            .stop_ticket(t, source, by.clone(), "a person stopped it")
            .map(unit),
        Op::Retry(t) => e.retry(t).map(|a| format!("{a:?}")),
        Op::Handback(t, trigger) => e.handback(t, trigger, "not mine").map(|b| b.to_string()),
        Op::Started(t, a) => e.assignment_started(t, a).map(unit),
        Op::Ended(t, a) => e.assignment_ended(t, a, "process exited").map(unit),
        Op::Report(t, a, r) => e.report(t, a, r.clone()).map(unit),
        Op::TrailAsk(t, a, action) => e.trail_ask(t, a, action.clone()).map(|n| n.to_string()),
        Op::TrailDone(t, a, n, outcome) => e.trail_done(t, a, *n, outcome.clone()).map(unit),
        Op::AskSecret(t, secret, label, by) => e
            .ask_secret(t, secret, label, by)
            .map(|grant| format!("{grant:?}")),
        Op::AnswerSecret(t, secret, source, by, allow) => e
            .answer_secret(t, secret, source, by.clone(), *allow)
            .map(unit),
        Op::UseKey(t, a, secret, method, url) => e.use_key(t, a, secret, method, url).map(unit),
        Op::SetPolicy(p) => {
            e.set_policy(p.clone());
            Ok(String::new())
        }
        Op::Advance(ms) => {
            w.now.set(w.now.get() + ms);
            Ok(String::new())
        }
    }
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
    match rng.below(10) {
        0..=2 => p.paused = !p.paused,
        3..=5 => {
            p.auto_approve_microusd = rng.pick(&[
                500_000, 1_000_000, 2_000_000, 5_000_000, 10_000_000, 50_000_000,
            ])
        }
        6 => toggle(&mut p.people, "slack:bob"),
        7 => {
            if rng.chance(50) {
                toggle(&mut p.people, "slack:alice")
            } else {
                toggle(&mut p.approvers, "slack:bob")
            }
        }
        8 => p.accept_mode = rng.pick(&[AcceptMode::Off, AcceptMode::Observe, AcceptMode::Auto]),
        _ => {
            if rng.chance(50) {
                toggle(&mut p.rules, "r1")
            } else {
                p.eval_timeout_ms = rng.pick(&[30_000, 120_000])
            }
        }
    }
    p
}

fn gen_actor(rng: &mut Lcg) -> (String, Actor) {
    let (source, actor) = match rng.below(16) {
        0..=5 => ("slack", Actor::person("alice")),
        // The same name on another Source is a different, unlisted identity.
        6 => ("github", Actor::person("alice")),
        7 | 8 => ("slack", Actor::person("bob")),
        9 | 10 => ("slack", Actor::person("mallory")),
        11 | 12 => ("slack", Actor::bot("ci-bot")),
        13 => ("slack", Actor::bot("spam-bot")),
        _ => ("cron", Actor::system("schedule")),
    };
    (source.to_owned(), actor)
}

fn gen_trigger(rng: &mut Lcg, w: &mut World) -> Trigger {
    let existing: Vec<TriggerId> = w.repos().triggers.keys().cloned().collect();
    let id = if !existing.is_empty() && rng.chance(4) {
        rng.pick(&existing)
    } else {
        w.fresh += 1;
        TriggerId::new(format!("t{}", w.fresh))
    };
    let (source, actor) = gen_actor(rng);
    let addressed = rng.pick(&[
        Addressed::Mention,
        Addressed::Direct,
        Addressed::Unaddressed,
    ]);
    let n = match rng.below(100) {
        0..=1 => 0,
        2..=61 => 1,
        62..=91 => 2,
        _ => 3,
    };
    let threads: Vec<ThreadKey> = (0..n).map(|_| ThreadKey::new(rng.pick(&THREADS))).collect();
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
    Trigger {
        id,
        source,
        at_ms: w.now.get(),
        actor,
        addressed,
        text: "please look at this".into(),
        threads,
        reply_to,
        meta: serde_json::Value::Null,
    }
}

fn gen_accept(rng: &mut Lcg) -> Accept {
    let plan = if rng.chance(70) {
        Plan::Session {
            harness: None,
            environment: None,
        }
    } else {
        let agents = if rng.chance(12) {
            rng.pick(&[0, 31])
        } else {
            rng.pick(&[1, 4, 30])
        };
        Plan::Swarm {
            agents,
            goal_template: None,
        }
    };
    let budget = if rng.chance(5) {
        0
    } else {
        rng.pick(&[400_000, 1_500_000, 3_000_000, 8_000_000, 40_000_000])
    };
    Accept {
        title: if rng.chance(4) { "" } else { "task" }.into(),
        scope: if rng.chance(4) { "  " } else { "scope" }.into(),
        reason: "asked".into(),
        plan,
        budget_microusd: budget,
        acknowledge: rng.pick(&[
            Acknowledge::Standard,
            Acknowledge::Silent,
            Acknowledge::Text("ack".into()),
        ]),
        rules: rng.pick(&[
            vec![],
            vec!["r1".to_owned()],
            vec!["r1".into(), "r2".into()],
        ]),
    }
}

/// The stable reference to an existing Ticket.
fn ticket_ref(r: &MemoryRepositories, id: &TicketId) -> TicketRef {
    let Some(origin) = r.tickets.get(id).map(|t| t.origin.clone()) else {
        return TicketRef::Id(id.clone());
    };
    let k = ticket_order(r)
        .iter()
        .take_while(|x| *x != id)
        .filter(|x| r.tickets.get(*x).is_some_and(|t| t.origin == origin))
        .count();
    TicketRef::Origin(origin, k)
}

/// Picks a Ticket, preferring (75%) the ones `want` accepts; 5% of the time
/// a Ticket that does not exist.
fn pick_ticket(rng: &mut Lcg, r: &MemoryRepositories, want: impl Fn(&Ticket) -> bool) -> TicketRef {
    let order = ticket_order(r);
    if order.is_empty() || rng.chance(5) {
        return TicketRef::Id(TicketId::new("tk_missing"));
    }
    let preferred: Vec<TicketId> = order
        .iter()
        .filter(|id| r.tickets.get(*id).is_some_and(&want))
        .cloned()
        .collect();
    let id = if !preferred.is_empty() && rng.chance(75) {
        rng.pick(&preferred)
    } else {
        rng.pick(&order)
    };
    ticket_ref(r, &id)
}

fn pick_assignment(rng: &mut Lcg) -> AssignmentRef {
    match rng.below(100) {
        0..=79 => AssignmentRef::Latest,
        80..=94 => AssignmentRef::First,
        _ => AssignmentRef::Id(AssignmentId::new("as_missing")),
    }
}

fn latest_is(t: &Ticket, state: AssignmentState) -> bool {
    t.assignments.last().is_some_and(|a| a.state == state)
}

/// Triggers that reached `t` from an allowed person and were not handed back.
fn owned_triggers(t: &Ticket) -> Vec<TriggerId> {
    t.inputs
        .iter()
        .filter(|i| i.from_allowed_person)
        .filter_map(|i| match &i.body {
            InputBody::Trigger { trigger } if !t.handed_back.contains(trigger) => {
                Some(trigger.clone())
            }
            _ => None,
        })
        .collect()
}

/// Whether the command a roll picks has a target that can succeed.
fn has_target(roll: usize, r: &MemoryRepositories) -> bool {
    let any = |f: &dyn Fn(&Ticket) -> bool| r.tickets.values().any(f);
    match roll {
        34..=45 => r.threads.values().any(|t| t.eval.is_some()),
        46..=53 => any(&|t| latest_is(t, AssignmentState::Pending)),
        54..=63 => any(&|t| latest_is(t, AssignmentState::Running)),
        64..=67 => any(&|t| t.approval == Approval::Waiting),
        68..=77 => any(&|t| t.ended.is_none()),
        78..=83 => any(&|t| !owned_triggers(t).is_empty()),
        84..=86 => any(&Ticket::has_live_assignment),
        92..=97 => any(&|t| latest_is(t, AssignmentState::Running)),
        98..=105 => r.trails.values().any(|c| c.pending.is_some()),
        112..=117 => any(&|t| t.ended.is_none()),
        118..=125 => any(&|t| t.ended.is_none() && !waiting_secrets(t).is_empty()),
        126..=129 => any(&|t| latest_is(t, AssignmentState::Running)),
        _ => true,
    }
}

fn waiting_secrets(t: &Ticket) -> Vec<SecretId> {
    t.secrets
        .iter()
        .filter(|(_, grant)| matches!(grant, SecretGrant::Waiting { .. }))
        .map(|(secret, _)| secret.clone())
        .collect()
}

fn gen_secret(rng: &mut Lcg) -> SecretId {
    SecretId::try_from(rng.pick(&SECRETS).to_owned()).unwrap()
}

fn gen_answerable_by(rng: &mut Lcg) -> Vec<String> {
    let names = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect();
    match rng.below(8) {
        0 | 1 => Vec::new(),
        2 | 3 => names(&["slack:alice"]),
        4 => names(&["slack:bob"]),
        5 => names(&["slack:alice", "slack:bob"]),
        6 => names(&["slack:ci-bot", "slack:mallory"]),
        _ => names(&["github:alice"]),
    }
}

fn gen_clicker(rng: &mut Lcg) -> (String, Actor) {
    let source = if rng.chance(85) { "slack" } else { "github" };
    let by = match rng.below(20) {
        0..=9 => Actor::person("alice"),
        10..=13 => Actor::person("bob"),
        14 | 15 => Actor::person("mallory"),
        16 | 17 => Actor::bot("ci-bot"),
        _ => Actor::system("schedule"),
    };
    (source.to_owned(), by)
}

fn trail_open(url: &str) -> TrailAction {
    TrailAction::Open {
        url: url.into(),
        key: None,
    }
}

fn trail_page() -> StepOutcome {
    StepOutcome::Page(PageSeen {
        url: "https://example.com/".into(),
        status: Some(200),
        title: "Example".into(),
        snapshot: Digest::of(b"snapshot"),
        view: Digest::of(b"view"),
        target: None,
        filled: Vec::new(),
        keyed: Vec::new(),
    })
}

fn gen_action(rng: &mut Lcg) -> TrailAction {
    match rng.below(20) {
        0..=5 => trail_open(&format!("https://example.com/{}", rng.below(4))),
        6..=11 => TrailAction::Act {
            action: rng.pick(&[
                PageAction::Click,
                PageAction::Type,
                PageAction::Select,
                PageAction::Press,
                PageAction::Scroll,
                PageAction::Back,
            ]),
            element: rng.chance(80).then(|| rng.below(30) as u32),
            text: match rng.below(3) {
                0 => None,
                1 => Some(ActText::Named {
                    text: "Next".into(),
                }),
                _ => Some(ActText::Typed {
                    digest: Digest::of(b"typed"),
                    chars: 5,
                }),
            },
        },
        12..=13 => TrailAction::Login {
            login: "github-gui".into(),
            username_element: None,
            password_element: None,
            submit: rng.chance(70),
        },
        14..=15 => TrailAction::Read {
            part: rng.chance(50).then_some(2),
            element: None,
        },
        _ => TrailAction::Close {
            reason: rng.pick(&[
                CloseReason::AssignmentEnded,
                CloseReason::Idle,
                CloseReason::Shutdown,
            ]),
        },
    }
}

fn gen_reply(rng: &mut Lcg) -> Reply {
    match rng.below(100) {
        0..=54 => Reply::Faithful,
        55..=64 => Reply::Lost,
        65..=71 => Reply::OtherAction,
        72..=78 => Reply::NumberInBody,
        79..=85 => Reply::NumberInCommand,
        86..=91 => Reply::Crashed,
        92..=95 => Reply::ClosedWithoutAsking,
        _ => Reply::OtherTicket,
    }
}

fn answer(
    reply: Reply,
    ticket: &TicketId,
    assignment: &AssignmentId,
    n: u32,
    asked: &TrailAction,
) -> (u32, TrailOutcome) {
    let honest = match asked {
        TrailAction::Close { reason } => StepOutcome::Closed {
            steps: 3,
            reason: *reason,
        },
        _ => trail_page(),
    };
    let signed = |ticket: &TicketId, n: u32, action: &TrailAction, outcome: StepOutcome| {
        TrailOutcome::signed(SignedStep {
            body: StepBody {
                v: STEP_VERSION,
                ticket: ticket.clone(),
                assignment: assignment.clone(),
                n,
                environment: "local".into(),
                session: "7a3c0f9e12d45b60".into(),
                session_step: n.saturating_sub(1),
                at_ms: 1_000,
                provenance: None,
                action: action.clone(),
                outcome,
            },
            signer: TrailKey::try_from(format!("ed25519:{}", "9c".repeat(32))).unwrap(),
            signature: Signature::try_from("ab".repeat(64)).unwrap(),
        })
    };
    match reply {
        Reply::Faithful => (n, signed(ticket, n, asked, honest)),
        Reply::Lost => (
            n,
            TrailOutcome::Lost {
                why: "the node went away".into(),
            },
        ),
        Reply::OtherAction => {
            let other = trail_open("https://evil.example/");
            (n, signed(ticket, n, &other, honest))
        }
        Reply::NumberInBody => (n, signed(ticket, n + 1, asked, honest)),
        Reply::NumberInCommand => (n + 1, signed(ticket, n, asked, honest)),
        Reply::Crashed => {
            let crashed = StepOutcome::Closed {
                steps: 1,
                reason: CloseReason::Crashed,
            };
            (n, signed(ticket, n, asked, crashed))
        }
        Reply::ClosedWithoutAsking => {
            let idle = StepOutcome::Closed {
                steps: 1,
                reason: CloseReason::Idle,
            };
            (n, signed(ticket, n, asked, idle))
        }
        Reply::OtherTicket => {
            let other = TicketId::new("tk_other");
            (n, signed(&other, n, asked, honest))
        }
    }
}

fn has_waiting_step(r: &MemoryRepositories, t: &Ticket) -> bool {
    t.assignments
        .iter()
        .any(|a| r.trails.get(&a.id).is_some_and(|c| c.pending.is_some()))
}

fn gen_cmd(rng: &mut Lcg, w: &mut World) -> Cmd {
    let mut roll = rng.below(ROLLS);
    // Mostly re-roll a command with nothing to act on; keep some for the error paths.
    for _ in 0..3 {
        if has_target(roll, w.repos()) || rng.chance(30) {
            break;
        }
        roll = rng.below(ROLLS);
    }
    if roll < 22 {
        return Cmd::Receive(gen_trigger(rng, w));
    }
    let r = w.repos();
    match roll {
        22..=33 => Cmd::EvaluateDue,
        34..=45 => {
            let running: Vec<ThreadKey> = r
                .threads
                .values()
                .filter(|t| t.eval.is_some())
                .map(|t| t.key.clone())
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
                (ThreadKey::new(rng.pick(&THREADS)), EvalRef::Id(eval))
            } else {
                let key = rng.pick(&running);
                (key.clone(), EvalRef::Running(key))
            };
            let verdict = match rng.below(10) {
                0..=5 => VerdictSpec::Accept(gen_accept(rng)),
                6 | 7 => VerdictSpec::Ignore,
                _ => {
                    let subs = r
                        .threads
                        .get(&thread)
                        .map(|t| t.subscriptions.clone())
                        .unwrap_or_default();
                    let on_thread: Vec<TicketId> = subs.into_iter().collect();
                    if !on_thread.is_empty() && rng.chance(80) {
                        VerdictSpec::Stop(ticket_ref(r, &rng.pick(&on_thread)))
                    } else {
                        VerdictSpec::Stop(pick_ticket(rng, r, |_| true))
                    }
                }
            };
            Cmd::Decide(eval, verdict)
        }
        46..=53 => Cmd::Started(
            pick_ticket(rng, r, |t| latest_is(t, AssignmentState::Pending)),
            pick_assignment(rng),
        ),
        54..=63 => {
            let t = pick_ticket(rng, r, |t| latest_is(t, AssignmentState::Running));
            let a = pick_assignment(rng);
            let status = rng.pick(&[
                ReportStatus::Progress,
                ReportStatus::Waiting,
                ReportStatus::Ready,
                ReportStatus::Blocked,
                ReportStatus::Failed,
                ReportStatus::Dropped,
            ]);
            let question = rng.chance(25).then(|| "which branch?".to_owned());
            let spent_microusd = rng.pick(&[0, 100_000, 900_000]);
            Cmd::Report(
                t,
                a,
                ReportIn {
                    status,
                    text: format!("{status:?}"),
                    question,
                    spent_microusd,
                },
            )
        }
        64..=67 => {
            let t = pick_ticket(rng, r, |t| t.approval == Approval::Waiting);
            let source = if rng.chance(85) { "slack" } else { "github" };
            let by = match rng.below(20) {
                0..=11 => Actor::person("alice"),
                12..=14 => Actor::person("bob"),
                15 | 16 => Actor::person("mallory"),
                17 | 18 => Actor::bot("ci-bot"),
                _ => Actor::system("schedule"),
            };
            Cmd::Approve(t, source.into(), by)
        }
        68..=70 => Cmd::Note(pick_ticket(rng, r, |t| t.ended.is_none())),
        71..=73 => {
            let t = pick_ticket(rng, r, |t| t.ended.is_none());
            let (source, by) = gen_clicker(rng);
            Cmd::StopTicket(t, source, by)
        }
        74..=77 => Cmd::Retry(pick_ticket(rng, r, |t| t.ended.is_none())),
        78..=83 => {
            let t = pick_ticket(rng, r, |t| !owned_triggers(t).is_empty());
            let Op::Note(ticket) = resolve(w, &Cmd::Note(t.clone())) else {
                unreachable!()
            };
            let reached: Vec<TriggerId> = r
                .tickets
                .get(&ticket)
                .map(|ticket| {
                    let mine = owned_triggers(ticket);
                    if !mine.is_empty() && rng.chance(70) {
                        return mine;
                    }
                    ticket
                        .inputs
                        .iter()
                        .filter_map(|i| match &i.body {
                            InputBody::Trigger { trigger } => Some(trigger.clone()),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let trigger = if !reached.is_empty() && rng.chance(90) {
                rng.pick(&reached)
            } else {
                let all: Vec<TriggerId> = r.triggers.keys().cloned().collect();
                if all.is_empty() {
                    TriggerId::new("t_missing")
                } else {
                    rng.pick(&all)
                }
            };
            Cmd::Handback(t, trigger)
        }
        84..=86 => Cmd::Ended(
            pick_ticket(rng, r, Ticket::has_live_assignment),
            pick_assignment(rng),
        ),
        87..=91 => Cmd::SetPolicy(mutate_policy(rng, w.engine.policy())),
        92..=97 => Cmd::TrailAsk(
            pick_ticket(rng, r, |t| latest_is(t, AssignmentState::Running)),
            pick_assignment(rng),
            gen_action(rng),
        ),
        98..=105 => {
            let t = pick_ticket(rng, r, |t| has_waiting_step(r, t));
            let a = match rng.below(100) {
                0..=84 => AssignmentRef::WithPendingStep,
                85..=94 => AssignmentRef::Latest,
                _ => AssignmentRef::First,
            };
            Cmd::TrailDone(t, a, gen_reply(rng))
        }
        106..=111 => Cmd::Advance(rng.pick(&[1, 1_000, 40_000, 130_000])),
        112..=117 => {
            let asked: Vec<(TicketId, SecretId)> = ticket_order(r)
                .into_iter()
                .filter_map(|id| r.tickets.get(&id).map(|t| (id, t)))
                .filter(|(_, t)| t.ended.is_none())
                .flat_map(|(id, t)| t.secrets.keys().map(move |s| (id.clone(), s.clone())))
                .collect();
            let (t, secret) = if !asked.is_empty() && rng.chance(35) {
                let (id, secret) = rng.pick(&asked);
                (ticket_ref(r, &id), secret)
            } else {
                (pick_ticket(rng, r, |t| t.ended.is_none()), gen_secret(rng))
            };
            let label = match rng.below(20) {
                0 => String::new(),
                1 => "  ".to_owned(),
                _ => "sign in to https://github.com as gui".to_owned(),
            };
            Cmd::AskSecret(t, secret, label, gen_answerable_by(rng))
        }
        118..=125 => {
            let t = pick_ticket(rng, r, |t| {
                t.ended.is_none() && !waiting_secrets(t).is_empty()
            });
            let Op::Note(ticket) = resolve(w, &Cmd::Note(t.clone())) else {
                unreachable!()
            };
            let waiting = r
                .tickets
                .get(&ticket)
                .map(waiting_secrets)
                .unwrap_or_default();
            let secret = if !waiting.is_empty() && rng.chance(85) {
                rng.pick(&waiting)
            } else {
                gen_secret(rng)
            };
            let (source, by) = gen_clicker(rng);
            Cmd::AnswerSecret(t, secret, source, by, rng.chance(50))
        }
        _ => {
            let t = pick_ticket(rng, r, |t| latest_is(t, AssignmentState::Running));
            let Op::Note(ticket) = resolve(w, &Cmd::Note(t.clone())) else {
                unreachable!()
            };
            let granted: Vec<SecretId> = r
                .tickets
                .get(&ticket)
                .map(|t| t.secrets.keys().cloned().collect())
                .unwrap_or_default();
            let secret = if !granted.is_empty() && rng.chance(60) {
                rng.pick(&granted)
            } else {
                gen_secret(rng)
            };
            let method = match rng.below(20) {
                0..=14 => "GET",
                15 | 16 => "POST",
                17 => "get",
                18 => "",
                _ => "ABCDEFGHIJKLMNOPQ",
            };
            let url = match rng.below(20) {
                0..=16 => "https://api.openai.com/v1/models".to_owned(),
                17 | 18 => format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS)),
                _ => "é".repeat(MAX_URL_CHARS),
            };
            Cmd::UseKey(t, pick_assignment(rng), secret, method.to_owned(), url)
        }
    }
}

// ---------- invariants ----------

#[derive(Clone, Debug)]
struct Finding {
    /// Invariant kind plus the command that broke it.
    category: String,
    kind: &'static str,
    detail: String,
}

fn describe(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "1-err-changed-state" => (
            "A command that returned Err changed committed state",
            "Err leaves triggers, threads, tickets, decisions, effects and events identical",
        ),
        "2-assess-missing-trigger" => (
            "Assess names a Trigger that does not exist",
            "every Candidate trigger exists",
        ),
        "2-assess-missing-ticket" => (
            "Assess names a Ticket that does not exist",
            "every TicketSummary names an existing Ticket",
        ),
        "2-assess-eval-not-running" => (
            "Assess names an Eval that is not running on its Thread",
            "the Candidate's eval is the running Eval of its thread after the commit",
        ),
        "2-launch-missing-ticket" => ("Launch names a missing Ticket", "the Ticket exists"),
        "2-launch-missing-assignment" => (
            "Launch names an Assignment its Ticket does not have",
            "the Assignment is one of the Ticket's",
        ),
        "2-deliver-missing-ticket" => ("Deliver names a missing Ticket", "the Ticket exists"),
        "2-deliver-missing-assignment" => (
            "Deliver names an Assignment its Ticket does not have",
            "the Assignment is one of the Ticket's",
        ),
        "2-deliver-foreign-input" => (
            "Deliver names an Input that is not its Ticket's",
            "every delivered Input is one of that Ticket's Inputs",
        ),
        "2-deliver-sent-to-mismatch" => (
            "Deliver names an Input whose sent_to is a different Assignment",
            "a delivered Input has sent_to = the Deliver's Assignment",
        ),
        "2-deliver-empty" => (
            "Deliver names no Inputs",
            "a Deliver names at least one Input",
        ),
        "2-stop-missing-ticket" => ("Stop names a missing Ticket", "the Ticket exists"),
        "2-stop-missing-assignment" => (
            "Stop names an Assignment its Ticket does not have",
            "the Assignment is one of the Ticket's",
        ),
        "2-send-missing-ticket" => ("Send names a missing Ticket", "the Ticket exists"),
        "3-input-delivered-twice" => (
            "An Input id appears in two Deliver effects",
            "each Input id appears in at most one Deliver",
        ),
        "4-two-live-assignments" => (
            "A Ticket has two Assignments that are not Ended",
            "at most one Assignment per Ticket is Pending or Running",
        ),
        "4-live-assignment-not-latest" => (
            "A live Assignment is not the Ticket's latest",
            "only the latest Assignment can be Pending or Running",
        ),
        "5-eval-on-two-threads" => (
            "One Eval id runs on two Threads",
            "each running Eval id belongs to one Thread",
        ),
        "5-trigger-queued-twice" => (
            "A Trigger id is queued twice on one Thread",
            "Thread.queued holds each Trigger id at most once",
        ),
        "5-trigger-queued-and-in-eval" => (
            "A Trigger id is both queued and in the running Eval of the same Thread",
            "a Trigger in the running Eval is not also in Thread.queued",
        ),
        "5-eval-batch-repeats-trigger" => (
            "A running Eval names the same Trigger twice",
            "Eval.triggers holds each Trigger id at most once",
        ),
        "7-trail-numbering" => (
            "A trail step was numbered out of order, or asked while another waited",
            "a step gets the Assignment's next number, and only when none waits",
        ),
        "7-trail-not-running" => (
            "A trail step was asked by an Assignment that was not running",
            "only the Ticket's running Assignment asks a step, except a Close",
        ),
        "7-trail-cursor" => (
            "A cursor does not hold the step just asked",
            "the cursor's next is n + 1 and its pending step is n with the asked action",
        ),
        "7-trail-done-unpaired" => (
            "A trail answer names a step that did not wait",
            "an answer pairs with the step that waited",
        ),
        "7-trail-done-mismatch" => (
            "A signed step answered an action other than the one asked",
            "a signed step's action equals the action asked",
        ),
        "7-trail-done-left-pending" => (
            "A step still waits after its answer",
            "an answer clears the pending step",
        ),
        "7-trail-silent-change" => (
            "A cursor changed with no trail event",
            "only trail commands change a cursor",
        ),
        "8-grant-removed" => (
            "A secret grant disappeared",
            "a grant is kept for the Ticket's life",
        ),
        "8-grant-changed" => (
            "A secret grant changed other than from Waiting to Granted or Denied",
            "a grant leaves Waiting at most once and never changes after",
        ),
        "8-grant-new-not-waiting" => (
            "A new secret grant did not start Waiting",
            "a grant starts as Waiting, with the label and list it was asked with",
        ),
        "8-grant-without-event" => (
            "A grant appeared or moved with no matching event",
            "every new grant has a SecretAsked and every move a SecretAnswered",
        ),
        "9-answer-without-ask" => (
            "A SecretAnswered has no Waiting ask before it",
            "every SecretAnswered follows a SecretAsked of the same Ticket and secret, once",
        ),
        "9-answer-not-allowed" => (
            "A secret was answered by someone the item does not name",
            "only a person on the item's list, or an approver when it has none, answers",
        ),
        "9-answer-by-other-command" => (
            "A SecretAnswered came from a command other than answer_secret",
            "only answer_secret answers a secret",
        ),
        "9-answer-input" => (
            "An answer did not add exactly one matching Secret Input",
            "an answer adds one Input::Secret from a person, with its grant and verdict",
        ),
        "10-ask-repeated-committed" => (
            "A repeated ask_secret committed an event or an Effect",
            "an ask for an existing grant returns it and commits nothing",
        ),
        "10-ask-posts" => (
            "A first ask_secret did not post one ask per Channel",
            "one SecretAsked and one Attention ask per Channel of the Ticket",
        ),
        "11-key-used" => (
            "A KeyUsed does not name the running Assignment and an allowed secret",
            "a use is for the Ticket's running Assignment and a secret that is not Denied",
        ),
        "11-key-bounds" => (
            "A KeyUsed holds a method or a url beyond its bounds",
            "a method is 1 to 16 uppercase letters and a url at most MAX_URL_CHARS",
        ),
        "12-stop-not-allowed" => (
            "A stop by someone the Policy does not allow succeeded",
            "only an allowed person's stop ends a Ticket",
        ),
        "11-key-changed-state" => (
            "A keyed use changed state or sent something",
            "use_key records one event and changes nothing else",
        ),
        "2-send-ask-not-standing" => (
            "A message asks something its Ticket no longer awaits",
            "a message with an ask is committed with its Ticket awaiting that ask",
        ),
        "2-send-ask-not-attention" => (
            "A message with an ask is not an Attention message",
            "an ask is always an Attention message",
        ),
        "6-replay-differs" => (
            "Replaying a seed gives different state",
            "two runs of one seed give byte-identical Debug output and results",
        ),
        "x-decision-id-reused" => (
            "A new Decision reuses the id of a recorded Decision (it overwrites it)",
            "every DecisionRecorded names a fresh Decision id",
        ),
        _ => ("unknown", "unknown"),
    }
}

/// Violations of the state invariants (4, 5), keyed by the entity that breaks them.
fn state_violations(r: &MemoryRepositories) -> BTreeMap<(&'static str, String), String> {
    let mut out = BTreeMap::new();
    for t in r.tickets.values() {
        let live: Vec<usize> = t
            .assignments
            .iter()
            .enumerate()
            .filter(|(_, a)| a.state != AssignmentState::Ended)
            .map(|(i, _)| i)
            .collect();
        let states: Vec<String> = t
            .assignments
            .iter()
            .map(|a| format!("{}={:?}", a.id, a.state))
            .collect();
        if live.len() > 1 {
            out.insert(
                ("4-two-live-assignments", t.id.to_string()),
                format!(
                    "ticket {} approval={:?} assignments [{}]",
                    t.id,
                    t.approval,
                    states.join(", ")
                ),
            );
        }
        if live.iter().any(|&i| i + 1 != t.assignments.len()) {
            out.insert(
                ("4-live-assignment-not-latest", t.id.to_string()),
                format!("ticket {} assignments [{}]", t.id, states.join(", ")),
            );
        }
    }
    let mut eval_threads: BTreeMap<EvalId, Vec<String>> = BTreeMap::new();
    for th in r.threads.values() {
        let mut seen = BTreeSet::new();
        for id in &th.queued {
            if !seen.insert(id) {
                out.insert(
                    ("5-trigger-queued-twice", format!("{} {id}", th.key)),
                    format!("thread {} queued {:?}", th.key, ids(&th.queued)),
                );
            }
        }
        if let Some(e) = &th.eval {
            eval_threads
                .entry(e.id.clone())
                .or_default()
                .push(th.key.to_string());
            for id in &th.queued {
                if e.triggers.contains(id) {
                    out.insert(
                        (
                            "5-trigger-queued-and-in-eval",
                            format!("{} {} {id}", th.key, e.id),
                        ),
                        format!(
                            "thread {} running eval {} triggers {:?}, queued {:?}",
                            th.key,
                            e.id,
                            ids(&e.triggers),
                            ids(&th.queued)
                        ),
                    );
                }
            }
            let mut batch = BTreeSet::new();
            for id in &e.triggers {
                if !batch.insert(id) {
                    out.insert(
                        ("5-eval-batch-repeats-trigger", format!("{} {id}", e.id)),
                        format!(
                            "thread {} running eval {} triggers {:?}",
                            th.key,
                            e.id,
                            ids(&e.triggers)
                        ),
                    );
                }
            }
        }
    }
    for (eval, threads) in eval_threads {
        if threads.len() > 1 {
            out.insert(
                ("5-eval-on-two-threads", eval.to_string()),
                format!("eval {eval} runs on {threads:?}"),
            );
        }
    }
    out
}

fn ids<T: std::fmt::Display>(items: &[T]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

#[derive(Default)]
struct Checker {
    /// Every Input id a Deliver named so far, with the Assignment it went to.
    delivered: BTreeMap<InputId, AssignmentId>,
    state: BTreeMap<(&'static str, String), String>,
    answered: BTreeSet<(TicketId, SecretId)>,
}

impl Checker {
    fn step(&mut self, w: &mut World, cmd: &Cmd) -> (Op, Result<String, Error>, Vec<Finding>) {
        let op = resolve(w, cmd);
        let stop_allowed = match &op {
            Op::StopTicket(_, source, by) => Some(w.engine.policy().may_stop(source, by)),
            _ => None,
        };
        let before = w.repos().clone();
        let result = apply(w, &op);
        let after = w.repos();
        let mut found: Vec<(&'static str, String)> = Vec::new();

        // 12. Only a person the Policy allows stops a Ticket, and the Engine
        // itself says so: a stop by anyone else must not succeed.
        if stop_allowed == Some(false) && result.is_ok() {
            found.push((
                "12-stop-not-allowed",
                format!("{} succeeded without the Policy's permission", op.name()),
            ));
        }

        // 1. Atomicity.
        if let Err(err) = &result {
            let mut changed = Vec::new();
            if before.triggers != after.triggers {
                changed.push("triggers");
            }
            if before.threads != after.threads {
                changed.push("threads");
            }
            if before.tickets != after.tickets {
                changed.push("tickets");
            }
            if before.decisions != after.decisions {
                changed.push("decisions");
            }
            if before.trails != after.trails {
                changed.push("trails");
            }
            if before.effects != after.effects {
                changed.push("effects");
            }
            if before.events != after.events {
                changed.push("events");
            }
            if !changed.is_empty() || format!("{before:?}") != format!("{after:?}") {
                found.push((
                    "1-err-changed-state",
                    format!("returned Err({err}) and changed {changed:?}"),
                ));
            }
        }

        // 2 and 3. Effect integrity, on this step's new Effects.
        let fresh = &after.effects[before.effects.len().min(after.effects.len())..];
        for effect in fresh {
            check_effect(after, effect, &mut self.delivered, &mut found);
        }

        // Decision ids.
        let fresh_events = &after.events[before.events.len().min(after.events.len())..];
        let mut recorded: BTreeSet<String> =
            before.decisions.keys().map(ToString::to_string).collect();
        for event in fresh_events {
            if let DomainEvent::DecisionRecorded {
                decision,
                kind,
                thread,
            } = &event.event
                && !recorded.insert(decision.to_string())
            {
                let old = before.decisions.get(decision);
                found.push((
                    "x-decision-id-reused",
                    format!(
                        "DecisionRecorded {decision} ({kind:?} on {thread}) reuses an id; the earlier one was {:?}",
                        old.map(|d| (d.kind, d.thread.to_string(), ids(&d.triggers)))
                    ),
                ));
            }
        }

        self.secrets(w.engine.policy(), &op, &result, &before, after, &mut found);

        let mut trail_events = false;
        for event in fresh_events {
            match &event.event {
                DomainEvent::TrailAsked {
                    ticket,
                    assignment,
                    n,
                    action,
                } => {
                    trail_events = true;
                    let was = before.trails.get(assignment);
                    if *n != was.map_or(1, |c| c.next) || was.is_some_and(|c| c.pending.is_some()) {
                        found.push((
                            "7-trail-numbering",
                            format!("{assignment} was handed step {n} after {was:?}"),
                        ));
                    }
                    let running = before
                        .tickets
                        .get(ticket)
                        .and_then(Ticket::running_assignment)
                        .is_some_and(|a| a.id == *assignment);
                    if !running && !matches!(action, TrailAction::Close { .. }) {
                        found.push((
                            "7-trail-not-running",
                            format!("{assignment} was asked to {action:?} while not running"),
                        ));
                    }
                    let kept = after
                        .trails
                        .get(assignment)
                        .map(|c| (c.next, c.pending.as_ref().map(|p| (p.n, &p.action))));
                    if kept != Some((n + 1, Some((*n, action)))) {
                        found.push((
                            "7-trail-cursor",
                            format!("{assignment} holds {kept:?} after asking step {n}"),
                        ));
                    }
                }
                DomainEvent::TrailDone {
                    assignment,
                    n,
                    outcome,
                    ..
                } => {
                    trail_events = true;
                    let waiting = before
                        .trails
                        .get(assignment)
                        .and_then(|c| c.pending.as_ref())
                        .filter(|p| p.n == *n);
                    match (waiting, outcome) {
                        (None, _) => found.push((
                            "7-trail-done-unpaired",
                            format!("{assignment} answered step {n}, which did not wait"),
                        )),
                        (Some(p), TrailOutcome::Signed(s)) if s.body.action != p.action => found
                            .push((
                                "7-trail-done-mismatch",
                                format!(
                                    "{assignment} step {n} did {:?}, not {:?}",
                                    s.body.action, p.action
                                ),
                            )),
                        _ => {}
                    }
                    if after
                        .trails
                        .get(assignment)
                        .is_none_or(|c| c.pending.is_some())
                    {
                        found.push((
                            "7-trail-done-left-pending",
                            format!("{assignment} still waits after step {n} was answered"),
                        ));
                    }
                }
                _ => {}
            }
        }
        if !trail_events && before.trails != after.trails {
            found.push((
                "7-trail-silent-change",
                "a cursor changed with no trail event".into(),
            ));
        }

        // 4 and 5. State invariants, counted when they first break.
        let now = state_violations(after);
        for (k, detail) in &now {
            if !self.state.contains_key(k) {
                found.push((k.0, detail.clone()));
            }
        }
        self.state = now;

        let findings = found
            .into_iter()
            .map(|(kind, detail)| Finding {
                category: format!("{kind} <- {}", op.name()),
                kind,
                detail,
            })
            .collect();
        (op, result, findings)
    }
}

type Found = Vec<(&'static str, String)>;

impl Checker {
    fn secrets(
        &mut self,
        policy: &AllowlistPolicy,
        op: &Op,
        result: &Result<String, Error>,
        before: &MemoryRepositories,
        after: &MemoryRepositories,
        found: &mut Found,
    ) {
        let fresh_events = &after.events[before.events.len().min(after.events.len())..];
        let fresh_effects = &after.effects[before.effects.len().min(after.effects.len())..];
        let grant = |r: &MemoryRepositories, t: &TicketId, s: &SecretId| {
            r.tickets.get(t).and_then(|t| t.secrets.get(s)).cloned()
        };
        let mut moved = 0;
        let mut created = 0;
        for (id, now) in &after.tickets {
            let was = before.tickets.get(id);
            for (secret, new) in &now.secrets {
                let old = was.and_then(|t| t.secrets.get(secret));
                match old {
                    None => {
                        created += 1;
                        if !matches!(new, SecretGrant::Waiting { .. }) {
                            found.push((
                                "8-grant-new-not-waiting",
                                format!("{id} {secret} began as {new:?}"),
                            ));
                        }
                    }
                    Some(old) if old == new => {}
                    Some(SecretGrant::Waiting { .. })
                        if !matches!(new, SecretGrant::Waiting { .. }) =>
                    {
                        moved += 1;
                    }
                    Some(old) => found.push((
                        "8-grant-changed",
                        format!("{id} {secret} went from {old:?} to {new:?}"),
                    )),
                }
            }
            for secret in was.iter().flat_map(|t| t.secrets.keys()) {
                if !now.secrets.contains_key(secret) {
                    found.push(("8-grant-removed", format!("{id} {secret} is gone")));
                }
            }
        }
        let asked: Vec<(&TicketId, &SecretId)> = fresh_events
            .iter()
            .filter_map(|e| match &e.event {
                DomainEvent::SecretAsked { ticket, secret } => Some((ticket, secret)),
                _ => None,
            })
            .collect();
        let answers: Vec<_> = fresh_events
            .iter()
            .filter_map(|e| match &e.event {
                DomainEvent::SecretAnswered {
                    ticket,
                    secret,
                    by,
                    granted,
                } => Some((ticket, secret, by, *granted)),
                _ => None,
            })
            .collect();
        if asked.len() != created || answers.len() != moved {
            found.push((
                "8-grant-without-event",
                format!(
                    "{created} new grants and {moved} answered ones, but {} asks and {} answers",
                    asked.len(),
                    answers.len()
                ),
            ));
        }

        let new_inputs = |t: &Ticket| -> Vec<(SecretId, bool, Actor, bool)> {
            let seen = before.tickets.get(&t.id).map_or(0, |b| b.inputs.len());
            t.inputs[seen..]
                .iter()
                .filter_map(|i| match &i.body {
                    InputBody::Secret {
                        secret,
                        granted,
                        by,
                    } => Some((secret.clone(), *granted, by.clone(), i.from_allowed_person)),
                    _ => None,
                })
                .collect()
        };
        let secret_inputs: usize = after.tickets.values().map(|t| new_inputs(t).len()).sum();
        if secret_inputs != answers.len() {
            found.push((
                "9-answer-input",
                format!(
                    "{} answers added {secret_inputs} Secret Inputs",
                    answers.len()
                ),
            ));
        }
        for (ticket, secret, by, granted) in &answers {
            let key = ((*ticket).clone(), (*secret).clone());
            if !self.answered.insert(key.clone()) {
                found.push((
                    "9-answer-without-ask",
                    format!("{ticket} {secret} was answered twice"),
                ));
            }
            let asked_before = before.events.iter().any(|e| {
                matches!(&e.event, DomainEvent::SecretAsked { ticket: t, secret: s } if t == *ticket && s == *secret)
            });
            let Some(SecretGrant::Waiting { answerable_by, .. }) = grant(before, ticket, secret)
            else {
                found.push((
                    "9-answer-without-ask",
                    format!("{ticket} {secret} did not wait before its answer"),
                ));
                continue;
            };
            if !asked_before {
                found.push((
                    "9-answer-without-ask",
                    format!("{ticket} {secret} has no SecretAsked before its answer"),
                ));
            }
            match op {
                Op::AnswerSecret(t, s, source, actor, allow)
                    if t == *ticket && s == *secret && actor == *by && allow == granted =>
                {
                    let named = if answerable_by.is_empty() {
                        policy.may_approve(source, actor)
                    } else {
                        answerable_by.contains(&format!("{source}:{}", actor.id))
                    };
                    if !(actor.is_person() && named) {
                        found.push((
                            "9-answer-not-allowed",
                            format!("{source}:{} answered {secret} on {ticket}", actor.id),
                        ));
                    }
                }
                _ => found.push((
                    "9-answer-by-other-command",
                    format!("{} answered {secret} on {ticket}", op.name()),
                )),
            }
            let verdict_kept = match grant(after, ticket, secret) {
                Some(SecretGrant::Granted { by: who, .. }) => *granted && who == **by,
                Some(SecretGrant::Denied { by: who, .. }) => !*granted && who == **by,
                _ => false,
            };
            let inputs = after
                .tickets
                .get(*ticket)
                .map(&new_inputs)
                .unwrap_or_default();
            let matching = inputs
                .iter()
                .filter(|(s, g, b, person)| s == *secret && g == granted && b == *by && *person)
                .count();
            if !verdict_kept || matching != 1 {
                found.push((
                    "9-answer-input",
                    format!(
                        "{ticket} {secret}: grant {:?}, {matching} matching Inputs of {inputs:?}",
                        grant(after, ticket, secret)
                    ),
                ));
            }
        }

        for (ticket, secret) in &asked {
            let channels = after.tickets.get(*ticket).map_or(0, |t| t.channels.len());
            let posts = fresh_effects
                .iter()
                .filter(|e| matches!(e, Effect::Send(m) if m.ask == Some(Ask::Secret { secret: (*secret).clone() }) && m.ticket.as_ref() == Some(*ticket) && m.kind == MessageKind::Attention))
                .count();
            let others = fresh_effects
                .iter()
                .filter(|e| !matches!(e, Effect::Send(m) if m.ask.is_some()))
                .count();
            if posts != channels || others != 0 || !matches!(op, Op::AskSecret(..)) {
                found.push((
                    "10-ask-posts",
                    format!("{ticket} {secret}: {posts} asks for {channels} Channels, {others} other Effects"),
                ));
            }
        }
        if let Op::AskSecret(t, s, ..) = op
            && let Ok(shown) = result
            && let Some(old) = grant(before, t, s)
        {
            let quiet = fresh_events.is_empty()
                && fresh_effects.is_empty()
                && before.tickets == after.tickets;
            if !quiet || *shown != format!("{old:?}") {
                found.push((
                    "10-ask-repeated-committed",
                    format!("{t} {s} was asked again and returned {shown}"),
                ));
            }
        }

        let used: Vec<_> = fresh_events
            .iter()
            .filter_map(|e| match &e.event {
                DomainEvent::KeyUsed {
                    ticket,
                    assignment,
                    secret,
                    method,
                    url,
                } => Some((ticket, assignment, secret, method, url)),
                _ => None,
            })
            .collect();
        for (ticket, assignment, secret, method, url) in &used {
            let open = before.tickets.get(*ticket).filter(|t| t.ended.is_none());
            let running = open
                .and_then(Ticket::running_assignment)
                .is_some_and(|a| a.id == **assignment);
            let denied = matches!(
                grant(before, ticket, secret),
                Some(SecretGrant::Denied { .. })
            );
            let by_use_key = matches!(op, Op::UseKey(t, a, s, m, u)
                if t == *ticket && a == *assignment && s == *secret && m == *method && u == *url);
            if !running || denied || !by_use_key {
                found.push((
                    "11-key-used",
                    format!(
                        "{} recorded {assignment} using {secret} on {ticket} (running {running}, denied {denied})",
                        op.name()
                    ),
                ));
            }
            let bounded = (1..=16).contains(&method.len())
                && method.bytes().all(|b| b.is_ascii_uppercase())
                && url.chars().count() <= MAX_URL_CHARS;
            if !bounded {
                found.push((
                    "11-key-bounds",
                    format!(
                        "{assignment} used {secret} with method {method:?} and {} url characters",
                        url.chars().count()
                    ),
                ));
            }
            if before.tickets != after.tickets || !fresh_effects.is_empty() || used.len() != 1 {
                found.push((
                    "11-key-changed-state",
                    format!("{assignment} used {secret}: tickets or effects changed"),
                ));
            }
        }
        if matches!(op, Op::UseKey(..)) && result.is_ok() && used.len() != 1 {
            found.push((
                "11-key-used",
                format!("an accepted use_key recorded {} events", used.len()),
            ));
        }
    }
}

fn check_effect(
    r: &MemoryRepositories,
    effect: &Effect,
    delivered: &mut BTreeMap<InputId, AssignmentId>,
    found: &mut Vec<(&'static str, String)>,
) {
    let has_assignment = |t: &TicketId, a: &AssignmentId| {
        r.tickets
            .get(t)
            .is_some_and(|t| t.assignments.iter().any(|x| x.id == *a))
    };
    match effect {
        Effect::Assess(c) => {
            for t in &c.triggers {
                if !r.triggers.contains_key(&t.id) {
                    found.push((
                        "2-assess-missing-trigger",
                        format!("Assess {} names {}", c.eval, t.id),
                    ));
                }
            }
            for s in &c.tickets {
                if !r.tickets.contains_key(&s.id) {
                    found.push((
                        "2-assess-missing-ticket",
                        format!("Assess {} names {}", c.eval, s.id),
                    ));
                }
            }
            let running = r
                .threads
                .get(&c.thread)
                .and_then(|t| t.eval.as_ref())
                .is_some_and(|e| e.id == c.eval);
            if !running {
                found.push((
                    "2-assess-eval-not-running",
                    format!(
                        "Assess {} on {} but the thread runs {:?}",
                        c.eval,
                        c.thread,
                        r.threads
                            .get(&c.thread)
                            .and_then(|t| t.eval.as_ref())
                            .map(|e| e.id.to_string())
                    ),
                ));
            }
        }
        Effect::Launch {
            ticket, assignment, ..
        } => {
            if !r.tickets.contains_key(ticket) {
                found.push((
                    "2-launch-missing-ticket",
                    format!("Launch {ticket} {assignment}"),
                ));
            } else if !has_assignment(ticket, assignment) {
                found.push((
                    "2-launch-missing-assignment",
                    format!("Launch {ticket} {assignment}"),
                ));
            }
        }
        Effect::Deliver {
            ticket,
            assignment,
            inputs,
        } => {
            let Some(t) = r.tickets.get(ticket) else {
                found.push((
                    "2-deliver-missing-ticket",
                    format!("Deliver {ticket} {assignment}"),
                ));
                return;
            };
            if !has_assignment(ticket, assignment) {
                found.push((
                    "2-deliver-missing-assignment",
                    format!("Deliver {ticket} {assignment}"),
                ));
            }
            if inputs.is_empty() {
                found.push((
                    "2-deliver-empty",
                    format!("Deliver {ticket} {assignment} []"),
                ));
            }
            for id in inputs {
                match t.inputs.iter().find(|i| i.id == *id) {
                    None => found.push((
                        "2-deliver-foreign-input",
                        format!(
                            "Deliver {ticket} {assignment} names {id}, not an Input of {ticket}"
                        ),
                    )),
                    Some(i) if i.sent_to.as_ref() != Some(assignment) => found.push((
                        "2-deliver-sent-to-mismatch",
                        format!(
                            "Deliver {ticket} {assignment} names {id} whose sent_to is {:?}",
                            i.sent_to
                        ),
                    )),
                    Some(_) => {}
                }
                if let Some(first) = delivered.insert(id.clone(), assignment.clone()) {
                    found.push((
                        "3-input-delivered-twice",
                        format!("{id} delivered to {first}, then again to {assignment}"),
                    ));
                }
            }
        }
        Effect::Stop {
            ticket, assignment, ..
        } => {
            if !r.tickets.contains_key(ticket) {
                found.push((
                    "2-stop-missing-ticket",
                    format!("Stop {ticket} {assignment}"),
                ));
            } else if !has_assignment(ticket, assignment) {
                found.push((
                    "2-stop-missing-assignment",
                    format!("Stop {ticket} {assignment}"),
                ));
            }
        }
        Effect::Send(m) => {
            if let Some(t) = &m.ticket
                && !r.tickets.contains_key(t)
            {
                found.push(("2-send-missing-ticket", format!("Send {} names {t}", m.id)));
            }
            if let Some(ask) = &m.ask {
                let stands = m
                    .ticket
                    .as_ref()
                    .and_then(|t| r.tickets.get(t))
                    .is_some_and(|t| t.awaits(ask));
                if !stands {
                    found.push((
                        "2-send-ask-not-standing",
                        format!("Send {} asks {ask:?} of {:?}", m.id, m.ticket),
                    ));
                }
                if m.kind != MessageKind::Attention {
                    found.push((
                        "2-send-ask-not-attention",
                        format!("Send {} asks {ask:?} as {:?}", m.id, m.kind),
                    ));
                }
            }
        }
    }
}

// ---------- walking, replaying, shrinking ----------

struct Walk {
    policy0: AllowlistPolicy,
    cmds: Vec<Cmd>,
    /// First step at which each category fired in this walk.
    findings: BTreeMap<String, (usize, Finding)>,
    fingerprint: String,
    outcomes: BTreeMap<&'static str, (u64, u64)>,
    effects: BTreeMap<&'static str, u64>,
}

fn walk(seed: u64, steps: usize, check: bool) -> Walk {
    let mut rng = Lcg::new(seed);
    let policy0 = base_policy(&mut rng);
    let mut w = World::new(policy0.clone());
    let mut checker = Checker::default();
    let mut cmds = Vec::with_capacity(steps);
    let mut findings = BTreeMap::new();
    let mut results = String::new();
    let mut outcomes: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
    for step in 0..steps {
        let cmd = gen_cmd(&mut rng, &mut w);
        let (op, result) = if check {
            let (op, result, found) = checker.step(&mut w, &cmd);
            for f in found {
                findings.entry(f.category.clone()).or_insert((step, f));
            }
            (op, result)
        } else {
            let op = resolve(&w, &cmd);
            let result = apply(&mut w, &op);
            (op, result)
        };
        let slot = outcomes.entry(op.name()).or_default();
        if result.is_ok() {
            slot.0 += 1;
        } else {
            slot.1 += 1;
        }
        results.push_str(&format!("{step}: {result:?}\n"));
        cmds.push(cmd);
    }
    let fingerprint = format!("{:?}\n{results}", w.repos());
    let mut effects: BTreeMap<&'static str, u64> = BTreeMap::new();
    for effect in &w.repos().effects {
        let kind = match effect {
            Effect::Assess(_) => "assess",
            Effect::Launch { .. } => "launch",
            Effect::Deliver { .. } => "deliver",
            Effect::Stop { .. } => "stop",
            Effect::Send(_) => "send",
        };
        *effects.entry(kind).or_default() += 1;
    }
    Walk {
        policy0,
        cmds,
        findings,
        fingerprint,
        outcomes,
        effects,
    }
}

/// Runs `cmds` from a fresh engine; returns the first step where `category`
/// fires, with the finding and the state at that step.
fn reproduce(
    policy0: &AllowlistPolicy,
    cmds: &[Cmd],
    category: &str,
) -> Option<(usize, Finding, Vec<String>, MemoryRepositories)> {
    let mut w = World::new(policy0.clone());
    let mut checker = Checker::default();
    let mut printed = Vec::new();
    for (i, cmd) in cmds.iter().enumerate() {
        let (op, result, found) = checker.step(&mut w, cmd);
        let outcome = match &result {
            Ok(v) if v.is_empty() => "Ok".to_owned(),
            Ok(v) => format!("Ok({v})"),
            Err(e) => format!("Err({e})"),
        };
        printed.push(format!("{}  -> {outcome}", show(&op)));
        if let Some(f) = found.into_iter().find(|f| f.category == category) {
            return Some((i, f, printed, w.repos().clone()));
        }
    }
    None
}

fn shrink(policy0: &AllowlistPolicy, mut best: Vec<Cmd>, category: &str) -> Vec<Cmd> {
    let mut chunk = (best.len() / 2).max(1);
    loop {
        let mut changed = false;
        let mut i = 0;
        while i < best.len() {
            let end = (i + chunk).min(best.len());
            let mut candidate: Vec<Cmd> = best[..i].iter().chain(&best[end..]).cloned().collect();
            if let Some((at, ..)) = reproduce(policy0, &candidate, category) {
                candidate.truncate(at + 1);
                best = candidate;
                changed = true;
            } else {
                i += chunk;
            }
        }
        if chunk > 1 {
            chunk /= 2;
        } else if !changed && !drop_pair(policy0, &mut best, category) {
            return best;
        }
    }
}

/// Removes two steps that only go together (an Eval and its decide, say).
fn drop_pair(policy0: &AllowlistPolicy, best: &mut Vec<Cmd>, category: &str) -> bool {
    for i in 0..best.len() {
        for j in i + 1..best.len() {
            let mut candidate = best.clone();
            candidate.remove(j);
            candidate.remove(i);
            if let Some((at, ..)) = reproduce(policy0, &candidate, category) {
                candidate.truncate(at + 1);
                *best = candidate;
                return true;
            }
        }
    }
    false
}

// ---------- printing ----------

/// Readable names for the engine's hashed ids: `ev[th1#2]` is the second Eval
/// of th1, `tk[th1#2]` its Ticket, `as[th1#2/1]` that Ticket's first
/// Assignment, `in[th1#2@5]` its Input with seq 5.
fn alias_table(states: &[&MemoryRepositories]) -> BTreeMap<String, String> {
    let mut table = BTreeMap::new();
    for th in THREADS {
        for n in 1..=150u32 {
            let ev = key("ev", &[th, &n.to_string()]);
            let tk = key("tk", &[&ev]);
            table.insert(key("dc", &[&ev]), format!("dc[{th}#{n}]"));
            table.insert(tk, format!("tk[{th}#{n}]"));
            table.insert(ev, format!("ev[{th}#{n}]"));
        }
    }
    for r in states {
        for d in r.decisions.values() {
            if d.eval.is_none() {
                table.insert(
                    d.id.to_string(),
                    format!(
                        "dc[gate {} {}]",
                        d.thread,
                        d.triggers.first().map(|t| t.as_str()).unwrap_or("")
                    ),
                );
            }
        }
        for t in r.tickets.values() {
            let Some(name) = table.get(t.id.as_str()).cloned() else {
                continue;
            };
            let inner = &name[3..name.len() - 1];
            for (i, a) in t.assignments.iter().enumerate() {
                table.insert(a.id.to_string(), format!("as[{inner}/{}]", i + 1));
            }
            for input in &t.inputs {
                table.insert(input.id.to_string(), format!("in[{inner}@{}]", input.seq));
            }
        }
    }
    table
}

fn alias(text: &str, table: &BTreeMap<String, String>) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let end = i + 19;
        if end <= bytes.len()
            && bytes[i].is_ascii_lowercase()
            && bytes[i + 1].is_ascii_lowercase()
            && bytes[i + 2] == b'_'
            && bytes[i + 3..end].iter().all(|b| b.is_ascii_hexdigit())
            && let Some(name) = table.get(&text[i..end])
        {
            out.push_str(name);
            i = end;
            continue;
        }
        let ch = text[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn show_policy(p: &AllowlistPolicy) -> String {
    format!(
        "paused={} people={:?} approvers={:?} bots={:?} mode={:?} rules={:?} auto_approve={} eval_timeout={}",
        p.paused,
        p.people,
        p.approvers,
        p.bots,
        p.accept_mode,
        p.rules,
        p.auto_approve_microusd,
        p.eval_timeout_ms
    )
}

fn show_plan(p: &Plan) -> String {
    match p {
        Plan::Session { .. } => "session".into(),
        Plan::Swarm { agents, .. } => format!("swarm({agents})"),
    }
}

fn show(cmd: &Op) -> String {
    match cmd {
        Op::Receive(t) => format!(
            "receive {} by {}:{} ({:?}, {:?}) threads={:?} reply_to={}",
            t.id,
            t.source,
            t.actor.id,
            t.actor.kind,
            t.addressed,
            ids(&t.threads),
            t.reply_to.len()
        ),
        Op::EvaluateDue => "evaluate_due".into(),
        Op::Decide(e, Verdict::Accept(a)) => format!(
            "decide {e} accept plan={} budget={} title={:?} scope={:?} ack={:?} rules={:?}",
            show_plan(&a.plan),
            a.budget_microusd,
            a.title,
            a.scope,
            a.acknowledge,
            a.rules
        ),
        Op::Decide(e, Verdict::Ignore { .. }) => format!("decide {e} ignore"),
        Op::Decide(e, Verdict::Stop { ticket, .. }) => format!("decide {e} stop {ticket}"),
        Op::Approve(t, source, by) => format!("approve {t} by {source}:{} ({:?})", by.id, by.kind),
        Op::Note(t) => format!("note {t}"),
        Op::StopTicket(t, source, by) => {
            format!("stop_ticket {t} by {source}:{} ({:?})", by.id, by.kind)
        }
        Op::Retry(t) => format!("retry {t}"),
        Op::Handback(t, trigger) => format!("handback {t} {trigger}"),
        Op::Started(t, a) => format!("assignment_started {t} {a}"),
        Op::Ended(t, a) => format!("assignment_ended {t} {a}"),
        Op::Report(t, a, r) => format!(
            "report {t} {a} {:?} question={:?} spent={}",
            r.status, r.question, r.spent_microusd
        ),
        Op::TrailAsk(t, a, action) => format!("trail_ask {t} {a} {action:?}"),
        Op::TrailDone(t, a, n, TrailOutcome::Lost { .. }) => {
            format!("trail_done {t} {a} #{n} lost")
        }
        Op::TrailDone(t, a, n, TrailOutcome::Signed(s)) => format!(
            "trail_done {t} {a} #{n} signed #{} by {} {:?} -> {:?}",
            s.body.n, s.body.ticket, s.body.action, s.body.outcome
        ),
        Op::AskSecret(t, secret, label, by) => {
            format!("ask_secret {t} {secret} label={label:?} answerable_by={by:?}")
        }
        Op::AnswerSecret(t, secret, source, by, allow) => format!(
            "answer_secret {t} {secret} by {source}:{} ({:?}) allow={allow}",
            by.id, by.kind
        ),
        Op::UseKey(t, a, secret, method, url) => format!(
            "use_key {t} {a} {secret} {method:?} ({} url characters)",
            url.chars().count()
        ),
        Op::SetPolicy(p) => format!("set_policy {}", show_policy(p)),
        Op::Advance(ms) => format!("advance_clock {ms}ms"),
    }
}

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Hit {
    seed: u64,
    step: usize,
    seeds_hit: u64,
    finding: Finding,
}

#[test]
fn seeded_random_walk_keeps_atomicity_and_effect_integrity() {
    let seeds = env_or("FUZZ_SEEDS", 300);
    let steps = env_or("FUZZ_STEPS", 200) as usize;
    let first = env_or("FUZZ_FIRST_SEED", 0);
    let mut hits: BTreeMap<String, Hit> = BTreeMap::new();
    let mut outcomes: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
    let mut effects: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut walks: BTreeMap<u64, Walk> = BTreeMap::new();
    for seed in first..first + seeds {
        let run = walk(seed, steps, true);
        let again = walk(seed, steps, false);
        let mut findings: Vec<(usize, Finding)> = run.findings.values().cloned().collect();
        if run.fingerprint != again.fingerprint {
            let at = run
                .fingerprint
                .bytes()
                .zip(again.fingerprint.bytes())
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            findings.push((
                steps,
                Finding {
                    category: "6-replay-differs <- replay".into(),
                    kind: "6-replay-differs",
                    detail: format!("first difference at byte {at}"),
                },
            ));
        }
        for (name, (ok, err)) in &run.outcomes {
            let slot = outcomes.entry(name).or_default();
            slot.0 += ok;
            slot.1 += err;
        }
        for (kind, n) in &run.effects {
            *effects.entry(kind).or_default() += n;
        }
        let mut keep = false;
        for (step, finding) in findings {
            let hit = hits.entry(finding.category.clone()).or_insert_with(|| {
                keep = true;
                Hit {
                    seed,
                    step,
                    seeds_hit: 0,
                    finding: finding.clone(),
                }
            });
            hit.seeds_hit += 1;
            if step < hit.step {
                keep = true;
                *hit = Hit {
                    seed,
                    step,
                    seeds_hit: hit.seeds_hit,
                    finding,
                };
            }
        }
        if keep {
            walks.insert(seed, run);
        }
    }

    let mut report = format!(
        "fuzz: seeds {first}..{} ({seeds} seeds), {steps} steps each\ncommand outcomes (ok, err):\n",
        first + seeds
    );
    for (name, (ok, err)) in &outcomes {
        report.push_str(&format!("  {name:<20} ok={ok:<7} err={err}\n"));
    }
    report.push_str(&format!("effects committed: {effects:?}\n"));
    report.push_str(&format!("distinct violations: {}\n", hits.len()));
    for (category, hit) in &hits {
        let (title, required) = describe(hit.finding.kind);
        let run = &walks[&hit.seed];
        report.push_str(&format!(
            "\n=== {title}\n    category: {category}\n    seeds hit: {} of {seeds}; smallest first step: seed {} step {}\n    required: {required}\n",
            hit.seeds_hit, hit.seed, hit.step
        ));
        if hit.finding.kind == "6-replay-differs" {
            report.push_str(&format!("    printed: {}\n", hit.finding.detail));
            continue;
        }
        let minimal = shrink(&run.policy0, run.cmds[..=hit.step].to_vec(), category);
        let Some((at, finding, printed, state)) = reproduce(&run.policy0, &minimal, category)
        else {
            report.push_str("    (did not reproduce after shrinking)\n");
            continue;
        };
        let table = alias_table(&[&state]);
        report.push_str(&format!(
            "    minimal sequence ({} steps) under initial policy: {}\n",
            at + 1,
            show_policy(&run.policy0)
        ));
        for (i, line) in printed.iter().enumerate() {
            report.push_str(&format!("      {i:>2}. {}\n", alias(line, &table)));
        }
        report.push_str(&format!(
            "    printed: {}\n",
            alias(&finding.detail, &table)
        ));
    }
    println!("{report}");
    assert!(hits.is_empty(), "{report}");
    if seeds * steps as u64 >= 2_000 {
        for name in [
            "trail_ask",
            "trail_done",
            "ask_secret",
            "answer_secret",
            "use_key",
        ] {
            let ok = outcomes.get(name).map_or(0, |o| o.0);
            assert!(ok > 0, "{name} never succeeded\n{report}");
        }
    }
}
