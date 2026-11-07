//! Schema 8 holds memos, one row each. The text is in the row and only its
//! digest on the chain, so forgetting a memo erases its text from the file
//! and the chain still verifies; a file from schema 7 opens with its rows
//! and its chain as they were. Schema 9 adds what a memo supersedes, what
//! replaced a retired one and what the dream cited; a schema 8 file's memos
//! read back as superseding nothing.

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
            supersedes: None,
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

/// Whether the file at `path` holds `text`, byte for byte.
fn holds(path: &std::path::Path, text: &str) -> bool {
    std::fs::read(path).is_ok_and(|bytes| {
        bytes
            .windows(text.len())
            .any(|window| window == text.as_bytes())
    })
}

#[test]
fn after_a_forget_the_text_is_in_neither_the_database_file_nor_its_wal() {
    let db = TempDb::new("memo-bytes");
    let wal = db.path.with_file_name("agent.db-wal");
    let now = Rc::new(Cell::new(1_000));
    let mut e = engine(&db, &now);
    let canary = "canary 7f3e9a: the owner's private note";
    let id = remember(&mut e, MemoScope::Global, canary);
    // A retire rewrites the row while it still holds the text, so a stale
    // copy would be left in the page's free space without secure_delete.
    let alice = MemoActor::Operator {
        source: "slack".into(),
        by: Actor::person("alice"),
    };
    e.retire_memo(&id, alice, "superseded").unwrap();
    for n in 0..40 {
        now.set(now.get() + 1);
        remember(
            &mut e,
            MemoScope::Global,
            &format!("filler {n}: {}", "z".repeat(300)),
        );
    }
    assert!(
        holds(&db.path, canary) || holds(&wal, canary),
        "the text was written before it was forgotten"
    );

    e.forget_memo(&id, "slack", Actor::person("alice")).unwrap();

    assert!(!holds(&db.path, canary), "agent.db still holds the text");
    assert!(!holds(&wal, canary), "agent.db-wal still holds the text");
    // The Store goes on, and still holds nothing of it after more writes
    // and a close.
    remember(&mut e, MemoScope::Global, "Answer in French.");
    e.repos().verify_chain().unwrap();
    drop(e);
    assert!(!holds(&db.path, canary));
    assert!(!holds(&wal, canary));
    let reopened = SqliteStore::open(&db.path).unwrap();
    assert_eq!(reopened.read_memo(&id).unwrap().unwrap().text, "");
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
    assert_eq!(e.repos().schema_version().unwrap(), 9);
    assert_eq!(sqlite_contents(e.repos()), before);
    assert_eq!(e.repos().verify_chain().unwrap(), chain);
    let id = remember(&mut e, MemoScope::Global, "Answer in French.");
    assert!(e.repos().read_memo(&id).unwrap().unwrap().is_live());
    assert_eq!(e.repos().verify_chain().unwrap().len, chain.len + 1);
}

/// Rows exactly as a schema 8 build wrote them: an active memo, and one an
/// operator retired, whose state names no replacement and cites nothing.
const V8_ACTIVE: &str = r#"{"id":"mm_v8active","scope":"repo:acme/widgets","text":"Tests run with cargo test.","text_sha256":"sha256:1111111111111111111111111111111111111111111111111111111111111111","sources":[{"kind":"call","assignment":"as_1","call":"run_code#2","tool":"run_code","transcript":"sha256:2222222222222222222222222222222222222222222222222222222222222222","quote":"test result: ok"}],"trust":"model","author":{"kind":"worker","ticket":"tk_1","assignment":"as_1"},"state":{"state":"active","since_ms":5,"review":{"by":"review:deepseek-flash@max","reason":"a tested lesson"}},"created_ms":4,"verified_ms":5}"#;
const V8_RETIRED: &str = r#"{"id":"mm_v8retired","scope":"global","text":"Answer in French.","text_sha256":"sha256:3333333333333333333333333333333333333333333333333333333333333333","sources":[{"kind":"operator"}],"trust":"person","author":{"kind":"operator","source":"slack","by":{"id":"alice","kind":"person"}},"state":{"state":"retired","by":{"kind":"operator","source":"slack","by":{"id":"alice","kind":"person"}},"reason":"changed my mind","at_ms":9},"created_ms":2,"verified_ms":2}"#;

#[test]
fn a_schema_8_file_opens_at_9_and_its_memos_read_back_as_superseding_nothing() {
    let db = TempDb::new("memo-v8");
    let now = Rc::new(Cell::new(1_000));
    drop(engine(&db, &now));
    // What a schema 8 build left: version 8, and memo rows of its shape.
    let raw = db.raw();
    raw.execute_batch("DELETE FROM schema_version WHERE version > 8;")
        .unwrap();
    for (id, scope, state, body) in [
        ("mm_v8active", "repo:acme/widgets", "active", V8_ACTIVE),
        ("mm_v8retired", "global", "retired", V8_RETIRED),
    ] {
        raw.execute(
            "INSERT INTO memos (id, scope, state, body) VALUES (?1, ?2, ?3, ?4)",
            [id, scope, state, body],
        )
        .unwrap();
    }
    drop(raw);

    let e = engine(&db, &now);
    assert_eq!(e.repos().schema_version().unwrap(), 9);
    let active = e
        .repos()
        .read_memo(&MemoId::new("mm_v8active"))
        .unwrap()
        .unwrap();
    assert_eq!(active.supersedes, None);
    assert!(active.is_active());
    assert_eq!(
        serde_json::to_string(&active).unwrap(),
        V8_ACTIVE,
        "the same bytes back"
    );
    let retired = e
        .repos()
        .read_memo(&MemoId::new("mm_v8retired"))
        .unwrap()
        .unwrap();
    match &retired.state {
        MemoState::Retired {
            replaced_by,
            evidence,
            reason,
            ..
        } => {
            assert_eq!((replaced_by, evidence.len()), (&None, 0));
            assert_eq!(reason, "changed my mind");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(serde_json::to_string(&retired).unwrap(), V8_RETIRED);
    assert_eq!(e.repos().live_memos().len(), 1);
}
