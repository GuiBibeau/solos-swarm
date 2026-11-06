//! A pull request is owed, not done: `ready` is admitted or refused by the
//! host's gate, an admitted head is one record plus one Effect, and the
//! Ticket ends only when the host reports the pull request it read back.

use solos_agent_core::change::{
    BranchName, Change, ChangeState, Checks, CommitSha, Deliverable, ForgeTerms, Harvest,
    MergeMode, PrRef, RepoName, Withheld,
};
use solos_agent_core::delivery::{Admission, ForgeJob, NotReady, Observed, ready_gate};
use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, AssignmentId, ChannelRef, DomainEvent, Effect,
    MessageKind, Method, Plan, ReportIn, ReportStatus, Stage, ThreadKey, Ticket, TicketId,
    TicketOutcome, Trigger, TriggerId, Verdict,
};
use solos_agent_core::{AllowlistPolicy, Engine, Error, MemoryRepositories, Repositories};
use std::collections::BTreeMap;

type TestEngine = Engine<MemoryRepositories, AllowlistPolicy, Box<dyn Fn() -> u64>>;

struct World {
    engine: TestEngine,
}

impl World {
    fn new() -> Self {
        let policy = AllowlistPolicy {
            people: ["discord:alice".to_owned(), "github:alice".to_owned()].into(),
            approvers: ["discord:alice".to_owned()].into(),
            ..AllowlistPolicy::default()
        };
        let clock = Box::new(|| 1_000u64) as Box<dyn Fn() -> u64>;
        Self {
            engine: Engine::new(MemoryRepositories::default(), policy, clock),
        }
    }
    fn effects(&mut self) -> Vec<Effect> {
        self.engine.repos_mut().take_effects()
    }
    fn ticket(&self, id: &TicketId) -> Ticket {
        self.engine.repos().ticket(id).unwrap()
    }
    fn events(&self) -> Vec<DomainEvent> {
        self.engine
            .repos()
            .events
            .iter()
            .map(|e| e.event.clone())
            .collect()
    }
}

fn widgets() -> RepoName {
    RepoName::try_from("acme/widgets".to_owned()).unwrap()
}

fn sha(digit: char) -> CommitSha {
    CommitSha::try_from(digit.to_string().repeat(40)).unwrap()
}

fn terms() -> ForgeTerms {
    ForgeTerms {
        merge: MergeMode::Ask,
        ci_expected: true,
        max_pushes: 2,
        max_repairs: 2,
    }
}

fn harvest(digit: char) -> Harvest {
    Harvest {
        head: sha(digit),
        base: "main".into(),
        commits: 2,
        files: 3,
        bytes: 1_234,
    }
}

fn session(deliverable: Deliverable) -> Plan {
    Plan::Session {
        harness: None,
        environment: None,
        method: Some(Method { deliverable }),
    }
}

fn trigger(id: &str, thread: &str, text: &str) -> Trigger {
    Trigger {
        id: TriggerId::new(id),
        source: "discord".into(),
        at_ms: 1_000,
        actor: Actor::person("alice"),
        addressed: Addressed::Mention,
        text: text.into(),
        threads: vec![ThreadKey::new(thread)],
        reply_to: vec![ChannelRef {
            source: "discord".into(),
            address: BTreeMap::from([("channel".to_owned(), thread.to_owned())]),
        }],
        meta: serde_json::Value::Null,
    }
}

/// A Ticket with a running worker that owes `deliverable` on acme/widgets.
fn working(w: &mut World, deliverable: Deliverable) -> (TicketId, AssignmentId) {
    w.engine
        .receive(trigger("t1", "th1", "add --json to status"))
        .unwrap();
    let eval = w.engine.evaluate_due().unwrap().remove(0);
    let owed = deliverable > Deliverable::Answer;
    w.engine
        .decide(
            &eval,
            Verdict::Accept(Accept {
                title: "Add --json to status!".into(),
                scope: "Add a --json flag to agent status".into(),
                reason: "alice asked".into(),
                plan: session(deliverable),
                budget_microusd: Some(100_000),
                acknowledge: Acknowledge::Standard,
                rules: Vec::new(),
                repo: Some(widgets()),
                forge: owed.then(terms),
            }),
        )
        .unwrap();
    let launch = w
        .effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Launch {
                ticket, assignment, ..
            } => Some((ticket, assignment)),
            _ => None,
        })
        .unwrap();
    w.engine.assignment_started(&launch.0, &launch.1).unwrap();
    w.effects();
    launch
}

fn ready(text: &str, harvest: Option<Harvest>) -> ReportIn {
    ReportIn {
        status: ReportStatus::Ready,
        text: text.into(),
        question: None,
        spent_microusd: 0,
        harvest,
    }
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

fn published(ticket: &TicketId, head: CommitSha, number: u64) -> Observed {
    Observed::Published {
        ticket: ticket.clone(),
        head,
        pr: PrRef {
            repo: widgets(),
            number,
        },
        url: format!("https://github.com/acme/widgets/pull/{number}"),
        thread: ThreadKey::new(format!("github:acme/widgets#{number}")),
        reply_to: ChannelRef {
            source: "github".into(),
            address: BTreeMap::from([
                ("repo".to_owned(), "acme/widgets".to_owned()),
                ("number".to_owned(), number.to_string()),
            ]),
        },
        created: true,
    }
}

// ---- names ----

#[test]
fn a_commit_id_is_forty_or_sixty_four_lowercase_hex_digits() {
    for good in ["a".repeat(40), "0123456789abcdef".repeat(4)] {
        assert_eq!(CommitSha::try_from(good.clone()).unwrap().as_str(), good);
    }
    for bad in [
        String::new(),
        "a".repeat(39),
        "a".repeat(41),
        "A".repeat(40),
        "g".repeat(40),
        format!("{} ", "a".repeat(39)),
        format!("--upload-pack={}", "a".repeat(30)),
    ] {
        assert!(CommitSha::try_from(bad.clone()).is_err(), "{bad:?}");
    }
    assert_eq!(sha('c').short(), "cccccccccccc");
    let parsed: Result<CommitSha, _> = serde_json::from_str("\"HEAD\"");
    assert!(parsed.is_err());
}

#[test]
fn a_branch_is_the_hosts_name_for_one_ticket_and_only_that_shape_parses() {
    let ticket = TicketId::new("tk_9f2a0b1c2d3e4f50");
    let branch = BranchName::for_ticket(&ticket, "Fix the flaky retry test!");
    assert_eq!(
        branch.as_str(),
        "solos/tk_9f2a0b1c2d3e4f50/fix-the-flaky-retry-test"
    );
    assert_eq!(BranchName::parse(branch.as_str()), Some(branch.clone()));
    assert_eq!(branch.ticket(), ticket);

    // The title is the model's only word in it, and it passes through `slug`.
    for (title, slug) in [
        ("  --upload-pack=x ; rm -rf /  ", "upload-pack-x-rm-rf"),
        ("../../etc/passwd", "etc-passwd"),
        ("Ünïcode only", "n-code-only"),
        ("日本語", "change"),
        ("", "change"),
        ("a   b\t\nc", "a-b-c"),
    ] {
        assert_eq!(BranchName::slug(title), slug, "{title:?}");
    }
    let long = BranchName::slug(&"word ".repeat(30));
    assert!(long.len() <= 40 && !long.ends_with('-'), "{long}");

    for bad in [
        "",
        "main",
        "solos",
        "solos/",
        "solos/tk_1",
        "solos/tk_1/",
        "solos//slug",
        "solos/tk_1/../x",
        "solos/x/../y",
        "solos/tk_1/Upper",
        "solos/tk_1/has space",
        "solos/tk_1/a--b",
        "solos/tk_1/-lead",
        "solos/tk_1/trail-",
        "solos/tk_1/a/b",
        "other/tk_1/slug",
        "solos/tk 1/slug",
        "refs/heads/solos/tk_1/slug",
    ] {
        assert!(BranchName::parse(bad).is_none(), "{bad:?}");
    }
    assert!(BranchName::parse(&format!("solos/tk_1/{}", "a".repeat(41))).is_none());
    let parsed: Result<BranchName, _> = serde_json::from_str("\"feature/x\"");
    assert!(parsed.is_err());
}

// ---- the ready gate ----

fn held_change(deliverable: Deliverable) -> Ticket {
    let mut w = World::new();
    let (ticket, _) = working(&mut w, deliverable);
    w.ticket(&ticket)
}

fn with_change(mut ticket: Ticket, edit: impl FnOnce(&mut Change)) -> Ticket {
    let branch = BranchName::for_ticket(&ticket.id, &ticket.title);
    let mut change = Change::first(branch, &harvest('a'), "text".into());
    change.delivered = Some(solos_agent_core::change::Delivered {
        pr: PrRef {
            repo: widgets(),
            number: 7,
        },
        head: sha('a'),
        url: "https://github.com/acme/widgets/pull/7".into(),
        at_ms: 5,
    });
    edit(&mut change);
    ticket.change = Some(change);
    ticket
}

#[test]
fn the_gate_admits_what_a_ticket_owes_and_refuses_first_failure_first() {
    let terms = terms();
    let answer = held_change(Deliverable::Answer);
    let change = held_change(Deliverable::Change);
    let pull_request = held_change(Deliverable::PullRequest);
    let some = harvest('b');

    // An answer needs no harvest, and a Mandate with no terms owes one.
    assert_eq!(
        ready_gate(&answer, None, None),
        Ok(Admission::Answer),
        "a ticket owing an answer"
    );
    assert_eq!(
        ready_gate(&pull_request, None, None),
        Ok(Admission::Answer),
        "a ticket accepted before pull requests were delivered"
    );

    // A change must be committed: the harvest is the host's admission.
    assert_eq!(
        ready_gate(&change, Some(&terms), Some(&some)),
        Ok(Admission::Committed(some.clone()))
    );
    assert_eq!(
        ready_gate(&change, Some(&terms), None),
        Err(NotReady::NothingToDeliver)
    );

    // A pull request: a new head is proposed, nothing to propose is refused.
    assert_eq!(
        ready_gate(&pull_request, Some(&terms), Some(&some)),
        Ok(Admission::Propose(some.clone()))
    );
    assert_eq!(
        ready_gate(&pull_request, Some(&terms), None),
        Err(NotReady::NothingToDeliver)
    );

    // One row per refusal, each alone.
    let delivered = with_change(pull_request.clone(), |_| {});
    assert_eq!(
        ready_gate(&delivered, Some(&terms), Some(&some)),
        Ok(Admission::Propose(some.clone())),
        "a second head after a delivery"
    );
    assert_eq!(
        ready_gate(&delivered, Some(&terms), Some(&harvest('a'))),
        Ok(Admission::Reply),
        "the same head again is an answer, never a second proposal"
    );
    let in_flight = with_change(pull_request.clone(), |change| change.delivered = None);
    assert_eq!(
        ready_gate(&in_flight, Some(&terms), Some(&some)),
        Err(NotReady::DeliveryInFlight { head: sha('a') })
    );
    let held = with_change(pull_request.clone(), |change| {
        change.held = Some(Withheld::PrClosed);
    });
    assert_eq!(
        ready_gate(&held, Some(&terms), Some(&some)),
        Err(NotReady::Held {
            why: Withheld::PrClosed
        })
    );
    let merged = with_change(pull_request.clone(), |change| {
        change.state = ChangeState::Closed { at_ms: 9 };
    });
    assert_eq!(
        ready_gate(&merged, Some(&terms), Some(&some)),
        Err(NotReady::Finished)
    );
    let spent = with_change(pull_request, |change| change.pushes = 2);
    assert_eq!(
        ready_gate(&spent, Some(&terms), Some(&some)),
        Err(NotReady::PushLimit { max: 2 })
    );
    assert_eq!(
        ready_gate(&spent, Some(&terms), Some(&harvest('a'))),
        Ok(Admission::Reply),
        "an answer needs no push"
    );

    // Each refusal says what to do next.
    for (why, said) in [
        (NotReady::NothingToDeliver, "Commit your change"),
        (
            NotReady::DeliveryInFlight { head: sha('a') },
            "Report waiting",
        ),
        (
            NotReady::Held {
                why: Withheld::ReadOnly,
            },
            "Report blocked",
        ),
        (NotReady::PushLimit { max: 6 }, "limit of 6"),
        (NotReady::Finished, "Report dropped"),
    ] {
        assert!(why.tell().contains(said), "{}", why.tell());
    }
}

// ---- the engine ----

#[test]
fn an_admitted_ready_is_one_record_one_effect_and_a_ticket_that_is_not_done() {
    let mut w = World::new();
    let (ticket, assignment) = working(&mut w, Deliverable::PullRequest);

    w.engine
        .report(
            &ticket,
            &assignment,
            ready("Added --json.", Some(harvest('a'))),
        )
        .unwrap();

    let effects = w.effects();
    let jobs: Vec<&ForgeJob> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::Forge(job) => Some(job),
            _ => None,
        })
        .collect();
    assert_eq!(
        jobs,
        [&ForgeJob::Publish {
            ticket: ticket.clone(),
            head: sha('a')
        }]
    );
    assert!(
        sent(&effects).is_empty(),
        "nothing is told until the pull request exists: {effects:?}"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Stop { assignment: a, .. } if *a == assignment)),
        "the worker's Assignment ends"
    );
    let t = w.ticket(&ticket);
    assert_eq!(t.ended, None, "the Ticket stays open");
    assert_eq!(t.stage(), Stage::Delivering);
    assert!(!t.has_live_assignment());
    let change = t.change.unwrap();
    assert_eq!(
        change.branch.as_str(),
        format!("solos/{ticket}/add-json-to-status")
    );
    assert_eq!(
        (
            change.base.as_str(),
            &change.head,
            change.pushes,
            change.text.as_str()
        ),
        ("main", &sha('a'), 1, "Added --json.")
    );
    assert_eq!(change.delivered, None);
    assert_eq!(change.checks, Checks::Pending { reported: 0 });
    let admitted = w.events().into_iter().find_map(|event| match event {
        DomainEvent::ProposalAdmitted {
            commits,
            files,
            bytes,
            round,
            ..
        } => Some((commits, files, bytes, round)),
        _ => None,
    });
    assert_eq!(admitted, Some((2, 3, 1_234, 1)));
}

#[test]
fn a_refused_ready_commits_nothing_and_a_ready_with_nothing_to_propose_is_refused() {
    let mut w = World::new();
    let (ticket, assignment) = working(&mut w, Deliverable::PullRequest);
    let before = (w.ticket(&ticket), w.events().len());

    let err = w
        .engine
        .report(&ticket, &assignment, ready("done", None))
        .unwrap_err();

    assert_eq!(err, Error::Refused(NotReady::NothingToDeliver.tell()));
    assert_eq!((w.ticket(&ticket), w.events().len()), before);
    assert!(w.effects().is_empty());

    // The same worker may still report anything else, and answer later.
    w.engine
        .report(
            &ticket,
            &assignment,
            ReportIn {
                status: ReportStatus::Waiting,
                text: "thinking".into(),
                question: None,
                spent_microusd: 0,
                harvest: None,
            },
        )
        .unwrap();
    w.engine
        .report(&ticket, &assignment, ready("now", Some(harvest('a'))))
        .unwrap();
}

#[test]
fn a_change_ticket_ends_on_a_committed_head_and_says_so_in_a_host_line() {
    let mut w = World::new();
    let (ticket, assignment) = working(&mut w, Deliverable::Change);

    let err = w
        .engine
        .report(&ticket, &assignment, ready("done", None))
        .unwrap_err();
    assert_eq!(err, Error::Refused(NotReady::NothingToDeliver.tell()));

    w.engine
        .report(
            &ticket,
            &assignment,
            ready("Renamed it.", Some(harvest('a'))),
        )
        .unwrap();

    let effects = w.effects();
    assert!(!effects.iter().any(|e| matches!(e, Effect::Forge(_))));
    let told = sent(&effects);
    assert_eq!(
        told,
        [(
            MessageKind::Result,
            format!(
                "Renamed it.\n\nCommitted {} (3 files) in the ticket's clone; not published.",
                sha('a').short()
            )
        )]
    );
    let t = w.ticket(&ticket);
    assert_eq!(
        t.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    assert_eq!(
        t.change, None,
        "nothing is delivered, so there is no Change"
    );
}

#[test]
fn the_pull_request_ends_the_ticket_tells_who_asked_and_moves_the_reply_channel() {
    let mut w = World::new();
    let (ticket, assignment) = working(&mut w, Deliverable::PullRequest);
    w.engine
        .report(
            &ticket,
            &assignment,
            ready("Added --json.", Some(harvest('a'))),
        )
        .unwrap();
    w.effects();

    w.engine.forge(published(&ticket, sha('a'), 75)).unwrap();

    let effects = w.effects();
    let told = sent(&effects);
    assert_eq!(
        told,
        [(
            MessageKind::Result,
            format!(
                "Added --json.\n\nPR https://github.com/acme/widgets/pull/75 · head {} · opened",
                sha('a').short()
            )
        )]
    );
    let Some(Effect::Send(message)) = effects.iter().find(|e| matches!(e, Effect::Send(_))) else {
        panic!()
    };
    assert_eq!(
        message.channel.source, "discord",
        "the origin hears it first"
    );
    let t = w.ticket(&ticket);
    assert_eq!(
        t.ended.as_ref().map(|e| (e.outcome, e.reason.as_str())),
        Some((TicketOutcome::Completed, "ready"))
    );
    let sources: Vec<&str> = t.channels.iter().map(|c| c.source.as_str()).collect();
    assert_eq!(
        sources,
        ["discord", "github"],
        "a chat origin keeps hearing"
    );
    let delivered = t.change.as_ref().unwrap().delivered.clone().unwrap();
    assert_eq!(
        (delivered.pr.number, delivered.head.clone()),
        (75, sha('a'))
    );
    let conversation = ThreadKey::new("github:acme/widgets#75");
    assert!(t.threads.contains(&conversation));
    assert!(
        w.engine
            .repos()
            .thread(&conversation)
            .unwrap()
            .subscriptions
            .contains(&ticket),
        "a listed person's comment on the pull request reaches the Ticket"
    );

    // The same fact again changes nothing and appends no event.
    let events = w.events().len();
    w.engine.forge(published(&ticket, sha('a'), 75)).unwrap();
    assert_eq!(w.events().len(), events);
    assert!(w.effects().is_empty());
    let delivered_events = w
        .events()
        .into_iter()
        .filter(|e| matches!(e, DomainEvent::Delivered { .. }))
        .count();
    assert_eq!(delivered_events, 1);
}

#[test]
fn a_github_origin_is_replaced_by_the_pull_request_after_the_first_result() {
    let mut w = World::new();
    let (ticket, assignment) = working(&mut w, Deliverable::PullRequest);
    // The origin is an issue on GitHub: the same Source as the pull request.
    let mut t = w.ticket(&ticket);
    t.channels = vec![ChannelRef {
        source: "github".into(),
        address: BTreeMap::from([
            ("repo".to_owned(), "acme/widgets".to_owned()),
            ("number".to_owned(), "12".to_owned()),
        ]),
    }];
    w.engine.repos_mut().tickets.insert(ticket.clone(), t);
    w.engine
        .report(&ticket, &assignment, ready("Fixed.", Some(harvest('a'))))
        .unwrap();
    w.effects();

    w.engine.forge(published(&ticket, sha('a'), 13)).unwrap();

    let effects = w.effects();
    let Some(Effect::Send(message)) = effects.iter().find(|e| matches!(e, Effect::Send(_))) else {
        panic!()
    };
    assert_eq!(
        message.channel.address["number"], "12",
        "the issue hears it"
    );
    let t = w.ticket(&ticket);
    let numbers: Vec<&str> = t
        .channels
        .iter()
        .map(|c| c.address["number"].as_str())
        .collect();
    assert_eq!(
        numbers,
        ["13"],
        "then the pull request supersedes the issue"
    );
}

#[test]
fn a_published_head_that_a_newer_proposal_replaced_changes_nothing() {
    let mut w = World::new();
    let (ticket, assignment) = working(&mut w, Deliverable::PullRequest);
    w.engine
        .report(&ticket, &assignment, ready("v1", Some(harvest('a'))))
        .unwrap();
    w.effects();
    let events = w.events().len();

    // The forge holds a head the Store no longer wants; and one it never saw.
    let mut stale = w.ticket(&ticket);
    stale.change.as_mut().unwrap().head = sha('b');
    w.engine.repos_mut().tickets.insert(ticket.clone(), stale);
    w.engine.forge(published(&ticket, sha('a'), 75)).unwrap();
    w.engine
        .forge(Observed::Withheld {
            ticket: ticket.clone(),
            head: sha('a'),
            why: Withheld::PrClosed,
        })
        .unwrap();

    assert_eq!(w.events().len(), events);
    assert!(w.effects().is_empty());
    let t = w.ticket(&ticket);
    assert_eq!(t.change.unwrap().delivered, None);
    assert_eq!(t.ended, None);

    // A ticket the Store does not have is an error, not a panic.
    let err = w
        .engine
        .forge(published(&TicketId::new("tk_missing"), sha('a'), 1))
        .unwrap_err();
    assert!(matches!(err, Error::Missing { .. }), "{err:?}");
}

#[test]
fn a_forge_that_says_no_holds_the_change_and_asks_a_person_once() {
    let mut w = World::new();
    let (ticket, assignment) = working(&mut w, Deliverable::PullRequest);
    w.engine
        .report(&ticket, &assignment, ready("v1", Some(harvest('a'))))
        .unwrap();
    w.effects();
    let why = Withheld::BranchMoved { found: sha('f') };

    w.engine
        .forge(Observed::Withheld {
            ticket: ticket.clone(),
            head: sha('a'),
            why: why.clone(),
        })
        .unwrap();

    let effects = w.effects();
    let told = sent(&effects);
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0].0, MessageKind::Attention);
    assert!(
        told[0].1.contains("someone else pushed ffffffffffff")
            && told[0].1.contains("never overwrite"),
        "{}",
        told[0].1
    );
    let t = w.ticket(&ticket);
    assert_eq!(t.ended, None, "kept open for a person");
    assert_eq!(t.stage(), Stage::Answer);
    assert_eq!(t.change.as_ref().unwrap().held, Some(why.clone()));
    assert!(w.events().contains(&DomainEvent::Withheld {
        ticket: ticket.clone(),
        head: sha('a'),
        why: why.clone(),
    }));

    // Told once, however often the runtime repeats it.
    w.engine
        .forge(Observed::Withheld {
            ticket: ticket.clone(),
            head: sha('a'),
            why,
        })
        .unwrap();
    assert!(w.effects().is_empty());

    // A worker that comes back is refused until a person decides.
    let err = w
        .engine
        .report(&ticket, &assignment, ready("again", Some(harvest('c'))))
        .unwrap_err();
    assert!(
        matches!(&err, Error::Refused(why) if why.contains("Report blocked")),
        "{err:?}"
    );
}

#[test]
fn a_follow_up_during_delivery_waits_for_the_pull_request_then_resumes_the_ticket() {
    let mut w = World::new();
    let (ticket, assignment) = working(&mut w, Deliverable::PullRequest);
    w.engine
        .report(&ticket, &assignment, ready("v1", Some(harvest('a'))))
        .unwrap();
    w.effects();

    // alice adds a request on the same thread while the host publishes.
    w.engine
        .receive(trigger("t2", "th1", "also add --pretty"))
        .unwrap();

    assert!(
        !w.effects()
            .iter()
            .any(|e| matches!(e, Effect::Launch { .. })),
        "no second worker races the delivery"
    );
    assert_eq!(w.ticket(&ticket).stage(), Stage::Delivering);

    w.engine.forge(published(&ticket, sha('a'), 75)).unwrap();

    let effects = w.effects();
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Launch { recovery: true, .. })),
        "her request was kept, and the Ticket takes it up now: {effects:?}"
    );
    let t = w.ticket(&ticket);
    assert_eq!(t.ended, None);
    assert!(
        sent(&effects)
            .iter()
            .any(|(kind, text)| *kind == MessageKind::Result && text.contains("opened")),
        "the pull request is still told"
    );
}

#[test]
fn a_ticket_stopped_while_its_push_was_in_flight_records_the_pull_request_and_tells_nobody() {
    let mut w = World::new();
    let (ticket, assignment) = working(&mut w, Deliverable::PullRequest);
    w.engine
        .report(&ticket, &assignment, ready("v1", Some(harvest('a'))))
        .unwrap();
    w.effects();
    w.engine
        .stop_ticket(&ticket, "discord", Actor::person("alice"), "stopped")
        .unwrap();
    w.effects();

    w.engine.forge(published(&ticket, sha('a'), 75)).unwrap();

    assert!(sent(&w.effects()).is_empty());
    let t = w.ticket(&ticket);
    assert!(t.stopped_by_person());
    assert!(
        t.change.unwrap().delivered.is_some(),
        "the forge's fact is kept"
    );
}

#[test]
fn a_second_head_is_a_new_proposal_and_the_result_says_updated() {
    let mut w = World::new();
    let (ticket, assignment) = working(&mut w, Deliverable::PullRequest);
    w.engine
        .report(&ticket, &assignment, ready("v1", Some(harvest('a'))))
        .unwrap();
    w.engine.forge(published(&ticket, sha('a'), 75)).unwrap();
    w.effects();

    // A listed person's comment on the pull request resumes the ended Ticket.
    let mut comment = trigger("c1", "github:acme/widgets#75", "use a flag, not a command");
    comment.source = "github".into();
    w.engine.receive(comment).unwrap();
    let second = w
        .effects()
        .into_iter()
        .find_map(|e| match e {
            Effect::Launch { assignment, .. } => Some(assignment),
            _ => None,
        })
        .expect("the comment resumes the Ticket");
    w.engine.assignment_started(&ticket, &second).unwrap();
    w.effects();
    assert_eq!(w.ticket(&ticket).stage(), Stage::Working);

    // Its worker answers without a commit: the text is the reply.
    w.engine
        .report(
            &ticket,
            &second,
            ready("It is a flag already.", Some(harvest('a'))),
        )
        .unwrap();
    let told = sent(&w.effects());
    assert_eq!(told.len(), 2, "one Result to each channel: {told:?}");
    assert!(
        told[0].1.ends_with(&format!(
            "PR https://github.com/acme/widgets/pull/75 · head {} · unchanged",
            sha('a').short()
        )),
        "{}",
        told[0].1
    );
    assert_eq!(
        w.ticket(&ticket).change.unwrap().pushes,
        1,
        "an answer is no push"
    );

    // A new comment, a new commit: a second head, delivered to the same PR.
    let mut again = trigger("c2", "github:acme/widgets#75", "rename it to --format");
    again.source = "github".into();
    w.engine.receive(again).unwrap();
    let third = w
        .effects()
        .into_iter()
        .find_map(|e| match e {
            Effect::Launch { assignment, .. } => Some(assignment),
            _ => None,
        })
        .unwrap();
    w.engine.assignment_started(&ticket, &third).unwrap();
    w.effects();
    w.engine
        .report(&ticket, &third, ready("Renamed.", Some(harvest('b'))))
        .unwrap();
    let jobs: Vec<_> = w
        .effects()
        .into_iter()
        .filter_map(|e| match e {
            Effect::Forge(job) => Some(job),
            _ => None,
        })
        .collect();
    assert_eq!(
        jobs,
        [ForgeJob::Publish {
            ticket: ticket.clone(),
            head: sha('b')
        }]
    );
    let change = w.ticket(&ticket).change.unwrap();
    assert_eq!((change.pushes, change.head.clone()), (2, sha('b')));
    assert_eq!(
        change.delivered.as_ref().map(|d| d.head.clone()),
        Some(sha('a')),
        "the forge still holds the first head: a delivery is owed"
    );

    w.engine.forge(published(&ticket, sha('b'), 75)).unwrap();

    let told = sent(&w.effects());
    assert!(
        told.iter().all(|(_, text)| text.ends_with(&format!(
            "PR https://github.com/acme/widgets/pull/75 · head {} · updated",
            sha('b').short()
        ))),
        "{told:?}"
    );

    // The limit holds: a third head is refused.
    let mut more = trigger("c3", "github:acme/widgets#75", "and one more");
    more.source = "github".into();
    w.engine.receive(more).unwrap();
    let fourth = w
        .effects()
        .into_iter()
        .find_map(|e| match e {
            Effect::Launch { assignment, .. } => Some(assignment),
            _ => None,
        })
        .unwrap();
    w.engine.assignment_started(&ticket, &fourth).unwrap();
    let err = w
        .engine
        .report(&ticket, &fourth, ready("x", Some(harvest('c'))))
        .unwrap_err();
    assert_eq!(err, Error::Refused(NotReady::PushLimit { max: 2 }.tell()));
}

#[test]
fn a_ticket_stored_before_delivery_reads_back_and_a_change_round_trips() {
    let stored = serde_json::json!({
        "id": "tk_1", "title": "t", "origin": "t1", "decision": "dc_1",
        "mandate": {"policy_version":"p1","scope":"s","strength":"explicit","rules":[],
                    "requester":{"id":"alice","kind":"person"},"source":"slack",
                    "budget_microusd":500000,"repo":"acme/widgets"},
        "plan": {"kind":"session","method":{"deliverable":"pull_request"}},
        "channels": [], "threads": [], "approval": {"state":"not_needed"},
        "created_ms": 1, "seq": 0
    });
    let ticket: Ticket = serde_json::from_value(stored.clone()).unwrap();
    assert_eq!(ticket.change, None);
    assert_eq!(ticket.mandate.forge, None);
    assert_eq!(
        ready_gate(&ticket, ticket.mandate.forge.as_ref(), None),
        Ok(Admission::Answer),
        "an old Ticket owing a pull request ends as it always did"
    );
    let written = serde_json::to_value(&ticket).unwrap();
    assert!(written.get("change").is_none() && written["mandate"].get("forge").is_none());

    let with = with_change(ticket, |_| {});
    let json = serde_json::to_string(&with).unwrap();
    let back: Ticket = serde_json::from_str(&json).unwrap();
    assert_eq!(back, with);
}
