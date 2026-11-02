//! The GitHub adapters against a fake GitHub: an HTTP server for the REST
//! endpoints they call, with `since`, pages, ETags and rate limits. No real
//! GitHub is contacted.

use anyhow::Result;
use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use solos_agent_core::AllowlistPolicy;
use solos_agent_core::model::{
    Actor, ActorKind, Addressed, AssignmentId, ChannelRef, Effect, MessageId, MessageKind,
    OutboundMessage, ThreadKey, TicketId, Trigger, TriggerId,
};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::github::{
    self, CURSOR_FILE, GithubApi, GithubChannel, GithubSource, OVERLAP, Token, marker,
};
use solos_swarm::agent::jsonl::JsonlSource;
use solos_swarm::agent::ports::{
    BoxFuture, Channel, Delivery, Executor, FactSink, Launch, RetryAfter, SendOptions, SendOutcome,
    Source,
};
use solos_swarm::agent::scripted::{ScriptedExecutor, ScriptedRouter};
use solos_swarm::agent::utc;
use solos_swarm::agent::{Adapters, RunOptions, Runtime, STORE_FILE, Summary, serve};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const LIMIT: Duration = Duration::from_secs(30);
const TOKEN: &str = "ghp_fakefakefake";
const ME: &str = "solos-bot";
const REPO: &str = "acme/widgets";
const BUDGET: u64 = 500_000;

// ---- The fake GitHub ----

/// How the fake answers the next comment the agent posts.
enum Reply {
    Status(u16, &'static str, Option<&'static str>),
    /// Keep the comment but never answer, as if the agent crashed while
    /// GitHub took it.
    Hang,
}

/// A rate limit the next request meets, whatever it asks for.
#[derive(Clone, Copy, Debug)]
enum Limit {
    /// 403: nothing remains until the reset, two seconds on.
    Primary,
    /// 429 with `Retry-After: 1`.
    Secondary,
    /// Answered, but it was the last request until the reset.
    Exhausted,
}

struct Seen {
    at: Instant,
    method: String,
    path: String,
    query: HashMap<String, String>,
    if_none_match: Option<String>,
    status: u16,
    limit: Option<Limit>,
}

struct Post {
    number: u64,
    body: String,
}

#[derive(Default)]
struct Hub {
    issues: Vec<Value>,
    comments: Vec<Value>,
    review_comments: Vec<Value>,
    runs: Vec<Value>,
    pulls: Vec<Value>,
    /// Listings serve this many items a page instead of `per_page`.
    page_size: Option<usize>,
    /// One issue's comments come this many a page.
    issue_page: Option<usize>,
    requests: Vec<Seen>,
    posts: Vec<Post>,
    script: VecDeque<Reply>,
    limits: VecDeque<Limit>,
    next_id: u64,
}

#[derive(Clone)]
struct Shared {
    hub: Arc<Mutex<Hub>>,
    api: String,
}

struct Fake {
    shared: Shared,
    api: String,
    /// Now, in whole seconds, as GitHub writes times.
    t0: u64,
    task: JoinHandle<()>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn message(status: StatusCode, text: &str) -> Response {
    (status, Json(json!({ "message": text }))).into_response()
}

/// Records each request, checks the token and applies a scripted limit.
async fn gate(State(shared): State<Shared>, request: Request, next: Next) -> Response {
    let at = Instant::now();
    let method = request.method().to_string();
    let path = request.uri().path().trim_start_matches("/api/").to_owned();
    let query = Query::<HashMap<String, String>>::try_from_uri(request.uri())
        .map(|q| q.0)
        .unwrap_or_default();
    let headers = request.headers().clone();
    let header = |name| {
        headers
            .get(name)
            .and_then(|v: &HeaderValue| v.to_str().ok())
            .map(str::to_owned)
    };
    let if_none_match = header(header::IF_NONE_MATCH);
    let authorized = header(header::AUTHORIZATION).as_deref() == Some(&format!("Bearer {TOKEN}"));
    let limit = shared.hub.lock().unwrap().limits.pop_front();
    let reset = (now_secs() + 2).to_string();
    let mut response = match (authorized, limit) {
        (false, _) => message(StatusCode::UNAUTHORIZED, "Bad credentials"),
        (true, Some(Limit::Primary)) => {
            let mut response = message(StatusCode::FORBIDDEN, "API rate limit exceeded for user");
            response
                .headers_mut()
                .insert("x-ratelimit-reset", reset.parse().unwrap());
            response
        }
        (true, Some(Limit::Secondary)) => {
            let mut response = message(
                StatusCode::TOO_MANY_REQUESTS,
                "You have exceeded a secondary rate limit.",
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            response
        }
        (true, Some(Limit::Exhausted)) => {
            let mut response = next.run(request).await;
            response
                .headers_mut()
                .insert("x-ratelimit-reset", reset.parse().unwrap());
            response
        }
        (true, None) => next.run(request).await,
    };
    let remaining = if matches!(limit, Some(Limit::Primary | Limit::Exhausted)) {
        "0"
    } else {
        "4999"
    };
    response
        .headers_mut()
        .insert("x-ratelimit-remaining", HeaderValue::from_static(remaining));
    shared.hub.lock().unwrap().requests.push(Seen {
        at,
        method,
        path,
        query,
        if_none_match,
        status: response.status().as_u16(),
        limit,
    });
    response
}

fn ms(text: &Value) -> u64 {
    utc::parse(text.as_str().unwrap()).unwrap()
}

/// One page of `items` with an ETag and `Link`s to the next and last pages,
/// or a 304 when the asked ETag still holds.
fn page(
    shared: &Shared,
    path: &str,
    items: Vec<Value>,
    per_page: usize,
    query: &HashMap<String, String>,
    headers: &HeaderMap,
    wrap: impl Fn(Vec<Value>) -> Value,
) -> Response {
    let number: usize = query.get("page").map_or(1, |p| p.parse().unwrap());
    let pages = items.len().div_ceil(per_page).max(1);
    let body = wrap(
        items
            .into_iter()
            .skip((number - 1) * per_page)
            .take(per_page)
            .collect(),
    )
    .to_string();
    let mut hasher = DefaultHasher::new();
    body.hash(&mut hasher);
    let etag = format!("W/\"{:016x}\"", hasher.finish());
    if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|asked| asked.to_str().unwrap() == etag)
    {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response
            .headers_mut()
            .insert(header::ETAG, etag.parse().unwrap());
        return response;
    }
    let link = |to: usize| {
        let mut query = query.clone();
        query.insert("page".into(), to.to_string());
        let url =
            reqwest::Url::parse_with_params(&format!("{}{path}", shared.api), &query).unwrap();
        format!("<{url}>")
    };
    let mut response = ([(header::CONTENT_TYPE, "application/json")], body).into_response();
    response
        .headers_mut()
        .insert(header::ETAG, etag.parse().unwrap());
    if number < pages {
        let links = format!(
            "{}; rel=\"next\", {}; rel=\"last\"",
            link(number + 1),
            link(pages)
        );
        response
            .headers_mut()
            .insert(header::LINK, links.parse().unwrap());
    }
    response
}

/// A listing `since` its query asks, oldest update first.
fn since(
    shared: &Shared,
    path: &str,
    items: &[Value],
    query: &HashMap<String, String>,
    headers: &HeaderMap,
) -> Response {
    let from = query.get("since").map(|s| utc::parse(s).unwrap());
    let mut found: Vec<Value> = items
        .iter()
        .filter(|item| from.is_none_or(|from| ms(&item["updated_at"]) >= from))
        .cloned()
        .collect();
    found.sort_by_key(|item| ms(&item["updated_at"]));
    let per_page = shared
        .hub
        .lock()
        .unwrap()
        .page_size
        .unwrap_or_else(|| query["per_page"].parse().unwrap());
    page(shared, path, found, per_page, query, headers, Value::Array)
}

type Listing = (
    State<Shared>,
    Path<(String, String)>,
    Query<HashMap<String, String>>,
    HeaderMap,
);

fn ours(owner: &str, repo: &str) -> bool {
    format!("{owner}/{repo}") == REPO
}

async fn user() -> Response {
    Json(json!({ "login": ME, "id": 1, "type": "User" })).into_response()
}

async fn repository(Path((owner, repo)): Path<(String, String)>) -> Response {
    if !ours(&owner, &repo) {
        return message(StatusCode::NOT_FOUND, "Not Found");
    }
    Json(json!({ "full_name": REPO })).into_response()
}

async fn issues((State(shared), Path((owner, repo)), Query(query), headers): Listing) -> Response {
    assert!(ours(&owner, &repo));
    assert_eq!(query["state"], "all");
    let items = shared.hub.lock().unwrap().issues.clone();
    since(
        &shared,
        &format!("repos/{REPO}/issues"),
        &items,
        &query,
        &headers,
    )
}

async fn comments(
    (State(shared), Path((owner, repo)), Query(query), headers): Listing,
) -> Response {
    assert!(ours(&owner, &repo));
    let items = shared.hub.lock().unwrap().comments.clone();
    let path = format!("repos/{REPO}/issues/comments");
    since(&shared, &path, &items, &query, &headers)
}

async fn review_comments(
    (State(shared), Path((owner, repo)), Query(query), headers): Listing,
) -> Response {
    assert!(ours(&owner, &repo));
    let items = shared.hub.lock().unwrap().review_comments.clone();
    let path = format!("repos/{REPO}/pulls/comments");
    since(&shared, &path, &items, &query, &headers)
}

/// The latest completed runs, newest first.
async fn runs((State(shared), Path((owner, repo)), Query(query), headers): Listing) -> Response {
    assert!(ours(&owner, &repo));
    assert_eq!(query["status"], "completed");
    let mut items = shared.hub.lock().unwrap().runs.clone();
    items.sort_by_key(|run| std::cmp::Reverse(run["id"].as_u64().unwrap()));
    let path = format!("repos/{REPO}/actions/runs");
    page(
        &shared,
        &path,
        items,
        100,
        &query,
        &headers,
        |runs| json!({ "total_count": runs.len(), "workflow_runs": runs }),
    )
}

async fn pulls((State(shared), Path((owner, repo)), Query(query), headers): Listing) -> Response {
    assert!(ours(&owner, &repo));
    assert_eq!(query["state"], "open");
    let items = shared.hub.lock().unwrap().pulls.clone();
    page(
        &shared,
        &format!("repos/{REPO}/pulls"),
        items,
        100,
        &query,
        &headers,
        Value::Array,
    )
}

async fn issue_comments(
    State(shared): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if !ours(&owner, &repo) {
        return message(StatusCode::NOT_FOUND, "Not Found");
    }
    let (items, per_page) = {
        let hub = shared.hub.lock().unwrap();
        let on = format!("/issues/{number}");
        let items: Vec<Value> = hub
            .comments
            .iter()
            .filter(|c| c["issue_url"].as_str().unwrap().ends_with(&on))
            .cloned()
            .collect();
        let per_page = hub
            .issue_page
            .unwrap_or_else(|| query["per_page"].parse().unwrap());
        (items, per_page)
    };
    let path = format!("repos/{REPO}/issues/{number}/comments");
    page(
        &shared,
        &path,
        items,
        per_page,
        &query,
        &headers,
        Value::Array,
    )
}

async fn post_comment(
    State(shared): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    Json(body): Json<Value>,
) -> Response {
    if !ours(&owner, &repo) {
        return message(StatusCode::NOT_FOUND, "Not Found");
    }
    let text = body["body"].as_str().unwrap().to_owned();
    let (reply, posted) = {
        let mut hub = shared.hub.lock().unwrap();
        hub.posts.push(Post {
            number,
            body: text.clone(),
        });
        let reply = hub.script.pop_front();
        hub.next_id += 1;
        let at = now_secs() * 1_000;
        let posted = comment(900 + hub.next_id, number, ME, &text, at, at, false);
        if matches!(reply, None | Some(Reply::Hang)) {
            hub.comments.push(posted.clone());
        }
        (reply, posted)
    };
    match reply {
        None => (StatusCode::CREATED, Json(posted)).into_response(),
        Some(Reply::Status(status, text, retry_after)) => {
            let mut response = message(StatusCode::from_u16(status).unwrap(), text);
            if let Some(seconds) = retry_after {
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, seconds.parse().unwrap());
            }
            response
        }
        Some(Reply::Hang) => std::future::pending().await,
    }
}

impl Fake {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}/api/", listener.local_addr().unwrap());
        let shared = Shared {
            hub: Arc::default(),
            api: api.clone(),
        };
        let numbered = "/api/repos/{owner}/{repo}/issues/{number}/comments";
        let app = axum::Router::new()
            .route("/api/user", get(user))
            .route("/api/repos/{owner}/{repo}", get(repository))
            .route("/api/repos/{owner}/{repo}/issues", get(issues))
            .route("/api/repos/{owner}/{repo}/issues/comments", get(comments))
            .route(numbered, get(issue_comments).post(post_comment))
            .route("/api/repos/{owner}/{repo}/pulls", get(pulls))
            .route(
                "/api/repos/{owner}/{repo}/pulls/comments",
                get(review_comments),
            )
            .route("/api/repos/{owner}/{repo}/actions/runs", get(runs))
            .layer(middleware::from_fn_with_state(shared.clone(), gate))
            .with_state(shared.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            shared,
            api,
            t0: now_secs() * 1_000,
            task,
        }
    }

    fn hub(&self) -> std::sync::MutexGuard<'_, Hub> {
        self.shared.hub.lock().unwrap()
    }

    fn script(&self, replies: impl IntoIterator<Item = Reply>) {
        self.hub().script.extend(replies);
    }

    async fn until<T>(&self, what: &str, check: impl Fn(&Hub) -> Option<T>) -> T {
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Some(found) = check(&self.hub()) {
                return found;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The GET requests for `path`.
    fn reads(&self, path: &str) -> Vec<(Option<String>, u16, HashMap<String, String>)> {
        self.hub()
            .requests
            .iter()
            .filter(|r| r.method == "GET" && r.path == path)
            .map(|r| (r.if_none_match.clone(), r.status, r.query.clone()))
            .collect()
    }

    /// Each call has its own rate-limit hold, as a restart would.
    fn api(&self) -> Arc<GithubApi> {
        Arc::new(GithubApi::new(&self.api, Token::new(TOKEN)).unwrap())
    }

    fn source(&self, scratch: &Scratch, follow: bool) -> Box<dyn Source> {
        let source = GithubSource::new(self.api(), [REPO.to_owned()], scratch.cursor(), follow)
            .with_poll(Duration::from_millis(20))
            .with_backoff(Duration::from_millis(10), Duration::from_millis(100));
        Box::new(source)
    }

    fn channel(&self) -> Arc<dyn Channel> {
        Arc::new(GithubChannel::new(self.api()))
    }
}

// ---- What GitHub lists ----

fn ts(at: u64) -> String {
    utc::format(at)
}

fn user_of(login: &str) -> Value {
    let kind = if login.ends_with("[bot]") {
        "Bot"
    } else {
        "User"
    };
    json!({ "login": login, "id": 1_000 + login.len(), "type": kind })
}

fn kind_path(pull: bool) -> &'static str {
    if pull { "pull" } else { "issues" }
}

fn issue(
    number: u64,
    login: &str,
    title: &str,
    body: Option<&str>,
    (created, updated): (u64, u64),
    pull: bool,
) -> Value {
    let mut issue = json!({
        "id": 5_000 + number,
        "number": number,
        "title": title,
        "body": body,
        "user": user_of(login),
        "state": "open",
        "created_at": ts(created),
        "updated_at": ts(updated),
        "html_url": format!("https://github.com/{REPO}/{}/{number}", kind_path(pull)),
    });
    if pull {
        issue["pull_request"] =
            json!({ "url": format!("https://api.github.com/repos/{REPO}/pulls/{number}") });
    }
    issue
}

fn comment(
    id: u64,
    number: u64,
    login: &str,
    body: &str,
    created: u64,
    updated: u64,
    pull: bool,
) -> Value {
    json!({
        "id": id,
        "issue_url": format!("https://api.github.com/repos/{REPO}/issues/{number}"),
        "html_url": format!("https://github.com/{REPO}/{}/{number}#issuecomment-{id}", kind_path(pull)),
        "user": user_of(login),
        "body": body,
        "created_at": ts(created),
        "updated_at": ts(updated),
    })
}

fn review_comment(id: u64, number: u64, login: &str, body: &str, at: u64) -> Value {
    json!({
        "id": id,
        "pull_request_url": format!("https://api.github.com/repos/{REPO}/pulls/{number}"),
        "html_url": format!("https://github.com/{REPO}/pull/{number}#discussion_r{id}"),
        "user": user_of(login),
        "body": body,
        "path": "src/parse.rs",
        "line": 12,
        "commit_id": "abc123",
        "in_reply_to_id": null,
        "pull_request_review_id": 77,
        "created_at": ts(at),
        "updated_at": ts(at),
    })
}

fn run(id: u64, conclusion: &str, sha: &str, pulls: &[u64], ended: u64) -> Value {
    let pulls: Vec<Value> = pulls
        .iter()
        .map(|n| json!({ "id": n, "number": n }))
        .collect();
    json!({
        "id": id,
        "run_attempt": 1,
        "name": "CI",
        "workflow_id": 9,
        "status": "completed",
        "conclusion": conclusion,
        "head_sha": sha,
        "head_branch": "feature",
        "event": "pull_request",
        "html_url": format!("https://github.com/{REPO}/actions/runs/{id}"),
        "pull_requests": pulls,
        "created_at": ts(ended - 60_000),
        "updated_at": ts(ended),
    })
}

fn pull(number: u64, sha: &str) -> Value {
    json!({ "number": number, "state": "open", "head": { "sha": sha } })
}

// ---- The agent ----

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agent-github-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        Self { root }
    }
    fn store(&self) -> SqliteStore {
        SqliteStore::open(self.root.join("state").join(STORE_FILE)).unwrap()
    }
    fn cursor(&self) -> PathBuf {
        self.root.join("state").join(CURSOR_FILE)
    }
    fn triggers(&self, triggers: &[Trigger]) -> Box<dyn Source> {
        let path = self.root.join("triggers.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        for trigger in triggers {
            writeln!(file, "{}", serde_json::to_string(trigger).unwrap()).unwrap();
        }
        Box::new(JsonlSource::new(path, false))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn policy() -> AllowlistPolicy {
    AllowlistPolicy {
        people: ["github:alice".to_owned(), "github:bob".to_owned()].into(),
        bots: ["github:github-actions[bot]".to_owned()].into(),
        ..AllowlistPolicy::default()
    }
}

struct Agent<'a> {
    scratch: &'a Scratch,
    source: Box<dyn Source>,
    channel: Option<Arc<dyn Channel>>,
    executor: Arc<dyn Executor>,
    until_idle: bool,
}

impl Agent<'_> {
    fn runtime(self) -> Runtime {
        let adapters = Adapters {
            sources: vec![self.source],
            channels: self
                .channel
                .map(|channel| BTreeMap::from([("github".to_owned(), channel)]))
                .unwrap_or_default(),
            router: Arc::new(ScriptedRouter::new(BUDGET)),
            executor: self.executor,
        };
        let options = RunOptions {
            until_idle: self.until_idle,
            tick: Duration::from_millis(10),
            retry_base: Duration::from_millis(10),
            retry_max: Duration::from_millis(500),
        };
        Runtime::new(self.scratch.store(), policy(), adapters, options)
    }
}

/// One read of the fake through the runtime, until it is idle.
async fn read_once(fake: &Fake, scratch: &Scratch) -> Summary {
    let runtime = Agent {
        scratch,
        source: fake.source(scratch, false),
        channel: None,
        executor: Arc::new(ScriptedExecutor),
        until_idle: true,
    }
    .runtime();
    tokio::time::timeout(LIMIT, runtime.run(std::future::pending()))
        .await
        .expect("the run did not go idle")
        .unwrap()
}

async fn until_triggers(scratch: &Scratch, n: usize) {
    let deadline = Instant::now() + LIMIT;
    while scratch.store().triggers().unwrap().len() < n {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {n} triggers"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Run until `driver` is done with the fake, then stop the runtime.
async fn drive<T>(runtime: Runtime, driver: impl Future<Output = T>) -> (Summary, T) {
    let (stop, stopped) = oneshot::channel::<()>();
    let driver = async {
        let value = driver.await;
        let _ = stop.send(());
        value
    };
    let run = runtime.run(async {
        let _ = stopped.await;
    });
    let (summary, value) = tokio::time::timeout(LIMIT, async { tokio::join!(run, driver) })
        .await
        .expect("the run did not end");
    (summary.unwrap(), value)
}

fn request(id: &str, number: u64, text: &str) -> Trigger {
    Trigger {
        id: TriggerId::new(id),
        source: "github".into(),
        at_ms: 1_000,
        actor: Actor::person("alice"),
        addressed: Addressed::Mention,
        text: text.into(),
        threads: vec![github::thread(REPO, number)],
        reply_to: vec![github::reply_to(REPO, number)],
        meta: json!({}),
    }
}

fn outbound(id: &str, kind: MessageKind, number: u64, text: &str) -> OutboundMessage {
    OutboundMessage {
        id: MessageId::new(id),
        channel: github::reply_to(REPO, number),
        kind,
        text: text.into(),
        ticket: Some(TicketId::new("tk_1")),
    }
}

/// The `OutboundMessage` id a posted comment names in its marker.
fn message_id(body: &str) -> &str {
    body.trim_end()
        .rsplit_once("<!-- solos-swarm message ")
        .and_then(|(_, rest)| rest.strip_suffix(" -->"))
        .unwrap_or_else(|| panic!("no marker: {body}"))
}

fn repeat() -> SendOptions {
    SendOptions {
        repeat: true,
        ..SendOptions::default()
    }
}

fn retry_after(err: &anyhow::Error) -> Option<Duration> {
    err.chain()
        .find_map(|e| e.downcast_ref::<RetryAfter>())
        .map(|after| after.0)
}

fn assert_settled(store: &SqliteStore) {
    let pending: Vec<_> = store
        .effects()
        .unwrap()
        .into_iter()
        .filter(|q| !q.done)
        .collect();
    assert!(pending.is_empty(), "effects not done: {pending:?}");
}

fn send<'a>(channel: &'a Arc<dyn Channel>, id: &str) -> BoxFuture<'a, Result<SendOutcome>> {
    channel.send(
        outbound(id, MessageKind::Result, 7, "done"),
        SendOptions::default(),
    )
}

/// Never finishes a launch, so the run can be cut short mid-flight.
struct Hanging;

impl Executor for Hanging {
    fn launch(&self, _: Launch, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(std::future::pending())
    }
    fn deliver(&self, _: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

// ---- The Source ----

#[tokio::test]
async fn events_become_triggers_on_their_threads_and_the_agents_own_are_dropped() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("mapping");
    let ago = |seconds: u64| fake.t0 - seconds * 1_000;
    {
        let mut hub = fake.hub();
        hub.page_size = Some(2);
        hub.issues = vec![
            issue(
                7,
                "alice",
                "Flaky test in CI",
                Some("@solos-bot can you look? It fails on main."),
                (ago(300), ago(300)),
                false,
            ),
            issue(
                8,
                "bob",
                "Speed up the parser",
                None,
                (ago(280), ago(200)),
                true,
            ),
            // Updated now, opened days ago: not new.
            issue(
                3,
                "carol",
                "An old issue",
                None,
                (ago(259_200), ago(250)),
                false,
            ),
        ];
        hub.comments = vec![
            comment(
                101,
                7,
                "alice",
                "@solos-bot please fix it",
                ago(260),
                ago(260),
                false,
            ),
            comment(102, 7, ME, "On it.", ago(250), ago(250), false),
            comment(
                103,
                8,
                "dependabot[bot]",
                "Bumps serde to 1.0.2.",
                ago(240),
                ago(240),
                true,
            ),
            // An edit of an old comment.
            comment(
                104,
                3,
                "carol",
                "@solos-bot edited",
                ago(172_800),
                ago(230),
                false,
            ),
            comment(
                105,
                7,
                "alice",
                "mail alice@solos-bot.dev",
                ago(220),
                ago(220),
                false,
            ),
        ];
        hub.review_comments = vec![review_comment(
            201,
            8,
            "bob",
            "@SOLOS-BOT nit: rename this",
            ago(210),
        )];
        hub.runs = vec![
            run(301, "timed_out", "aaa1111", &[8], ago(200)),
            run(302, "success", "aaa1111", &[8], ago(190)),
            // From a fork: GitHub names no pull request, but #9's head matches.
            run(303, "failure", "bbb2222", &[], ago(180)),
            // A push to main: on no pull request.
            run(304, "failure", "ccc3333", &[], ago(170)),
            // Ended before the first read's window.
            run(305, "failure", "aaa1111", &[8], ago(7_200)),
        ];
        hub.pulls = vec![pull(8, "aaa1111"), pull(9, "bbb2222")];
    }

    let summary = read_once(&fake, &scratch).await;

    assert_eq!(
        (summary.triggers_received, summary.triggers_duplicate),
        (8, 0)
    );
    let mut triggers = scratch.store().triggers().unwrap();
    triggers.sort_by_key(|t| t.at_ms);
    let ids: Vec<&str> = triggers.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "github:acme/widgets#7",
            "github:acme/widgets#8",
            "github:acme/widgets:comment:101",
            "github:acme/widgets:comment:103",
            "github:acme/widgets:comment:105",
            "github:acme/widgets:review_comment:201",
            "github:acme/widgets:run:301:1",
            "github:acme/widgets:run:303:1",
        ],
        "own comments, old items, edits, passing runs and runs off pull requests stay out"
    );
    let [
        opened,
        pull_request,
        asked,
        bumped,
        mailed,
        reviewed,
        failed,
        forked,
    ] = &triggers[..]
    else {
        unreachable!()
    };
    let on = |number| {
        (
            vec![github::thread(REPO, number)],
            vec![github::reply_to(REPO, number)],
        )
    };

    assert_eq!(opened.source, "github");
    assert_eq!(opened.actor, Actor::person("alice"));
    assert_eq!(opened.addressed, Addressed::Mention);
    assert_eq!(
        opened.text,
        "Flaky test in CI\n\n@solos-bot can you look? It fails on main."
    );
    assert_eq!(opened.at_ms, ago(300));
    assert_eq!((opened.threads.clone(), opened.reply_to.clone()), on(7));
    assert_eq!(opened.threads, [ThreadKey::new("github:acme/widgets#7")]);
    assert_eq!(
        opened.reply_to[0],
        ChannelRef {
            source: "github".into(),
            address: BTreeMap::from([
                ("repo".to_owned(), REPO.to_owned()),
                ("number".to_owned(), "7".to_owned()),
            ]),
        }
    );
    assert_eq!(opened.meta["kind"], "issue");
    assert_eq!(opened.meta["id"], 5_007);

    assert_eq!(pull_request.addressed, Addressed::Unaddressed);
    assert_eq!(pull_request.text, "Speed up the parser");
    assert_eq!(pull_request.meta["kind"], "pull_request");

    assert_eq!(asked.actor, Actor::person("alice"));
    assert_eq!(asked.addressed, Addressed::Mention);
    assert_eq!(
        asked.text, "please fix it",
        "the agent's mention is dropped"
    );
    assert_eq!((asked.threads.clone(), asked.reply_to.clone()), on(7));
    assert_eq!(asked.meta["comment_id"], 101);
    assert_eq!(asked.meta["pull_request"], false);

    assert_eq!(bumped.actor.kind, ActorKind::Bot);
    assert_eq!(bumped.actor.id, "dependabot[bot]");
    assert_eq!(bumped.meta["pull_request"], true);
    assert_eq!((bumped.threads.clone(), bumped.reply_to.clone()), on(8));

    assert_eq!(
        mailed.addressed,
        Addressed::Unaddressed,
        "an email is not a mention"
    );

    assert_eq!(reviewed.addressed, Addressed::Mention);
    assert_eq!(reviewed.text, "nit: rename this");
    assert_eq!((reviewed.threads.clone(), reviewed.reply_to.clone()), on(8));
    assert_eq!(reviewed.meta["path"], "src/parse.rs");
    assert_eq!(reviewed.meta["line"], 12);

    assert_eq!(
        failed.actor,
        Actor {
            id: "github-actions[bot]".into(),
            kind: ActorKind::Bot,
            name: Some("CI".into()),
        }
    );
    assert_eq!(failed.addressed, Addressed::Unaddressed);
    assert_eq!((failed.threads.clone(), failed.reply_to.clone()), on(8));
    assert_eq!(
        failed.text,
        format!(
            "CI failed on #8: the CI workflow ended timed_out at aaa1111. https://github.com/{REPO}/actions/runs/301"
        )
    );
    assert_eq!(failed.meta["run_id"], 301);
    assert_eq!(failed.meta["head_sha"], "aaa1111");
    assert_eq!((forked.threads.clone(), forked.reply_to.clone()), on(9));

    let since = utc::format(fake.t0 - OVERLAP.as_millis() as u64);
    let reads = fake.reads("repos/acme/widgets/issues/comments");
    assert_eq!(reads.len(), 3, "five comments, two a page");
    let first = &reads[0].2;
    assert_eq!(
        (first["sort"].as_str(), first["direction"].as_str()),
        ("updated", "asc")
    );
    assert!(
        utc::parse(&first["since"]).unwrap() >= utc::parse(&since).unwrap(),
        "a first read starts {OVERLAP:?} before now: {} < {since}",
        first["since"]
    );
    assert_eq!(
        fake.reads("repos/acme/widgets/pulls").len(),
        1,
        "read once, for the fork"
    );
    assert_eq!(fake.reads("user").len(), 1);
    let cursor: Value = serde_json::from_slice(&std::fs::read(scratch.cursor()).unwrap()).unwrap();
    let seen = |feed: &str| -> Vec<String> {
        let seen = cursor["repos"][REPO][feed]["seen"].as_object().unwrap();
        seen.keys().cloned().collect()
    };
    assert_eq!(
        seen("comments"),
        [
            "github:acme/widgets:comment:101",
            "github:acme/widgets:comment:103",
            "github:acme/widgets:comment:105",
        ]
    );
    assert_eq!(
        seen("runs"),
        [
            "github:acme/widgets:run:301:1",
            "github:acme/widgets:run:303:1"
        ]
    );
    let at = cursor["repos"][REPO]["comments"]["at"].as_u64().unwrap();
    assert!(at >= fake.t0, "a first read starts its position now");
}

#[tokio::test]
async fn quiet_polls_are_conditional_and_rate_limits_are_waited_out() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("conditional");
    let t0 = fake.t0;
    fake.hub().comments = vec![comment(
        101,
        7,
        "alice",
        "@solos-bot fix the flaky test",
        t0 - 60_000,
        t0 - 60_000,
        false,
    )];
    let runtime = Agent {
        scratch: &scratch,
        source: fake.source(&scratch, true),
        channel: None,
        executor: Arc::new(ScriptedExecutor),
        until_idle: false,
    }
    .runtime();
    let driver = async {
        let path = "repos/acme/widgets/issues/comments";
        fake.until("three reads", |hub| {
            (hub.requests.iter().filter(|r| r.path == path).count() >= 3).then_some(())
        })
        .await;
        let reads = fake.reads(path);
        assert_eq!((reads[0].0.as_deref(), reads[0].1), (None, 200));
        for (etag, status, _) in &reads[1..3] {
            assert!(etag.is_some(), "a later read is conditional");
            assert_eq!(*status, 304);
        }
        let mut waits = Vec::new();
        for limit in [Limit::Primary, Limit::Secondary, Limit::Exhausted] {
            let before = fake.hub().requests.len();
            fake.hub().limits.push_back(limit);
            let gap = fake
                .until("a request after the limit", |hub| {
                    let at = before
                        + hub.requests[before..]
                            .iter()
                            .position(|r| r.limit.is_some())?;
                    Some(hub.requests.get(at + 1)?.at - hub.requests[at].at)
                })
                .await;
            waits.push((limit, gap));
        }
        fake.hub().comments.push(comment(
            106,
            7,
            "alice",
            "@solos-bot and the docs",
            t0 - 1_000,
            t0 - 1_000,
            false,
        ));
        let new = "github:acme/widgets:comment:106";
        let deadline = Instant::now() + LIMIT;
        while !scratch
            .store()
            .triggers()
            .unwrap()
            .iter()
            .any(|t| t.id.as_str() == new)
        {
            assert!(Instant::now() < deadline, "the new comment never arrived");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        waits
    };

    let (summary, waits) = drive(runtime, driver).await;

    for (limit, gap) in waits {
        assert!(
            gap >= Duration::from_millis(950),
            "{limit:?}: the next request came {gap:?} after the limit"
        );
    }
    assert_eq!(
        (summary.triggers_received, summary.triggers_duplicate),
        (2, 0),
        "each comment went out once"
    );
}

#[tokio::test]
async fn a_listing_of_several_pages_is_read_whole_so_new_items_on_its_last_page_arrive() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("pages");
    let at = |id: u64| fake.t0 - (200 - id) * 1_000;
    {
        let mut hub = fake.hub();
        hub.page_size = Some(2);
        hub.comments = (101..=103)
            .map(|id| comment(id, 7, "bob", "chatter", at(id), at(id), false))
            .collect();
    }
    let runtime = Agent {
        scratch: &scratch,
        source: fake.source(&scratch, true),
        channel: None,
        executor: Arc::new(ScriptedExecutor),
        until_idle: false,
    }
    .runtime();
    let driver = async {
        until_triggers(&scratch, 3).await;
        let comment = comment(104, 7, "bob", "more chatter", at(104), at(104), false);
        fake.hub().comments.push(comment);
        until_triggers(&scratch, 4).await;
    };

    let (summary, ()) = drive(runtime, driver).await;

    assert_eq!(
        (summary.triggers_received, summary.triggers_duplicate),
        (4, 0)
    );
}

#[tokio::test]
async fn a_restart_reads_from_its_cursor_and_sends_nothing_twice() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("restart");
    let t0 = fake.t0;
    let minutes = |m: u64| t0 - m * 60_000;
    // A run an hour ago left this position; the agent was down since.
    let position = json!({ "at": minutes(60) });
    let left = json!({ "repos": { "acme/widgets": {
        "issues": position, "comments": position, "review_comments": position, "runs": position,
    } } });
    std::fs::write(scratch.cursor(), left.to_string()).unwrap();
    {
        let mut hub = fake.hub();
        hub.issues = vec![issue(
            7,
            "alice",
            "Flaky test",
            Some("@solos-bot look"),
            (minutes(30), minutes(30)),
            false,
        )];
        hub.comments = vec![comment(
            101,
            7,
            "alice",
            "@solos-bot fix it",
            minutes(20),
            minutes(20),
            false,
        )];
    }

    let first = read_once(&fake, &scratch).await;

    assert_eq!(
        (first.triggers_received, first.triggers_duplicate),
        (2, 0),
        "what arrived while the agent was down is read"
    );
    let cursor: Value = serde_json::from_slice(&std::fs::read(scratch.cursor()).unwrap()).unwrap();
    let comments = &cursor["repos"][REPO]["comments"];
    assert_eq!(comments["at"], minutes(20));
    assert_eq!(
        comments["seen"]["github:acme/widgets:comment:101"],
        minutes(20)
    );
    fake.hub().comments.push(comment(
        106,
        7,
        "alice",
        "@solos-bot and the docs",
        t0 - 10_000,
        t0 - 10_000,
        false,
    ));
    let before = fake.hub().requests.len();

    let second = read_once(&fake, &scratch).await;

    assert_eq!(
        (second.triggers_received, second.triggers_duplicate),
        (1, 0),
        "only the new comment went out"
    );
    let read = fake.hub().requests[before..]
        .iter()
        .find(|r| r.path == "repos/acme/widgets/issues/comments")
        .map(|r| r.query["since"].clone())
        .unwrap();
    let overlap = OVERLAP.as_millis() as u64;
    assert_eq!(
        read,
        utc::format(minutes(20) - overlap),
        "read from the cursor, less the overlap"
    );
    let ids: Vec<String> = scratch
        .store()
        .triggers()
        .unwrap()
        .iter()
        .map(|t| t.id.to_string())
        .collect();
    assert_eq!(
        ids,
        [
            "github:acme/widgets#7",
            "github:acme/widgets:comment:101",
            "github:acme/widgets:comment:106",
        ]
    );
}

// ---- The Channel ----

#[tokio::test]
async fn a_reply_is_a_comment_on_its_issue_ending_in_a_hidden_marker() {
    let fake = Fake::start().await;
    let channel = fake.channel();
    let long = format!(
        "Fixed, thanks @alice and @acme/core.\n\n```\n@Override\n```\n\n{}",
        "word ".repeat(5_000)
    );
    let sends = [
        outbound(
            "ms_ack",
            MessageKind::Acknowledgement,
            7,
            "On it: fix the flaky test.",
        ),
        outbound("ms_result", MessageKind::Result, 8, &long),
        outbound("ms_ask", MessageKind::Attention, 7, &"why? ".repeat(1_000)),
    ];
    for message in sends {
        let sent = channel.send(message, SendOptions::default()).await.unwrap();
        assert_eq!(sent, SendOutcome::Sent);
    }

    let hub = fake.hub();
    let [ack, result, ask] = &hub.posts[..] else {
        panic!("{} posts", hub.posts.len())
    };
    assert_eq!(ack.number, 7);
    assert_eq!(
        ack.body,
        format!(
            "On it: fix the flaky test.\n\n{}",
            marker(&MessageId::new("ms_ack"))
        )
    );
    assert_eq!(result.number, 8, "each reply goes on its own issue");
    assert!(
        result
            .body
            .starts_with("Fixed, thanks `@alice` and `@acme/core`.\n\n```\n@Override\n```\n\nword"),
        "{}",
        &result.body[..120]
    );
    assert_eq!(message_id(&result.body), "ms_result");
    assert!(result.body.contains("…\n\n_[Cut short: "), "a cut says so");
    assert!(
        result.body.chars().count() < 16_200,
        "{}",
        result.body.chars().count()
    );
    assert_eq!(message_id(&ask.body), "ms_ask");
    assert!(
        ask.body.chars().count() < 2_200,
        "{}",
        ask.body.chars().count()
    );
    assert_eq!(format!("{:?}", Token::new(TOKEN)), "Token(..)");
}

#[tokio::test]
async fn a_repeat_looks_for_its_marker_newest_first_and_posts_only_what_is_missing() {
    let fake = Fake::start().await;
    let t0 = fake.t0;
    {
        let mut hub = fake.hub();
        hub.issue_page = Some(1);
        for id in 1..=3 {
            hub.comments.push(comment(
                id,
                7,
                "alice",
                "chatter",
                t0 - 60_000,
                t0 - 60_000,
                false,
            ));
        }
        // Someone else's comment with ms_3's marker is not the agent's.
        let forged = format!("look\n\n{}", marker(&MessageId::new("ms_3")));
        hub.comments.push(comment(
            4,
            7,
            "mallory",
            &forged,
            t0 - 50_000,
            t0 - 50_000,
            false,
        ));
    }
    let channel = fake.channel();
    let send = |id: &str| outbound(id, MessageKind::Result, 7, id);
    let path = "repos/acme/widgets/issues/7/comments";

    channel
        .send(send("ms_1"), SendOptions::default())
        .await
        .unwrap();
    assert!(fake.reads(path).is_empty(), "a first attempt posts at once");
    assert_eq!(
        channel.send(send("ms_2"), repeat()).await.unwrap(),
        SendOutcome::Sent
    );
    let searched = fake.reads(path).len();
    assert_eq!(searched, 5, "ms_2 was searched for on all five pages first");
    assert_eq!(
        channel.send(send("ms_2"), repeat()).await.unwrap(),
        SendOutcome::Sent
    );
    let pages: Vec<String> = fake.reads(path)[searched..]
        .iter()
        .map(|r| r.2["page"].clone())
        .collect();
    assert_eq!(
        pages,
        ["1", "6"],
        "the newest page is read right after the first"
    );
    assert_eq!(
        channel.send(send("ms_1"), repeat()).await.unwrap(),
        SendOutcome::Sent
    );
    assert_eq!(
        channel.send(send("ms_3"), repeat()).await.unwrap(),
        SendOutcome::Sent
    );

    let ids: Vec<String> = fake
        .hub()
        .posts
        .iter()
        .map(|p| message_id(&p.body).to_owned())
        .collect();
    assert_eq!(
        ids,
        ["ms_1", "ms_2", "ms_3"],
        "each message was posted once"
    );
}

#[tokio::test]
async fn gone_is_for_an_issue_that_can_never_take_it_and_the_rest_is_retried() {
    let fake = Fake::start().await;
    let channel = fake.channel();
    fake.script([
        Reply::Status(404, "Not Found", None),
        Reply::Status(410, "Issues are disabled for this repo", None),
        Reply::Status(
            403,
            "Unable to create comment because issue is locked.",
            None,
        ),
        Reply::Status(500, "Server Error", None),
        Reply::Status(
            403,
            "Resource not accessible by personal access token",
            None,
        ),
        Reply::Status(422, "Validation Failed", None),
    ]);

    for id in ["ms_1", "ms_2", "ms_3"] {
        assert_eq!(send(&channel, id).await.unwrap(), SendOutcome::Gone, "{id}");
    }
    for (id, status) in [("ms_4", "500"), ("ms_5", "403"), ("ms_6", "422")] {
        let err = send(&channel, id).await.unwrap_err();
        assert_eq!(retry_after(&err), None, "{err:#}");
        assert!(format!("{err:#}").contains(status), "{err:#}");
    }

    fake.script([Reply::Status(
        403,
        "You have exceeded a secondary rate limit.",
        Some("7"),
    )]);
    let limited = send(&channel, "ms_7").await.unwrap_err();
    assert_eq!(
        retry_after(&limited),
        Some(Duration::from_secs(7)),
        "{limited:#}"
    );
    let posts = fake.hub().posts.len();
    let held = send(&channel, "ms_8").await.unwrap_err();
    let wait = retry_after(&held).unwrap();
    assert!(
        wait > Duration::from_secs(5) && wait <= Duration::from_secs(7),
        "{wait:?}"
    );
    assert_eq!(
        fake.hub().posts.len(),
        posts,
        "the hold kept the send from GitHub"
    );
    let fresh = fake.channel();
    fake.script([Reply::Status(429, "slow down", None)]);
    let limited = send(&fresh, "ms_9").await.unwrap_err();
    assert_eq!(
        retry_after(&limited),
        Some(Duration::from_secs(60)),
        "{limited:#}"
    );

    let other = fake.channel();
    let mut nowhere = outbound("ms_10", MessageKind::Result, 7, "done");
    nowhere
        .channel
        .address
        .insert("repo".into(), "acme/../x".into());
    let gone = other.send(nowhere, SendOptions::default()).await.unwrap();
    assert_eq!(gone, SendOutcome::Gone);
    let mut deleted = outbound("ms_11", MessageKind::Result, 7, "done");
    deleted.channel = github::reply_to("acme/gadgets", 7);
    assert_eq!(
        other.send(deleted, repeat()).await.unwrap(),
        SendOutcome::Gone
    );
    assert_eq!(
        fake.hub().posts.len(),
        posts + 1,
        "neither lost message was posted"
    );
}

// ---- The runtime with GitHub ----

#[tokio::test]
async fn a_crash_after_a_post_and_before_its_done_mark_does_not_post_twice() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("crash");
    let asked = [request("t1", 7, "Fix the flaky test")];
    fake.script([Reply::Hang]);
    let first = Agent {
        scratch: &scratch,
        source: scratch.triggers(&asked),
        channel: Some(fake.channel()),
        executor: Arc::new(Hanging),
        until_idle: false,
    }
    .runtime();
    tokio::select! {
        ended = first.run(std::future::pending()) => panic!("the runtime ended: {ended:?}"),
        () = fake.until("a post", |hub| (!hub.posts.is_empty()).then_some(())) => {}
    }
    let acknowledged = message_id(&fake.hub().posts[0].body).to_owned();
    let pending =
        scratch.store().effects().unwrap().into_iter().any(|q| {
            matches!(&q.effect, Effect::Send(m) if m.id.as_str() == acknowledged) && !q.done
        });
    assert!(
        pending,
        "GitHub holds the comment, but it was never marked sent"
    );

    let restarted = Agent {
        scratch: &scratch,
        source: scratch.triggers(&asked),
        channel: Some(fake.channel()),
        executor: Arc::new(ScriptedExecutor),
        until_idle: true,
    }
    .runtime();
    tokio::time::timeout(LIMIT, restarted.run(std::future::pending()))
        .await
        .expect("the restart did not go idle")
        .unwrap();

    let hub = fake.hub();
    let ids: Vec<&str> = hub.posts.iter().map(|p| message_id(&p.body)).collect();
    assert_eq!(ids.len(), 2, "the acknowledgement once, then the result");
    assert_eq!(ids[0], acknowledged);
    assert!(hub.posts[0].body.starts_with("On it: Fix the flaky test."));
    assert!(hub.posts[1].body.starts_with("Done: Fix the flaky test."));
    assert!(hub.posts.iter().all(|p| p.number == 7));
    drop(hub);
    assert_eq!(
        fake.reads("repos/acme/widgets/issues/7/comments").len(),
        1,
        "only the redone send searched the issue"
    );
    assert_settled(&scratch.store());
}

// ---- agent serve ----

#[tokio::test]
async fn agent_serve_answers_on_github_and_a_schedule_reports_on_its_issue() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("serve");
    let t0 = fake.t0;
    fake.hub().comments = vec![comment(
        101,
        7,
        "alice",
        "@solos-bot summarise the incident",
        t0 - 60_000,
        t0 - 60_000,
        false,
    )];
    let token = scratch.root.join("github.token");
    std::fs::write(&token, format!("{TOKEN}\n")).unwrap();
    let config = scratch.root.join("agent.yaml");
    std::fs::write(
        &config,
        format!(
            "policy:
  people: [github:alice]
sources:
  - kind: github
    repos: [{REPO}]
    token_file: github.token
    api_base: {}
  - kind: schedule
    schedules:
      - name: triage
        every: 1h
        text: Triage the new issues.
        reply_to: {{kind: github, repo: {REPO}, number: 1}}
channels:
  - kind: github
router: {{kind: scripted}}
executor: {{kind: scripted}}
budgets: {{session_microusd: {BUDGET}}}
",
            fake.api
        ),
    )
    .unwrap();
    let state = scratch.root.join("state");

    let summary = tokio::time::timeout(LIMIT, serve(&config, &state, true))
        .await
        .expect("agent serve did not go idle")
        .unwrap();

    assert!(!token.exists(), "the token file was read once and deleted");
    assert_eq!((summary.triggers_received, summary.effects_pending), (2, 0));
    let hub = fake.hub();
    let on = |number: u64| -> Vec<&str> {
        hub.posts
            .iter()
            .filter(|p| p.number == number)
            .map(|p| p.body.as_str())
            .collect()
    };
    let asked = on(7);
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(
        asked[0].starts_with("On it: summarise the incident."),
        "{}",
        asked[0]
    );
    assert!(
        asked[1].starts_with("Done: summarise the incident."),
        "{}",
        asked[1]
    );
    let swept = on(1);
    assert_eq!(swept.len(), 2, "{swept:?}");
    assert!(
        swept[0].starts_with("On it: Triage the new issues."),
        "{}",
        swept[0]
    );
    assert!(
        swept[1].starts_with("Done: Triage the new issues."),
        "{}",
        swept[1]
    );
    for body in asked.iter().chain(&swept) {
        message_id(body);
    }
    drop(hub);
    assert!(
        scratch.cursor().exists(),
        "the read position is kept in the state dir"
    );
    let ids: Vec<String> = scratch
        .store()
        .triggers()
        .unwrap()
        .iter()
        .map(|t| t.id.to_string())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], "github:acme/widgets:comment:101");
    assert!(ids[1].starts_with("schedule:triage:"), "{}", ids[1]);
    assert_settled(&scratch.store());
}
