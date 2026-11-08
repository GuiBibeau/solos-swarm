use serde_json::json;
use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, AssignmentId, ChannelRef, DomainEvent, Event, Plan,
    ReportIn, ReportStatus, ThreadKey, TicketId, Trigger, TriggerId, Verdict,
};
use solos_agent_core::trail::{
    ActText, CloseReason, Digest, MAX_NAMED_CHARS, MAX_URL_CHARS, MAX_WHY_CHARS, PageAction,
    PageSeen, Provenance, STEP_VERSION, Signature, SignedStep, StepBody, StepOutcome, TrailAction,
    TrailCursor, TrailKey, TrailOutcome,
};
use solos_agent_core::{AllowlistPolicy, Engine, Error, MemoryRepositories, Repositories};
use std::collections::BTreeMap;

type TestEngine = Engine<MemoryRepositories, AllowlistPolicy, Box<dyn Fn() -> u64>>;

struct World {
    engine: TestEngine,
    ticket: TicketId,
    assignment: AssignmentId,
}

impl World {
    fn new() -> Self {
        let policy = AllowlistPolicy {
            people: ["slack:alice".to_owned()].into(),
            approvers: ["slack:alice".to_owned()].into(),
            ..AllowlistPolicy::default()
        };
        let mut engine: TestEngine =
            Engine::new(MemoryRepositories::default(), policy, Box::new(|| 1_000));
        let (ticket, assignment) = running_ticket(&mut engine, "t1", "th1");
        Self {
            engine,
            ticket,
            assignment,
        }
    }

    fn another_ticket(&mut self) -> (TicketId, AssignmentId) {
        running_ticket(&mut self.engine, "t2", "th2")
    }

    fn ask(&mut self, action: TrailAction) -> Result<u32, Error> {
        self.engine
            .trail_ask(&self.ticket, &self.assignment, action)
    }

    fn done(&mut self, n: u32, outcome: TrailOutcome) -> Result<(), Error> {
        self.engine
            .trail_done(&self.ticket, &self.assignment, n, outcome)
    }

    fn step(&mut self, action: TrailAction) -> u32 {
        let n = self.ask(action.clone()).unwrap();
        let outcome = self.signed(n, action, page("https://github.com/"));
        self.done(n, outcome).unwrap();
        n
    }

    fn signed(&self, n: u32, action: TrailAction, outcome: StepOutcome) -> TrailOutcome {
        TrailOutcome::signed(SignedStep {
            body: StepBody {
                v: STEP_VERSION,
                ticket: self.ticket.clone(),
                assignment: self.assignment.clone(),
                n,
                environment: "local".into(),
                session: "7a3c0f9e12d45b60".into(),
                session_step: n - 1,
                at_ms: 999,
                provenance: None,
                action,
                outcome,
            },
            signer: key(),
            signature: signature(),
        })
    }

    fn retry(&mut self) -> AssignmentId {
        self.engine.retry(&self.ticket).unwrap().unwrap();
        let next = self.engine.repos().tickets[&self.ticket]
            .assignments
            .last()
            .unwrap()
            .id
            .clone();
        self.engine.assignment_started(&self.ticket, &next).unwrap();
        std::mem::replace(&mut self.assignment, next)
    }

    fn cursor(&self) -> Option<TrailCursor> {
        self.engine.repos().trail(&self.assignment)
    }

    fn events(&self) -> Vec<Event> {
        self.engine.repos().events.clone()
    }

    fn trail_events(&self) -> Vec<DomainEvent> {
        self.events()
            .into_iter()
            .map(|e| e.event)
            .filter(|e| {
                matches!(
                    e,
                    DomainEvent::TrailAsked { .. } | DomainEvent::TrailDone { .. }
                )
            })
            .collect()
    }

    fn state(&self) -> (Vec<Event>, BTreeMap<AssignmentId, TrailCursor>) {
        let repos = self.engine.repos();
        (repos.events.clone(), repos.trails.clone())
    }
}

fn running_ticket(
    engine: &mut TestEngine,
    trigger: &str,
    thread: &str,
) -> (TicketId, AssignmentId) {
    engine
        .receive(Trigger {
            id: TriggerId::new(trigger),
            source: "slack".into(),
            at_ms: 1_000,
            actor: Actor::person("alice"),
            addressed: Addressed::Mention,
            text: "@agent read my notifications".into(),
            threads: vec![ThreadKey::new(thread)],
            reply_to: vec![ChannelRef {
                source: "slack".into(),
                address: BTreeMap::from([("channel".to_owned(), "C1".to_owned())]),
            }],
            meta: serde_json::Value::Null,
        })
        .unwrap();
    let eval = engine.evaluate_due().unwrap().remove(0);
    let accept = Verdict::Accept(Accept {
        title: "Read notifications".into(),
        scope: "Read my GitHub notifications".into(),
        reason: "alice asked".into(),
        plan: Plan::Session {
            harness: None,
            environment: None,
            method: None,
        },
        budget_microusd: Some(1_000_000),
        acknowledge: Acknowledge::Standard,
        rules: Vec::new(),
        repo: None,
        forge: None,
        upstream: None,
    });
    let decision = engine.decide(&eval, accept).unwrap();
    let ticket = engine.repos().decisions[&decision].ticket.clone().unwrap();
    let assignment = engine.repos().tickets[&ticket].assignments[0].id.clone();
    engine.assignment_started(&ticket, &assignment).unwrap();
    (ticket, assignment)
}

fn key() -> TrailKey {
    TrailKey::try_from(format!("ed25519:{}", "9c".repeat(32))).unwrap()
}

fn signature() -> Signature {
    Signature::try_from("ab".repeat(64)).unwrap()
}

fn open(url: &str) -> TrailAction {
    TrailAction::Open {
        url: url.into(),
        key: None,
    }
}

fn click(element: u32) -> TrailAction {
    TrailAction::Act {
        action: PageAction::Click,
        element: Some(element),
        text: None,
    }
}

fn close(reason: CloseReason) -> TrailAction {
    TrailAction::Close { reason }
}

fn page(url: &str) -> StepOutcome {
    StepOutcome::Page(PageSeen {
        url: url.into(),
        status: Some(200),
        title: "GitHub".into(),
        snapshot: Digest::of(b"snapshot"),
        view: Digest::of(b"view"),
        target: None,
        filled: Vec::new(),
        keyed: Vec::new(),
    })
}

fn closed(reason: CloseReason) -> StepOutcome {
    StepOutcome::Closed { steps: 2, reason }
}

fn refused(result: Result<impl std::fmt::Debug, Error>, about: &str) {
    match result {
        Err(Error::Refused(why)) => assert!(why.contains(about), "{why}"),
        other => panic!("expected a refusal about {about:?}, got {other:?}"),
    }
}

fn invalid(result: Result<impl std::fmt::Debug, Error>) {
    assert!(
        matches!(result, Err(Error::Invalid(_))),
        "expected an invalid step: {result:?}"
    );
}

#[test]
fn steps_are_numbered_from_one_in_the_order_they_are_asked() {
    let mut w = World::new();
    assert_eq!(
        w.cursor(),
        None,
        "an Assignment that never asked has no row"
    );

    assert_eq!(w.step(open("https://github.com/login")), 1);
    assert_eq!(w.step(click(14)), 2);
    assert_eq!(w.ask(click(15)).unwrap(), 3);

    let cursor = w.cursor().unwrap();
    assert_eq!(cursor.next, 4);
    assert_eq!(cursor.pending.as_ref().map(|p| p.n), Some(3));
    let order: Vec<String> = w
        .trail_events()
        .iter()
        .map(|e| match e {
            DomainEvent::TrailAsked { n, .. } => format!("asked {n}"),
            DomainEvent::TrailDone { n, .. } => format!("done {n}"),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(order, ["asked 1", "done 1", "asked 2", "done 2", "asked 3"]);
}

#[test]
fn each_assignment_numbers_its_own_steps() {
    let mut w = World::new();
    w.step(open("https://example.com"));
    w.step(click(1));
    let first = w.retry();
    assert_eq!(w.step(open("https://example.com")), 1);
    assert_eq!(w.engine.repos().trail(&first).unwrap().next, 3);
    assert_eq!(w.cursor().unwrap().next, 2);
}

#[test]
fn a_step_waits_for_the_answer_to_the_one_before() {
    let mut w = World::new();
    let n = w.ask(open("https://github.com")).unwrap();

    let before = w.state();
    refused(w.ask(click(1)), "still running");
    assert_eq!(w.state(), before, "a refused ask records nothing");

    let outcome = w.signed(n, open("https://github.com"), page("https://github.com/"));
    w.done(n, outcome).unwrap();
    assert_eq!(w.ask(click(1)).unwrap(), 2);
}

#[test]
fn only_the_running_assignment_may_act() {
    let mut w = World::new();
    w.step(open("https://github.com"));
    let old = w.retry();

    let stale = w.engine.trail_ask(&w.ticket, &old, click(1));
    refused(stale, "is not running");
    refused(
        w.engine
            .trail_ask(&w.ticket, &AssignmentId::new("as_unknown"), click(1)),
        "is not running",
    );
    assert!(w.ask(click(1)).is_ok(), "the new Assignment runs");

    let elsewhere = w
        .engine
        .trail_ask(&TicketId::new("tk_unknown"), &w.assignment, click(1));
    assert!(
        matches!(elsewhere, Err(Error::Missing { .. })),
        "{elsewhere:?}"
    );
}

#[test]
fn an_assignment_belongs_to_one_ticket() {
    let mut w = World::new();
    let (other, _) = w.another_ticket();
    refused(
        w.engine.trail_ask(&other, &w.assignment, click(1)),
        "is not running",
    );
    let n = w.ask(click(1)).unwrap();
    invalid(w.engine.trail_ask(&other, &w.assignment, click(2)));
    let lost = TrailOutcome::Lost { why: "x".into() };
    invalid(w.engine.trail_done(&other, &w.assignment, n, lost));
    assert_eq!(w.cursor().unwrap().pending.map(|p| p.n), Some(n));
}

#[test]
fn an_assignment_that_is_not_yet_running_may_not_act() {
    let mut w = World::new();
    w.engine.retry(&w.ticket).unwrap().unwrap();
    let pending = w.engine.repos().tickets[&w.ticket]
        .assignments
        .last()
        .unwrap()
        .id
        .clone();
    refused(
        w.engine.trail_ask(&w.ticket, &pending, click(1)),
        "is not running",
    );
}

#[test]
fn a_close_is_admitted_after_the_assignment_and_the_ticket_end() {
    let mut w = World::new();
    w.step(open("https://github.com"));
    w.engine
        .assignment_ended(&w.ticket, &w.assignment, "process exited")
        .unwrap();
    refused(w.ask(click(1)), "is not running");

    let n = w.ask(close(CloseReason::AssignmentEnded)).unwrap();
    assert_eq!(n, 2);
    let outcome = w.signed(
        n,
        close(CloseReason::AssignmentEnded),
        closed(CloseReason::AssignmentEnded),
    );
    w.done(n, outcome).unwrap();

    let mut w = World::new();
    w.step(open("https://github.com"));
    let report = ReportIn {
        status: ReportStatus::Ready,
        text: "done".into(),
        question: None,
        spent_microusd: 0,
        harvest: None,
    };
    w.engine.report(&w.ticket, &w.assignment, report).unwrap();
    assert!(w.engine.repos().tickets[&w.ticket].ended.is_some());
    assert_eq!(w.ask(close(CloseReason::AssignmentEnded)).unwrap(), 2);
}

#[test]
fn there_is_nothing_to_close_before_a_step() {
    let mut w = World::new();
    refused(w.ask(close(CloseReason::Idle)), "nothing to close");
    assert_eq!(w.cursor(), None);
    assert!(w.trail_events().is_empty());
}

#[test]
fn a_done_that_does_not_answer_the_asked_step_is_refused() {
    let mut w = World::new();
    let asked = open("https://github.com/login");
    let n = w.ask(asked.clone()).unwrap();
    let other_ticket = TicketId::new("tk_other");
    let other_assignment = AssignmentId::new("as_other");

    let cases: Vec<(&str, TrailOutcome)> = vec![
        (
            "another action",
            w.signed(
                n,
                open("https://evil.example"),
                page("https://evil.example/"),
            ),
        ),
        (
            "another kind of action",
            w.signed(n, click(1), page("https://github.com/")),
        ),
        (
            "another number",
            w.signed(n + 1, asked.clone(), page("https://github.com/")),
        ),
        ("a version it does not know", {
            let TrailOutcome::Signed(mut step) =
                w.signed(n, asked.clone(), page("https://github.com/"))
            else {
                unreachable!()
            };
            step.body.v = STEP_VERSION + 1;
            TrailOutcome::Signed(step)
        }),
        ("another ticket", {
            let TrailOutcome::Signed(mut step) =
                w.signed(n, asked.clone(), page("https://github.com/"))
            else {
                unreachable!()
            };
            step.body.ticket = other_ticket;
            TrailOutcome::Signed(step)
        }),
        ("another assignment", {
            let TrailOutcome::Signed(mut step) =
                w.signed(n, asked.clone(), page("https://github.com/"))
            else {
                unreachable!()
            };
            step.body.assignment = other_assignment;
            TrailOutcome::Signed(step)
        }),
        (
            "a close for an open",
            w.signed(n, asked.clone(), closed(CloseReason::Idle)),
        ),
    ];
    let before = w.state();
    for (what, outcome) in cases {
        let result = w.done(n, outcome);
        assert!(
            matches!(result, Err(Error::Invalid(_))),
            "{what} was not refused: {result:?}"
        );
        assert_eq!(w.state(), before, "{what} recorded something");
    }

    let right = w.signed(n, asked, page("https://github.com/login"));
    w.done(n, right).unwrap();
    assert_eq!(w.cursor().unwrap().pending, None);
}

#[test]
fn a_session_closes_only_as_asked_or_by_a_crash() {
    let mut w = World::new();
    let n = w.ask(open("https://github.com")).unwrap();
    let crashed = w.signed(n, open("https://github.com"), closed(CloseReason::Crashed));
    w.done(n, crashed).unwrap();

    let asked = close(CloseReason::Idle);
    let n = w.ask(asked.clone()).unwrap();
    for outcome in [
        page("https://github.com/"),
        closed(CloseReason::Shutdown),
        StepOutcome::NeedsApproval {
            secret: "local/github".into(),
            label: "sign in".into(),
        },
    ] {
        let wrong = w.signed(n, asked.clone(), outcome);
        invalid(w.done(n, wrong));
    }
    let refused_close = StepOutcome::Refused {
        why: "no session".into(),
    };
    let answers = [refused_close, closed(CloseReason::Idle)];
    let [first, second] = answers;
    let a = w.signed(n, asked.clone(), first);
    w.done(n, a).unwrap();
    let n = w.ask(asked.clone()).unwrap();
    let b = w.signed(n, asked, second);
    w.done(n, b).unwrap();
}

#[test]
fn a_done_for_an_ended_assignment_is_recorded() {
    let mut w = World::new();
    let n = w.ask(click(1)).unwrap();
    w.engine
        .assignment_ended(&w.ticket, &w.assignment, "process exited")
        .unwrap();
    w.engine
        .stop_ticket(
            &w.ticket,
            "slack",
            Actor::person("alice"),
            "a person stopped it",
        )
        .unwrap();

    let outcome = w.signed(n, click(1), page("https://github.com/notifications"));
    w.done(n, outcome.clone()).unwrap();

    assert_eq!(w.cursor().unwrap().pending, None);
    let recorded = w.trail_events().pop().unwrap();
    assert_eq!(
        recorded,
        DomainEvent::TrailDone {
            ticket: w.ticket.clone(),
            assignment: w.assignment.clone(),
            n,
            outcome,
        }
    );
}

#[test]
fn a_step_is_answered_once_and_only_the_step_that_waits() {
    let mut w = World::new();
    let lost = || TrailOutcome::Lost {
        why: "the node went away".into(),
    };
    let missing = w.done(1, lost());
    assert!(matches!(missing, Err(Error::Missing { .. })), "{missing:?}");

    let n = w.ask(click(1)).unwrap();
    refused(w.done(n + 1, lost()), "is not waiting");
    refused(w.done(n - 1, lost()), "is not waiting");
    w.done(n, lost()).unwrap();
    refused(w.done(n, lost()), "is not waiting");
    assert_eq!(w.trail_events().len(), 2);
}

#[test]
fn a_lost_step_frees_the_next_one() {
    let mut w = World::new();
    let n = w.ask(open("https://github.com")).unwrap();
    w.done(
        n,
        TrailOutcome::Lost {
            why: "its answer did not verify".into(),
        },
    )
    .unwrap();
    assert_eq!(w.ask(click(1)).unwrap(), 2);
}

#[test]
fn what_a_model_writes_into_the_chain_is_bounded() {
    let mut w = World::new();
    let url = |len: usize| open(&format!("https://example.com/{}", "a".repeat(len - 20)));
    let text = |len: usize| TrailAction::Act {
        action: PageAction::Select,
        element: Some(3),
        text: Some(ActText::Named {
            text: "é".repeat(len),
        }),
    };
    let login = |len: usize| TrailAction::Login {
        login: "a".repeat(len),
        username_element: None,
        password_element: None,
        submit: true,
    };
    let key = |len: usize| TrailAction::Open {
        url: "https://example.com/".into(),
        key: Some("a".repeat(len)),
    };
    let cases = [
        ("url", url(MAX_URL_CHARS + 1), url(MAX_URL_CHARS)),
        ("text", text(MAX_NAMED_CHARS + 1), text(MAX_NAMED_CHARS)),
        ("login", login(MAX_NAMED_CHARS + 1), login(MAX_NAMED_CHARS)),
        ("key", key(MAX_NAMED_CHARS + 1), key(MAX_NAMED_CHARS)),
    ];
    for (what, too_long, longest) in cases {
        let refused = w.ask(too_long);
        assert!(
            matches!(refused, Err(Error::Invalid(_))),
            "a {what} over the bound was not refused: {refused:?}"
        );
        let n = w.ask(longest).unwrap();
        w.done(n, TrailOutcome::Lost { why: "x".into() }).unwrap();
    }
    assert_eq!(w.cursor().unwrap().next, 5, "a refused ask burns no number");
}

#[test]
fn a_reason_nobody_signed_is_cut_to_its_bound_and_still_recorded() {
    let mut w = World::new();
    let n = w.ask(click(1)).unwrap();
    let why = "é".repeat(MAX_WHY_CHARS * 3);
    w.done(n, TrailOutcome::Lost { why }).unwrap();

    let Some(DomainEvent::TrailDone {
        outcome: TrailOutcome::Lost { why },
        ..
    }) = w.trail_events().pop()
    else {
        panic!("the answer was not recorded")
    };
    assert_eq!(why.chars().count(), MAX_WHY_CHARS);
    assert_eq!(w.cursor().unwrap().pending, None);
}

#[test]
fn a_crash_is_reported_by_the_browser_and_never_asked_for() {
    let mut w = World::new();
    w.step(open("https://github.com"));
    invalid(w.ask(close(CloseReason::Crashed)));
    assert_eq!(w.ask(close(CloseReason::Idle)).unwrap(), 2);
}

fn hex(byte: &str, times: usize) -> String {
    byte.repeat(times)
}

#[test]
fn the_chain_records_a_step_in_its_documented_shape() {
    let mut w = World::new();
    let action = TrailAction::Open {
        url: "https://github.com/login".into(),
        key: None,
    };
    let n = w.ask(action.clone()).unwrap();
    let outcome = TrailOutcome::signed(SignedStep {
        body: StepBody {
            v: STEP_VERSION,
            ticket: w.ticket.clone(),
            assignment: w.assignment.clone(),
            n,
            environment: "local".into(),
            session: "7a3c0f9e12d45b60".into(),
            session_step: 0,
            at_ms: 999,
            provenance: Some(Provenance {
                binary_sha256: Digest::try_from(format!("sha256:{}", hex("b1", 32))).unwrap(),
                argv_sha256: Digest::try_from(format!("sha256:{}", hex("a2", 32))).unwrap(),
            }),
            action,
            outcome: StepOutcome::Page(PageSeen {
                url: "https://github.com/login".into(),
                status: Some(200),
                title: "Sign in".into(),
                snapshot: Digest::try_from(format!("sha256:{}", hex("8a", 32))).unwrap(),
                view: Digest::try_from(format!("sha256:{}", hex("2f", 32))).unwrap(),
                target: None,
                filled: vec!["username".into()],
                keyed: Vec::new(),
            }),
        },
        signer: key(),
        signature: signature(),
    });
    w.done(n, outcome).unwrap();

    let events = w.events();
    let [.., asked, done] = &events[..] else {
        panic!("{events:?}")
    };
    let (ticket, assignment) = (w.ticket.as_str(), w.assignment.as_str());
    let open = json!({"kind": "open", "url": "https://github.com/login"});
    let expected_asked = json!({
        "at_ms": 1_000, "type": "trail_asked", "ticket": ticket, "assignment": assignment,
        "n": 1, "action": open,
    });
    let expected_done = json!({
        "at_ms": 1_000, "type": "trail_done", "ticket": ticket, "assignment": assignment, "n": 1,
        "outcome": {
            "kind": "signed",
            "body": {
                "v": 1, "ticket": ticket, "assignment": assignment, "n": 1,
                "environment": "local", "session": "7a3c0f9e12d45b60", "session_step": 0,
                "at_ms": 999,
                "provenance": {
                    "binary_sha256": format!("sha256:{}", hex("b1", 32)),
                    "argv_sha256": format!("sha256:{}", hex("a2", 32)),
                },
                "action": open,
                "outcome": {
                    "kind": "page", "url": "https://github.com/login", "status": 200,
                    "title": "Sign in",
                    "snapshot": format!("sha256:{}", hex("8a", 32)),
                    "view": format!("sha256:{}", hex("2f", 32)),
                    "filled": ["username"],
                },
            },
            "signer": format!("ed25519:{}", hex("9c", 32)),
            "signature": hex("ab", 64),
        },
    });
    assert_eq!(serde_json::to_value(asked).unwrap(), expected_asked);
    assert_eq!(serde_json::to_value(done).unwrap(), expected_done);
    assert_eq!(
        &serde_json::from_value::<Event>(expected_asked).unwrap(),
        asked
    );
    assert_eq!(
        &serde_json::from_value::<Event>(expected_done).unwrap(),
        done
    );
}

#[test]
fn typed_text_appears_only_as_a_digest_and_a_length() {
    let typed = TrailAction::Act {
        action: PageAction::Type,
        element: Some(2),
        text: Some(ActText::Typed {
            digest: Digest::of(b"hunter2"),
            chars: 7,
        }),
    };
    let json = serde_json::to_string(&typed).unwrap();
    assert!(!json.contains("hunter2"), "{json}");
    assert!(json.contains("\"chars\":7"), "{json}");
    assert_eq!(serde_json::from_str::<TrailAction>(&json).unwrap(), typed);
}

#[test]
fn a_digest_a_key_and_a_signature_must_be_lowercase_hex_of_the_right_length() {
    let digest = |s: String| serde_json::from_value::<Digest>(json!(s));
    let signer = |s: String| serde_json::from_value::<TrailKey>(json!(s));
    let sig = |s: String| serde_json::from_value::<Signature>(json!(s));

    assert!(digest(format!("sha256:{}", hex("0f", 32))).is_ok());
    for bad in [
        format!("sha256:{}", hex("0f", 31)),
        format!("sha256:{}", hex("0F", 32)),
        format!("sha256:{}", hex("zz", 32)),
        format!("sha1:{}", hex("0f", 32)),
        hex("0f", 32),
        String::new(),
    ] {
        assert!(digest(bad.clone()).is_err(), "{bad}");
    }
    assert!(signer(format!("ed25519:{}", hex("9c", 32))).is_ok());
    assert!(signer(format!("ed25519:{}", hex("9c", 64))).is_err());
    assert!(signer(hex("9c", 32)).is_err());
    assert!(sig(hex("ab", 64)).is_ok());
    assert!(sig(hex("ab", 63)).is_err());
    assert!(sig(format!("{}g", hex("ab", 63))).is_err());
    assert_eq!(Digest::of(b"x").digits().len(), 64);
    assert_eq!(
        Digest::of(b"abc").as_str(),
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn a_step_body_with_a_field_this_build_does_not_know_is_refused() {
    let mut w = World::new();
    let n = w.ask(click(1)).unwrap();
    let TrailOutcome::Signed(step) = w.signed(n, click(1), page("https://github.com/")) else {
        unreachable!()
    };
    let signed = serde_json::to_value(&step).unwrap();
    assert!(serde_json::from_value::<SignedStep>(signed.clone()).is_ok());
    for at in ["/body", "/body/outcome"] {
        let mut value = signed.clone();
        value.pointer_mut(at).unwrap()["png"] = json!(format!("sha256:{}", hex("00", 32)));
        let parsed = serde_json::from_value::<SignedStep>(value);
        assert!(parsed.is_err(), "a png under {at} was accepted");
    }
}
