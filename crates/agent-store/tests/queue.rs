mod common;

use common::{TempDb, channel};
use solos_agent_core::Repositories;
use solos_agent_core::model::{
    AssignmentId, ChangeSet, Effect, MessageId, MessageKind, OutboundMessage, TicketId,
};
use solos_agent_store::{EffectId, QueuedEffect, SqliteStore, StoreError};

fn send(id: &str, thread: &str) -> Effect {
    Effect::Send(OutboundMessage {
        id: MessageId::new(id),
        channel: channel(thread),
        kind: MessageKind::Result,
        text: format!("message {id}"),
        ticket: None,
        ask: None,
    })
}

fn stop(id: &str) -> Effect {
    Effect::Stop {
        ticket: TicketId::new("tk"),
        assignment: AssignmentId::new(id),
        reason: "stopped".into(),
    }
}

fn commit(store: &mut SqliteStore, effects: Vec<Effect>) {
    let changes = ChangeSet {
        effects,
        ..ChangeSet::default()
    };
    store.commit(changes).unwrap();
}

fn names(queued: &[QueuedEffect]) -> Vec<String> {
    queued
        .iter()
        .map(|q| match &q.effect {
            Effect::Send(m) => m.id.to_string(),
            Effect::Stop { assignment, .. } => assignment.to_string(),
            other => format!("{other:?}"),
        })
        .collect()
}

fn find(store: &SqliteStore, name: &str) -> QueuedEffect {
    let all = store.effects().unwrap();
    let i = names(&all).iter().position(|n| n == name).unwrap();
    all[i].clone()
}

#[test]
fn sends_to_one_conversation_keep_their_commit_order() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    commit(&mut store, vec![send("a1", "th1"), send("b1", "th2")]);
    commit(&mut store, vec![stop("x"), send("a2", "th1")]);
    commit(&mut store, vec![send("a3", "th1")]);

    let due = |s: &SqliteStore, now| names(&s.due_effects(now, 10).unwrap());
    assert_eq!(due(&store, 0), ["a1", "b1", "x"], "a2 and a3 wait for a1");
    assert_eq!(names(&store.due_effects(0, 2).unwrap()), ["a1", "b1"]);

    let a1 = find(&store, "a1");
    assert!(store.fail_effect(a1.id, 0, "rate limited", 1_000).unwrap());
    assert_eq!(
        due(&store, 500),
        ["b1", "x"],
        "a retry keeps its lane's place"
    );
    for name in ["b1", "x"] {
        assert!(store.complete_effect(find(&store, name).id).unwrap());
    }
    assert!(due(&store, 999).is_empty());
    assert_eq!(due(&store, 1_000), ["a1"]);
    assert!(store.complete_effect(a1.id).unwrap());
    assert_eq!(due(&store, 1_000), ["a2"]);
    assert!(store.complete_effect(find(&store, "a2").id).unwrap());
    assert_eq!(due(&store, 1_000), ["a3"]);
    assert!(store.complete_effect(find(&store, "a3").id).unwrap());
    assert!(due(&store, u64::MAX >> 1).is_empty());
    let ids: Vec<EffectId> = store.effects().unwrap().iter().map(|q| q.id).collect();
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "ids follow commit order"
    );
}

#[test]
fn a_failure_counts_an_attempt_and_waits_for_its_retry_time() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    commit(&mut store, vec![stop("x")]);
    let x = find(&store, "x");
    assert_eq!((x.attempts, x.due_ms, x.done), (0, 0, false));
    assert_eq!(x.last_error, None);

    assert!(store.fail_effect(x.id, 0, "timeout", 100).unwrap());
    let after = store.effect(x.id).unwrap().unwrap();
    assert_eq!((after.attempts, after.due_ms), (1, 100));
    assert_eq!(after.last_error.as_deref(), Some("timeout"));
    assert!(
        !store.fail_effect(x.id, 0, "timeout", 100).unwrap(),
        "the same failure counts once"
    );
    assert_eq!(store.effect(x.id).unwrap().unwrap(), after);

    assert!(store.due_effects(99, 10).unwrap().is_empty());
    let due = store.due_effects(100, 10).unwrap();
    assert_eq!(due, [after]);
    assert!(store.fail_effect(x.id, 1, "refused", 300).unwrap());
    let again = store.effect(x.id).unwrap().unwrap();
    assert_eq!((again.attempts, again.due_ms), (2, 300));
    assert_eq!(again.last_error.as_deref(), Some("refused"));

    assert!(store.complete_effect(x.id).unwrap());
    assert!(
        !store.fail_effect(x.id, 2, "late", 400).unwrap(),
        "done stays done"
    );
    let done = store.effect(x.id).unwrap().unwrap();
    assert!(done.done);
    assert_eq!(done.attempts, 2);
    assert!(store.due_effects(u64::MAX >> 1, 10).unwrap().is_empty());

    let missing = EffectId(999);
    assert!(matches!(
        store.complete_effect(missing),
        Err(StoreError::NoEffect(id)) if id == missing
    ));
    assert!(matches!(
        store.fail_effect(missing, 0, "x", 0),
        Err(StoreError::NoEffect(_))
    ));
}

#[test]
fn done_and_fail_repeat_safely_after_a_crash() {
    let db = TempDb::new("queue-crash");
    let mut store = SqliteStore::open(&db.path).unwrap();
    commit(
        &mut store,
        vec![send("a1", "th1"), send("a2", "th1"), stop("x")],
    );
    let (a1, x) = (find(&store, "a1"), find(&store, "x"));
    assert_eq!(names(&store.due_effects(0, 10).unwrap()), ["a1", "x"]);

    assert!(store.complete_effect(a1.id).unwrap());
    drop(store);
    let store = SqliteStore::open(&db.path).unwrap();
    assert!(!store.complete_effect(a1.id).unwrap(), "already done");
    assert_eq!(names(&store.due_effects(0, 10).unwrap()), ["a2", "x"]);

    assert!(store.fail_effect(x.id, 0, "boom", 50).unwrap());
    drop(store);
    let store = SqliteStore::open(&db.path).unwrap();
    assert!(!store.fail_effect(x.id, 0, "boom", 50).unwrap());
    let failed = store.effect(x.id).unwrap().unwrap();
    assert_eq!(
        (failed.attempts, failed.due_ms, failed.done),
        (1, 50, false)
    );

    // A crash before the outcome is recorded leaves the Effect due: it runs again.
    drop(store);
    let store = SqliteStore::open(&db.path).unwrap();
    assert_eq!(names(&store.due_effects(50, 10).unwrap()), ["a2", "x"]);
    assert!(store.complete_effect(x.id).unwrap());
    drop(store);
    let store = SqliteStore::open(&db.path).unwrap();
    assert!(!store.complete_effect(x.id).unwrap());
    assert!(!store.fail_effect(x.id, 1, "late", 60).unwrap());
    let done = store.effect(x.id).unwrap().unwrap();
    assert_eq!((done.attempts, done.done), (1, true));
    assert_eq!(names(&store.due_effects(1_000, 10).unwrap()), ["a2"]);
}
