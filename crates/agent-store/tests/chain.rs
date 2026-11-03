mod common;

use common::TempDb;
use sha2::{Digest, Sha256};
use solos_agent_core::Repositories;
use solos_agent_core::model::{ChangeSet, DomainEvent, EvalId, Event, ThreadKey};
use solos_agent_store::{SqliteStore, StoreError};

fn expired(n: u64) -> Event {
    Event {
        at_ms: 1_000 + n,
        event: DomainEvent::EvalExpired {
            eval: EvalId::new(format!("ev{n}")),
            thread: ThreadKey::new("th1"),
        },
    }
}

fn commit(store: &mut SqliteStore, events: impl IntoIterator<Item = u64>) {
    let changes = ChangeSet {
        events: events.into_iter().map(expired).collect(),
        ..ChangeSet::default()
    };
    store.commit(changes).unwrap();
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

fn chained(n: u64) -> (TempDb, SqliteStore) {
    let db = TempDb::new("chain");
    let mut store = SqliteStore::open(&db.path).unwrap();
    commit(&mut store, 1..=2);
    commit(&mut store, 3..=n);
    (db, store)
}

fn unprotect(db: &TempDb) -> rusqlite::Connection {
    let raw = db.raw();
    raw.execute_batch("DROP TRIGGER events_no_update; DROP TRIGGER events_no_delete;")
        .unwrap();
    raw
}

#[test]
fn each_row_hashes_its_predecessor_and_its_json_like_events_jsonl() {
    let (db, store) = chained(5);
    let mut prev = "0".repeat(64);
    let rows = rows(&db);
    assert_eq!(rows.len(), 5);
    for (n, (seq, body, prev_hash, hash)) in (1..).zip(&rows) {
        assert_eq!(*seq, n);
        assert_eq!(*body, serde_json::to_string(&expired(n)).unwrap());
        assert_eq!(*prev_hash, prev);
        let expected = format!("{:x}", Sha256::digest(format!("{prev}{body}").as_bytes()));
        assert_eq!(*hash, expected);
        prev = hash.clone();
    }
    let chain = store.verify_chain().unwrap();
    assert_eq!((chain.len, chain.head), (5, prev));
    assert_eq!(
        store.events().unwrap(),
        (1..=5).map(expired).collect::<Vec<_>>()
    );
}

#[test]
fn an_empty_log_verifies_and_the_chain_continues_after_a_reopen() {
    let db = TempDb::new("chain-reopen");
    let mut store = SqliteStore::open(&db.path).unwrap();
    let empty = store.verify_chain().unwrap();
    assert_eq!((empty.len, empty.head), (0, "0".repeat(64)));
    commit(&mut store, 1..=3);
    let head = store.verify_chain().unwrap().head;
    drop(store);
    let mut store = SqliteStore::open(&db.path).unwrap();
    commit(&mut store, 4..=6);
    assert_eq!(rows(&db)[3].2, head);
    assert_eq!(store.verify_chain().unwrap().len, 6);
}

#[test]
fn verify_chain_names_a_tampered_row() {
    let (db, store) = chained(5);
    let raw = unprotect(&db);
    raw.execute(
        "UPDATE events SET body = replace(body, 'ev3', 'ev9') WHERE seq = 3",
        [],
    )
    .unwrap();
    let err = store.verify_chain().unwrap_err();
    assert!(
        matches!(err, StoreError::Chain { seq: 3, reason } if reason.contains("hash")),
        "{err}"
    );
}

#[test]
fn verified_events_are_the_events_of_the_chain_verify_chain_finds() {
    let (db, store) = chained(5);
    let (chain, events) = store.verified_events().unwrap();
    assert_eq!(chain, store.verify_chain().unwrap());
    assert_eq!(events, (1..=5).map(expired).collect::<Vec<_>>());

    unprotect(&db)
        .execute(
            "UPDATE events SET body = replace(body, 'ev4', 'ev9') WHERE seq = 4",
            [],
        )
        .unwrap();
    let err = store.verified_events().unwrap_err();
    assert!(matches!(err, StoreError::Chain { seq: 4, .. }), "{err}");
}

#[test]
fn rehashing_a_tampered_row_breaks_the_next_link() {
    let (db, store) = chained(5);
    let raw = unprotect(&db);
    let (_, body, prev, _) = rows(&db)[2].clone();
    let forged = body.replace("ev3", "ev9");
    let hash = format!("{:x}", Sha256::digest(format!("{prev}{forged}").as_bytes()));
    raw.execute(
        "UPDATE events SET body = ?1, hash = ?2 WHERE seq = 3",
        [&forged, &hash],
    )
    .unwrap();
    let err = store.verify_chain().unwrap_err();
    assert!(
        matches!(err, StoreError::Chain { seq: 4, reason } if reason.contains("chain")),
        "{err}"
    );
}

#[test]
fn a_deleted_row_leaves_a_gap() {
    let (db, store) = chained(5);
    unprotect(&db)
        .execute("DELETE FROM events WHERE seq = 2", [])
        .unwrap();
    let err = store.verify_chain().unwrap_err();
    assert!(
        matches!(err, StoreError::Chain { seq: 3, reason } if reason.contains("sequence")),
        "{err}"
    );
}

#[test]
fn the_log_refuses_updates_and_deletes() {
    let (db, store) = chained(3);
    let raw = db.raw();
    for sql in [
        "UPDATE events SET body = '{}' WHERE seq = 1",
        "DELETE FROM events WHERE seq = 3",
    ] {
        let err = raw.execute(sql, []).unwrap_err().to_string();
        assert!(err.contains("append-only"), "{sql}: {err}");
    }
    assert_eq!(store.verify_chain().unwrap().len, 3);
}
