mod common;

use common::TempDb;
use solos_agent_store::{
    Charge, HoldId, HoldRequest, Reservation, SpendState, SqliteStore, StoreError, Tokens,
};

const DAY: u64 = 86_400_000;

fn request(account: &str, cap: u64, hold: u64, at_ms: u64) -> HoldRequest<'_> {
    HoldRequest {
        account,
        provider: "deepseek",
        since_ms: at_ms - at_ms % DAY,
        cap_microusd: Some(cap),
        hold_microusd: hold,
        at_ms,
    }
}

fn held(store: &mut SqliteStore, request: &HoldRequest<'_>) -> HoldId {
    match store.reserve(request).unwrap() {
        Reservation::Held(id) => id,
        refused => panic!("refused: {refused:?}"),
    }
}

const TOKENS: Tokens = Tokens {
    input: 100,
    cached_input: 90,
    output: 50,
};

#[test]
fn an_account_with_no_cap_is_held_and_recorded_but_never_refused() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let now = 10 * DAY + 5;
    let uncapped = |hold, at_ms| HoldRequest {
        cap_microusd: None,
        ..request("ticket:tk_1", 0, hold, at_ms)
    };
    let first = held(&mut store, &uncapped(4_000_000_000, now));
    let second = held(&mut store, &uncapped(4_000_000_000, now));
    assert_eq!(
        store.committed("ticket:tk_1", 0).unwrap(),
        8_000_000_000,
        "open holds count at their hold"
    );
    store
        .settle(first, 5_000_000_000, TOKENS, "receipt-1", now + 1)
        .unwrap();
    store.release(second, now + 2).unwrap();
    assert_eq!(
        store.committed("ticket:tk_1", 0).unwrap(),
        5_000_000_000,
        "a settlement above its hold is kept as billed"
    );
    let rows = store.spend("ticket:tk_1").unwrap();
    let states: Vec<_> = rows.iter().map(|r| (r.state, r.cost_microusd)).collect();
    assert_eq!(
        states,
        [
            (SpendState::Settled, 5_000_000_000),
            (SpendState::Released, 0)
        ]
    );
}

#[test]
fn open_holds_count_at_their_hold_and_closed_ones_at_their_cost() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let now = 10 * DAY + 5;
    let first = held(&mut store, &request("router", 1_000, 400, now));
    let second = held(&mut store, &request("router", 1_000, 400, now));
    assert_eq!(store.committed("router", now - 5).unwrap(), 800);
    assert_eq!(
        store.reserve(&request("router", 1_000, 400, now)).unwrap(),
        Reservation::Refused { committed: 800 },
        "a third hold would pass the cap"
    );

    store
        .settle(first, 64, TOKENS, "receipt-1", now + 1)
        .unwrap();
    store.release(second, now + 2).unwrap();
    assert_eq!(store.committed("router", now - 5).unwrap(), 64);
    let third = held(&mut store, &request("router", 1_000, 400, now + 3));
    store.charge_hold(third, now + 4).unwrap();
    assert_eq!(store.committed("router", now - 5).unwrap(), 464);

    let rows = store.spend("router").unwrap();
    let states: Vec<_> = rows.iter().map(|r| (r.state, r.cost_microusd)).collect();
    assert_eq!(
        states,
        [
            (SpendState::Settled, 64),
            (SpendState::Released, 0),
            (SpendState::Unknown, 400)
        ]
    );
    assert_eq!(rows[0].tokens, Some(TOKENS));
    assert_eq!(rows[0].request_id.as_deref(), Some("receipt-1"));
    assert_eq!(rows[0].closed_ms, Some(now + 1));
    assert_eq!(rows[1].tokens, None);
    assert_eq!(store.hold(first).unwrap().as_ref(), rows.first());
}

#[test]
fn a_hold_closes_once() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let id = held(&mut store, &request("router", 1_000, 400, DAY));
    store.settle(id, 64, TOKENS, "receipt-1", DAY).unwrap();
    for again in [
        store.settle(id, 1, TOKENS, "receipt-2", DAY),
        store.release(id, DAY),
        store.charge_hold(id, DAY),
        store.release(HoldId(99), DAY),
    ] {
        assert!(matches!(again, Err(StoreError::NotHeld(_))), "{again:?}");
    }
    assert_eq!(store.committed("router", 0).unwrap(), 64);
}

#[test]
fn accounts_and_days_are_counted_apart() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let today = 20 * DAY + 1_000;
    let yesterday = today - DAY;
    let old = held(&mut store, &request("router", 500, 400, yesterday));
    store.settle(old, 400, TOKENS, "r-old", yesterday).unwrap();
    held(&mut store, &request("router", 500, 400, today));
    held(&mut store, &request("ticket:tk_1", 500, 400, today));
    assert_eq!(store.committed("router", today - today % DAY).unwrap(), 400);
    assert_eq!(store.committed("router", 0).unwrap(), 800);
    assert_eq!(store.committed("ticket:tk_1", 0).unwrap(), 400);
    assert_eq!(store.committed("ticket:tk_2", 0).unwrap(), 0);
    assert!(matches!(
        store
            .reserve(&request("ticket:tk_1", 500, 101, today))
            .unwrap(),
        Reservation::Refused { committed: 400 }
    ));
}

#[test]
fn a_charge_counts_once_per_request_id_against_its_accounts_cap() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let now = 20 * DAY;
    let run = |cost, request_id| Charge {
        account: "ticket:tk_1",
        provider: "deepseek",
        cost_microusd: cost,
        request_id,
        at_ms: now,
    };
    assert!(store.charge(&run(600, "swarm:as_1")).unwrap());
    assert!(
        !store.charge(&run(600, "swarm:as_1")).unwrap(),
        "a repeat charges nothing"
    );
    assert!(store.charge(&run(0, "swarm:as_2")).unwrap());
    assert_eq!(store.committed("ticket:tk_1", 0).unwrap(), 600);
    assert_eq!(store.committed("ticket:tk_2", 0).unwrap(), 0);
    let rows = store.spend("ticket:tk_1").unwrap();
    let rows: Vec<_> = rows
        .iter()
        .map(|r| {
            (
                r.state,
                r.cost_microusd,
                r.request_id.as_deref(),
                r.closed_ms,
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (SpendState::Settled, 600, Some("swarm:as_1"), Some(now)),
            (SpendState::Settled, 0, Some("swarm:as_2"), Some(now)),
        ]
    );
    let after = request("ticket:tk_1", 1_000, 401, now);
    assert_eq!(
        store
            .reserve(&HoldRequest {
                since_ms: 0,
                ..after
            })
            .unwrap(),
        Reservation::Refused { committed: 600 },
        "a worker cannot spend what the swarm spent"
    );
}

#[test]
fn a_reopened_file_keeps_every_cap_and_charges_holds_left_open() {
    let db = TempDb::new("ledger");
    let now = 30 * DAY;
    {
        let mut store = SqliteStore::open(&db.path).unwrap();
        let settled = held(&mut store, &request("router", 1_000, 400, now));
        store.settle(settled, 300, TOKENS, "r1", now).unwrap();
        held(&mut store, &request("router", 1_000, 400, now));
    }

    let mut store = SqliteStore::open(&db.path).unwrap();
    assert_eq!(store.committed("router", now).unwrap(), 700);
    assert_eq!(store.close_open_holds(now + 1).unwrap(), 1);
    assert_eq!(store.close_open_holds(now + 2).unwrap(), 0);
    assert_eq!(store.committed("router", now).unwrap(), 700);
    assert_eq!(
        store.reserve(&request("router", 1_000, 400, now)).unwrap(),
        Reservation::Refused { committed: 700 },
        "a restart did not reopen the cap"
    );
    let states: Vec<_> = store
        .spend("router")
        .unwrap()
        .iter()
        .map(|r| r.state)
        .collect();
    assert_eq!(states, [SpendState::Settled, SpendState::Unknown]);
}

#[test]
fn a_closed_row_cannot_change_and_no_row_can_be_deleted() {
    let db = TempDb::new("ledger-final");
    let mut store = SqliteStore::open(&db.path).unwrap();
    let id = held(&mut store, &request("router", 1_000, 400, DAY));
    store.settle(id, 64, TOKENS, "r1", DAY).unwrap();
    let raw = db.raw();
    let update = raw.execute("UPDATE spend SET cost_microusd = 0 WHERE id = ?1", [id.0]);
    assert!(update.is_err(), "a settled row was rewritten");
    let delete = raw.execute("DELETE FROM spend", []);
    assert!(delete.is_err(), "spend was deleted");
    assert_eq!(store.committed("router", 0).unwrap(), 64);
}
