//! The same seeded command walks drive an Engine over `MemoryRepositories`
//! and one over `SqliteStore`. After every step both stores hold the same
//! Triggers, Threads, Tickets, Decisions, trails, memos, Effects and Events,
//! answer the port's queries alike, and every command returned the same
//! result.
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
    MemEngine, SqlEngine, TempDb, clock, first_difference, memory_contents, page_seen, port_view,
    signed_step, sqlite_contents,
};
use solos_agent_core::change::{Deliverable, RepoName};
use solos_agent_core::memo::{
    Memo, MemoActor, MemoAudience, MemoAuthor, MemoIn, MemoReview, MemoScope, MemoSource,
    MemoState, MemoTrust, PROPOSAL_TTL_MS,
};
use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, Approval, AssignmentId, AssignmentState, ChannelRef,
    Effect, EvalId, InputBody, MemoId, Method, Plan, ReportIn, ReportStatus, SecretGrant, SecretId,
    ThreadKey, Ticket, TicketId, Trigger, TriggerId, Verdict,
};
use solos_agent_core::trail::{
    ActText, CloseReason, Digest, MAX_URL_CHARS, PageAction, StepOutcome, TrailAction, TrailOutcome,
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
const SECRETS: [&str; 3] = ["local/github-gui", "local/openai", "laptop/gh"];
const TRIGGER_TEXT: &str = "please look at this, \"quoted\" and ünïcode";

/// Every public Engine command, by the name `Op::name` gives it.
const COMMANDS: [&str; 26] = [
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
    "restart",
    "report",
    "trail_ask",
    "trail_done",
    "ask_secret",
    "answer_secret",
    "use_key",
    "propose_memo",
    "activate_memo",
    "retire_memo",
    "forget_memo",
    "show_memos",
    "revise_memo",
    "expire_memos",
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
    Restart(TicketId, AssignmentId),
    Report(TicketId, AssignmentId, ReportIn),
    TrailAsk(TicketId, AssignmentId, TrailAction),
    TrailDone(TicketId, AssignmentId, u32, TrailOutcome),
    AskSecret(TicketId, SecretId, String, Vec<String>),
    AnswerSecret(TicketId, SecretId, String, Actor, bool),
    UseKey(TicketId, AssignmentId, SecretId, String, String),
    ProposeMemo(MemoIn),
    ActivateMemo(MemoId, MemoReview),
    RetireMemo(MemoId, MemoActor, String),
    ForgetMemo(MemoId, String, Actor),
    ShowMemos(MemoAudience),
    ReviseMemo(MemoId, MemoReview, String, Option<MemoId>),
    ExpireMemos,
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
            Op::Decide(_, Verdict::Decline { .. }) => "decide(decline)",
            Op::Decide(_, Verdict::Reply { .. }) => "decide(reply)",
            Op::Approve(..) => "approve",
            Op::Note(_) => "note",
            Op::StopTicket(_) => "stop_ticket",
            Op::Retry(_) => "retry",
            Op::Handback(..) => "handback",
            Op::Started(..) => "assignment_started",
            Op::Ended(..) => "assignment_ended",
            Op::Restart(..) => "restart",
            Op::Report(..) => "report",
            Op::TrailAsk(..) => "trail_ask",
            Op::TrailDone(..) => "trail_done",
            Op::AskSecret(..) => "ask_secret",
            Op::AnswerSecret(..) => "answer_secret",
            Op::UseKey(..) => "use_key",
            Op::ProposeMemo(_) => "propose_memo",
            Op::ActivateMemo(..) => "activate_memo",
            Op::RetireMemo(..) => "retire_memo",
            Op::ForgetMemo(..) => "forget_memo",
            Op::ShowMemos(_) => "show_memos",
            Op::ReviseMemo(..) => "revise_memo",
            Op::ExpireMemos => "expire_memos",
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
        Op::StopTicket(t) => e
            .stop_ticket(t, "slack", Actor::person("alice"), "a person stopped it")
            .map(unit),
        Op::Retry(t) => e.retry(t).map(|a| format!("{a:?}")),
        Op::Handback(t, trigger) => e.handback(t, trigger, "not mine").map(|b| b.to_string()),
        Op::Started(t, a) => e.assignment_started(t, a).map(unit),
        Op::Ended(t, a) => e.assignment_ended(t, a, "process exited").map(unit),
        Op::Restart(t, a) => e.restart(t, a, "host restarted").map(|a| format!("{a:?}")),
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
        Op::ProposeMemo(m) => e.propose_memo(m.clone()).map(|id| id.to_string()),
        Op::ActivateMemo(m, review) => e.activate_memo(m, review.clone()).map(unit),
        Op::RetireMemo(m, by, reason) => e.retire_memo(m, by.clone(), reason).map(unit),
        Op::ForgetMemo(m, source, by) => e.forget_memo(m, source, by.clone()).map(unit),
        Op::ShowMemos(to) => e.show_memos(to.clone()).map(|index| index.text),
        Op::ReviseMemo(m, review, text, merges) => e
            .revise_memo(m, review.clone(), text.clone(), merges.as_ref())
            .map(|id| id.to_string()),
        Op::ExpireMemos => e.expire_memos().map(|ids| format!("{ids:?}")),
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
        answer_sources: Default::default(),
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
        text: TRIGGER_TEXT.into(),
        threads,
        reply_to,
        meta,
    }
}

fn gen_accept(rng: &mut Lcg) -> Accept {
    let plan = if rng.chance(70) {
        Plan::Session {
            harness: rng.chance(20).then(|| "pi".to_owned()),
            environment: rng.chance(30).then(|| "laptop".to_owned()),
            method: rng.chance(30).then(|| Method {
                deliverable: rng.pick(&[
                    Deliverable::Answer,
                    Deliverable::Change,
                    Deliverable::PullRequest,
                ]),
            }),
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
        budget_microusd: Some(rng.pick(&[0, 400_000, 1_500_000, 3_000_000, 8_000_000])),
        acknowledge: rng.pick(&[
            Acknowledge::Standard,
            Acknowledge::Silent,
            Acknowledge::Text("ack".into()),
        ]),
        rules: rng.pick(&[vec![], vec!["r1".to_owned()]]),
        repo: rng
            .chance(40)
            .then(|| RepoName::try_from("acme/widgets".to_owned()).unwrap()),
        forge: None,
        upstream: None,
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

fn gen_action(rng: &mut Lcg) -> TrailAction {
    match rng.below(20) {
        0..=5 => TrailAction::Open {
            url: if rng.chance(5) {
                format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS))
            } else {
                format!("https://example.com/{}", rng.below(4))
            },
            key: rng.chance(20).then(|| "openai".to_owned()),
        },
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
                    text: "é".repeat(rng.pick(&[3, 100, 101])),
                }),
                _ => Some(ActText::Typed {
                    digest: Digest::of(b"typed"),
                    chars: 5,
                }),
            },
        },
        12..=13 => TrailAction::Login {
            login: "github-gui".into(),
            username_element: rng.chance(50).then_some(1),
            password_element: rng.chance(50).then_some(2),
            submit: rng.chance(70),
        },
        14..=15 => TrailAction::Read {
            part: rng.chance(50).then_some(2),
            element: rng.chance(50).then_some(9),
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

fn gen_outcome(
    rng: &mut Lcg,
    ticket: &TicketId,
    assignment: &AssignmentId,
    n: u32,
    asked: &TrailAction,
) -> TrailOutcome {
    let honest = match asked {
        TrailAction::Close { reason } => StepOutcome::Closed {
            steps: 3,
            reason: *reason,
        },
        _ => page_seen(),
    };
    let step = |n, action: &TrailAction, outcome| {
        TrailOutcome::signed(signed_step(ticket, assignment, n, action.clone(), outcome))
    };
    match rng.below(100) {
        0..=59 => step(n, asked, honest),
        60..=69 => TrailOutcome::Lost {
            why: "the node went away".into(),
        },
        70..=74 => {
            let other = TrailAction::Open {
                url: "https://evil.example".into(),
                key: None,
            };
            step(n, &other, honest)
        }
        75..=79 => step(n + 1, asked, honest),
        80..=84 => step(
            n,
            asked,
            StepOutcome::Closed {
                steps: 1,
                reason: CloseReason::Crashed,
            },
        ),
        85..=89 => step(
            n,
            asked,
            StepOutcome::Closed {
                steps: 1,
                reason: CloseReason::Idle,
            },
        ),
        90..=94 => step(
            n,
            asked,
            StepOutcome::Refused {
                why: "not on the login's site".into(),
            },
        ),
        _ => {
            let TrailOutcome::Signed(mut signed) = step(n, asked, honest) else {
                unreachable!()
            };
            signed.body.v += 1;
            TrailOutcome::Signed(signed)
        }
    }
}

fn gen_trail_done(rng: &mut Lcg, r: &MemoryRepositories) -> Op {
    let waiting: Vec<_> = r
        .trails
        .values()
        .filter_map(|c| c.pending.as_ref().map(|p| (c, p)))
        .collect();
    if !waiting.is_empty() && rng.chance(85) {
        let (cursor, pending) = waiting[rng.below(waiting.len())];
        let n = match rng.below(10) {
            0 => pending.n + 1,
            1 => pending.n.saturating_sub(1),
            _ => pending.n,
        };
        let outcome = gen_outcome(rng, &cursor.ticket, &cursor.assignment, n, &pending.action);
        return Op::TrailDone(cursor.ticket.clone(), cursor.assignment.clone(), n, outcome);
    }
    let ticket = pick_ticket(rng, r, |_| true);
    let assignment = pick_assignment(rng, r, &ticket);
    let asked = gen_action(rng);
    let outcome = gen_outcome(rng, &ticket, &assignment, 1, &asked);
    Op::TrailDone(ticket, assignment, 1, outcome)
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

fn gen_ask_secret(rng: &mut Lcg, r: &MemoryRepositories) -> Op {
    let asked: Vec<(TicketId, SecretId)> = r
        .tickets
        .values()
        .filter(|t| t.ended.is_none())
        .flat_map(|t| t.secrets.keys().map(|s| (t.id.clone(), s.clone())))
        .collect();
    let (ticket, secret) = if !asked.is_empty() && rng.chance(35) {
        rng.pick(&asked)
    } else {
        (pick_ticket(rng, r, |t| t.ended.is_none()), gen_secret(rng))
    };
    let label = match rng.below(20) {
        0 => String::new(),
        1 => "  ".to_owned(),
        _ => "sign in to https://github.com as gui".to_owned(),
    };
    Op::AskSecret(ticket, secret, label, gen_answerable_by(rng))
}

fn gen_answer_secret(rng: &mut Lcg, r: &MemoryRepositories) -> Op {
    let ticket = pick_ticket(rng, r, |t| {
        t.ended.is_none() && !waiting_secrets(t).is_empty()
    });
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
    let source = if rng.chance(85) { "slack" } else { "github" };
    let by = match rng.below(20) {
        0..=9 => Actor::person("alice"),
        10..=13 => Actor::person("bob"),
        14 | 15 => Actor::person("mallory"),
        16 | 17 => Actor::bot("ci-bot"),
        _ => Actor::system("schedule"),
    };
    Op::AnswerSecret(ticket, secret, source.into(), by, rng.chance(50))
}

fn gen_use_key(rng: &mut Lcg, r: &MemoryRepositories) -> Op {
    let ticket = pick_ticket(rng, r, |t| latest_is(t, AssignmentState::Running));
    let assignment = pick_assignment(rng, r, &ticket);
    let held: Vec<SecretId> = r
        .tickets
        .get(&ticket)
        .map(|t| t.secrets.keys().cloned().collect())
        .unwrap_or_default();
    let secret = if !held.is_empty() && rng.chance(60) {
        rng.pick(&held)
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
        0..=16 => "https://api.openai.com/v1/models?q=ünï".to_owned(),
        17 | 18 => format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS)),
        _ => "é".repeat(MAX_URL_CHARS),
    };
    Op::UseKey(ticket, assignment, secret, method.to_owned(), url)
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

/// A memo, preferring (75%) one `want` accepts; 5% a missing one.
fn pick_memo(rng: &mut Lcg, r: &MemoryRepositories, want: impl Fn(&Memo) -> bool) -> MemoId {
    if r.memos.is_empty() || rng.chance(5) {
        return MemoId::new("mm_missing");
    }
    let preferred: Vec<MemoId> = r
        .memos
        .values()
        .filter(|m| want(m))
        .map(|m| m.id.clone())
        .collect();
    if !preferred.is_empty() && rng.chance(75) {
        rng.pick(&preferred)
    } else {
        let all: Vec<MemoId> = r.memos.keys().cloned().collect();
        rng.pick(&all)
    }
}

fn gen_propose_memo(rng: &mut Lcg, r: &MemoryRepositories) -> Op {
    let scope = match rng.below(10) {
        0..=5 => MemoScope::Global,
        _ => MemoScope::Repo(RepoName::try_from("acme/widgets".to_owned()).unwrap()),
    };
    let text = match rng.below(20) {
        0..=6 => TRIGGER_TEXT.to_owned(),
        7..=10 => "Tests run with ./check, never cargo test.".to_owned(),
        11..=17 => format!(
            "Lesson {}: {}",
            rng.below(1_000),
            "ü".repeat(rng.pick(&[5, 200]))
        ),
        18 => String::new(),
        _ => "y".repeat(401),
    };
    let operator = rng.chance(45);
    let author = if operator {
        let (source, by) = gen_clicker(rng);
        MemoAuthor::Operator { source, by }
    } else {
        let ticket = pick_ticket(rng, r, |t| latest_is(t, AssignmentState::Running));
        let assignment = pick_assignment(rng, r, &ticket);
        MemoAuthor::Worker { ticket, assignment }
    };
    let n = match rng.below(20) {
        0 => 0,
        1..=16 => 1,
        _ => 2,
    };
    let sources = (0..n)
        .map(|_| match rng.below(10) {
            0..=3 if operator => MemoSource::Operator,
            0 => MemoSource::Operator,
            1..=5 => {
                let all: Vec<TriggerId> = r.triggers.keys().cloned().collect();
                let trigger = if all.is_empty() || rng.chance(5) {
                    TriggerId::new("t_missing")
                } else {
                    rng.pick(&all)
                };
                let quote = rng.pick(&[TRIGGER_TEXT, TRIGGER_TEXT, "\"quoted\"", "not in it"]);
                MemoSource::Trigger {
                    trigger,
                    quote: quote.to_owned(),
                }
            }
            6..=8 => MemoSource::Call {
                assignment: AssignmentId::new("as_cited"),
                call: "call-1".into(),
                tool: rng
                    .pick(&["run_code", "fetch_url", "browser_read"])
                    .to_owned(),
                transcript: Digest::of(b"transcript"),
                quote: "./check: ok".into(),
            },
            _ => MemoSource::Ticket {
                ticket: pick_ticket(rng, r, |_| true),
            },
        })
        .collect();
    Op::ProposeMemo(MemoIn {
        scope,
        text,
        sources,
        author,
        supersedes: None,
    })
}

fn gen_memo_op(rng: &mut Lcg, r: &MemoryRepositories) -> Op {
    let reason = if rng.chance(5) {
        String::new()
    } else {
        "checked against the transcript".to_owned()
    };
    let review_name = if rng.chance(5) { "" } else { "review:diff" };
    match rng.below(17) {
        0..=4 => gen_propose_memo(rng, r),
        5..=6 => Op::ActivateMemo(
            pick_memo(rng, r, |m| m.state == MemoState::Proposed),
            MemoReview {
                by: review_name.into(),
                reason,
            },
        ),
        7..=8 => {
            let by = if rng.chance(60) {
                let (source, by) = gen_clicker(rng);
                MemoActor::Operator { source, by }
            } else {
                MemoActor::Review {
                    by: review_name.into(),
                }
            };
            Op::RetireMemo(pick_memo(rng, r, Memo::is_live), by, reason)
        }
        9 => {
            let (source, by) = gen_clicker(rng);
            let m = pick_memo(rng, r, |m| !matches!(m.state, MemoState::Forgotten { .. }));
            Op::ForgetMemo(m, source, by)
        }
        10 => Op::ShowMemos(MemoAudience::Router),
        11..=13 => {
            let ticket = pick_ticket(rng, r, |_| true);
            let assignment = pick_assignment(rng, r, &ticket);
            Op::ShowMemos(MemoAudience::Worker { ticket, assignment })
        }
        14..=15 => {
            let m = pick_memo(rng, r, |m| m.state == MemoState::Proposed);
            let scope = r.memos.get(&m).map(|m| m.scope.clone());
            let merges = rng.chance(40).then(|| {
                pick_memo(rng, r, |other| {
                    other.is_active()
                        && other.trust != MemoTrust::Person
                        && Some(&other.scope) == scope.as_ref()
                })
            });
            let text = match rng.below(10) {
                0 => String::new(),
                1 => TRIGGER_TEXT.to_owned(),
                _ => format!("Revised lesson {}: ü", rng.below(1_000)),
            };
            let review = MemoReview {
                by: review_name.into(),
                reason,
            };
            Op::ReviseMemo(m, review, text, merges)
        }
        // Now and then a week passes first, so a proposal is due.
        _ if rng.chance(30) => Op::Advance(PROPOSAL_TTL_MS),
        _ => Op::ExpireMemos,
    }
}

fn gen_op(rng: &mut Lcg, w: &mut Walk) -> Op {
    let r = w.mem.repos();
    match rng.below(145) {
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
                harvest: None,
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
            if rng.chance(50) {
                Op::Ended(t, a)
            } else {
                Op::Restart(t, a)
            }
        }
        79..=82 => Op::SetPolicy(mutate_policy(rng, w.mem.policy())),
        83..=89 => Op::Advance(rng.pick(&[1, 1_000, 40_000, 130_000])),
        90..=96 => Op::RunEffects(1 + rng.below(4)),
        97..=102 => {
            let t = pick_ticket(rng, r, |t| latest_is(t, AssignmentState::Running));
            let a = pick_assignment(rng, r, &t);
            Op::TrailAsk(t, a, gen_action(rng))
        }
        103..=108 => gen_trail_done(rng, r),
        112..=117 => gen_ask_secret(rng, r),
        118..=125 => gen_answer_secret(rng, r),
        126..=129 => gen_use_key(rng, r),
        130..=144 => gen_memo_op(rng, r),
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
        Effect::Launch { ticket, .. }
        | Effect::Deliver { ticket, .. }
        | Effect::Stop { ticket, .. } => Some(format!("ticket:{ticket}")),
        Effect::Forge(job) => Some(format!("forge:{}", job.ticket())),
        Effect::Assess(_) => None,
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
        let memo = if !a.memos.is_empty() && p.chance(80) {
            p.pick(&a.memos).id
        } else {
            MemoId::new("mm_missing")
        };
        assert_eq!(m.memo(&memo), s.memo(&memo), "{at}: {memo}");
        assert_eq!(s.read_memo(&memo).unwrap(), m.memo(&memo), "{at}: {memo}");
        let assignment = if !a.trails.is_empty() && p.chance(80) {
            p.pick(&a.trails).assignment
        } else {
            AssignmentId::new("as_missing")
        };
        assert_eq!(
            m.trail(&assignment),
            s.trail(&assignment),
            "{at}: {assignment}"
        );
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
