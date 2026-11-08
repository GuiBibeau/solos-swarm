//! `agent publish`: the snapshots of a real Store made by the scripted agent,
//! the hash that decides what is uploaded, and the Blob upload itself against
//! a fake of Vercel's API that enforces what `@vercel/blob@2.8.2` sends.

use anyhow::Result;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::response::Response;
use serde_json::{Value, json};
use solos_agent_core::AllowlistPolicy;
use solos_agent_core::model::{
    Actor, Addressed, AssignmentId, ChannelRef, ReportIn, ReportStatus, ThreadKey, TicketId,
    Trigger, TriggerId,
};
use solos_agent_core::trail::{
    Digest, PageAction, PageSeen, STEP_VERSION, Signature, SignedStep, StepBody, StepOutcome,
    TrailAction, TrailKey, TrailOutcome,
};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::external_jobs::{
    FORMAT as JOBS_FORMAT, Record, State as JobsState, Status,
};
use solos_swarm::agent::jsonl::{JsonlChannel, JsonlSource};
use solos_swarm::agent::ports::{BoxFuture, Channel, Delivery, Executor, Fact, FactSink, Launch};
use solos_swarm::agent::publish::{
    API_VERSION, Backoff, BlobClient, BlobToken, Built, CycleError, INDEX_PATH, JOBS_PATH, Options,
    Publisher, PutError, STATE_FILE, Source, ticket_path,
};
use solos_swarm::agent::scripted::{ScriptedExecutor, ScriptedRouter};
use solos_swarm::agent::{Adapters, RunOptions, Runtime, STORE_FILE};
use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

const LIMIT: Duration = Duration::from_secs(30);
const TOKEN: &str = "vercel_blob_rw_Store123_secretsecretsecret";

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agent-publish-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        Self { root }
    }
    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    fn db(&self) -> PathBuf {
        self.state().join(STORE_FILE)
    }
    fn append(&self, triggers: &[Trigger]) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("triggers.jsonl"))
            .unwrap();
        for trigger in triggers {
            writeln!(file, "{}", serde_json::to_string(trigger).unwrap()).unwrap();
        }
    }
    fn outbox(&self) -> PathBuf {
        self.root.join("outbox.jsonl")
    }
    fn sent(&self) -> Vec<Value> {
        std::fs::read_to_string(self.outbox())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn runtime(&self, executor: Arc<dyn Executor>, until_idle: bool) -> Runtime {
        let channel: Arc<dyn Channel> = Arc::new(JsonlChannel::open(self.outbox()).unwrap());
        let adapters = Adapters {
            sources: vec![Box::new(JsonlSource::new(
                self.root.join("triggers.jsonl"),
                false,
            ))],
            channels: BTreeMap::from([("slack".to_owned(), channel)]),
            router: Arc::new(ScriptedRouter::new(500_000)),
            executor,
        };
        let options = RunOptions {
            until_idle,
            tick: Duration::from_millis(10),
            retry_base: Duration::from_millis(50),
            retry_max: Duration::from_secs(1),
            remind_after: None,
        };
        Runtime::new(
            SqliteStore::open(self.db()).unwrap(),
            AllowlistPolicy {
                people: ["slack:alice".to_owned()].into(),
                bots: ["slack:deploy-bot".to_owned()].into(),
                approvers: ["slack:alice".to_owned()].into(),
                ..AllowlistPolicy::default()
            },
            adapters,
            options,
        )
    }
    /// Read the triggers once and finish everything they started.
    async fn serve(&self, executor: Arc<dyn Executor>) {
        tokio::time::timeout(
            LIMIT,
            self.runtime(executor, true).run(std::future::pending()),
        )
        .await
        .expect("the agent did not go idle")
        .unwrap();
    }
    /// A Source on this scratch's Store.
    fn source(&self) -> Source {
        Source::open(&self.state()).unwrap()
    }
    fn publisher(&self, fake: &Fake) -> Publisher {
        self.publisher_with(fake, TOKEN, Duration::from_secs(300))
    }
    fn publisher_with(&self, fake: &Fake, token: &str, heartbeat: Duration) -> Publisher {
        let options = Options {
            every: Duration::from_millis(50),
            heartbeat,
            api_url: fake.url(),
            backoff: fast(),
        };
        Publisher::new(&self.state(), BlobToken::parse(token).unwrap(), options).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fast() -> Backoff {
    Backoff {
        attempts: 3,
        base: Duration::from_millis(1),
        max: Duration::from_millis(4),
    }
}

fn slack(id: &str, actor: Actor, thread: &str, text: &str) -> Trigger {
    Trigger {
        id: TriggerId::new(id),
        source: "slack".into(),
        at_ms: 1_000,
        actor,
        addressed: Addressed::Mention,
        text: text.into(),
        threads: vec![ThreadKey::new(thread)],
        reply_to: vec![ChannelRef {
            source: "slack".into(),
            address: BTreeMap::from([
                ("channel".to_owned(), "C1".to_owned()),
                ("thread_ts".to_owned(), thread.to_owned()),
            ]),
        }],
        meta: json!({}),
    }
}

fn request(id: &str, thread: &str, text: &str) -> Trigger {
    slack(id, Actor::person("alice"), thread, text)
}

fn value<T: serde::Serialize>(of: &T) -> Value {
    serde_json::to_value(of).unwrap()
}

/// With `PUBLISH_FIXTURE_DIR` set, write what was built as the files the dashboard reads, so its tests can render
/// real snapshots: `<name>-index.json` and `<name>-<n>.json`. From the repository root:
/// `PUBLISH_FIXTURE_DIR=web/agent-dashboard/tests/fixtures cargo test --test agent_publish -- fixtures`.
fn dump_fixtures(name: &str, built: &Built) {
    let Ok(dir) = std::env::var("PUBLISH_FIXTURE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut index = value(&built.index);
    index["generated_at"] = json!("2026-11-06T14:00:00Z");
    let write = |file: String, json: &Value| {
        std::fs::write(
            dir.join(file),
            serde_json::to_string_pretty(json).unwrap() + "\n",
        )
        .unwrap();
    };
    write(format!("{name}-index.json"), &index);
    for (n, (_, snapshot)) in built.tickets.iter().enumerate() {
        write(format!("{name}-{n}.json"), &value(snapshot));
    }
}

// -- a fake of Vercel's Blob API -------------------------------------------------------------

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    query: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

enum Scripted {
    Status(u16),
    Limited(u16, u64),
}

#[derive(Default)]
struct Shared {
    token: String,
    requests: Vec<Seen>,
    blobs: BTreeMap<String, Vec<u8>>,
    /// The pathnames that were put, in order.
    puts: Vec<String>,
    /// Answers given to the next requests instead of the normal ones.
    script: VecDeque<Scripted>,
}

struct Fake {
    addr: SocketAddr,
    shared: Arc<Mutex<Shared>>,
    task: JoinHandle<()>,
}

impl Fake {
    async fn start() -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            token: TOKEN.to_owned(),
            ..Shared::default()
        }));
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { addr, shared, task }
    }
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    fn script(&self, steps: impl IntoIterator<Item = Scripted>) {
        self.shared.lock().unwrap().script.extend(steps);
    }
    fn requests(&self) -> Vec<Seen> {
        self.shared.lock().unwrap().requests.clone()
    }
    fn puts(&self) -> Vec<String> {
        self.shared.lock().unwrap().puts.clone()
    }
    fn blob(&self, pathname: &str) -> Value {
        let shared = self.shared.lock().unwrap();
        let bytes = shared
            .blobs
            .get(pathname)
            .unwrap_or_else(|| panic!("no blob {pathname}: {:?}", shared.blobs.keys()));
        serde_json::from_slice(bytes).unwrap()
    }
    fn clear_puts(&self) {
        self.shared.lock().unwrap().puts.clear();
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn answer(status: u16, headers: &[(&str, String)], body: &Value) -> Response {
    let mut response = Response::builder()
        .status(status)
        .header("content-type", "application/json");
    for (name, value) in headers {
        response = response.header(*name, value);
    }
    response.body(Body::from(body.to_string())).unwrap()
}

fn error(status: u16, code: &str, message: &str) -> Response {
    answer(
        status,
        &[],
        &json!({"error": {"code": code, "message": message}}),
    )
}

/// What the real API does with a put, as far as `put()` can tell.
async fn handle(State(shared): State<Arc<Mutex<Shared>>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, usize::MAX).await.unwrap().to_vec();
    let headers: BTreeMap<String, String> = parts
        .headers
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                value.to_str().unwrap_or("<binary>").to_owned(),
            )
        })
        .collect();
    let query = parts.uri.query().unwrap_or_default().to_owned();
    let pathname = reqwest::Url::parse(&format!("http://fake/?{query}"))
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "pathname")
        .map(|(_, value)| value.into_owned());
    let mut shared = shared.lock().unwrap();
    shared.requests.push(Seen {
        method: parts.method.to_string(),
        query,
        headers: headers.clone(),
        body: body.clone(),
    });
    if let Some(scripted) = shared.script.pop_front() {
        return match scripted {
            Scripted::Status(status) => error(status, "service_unavailable", "try again"),
            Scripted::Limited(status, seconds) => answer(
                status,
                &[("retry-after", seconds.to_string())],
                &json!({"error": {"code": "rate_limited", "message": "slow down"}}),
            ),
        };
    }
    if headers.get("authorization") != Some(&format!("Bearer {}", shared.token)) {
        return error(403, "forbidden", "invalid token");
    }
    let Some(pathname) = pathname.filter(|_| parts.method == "PUT") else {
        return error(400, "bad_request", "a put needs a pathname");
    };
    if headers.get("x-api-version").map(String::as_str) != Some(API_VERSION)
        || headers.get("x-vercel-blob-access").map(String::as_str) != Some("private")
    {
        return error(400, "bad_request", "wrong api version or access");
    }
    let overwrite = headers.get("x-allow-overwrite").map(String::as_str) == Some("1");
    if shared.blobs.contains_key(&pathname) && !overwrite {
        return error(
            400,
            "bad_request",
            "This blob already exists, if you want to overwrite it use allowOverwrite: true",
        );
    }
    shared.blobs.insert(pathname.clone(), body);
    shared.puts.push(pathname.clone());
    let store = "Store123";
    answer(
        200,
        &[],
        &json!({
            "url": format!("https://{store}.private.blob.vercel-storage.com/{pathname}"),
            "downloadUrl": format!("https://{store}.private.blob.vercel-storage.com/{pathname}?download=1"),
            "pathname": pathname,
            "contentType": headers.get("x-content-type"),
            "contentDisposition": "inline",
            "etag": "\"abc\"",
        }),
    )
}

// -- the Blob client ---------------------------------------------------------------------------

fn client(fake: &Fake) -> BlobClient {
    BlobClient::new(&fake.url(), BlobToken::parse(TOKEN).unwrap(), fast()).unwrap()
}

#[tokio::test]
async fn a_put_sends_exactly_what_the_sdks_put_sends() {
    let fake = Fake::start().await;
    let put = client(&fake)
        .put(
            "tickets/tk_0123 abc.json",
            "application/json",
            b"{\"a\":1}".to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(put.pathname, "tickets/tk_0123 abc.json");
    assert!(put.url.contains("Store123.private.blob.vercel-storage.com"));

    let [seen] = &fake.requests()[..] else {
        panic!("one request")
    };
    assert_eq!(seen.method, "PUT");
    // URLSearchParams: the slash is %2F and the space is +.
    assert_eq!(seen.query, "pathname=tickets%2Ftk_0123+abc.json");
    assert_eq!(seen.body, b"{\"a\":1}");
    let header = |name: &str| seen.headers.get(name).map(String::as_str);
    assert_eq!(
        header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(header("x-api-version"), Some("13"));
    assert_eq!(header("x-vercel-blob-store-id"), Some("Store123"));
    assert_eq!(header("x-vercel-blob-access"), Some("private"));
    assert_eq!(header("x-add-random-suffix"), Some("0"));
    assert_eq!(header("x-allow-overwrite"), Some("1"));
    assert_eq!(header("x-content-type"), Some("application/json"));
    assert_eq!(header("x-cache-control-max-age"), Some("60"));
    assert_eq!(header("x-api-blob-request-attempt"), Some("0"));
    assert!(
        header("x-api-blob-request-id").is_some_and(|id| id.starts_with("Store123:")),
        "{:?}",
        seen.headers
    );
    assert_eq!(
        header("content-type"),
        None,
        "the body names no type of its own"
    );
}

#[tokio::test]
async fn a_second_put_overwrites_and_the_token_is_never_in_an_error() {
    let fake = Fake::start().await;
    let client = client(&fake);
    client
        .put("tickets/x.json", "application/json", b"1".to_vec())
        .await
        .unwrap();
    client
        .put("tickets/x.json", "application/json", b"2".to_vec())
        .await
        .unwrap();
    assert_eq!(fake.shared.lock().unwrap().blobs["tickets/x.json"], b"2");

    fake.shared.lock().unwrap().token = "vercel_blob_rw_Store123_other".into();
    let refused = client
        .put("tickets/x.json", "application/json", b"3".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(refused, PutError::Unauthorized(_)), "{refused:?}");
    assert!(!refused.to_string().contains("secretsecret"), "{refused}");
}

#[tokio::test]
async fn a_5xx_a_429_and_a_dropped_connection_are_retried_and_a_403_and_a_400_are_not() {
    let fake = Fake::start().await;
    let client = client(&fake);

    fake.script([Scripted::Status(503), Scripted::Status(500)]);
    client
        .put("a.json", "application/json", b"1".to_vec())
        .await
        .unwrap();
    let attempts: Vec<_> = fake
        .requests()
        .iter()
        .map(|seen| seen.headers["x-api-blob-request-attempt"].clone())
        .collect();
    assert_eq!(attempts, ["0", "1", "2"], "the same request, three tries");
    let ids: std::collections::BTreeSet<_> = fake
        .requests()
        .iter()
        .map(|seen| seen.headers["x-api-blob-request-id"].clone())
        .collect();
    assert_eq!(ids.len(), 1, "a retry keeps its request id");

    fake.script([Scripted::Limited(429, 0)]);
    client
        .put("b.json", "application/json", b"1".to_vec())
        .await
        .unwrap();

    let before = fake.requests().len();
    fake.script([
        Scripted::Status(503),
        Scripted::Status(503),
        Scripted::Status(503),
    ]);
    let spent = client
        .put("c.json", "application/json", b"1".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(spent, PutError::Transient(_)), "{spent:?}");
    assert_eq!(fake.requests().len() - before, 3, "three tries in all");

    let before = fake.requests().len();
    fake.script([Scripted::Status(403)]);
    let refused = client
        .put("d.json", "application/json", b"1".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(refused, PutError::Unauthorized(_)), "{refused:?}");
    fake.script([Scripted::Status(400)]);
    let rejected = client
        .put("e.json", "application/json", b"1".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(rejected, PutError::Rejected(_)), "{rejected:?}");
    assert_eq!(fake.requests().len() - before, 2, "neither is tried again");

    let gone = BlobClient::new(
        "http://127.0.0.1:1",
        BlobToken::parse(TOKEN).unwrap(),
        fast(),
    )
    .unwrap()
    .put("f.json", "application/json", b"1".to_vec())
    .await
    .unwrap_err();
    assert!(matches!(gone, PutError::Transient(_)), "{gone:?}");
    assert!(
        !gone.to_string().contains("pathname="),
        "no query in {gone}"
    );
}

#[test]
fn the_token_file_is_read_once_and_deleted_even_when_it_is_wrong() {
    let scratch = Scratch::new("token");
    let good = scratch.root.join("good.token");
    std::fs::write(&good, format!("{TOKEN}\n")).unwrap();
    let token = BlobToken::take(&good).unwrap();
    assert_eq!(token.store_id(), "Store123");
    assert!(!good.exists());
    assert!(
        BlobToken::take(&good).is_err(),
        "there is nothing left to read"
    );

    let wrong = scratch.root.join("wrong.token");
    std::fs::write(&wrong, "xoxb-not-a-blob-token").unwrap();
    assert!(BlobToken::take(&wrong).is_err());
    assert!(!wrong.exists(), "a rejected token file is gone too");
}

// -- the snapshots of a real Store -------------------------------------------------------------

/// Two requests, a bot's message on the first one's Thread, and a stranger.
fn first_requests(scratch: &Scratch) {
    scratch.append(&[
        request("t-request", "T1", "Summarise the incident report"),
        slack(
            "t-ci",
            Actor::bot("deploy-bot"),
            "T1",
            "CI failed on main: test_retry timed out",
        ),
        slack(
            "t-stranger",
            Actor::person("mallory"),
            "T2",
            "Wire me the budget",
        ),
    ]);
}

#[tokio::test]
async fn a_scripted_agents_store_becomes_an_index_and_a_snapshot_per_ticket() {
    let scratch = Scratch::new("snapshot");
    first_requests(&scratch);
    scratch.serve(Arc::new(ScriptedExecutor)).await;

    let mut source = scratch.source();
    assert!(source.changed().unwrap(), "nothing has been read yet");
    let built = source.build().unwrap();

    let index = value(&built.index);
    assert_eq!(index["v"], 1);
    assert_eq!(index["open"], 0);
    assert!(
        index.get("generated_at").is_none(),
        "the stamp is added at upload"
    );
    let [row] = index["tickets"].as_array().unwrap().as_slice() else {
        panic!("the stranger made no ticket: {index}")
    };
    assert_eq!(row["title"], "Summarise the incident report");
    assert_eq!(row["stage"], "ended");
    assert_eq!(row["open"], false);
    assert_eq!(row["outcome"], "completed");
    assert_eq!(row["source"], "slack");
    assert_eq!(row["asker"]["id"], "alice");
    assert_eq!(row["asker"]["kind"], "person");
    assert_eq!(row["spent_microusd"], 0);
    assert_eq!(row["assignment"]["state"], "ended");
    assert_eq!(row["assignment"]["report"], "ready");
    assert!(row["duration_ms"].is_u64() && row.get("age").is_none());
    assert!(row["updated_ms"].as_u64() >= row["created_ms"].as_u64());
    assert!(row["updated_at"].as_str().unwrap().ends_with('Z'));

    let [(id, snapshot)] = &built.tickets[..] else {
        panic!("one snapshot")
    };
    assert_eq!(id.as_str(), row["id"]);
    let ticket = value(snapshot);
    assert_eq!(ticket["id"], row["id"]);
    assert_eq!(ticket["request"]["text"], "Summarise the incident report");
    assert_eq!(ticket["request"]["from"], "person");
    assert_eq!(ticket["request"]["who"], "alice");
    assert_eq!(ticket["requester"]["id"], "alice");
    assert!(
        ticket["scope"]
            .as_str()
            .unwrap()
            .contains("incident report")
    );
    assert_eq!(ticket["plan"]["kind"], "session");
    assert_eq!(ticket["deliverable"], "answer");
    assert_eq!(ticket["approval"]["state"], "not_needed");
    assert_eq!(ticket["ended"]["outcome"], "completed");
    assert!(
        ticket.get("source_url").is_none(),
        "slack has no link the agent can make"
    );
    assert!(ticket.get("browser").is_none(), "no browser ran");

    // The conversation is the Ticket's own order: its acknowledgement, the
    // bot's message it received, then its result. Every agent reply is the
    // exact text the channel was given.
    let conversation = ticket["conversation"].as_array().unwrap();
    let shape: Vec<(&str, &str)> = conversation
        .iter()
        .map(|turn| {
            (
                turn["from"].as_str().unwrap(),
                turn["kind"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            ("agent", "acknowledgement"),
            ("bot", "message"),
            ("agent", "result")
        ]
    );
    assert_eq!(
        conversation[1]["text"],
        "CI failed on main: test_retry timed out"
    );
    let sent: Vec<String> = scratch
        .sent()
        .iter()
        .map(|message| message["text"].as_str().unwrap().to_owned())
        .collect();
    let said: Vec<&str> = conversation
        .iter()
        .filter(|turn| turn["from"] == "agent")
        .map(|turn| turn["text"].as_str().unwrap())
        .collect();
    assert_eq!(said, sent);
    for turn in conversation.iter().filter(|turn| turn["from"] == "agent") {
        assert_eq!(turn["channel"], "slack");
        assert_eq!(turn["delivery"]["delivered"], true);
        assert_eq!(turn["delivery"]["attempts"], 0);
    }

    let decisions = ticket["decisions"].as_array().unwrap();
    assert!(
        decisions.iter().any(|d| d["kind"] == "accept"),
        "{decisions:?}"
    );
    let [assignment] = ticket["assignments"].as_array().unwrap().as_slice() else {
        panic!("one assignment")
    };
    assert_eq!(assignment["state"], "ended");
    assert!(
        assignment["started_ms"].is_u64(),
        "from the event log: {assignment}"
    );
    assert!(assignment["ended_ms"].is_u64());
    let reports = assignment["reports"].as_array().unwrap();
    assert_eq!(assignment["report_count"], 1);
    assert_eq!(reports.last().unwrap()["status"], "ready");
    assert!(
        reports.last().unwrap()["text"]
            .as_str()
            .unwrap()
            .starts_with("Done: Summarise the incident report"),
    );
}

/// Starts every Assignment and says nothing more, as a worker still at it.
struct Working;

impl Executor for Working {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            facts
                .send(Fact::Started {
                    ticket: launch.ticket,
                    assignment: launch.assignment,
                })
                .await
        })
    }
    fn deliver(&self, _: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// Run the agent until its first Ticket is working and has been told, then
/// drop it, as a crash would.
async fn until_working(scratch: &Scratch) {
    let runtime = scratch.runtime(Arc::new(Working), false);
    let working = async {
        for _ in 0..1_000 {
            let store = SqliteStore::open_read_only(scratch.db());
            let running = store.is_ok_and(|store| {
                store
                    .tickets()
                    .unwrap()
                    .iter()
                    .any(|t| t.running_assignment().is_some())
            });
            if running && !scratch.sent().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the assignment never ran");
    };
    tokio::select! {
        ended = runtime.run(std::future::pending()) => panic!("the runtime ended: {ended:?}"),
        () = working => {}
    }
}

#[tokio::test]
async fn a_look_that_follows_the_logs_finds_what_a_fresh_look_finds() {
    let scratch = Scratch::new("incremental");
    scratch.append(&[request("t-request", "T1", "Fix the flaky test")]);
    until_working(&scratch).await;

    let mut following = scratch.source();
    let before = following.build().unwrap();
    let index = value(&before.index);
    assert_eq!(index["open"], 1);
    let row = &index["tickets"][0];
    assert_eq!(row["open"], true);
    assert_eq!(row["stage"], "working");
    assert_eq!(row["assignment"]["state"], "running");
    assert!(
        row.get("duration_ms").is_none(),
        "an open ticket has no duration yet"
    );
    assert!(
        !following.changed().unwrap(),
        "nothing was written since the build"
    );
    assert!(!following.changed().unwrap());

    // The agent restarts, finishes the work and answers a second request.
    scratch.append(&[request("t-second", "T4", "Rotate the staging keys")]);
    scratch.serve(Arc::new(ScriptedExecutor)).await;
    assert!(following.changed().unwrap(), "another connection committed");
    let after = following.build().unwrap();
    assert!(!following.changed().unwrap());

    let fresh = scratch.source().build().unwrap();
    assert_eq!(value(&after.index), value(&fresh.index));
    assert_eq!(after.tickets.len(), 2);
    for ((id, snapshot), (fresh_id, fresh_snapshot)) in after.tickets.iter().zip(&fresh.tickets) {
        assert_eq!(id, fresh_id);
        assert_eq!(value(snapshot), value(fresh_snapshot), "ticket {id}");
    }
    let index = value(&after.index);
    assert_eq!(index["open"], 0);
    let first = after
        .tickets
        .iter()
        .map(|(_, snapshot)| value(snapshot))
        .find(|snapshot| snapshot["title"] == "Fix the flaky test")
        .unwrap();
    assert_eq!(first["stage"], "ended");
    let kinds: Vec<&str> = first["conversation"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| turn["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["acknowledgement", "result"],
        "the ack once, then the result"
    );
}

/// Asks for one page whose address carries a token, and gets it.
struct Browsing;

impl Executor for Browsing {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let (ticket, assignment) = (launch.ticket, launch.assignment);
            facts
                .send(Fact::Started {
                    ticket: ticket.clone(),
                    assignment: assignment.clone(),
                })
                .await?;
            let open = TrailAction::Open {
                url: "https://example.com/reset?token=SECRETVALUE#frag".into(),
                key: None,
            };
            let click = TrailAction::Act {
                action: PageAction::Click,
                element: Some(3),
                text: None,
            };
            let signed = |n: u32, action: &TrailAction, url: &str, title: &str| {
                TrailOutcome::signed(SignedStep {
                    body: StepBody {
                        v: STEP_VERSION,
                        ticket: ticket.clone(),
                        assignment: assignment.clone(),
                        n,
                        environment: "local".into(),
                        session: "7a3c0f9e12d45b60".into(),
                        session_step: n - 1,
                        at_ms: 1_000 * u64::from(n),
                        provenance: None,
                        action: action.clone(),
                        outcome: StepOutcome::Page(PageSeen {
                            url: url.into(),
                            status: Some(200),
                            title: title.into(),
                            snapshot: Digest::of(b"snapshot"),
                            view: Digest::of(b"view"),
                            target: None,
                            filled: Vec::new(),
                            keyed: Vec::new(),
                        }),
                    },
                    signer: TrailKey::try_from(format!("ed25519:{}", "9c".repeat(32))).unwrap(),
                    signature: Signature::try_from("ab".repeat(64)).unwrap(),
                })
            };
            for (n, action, url, title) in [
                (
                    1,
                    &open,
                    "https://example.com/reset?token=SECRETVALUE#frag",
                    "Reset",
                ),
                (
                    2,
                    &click,
                    "https://example.com/done?session=SECRETVALUE",
                    "Done",
                ),
            ] {
                let ask = Fact::TrailAsk {
                    ticket: ticket.clone(),
                    assignment: assignment.clone(),
                    action: action.clone(),
                };
                facts.ask(ask).await.unwrap().unwrap();
                let done = Fact::TrailDone {
                    ticket: ticket.clone(),
                    assignment: assignment.clone(),
                    n,
                    outcome: signed(n, action, url, title),
                };
                facts.ask(done).await.unwrap().unwrap();
            }
            facts
                .send(Fact::Report {
                    ticket,
                    assignment,
                    report: ReportIn {
                        status: ReportStatus::Ready,
                        text: "done".into(),
                        question: None,
                        spent_microusd: 0,
                        harvest: None,
                    },
                })
                .await
        })
    }
    fn deliver(&self, _: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn the_browser_trail_names_pages_and_never_their_queries() {
    let scratch = Scratch::new("trail");
    scratch.append(&[request("t-request", "T1", "Read my notifications")]);
    scratch.serve(Arc::new(Browsing)).await;

    let built = scratch.source().build().unwrap();
    let ticket = value(&built.tickets[0].1);
    let browser = &ticket["browser"];
    assert_eq!(browser["steps"], 2);
    let pages: Vec<(&str, &str, &str)> = browser["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|page| {
            (
                page["action"].as_str().unwrap(),
                page["url"].as_str().unwrap(),
                page["title"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        pages,
        [
            ("open", "https://example.com/reset", "Reset"),
            ("click", "https://example.com/done", "Done"),
        ]
    );
    let everything = serde_json::to_string(&built.index).unwrap() + &ticket.to_string();
    assert!(
        !everything.contains("SECRETVALUE"),
        "no query, token or fragment leaves"
    );
}

/// Starts the Assignment and reports `status` with `text` (and `question`) at once.
struct Says {
    status: ReportStatus,
    text: &'static str,
    question: Option<&'static str>,
}

impl Executor for Says {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let (ticket, assignment) = (launch.ticket, launch.assignment);
            facts
                .send(Fact::Started {
                    ticket: ticket.clone(),
                    assignment: assignment.clone(),
                })
                .await?;
            facts
                .send(Fact::Report {
                    ticket,
                    assignment,
                    report: ReportIn {
                        status: self.status,
                        text: self.text.into(),
                        question: self.question.map(Into::into),
                        spent_microusd: 0,
                        harvest: None,
                    },
                })
                .await
        })
    }
    fn deliver(&self, _: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// Each request to the executor that suits its title.
struct Mixed {
    writing: Says,
    asking: Says,
    browsing: Browsing,
}

impl Executor for Mixed {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        if launch.title.contains("notifications") {
            self.browsing.launch(launch, facts)
        } else if launch.title.contains("staging") {
            self.asking.launch(launch, facts)
        } else {
            self.writing.launch(launch, facts)
        }
    }
    fn deliver(&self, _: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

const REPORT: &str = "## Incident summary\n\nThe **retry test** timed out because the fixture waited on a clock it \
never advanced.\n\n- cause: `Instant::now()` in the fixture\n- fix: [PR #7](https://example.com/acme/app/pull/7)\n\n\
| step | result |\n|---|---|\n| reproduce | yes |\n| fix | merged |\n\n```rust\nlet now = clock.now();\n```\n\n\
![chart](https://evil.example/leak?x=1)\n\n<script>alert(1)</script>";

/// A mix of tickets, for the dashboard's tests to render: a long markdown answer, a browsing ticket, and one that
/// waits for a person's answer.
#[tokio::test]
async fn a_realistic_mix_of_tickets_for_the_dashboards_fixtures() {
    let scratch = Scratch::new("fixtures");
    let mut asked = vec![
        request("t-one", "T1", "Summarise the incident report"),
        slack(
            "t-ci",
            Actor::bot("deploy-bot"),
            "T1",
            "CI failed on main: test_retry timed out",
        ),
        request("t-two", "T2", "Read my notifications"),
        request("t-three", "T3", "Rotate the staging keys"),
    ];
    for (n, trigger) in asked.iter_mut().enumerate() {
        trigger.at_ms = solos_swarm::broker::now_ms() - 60_000 + 1_000 * n as u64;
    }
    scratch.append(&asked);
    let executor = Mixed {
        writing: Says {
            status: ReportStatus::Ready,
            text: REPORT,
            question: None,
        },
        asking: Says {
            status: ReportStatus::Blocked,
            text: "I need to know which staging environment.",
            question: Some("Which staging environment should I rotate: eu or us?"),
        },
        browsing: Browsing,
    };
    scratch.serve(Arc::new(executor)).await;

    let built = scratch.source().build().unwrap();
    let index = value(&built.index);
    assert_eq!(index["open"], 1, "{index}");
    let stages: BTreeMap<&str, &str> = index["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["title"].as_str().unwrap(),
                row["stage"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(stages["Rotate the staging keys"], "answer");
    assert_eq!(stages["Summarise the incident report"], "ended");
    assert_eq!(
        index["tickets"][0]["waiting_for"], "Which staging environment should I rotate: eu or us?",
        "the open ticket is first, with what it waits for"
    );
    let asking = built
        .tickets
        .iter()
        .map(|(_, snapshot)| value(snapshot))
        .find(|snapshot| snapshot["title"] == "Rotate the staging keys")
        .unwrap();
    assert_eq!(
        asking["attention"],
        "Which staging environment should I rotate: eu or us?"
    );
    let kinds: Vec<&str> = asking["conversation"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| turn["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["acknowledgement", "attention"]);
    dump_fixtures("mix", &built);
}

// -- what is uploaded, and when ----------------------------------------------------------------

fn paths(puts: &[String]) -> Vec<&str> {
    puts.iter().map(String::as_str).collect()
}

fn ticket_id(index: &Value, title: &str) -> String {
    index["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["title"] == title)
        .unwrap_or_else(|| panic!("no {title}: {index}"))["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn the_first_look_uploads_every_ticket_then_the_index_and_the_second_uploads_nothing() {
    let scratch = Scratch::new("upload");
    first_requests(&scratch);
    scratch.serve(Arc::new(ScriptedExecutor)).await;
    let fake = Fake::start().await;
    let mut publisher = scratch.publisher(&fake);

    let report = publisher.cycle().await.unwrap();
    assert!(report.built);
    let index = fake.blob(INDEX_PATH);
    let id = ticket_id(&index, "Summarise the incident report");
    assert_eq!(report.uploaded, [ticket_path(&id), INDEX_PATH.to_owned()]);
    assert_eq!(
        paths(&fake.puts()),
        [ticket_path(&id).as_str(), INDEX_PATH],
        "the ticket first, so the index never names a blob that is not there"
    );
    assert!(
        index["generated_at"]
            .as_str()
            .is_some_and(|at| at.ends_with('Z')),
        "the uploaded index is stamped: {index}"
    );
    let snapshot = fake.blob(&ticket_path(&id));
    assert_eq!(snapshot["request"]["text"], "Summarise the incident report");
    let seen = fake.requests();
    assert!(seen.iter().all(|seen| seen.method == "PUT"));
    assert!(
        seen.iter()
            .all(|seen| seen.headers["x-content-type"] == "application/json"),
    );

    // Nothing changed: no build, no request.
    let requests = fake.requests().len();
    let quiet = publisher.cycle().await.unwrap();
    assert!(!quiet.built);
    assert!(quiet.uploaded.is_empty());
    assert_eq!(fake.requests().len(), requests);
}

#[tokio::test]
async fn a_restart_uploads_nothing_that_is_already_there_and_another_store_gets_everything() {
    let scratch = Scratch::new("restart");
    first_requests(&scratch);
    scratch.serve(Arc::new(ScriptedExecutor)).await;
    let fake = Fake::start().await;
    scratch.publisher(&fake).cycle().await.unwrap();
    assert_eq!(fake.puts().len(), 2);

    let state = scratch.state().join(STATE_FILE);
    let remembered: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    assert_eq!(remembered["store"], "Store123");
    assert_eq!(remembered["hashes"].as_object().unwrap().len(), 2);
    assert!(
        !remembered.to_string().contains("secretsecret"),
        "no token in the state"
    );
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&state).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    fake.clear_puts();
    let mut restarted = scratch.publisher(&fake);
    let report = restarted.cycle().await.unwrap();
    assert!(report.built, "a new process builds once");
    assert!(report.uploaded.is_empty(), "{report:?}");
    assert_eq!(report.unchanged, 2);
    assert!(fake.puts().is_empty());

    // Another store has none of it, whatever the state file says.
    fake.shared.lock().unwrap().token = "vercel_blob_rw_Other99_secretsecret".into();
    let mut elsewhere = scratch.publisher_with(
        &fake,
        "vercel_blob_rw_Other99_secretsecret",
        Duration::from_secs(300),
    );
    let report = elsewhere.cycle().await.unwrap();
    assert_eq!(report.uploaded.len(), 2, "{report:?}");
}

#[tokio::test]
async fn only_what_changed_goes_up_again() {
    let scratch = Scratch::new("changed");
    scratch.append(&[request("t-request", "T1", "Summarise the incident report")]);
    scratch.serve(Arc::new(ScriptedExecutor)).await;
    let fake = Fake::start().await;
    let mut publisher = scratch.publisher(&fake);
    publisher.cycle().await.unwrap();
    let first = ticket_id(&fake.blob(INDEX_PATH), "Summarise the incident report");
    fake.clear_puts();

    scratch.append(&[request("t-second", "T4", "Rotate the staging keys")]);
    scratch.serve(Arc::new(ScriptedExecutor)).await;
    let report = publisher.cycle().await.unwrap();
    assert!(report.built);
    let index = fake.blob(INDEX_PATH);
    let second = ticket_id(&index, "Rotate the staging keys");
    assert_eq!(
        report.uploaded,
        [ticket_path(&second), INDEX_PATH.to_owned()],
        "the first ticket's snapshot did not change, so it did not go up"
    );
    assert_eq!(report.unchanged, 1);
    assert_eq!(index["tickets"].as_array().unwrap().len(), 2);
    assert_ne!(first, second);
}

#[tokio::test]
async fn a_failed_upload_is_tried_again_with_the_store_unchanged_and_the_index_waits_for_its_ticket()
 {
    let scratch = Scratch::new("retry");
    scratch.append(&[request("t-request", "T1", "Summarise the incident report")]);
    scratch.serve(Arc::new(ScriptedExecutor)).await;
    let fake = Fake::start().await;
    let mut publisher = scratch.publisher(&fake);

    // Three tries of the ticket all fail, and the index is not tried at all.
    fake.script([
        Scripted::Status(503),
        Scripted::Status(503),
        Scripted::Status(502),
    ]);
    let failed = publisher.cycle().await.unwrap_err();
    assert!(matches!(failed, CycleError::Retry(_)), "{failed}");
    assert!(fake.puts().is_empty());
    assert_eq!(fake.requests().len(), 3);

    let report = publisher.cycle().await.unwrap();
    assert!(
        !report.built,
        "the store did not change; the queue did the work"
    );
    assert_eq!(report.uploaded.len(), 2);
    let puts = fake.puts();
    assert_eq!(puts.last().map(String::as_str), Some(INDEX_PATH));
}

#[tokio::test]
async fn a_refused_token_stops_the_publisher_and_a_refused_blob_does_not() {
    let scratch = Scratch::new("refused");
    scratch.append(&[request("t-request", "T1", "Summarise the incident report")]);
    scratch.serve(Arc::new(ScriptedExecutor)).await;
    let fake = Fake::start().await;

    let mut publisher = scratch.publisher(&fake);
    fake.script([Scripted::Status(403)]);
    let refused = publisher.cycle().await.unwrap_err();
    assert!(matches!(refused, CycleError::Fatal(_)), "{refused}");
    assert!(refused.to_string().contains("replace it"), "{refused}");
    assert!(!refused.to_string().contains("secretsecret"));

    // A 400 for the ticket's blob is reported and does not hold the index back.
    let mut publisher = scratch.publisher(&fake);
    fake.script([Scripted::Status(400)]);
    let report = publisher.cycle().await.unwrap();
    assert_eq!(report.rejected.len(), 1, "{report:?}");
    assert_eq!(report.uploaded, [INDEX_PATH.to_owned()]);
}

#[tokio::test]
async fn the_index_goes_up_again_after_a_heartbeat_and_nothing_else_does() {
    let scratch = Scratch::new("heartbeat");
    scratch.append(&[request("t-request", "T1", "Summarise the incident report")]);
    scratch.serve(Arc::new(ScriptedExecutor)).await;
    let fake = Fake::start().await;
    let mut publisher = scratch.publisher_with(&fake, TOKEN, Duration::from_millis(1_200));
    publisher.cycle().await.unwrap();
    let stamped = fake.blob(INDEX_PATH)["generated_at"].clone();
    fake.clear_puts();

    assert!(
        publisher.cycle().await.unwrap().uploaded.is_empty(),
        "not due yet"
    );
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    let report = publisher.cycle().await.unwrap();
    assert!(!report.built);
    assert_eq!(report.uploaded, [INDEX_PATH.to_owned()]);
    assert_ne!(
        fake.blob(INDEX_PATH)["generated_at"],
        stamped,
        "a new stamp says it is alive"
    );
}

#[tokio::test]
async fn a_restored_agent_db_is_a_new_file_and_is_read_again() {
    let scratch = Scratch::new("replaced");
    scratch.append(&[request("t-request", "T1", "Summarise the incident report")]);
    scratch.serve(Arc::new(ScriptedExecutor)).await;
    let fake = Fake::start().await;
    let mut publisher = scratch.publisher(&fake);
    publisher.cycle().await.unwrap();

    let copy = scratch.root.join("copy.db");
    std::fs::copy(scratch.db(), &copy).unwrap();
    std::fs::rename(&copy, scratch.db()).unwrap();
    fake.clear_puts();
    let report = publisher.cycle().await.unwrap();
    assert!(
        report.built,
        "the file under the path is not the one that was open"
    );
    assert!(
        report.uploaded.is_empty(),
        "same content, same hashes: {report:?}"
    );
}

// -- the Store is only read --------------------------------------------------------------------

#[tokio::test]
async fn publishing_leaves_the_store_byte_for_byte_alone_and_creates_nothing_when_it_is_missing() {
    let scratch = Scratch::new("read-only");
    first_requests(&scratch);
    scratch.serve(Arc::new(ScriptedExecutor)).await;
    let before = std::fs::read(scratch.db()).unwrap();
    let fake = Fake::start().await;

    let mut publisher = scratch.publisher(&fake);
    publisher.cycle().await.unwrap();
    publisher.cycle().await.unwrap();
    assert_eq!(std::fs::read(scratch.db()).unwrap(), before);

    // What Source opens with refuses a write, whoever asks.
    let reader = SqliteStore::open_read_only(scratch.db()).unwrap();
    let refused = reader.mark_alert_sent("wd_x", 1).unwrap_err();
    assert!(refused.to_string().contains("readonly"), "{refused}");
    drop(reader);
    assert_eq!(std::fs::read(scratch.db()).unwrap(), before);

    let empty = Scratch::new("missing");
    let refused = Source::open(&empty.state()).err().unwrap();
    assert!(
        refused.to_string().contains("does not exist"),
        "{refused:#}"
    );
    assert!(!empty.db().exists(), "no file was created");
    assert_eq!(
        std::fs::read_dir(empty.state()).unwrap().count(),
        0,
        "nothing was created beside it either"
    );
    let mut waiting = empty.publisher(&fake);
    assert!(matches!(
        waiting.cycle().await.unwrap_err(),
        CycleError::Retry(_)
    ));
}

// -- the command ----------------------------------------------------------------------------------

fn config(scratch: &Scratch, web: &str) -> PathBuf {
    let yaml = format!(
        "policy:
  people: [slack:alice]
  bots: [slack:deploy-bot]
  approvers: [slack:alice]
sources:
  - kind: jsonl
    path: triggers.jsonl
channels:
  - kind: jsonl
    source: slack
    path: outbox.jsonl
router: {{kind: scripted}}
executor: {{kind: scripted}}
budgets: {{session_microusd: 500000}}
{web}
"
    );
    let path = scratch.root.join("agent.yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

#[tokio::test]
async fn the_command_reads_its_token_beside_the_config_and_publishes_once_or_until_stopped() {
    let scratch = Scratch::new("command");
    scratch.append(&[request("t-request", "T1", "Summarise the incident report")]);
    scratch.serve(Arc::new(ScriptedExecutor)).await;
    let fake = Fake::start().await;
    let web = format!(
        "web:
  public_url: https://agent.example
  publish:
    token_file: blob.token
    every_secs: 1
    api_url: {}
",
        fake.url()
    );
    let config = config(&scratch, &web);

    // No publisher configured: nothing to do, and the token file is not touched.
    let bare = scratch.root.join("bare.yaml");
    std::fs::write(
        &bare,
        std::fs::read_to_string(&config)
            .unwrap()
            .replace(&web, "web: {public_url: https://agent.example}\n"),
    )
    .unwrap();
    let nothing = solos_swarm::agent::publish::run_until(
        &bare,
        &scratch.state(),
        true,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(nothing.to_string().contains("web.publish"), "{nothing:#}");

    let token = scratch.root.join("blob.token");
    std::fs::write(&token, TOKEN).unwrap();
    solos_swarm::agent::publish::run_until(&config, &scratch.state(), true, std::future::pending())
        .await
        .unwrap();
    assert!(!token.exists(), "the token file is consumed");
    assert_eq!(fake.puts().len(), 2);

    // Serving: a new request reaches the store, and SIGTERM ends it cleanly.
    std::fs::write(&token, TOKEN).unwrap();
    let stop = Arc::new(tokio::sync::Notify::new());
    let state = scratch.state();
    let stopper = stop.clone();
    let serving = tokio::spawn(async move {
        solos_swarm::agent::publish::run_until(&config, &state, false, async move {
            stopper.notified().await;
        })
        .await
    });
    scratch.append(&[request("t-second", "T4", "Rotate the staging keys")]);
    scratch.serve(Arc::new(ScriptedExecutor)).await;
    let waited = async {
        for _ in 0..600 {
            let index = fake.shared.lock().unwrap().blobs.get(INDEX_PATH).cloned();
            if let Some(index) = index
                && serde_json::from_slice::<Value>(&index).unwrap()["tickets"]
                    .as_array()
                    .unwrap()
                    .len()
                    == 2
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the second ticket never reached the store");
    };
    tokio::time::timeout(LIMIT, waited).await.unwrap();
    stop.notify_one();
    tokio::time::timeout(LIMIT, serving)
        .await
        .expect("the publisher did not stop")
        .unwrap()
        .unwrap();
}

#[test]
fn a_config_with_a_public_url_that_is_not_an_https_origin_is_refused() {
    let scratch = Scratch::new("bad-url");
    for bad in ["http://agent.example", "https://agent.example/"] {
        let path = config(&scratch, &format!("web: {{public_url: '{bad}'}}"));
        let refused = solos_swarm::agent::AgentConfig::load(&path).unwrap_err();
        assert!(refused.to_string().contains("public_url"), "{refused:#}");
    }
    let path = config(
        &scratch,
        "web: {public_url: https://agent.example, publish: {token_file: tokens/blob}}",
    );
    let loaded = solos_swarm::agent::AgentConfig::load(&path).unwrap();
    let publish = loaded.web.unwrap().publish.unwrap();
    assert_eq!(publish.token_file, scratch.root.join("tokens/blob"));
    assert_eq!((publish.every_secs, publish.heartbeat_secs), (5, 300));
}

// -- the Watchdog's external jobs ----------------------------------------------------------------

/// 2026-11-06T12:00:00Z, two hours before the dashboard fixtures' index was written.
const JOBS_T0: u64 = 1_793_966_400_000;

fn job_record(name: &str, check: &str, status: Status, detail: &str, since_ms: u64) -> Record {
    Record {
        name: name.into(),
        check: check.into(),
        status,
        detail: detail.into(),
        since_ms,
    }
}

/// What the Watchdog wrote at `checked_ms`: a unit that is up and a tmux
/// session in `trouble`.
fn jobs_state(checked_ms: u64, trouble: Status) -> JobsState {
    JobsState {
        v: JOBS_FORMAT,
        checked_ms,
        jobs: vec![
            job_record(
                "l2book",
                "systemd unit l2book-download.service",
                Status::Up,
                "active (running)",
                JOBS_T0,
            ),
            job_record(
                "build",
                "tmux session data-build",
                trouble,
                "no tmux session data-build",
                JOBS_T0 + 60_000,
            ),
        ],
    }
}

#[tokio::test]
async fn the_watchdogs_jobs_go_up_when_they_change_and_every_heartbeat_and_not_between() {
    let scratch = Scratch::new("jobs");
    SqliteStore::open(scratch.db()).unwrap();
    let fake = Fake::start().await;
    let mut publisher = scratch.publisher_with(&fake, TOKEN, Duration::from_millis(1_200));

    // The Watchdog has written nothing: there are no jobs to show.
    publisher.cycle().await.unwrap();
    assert_eq!(paths(&fake.puts()), [INDEX_PATH]);
    fake.clear_puts();

    // Its first look: the jobs go up alone, as the Watchdog wrote them.
    jobs_state(JOBS_T0, Status::Up)
        .save(&scratch.state())
        .unwrap();
    let report = publisher.cycle().await.unwrap();
    assert_eq!(report.uploaded, [JOBS_PATH.to_owned()]);
    let blob = fake.blob(JOBS_PATH);
    assert_eq!(blob["v"], 1);
    assert_eq!(blob["checked_ms"], JOBS_T0);
    assert_eq!(blob["jobs"][0]["name"], "l2book");
    assert_eq!(
        blob["jobs"][0]["check"],
        "systemd unit l2book-download.service"
    );
    assert_eq!(blob["jobs"][1]["status"], "up");
    fake.clear_puts();

    // The same findings a minute later change nothing the page shows: no upload.
    jobs_state(JOBS_T0 + 60_000, Status::Up)
        .save(&scratch.state())
        .unwrap();
    assert!(publisher.cycle().await.unwrap().uploaded.is_empty());
    assert!(fake.puts().is_empty());

    // A job that goes down is shown at once.
    jobs_state(JOBS_T0 + 120_000, Status::Down)
        .save(&scratch.state())
        .unwrap();
    let report = publisher.cycle().await.unwrap();
    assert_eq!(report.uploaded, [JOBS_PATH.to_owned()]);
    assert_eq!(fake.blob(JOBS_PATH)["jobs"][1]["status"], "down");
    fake.clear_puts();

    // Once a heartbeat is due they go up again with the time of the latest
    // look, so the page can tell a quiet Watchdog from a stopped one.
    jobs_state(JOBS_T0 + 180_000, Status::Down)
        .save(&scratch.state())
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    let report = publisher.cycle().await.unwrap();
    assert_eq!(
        report.uploaded,
        [JOBS_PATH.to_owned(), INDEX_PATH.to_owned()],
        "the jobs, then the index"
    );
    assert_eq!(fake.blob(JOBS_PATH)["checked_ms"], JOBS_T0 + 180_000);
}

#[tokio::test]
async fn a_restart_does_not_upload_jobs_that_are_already_there() {
    let scratch = Scratch::new("jobs-restart");
    SqliteStore::open(scratch.db()).unwrap();
    jobs_state(JOBS_T0, Status::Down)
        .save(&scratch.state())
        .unwrap();
    let fake = Fake::start().await;
    scratch.publisher(&fake).cycle().await.unwrap();
    assert_eq!(paths(&fake.puts()), [JOBS_PATH, INDEX_PATH]);
    fake.clear_puts();

    let mut restarted = scratch.publisher(&fake);
    assert!(restarted.cycle().await.unwrap().uploaded.is_empty());
    assert!(fake.puts().is_empty());
}

#[tokio::test]
async fn jobs_the_api_refuses_are_not_sent_again_until_they_change() {
    let scratch = Scratch::new("jobs-refused");
    SqliteStore::open(scratch.db()).unwrap();
    jobs_state(JOBS_T0, Status::Up)
        .save(&scratch.state())
        .unwrap();
    let fake = Fake::start().await;
    let mut publisher = scratch.publisher(&fake);

    fake.script([Scripted::Status(400)]);
    let report = publisher.cycle().await.unwrap();
    assert_eq!(report.rejected, [JOBS_PATH.to_owned()]);
    assert_eq!(
        report.uploaded,
        [INDEX_PATH.to_owned()],
        "the index is not held back"
    );

    let requests = fake.requests().len();
    let quiet = publisher.cycle().await.unwrap();
    assert!(
        quiet.rejected.is_empty() && quiet.uploaded.is_empty(),
        "{quiet:?}"
    );
    assert_eq!(fake.requests().len(), requests, "no request at all");

    jobs_state(JOBS_T0 + 60_000, Status::Down)
        .save(&scratch.state())
        .unwrap();
    let report = publisher.cycle().await.unwrap();
    assert_eq!(report.uploaded, [JOBS_PATH.to_owned()]);
}

/// With `PUBLISH_FIXTURE_DIR` set, write what the dashboard reads for the
/// jobs: `jobs.json`, with one job in each status.
#[tokio::test]
async fn the_jobs_blob_has_the_shape_the_dashboard_reads_and_feeds_its_fixtures() {
    let scratch = Scratch::new("jobs-fixture");
    SqliteStore::open(scratch.db()).unwrap();
    let state = JobsState {
        v: JOBS_FORMAT,
        checked_ms: JOBS_T0 + 7_200_000,
        jobs: vec![
            job_record(
                "l2book",
                "systemd unit l2book-download.service",
                Status::Up,
                "active (running)",
                JOBS_T0,
            ),
            job_record(
                "data-build",
                "tmux session data-build",
                Status::Down,
                "can't find session: data-build",
                JOBS_T0 + 3_600_000,
            ),
            job_record(
                "modal-wave-3",
                "status command /home/agent/bin/wave-status",
                Status::Stalled,
                "no new shard for 40m",
                JOBS_T0 + 4_800_000,
            ),
            job_record(
                "l2book-archive",
                "pid file /data/l2book/archive.pid",
                Status::LowDisk,
                "11.2 GB free on /data/l2book (floor 50.0 GB)",
                JOBS_T0 + 6_000_000,
            ),
        ],
    };
    state.save(&scratch.state()).unwrap();
    let fake = Fake::start().await;
    scratch.publisher(&fake).cycle().await.unwrap();
    let blob = fake.blob(JOBS_PATH);
    let statuses: Vec<&str> = blob["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|job| job["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["up", "down", "stalled", "low_disk"]);
    let mut keys: Vec<&str> = blob["jobs"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["check", "detail", "name", "since_ms", "status"]);

    if let Ok(dir) = std::env::var("PUBLISH_FIXTURE_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            PathBuf::from(dir).join("jobs.json"),
            serde_json::to_string_pretty(&blob).unwrap() + "\n",
        )
        .unwrap();
    }
}
