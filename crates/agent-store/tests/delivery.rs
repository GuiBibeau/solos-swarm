//! Schema 7 holds a Ticket's `Change`, a Mandate's forge terms, the forge's
//! Effect and the delivery events. A file from before it reads back byte for
//! byte, and one with a delivery in it survives a reopen.

mod common;

use common::{SqlEngine, TempDb, channel, clock, policy, slack};
use sha2::{Digest as _, Sha256};
use solos_agent_core::Engine;
use solos_agent_core::change::{
    Checks, CommitSha, Deliverable, ForgeTerms, Harvest, MergeMode, PrRef, RepoName,
};
use solos_agent_core::delivery::{ForgeJob, Observed};
use solos_agent_core::model::{
    Acknowledge, Actor, ChannelRef, DomainEvent, Effect, Method, Plan, ReportIn, ReportStatus,
    Stage, ThreadKey, TicketId, Verdict,
};
use solos_agent_store::SqliteStore;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// A Ticket as schema 6 wrote it: a repository and a pull request owed, and
/// no `change`, no forge terms.
const V6_TICKET: &str = concat!(
    r#"{"id":"tk_1","title":"Add --json","origin":"t1","decision":"dc_1","#,
    r#""mandate":{"policy_version":"pv_1","scope":"Add --json to status","strength":"explicit","#,
    r#""rules":[],"requester":{"id":"alice","kind":"person"},"source":"slack","#,
    r#""budget_microusd":500000,"repo":"acme/widgets"},"#,
    r#""plan":{"kind":"session","method":{"deliverable":"pull_request"}},"#,
    r#""channels":[{"source":"slack","address":{"channel":"C1","thread_ts":"th1"}}],"#,
    r#""threads":["th1"],"#,
    r#""inputs":[{"id":"in_1","seq":1,"body":{"kind":"trigger","trigger":"t1"},"from_allowed_person":true,"at_ms":1000,"sent_to":"as_1"}],"#,
    r#""assignments":[{"id":"as_1","state":"running","created_ms":1000,"reports":[]}],"#,
    r#""approval":{"state":"not_needed"},"created_ms":1000,"seq":1,"handed_back":[]}"#,
);
const V6_SEND: &str = concat!(
    r#"{"effect":"send","id":"ms_1","channel":{"source":"slack","address":{"channel":"C1","thread_ts":"th1"}},"#,
    r#""kind":"acknowledgement","text":"On it: Add --json.","ticket":"tk_1"}"#,
);
const V6_EVENTS: [&str; 2] = [
    r#"{"at_ms":1000,"type":"ticket_created","ticket":"tk_1","decision":"dc_1","plan":{"kind":"session","method":{"deliverable":"pull_request"}},"budget_microusd":500000}"#,
    r#"{"at_ms":1000,"type":"assignment_started","ticket":"tk_1","assignment":"as_1"}"#,
];

fn a_v6_file(db: &TempDb) {
    drop(SqliteStore::open(&db.path).unwrap());
    let raw = db.raw();
    raw.execute("DELETE FROM schema_version WHERE version > 6", [])
        .unwrap();
    raw.execute(
        "INSERT INTO tickets (id, open, body) VALUES ('tk_1', 1, ?1)",
        [V6_TICKET],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO effects (lane, body) VALUES (?1, ?2)",
        [
            r#"{"source":"slack","address":{"channel":"C1","thread_ts":"th1"}}"#,
            V6_SEND,
        ],
    )
    .unwrap();
    let mut prev = "0".repeat(64);
    for body in V6_EVENTS {
        let hash = format!("{:x}", Sha256::digest(format!("{prev}{body}").as_bytes()));
        raw.execute(
            "INSERT INTO events (body, prev_hash, hash) VALUES (?1, ?2, ?3)",
            [body, prev.as_str(), hash.as_str()],
        )
        .unwrap();
        prev = hash;
    }
}

fn sha(digit: char) -> CommitSha {
    CommitSha::try_from(digit.to_string().repeat(40)).unwrap()
}

#[test]
fn a_v6_file_opens_at_the_latest_version_and_reads_its_ticket_effect_and_events_back_unchanged() {
    let db = TempDb::new("v6-file");
    a_v6_file(&db);
    let version = |db: &TempDb| {
        db.raw()
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| {
                r.get::<_, u32>(0)
            })
            .unwrap()
    };
    assert_eq!(version(&db), 6);

    let store = SqliteStore::open(&db.path).unwrap();

    assert_eq!(store.schema_version().unwrap(), 11);
    let ticket = store.read_ticket(&TicketId::new("tk_1")).unwrap().unwrap();
    assert_eq!(
        (ticket.change.clone(), ticket.mandate.forge.clone()),
        (None, None)
    );
    assert_eq!(
        serde_json::to_string(&ticket).unwrap(),
        V6_TICKET,
        "a ticket from before delivery writes back unchanged"
    );
    let queued = store.effects().unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(serde_json::to_string(&queued[0].effect).unwrap(), V6_SEND);
    let events: Vec<String> = store
        .events()
        .unwrap()
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect();
    assert_eq!(events, V6_EVENTS);
    assert_eq!(store.verify_chain().unwrap().len, 2);
    assert_eq!(version(&db), 11, "the migration is recorded once");
}

#[test]
fn a_ticket_from_a_v6_file_ends_on_ready_as_it_did_and_the_chain_still_verifies() {
    let db = TempDb::new("v6-ready");
    a_v6_file(&db);
    let now = Rc::new(Cell::new(2_000));
    let store = SqliteStore::open(&db.path).unwrap();
    let mut engine: SqlEngine = Engine::new(store, policy(), clock(&now));
    let id = TicketId::new("tk_1");
    let assignment = solos_agent_core::model::AssignmentId::new("as_1");

    engine
        .report(
            &id,
            &assignment,
            ReportIn {
                status: ReportStatus::Ready,
                text: "Done.".into(),
                question: None,
                spent_microusd: 0,
                harvest: None,
            },
        )
        .unwrap();
    drop(engine);

    let store = SqliteStore::open(&db.path).unwrap();
    let ticket = store.read_ticket(&id).unwrap().unwrap();
    assert!(ticket.ended.is_some(), "no forge terms: nothing is owed");
    assert!(
        store
            .effects()
            .unwrap()
            .iter()
            .all(|q| !matches!(q.effect, Effect::Forge(_)))
    );
    assert!(store.verify_chain().unwrap().len > 2);
}

#[test]
fn a_delivery_is_kept_across_a_reopen_with_its_effect_in_a_lane_of_its_own() {
    let db = TempDb::new("delivery");
    let now = Rc::new(Cell::new(1_000));
    let store = SqliteStore::open(&db.path).unwrap();
    let mut engine: SqlEngine = Engine::new(store, policy(), clock(&now));
    engine
        .receive(slack("t1", Actor::person("alice"), "th1", "add --json"))
        .unwrap();
    let eval = engine.evaluate_due().unwrap().remove(0);
    let widgets = RepoName::try_from("acme/widgets".to_owned()).unwrap();
    let Verdict::Accept(mut accept) = common::accept(
        "Add --json",
        Plan::Session {
            harness: None,
            environment: None,
            method: Some(Method {
                deliverable: Deliverable::PullRequest,
            }),
        },
        500_000,
    ) else {
        unreachable!()
    };
    accept.repo = Some(widgets.clone());
    accept.acknowledge = Acknowledge::Silent;
    accept.forge = Some(ForgeTerms {
        merge: MergeMode::Ask,
        ci_expected: true,
        max_pushes: 6,
        max_repairs: 2,
    });
    engine.decide(&eval, Verdict::Accept(accept)).unwrap();
    let ticket = engine.repos().tickets().unwrap().remove(0);
    let assignment = ticket.assignments[0].id.clone();
    engine.assignment_started(&ticket.id, &assignment).unwrap();

    engine
        .report(
            &ticket.id,
            &assignment,
            ReportIn {
                status: ReportStatus::Ready,
                text: "Added it.".into(),
                question: None,
                spent_microusd: 0,
                harvest: Some(Harvest {
                    head: sha('a'),
                    base: "main".into(),
                    commits: 1,
                    files: 2,
                    bytes: 99,
                }),
            },
        )
        .unwrap();
    drop(engine);

    let store = SqliteStore::open(&db.path).unwrap();
    let read = store.read_ticket(&ticket.id).unwrap().unwrap();
    assert_eq!(read.stage(), Stage::Delivering);
    assert_eq!(read.mandate.forge.as_ref().map(|t| t.max_pushes), Some(6));
    let change = read.change.clone().unwrap();
    assert_eq!(change.head, sha('a'));
    let queued: Vec<_> = store
        .pending_effects()
        .unwrap()
        .into_iter()
        .filter(|q| matches!(q.effect, Effect::Forge(_)))
        .collect();
    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued[0].effect,
        Effect::Forge(ForgeJob::Publish {
            ticket: ticket.id.clone(),
            head: sha('a')
        })
    );
    let raw = db.raw();
    let mut lanes = raw
        .prepare("SELECT lane FROM effects WHERE lane LIKE 'forge:%'")
        .unwrap();
    let lanes: Vec<String> = lanes
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(lanes, [format!("forge:{}", ticket.id)]);

    // The host reports the pull request it read back; the Ticket ends.
    let now = Rc::new(Cell::new(3_000));
    let mut engine: SqlEngine = Engine::new(store, policy(), clock(&now));
    engine
        .forge(Observed::Published {
            ticket: ticket.id.clone(),
            head: sha('a'),
            pr: PrRef {
                repo: widgets,
                number: 75,
            },
            url: "https://github.com/acme/widgets/pull/75".into(),
            thread: ThreadKey::new("github:acme/widgets#75"),
            reply_to: ChannelRef {
                source: "github".into(),
                address: BTreeMap::from([
                    ("repo".to_owned(), "acme/widgets".to_owned()),
                    ("number".to_owned(), "75".to_owned()),
                ]),
            },
            created: true,
        })
        .unwrap();
    drop(engine);

    let store = SqliteStore::open(&db.path).unwrap();
    let done = store.read_ticket(&ticket.id).unwrap().unwrap();
    assert!(done.ended.is_some());
    let change = done.change.unwrap();
    assert_eq!(change.delivered.as_ref().map(|d| d.pr.number), Some(75));
    assert_eq!(change.checks, Checks::Pending { reported: 0 });
    let kinds: Vec<&'static str> = store
        .events()
        .unwrap()
        .iter()
        .filter_map(|e| match e.event {
            DomainEvent::ProposalAdmitted { .. } => Some("admitted"),
            DomainEvent::Delivered { .. } => Some("delivered"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["admitted", "delivered"]);
    let chain = store.verify_chain().unwrap();
    assert!(chain.len >= 2, "the chain with the new events verifies");
    assert_eq!(channel("th1").source, "slack");
}
