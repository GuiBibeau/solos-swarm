//! The GitHub adapters against a fake GitHub: an HTTP server for the REST
//! endpoints they call, with `since`, pages, ETags and rate limits, and for
//! the GitHub App's own (its JWT, its installations and the installation
//! tokens it mints, whose scope the fake records on every request). No real
//! GitHub is contacted.

use anyhow::Result;
use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::RsaPrivateKey;
use rsa::RsaPublicKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::signature::Verifier;
use serde_json::{Value, json};
use sha2::Sha256;
use solos_agent_core::AllowlistPolicy;
use solos_agent_core::change::{Deliverable, RepoName};
use solos_agent_core::model::{
    Actor, ActorKind, Addressed, AssignmentId, ChannelRef, Effect, MessageId, MessageKind,
    OutboundMessage, ThreadKey, TicketId, Trigger, TriggerId,
};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::config::{AgentConfig, ForgeConfig, GithubConfig, RepoConfig};
use solos_swarm::agent::github::{
    self, Access, AppId, AppKey, CURSOR_FILE, GithubApi, GithubApp, GithubChannel, GithubSource,
    IDENTITIES_FILE, Identities, InstallState, Minter, MinterConfig, OVERLAP, REFRESH_MARGIN,
    SUSPENDED_WAIT, marker,
};
use solos_swarm::agent::jsonl::JsonlSource;
use solos_swarm::agent::ports::{
    BoxFuture, Channel, Delivery, Executor, FactSink, Launch, RetryAfter, SendOptions, SendOutcome,
    Source, TicketDirs,
};
use solos_swarm::agent::scripted::{ScriptedExecutor, ScriptedRouter};
use solos_swarm::agent::utc;
use solos_swarm::agent::{
    Adapters, PiHost, RunOptions, Runtime, STORE_FILE, ServeOptions, Summary, forge, serve,
    serve_with,
};
use solos_swarm::gateway::{Dispatch, Provider};
use solos_swarm::harness::scripted::{Call, ScriptedHarness};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
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
/// The App the fake knows: its numeric id, its slug and its bot's account id.
const APP_ID: u64 = 123_456;
const SLUG: &str = "solos-bot";
const BOT_ID: u64 = 4_242;
/// The bot's login, which the agent's own comments carry.
const ME: &str = "solos-bot[bot]";
const REPO: &str = "acme/widgets";
const DOCS: &str = "acme/docs";
const BUDGET: u64 = 500_000;
/// The App's private key, and another that is not its.
const KEY: &str = include_str!("fixtures/github-app-test-key.pem");
const WRONG_KEY: &str = include_str!("fixtures/github-app-wrong-test-key.pem");
const PKCS8_KEY: &str = include_str!("fixtures/github-app-test-key.pkcs8.pem");

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
    /// What the bearer was: the App's JWT, an installation token (named,
    /// with the repositories it is narrowed to and whether it may write), or
    /// nothing valid.
    who: Who,
}

/// The bearer of a request.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Who {
    /// A JWT that verifies against the App's public key.
    App,
    Install {
        token: String,
        repos: Vec<String>,
        write: bool,
    },
    Nobody(&'static str),
}

/// An installation token the fake minted.
struct Minted {
    repos: Vec<String>,
    write: bool,
    expires_ms: u64,
}

/// One mint request, as the fake received it.
#[derive(Clone, Debug)]
struct Mint {
    installation: u64,
    repositories: Value,
    permissions: Value,
}

/// A JWT the fake was shown.
#[derive(Clone, Debug)]
struct Jwt {
    header: Value,
    claims: Value,
    verified: bool,
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
    // ---- the App ----
    /// Installed repositories and the installation each is in.
    installed: BTreeMap<String, u64>,
    suspended: bool,
    /// The permissions an installation reports.
    granted: Value,
    tokens: BTreeMap<String, Minted>,
    next_token: u64,
    mints: Vec<Mint>,
    jwts: Vec<Jwt>,
    /// Account ids by login; a login not here has `1000 + its length`.
    accounts: BTreeMap<String, u64>,
    /// `GET /users/{login}` answers 404.
    lookups_fail: bool,
    /// Every installation token is refused with a 401.
    refuse_installs: bool,
}

/// Time the test moves: what the fake's JWT and token checks and the Minter
/// both read, so a token near its expiry is provable without sleeping.
#[derive(Clone)]
struct Clock(Arc<AtomicU64>);

impl Clock {
    fn new() -> Self {
        Self(Arc::new(AtomicU64::new(now_secs() * 1_000)))
    }
    fn ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.ms())
    }
    fn advance(&self, by: Duration) {
        self.0.fetch_add(by.as_millis() as u64, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct Shared {
    hub: Arc<Mutex<Hub>>,
    api: String,
    clock: Clock,
    /// The public half of the App's key.
    public: RsaPublicKey,
}

struct Fake {
    shared: Shared,
    api: String,
    /// Now, in whole seconds, as GitHub writes times.
    t0: u64,
    task: JoinHandle<()>,
    /// Where each fresh API keeps its account pins.
    dir: PathBuf,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
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

/// A JWT's bearer: valid when RS256 signed by the App's key, issued by the
/// App, short-lived and not in the future or expired by the fake's clock.
fn jwt(shared: &Shared, bearer: &str) -> Result<(), &'static str> {
    const BAD: &str = "A JSON web token could not be decoded";
    let mut parts = bearer.split('.');
    let (Some(head), Some(body), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(BAD);
    };
    let decode = |part: &str| -> Option<Value> {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).ok()?).ok()
    };
    let (Some(header), Some(claims)) = (decode(head), decode(body)) else {
        return Err(BAD);
    };
    let signature = URL_SAFE_NO_PAD
        .decode(sig)
        .ok()
        .and_then(|bytes| Signature::try_from(bytes.as_slice()).ok());
    let message = format!("{head}.{body}");
    let signed = signature.is_some_and(|signature| {
        VerifyingKey::<Sha256>::new(shared.public.clone())
            .verify(message.as_bytes(), &signature)
            .is_ok()
    });
    let now = shared.clock.ms() / 1_000;
    let (iat, exp) = (claims["iat"].as_u64(), claims["exp"].as_u64());
    let timed = matches!((iat, exp), (Some(iat), Some(exp))
        if iat <= now && exp > now && exp - iat <= 600);
    let verified = header["alg"] == "RS256" && claims["iss"] == APP_ID && signed && timed;
    shared.hub.lock().unwrap().jwts.push(Jwt {
        header,
        claims,
        verified,
    });
    if signed && !verified {
        return Err("'Expiration time' claim ('exp') or the issuer is not acceptable");
    }
    if verified { Ok(()) } else { Err(BAD) }
}

/// What a bearer is.
fn who(shared: &Shared, bearer: Option<&str>) -> Who {
    let Some(bearer) = bearer else {
        return Who::Nobody("Requires authentication");
    };
    if !bearer.starts_with("ghs_fake_") {
        return match jwt(shared, bearer) {
            Ok(()) => Who::App,
            Err(why) => Who::Nobody(why),
        };
    }
    let hub = shared.hub.lock().unwrap();
    match hub.tokens.get(bearer) {
        Some(minted)
            if !hub.refuse_installs
                && minted.expires_ms > shared.clock.ms()
                && minted
                    .repos
                    .iter()
                    .all(|repo| hub.installed.contains_key(repo)) =>
        {
            Who::Install {
                token: bearer.to_owned(),
                repos: minted.repos.clone(),
                write: minted.write,
            }
        }
        _ => Who::Nobody("Bad credentials"),
    }
}

/// Records each request and its bearer, checks the bearer and its scope, and
/// applies a scripted limit.
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
    let bearer = header(header::AUTHORIZATION);
    let who = who(
        &shared,
        bearer.as_deref().and_then(|b| b.strip_prefix("Bearer ")),
    );
    let for_the_app =
        path == "app" || path.ends_with("/installation") || path.starts_with("app/installations/");
    let segments: Vec<&str> = path.split('/').collect();
    let on_repo = (segments.len() >= 3 && segments[0] == "repos")
        .then(|| format!("{}/{}", segments[1], segments[2]));
    let refusal = match (&who, for_the_app) {
        (Who::Nobody(why), _) => Some(message(StatusCode::UNAUTHORIZED, why)),
        (Who::App, false) | (Who::Install { .. }, true) => {
            Some(message(StatusCode::UNAUTHORIZED, "Bad credentials"))
        }
        (Who::App, true) => None,
        (Who::Install { repos, write, .. }, false) => {
            if shared.hub.lock().unwrap().suspended {
                Some(message(
                    StatusCode::FORBIDDEN,
                    "This installation has been suspended",
                ))
            } else if on_repo.as_ref().is_some_and(|repo| !repos.contains(repo)) {
                Some(message(StatusCode::NOT_FOUND, "Not Found"))
            } else if request.method() != axum::http::Method::GET && !write {
                Some(message(
                    StatusCode::FORBIDDEN,
                    "Resource not accessible by integration",
                ))
            } else {
                None
            }
        }
    };
    let limit = match refusal {
        None => shared.hub.lock().unwrap().limits.pop_front(),
        Some(_) => None,
    };
    let reset = (now_secs() + 2).to_string();
    let mut response = match (refusal, limit) {
        (Some(refused), _) => refused,
        (None, Some(Limit::Primary)) => {
            let mut response = message(StatusCode::FORBIDDEN, "API rate limit exceeded for user");
            response
                .headers_mut()
                .insert("x-ratelimit-reset", reset.parse().unwrap());
            response
        }
        (None, Some(Limit::Secondary)) => {
            let mut response = message(
                StatusCode::TOO_MANY_REQUESTS,
                "You have exceeded a secondary rate limit.",
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            response
        }
        (None, Some(Limit::Exhausted)) => {
            let mut response = next.run(request).await;
            response
                .headers_mut()
                .insert("x-ratelimit-reset", reset.parse().unwrap());
            response
        }
        (None, None) => next.run(request).await,
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
        who,
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

/// The App, as `GET /app` shows it to its JWT.
async fn app() -> Response {
    Json(json!({
        "id": APP_ID,
        "slug": SLUG,
        "name": SLUG,
        "html_url": format!("https://github.com/apps/{SLUG}"),
    }))
    .into_response()
}

/// The installation that holds a repository, as its JWT sees it.
async fn installation(
    State(shared): State<Shared>,
    Path((owner, repo)): Path<(String, String)>,
) -> Response {
    let hub = shared.hub.lock().unwrap();
    match hub.installed.get(&format!("{owner}/{repo}")) {
        None => message(StatusCode::NOT_FOUND, "Not Found"),
        Some(id) => Json(json!({
            "id": id,
            "suspended_at": hub.suspended.then(|| ts(now_secs() * 1_000)),
            "permissions": hub.granted,
        }))
        .into_response(),
    }
}

/// An installation token, narrowed as the request asks.
async fn mint(
    State(shared): State<Shared>,
    Path(installation): Path<u64>,
    Json(body): Json<Value>,
) -> Response {
    let mut hub = shared.hub.lock().unwrap();
    hub.mints.push(Mint {
        installation,
        repositories: body["repositories"].clone(),
        permissions: body["permissions"].clone(),
    });
    if !hub.installed.values().any(|id| *id == installation) {
        return message(StatusCode::NOT_FOUND, "Not Found");
    }
    if hub.suspended {
        return message(
            StatusCode::FORBIDDEN,
            "This installation has been suspended",
        );
    }
    let repos: Vec<String> = body["repositories"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| Some(format!("acme/{}", name.as_str()?)))
        .collect();
    if repos.is_empty()
        || repos
            .iter()
            .any(|repo| hub.installed.get(repo) != Some(&installation))
    {
        return message(
            StatusCode::UNPROCESSABLE_ENTITY,
            "There is at least one repository that does not exist or is not accessible to the parent installation.",
        );
    }
    let write = ["contents", "pull_requests", "issues"]
        .iter()
        .any(|name| body["permissions"][*name] == "write");
    hub.next_token += 1;
    let token = format!("ghs_fake_{}", hub.next_token);
    let expires_ms = shared.clock.ms() + 3_600_000;
    hub.tokens.insert(
        token.clone(),
        Minted {
            repos,
            write,
            expires_ms,
        },
    );
    (
        StatusCode::CREATED,
        Json(json!({
            "token": token,
            "expires_at": ts(expires_ms),
            "permissions": body["permissions"],
            "repository_selection": "selected",
        })),
    )
        .into_response()
}

/// A public account: the bot's, or a person's, whose id is what a pin holds.
async fn account(State(shared): State<Shared>, Path(login): Path<String>) -> Response {
    let hub = shared.hub.lock().unwrap();
    if login == ME {
        return Json(json!({ "login": ME, "id": BOT_ID, "type": "Bot" })).into_response();
    }
    if hub.lookups_fail {
        return message(StatusCode::NOT_FOUND, "Not Found");
    }
    let id = hub
        .accounts
        .get(&login)
        .copied()
        .unwrap_or(1_000 + login.len() as u64);
    Json(json!({ "login": login, "id": id, "type": "User" })).into_response()
}

async fn branch(Path((owner, repo, branch)): Path<(String, String, String)>) -> Response {
    if ours(&owner, &repo) && branch == "main" {
        Json(json!({ "name": "main" })).into_response()
    } else {
        message(StatusCode::NOT_FOUND, "Branch not found")
    }
}

async fn repository(Path((owner, repo)): Path<(String, String)>) -> Response {
    if !ours(&owner, &repo) && format!("{owner}/{repo}") != DOCS {
        return message(StatusCode::NOT_FOUND, "Not Found");
    }
    Json(json!({ "full_name": format!("{owner}/{repo}") })).into_response()
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
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agent-github-fake-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let hub = Hub {
            installed: BTreeMap::from([(REPO.to_owned(), 7_788), (DOCS.to_owned(), 7_788)]),
            granted: Access::app_permissions(),
            ..Hub::default()
        };
        let shared = Shared {
            hub: Arc::new(Mutex::new(hub)),
            api: api.clone(),
            clock: Clock::new(),
            public: RsaPublicKey::from(&RsaPrivateKey::from_pkcs1_pem(KEY).unwrap()),
        };
        let numbered = "/api/repos/{owner}/{repo}/issues/{number}/comments";
        let app = axum::Router::new()
            .route("/api/app", get(app))
            .route("/api/repos/{owner}/{repo}/installation", get(installation))
            .route("/api/app/installations/{id}/access_tokens", post(mint))
            .route("/api/users/{login}", get(account))
            .route("/api/repos/{owner}/{repo}/branches/{branch}", get(branch))
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
            dir,
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

    /// The App's Minter on the fake's clock, for `forge.repos` of `REPO` and
    /// `DOCS`, with `key`.
    fn minter_with(&self, key: &str, adjust: impl FnOnce(&mut MinterConfig)) -> Arc<Minter> {
        let mut config = MinterConfig::new(
            AppId(APP_ID),
            &self.api,
            true,
            [repo(REPO), repo(DOCS)].into(),
        );
        adjust(&mut config);
        let clock = self.shared.clock.clone();
        let now: github::Now = Arc::new(move || clock.now());
        Arc::new(Minter::new(config, AppKey::parse(key).unwrap(), now).unwrap())
    }

    fn minter(&self) -> Arc<Minter> {
        self.minter_with(KEY, |_| {})
    }

    /// The Policy's GitHub logins, as `serve` lists them for pinning.
    fn listed() -> std::collections::BTreeSet<String> {
        let policy = policy();
        policy
            .people
            .iter()
            .chain(&policy.bots)
            .filter_map(|who| who.strip_prefix("github:"))
            .map(str::to_owned)
            .collect()
    }

    /// An API on `minter` whose pins are in `pins`.
    fn api_on(&self, minter: Arc<Minter>, pins: PathBuf) -> Arc<GithubApi> {
        let identities = Identities::load(pins, Self::listed()).unwrap();
        Arc::new(GithubApi::new(minter, Arc::new(identities)).unwrap())
    }

    /// Each call has its own rate-limit hold and its own pins, as a restart
    /// would.
    fn api(&self) -> Arc<GithubApi> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let pins = self.dir.join(format!(
            "pins-{}.json",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        self.api_on(self.minter(), pins)
    }

    fn source_on(&self, api: Arc<GithubApi>, scratch: &Scratch, follow: bool) -> Box<dyn Source> {
        let source = GithubSource::new(api, [repo(REPO)], scratch.cursor(), follow)
            .with_poll(Duration::from_millis(20))
            .with_backoff(Duration::from_millis(10), Duration::from_millis(100));
        Box::new(source)
    }

    /// A Source whose pins are in the scratch state directory, so a second
    /// Source on it is a restart.
    fn source(&self, scratch: &Scratch, follow: bool) -> Box<dyn Source> {
        let api = self.api_on(self.minter(), scratch.pins());
        self.source_on(api, scratch, follow)
    }

    fn channel(&self) -> Arc<dyn Channel> {
        Arc::new(GithubChannel::new(self.api()))
    }

    fn mints(&self) -> Vec<Mint> {
        self.hub().mints.clone()
    }

    /// What every request carried, the App's own calls apart.
    fn scopes(&self, method: &str) -> Vec<(String, Who)> {
        self.hub()
            .requests
            .iter()
            .filter(|r| r.method == method)
            .map(|r| (r.path.clone(), r.who.clone()))
            .collect()
    }

    fn tokens_minted(&self) -> usize {
        self.hub().tokens.len()
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
    let id = if login == ME {
        BOT_ID
    } else {
        1_000 + login.len() as u64
    };
    json!({ "login": login, "id": id, "type": kind })
}

fn repo(name: &str) -> RepoName {
    RepoName::try_from(name.to_owned()).unwrap()
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
    fn pins(&self) -> PathBuf {
        self.root.join("state").join(IDENTITIES_FILE)
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
        ask: None,
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
    assert_eq!(fake.reads("app").len(), 1, "the bot is learned once");
    assert_eq!(fake.reads("users/solos-bot[bot]").len(), 1);
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

    let token = fake
        .minter()
        .token(&repo(REPO), Access::Read)
        .await
        .unwrap();
    assert_eq!(format!("{token:?}"), "InstallToken(acme/widgets Read, ..)");
    assert_eq!(format!("{:?}", AppKey::parse(KEY).unwrap()), "AppKey(..)");

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
        Reply::Status(403, "Resource not accessible by integration", None),
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

// ---- The App's credentials ----

#[tokio::test]
async fn the_jwt_is_rs256_issued_by_the_app_signed_by_its_key_and_lives_under_ten_minutes() {
    let fake = Fake::start().await;

    fake.minter()
        .token(&repo(REPO), Access::Read)
        .await
        .unwrap();

    let jwts = fake.hub().jwts.clone();
    assert_eq!(
        jwts.len(),
        2,
        "the installation lookup and the mint: {jwts:?}"
    );
    let now = fake.shared.clock.ms() / 1_000;
    for jwt in &jwts {
        assert_eq!(jwt.header, json!({"alg": "RS256", "typ": "JWT"}));
        assert_eq!(jwt.claims["iss"], APP_ID);
        let (iat, exp) = (
            jwt.claims["iat"].as_u64().unwrap(),
            jwt.claims["exp"].as_u64().unwrap(),
        );
        assert_eq!((iat, exp), (now - 60, now + 540), "{jwt:?}");
        assert!(exp - iat <= 600, "GitHub refuses a JWT that lives longer");
        assert!(
            jwt.verified,
            "the signature verifies against the App's public key"
        );
    }
}

#[tokio::test]
async fn a_wrong_key_is_refused_and_stops_only_the_github_source() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("wrong-key");
    let wrong = fake.minter_with(WRONG_KEY, |_| {});

    let refused = wrong.bot().await.unwrap_err();

    let said = format!("{refused:#}");
    assert!(said.contains("GitHub refused the App's key"), "{said}");
    assert!(said.contains("HTTP 401"), "{said}");
    let github = fake.source_on(fake.api_on(wrong, scratch.pins()), &scratch, true);
    let asked = [request("t1", 7, "Fix the flaky test")];
    let adapters = Adapters {
        sources: vec![github, scratch.triggers(&asked)],
        channels: BTreeMap::new(),
        router: Arc::new(ScriptedRouter::new(BUDGET)),
        executor: Arc::new(ScriptedExecutor),
    };
    let options = RunOptions {
        until_idle: true,
        tick: Duration::from_millis(10),
        retry_base: Duration::from_millis(10),
        retry_max: Duration::from_millis(500),
    };
    let runtime = Runtime::new(scratch.store(), policy(), adapters, options);

    let summary = tokio::time::timeout(LIMIT, runtime.run(std::future::pending()))
        .await
        .expect("the run did not go idle")
        .expect("a refused key stops the github source, not the agent");

    assert_eq!(
        summary.triggers_received, 1,
        "the other source kept working"
    );
    assert!(
        fake.mints().is_empty(),
        "nothing was minted on a key GitHub refuses"
    );
}

#[tokio::test]
async fn every_mint_names_one_repository_and_exactly_the_permissions_of_its_access() {
    let fake = Fake::start().await;
    let minter = fake.minter();
    for (name, access) in [
        (REPO, Access::Read),
        (REPO, Access::Write),
        (DOCS, Access::Read),
    ] {
        minter.token(&repo(name), access).await.unwrap();
    }

    let read = json!({
        "metadata": "read", "contents": "read", "pull_requests": "read", "issues": "read",
        "checks": "read", "statuses": "read", "actions": "read",
    });
    let write = json!({
        "metadata": "read", "contents": "write", "pull_requests": "write", "issues": "write",
        "checks": "read", "statuses": "read", "actions": "read",
    });
    let mints = fake.mints();
    let asked: Vec<(Value, Value)> = mints
        .iter()
        .map(|m| (m.repositories.clone(), m.permissions.clone()))
        .collect();
    assert_eq!(
        asked,
        [
            (json!(["widgets"]), read.clone()),
            (json!(["widgets"]), write.clone()),
            (json!(["docs"]), read),
        ]
    );
    for mint in &mints {
        assert_eq!(mint.installation, 7_788);
        assert!(mint.permissions.get("workflows").is_none());
        assert!(mint.permissions.get("administration").is_none());
    }
    assert_eq!(Access::app_permissions(), write, "what the App must hold");
    let before = fake.hub().requests.len();
    let elsewhere = minter
        .token(&repo("acme/elsewhere"), Access::Read)
        .await
        .unwrap_err();
    assert!(
        elsewhere.to_string().contains("not in forge.repos"),
        "{elsewhere:#}"
    );
    assert_eq!(
        fake.hub().requests.len(),
        before,
        "no token is minted for a repository outside forge.repos, and nothing is asked"
    );
}

#[tokio::test]
async fn twenty_concurrent_calls_make_one_mint_narrowed_to_one_repository() {
    let fake = Fake::start().await;
    let minter = fake.minter();
    let calls = (0..20).map(|_| {
        let minter = minter.clone();
        tokio::spawn(async move { minter.token(&repo(REPO), Access::Read).await.unwrap() })
    });

    let tokens: Vec<_> = futures_util::future::join_all(calls)
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();

    let mints = fake.mints();
    assert_eq!(mints.len(), 1, "{mints:?}");
    assert_eq!(mints[0].repositories, json!(["widgets"]));
    assert_eq!(fake.reads("repos/acme/widgets/installation").len(), 1);
    let expiry = tokens[0].expires();
    assert!(
        tokens
            .iter()
            .all(|t| t.expires() == expiry && t.repo == repo(REPO) && t.access == Access::Read),
        "every caller got the one token"
    );

    // The same through the Channel: twenty posts at once share one write token.
    let channel = fake.channel();
    let posts = (0..20).map(|n| {
        let channel = channel.clone();
        tokio::spawn(async move { send(&channel, &format!("ms_{n}")).await.unwrap() })
    });
    for sent in futures_util::future::join_all(posts).await {
        assert_eq!(sent.unwrap(), SendOutcome::Sent);
    }
    let used: BTreeSet<(String, Vec<String>, bool)> = fake
        .scopes("POST")
        .into_iter()
        .filter_map(|(_, who)| match who {
            Who::Install {
                token,
                repos,
                write,
            } => Some((token, repos, write)),
            _ => None,
        })
        .collect();
    assert_eq!(used.len(), 1, "{used:?}");
    let (_, repos, write) = used.into_iter().next().unwrap();
    assert_eq!((repos, write), (vec![REPO.to_owned()], true));
    assert_eq!(fake.mints().len(), 2, "one more, for the write");
}

#[tokio::test]
async fn a_get_carries_a_read_token_and_a_post_a_write_token_for_the_one_repository() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("scopes");
    let channel = fake.channel();

    channel
        .send(outbound("ms_1", MessageKind::Result, 7, "done"), repeat())
        .await
        .unwrap();
    read_once(&fake, &scratch).await;

    let here = vec![REPO.to_owned()];
    let reads: Vec<_> = fake
        .scopes("GET")
        .into_iter()
        .filter(|(path, _)| path.starts_with("repos/acme/widgets/issues"))
        .collect();
    assert!(!reads.is_empty());
    for (path, who) in &reads {
        let Who::Install { repos, write, .. } = who else {
            panic!("{path}: {who:?}")
        };
        assert_eq!((repos, write), (&here, &false), "{path}");
    }
    let posts = fake.scopes("POST");
    let comments: Vec<_> = posts
        .iter()
        .filter(|(path, _)| path == "repos/acme/widgets/issues/7/comments")
        .collect();
    assert_eq!(comments.len(), 1);
    assert!(
        matches!(&comments[0].1, Who::Install { repos, write: true, .. } if repos == &here),
        "{:?}",
        comments[0]
    );
    assert!(
        fake.hub()
            .requests
            .iter()
            .filter(|r| r.method == "GET")
            .all(|r| !matches!(r.who, Who::Install { write: true, .. })),
        "no write token ever carried a read"
    );
    let (with_a_jwt, shown) = {
        let hub = fake.hub();
        (
            hub.requests.iter().filter(|r| r.who == Who::App).count(),
            hub.jwts.len(),
        )
    };
    assert_eq!(
        with_a_jwt, shown,
        "the App's own calls are the only ones with a JWT"
    );
}

#[tokio::test]
async fn a_401_on_a_cached_token_mints_again_once_and_a_second_is_the_error() {
    let fake = Fake::start().await;
    let channel = fake.channel();
    let sent = |id: &str| channel.send(outbound(id, MessageKind::Result, 7, "done"), repeat());
    assert_eq!(sent("ms_1").await.unwrap(), SendOutcome::Sent);
    assert_eq!(
        fake.mints().len(),
        3,
        "a read for the bot's account, a read for the search, a write for the post"
    );

    // GitHub revokes what it minted.
    fake.hub().tokens.clear();
    assert_eq!(sent("ms_2").await.unwrap(), SendOutcome::Sent);
    assert_eq!(
        fake.mints().len(),
        5,
        "each refused token was minted again, once"
    );

    // A token GitHub keeps refusing is the error after the one retry.
    fake.hub().refuse_installs = true;
    let err = sent("ms_3").await.unwrap_err();
    assert!(
        format!("{err:#}").contains("refused a fresh installation token"),
        "{err:#}"
    );
    assert_eq!(fake.mints().len(), 6);
}

#[tokio::test]
async fn a_token_is_reused_until_fifteen_minutes_before_it_expires() {
    let fake = Fake::start().await;
    let minter = fake.minter();
    assert_eq!(REFRESH_MARGIN, Duration::from_secs(15 * 60));
    let first = minter.token(&repo(REPO), Access::Read).await.unwrap();
    assert_eq!(
        first.expires(),
        fake.shared.clock.now() + Duration::from_secs(3_600)
    );

    fake.shared.clock.advance(Duration::from_secs(44 * 60));
    let reused = minter.token(&repo(REPO), Access::Read).await.unwrap();
    assert_eq!(
        fake.mints().len(),
        1,
        "16 minutes remain: the token is reused"
    );
    assert_eq!(reused.expires(), first.expires());

    fake.shared.clock.advance(Duration::from_secs(2 * 60));
    let again = minter.token(&repo(REPO), Access::Read).await.unwrap();
    assert_eq!(
        fake.mints().len(),
        2,
        "14 minutes remain: it is minted again"
    );
    assert!(again.expires() > first.expires());
    assert_eq!(
        fake.mints()[1].repositories,
        json!(["widgets"]),
        "the refresh is narrowed as the first mint was"
    );
}

#[tokio::test]
async fn writes_false_mints_no_write_token_and_still_reads() {
    let fake = Fake::start().await;
    let minter = fake.minter_with(KEY, |config| config.writes = false);
    let channel: Arc<dyn Channel> = Arc::new(GithubChannel::new(
        fake.api_on(minter.clone(), fake.dir.join("pins.json")),
    ));

    let refused = send(&channel, "ms_1").await.unwrap_err();

    assert!(
        format!("{refused:#}").contains("github.writes is false"),
        "{refused:#}"
    );
    assert_eq!(retry_after(&refused), None, "waiting would not mend it");
    assert!(fake.hub().posts.is_empty());
    minter
        .token(&repo(REPO), Access::Read)
        .await
        .expect("reads still work");
    let mints = fake.mints();
    assert_eq!(mints.len(), 1);
    assert_eq!(mints[0].permissions["contents"], "read");
}

// ---- The bot ----

#[tokio::test]
async fn the_bot_is_the_apps_slug_and_either_handle_addresses_it() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("bot");
    let minter = fake.minter();
    let bot = minter.bot().await.unwrap();
    assert_eq!(bot.slug, SLUG);
    assert_eq!(bot.login, ME);
    assert_eq!(bot.id, BOT_ID);
    assert_eq!(bot.email, "4242+solos-bot[bot]@users.noreply.github.com");
    let t0 = fake.t0;
    let at = |seconds: u64| t0 - seconds * 1_000;
    fake.hub().comments = vec![
        comment(
            101,
            7,
            "alice",
            "@solos-bot[bot] do it",
            at(60),
            at(60),
            false,
        ),
        comment(102, 7, ME, "@solos-bot On it.", at(50), at(50), false),
        comment(
            103,
            7,
            "alice",
            "@Solos-Bot do it too",
            at(40),
            at(40),
            false,
        ),
        comment(104, 7, "alice", "no one is asked", at(30), at(30), false),
    ];

    let summary = read_once(&fake, &scratch).await;

    assert_eq!(
        summary.triggers_received, 3,
        "the bot's own comment is dropped"
    );
    let mut triggers = scratch.store().triggers().unwrap();
    triggers.sort_by_key(|t| t.at_ms);
    let shown: Vec<(&str, Addressed, &str)> = triggers
        .iter()
        .map(|t| (t.id.as_str(), t.addressed, t.text.as_str()))
        .collect();
    assert_eq!(
        shown,
        [
            (
                "github:acme/widgets:comment:101",
                Addressed::Mention,
                "do it"
            ),
            (
                "github:acme/widgets:comment:103",
                Addressed::Mention,
                "do it too"
            ),
            (
                "github:acme/widgets:comment:104",
                Addressed::Unaddressed,
                "no one is asked"
            ),
        ]
    );
    // The Channel posts as the bot: its post is found again by its login.
    let channel = fake.channel();
    channel
        .send(outbound("ms_1", MessageKind::Result, 7, "done"), repeat())
        .await
        .unwrap();
    assert_eq!(
        channel
            .send(outbound("ms_1", MessageKind::Result, 7, "done"), repeat())
            .await
            .unwrap(),
        SendOutcome::Sent
    );
    assert_eq!(
        fake.hub().posts.len(),
        1,
        "the second was found, not posted"
    );
}

#[tokio::test]
async fn a_listed_login_pinned_to_one_account_is_another_actor_when_another_account_has_it() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("pins");
    let t0 = fake.t0;
    let at = |seconds: u64| t0 - seconds * 1_000;
    let mut impostor = comment(
        101,
        7,
        "alice",
        "@solos-bot approve it",
        at(60),
        at(60),
        false,
    );
    impostor["user"]["id"] = json!(99);
    let real = comment(
        102,
        7,
        "alice",
        "@solos-bot please look",
        at(50),
        at(50),
        false,
    );
    fake.hub().comments = vec![impostor, real];

    let first = read_once(&fake, &scratch).await;

    assert_eq!(first.triggers_received, 2);
    let pins: Value = serde_json::from_slice(&std::fs::read(scratch.pins()).unwrap()).unwrap();
    assert_eq!(pins, json!({"pins": {"alice": 1005, "bob": 1003}}));
    let by = |store: &SqliteStore, id: &str| {
        store
            .triggers()
            .unwrap()
            .into_iter()
            .find(|t| t.id.as_str() == id)
            .unwrap()
            .actor
            .id
    };
    let store = scratch.store();
    assert_eq!(by(&store, "github:acme/widgets:comment:101"), "alice#99");
    assert_eq!(by(&store, "github:acme/widgets:comment:102"), "alice");
    let gated: Vec<String> = store
        .decisions()
        .unwrap()
        .into_iter()
        .map(|d| d.reason)
        .collect();
    assert!(
        gated
            .iter()
            .any(|why| why.contains("comment:101: not_listed")),
        "the stranger holding alice's login is rejected not_listed: {gated:?}"
    );
    assert!(
        gated.iter().all(|why| !why.contains("comment:102")),
        "alice is not: {gated:?}"
    );
    drop(store);

    // The login now names another account. The pin is what it was, after a restart.
    fake.hub().accounts.insert("alice".into(), 99);
    fake.hub().comments.push(comment(
        103,
        7,
        "alice",
        "@solos-bot and again",
        at(10),
        at(10),
        false,
    ));
    read_once(&fake, &scratch).await;
    let after: Value = serde_json::from_slice(&std::fs::read(scratch.pins()).unwrap()).unwrap();
    assert_eq!(after, pins, "a pin never changes by itself");
    assert_eq!(
        fake.reads("users/alice").len(),
        1,
        "a pinned login is not looked up again"
    );
}

#[tokio::test]
async fn a_listed_login_whose_lookup_failed_is_not_admitted_until_it_can_be_pinned() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("unpinned");
    let t0 = fake.t0;
    fake.hub().lookups_fail = true;
    fake.hub().comments = vec![comment(
        101,
        7,
        "alice",
        "@solos-bot please look",
        t0 - 60_000,
        t0 - 60_000,
        false,
    )];

    read_once(&fake, &scratch).await;

    let store = scratch.store();
    let actor = store.triggers().unwrap().remove(0).actor.id;
    assert_eq!(actor, "alice#1005", "no pin, so not the listed alice");
    assert!(
        store
            .decisions()
            .unwrap()
            .iter()
            .any(|d| d.reason.contains("not_listed")),
        "and rejected"
    );
    assert!(!scratch.pins().exists(), "nothing was pinned");
}

// ---- Suspension, uninstall and reinstall ----

#[tokio::test]
async fn a_suspended_installation_makes_every_call_wait_and_work_resumes_without_a_restart() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("suspended");
    assert_eq!(SUSPENDED_WAIT, Duration::from_secs(300));
    let t0 = fake.t0;
    fake.hub().comments = vec![comment(
        101,
        7,
        "alice",
        "@solos-bot look",
        t0 - 60_000,
        t0 - 60_000,
        false,
    )];
    fake.hub().suspended = true;

    // By default a call waits five minutes, and sends GitHub nothing.
    let patient = fake.channel();
    let err = send(&patient, "ms_0").await.unwrap_err();
    assert_eq!(retry_after(&err), Some(Duration::from_secs(300)), "{err:#}");
    assert!(format!("{err:#}").contains("suspended"), "{err:#}");
    assert!(fake.hub().posts.is_empty());

    // A short wait, so the test can watch the Source and a queued send resume.
    let minter = fake.minter_with(KEY, |config| {
        config.suspended_wait = Duration::from_millis(100)
    });
    let api = fake.api_on(minter, scratch.pins());
    let channel: Arc<dyn Channel> = Arc::new(GithubChannel::new(api.clone()));
    let waiting = send(&channel, "ms_1").await.unwrap_err();
    assert_eq!(retry_after(&waiting), Some(Duration::from_millis(100)));
    let runtime = Agent {
        scratch: &scratch,
        source: fake.source_on(api, &scratch, true),
        channel: None,
        executor: Arc::new(ScriptedExecutor),
        until_idle: false,
    }
    .runtime();
    let driver = async {
        fake.until("the Source to look again", |hub| {
            let looks = hub
                .requests
                .iter()
                .filter(|r| r.path == "repos/acme/widgets/installation")
                .count();
            (looks >= 3).then_some(())
        })
        .await;
        assert!(
            fake.reads("repos/acme/widgets/issues/comments").is_empty(),
            "nothing is listed while the App is suspended"
        );
        fake.hub().suspended = false;
        until_triggers(&scratch, 1).await;
        assert_eq!(
            send(&channel, "ms_1").await.unwrap(),
            SendOutcome::Sent,
            "the send that waited goes through, with no restart"
        );
    };

    let (summary, ()) = drive(runtime, driver).await;

    assert_eq!(
        summary.triggers_received, 1,
        "the agent kept running while suspended"
    );
}

#[tokio::test]
async fn an_uninstalled_app_waits_and_a_reinstall_with_a_new_installation_is_found() {
    let fake = Fake::start().await;
    let minter = fake.minter_with(KEY, |config| {
        config.suspended_wait = Duration::from_millis(50)
    });
    let channel: Arc<dyn Channel> = Arc::new(GithubChannel::new(
        fake.api_on(minter, fake.dir.join("pins.json")),
    ));
    assert_eq!(send(&channel, "ms_1").await.unwrap(), SendOutcome::Sent);
    assert_eq!(fake.mints().last().unwrap().installation, 7_788);

    fake.hub().installed.remove(REPO);
    let err = send(&channel, "ms_2").await.unwrap_err();
    assert_eq!(
        retry_after(&err),
        Some(Duration::from_millis(50)),
        "{err:#}"
    );
    assert!(
        format!("{err:#}").contains("not installed on acme/widgets"),
        "{err:#}"
    );

    fake.hub().installed.insert(REPO.to_owned(), 7_789);
    assert_eq!(send(&channel, "ms_2").await.unwrap(), SendOutcome::Sent);
    assert_eq!(
        fake.mints().last().unwrap().installation,
        7_789,
        "the new installation was found"
    );
}

// ---- The App's key ----

fn temp_dir(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agent-github-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_key_file_is_read_once_and_refused_if_it_is_open_a_link_or_not_an_rsa_key() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = temp_dir("key");
    let private = |name: &str, pem: &str| {
        let path = dir.join(name);
        solos_swarm::sealed::private_file(&path, pem.as_bytes(), true).unwrap();
        path
    };

    let once = private("once.pem", KEY);
    AppKey::take(&once).unwrap();
    assert!(!once.exists(), "serve reads it once and deletes it");
    let kept = private("kept.pem", KEY);
    AppKey::read(&kept).unwrap();
    assert!(kept.exists(), "check reads it and deletes nothing");
    AppKey::take(&private("pkcs8.pem", PKCS8_KEY)).expect("PKCS#8 is accepted too");

    let open = private("open.pem", KEY);
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).unwrap();
    let refused = format!("{:#}", AppKey::take(&open).unwrap_err());
    assert!(
        refused.contains(&open.display().to_string())
            && refused.contains("open to group or others"),
        "{refused}"
    );
    assert!(open.exists(), "a refused key is left where it was");
    let link = dir.join("link.pem");
    symlink(&kept, &link).unwrap();
    let refused = format!("{:#}", AppKey::take(&link).unwrap_err());
    assert!(
        refused.contains(&link.display().to_string()) && refused.contains("is a symlink"),
        "{refused}"
    );
    let junk = private("junk.pem", "-----BEGIN RSA PRIVATE KEY-----\nnot a key\n");
    let refused = format!("{:#}", AppKey::take(&junk).unwrap_err());
    assert!(
        refused.contains(&junk.display().to_string())
            && refused.contains("is not an RSA private key"),
        "{refused}"
    );
    assert!(!junk.exists(), "a one-use copy is gone either way");
    let missing = format!("{:#}", AppKey::take(&dir.join("none.pem")).unwrap_err());
    assert!(
        missing.contains("cannot read the GitHub App key"),
        "{missing}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// What `agent.yaml` says for an agent that acts as the fake's App.
fn app_config(fake: &Fake, root: &std::path::Path, key: &str, extra: &str) -> PathBuf {
    let yaml = format!(
        "policy:
  people: [github:alice]
github:
  app_id: {APP_ID}
  key_file: {key}
  api_base: {}
  git_base: file://{}/remote/
sources:
  - kind: github
  - kind: schedule
    schedules:
      - name: triage
        every: 1h
        text: Triage the new issues.
        reply_to: {{kind: github, repo: {REPO}, number: 1}}
channels:
  - kind: github
router: {{kind: scripted}}
executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {{input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}}
  max_output_tokens: 2048
  run_code: {{{extra}}}
forge:
  kind: github
  repos:
    {REPO}: {{about: \"Widgets, in Rust.\", base: main}}
budgets: {{session_microusd: {BUDGET}}}
",
        fake.api,
        root.display()
    );
    let path = root.join("agent.yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

#[tokio::test]
async fn serve_refuses_a_key_under_a_path_a_sandbox_can_read_before_it_reads_it() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("exposed");
    let shared = scratch.root.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    let key = shared.join("github-app.pem");
    solos_swarm::sealed::private_file(&key, KEY.as_bytes(), true).unwrap();
    let mounts = format!("mounts: [{{target: /data, source: {}}}]", shared.display());
    let config = app_config(&fake, &scratch.root, &key.display().to_string(), &mounts);

    let refused = serve(
        &config,
        &scratch.root.join("state"),
        ServeOptions {
            until_idle: true,
            ..ServeOptions::default()
        },
    )
    .await
    .unwrap_err();

    let said = format!("{refused:#}");
    assert!(
        said.contains("github.key_file")
            && said.contains(&key.display().to_string())
            && said.contains("which a sandbox can read"),
        "{said}"
    );
    assert!(key.exists(), "the key was refused, not read and deleted");
    assert!(fake.mints().is_empty());
}

#[test]
fn the_manifest_asks_for_exactly_the_permissions_the_app_uses() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/agent-mode/github-app/manifest.json"
    );
    let manifest: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();

    assert_eq!(manifest["default_permissions"], Access::app_permissions());
    assert_eq!(manifest["hook_attributes"]["active"], false);
    assert_eq!(manifest["default_events"], json!([]));
    assert_eq!(manifest["public"], false);
}

// ---- The forge ----

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Alice",
            "-c",
            "user.email=alice@example.com",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn files(dir: &std::path::Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            files(&path, found);
        } else if entry.file_type().unwrap().is_file() {
            found.push(path);
        }
    }
}

#[tokio::test]
async fn the_mirror_fetches_with_a_read_token_for_the_one_repository_and_the_clone_is_the_bots() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("forge");
    let upstream = scratch.root.join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(upstream.join("README.md"), "Widgets\n").unwrap();
    git(&upstream, &["add", "README.md"]);
    git(&upstream, &["commit", "--quiet", "-m", "first"]);
    std::fs::create_dir_all(scratch.root.join("remote/acme")).unwrap();
    git(
        &scratch.root,
        &[
            "clone",
            "--quiet",
            "--bare",
            "upstream",
            "remote/acme/widgets.git",
        ],
    );
    let tip = git(&upstream, &["rev-parse", "main"]);
    let key = scratch.root.join("github-app.pem");
    solos_swarm::sealed::private_file(&key, KEY.as_bytes(), true).unwrap();
    let config = GithubConfig {
        app_id: APP_ID,
        key_file: key.clone(),
        poll_secs: 60,
        reconcile_secs: 300,
        writes: true,
        api_base: fake.api.clone(),
        git_base: format!("file://{}/remote/", scratch.root.display()),
    };
    let app = GithubApp::open(
        &config,
        [repo(REPO)].into(),
        &scratch.root.join("state"),
        Fake::listed(),
    )
    .unwrap();
    assert!(!key.exists(), "the key was read once and deleted");
    let repos = BTreeMap::from([(
        repo(REPO),
        RepoConfig {
            about: "Widgets.".into(),
            base: "main".into(),
            budget_microusd: None,
            notes: None,
            owes: Deliverable::Answer,
        },
    )]);
    let forge = forge::open(
        &ForgeConfig::Github { repos },
        scratch.root.join("state/forge"),
        Some(&app),
    )
    .unwrap();
    let (work, host) = (
        scratch.root.join("tk_1/work"),
        scratch.root.join("tk_1/host"),
    );
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&host).unwrap();

    let workspace = forge
        .prepare(
            &repo(REPO),
            TicketDirs {
                work: &work,
                host: &host,
            },
        )
        .await
        .unwrap();

    assert_eq!(workspace.base_commit, tip);
    assert_eq!(git(&work, &["rev-parse", "HEAD"]), tip);
    assert_eq!(
        git(&work, &["config", "--local", "user.name"]),
        "solos-bot[bot]",
        "the clone's identity is derived from the App"
    );
    assert_eq!(
        git(&work, &["config", "--local", "user.email"]),
        "4242+solos-bot[bot]@users.noreply.github.com"
    );
    let mints = fake.mints();
    assert_eq!(mints.len(), 1, "{mints:?}");
    assert_eq!(mints[0].repositories, json!(["widgets"]));
    assert_eq!(
        mints[0].permissions["contents"], "read",
        "the mirror's token reads"
    );
    assert_eq!(fake.tokens_minted(), 1);
    let minted: Vec<String> = fake.hub().tokens.keys().cloned().collect();
    let mut written = Vec::new();
    files(&scratch.root, &mut written);
    for path in written {
        let bytes = std::fs::read(&path).unwrap();
        for token in &minted {
            assert!(
                !bytes
                    .windows(token.len())
                    .any(|window| window == token.as_bytes()),
                "{} holds a token",
                path.display()
            );
        }
    }
    forge
        .prepare(
            &repo(REPO),
            TicketDirs {
                work: &work,
                host: &host,
            },
        )
        .await
        .unwrap();
    assert_eq!(fake.mints().len(), 1, "a finished clone needs no token");
}

// ---- agent github check ----

#[tokio::test]
async fn check_prints_how_the_app_stands_and_refuses_what_it_must_not_hold() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("check");
    let key = scratch.root.join("github-app.pem");
    solos_swarm::sealed::private_file(&key, KEY.as_bytes(), true).unwrap();
    let config = app_config(&fake, &scratch.root, "github-app.pem", "");
    let run = |scratch: &Scratch| {
        let args = github::CheckArgs {
            config: config.clone(),
            key_file: Some(key.clone()),
            state_dir: Some(scratch.root.join("state")),
        };
        async move {
            let mut out = Vec::new();
            let ok = github::check(args, &mut out).await.unwrap();
            (ok, String::from_utf8(out).unwrap())
        }
    };

    let (ok, table) = run(&scratch).await;

    assert!(ok, "{table}");
    for line in [
        "app solos-bot (123456) acts as solos-bot[bot]; people write @solos-bot",
        "commits are by solos-bot[bot] <4242+solos-bot[bot]@users.noreply.github.com>",
        "acme/widgets  installation 7788  write: contents issues pull_requests",
        "read: actions checks metadata statuses   workflows: absent (good)  administration: absent (good)",
        "mint read ok  mint write ok (unused here)  base main exists",
        "github:alice pinned to account 1005",
        "ok",
    ] {
        assert!(table.contains(line), "{line}\n{table}");
    }
    assert!(key.exists(), "check deletes nothing");
    let pins: Value = serde_json::from_slice(&std::fs::read(scratch.pins()).unwrap()).unwrap();
    assert_eq!(pins["pins"]["alice"], 1005);

    // An installation that holds workflows is refused.
    let mut holds = Access::app_permissions();
    holds["workflows"] = json!("write");
    fake.hub().granted = holds;
    let (ok, table) = run(&scratch).await;
    assert!(!ok, "{table}");
    assert!(table.contains("PRESENT (refused)"), "{table}");
    assert!(
        table.contains("FAIL the installation holds workflows; remove it"),
        "{table}"
    );

    // One that lacks a permission says which.
    let mut lacks = Access::app_permissions();
    lacks["contents"] = json!("read");
    fake.hub().granted = lacks;
    let (ok, table) = run(&scratch).await;
    assert!(!ok);
    assert!(
        table.contains("FAIL the installation lacks contents: write"),
        "{table}"
    );

    // A suspended one exits nonzero with the sentence.
    fake.hub().granted = Access::app_permissions();
    fake.hub().suspended = true;
    let (ok, table) = run(&scratch).await;
    assert!(!ok);
    assert!(
        table.contains("FAIL the App is suspended on it; unsuspend it"),
        "{table}"
    );
    fake.hub().suspended = false;

    // One it is not installed on says where to install it.
    fake.hub().installed.remove(REPO);
    let (ok, table) = run(&scratch).await;
    assert!(!ok);
    assert!(
        table.contains("FAIL the App is not installed on it; install it at https://github.com/apps/solos-bot/installations/new"),
        "{table}"
    );
    // A login whose pin names another account is not trusted.
    fake.hub().installed.insert(REPO.to_owned(), 7_788);
    fake.hub().accounts.insert("alice".into(), 99);
    let (ok, table) = run(&scratch).await;
    assert!(!ok);
    assert!(
        table
            .contains("github:alice  FAIL pinned to account 1005, but the login is now account 99"),
        "{table}"
    );

    // A key GitHub refuses ends the check at once.
    solos_swarm::sealed::private_file(&key, WRONG_KEY.as_bytes(), true).unwrap();
    let (ok, table) = run(&scratch).await;
    assert!(!ok);
    assert!(
        table.contains("FAIL GitHub refused the App's key"),
        "{table}"
    );
}

#[tokio::test]
async fn preflight_reports_each_repository_and_never_fails_on_a_suspension() {
    let fake = Fake::start().await;
    let minter = fake.minter();
    fake.hub().installed.remove(DOCS);
    fake.hub().suspended = true;

    let found = minter.preflight(&[repo(REPO), repo(DOCS)]).await.unwrap();

    assert!(matches!(found[0].state, InstallState::Suspended));
    assert!(matches!(
        &found[1].state,
        InstallState::NotInstalled { install_url } if install_url.ends_with("/apps/solos-bot/installations/new")
    ));
}

// ---- agent serve ----

/// The model of a Pi worker: it reports ready with one text.
struct Reports;

impl Provider for Reports {
    fn dispatch(&self, _body: Value) -> Dispatch {
        Box::pin(async move {
            Ok(json!({"id":"model-1","created":1,"model":"deepseek-flash",
                "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[
                    {"id":"call-1","type":"function","function":{"name":"worker_report",
                        "arguments":json!({"status":"ready","text":"It was a bad deploy."}).to_string()}}]},
                    "finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":0,"completion_tokens":50}}))
        })
    }
}

#[tokio::test]
async fn agent_serve_answers_on_github_as_the_app_and_a_schedule_reports_on_its_issue() {
    let bwrap = cfg!(target_os = "linux")
        && std::path::Path::new("/usr/bin/bwrap").is_file()
        && std::path::Path::new("/usr/bin/prlimit").is_file();
    if !bwrap {
        eprintln!("skipped: the forge's workers need Linux, /usr/bin/bwrap and /usr/bin/prlimit");
        return;
    }
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
    // The node's install root is bound into sandboxes, so the key sits beside it, not under it.
    let pi_root = scratch.root.join("pi");
    std::fs::create_dir_all(pi_root.join("bin")).unwrap();
    std::fs::write(pi_root.join("bin/node"), "").unwrap();
    let key = scratch.root.join("github-app.pem");
    solos_swarm::sealed::private_file(&key, KEY.as_bytes(), true).unwrap();
    let config =
        AgentConfig::load(&app_config(&fake, &scratch.root, "github-app.pem", "")).unwrap();
    let host = PiHost {
        harness: ScriptedHarness::new(BTreeMap::new()).every_turn(vec![Call::Replied]),
        provider: Arc::new(Reports),
        pi_root: pi_root.clone(),
        node: pi_root.join("bin/node"),
        browser: solos_swarm::browse::Launcher::Lightpanda,
    };
    let state = scratch.root.join("state");

    let summary = tokio::time::timeout(LIMIT, serve_with(config, &state, true, Some(host)))
        .await
        .expect("agent serve did not go idle")
        .unwrap();

    assert!(!key.exists(), "the key was read once and deleted");
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
    assert!(asked[1].contains("It was a bad deploy."), "{}", asked[1]);
    let swept = on(1);
    assert_eq!(swept.len(), 2, "{swept:?}");
    assert!(
        swept[0].starts_with("On it: Triage the new issues."),
        "{}",
        swept[0]
    );
    for body in asked.iter().chain(&swept) {
        message_id(body);
    }
    let posted_as_the_app = hub
        .requests
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/comments"))
        .all(|r| matches!(&r.who, Who::Install { repos, write: true, .. } if repos == &[REPO.to_owned()]));
    assert!(
        posted_as_the_app,
        "every comment used a write token for the one repository"
    );
    let minted: Vec<String> = hub.tokens.keys().cloned().collect();
    drop(hub);
    assert!(
        scratch.cursor().exists() && scratch.pins().exists(),
        "the read position and the account pins are kept in the state dir"
    );
    let mut written = Vec::new();
    files(&scratch.root, &mut written);
    for path in written {
        let bytes = std::fs::read(&path).unwrap();
        for token in &minted {
            assert!(
                !bytes
                    .windows(token.len())
                    .any(|window| window == token.as_bytes()),
                "{} holds a token",
                path.display()
            );
        }
    }
    assert_settled(&scratch.store());
}
