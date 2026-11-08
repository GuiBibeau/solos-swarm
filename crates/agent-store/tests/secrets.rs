mod common;

use common::{SqlEngine, TempDb, clock, policy};
use sha2::{Digest as _, Sha256};
use solos_agent_core::Engine;
use solos_agent_core::model::{
    Ask, DomainEvent, Effect, MessageKind, SecretGrant, SecretId, Ticket, TicketId,
};
use solos_agent_store::SqliteStore;
use std::cell::Cell;
use std::rc::Rc;

const V4_TICKET: &str = concat!(
    r#"{"id":"tk_1","title":"Read notifications","origin":"t1","decision":"dc_1","#,
    r#""mandate":{"policy_version":"pv_1","scope":"Read my notifications","strength":"explicit","#,
    r#""rules":[],"requester":{"id":"alice","kind":"person"},"source":"slack","budget_microusd":3000000},"#,
    r#""plan":{"kind":"session"},"#,
    r#""channels":[{"source":"slack","address":{"channel":"C1","thread_ts":"th1"}}],"#,
    r#""threads":["th1"],"#,
    r#""inputs":[{"id":"in_1","seq":1,"body":{"kind":"trigger","trigger":"t1"},"from_allowed_person":true,"at_ms":1000,"sent_to":"as_1"}],"#,
    r#""assignments":[{"id":"as_1","state":"running","created_ms":1000,"reports":[]}],"#,
    r#""approval":{"state":"not_needed"},"created_ms":1000,"seq":1,"handed_back":[]}"#,
);
const CHANNEL: &str = r#"{"source":"slack","address":{"channel":"C1","thread_ts":"th1"}}"#;
const V4_SEND: &str = concat!(
    r#"{"effect":"send","id":"ms_1","channel":{"source":"slack","address":{"channel":"C1","thread_ts":"th1"}},"#,
    r#""kind":"acknowledgement","text":"On it: Read notifications.","ticket":"tk_1"}"#,
);
const V4_EVENTS: [&str; 3] = [
    r#"{"at_ms":1000,"type":"ticket_created","ticket":"tk_1","decision":"dc_1","plan":{"kind":"session"},"budget_microusd":3000000}"#,
    r#"{"at_ms":1000,"type":"assignment_started","ticket":"tk_1","assignment":"as_1"}"#,
    r#"{"at_ms":1100,"type":"reported","ticket":"tk_1","assignment":"as_1","status":"progress"}"#,
];

fn a_v4_file(db: &TempDb) {
    drop(SqliteStore::open(&db.path).unwrap());
    let raw = db.raw();
    raw.execute("DELETE FROM schema_version WHERE version > 4", [])
        .unwrap();
    raw.execute(
        "INSERT INTO tickets (id, open, body) VALUES ('tk_1', 1, ?1)",
        [V4_TICKET],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO effects (lane, body) VALUES (?1, ?2)",
        [CHANNEL, V4_SEND],
    )
    .unwrap();
    let mut prev = "0".repeat(64);
    for body in V4_EVENTS {
        let hash = format!("{:x}", Sha256::digest(format!("{prev}{body}").as_bytes()));
        raw.execute(
            "INSERT INTO events (body, prev_hash, hash) VALUES (?1, ?2, ?3)",
            [body, prev.as_str(), hash.as_str()],
        )
        .unwrap();
        prev = hash;
    }
}

fn head(db: &TempDb) -> String {
    let raw = db.raw();
    raw.query_row(
        "SELECT hash FROM events ORDER BY seq DESC LIMIT 1",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn a_v4_file_opens_at_the_latest_version_and_reads_its_ticket_send_and_events_back_unchanged() {
    let db = TempDb::new("v4-file");
    a_v4_file(&db);
    assert_eq!(
        db.raw()
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        4
    );
    let old_head = head(&db);

    let store = SqliteStore::open(&db.path).unwrap();

    assert_eq!(store.schema_version().unwrap(), 10);
    let ticket = store.read_ticket(&TicketId::new("tk_1")).unwrap().unwrap();
    assert!(ticket.secrets.is_empty());
    assert_eq!(
        ticket.mandate.repo, None,
        "a ticket from before repositories has none"
    );
    assert_eq!(ticket.title, "Read notifications");
    assert_eq!(serde_json::to_string(&ticket).unwrap(), V4_TICKET);
    let effects = store.effects().unwrap();
    let [queued] = &effects[..] else {
        panic!("one Effect: {effects:?}")
    };
    let Effect::Send(message) = &queued.effect else {
        panic!("{:?}", queued.effect)
    };
    assert_eq!(message.ask, None);
    assert_eq!(message.text, "On it: Read notifications.");
    assert_eq!(serde_json::to_string(&queued.effect).unwrap(), V4_SEND);
    assert!(!queued.done);
    let chain = store.verify_chain().unwrap();
    assert_eq!((chain.len, chain.head), (3, old_head));
    let events: Vec<String> = store
        .events()
        .unwrap()
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect();
    assert_eq!(events, V4_EVENTS);
}

#[test]
fn a_ticket_from_a_v4_file_takes_a_grant_and_the_chain_still_verifies() {
    let db = TempDb::new("v4-grant");
    a_v4_file(&db);
    let now = Rc::new(Cell::new(2_000));
    let store = SqliteStore::open(&db.path).unwrap();
    let mut engine: SqlEngine = Engine::new(store, policy(), clock(&now));
    let id = TicketId::new("tk_1");
    let secret = SecretId::try_from("local/github-gui".to_owned()).unwrap();

    let grant = engine
        .ask_secret(&id, &secret, "sign in to https://github.com as gui", &[])
        .unwrap();
    drop(engine);

    let store = SqliteStore::open(&db.path).unwrap();
    let ticket: Ticket = store.read_ticket(&id).unwrap().unwrap();
    assert_eq!(ticket.secrets.get(&secret), Some(&grant));
    assert!(matches!(
        grant,
        SecretGrant::Waiting {
            asked_ms: 2_000,
            ..
        }
    ));
    assert_eq!(ticket.title, "Read notifications");
    assert_eq!(
        ticket.inputs.len(),
        1,
        "the old Input is still the only one"
    );
    let asks: Vec<_> = store
        .effects()
        .unwrap()
        .into_iter()
        .filter_map(|q| match q.effect {
            Effect::Send(m) if m.ask.is_some() => Some(m),
            _ => None,
        })
        .collect();
    let [ask] = &asks[..] else {
        panic!("one ask: {asks:?}")
    };
    assert_eq!(ask.kind, MessageKind::Attention);
    assert_eq!(ask.ask, Some(Ask::Secret { secret }));
    let events = store.events().unwrap();
    assert!(matches!(
        events.last().map(|e| &e.event),
        Some(DomainEvent::SecretAsked { .. })
    ));
    let chain = store.verify_chain().unwrap();
    assert_eq!(chain.len, 4, "the new event extends the old chain");
}
