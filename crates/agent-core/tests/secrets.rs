use serde_json::json;
use solos_agent_core::model::{
    Accept, Acknowledge, Actor, ActorKind, Addressed, Ask, AssignmentId, ChannelRef, DomainEvent,
    Effect, Event, InputBody, MAX_SECRET_CHARS, MessageKind, OutboundMessage, Plan, ReportIn,
    ReportStatus, SecretGrant, SecretId, Stage, ThreadKey, Ticket, TicketId, Trigger, TriggerId,
    Verdict,
};
use solos_agent_core::trail::MAX_URL_CHARS;
use solos_agent_core::{AllowlistPolicy, Engine, Error, MemoryRepositories};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

type TestEngine = Engine<MemoryRepositories, AllowlistPolicy, Box<dyn Fn() -> u64>>;

const LABEL: &str = "sign in to https://github.com as gui";
const TEXT: &str =
    "Read notifications wants to sign in to https://github.com as gui for the first time.";
const FOR: [&str; 2] = ["slack:carol", "slack:ci-bot"];

struct World {
    engine: TestEngine,
    now: Rc<Cell<u64>>,
    ticket: TicketId,
}

impl World {
    fn new() -> Self {
        let mut world = Self::with(policy(), 1_000_000);
        let assignment = world.assignment();
        world
            .engine
            .assignment_started(&world.ticket, &assignment)
            .unwrap();
        world.engine.repos_mut().take_effects();
        world
    }

    fn awaiting_budget() -> Self {
        let policy = AllowlistPolicy {
            auto_approve_microusd: 500_000,
            ..policy()
        };
        Self::with(policy, 1_000_000)
    }

    fn with(policy: AllowlistPolicy, budget_microusd: u64) -> Self {
        let now = Rc::new(Cell::new(1_000));
        let clock = {
            let now = now.clone();
            Box::new(move || now.get()) as Box<dyn Fn() -> u64>
        };
        let mut engine: TestEngine = Engine::new(MemoryRepositories::default(), policy, clock);
        engine
            .receive(trigger(
                "t1",
                Actor::person("alice"),
                "@agent read my notifications",
            ))
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
            budget_microusd,
            acknowledge: Acknowledge::Standard,
            rules: Vec::new(),
            repo: None,
        });
        let decision = engine.decide(&eval, accept).unwrap();
        let ticket = engine.repos().decisions[&decision].ticket.clone().unwrap();
        Self {
            engine,
            now,
            ticket,
        }
    }

    fn ticket(&self) -> Ticket {
        self.engine.repos().tickets[&self.ticket].clone()
    }

    fn assignment(&self) -> AssignmentId {
        self.ticket().latest_assignment().unwrap().id.clone()
    }

    fn ask(&mut self, answerable_by: &[&str]) -> Result<SecretGrant, Error> {
        let answerable_by: Vec<String> = answerable_by.iter().map(|s| (*s).to_owned()).collect();
        self.engine
            .ask_secret(&self.ticket, &secret(), LABEL, &answerable_by)
    }

    fn answer(&mut self, by: Actor, allow: bool) -> Result<(), Error> {
        self.answer_via("slack", by, allow)
    }

    fn answer_via(&mut self, source: &str, by: Actor, allow: bool) -> Result<(), Error> {
        self.engine
            .answer_secret(&self.ticket, &secret(), source, by, allow)
    }

    fn use_key(&mut self, method: &str, url: &str) -> Result<(), Error> {
        let assignment = self.assignment();
        self.engine
            .use_key(&self.ticket, &assignment, &secret(), method, url)
    }

    fn grant(&self) -> Option<SecretGrant> {
        self.ticket().secrets.get(&secret()).cloned()
    }

    fn effects(&mut self) -> Vec<Effect> {
        self.engine.repos_mut().take_effects()
    }

    fn events(&self) -> Vec<Event> {
        self.engine.repos().events.clone()
    }

    fn secret_events(&self) -> Vec<DomainEvent> {
        self.events()
            .into_iter()
            .map(|e| e.event)
            .filter(|e| {
                matches!(
                    e,
                    DomainEvent::SecretAsked { .. }
                        | DomainEvent::SecretAnswered { .. }
                        | DomainEvent::KeyUsed { .. }
                )
            })
            .collect()
    }

    fn state(&self) -> (Ticket, Vec<Event>, Vec<Effect>) {
        let repos = self.engine.repos();
        (self.ticket(), repos.events.clone(), repos.effects.clone())
    }

    fn report(&mut self, status: ReportStatus, text: &str, question: Option<&str>) {
        let assignment = self.assignment();
        let report = ReportIn {
            status,
            text: text.into(),
            question: question.map(str::to_owned),
            spent_microusd: 0,
        };
        self.engine
            .report(&self.ticket, &assignment, report)
            .unwrap();
    }

    fn type_text(&mut self, id: &str, by: Actor, text: &str) {
        self.engine.receive(trigger(id, by, text)).unwrap();
    }
}

fn policy() -> AllowlistPolicy {
    AllowlistPolicy {
        people: ["slack:alice".to_owned(), "slack:carol".to_owned()].into(),
        approvers: ["slack:alice".to_owned()].into(),
        ..AllowlistPolicy::default()
    }
}

fn secret() -> SecretId {
    SecretId::try_from("local/github-gui".to_owned()).unwrap()
}

fn trigger(id: &str, actor: Actor, text: &str) -> Trigger {
    Trigger {
        id: TriggerId::new(id),
        source: "slack".into(),
        at_ms: 1_000,
        actor,
        addressed: Addressed::Mention,
        text: text.into(),
        threads: vec![ThreadKey::new("th1")],
        reply_to: vec![
            ChannelRef {
                source: "slack".into(),
                address: BTreeMap::from([("channel".to_owned(), "C1".to_owned())]),
            },
            ChannelRef {
                source: "discord".into(),
                address: BTreeMap::from([("channel".to_owned(), "20".to_owned())]),
            },
        ],
        meta: serde_json::Value::Null,
    }
}

fn person(id: &str) -> Actor {
    Actor::person(id)
}

fn named(id: &str, name: &str) -> Actor {
    Actor {
        name: Some(name.into()),
        ..Actor::person(id)
    }
}

fn sends(effects: &[Effect]) -> Vec<OutboundMessage> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Send(message) => Some(message.clone()),
            _ => None,
        })
        .collect()
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
        "expected an invalid argument: {result:?}"
    );
}

#[test]
fn asking_twice_posts_one_ask_per_channel_and_records_one_event() {
    let mut w = World::new();

    let first = w.ask(&[]).unwrap();
    let posted = sends(&w.effects());
    assert_eq!(
        first,
        SecretGrant::Waiting {
            label: LABEL.into(),
            answerable_by: Vec::new(),
            asked_ms: 1_000,
        },
        "the new grant waits and says when it was asked"
    );
    let channels: Vec<&str> = posted.iter().map(|m| m.channel.source.as_str()).collect();
    assert_eq!(channels, ["slack", "discord"], "one message per Channel");
    for message in &posted {
        assert_eq!(message.kind, MessageKind::Attention);
        assert_eq!(message.text, TEXT);
        assert_eq!(message.ask, Some(Ask::Secret { secret: secret() }));
        assert_eq!(message.ticket.as_ref(), Some(&w.ticket));
    }
    assert_ne!(posted[0].id, posted[1].id, "each Channel gets its own id");
    assert_eq!(
        w.ticket().attention,
        None,
        "a secret ask is not the Ticket's question"
    );

    let before = w.state();
    let again = w.ask(&["slack:carol"]).unwrap();
    assert_eq!(again, first, "the existing grant comes back unchanged");
    assert_eq!(
        w.state(),
        before,
        "a repeated ask commits no event and no Effect"
    );
    assert_eq!(
        w.secret_events(),
        [DomainEvent::SecretAsked {
            ticket: w.ticket.clone(),
            secret: secret()
        }]
    );
}

#[test]
fn an_answered_grant_is_returned_unchanged_and_asks_nothing() {
    let mut w = World::new();
    w.ask(&FOR).unwrap();
    w.answer(person("carol"), false).unwrap();
    let denied = w.grant().unwrap();
    assert!(matches!(denied, SecretGrant::Denied { .. }));
    w.effects();

    let before = w.state();
    assert_eq!(
        w.ask(&[]).unwrap(),
        denied,
        "a denial is final for the Ticket"
    );
    assert_eq!(w.state(), before);
}

#[test]
fn an_ask_needs_a_label_and_commits_nothing_without_one() {
    let mut w = World::new();
    let before = w.state();
    for label in ["", "  \n\t"] {
        invalid(w.engine.ask_secret(&w.ticket, &secret(), label, &[]));
    }
    assert_eq!(w.state(), before);
    assert_eq!(w.grant(), None);
}

#[test]
fn only_the_budget_ask_carries_the_budget_ask() {
    let mut w = World::awaiting_budget();
    let effects = w.effects();
    let posted = sends(&effects);
    assert_eq!(
        posted.len(),
        2,
        "the approval question goes to both Channels"
    );
    for message in &posted {
        assert_eq!(message.kind, MessageKind::Attention);
        assert_eq!(message.ask, Some(Ask::Budget));
        assert_eq!(
            Some(message.text.as_str()),
            w.ticket().attention.as_deref(),
            "the budget ask is also the Ticket's question"
        );
    }
    w.engine
        .approve(&w.ticket, "slack", person("alice"))
        .unwrap();
    w.effects();

    let assignment = w.assignment();
    w.engine.assignment_started(&w.ticket, &assignment).unwrap();
    w.report(ReportStatus::Progress, "reading", None);
    w.report(ReportStatus::Waiting, "which repo?", Some("Which repo?"));
    w.report(ReportStatus::Blocked, "the page needs a code", None);
    w.report(ReportStatus::Ready, "5 notifications", None);
    let others = sends(&w.effects());
    let kinds: Vec<MessageKind> = others.iter().map(|m| m.kind).collect();
    assert_eq!(
        kinds,
        [
            MessageKind::Attention,
            MessageKind::Attention,
            MessageKind::Attention,
            MessageKind::Attention,
            MessageKind::Result,
            MessageKind::Result,
        ],
        "a question and a blocked report go to both Channels, and so does the result"
    );
    assert!(
        others.iter().all(|m| m.ask.is_none()),
        "a worker's question and its result ask nothing: {others:?}"
    );
}

#[test]
fn an_acknowledgement_carries_no_ask() {
    let w = World::with(policy(), 1_000_000);
    let acknowledged = sends(&w.engine.repos().effects);
    assert_eq!(acknowledged.len(), 2, "one acknowledgement per Channel");
    for message in &acknowledged {
        assert_eq!(message.kind, MessageKind::Acknowledgement);
        assert_eq!(message.ask, None);
    }
}

#[test]
fn a_person_on_the_items_list_may_answer_and_the_grant_records_who_and_when() {
    let mut w = World::new();
    w.ask(&FOR).unwrap();
    w.now.set(2_000);
    w.answer(named("carol", "Carol"), true).unwrap();

    assert_eq!(
        w.grant(),
        Some(SecretGrant::Granted {
            by: named("carol", "Carol"),
            at_ms: 2_000
        })
    );
    assert_eq!(
        w.secret_events(),
        [
            DomainEvent::SecretAsked {
                ticket: w.ticket.clone(),
                secret: secret()
            },
            DomainEvent::SecretAnswered {
                ticket: w.ticket.clone(),
                secret: secret(),
                by: named("carol", "Carol"),
                granted: true
            },
        ]
    );
}

#[test]
fn deny_records_denied_and_tells_the_worker() {
    let mut w = World::new();
    w.ask(&FOR).unwrap();
    w.effects();
    w.now.set(3_000);
    w.answer(person("carol"), false).unwrap();

    assert_eq!(
        w.grant(),
        Some(SecretGrant::Denied {
            by: person("carol"),
            at_ms: 3_000
        })
    );
    let input = w.ticket().inputs.last().unwrap().clone();
    assert_eq!(
        input.body,
        InputBody::Secret {
            secret: secret(),
            granted: false,
            by: person("carol")
        }
    );
    assert!(
        input.from_allowed_person,
        "a person's click is a person's input"
    );
}

#[test]
fn an_approver_who_is_not_on_a_non_empty_list_is_refused() {
    let mut w = World::new();
    w.ask(&FOR).unwrap();
    w.effects();
    let before = w.state();

    refused(
        w.answer(named("alice", "Alice"), true),
        "Alice may not answer for local/github-gui",
    );
    refused(
        w.answer(person("alice"), true),
        "alice may not answer for local/github-gui",
    );
    assert_eq!(w.state(), before, "a refused answer changes nothing");
    assert!(matches!(w.grant(), Some(SecretGrant::Waiting { .. })));
}

#[test]
fn with_an_empty_list_an_approver_may_answer_and_nobody_else_may() {
    let mut w = World::new();
    w.ask(&[]).unwrap();
    let before = w.state();

    refused(w.answer(person("carol"), true), "carol may not answer");
    refused(w.answer(person("dave"), true), "dave may not answer");
    assert_eq!(w.state(), before);

    w.answer(person("alice"), true).unwrap();
    assert!(matches!(w.grant(), Some(SecretGrant::Granted { .. })));
}

#[test]
fn an_entry_names_a_source_and_an_id() {
    let mut w = World::new();
    w.ask(&["slack:carol"]).unwrap();
    refused(
        w.answer_via("discord", person("carol"), true),
        "may not answer",
    );
    refused(w.answer(person("slack:carol"), true), "may not answer");
    w.answer_via("slack", person("carol"), true).unwrap();
}

#[test]
fn a_bot_or_the_system_is_refused_even_when_listed_or_an_approver() {
    let policy = AllowlistPolicy {
        approvers: ["slack:alice".to_owned(), "slack:ci-bot".to_owned()].into(),
        ..policy()
    };
    let mut listed = World::with(policy.clone(), 1_000_000);
    listed.ask(&FOR).unwrap();
    let before = listed.state();
    refused(
        listed.answer(Actor::bot("ci-bot"), true),
        "ci-bot may not answer",
    );
    refused(
        listed.answer(
            Actor {
                kind: ActorKind::System,
                ..person("carol")
            },
            true,
        ),
        "may not answer",
    );
    assert_eq!(listed.state(), before);

    let mut approver = World::with(policy, 1_000_000);
    approver.ask(&[]).unwrap();
    refused(
        approver.answer(Actor::bot("ci-bot"), true),
        "ci-bot may not answer",
    );
    assert!(matches!(
        approver.grant(),
        Some(SecretGrant::Waiting { .. })
    ));
}

#[test]
fn a_second_click_is_refused_and_the_grant_does_not_change() {
    for (first, second) in [(true, true), (true, false), (false, true), (false, false)] {
        let mut w = World::new();
        w.ask(&FOR).unwrap();
        w.now.set(2_000);
        w.answer(person("carol"), first).unwrap();
        let settled = w.grant().unwrap();
        w.effects();
        let before = w.state();

        w.now.set(9_000);
        refused(
            w.answer(person("carol"), second),
            "local/github-gui does not wait for an answer",
        );
        assert_eq!(w.grant().unwrap(), settled, "{first} then {second}");
        assert_eq!(
            w.state(),
            before,
            "no event, input or Effect for the second click"
        );
    }
}

#[test]
fn an_answer_to_an_ask_that_was_never_made_is_refused() {
    let mut w = World::new();
    let before = w.state();
    refused(
        w.answer(person("alice"), true),
        "does not wait for an answer",
    );
    assert_eq!(w.state(), before);
    assert_eq!(w.grant(), None);
}

#[test]
fn the_answer_reaches_a_running_assignment_as_an_input() {
    let mut w = World::new();
    w.ask(&FOR).unwrap();
    w.effects();
    w.answer(person("carol"), true).unwrap();

    let t = w.ticket();
    let assignment = w.assignment();
    let input = t.inputs.last().unwrap();
    assert_eq!(
        input.body,
        InputBody::Secret {
            secret: secret(),
            granted: true,
            by: person("carol")
        }
    );
    assert_eq!(input.sent_to.as_ref(), Some(&assignment));
    let effects = w.effects();
    let [
        Effect::Deliver {
            ticket,
            assignment: to,
            inputs,
        },
    ] = &effects[..]
    else {
        panic!("one delivery: {effects:?}")
    };
    assert_eq!((ticket, to), (&w.ticket, &assignment));
    assert_eq!(inputs, std::slice::from_ref(&input.id));
}

#[test]
fn the_answer_ends_a_hold_on_automatic_inputs() {
    let mut w = World::new();
    w.ask(&FOR).unwrap();
    w.report(ReportStatus::Blocked, "the page needs a code", None);
    w.engine.note(&w.ticket, "CI is green".into()).unwrap();
    w.effects();
    assert_eq!(w.ticket().stage(), Stage::Blocked);
    assert!(w.ticket().waits_for_person());
    assert!(
        w.ticket().inputs.iter().all(|i| i.sent_to.is_none()),
        "the note waits while the Ticket is blocked"
    );

    w.answer(person("carol"), true).unwrap();

    assert!(!w.ticket().waits_for_person(), "a person answered");
    let effects = w.effects();
    let [Effect::Deliver { inputs, .. }] = &effects[..] else {
        panic!("one delivery: {effects:?}")
    };
    let delivered: Vec<InputBody> = w
        .ticket()
        .inputs
        .into_iter()
        .filter(|i| inputs.contains(&i.id))
        .map(|i| i.body)
        .collect();
    assert_eq!(
        delivered,
        [
            InputBody::Note {
                text: "CI is green".into()
            },
            InputBody::Secret {
                secret: secret(),
                granted: true,
                by: person("carol")
            },
        ],
        "the held note and the answer go out together, in order"
    );
}

#[test]
fn typed_allow_is_only_a_trigger_and_grants_nothing() {
    let people = [
        ("carol", person("carol")),
        ("alice", person("alice")),
        ("dave", person("dave")),
        ("ci-bot", Actor::bot("ci-bot")),
    ];
    for (n, text) in ["allow", "Allow!", "allow local/github-gui", "approve it"]
        .into_iter()
        .enumerate()
    {
        for (who, actor) in &people {
            let mut w = World::new();
            w.ask(&FOR).unwrap();
            w.effects();
            let before = w.ticket().secrets;
            let id = format!("typed-{n}-{who}");

            w.type_text(&id, actor.clone(), text);

            let t = w.ticket();
            assert_eq!(t.secrets, before, "{who} typed {text:?}");
            assert!(
                matches!(t.secrets[&secret()], SecretGrant::Waiting { .. }),
                "{who} typed {text:?}"
            );
            let last = t.inputs.last().unwrap();
            assert_eq!(
                last.body,
                InputBody::Trigger {
                    trigger: TriggerId::new(&id)
                },
                "{who}'s message reaches the worker as an ordinary Trigger"
            );
            assert!(
                w.secret_events()
                    .iter()
                    .all(|e| matches!(e, DomainEvent::SecretAsked { .. })),
                "{who} typed {text:?}: nothing answered"
            );
        }
    }
}

#[test]
fn a_keyed_use_is_recorded_for_the_running_assignment() {
    let mut w = World::new();
    let url = "https://api.openai.com/v1/models";

    w.use_key("GET", url).unwrap();
    let before = w.ticket();
    w.ask(&FOR).unwrap();
    w.use_key("POST", url).unwrap();
    w.answer(person("carol"), true).unwrap();
    w.use_key("GET", url).unwrap();

    let assignment = w.assignment();
    let used: Vec<(String, String)> = w
        .secret_events()
        .into_iter()
        .filter_map(|e| match e {
            DomainEvent::KeyUsed {
                ticket,
                assignment: a,
                secret: s,
                method,
                url: sent,
            } => {
                assert_eq!((&ticket, &a, &s), (&w.ticket, &assignment, &secret()));
                Some((method, sent))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        used,
        [
            ("GET".to_owned(), url.to_owned()),
            ("POST".to_owned(), url.to_owned()),
            ("GET".to_owned(), url.to_owned()),
        ],
        "a use with no grant, a waiting one and a granted one are all recorded"
    );
    assert_eq!(
        w.ticket().inputs.len(),
        before.inputs.len() + 1,
        "only the answer added an input"
    );
}

#[test]
fn a_keyed_use_changes_no_state_and_sends_nothing() {
    let mut w = World::new();
    w.ask(&FOR).unwrap();
    w.effects();
    let (ticket, events, effects) = w.state();
    w.use_key("GET", "https://example.com/").unwrap();
    let (after, now_events, now_effects) = w.state();
    assert_eq!(after, ticket, "the Ticket is untouched");
    assert_eq!(now_effects, effects, "nothing is sent");
    assert_eq!(now_events.len(), events.len() + 1);
    assert!(matches!(
        now_events.last().map(|e| &e.event),
        Some(DomainEvent::KeyUsed { .. })
    ));
}

#[test]
fn only_the_running_assignment_may_use_a_key() {
    let mut w = World::new();
    let old = w.assignment();
    w.engine.retry(&w.ticket).unwrap().unwrap();
    let pending = w.assignment();
    let before = w.state();

    for stale in [old, pending.clone(), AssignmentId::new("as_unknown")] {
        refused(
            w.engine
                .use_key(&w.ticket, &stale, &secret(), "GET", "https://example.com/"),
            "is not running",
        );
    }
    assert_eq!(w.state(), before, "a refused use records nothing");

    w.engine.assignment_started(&w.ticket, &pending).unwrap();
    w.use_key("GET", "https://example.com/").unwrap();
}

#[test]
fn a_denied_secret_may_not_be_used() {
    let mut w = World::new();
    w.ask(&FOR).unwrap();
    w.answer(person("carol"), false).unwrap();
    let before = w.state();

    refused(
        w.use_key("GET", "https://example.com/"),
        "a person denied local/github-gui for this ticket",
    );
    assert_eq!(w.state(), before);
}

#[test]
fn a_keyed_use_is_bounded_before_it_reaches_the_chain() {
    let mut w = World::new();
    let before = w.state();
    let long = format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS));
    invalid(w.use_key("GET", &long));
    for method in [
        "",
        "get",
        "Get",
        "GE T",
        "GET1",
        "ÉGET",
        "ABCDEFGHIJKLMNOPQ",
    ] {
        invalid(w.use_key(method, "https://example.com/"));
    }
    assert_eq!(w.state(), before, "an invalid use records nothing");

    let longest = "x".repeat(MAX_URL_CHARS);
    w.use_key("ABCDEFGHIJKLMNOP", &longest).unwrap();
    w.use_key("PATCH", &"é".repeat(MAX_URL_CHARS)).unwrap();
}

#[test]
fn an_ended_ticket_takes_no_ask_answer_or_use() {
    let mut w = World::new();
    w.ask(&FOR).unwrap();
    w.engine
        .stop_ticket(&w.ticket, "slack", Actor::person("alice"), "stopped")
        .unwrap();
    w.effects();
    let before = w.state();

    refused(w.ask(&FOR), "has ended");
    refused(
        w.engine.ask_secret(
            &w.ticket,
            &SecretId::try_from("local/other".to_owned()).unwrap(),
            LABEL,
            &[],
        ),
        "has ended",
    );
    refused(w.answer(person("carol"), true), "has ended");
    refused(w.use_key("GET", "https://example.com/"), "has ended");
    assert_eq!(w.state(), before);
}

#[test]
fn a_ticket_awaits_an_ask_until_it_is_answered_or_ends() {
    let budget = Ask::Budget;
    let item = Ask::Secret { secret: secret() };
    let other = Ask::Secret {
        secret: SecretId::try_from("local/other".to_owned()).unwrap(),
    };

    let mut w = World::awaiting_budget();
    assert!(w.ticket().awaits(&budget), "the budget waits for approval");
    assert!(
        !w.ticket().awaits(&item),
        "nothing was asked about the item"
    );
    w.engine
        .approve(&w.ticket, "slack", person("alice"))
        .unwrap();
    assert!(
        !w.ticket().awaits(&budget),
        "an approved budget no longer waits"
    );

    let mut w = World::awaiting_budget();
    w.engine
        .stop_ticket(&w.ticket, "slack", Actor::person("alice"), "stopped")
        .unwrap();
    assert!(!w.ticket().awaits(&budget), "a stopped Ticket asks nothing");

    let mut w = World::new();
    assert!(!w.ticket().awaits(&budget), "no approval was needed");
    assert!(!w.ticket().awaits(&item));
    w.ask(&FOR).unwrap();
    assert!(w.ticket().awaits(&item), "a waiting grant awaits");
    assert!(
        !w.ticket().awaits(&other),
        "another item is not the one asked"
    );
    assert!(
        !w.ticket().awaits(&budget),
        "a secret ask is not the budget ask"
    );
    w.answer(person("carol"), true).unwrap();
    assert!(!w.ticket().awaits(&item), "a granted item no longer awaits");

    let mut w = World::new();
    w.ask(&FOR).unwrap();
    w.answer(person("carol"), false).unwrap();
    assert!(!w.ticket().awaits(&item), "a denied item no longer awaits");

    let mut w = World::new();
    w.ask(&FOR).unwrap();
    w.engine
        .stop_ticket(&w.ticket, "slack", Actor::person("alice"), "stopped")
        .unwrap();
    assert!(!w.ticket().awaits(&item), "an ended Ticket asks nothing");
}

#[test]
fn a_secret_id_is_an_environment_and_a_name() {
    let longest = format!("e/{}", "n".repeat(MAX_SECRET_CHARS - 2));
    let too_long = format!("e/{}", "n".repeat(MAX_SECRET_CHARS - 1));
    for good in [
        "local/github-gui",
        "laptop/open_ai",
        "a/b",
        "Node-1/KEY_2",
        longest.as_str(),
    ] {
        let id = SecretId::try_from(good.to_owned()).unwrap_or_else(|e| panic!("{good}: {e}"));
        assert_eq!(id.as_str(), good);
        assert_eq!(id.to_string(), good);
        assert_eq!(String::from(id), good);
    }
    for bad in [
        "",
        "/",
        "github-gui",
        "local/",
        "/github-gui",
        "a/b/c",
        "local//x",
        "local/git:hub",
        "local:x/y",
        "local/git hub",
        "local/git.hub",
        "local/gïthub",
        "local/new\nline",
        " local/x",
        "local/x ",
        too_long.as_str(),
    ] {
        assert!(
            SecretId::try_from(bad.to_owned()).is_err(),
            "{bad:?} is not a secret id"
        );
    }
    assert_eq!(MAX_SECRET_CHARS, 60);
}

#[test]
fn a_secret_id_is_a_json_string_and_a_map_key() {
    let id = secret();
    assert_eq!(serde_json::to_string(&id).unwrap(), "\"local/github-gui\"");
    assert_eq!(
        serde_json::from_str::<SecretId>("\"local/github-gui\"").unwrap(),
        id
    );
    assert!(serde_json::from_str::<SecretId>("\"github-gui\"").is_err());
    assert!(serde_json::from_str::<SecretId>("42").is_err());

    let grants = BTreeMap::from([
        (
            secret(),
            SecretGrant::Waiting {
                label: "sign in".into(),
                answerable_by: vec!["slack:carol".into()],
                asked_ms: 1,
            },
        ),
        (
            SecretId::try_from("laptop/gh".to_owned()).unwrap(),
            SecretGrant::Denied {
                by: person("carol"),
                at_ms: 5,
            },
        ),
    ]);
    let text = serde_json::to_string(&grants).unwrap();
    assert_eq!(
        text,
        concat!(
            r#"{"laptop/gh":{"state":"denied","by":{"id":"carol","kind":"person"},"at_ms":5},"#,
            r#""local/github-gui":{"state":"waiting","label":"sign in","answerable_by":["slack:carol"],"asked_ms":1}}"#,
        )
    );
    let back: BTreeMap<SecretId, SecretGrant> = serde_json::from_str(&text).unwrap();
    assert_eq!(back, grants);
    assert!(
        serde_json::from_str::<BTreeMap<SecretId, SecretGrant>>(
            r#"{"nope":{"state":"granted","by":{"id":"c","kind":"person"},"at_ms":1}}"#
        )
        .is_err(),
        "a key that is not a secret id is refused"
    );
}

#[test]
fn the_new_shapes_have_the_wire_form_the_chain_and_the_store_keep() {
    let t = TicketId::new("tk_1");
    let a = AssignmentId::new("as_1");
    let event = |event| serde_json::to_value(Event { at_ms: 7, event }).unwrap();
    assert_eq!(
        event(DomainEvent::SecretAsked {
            ticket: t.clone(),
            secret: secret()
        }),
        json!({"at_ms": 7, "type": "secret_asked", "ticket": "tk_1", "secret": "local/github-gui"})
    );
    assert_eq!(
        event(DomainEvent::SecretAnswered {
            ticket: t.clone(),
            secret: secret(),
            by: person("carol"),
            granted: true
        }),
        json!({"at_ms": 7, "type": "secret_answered", "ticket": "tk_1", "secret": "local/github-gui",
               "by": {"id": "carol", "kind": "person"}, "granted": true})
    );
    assert_eq!(
        event(DomainEvent::KeyUsed {
            ticket: t,
            assignment: a,
            secret: secret(),
            method: "GET".into(),
            url: "https://example.com/".into()
        }),
        json!({"at_ms": 7, "type": "key_used", "ticket": "tk_1", "assignment": "as_1",
               "secret": "local/github-gui", "method": "GET", "url": "https://example.com/"})
    );
    assert_eq!(
        serde_json::to_value(InputBody::Secret {
            secret: secret(),
            granted: false,
            by: person("carol")
        })
        .unwrap(),
        json!({"kind": "secret", "secret": "local/github-gui", "granted": false,
               "by": {"id": "carol", "kind": "person"}})
    );
    assert_eq!(
        serde_json::to_value(Ask::Budget).unwrap(),
        json!({"kind": "budget"})
    );
    assert_eq!(
        serde_json::to_value(Ask::Secret { secret: secret() }).unwrap(),
        json!({"kind": "secret", "secret": "local/github-gui"})
    );
}

#[test]
fn a_ticket_with_grants_round_trips_with_them_after_its_approval() {
    let mut w = World::new();
    w.ask(&FOR).unwrap();
    w.answer(person("carol"), true).unwrap();
    let t = w.ticket();

    let text = serde_json::to_string(&t).unwrap();
    assert!(
        text.contains(
            r#""approval":{"state":"not_needed"},"secrets":{"local/github-gui":{"state":"granted","#
        ),
        "{text}"
    );
    assert_eq!(serde_json::from_str::<Ticket>(&text).unwrap(), t);
}

const OLD_TICKET: &str = concat!(
    r#"{"id":"tk_1","title":"Read notifications","origin":"t1","decision":"dc_1","#,
    r#""mandate":{"policy_version":"pv_1","scope":"Read my notifications","strength":"explicit","#,
    r#""rules":[],"requester":{"id":"alice","kind":"person"},"source":"slack","budget_microusd":3000000},"#,
    r#""plan":{"kind":"session"},"#,
    r#""channels":[{"source":"slack","address":{"channel":"C1","thread_ts":"th1"}}],"#,
    r#""threads":["th1"],"#,
    r#""inputs":["#,
    r#"{"id":"in_1","seq":1,"body":{"kind":"trigger","trigger":"t2"},"from_allowed_person":true,"at_ms":1100,"sent_to":"as_1"},"#,
    r#"{"id":"in_2","seq":2,"body":{"kind":"note","text":"CI is green"},"from_allowed_person":false,"at_ms":1200,"sent_to":null},"#,
    r#"{"id":"in_3","seq":3,"body":{"kind":"approved","by":{"id":"alice","kind":"person","name":"Alice"}},"from_allowed_person":true,"at_ms":1300,"sent_to":"as_1"}],"#,
    r#""assignments":[{"id":"as_1","state":"running","created_ms":1050,"reports":["#,
    r#"{"seq":4,"status":"blocked","text":"Which repo?","question":"Which repo?","spent_microusd":120000,"at_ms":1400}]}],"#,
    r#""approval":{"state":"granted","by":{"id":"alice","kind":"person","name":"Alice"},"at_ms":1300},"#,
    r#""attention":"Which repo?","created_ms":1000,"seq":4,"handed_back":["t2"]}"#,
);

const OLD_MESSAGE: &str = concat!(
    r#"{"id":"ms_1","channel":{"source":"slack","address":{"channel":"C1","thread_ts":"th1"}},"#,
    r#""kind":"attention","text":"Read notifications needs approval first: one worker session with a budget of $3.00.","#,
    r#""ticket":"tk_1"}"#,
);

const OLD_SEND: &str = concat!(
    r#"{"effect":"send","id":"ms_1","channel":{"source":"slack","address":{"channel":"C1","thread_ts":"th1"}},"#,
    r#""kind":"attention","text":"Read notifications needs approval first: one worker session with a budget of $3.00.","#,
    r#""ticket":"tk_1"}"#,
);

const OLD_EVENTS: [&str; 4] = [
    r#"{"at_ms":1000,"type":"ticket_created","ticket":"tk_1","decision":"dc_1","plan":{"kind":"session"},"budget_microusd":3000000}"#,
    r#"{"at_ms":1050,"type":"assignment_started","ticket":"tk_1","assignment":"as_1"}"#,
    r#"{"at_ms":1300,"type":"ticket_approved","ticket":"tk_1","by":{"id":"alice","kind":"person","name":"Alice"}}"#,
    r#"{"at_ms":1400,"type":"reported","ticket":"tk_1","assignment":"as_1","status":"blocked"}"#,
];

#[test]
fn a_ticket_stored_before_secrets_reads_back_and_writes_back_unchanged() {
    let ticket: Ticket = serde_json::from_str(OLD_TICKET).unwrap();
    assert!(ticket.secrets.is_empty());
    assert_eq!(ticket.attention.as_deref(), Some("Which repo?"));
    assert_eq!(ticket.inputs.len(), 3);
    assert_eq!(serde_json::to_string(&ticket).unwrap(), OLD_TICKET);
}

#[test]
fn a_message_stored_before_asks_reads_back_and_writes_back_unchanged() {
    let message: OutboundMessage = serde_json::from_str(OLD_MESSAGE).unwrap();
    assert_eq!(message.ask, None);
    assert_eq!(serde_json::to_string(&message).unwrap(), OLD_MESSAGE);

    let effect: Effect = serde_json::from_str(OLD_SEND).unwrap();
    let Effect::Send(sent) = &effect else {
        panic!("{effect:?}")
    };
    assert_eq!(sent.ask, None);
    assert_eq!(serde_json::to_string(&effect).unwrap(), OLD_SEND);
}

#[test]
fn events_written_before_secrets_read_back_and_write_back_unchanged() {
    for old in OLD_EVENTS {
        let event: Event = serde_json::from_str(old).unwrap();
        assert_eq!(serde_json::to_string(&event).unwrap(), old);
    }
}
