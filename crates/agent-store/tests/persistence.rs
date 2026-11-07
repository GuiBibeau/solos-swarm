mod common;

use common::{
    Clock, MemEngine, SqlEngine, TempDb, accept, clock, first_difference, memory_contents, policy,
    port_view, slack, sqlite_contents,
};
use solos_agent_core::model::{
    Actor, AssignmentId, EvalId, Plan, ReportIn, ReportStatus, Stage, TicketId, Verdict,
};
use solos_agent_core::trail::{TrailAction, TrailOutcome};
use solos_agent_core::{AllowlistPolicy, Engine, Error, Intake, MemoryRepositories, Repositories};
use solos_agent_store::SqliteStore;
use std::cell::Cell;
use std::rc::Rc;

type AnyEngine<R> = Engine<R, AllowlistPolicy, Clock>;

fn one_eval<R: Repositories>(e: &mut AnyEngine<R>) -> EvalId {
    let started = e.evaluate_due().unwrap();
    assert_eq!(started.len(), 1, "one eval starts: {started:?}");
    started[0].clone()
}

fn ticket_of<R: Repositories>(e: &mut AnyEngine<R>, eval: &EvalId, verdict: Verdict) -> TicketId {
    let decision = e.decide(eval, verdict).unwrap();
    e.repos().decision(&decision).unwrap().ticket.unwrap()
}

fn latest<R: Repositories>(e: &AnyEngine<R>, ticket: &TicketId) -> AssignmentId {
    let ticket = e.repos().ticket(ticket).unwrap();
    ticket.assignments.last().unwrap().id.clone()
}

fn report(status: ReportStatus, question: Option<&str>) -> ReportIn {
    ReportIn {
        status,
        text: format!("{status:?}"),
        question: question.map(str::to_owned),
        spent_microusd: 100_000,
        harvest: None,
    }
}

/// Touches every table: a Ticket at work with a question and a pass-through
/// follow-up, a second Ticket that waits for approval and is then stopped, a
/// gate rejection, and an Eval left running. Returns that Eval.
fn script<R: Repositories>(e: &mut AnyEngine<R>, now: &Cell<u64>) -> EvalId {
    let alice = || Actor::person("alice");
    e.receive(slack("t1", alice(), "th1", "@agent fix the flake"))
        .unwrap();
    let eval = one_eval(e);
    let session = Plan::Session {
        harness: None,
        environment: None,
        method: None,
    };
    let first = ticket_of(e, &eval, accept("Fix the flake", session, 1_000_000));
    let assignment = latest(e, &first);
    e.assignment_started(&first, &assignment).unwrap();
    e.report(&first, &assignment, report(ReportStatus::Progress, None))
        .unwrap();
    let open = TrailAction::Open {
        url: "https://github.com/login".into(),
        key: None,
    };
    let n = e.trail_ask(&first, &assignment, open).unwrap();
    let lost = TrailOutcome::Lost {
        why: "the node went away".into(),
    };
    e.trail_done(&first, &assignment, n, lost).unwrap();
    let read = TrailAction::Read {
        part: None,
        element: None,
    };
    e.trail_ask(&first, &assignment, read).unwrap();
    let question = report(ReportStatus::Waiting, Some("which branch?"));
    e.report(&first, &assignment, question).unwrap();
    now.set(now.get() + 5_000);

    e.receive(slack("t2", alice(), "th2", "@agent survey the field"))
        .unwrap();
    let eval = one_eval(e);
    let swarm = Plan::Swarm {
        agents: 4,
        goal_template: None,
    };
    let second = ticket_of(e, &eval, accept("Survey", swarm, 8_000_000));
    assert_eq!(e.repos().ticket(&second).unwrap().stage(), Stage::Approval);
    e.approve(&second, "slack", alice()).unwrap();
    let follow_up = e.receive(slack("t3", alice(), "th1", "main, please"));
    assert_eq!(
        follow_up.unwrap(),
        Intake::Stored {
            reached: vec![first],
            queued: false
        }
    );
    e.receive(slack(
        "t4",
        Actor::person("mallory"),
        "th3",
        "@agent rm -rf",
    ))
    .unwrap();
    assert!(e.evaluate_due().unwrap().is_empty(), "the gate rejects it");
    e.stop_ticket(&second, "slack", alice(), "not needed")
        .unwrap();
    now.set(now.get() + 5_000);
    e.receive(slack("t5", alice(), "th3", "@agent look at this"))
        .unwrap();
    one_eval(e)
}

#[test]
fn state_survives_closing_and_reopening_the_file() {
    let db = TempDb::new("persist");
    let (sql_now, mem_now) = (Rc::new(Cell::new(1_000)), Rc::new(Cell::new(1_000)));
    let store = SqliteStore::open(&db.path).unwrap();
    let mut sql: SqlEngine = Engine::new(store, policy(), clock(&sql_now));
    let mut mem: MemEngine = Engine::new(MemoryRepositories::default(), policy(), clock(&mem_now));
    let running = script(&mut sql, &sql_now);
    assert_eq!(script(&mut mem, &mem_now), running);

    let before = sqlite_contents(sql.repos());
    let expected = memory_contents(mem.repos());
    assert_eq!(first_difference(&expected, &before), None);
    assert_eq!(
        (
            before.triggers.len(),
            before.threads.len(),
            before.tickets.len()
        ),
        (5, 3, 2)
    );
    assert_eq!(
        before.decisions.len(),
        3,
        "two accepts and a gate rejection"
    );
    assert!(!before.effects.is_empty() && !before.events.is_empty());
    assert_eq!(
        before.trails.iter().map(|c| c.next).collect::<Vec<_>>(),
        [3],
        "one Assignment asked two steps"
    );
    assert!(before.trails[0].pending.is_some(), "the second still waits");
    let view = port_view(sql.repos(), &before.threads);
    assert!(view.contains("th3"), "{view}");
    let queue = sql.repos().effects().unwrap();
    let open = sql.repos().open_tickets().unwrap();
    assert_eq!(open.len(), 1);
    let chain = sql.repos().verify_chain().unwrap();
    assert_eq!(chain.len, before.events.len() as u64);
    drop(sql);

    let store = SqliteStore::open(&db.path).unwrap();
    let after = sqlite_contents(&store);
    assert_eq!(first_difference(&before, &after), None);
    assert_eq!(after, before);
    assert_eq!(port_view(&store, &before.threads), view);
    assert_eq!(store.effects().unwrap(), queue);
    assert_eq!(store.open_tickets().unwrap(), open);
    assert_eq!(store.verify_chain().unwrap(), chain);
    assert_eq!(store.schema_version().unwrap(), 9);
    for t in &before.triggers {
        assert_eq!(store.trigger(&t.id).as_ref(), Some(t));
    }
    for t in &before.threads {
        assert_eq!(store.thread(&t.key).as_ref(), Some(t));
    }
    for t in &before.tickets {
        assert_eq!(store.ticket(&t.id).as_ref(), Some(t));
    }
    for d in &before.decisions {
        assert_eq!(store.decision(&d.id).as_ref(), Some(d));
    }
    for c in &before.trails {
        assert_eq!(store.trail(&c.assignment).as_ref(), Some(c));
    }

    // The reopened store carries on: the Eval left running is found and decided.
    let mut sql: SqlEngine = Engine::new(store, policy(), clock(&sql_now));
    let ignore = Verdict::Ignore {
        reason: "not for us".into(),
    };
    assert_eq!(
        sql.decide(&running, ignore.clone()),
        mem.decide(&running, ignore)
    );
    let expected = memory_contents(mem.repos());
    assert_eq!(
        first_difference(&expected, &sqlite_contents(sql.repos())),
        None
    );
    assert!(sql.repos().active_threads().is_empty());
    assert_eq!(
        sql.repos().verify_chain().unwrap().len,
        expected.events.len() as u64
    );
}

#[test]
fn a_commit_that_fails_part_way_writes_nothing() {
    let db = TempDb::new("atomic");
    let now = Rc::new(Cell::new(1_000));
    let store = SqliteStore::open(&db.path).unwrap();
    let mut e: SqlEngine = Engine::new(store, policy(), clock(&now));
    let alice = || Actor::person("alice");
    e.receive(slack("t1", alice(), "th1", "@agent one"))
        .unwrap();
    let before = sqlite_contents(e.repos());
    let raw = db.raw();
    raw.execute_batch(
        "CREATE TRIGGER refuse_events BEFORE INSERT ON events
         BEGIN SELECT RAISE(ABORT, 'disk on fire'); END;",
    )
    .unwrap();

    // evaluate_due writes a Thread and an Assess Effect before its event.
    let err = e.evaluate_due().unwrap_err();
    assert!(
        matches!(&err, Error::Store(why) if why.contains("disk on fire")),
        "{err}"
    );
    assert_eq!(sqlite_contents(e.repos()), before);
    let err = e
        .receive(slack("t2", alice(), "th2", "@agent two"))
        .unwrap_err();
    assert!(matches!(err, Error::Store(_)));
    assert_eq!(sqlite_contents(e.repos()), before);

    raw.execute_batch("DROP TRIGGER refuse_events").unwrap();
    assert_eq!(e.evaluate_due().unwrap().len(), 1);
    assert_eq!(e.repos().effects().unwrap().len(), 1);
    e.repos().verify_chain().unwrap();
}

#[test]
fn a_failed_read_fails_the_next_commit_once() {
    let db = TempDb::new("read-error");
    let now = Rc::new(Cell::new(1_000));
    let store = SqliteStore::open(&db.path).unwrap();
    let mut e: SqlEngine = Engine::new(store, policy(), clock(&now));
    let alice = || Actor::person("alice");
    e.receive(slack("t1", alice(), "th1", "@agent one"))
        .unwrap();
    db.raw()
        .execute("UPDATE triggers SET body = '{' WHERE id = 't1'", [])
        .unwrap();

    // The redelivery check reads t1, which no longer parses; the port read
    // returns nothing, so the commit is what fails.
    let err = e
        .receive(slack("t1", alice(), "th1", "@agent one"))
        .unwrap_err();
    assert!(
        matches!(&err, Error::Store(why) if why.contains("a read failed")),
        "{err}"
    );
    assert_eq!(e.repos().events().unwrap().len(), 1);
    assert_eq!(e.repos().threads().unwrap()[0].triggers.len(), 1);

    let next = e.receive(slack("t2", alice(), "th2", "@agent two"));
    assert_eq!(
        next.unwrap(),
        Intake::Stored {
            reached: Vec::new(),
            queued: true
        }
    );
}

#[test]
fn a_file_store_uses_wal_and_migrates_once() {
    let db = TempDb::new("wal");
    drop(SqliteStore::open(&db.path).unwrap());
    let store = SqliteStore::open(&db.path).unwrap();
    assert_eq!(store.schema_version().unwrap(), 9);
    let raw = db.raw();
    let mode: String = raw
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let rows: u32 = raw
        .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 9, "one row per migration");
}
