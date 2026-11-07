//! Schema 8 holds memos, one row each. The text is in the row and only its
//! digest on the chain, so forgetting a memo erases its text from the file
//! and the chain still verifies; a file from schema 7 opens with its rows
//! and its chain as they were.

mod common;

use common::{SqlEngine, TempDb, accept, clock, policy, slack, sqlite_contents};
use solos_agent_core::memo::{
    MemoActor, MemoAudience, MemoAuthor, MemoIn, MemoScope, MemoSource, MemoState,
};
use solos_agent_core::model::{Actor, DomainEvent, MemoId, Plan};
use solos_agent_core::trail::Digest;
use solos_agent_core::{Engine, Repositories};
use solos_agent_store::SqliteStore;
use std::cell::Cell;
use std::rc::Rc;

fn engine(db: &TempDb, now: &Rc<Cell<u64>>) -> SqlEngine {
    Engine::new(SqliteStore::open(&db.path).unwrap(), policy(), clock(now))
}

fn remember(engine: &mut SqlEngine, scope: MemoScope, text: &str) -> MemoId {
    engine
        .propose_memo(MemoIn {
            scope,
            text: text.into(),
            sources: vec![MemoSource::Operator],
            author: MemoAuthor::Operator {
                source: "slack".into(),
                by: Actor::person("alice"),
            },
        })
        .unwrap()
}

/// The `body` of every row of `table`, as stored.
fn raw_bodies(db: &TempDb, sql: &str) -> Vec<String> {
    let raw = db.raw();
    let mut stmt = raw.prepare(sql).unwrap();
    stmt.query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn forgetting_a_memo_erases_its_text_from_the_file_and_the_chain_still_verifies() {
    let db = TempDb::new("memo-forget");
    let now = Rc::new(Cell::new(1_000));
    let mut e = engine(&db, &now);
    let secret = "The staging box is at 10.1.2.3, password hunter2";
    let id = remember(&mut e, MemoScope::Global, secret);
    let kept = remember(&mut e, MemoScope::Global, "Answer in French.");
    let shown = e.show_memos(MemoAudience::Router).unwrap();
    assert_eq!(shown.shown.len(), 2);
    assert!(
        raw_bodies(&db, "SELECT body FROM memos")
            .concat()
            .contains("hunter2")
    );
    let chain = raw_bodies(&db, "SELECT body FROM events").concat();
    assert!(!chain.contains("hunter2"), "the chain holds digests only");
    assert!(chain.contains(Digest::of(secret.as_bytes()).as_str()));

    now.set(2_000);
    e.forget_memo(&id, "slack", Actor::person("alice")).unwrap();
    drop(e);

    let store = SqliteStore::open(&db.path).unwrap();
    let chain = store.verify_chain().unwrap();
    assert_eq!(chain.len, store.events().unwrap().len() as u64);
    let rows = raw_bodies(&db, "SELECT body FROM memos").concat();
    assert!(!rows.contains("hunter2"), "{rows}");
    let forgotten = store.read_memo(&id).unwrap().unwrap();
    assert_eq!(forgotten.text, "");
    assert_eq!(forgotten.text_sha256, Digest::of(secret.as_bytes()));
    assert!(matches!(forgotten.state, MemoState::Forgotten { .. }));
    assert_eq!(
        store
            .live_memos()
            .into_iter()
            .map(|m| m.id)
            .collect::<Vec<_>>(),
        [kept]
    );
    let state: String = db
        .raw()
        .query_row(
            "SELECT state FROM memos WHERE id = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "forgotten");
}

#[test]
fn memos_and_their_events_survive_a_reopen() {
    let db = TempDb::new("memo-reopen");
    let now = Rc::new(Cell::new(1_000));
    let mut e = engine(&db, &now);
    let repo = MemoScope::try_from("repo:acme/widgets".to_owned()).unwrap();
    let a = remember(&mut e, repo.clone(), "Tests run with ./check.");
    now.set(1_500);
    let b = remember(&mut e, MemoScope::Global, "Keep PR descriptions short.");
    e.retire_memo(
        &b,
        MemoActor::Operator {
            source: "slack".into(),
            by: Actor::person("alice"),
        },
        "the owner changed their mind",
    )
    .unwrap();
    let before = sqlite_contents(e.repos());
    drop(e);

    let e = engine(&db, &now);
    let after = sqlite_contents(e.repos());
    assert_eq!(after, before);
    assert_eq!(e.repos().memo(&a).unwrap().scope, repo);
    assert!(matches!(
        e.repos().memo(&b).unwrap().state,
        MemoState::Retired { .. }
    ));
    let kinds: Vec<&str> = after
        .events
        .iter()
        .map(|event| match event.event {
            DomainEvent::MemoProposed { .. } => "proposed",
            DomainEvent::MemoRetired { .. } => "retired",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["proposed", "proposed", "retired"]);
    e.repos().verify_chain().unwrap();
}

#[test]
fn a_schema_7_file_opens_with_its_rows_and_chain_and_then_holds_memos() {
    let db = TempDb::new("memo-v7");
    let now = Rc::new(Cell::new(1_000));
    let mut e = engine(&db, &now);
    e.receive(slack("t1", Actor::person("alice"), "th1", "@agent do it"))
        .unwrap();
    let eval = e.evaluate_due().unwrap().remove(0);
    let session = Plan::Session {
        harness: None,
        environment: None,
        method: None,
    };
    e.decide(&eval, accept("Do it", session, 1_000)).unwrap();
    let before = sqlite_contents(e.repos());
    let chain = e.repos().verify_chain().unwrap();
    drop(e);
    // What a schema 7 build left: no memos table, and version 7.
    let raw = db.raw();
    raw.execute_batch("DROP TABLE memos; DELETE FROM schema_version WHERE version > 7;")
        .unwrap();
    drop(raw);

    let mut e = engine(&db, &now);
    assert_eq!(e.repos().schema_version().unwrap(), 8);
    assert_eq!(sqlite_contents(e.repos()), before);
    assert_eq!(e.repos().verify_chain().unwrap(), chain);
    let id = remember(&mut e, MemoScope::Global, "Answer in French.");
    assert!(e.repos().read_memo(&id).unwrap().unwrap().is_live());
    assert_eq!(e.repos().verify_chain().unwrap().len, chain.len + 1);
}
