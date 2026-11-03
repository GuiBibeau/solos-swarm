mod common;

use common::{SqlEngine, TempDb, accept, clock, page_seen, policy, signed_step, slack};
use sha2::{Digest as _, Sha256};
use solos_agent_core::model::{Actor, AssignmentId, DomainEvent, Plan, TicketId};
use solos_agent_core::trail::{CloseReason, StepOutcome, TrailAction, TrailOutcome};
use solos_agent_core::{Engine, Error, Repositories};
use solos_agent_store::{SqliteStore, StoreError};
use std::cell::Cell;
use std::rc::Rc;

fn running(e: &mut SqlEngine) -> (TicketId, AssignmentId) {
    e.receive(slack(
        "t1",
        Actor::person("alice"),
        "th1",
        "@agent read my mail",
    ))
    .unwrap();
    let eval = e.evaluate_due().unwrap().remove(0);
    let session = Plan::Session {
        harness: None,
        environment: None,
    };
    let decision = e.decide(&eval, accept("Read", session, 1_000)).unwrap();
    let ticket = e.repos().decision(&decision).unwrap().ticket.unwrap();
    let assignment = e.repos().ticket(&ticket).unwrap().assignments[0].id.clone();
    e.assignment_started(&ticket, &assignment).unwrap();
    (ticket, assignment)
}

fn open() -> TrailAction {
    TrailAction::Open {
        url: "https://example.com/".into(),
        key: None,
    }
}

fn step(
    e: &mut SqlEngine,
    ticket: &TicketId,
    assignment: &AssignmentId,
    action: TrailAction,
) -> u32 {
    let n = e.trail_ask(ticket, assignment, action.clone()).unwrap();
    let outcome = match &action {
        TrailAction::Close { reason } => StepOutcome::Closed {
            steps: n,
            reason: *reason,
        },
        _ => page_seen(),
    };
    let signed = signed_step(ticket, assignment, n, action, outcome);
    e.trail_done(ticket, assignment, n, TrailOutcome::signed(signed))
        .unwrap();
    n
}

fn engine(db: &TempDb, now: &Rc<Cell<u64>>) -> SqlEngine {
    Engine::new(SqliteStore::open(&db.path).unwrap(), policy(), clock(now))
}

fn rows(db: &TempDb) -> Vec<(u64, String, String, String)> {
    let raw = db.raw();
    let mut stmt = raw
        .prepare("SELECT seq, body, prev_hash, hash FROM events ORDER BY seq")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn a_trail_survives_a_reopen_and_the_chain_still_verifies() {
    let db = TempDb::new("trail-reopen");
    let now = Rc::new(Cell::new(5_000));
    let mut e = engine(&db, &now);
    let (ticket, assignment) = running(&mut e);
    assert_eq!(step(&mut e, &ticket, &assignment, open()), 1);
    assert_eq!(step(&mut e, &ticket, &assignment, open()), 2);
    let pending = e.trail_ask(&ticket, &assignment, open()).unwrap();
    assert_eq!(pending, 3);
    let before = e.repos().trail(&assignment).unwrap();
    drop(e);

    let mut e = engine(&db, &now);
    assert_eq!(e.repos().trail(&assignment), Some(before.clone()));
    assert_eq!(e.repos().trails().unwrap(), [before]);
    let lost = TrailOutcome::Lost {
        why: "the host stopped".into(),
    };
    e.trail_done(&ticket, &assignment, 3, lost).unwrap();
    assert_eq!(step(&mut e, &ticket, &assignment, open()), 4);

    let store = e.repos();
    let events = store.events().unwrap();
    let asked: Vec<u32> = events
        .iter()
        .filter_map(|e| match &e.event {
            DomainEvent::TrailAsked { n, .. } => Some(*n),
            _ => None,
        })
        .collect();
    assert_eq!(asked, [1, 2, 3, 4]);
    let chain = store.verify_chain().unwrap();
    assert_eq!(chain.len, events.len() as u64);
}

#[test]
fn trail_events_hash_like_every_other_event_and_a_tampered_step_breaks_the_chain() {
    let db = TempDb::new("trail-chain");
    let now = Rc::new(Cell::new(5_000));
    let mut e = engine(&db, &now);
    let (ticket, assignment) = running(&mut e);
    step(&mut e, &ticket, &assignment, open());
    step(
        &mut e,
        &ticket,
        &assignment,
        TrailAction::Close {
            reason: CloseReason::Shutdown,
        },
    );

    let mut prev = "0".repeat(64);
    let rows = rows(&db);
    for ((seq, body, prev_hash, hash), event) in rows.iter().zip(e.repos().events().unwrap()) {
        assert_eq!(*body, serde_json::to_string(&event).unwrap(), "seq {seq}");
        assert_eq!(*prev_hash, prev);
        assert_eq!(
            *hash,
            format!("{:x}", Sha256::digest(format!("{prev}{body}").as_bytes()))
        );
        prev = hash.clone();
    }
    assert!(
        rows.iter()
            .any(|r| r.1.contains("\"type\":\"trail_asked\""))
    );
    assert!(rows.iter().any(|r| r.1.contains("\"type\":\"trail_done\"")));
    assert_eq!(e.repos().verify_chain().unwrap().head, prev);

    let (seq, body, ..) = rows
        .iter()
        .find(|r| r.1.contains("\"type\":\"trail_done\""))
        .unwrap()
        .clone();
    let raw = db.raw();
    raw.execute_batch("DROP TRIGGER events_no_update; DROP TRIGGER events_no_delete;")
        .unwrap();
    let forged = body.replace("\"environment\":\"local\"", "\"environment\":\"laptop\"");
    assert_ne!(forged, body);
    raw.execute(
        "UPDATE events SET body = ?1 WHERE seq = ?2",
        rusqlite::params![forged, seq],
    )
    .unwrap();
    let err = e.repos().verify_chain().unwrap_err();
    assert!(
        matches!(err, StoreError::Chain { seq: s, reason } if s == seq && reason.contains("hash")),
        "{err}"
    );
}

#[test]
fn a_step_and_its_cursor_commit_together_or_not_at_all() {
    let db = TempDb::new("trail-atomic");
    let now = Rc::new(Cell::new(5_000));
    let mut e = engine(&db, &now);
    let (ticket, assignment) = running(&mut e);
    let events = e.repos().events().unwrap().len();
    let raw = db.raw();
    raw.execute_batch(
        "CREATE TRIGGER refuse_events BEFORE INSERT ON events
         BEGIN SELECT RAISE(ABORT, 'disk on fire'); END;",
    )
    .unwrap();

    let err = e.trail_ask(&ticket, &assignment, open()).unwrap_err();
    assert!(
        matches!(&err, Error::Store(why) if why.contains("disk on fire")),
        "{err}"
    );
    assert_eq!(e.repos().trail(&assignment), None);
    assert!(e.repos().trails().unwrap().is_empty());

    raw.execute_batch("DROP TRIGGER refuse_events").unwrap();
    assert_eq!(
        e.trail_ask(&ticket, &assignment, open()).unwrap(),
        1,
        "the failed ask burned no number"
    );
    assert_eq!(e.repos().events().unwrap().len(), events + 1);
}

#[test]
fn an_assignment_keeps_one_row_that_its_ticket_can_find() {
    let db = TempDb::new("trail-row");
    let now = Rc::new(Cell::new(5_000));
    let mut e = engine(&db, &now);
    let (ticket, assignment) = running(&mut e);
    for _ in 0..3 {
        step(&mut e, &ticket, &assignment, open());
    }

    let raw = db.raw();
    let found: Vec<String> = raw
        .prepare("SELECT assignment FROM trails WHERE ticket = ?1")
        .unwrap()
        .query_map([ticket.as_str()], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(found, [assignment.to_string()]);
    assert_eq!(e.repos().trails().unwrap()[0].next, 4);
}
