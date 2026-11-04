mod common;

use common::{SqlEngine, TempDb, accept, clock, policy, slack};
use solos_agent_core::Engine;
use solos_agent_core::model::{Actor, DecisionKind, Plan};
use solos_agent_store::{HoldRequest, SqliteStore, StoreError};
use std::cell::Cell;
use std::rc::Rc;

/// Two accepted requests a minute apart, with a hold on the Router's account.
fn populate(db: &TempDb) {
    let now = Rc::new(Cell::new(60_000));
    let mut engine: SqlEngine =
        Engine::new(SqliteStore::open(&db.path).unwrap(), policy(), clock(&now));
    for (n, thread) in ["th1", "th2"].into_iter().enumerate() {
        let id = format!("t{n}");
        engine
            .receive(slack(&id, Actor::person("alice"), thread, "@agent do it"))
            .unwrap();
        let eval = engine.evaluate_due().unwrap().remove(0);
        let session = Plan::Session {
            harness: None,
            environment: None,
            method: None,
        };
        engine
            .decide(&eval, accept("Do it", session, 1_000))
            .unwrap();
        now.set(now.get() + 60_000);
    }
    let hold = HoldRequest {
        account: "router",
        provider: "deepseek",
        since_ms: 0,
        cap_microusd: 1_000,
        hold_microusd: 300,
        at_ms: 90_000,
    };
    engine.repos_mut().reserve(&hold).unwrap();
}

#[test]
fn a_read_only_store_reads_everything_and_writes_nothing() {
    let db = TempDb::new("read-only");
    populate(&db);
    let before = std::fs::read(&db.path).unwrap();

    let store = SqliteStore::open_read_only(&db.path).unwrap();
    assert_eq!(store.schema_version().unwrap(), 6);
    assert_eq!(store.open_tickets().unwrap().len(), 2);
    assert!(!store.pending_effects().unwrap().is_empty());
    let refused = store.mark_alert_sent("wd_x", 1).unwrap_err();
    assert!(
        matches!(&refused, StoreError::Sqlite(err) if err.to_string().contains("readonly")),
        "{refused}"
    );
    drop(store);

    assert_eq!(std::fs::read(&db.path).unwrap(), before);
    let missing = db.path.with_file_name("missing.db");
    assert!(SqliteStore::open_read_only(&missing).is_err());
    assert!(!missing.exists(), "a read-only open creates no file");
}

#[test]
fn a_file_from_a_newer_build_is_refused_and_this_build_writes_the_version_older_ones_refuse() {
    let db = TempDb::new("older-binary");
    populate(&db);
    let known = SqliteStore::open_read_only(&db.path)
        .unwrap()
        .schema_version()
        .unwrap();
    assert_eq!(known, 6, "a build that knows only 5 refuses this file");

    db.raw()
        .execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [known + 1],
        )
        .unwrap();
    let before = std::fs::read(&db.path).unwrap();
    let found = known + 1;
    let read_only = SqliteStore::open_read_only(&db.path).err().unwrap();
    assert_eq!(
        std::fs::read(&db.path).unwrap(),
        before,
        "nothing was written"
    );
    for refused in [read_only, SqliteStore::open(&db.path).err().unwrap()] {
        assert!(
            matches!(&refused, StoreError::Schema { found: f, known: k } if *f == found && *k == known),
            "{refused}"
        );
    }
}

#[test]
fn recent_decisions_come_newest_first_and_stop_at_the_window() {
    let db = TempDb::new("recent");
    populate(&db);
    let store = SqliteStore::open(&db.path).unwrap();
    let at = |since, limit| -> Vec<u64> {
        let found = store.recent_decisions(since, limit).unwrap();
        assert!(found.iter().all(|d| d.kind == DecisionKind::Accept));
        found.iter().map(|d| d.at_ms).collect()
    };
    assert_eq!(at(0, 10), [120_000, 60_000]);
    assert_eq!(at(0, 1), [120_000]);
    assert_eq!(at(100_000, 10), [120_000]);
    assert!(at(130_000, 10).is_empty());
    let rows = store.spend_since("router", 90_000).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(store.spend_since("router", 90_001).unwrap().is_empty());
    let pending = store.pending_effects().unwrap();
    assert!(pending.iter().all(|q| !q.done));
    assert_eq!(
        pending.len(),
        store.effects().unwrap().iter().filter(|q| !q.done).count()
    );
}
