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
    OutboundMessage, ThreadKey, TicketId, TicketOutcome, Trigger, TriggerId,
};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::config::{AgentConfig, ForgeConfig, GithubConfig, RepoConfig};
use solos_swarm::agent::github::{
    self, Access, AppId, AppKey, CURSOR_FILE, GithubApi, GithubApp, GithubChannel, GithubSource,
    IDENTITIES_FILE, Identities, InstallState, Minter, MinterConfig, OVERLAP, REFRESH_MARGIN,
    SUSPENDED_WAIT, marker,
};
use solos_swarm::agent::jsonl::JsonlSource;
use solos_swarm::agent::links::TicketLinks;
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
    /// Every installation is suspended.
    suspended: bool,
    /// These installations are suspended, as an owner suspends the App on one
    /// account.
    suspended_installs: BTreeSet<u64>,
    /// `GET /repos/{repo}` answers 404, though the App is installed there.
    unreadable: BTreeSet<String>,
    /// The issue comments of `DOCS`; the other listings of it are empty.
    docs: Vec<Value>,
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
    // ---- pull requests ----
    /// The directory of bare repositories the agent pushes to, so a pull request
    /// shows the head its branch holds, as GitHub does.
    remote: Option<PathBuf>,
    pull_posts: Vec<PullPost>,
    /// How the next `POST pulls` goes wrong.
    pull_faults: VecDeque<PullFault>,
    next_pull: u64,
}

/// A `POST pulls` as the fake received it.
#[derive(Clone, Debug)]
struct PullPost {
    title: String,
    head: String,
    base: String,
    body: String,
    draft: Value,
}

/// How the fake answers a pull request the agent opens.
#[derive(Clone, Copy, Debug)]
enum PullFault {
    /// Make the pull request, then answer 500: the agent cannot tell it was made.
    Create500,
    /// Refuse, making nothing.
    Refuse(u16, &'static str),
    /// Answer, correctly, a moment late.
    Delay,
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

/// Whether the installation that holds `repo` is suspended.
fn suspended(hub: &Hub, repo: &str) -> bool {
    hub.suspended
        || hub
            .installed
            .get(repo)
            .is_some_and(|id| hub.suspended_installs.contains(id))
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
            let down = {
                let hub = shared.hub.lock().unwrap();
                repos.iter().any(|repo| suspended(&hub, repo))
            };
            if down {
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
    let name = format!("{owner}/{repo}");
    match hub.installed.get(&name) {
        None => message(StatusCode::NOT_FOUND, "Not Found"),
        Some(id) => Json(json!({
            "id": id,
            "suspended_at": suspended(&hub, &name).then(|| ts(now_secs() * 1_000)),
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
    if hub.suspended || hub.suspended_installs.contains(&installation) {
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

async fn repository(
    State(shared): State<Shared>,
    Path((owner, repo)): Path<(String, String)>,
) -> Response {
    let name = format!("{owner}/{repo}");
    if (!ours(&owner, &repo) && name != DOCS)
        || shared.hub.lock().unwrap().unreadable.contains(&name)
    {
        return message(StatusCode::NOT_FOUND, "Not Found");
    }
    Json(json!({ "full_name": format!("{owner}/{repo}") })).into_response()
}

/// The repository a listing is of: `REPO`, or `DOCS`, which lists nothing but
/// the issue comments in `Hub::docs`.
fn listed(owner: &str, repo: &str) -> String {
    let name = format!("{owner}/{repo}");
    assert!(name == REPO || name == DOCS, "{name}");
    name
}

async fn issues((State(shared), Path((owner, repo)), Query(query), headers): Listing) -> Response {
    let name = listed(&owner, &repo);
    assert_eq!(query["state"], "all");
    let items = if name == REPO {
        shared.hub.lock().unwrap().issues.clone()
    } else {
        Vec::new()
    };
    let path = format!("repos/{name}/issues");
    since(&shared, &path, &items, &query, &headers)
}

async fn comments(
    (State(shared), Path((owner, repo)), Query(query), headers): Listing,
) -> Response {
    let name = listed(&owner, &repo);
    let items = {
        let hub = shared.hub.lock().unwrap();
        if name == REPO {
            hub.comments.clone()
        } else {
            hub.docs.clone()
        }
    };
    let path = format!("repos/{name}/issues/comments");
    since(&shared, &path, &items, &query, &headers)
}

async fn review_comments(
    (State(shared), Path((owner, repo)), Query(query), headers): Listing,
) -> Response {
    let name = listed(&owner, &repo);
    let items = if name == REPO {
        shared.hub.lock().unwrap().review_comments.clone()
    } else {
        Vec::new()
    };
    let path = format!("repos/{name}/pulls/comments");
    since(&shared, &path, &items, &query, &headers)
}

/// The latest completed runs, newest first.
async fn runs((State(shared), Path((owner, repo)), Query(query), headers): Listing) -> Response {
    let name = listed(&owner, &repo);
    assert_eq!(query["status"], "completed");
    let mut items = if name == REPO {
        shared.hub.lock().unwrap().runs.clone()
    } else {
        Vec::new()
    };
    items.sort_by_key(|run| std::cmp::Reverse(run["id"].as_u64().unwrap()));
    let path = format!("repos/{name}/actions/runs");
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

/// The head a branch holds in the bare repository the agent pushed to.
fn branch_head(hub: &Hub, branch: &str) -> Option<String> {
    let remote = hub.remote.as_ref()?.join("acme/widgets.git");
    let output = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(remote)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
}

/// A pull request as the fake shows it: its head is what its branch holds now.
fn shown(hub: &Hub, pull: &Value) -> Value {
    let mut pull = pull.clone();
    if let Some(branch) = pull["head"]["ref"].as_str().map(str::to_owned)
        && let Some(sha) = branch_head(hub, &branch)
        && pull["state"] == "open"
    {
        pull["head"]["sha"] = json!(sha);
    }
    pull
}

async fn pulls((State(shared), Path((owner, repo)), Query(query), headers): Listing) -> Response {
    assert!(ours(&owner, &repo));
    let items: Vec<Value> = {
        let hub = shared.hub.lock().unwrap();
        match query.get("head") {
            // `head=<owner>:<branch>&state=all`, as the agent looks for its own.
            Some(label) => {
                let wanted = query.get("state").map_or("open", String::as_str);
                hub.pulls
                    .iter()
                    .filter(|pull| pull["head"]["label"] == label.as_str())
                    .filter(|pull| wanted == "all" || pull["state"] == wanted)
                    .map(|pull| shown(&hub, pull))
                    .collect()
            }
            None => {
                assert_eq!(query["state"], "open");
                hub.pulls.clone()
            }
        }
    };
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

/// `POST pulls`: a pull request on a branch the bare repository holds, as the
/// bot. Faults are scripted: made and then a 500, or refused.
async fn post_pull(
    State(shared): State<Shared>,
    Path((owner, repo)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    if !ours(&owner, &repo) {
        return message(StatusCode::NOT_FOUND, "Not Found");
    }
    let fault = shared.hub.lock().unwrap().pull_faults.pop_front();
    if matches!(fault, Some(PullFault::Delay)) {
        tokio::time::sleep(Duration::from_millis(1_500)).await;
    }
    let mut hub = shared.hub.lock().unwrap();
    let text = |name: &str| body[name].as_str().unwrap_or_default().to_owned();
    hub.pull_posts.push(PullPost {
        title: text("title"),
        head: text("head"),
        base: text("base"),
        body: text("body"),
        draft: body["draft"].clone(),
    });
    if let Some(PullFault::Refuse(status, why)) = fault {
        return message(StatusCode::from_u16(status).unwrap(), why);
    }
    let branch = text("head");
    let label = format!("{owner}:{branch}");
    if hub
        .pulls
        .iter()
        .any(|pull| pull["head"]["label"] == label.as_str() && pull["state"] == "open")
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "message": "Validation Failed",
                "errors": [{"message": format!("A pull request already exists for {label}.")}],
            })),
        )
            .into_response();
    }
    let Some(sha) = branch_head(&hub, &branch) else {
        return message(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Validation Failed: head is invalid",
        );
    };
    hub.next_pull += 1;
    let number = 70 + hub.next_pull;
    let at = ts(now_secs() * 1_000);
    let pull = json!({
        "id": 9_000 + number,
        "number": number,
        "state": "open",
        "draft": body["draft"],
        "merged_at": null,
        "title": text("title"),
        "body": text("body"),
        "user": user_of(ME),
        "html_url": format!("https://github.com/{REPO}/pull/{number}"),
        "created_at": at,
        "updated_at": at,
        "head": {"ref": branch, "label": label, "sha": sha, "repo": {"full_name": REPO}},
        "base": {"ref": text("base")},
    });
    hub.pulls.push(pull.clone());
    if matches!(fault, Some(PullFault::Create500)) {
        return message(StatusCode::INTERNAL_SERVER_ERROR, "Server Error");
    }
    (StatusCode::CREATED, Json(pull)).into_response()
}

async fn get_pull(
    State(shared): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
) -> Response {
    if !ours(&owner, &repo) {
        return message(StatusCode::NOT_FOUND, "Not Found");
    }
    let hub = shared.hub.lock().unwrap();
    match hub.pulls.iter().find(|pull| pull["number"] == number) {
        Some(pull) => Json(shown(&hub, pull)).into_response(),
        None => message(StatusCode::NOT_FOUND, "Not Found"),
    }
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
            .route(
                "/api/repos/{owner}/{repo}/pulls",
                get(pulls).post(post_pull),
            )
            .route("/api/repos/{owner}/{repo}/pulls/{number}", get(get_pull))
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
        self.source_of(api, &[repo(REPO)], scratch, follow)
    }

    /// A Source of `repos`, which the Minter behind `api` must allow.
    fn source_of(
        &self,
        api: Arc<GithubApi>,
        repos: &[RepoName],
        scratch: &Scratch,
        follow: bool,
    ) -> Box<dyn Source> {
        let source = GithubSource::new(api, repos.iter().cloned(), scratch.cursor(), follow)
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
async fn a_result_links_the_tickets_page_before_its_marker_and_a_repeat_posts_nothing_more() {
    let fake = Fake::start().await;
    let page = "https://agent.example.com/tickets/tk_1";
    let links = TicketLinks::new("https://agent.example.com").unwrap();
    let channel: Arc<dyn Channel> = Arc::new(GithubChannel::new(fake.api()).with_links(links));
    let table = "## Done\n\n| a | b |\n|---|---|\n| 1 | 2 |";
    let long = format!("Report\n\n{}", "word ".repeat(5_000));
    let sends = [
        outbound("ms_ack", MessageKind::Acknowledgement, 7, "On it."),
        outbound("ms_result", MessageKind::Result, 7, table),
        outbound("ms_long", MessageKind::Result, 8, &long),
        outbound("ms_ask", MessageKind::Attention, 7, "Which branch?"),
    ];
    for message in &sends {
        let sent = channel
            .send(message.clone(), SendOptions::default())
            .await
            .unwrap();
        assert_eq!(sent, SendOutcome::Sent);
    }
    for message in &sends {
        let sent = channel.send(message.clone(), repeat()).await.unwrap();
        assert_eq!(sent, SendOutcome::Sent);
    }

    let hub = fake.hub();
    let [ack, result, long, ask] = &hub.posts[..] else {
        panic!("{} posts: a repeat found each marker", hub.posts.len())
    };
    assert_eq!(
        ack.body,
        format!("On it.\n\n{}", marker(&MessageId::new("ms_ack"))),
        "an acknowledgement has no link"
    );
    assert_eq!(
        result.body,
        format!(
            "{table}\n\n[Task details]({page})\n\n{}",
            marker(&MessageId::new("ms_result"))
        ),
        "the full Markdown, the link, then the marker last"
    );
    assert_eq!(message_id(&result.body), "ms_result");
    assert!(
        long.body.contains(&format!(
            " …\n\n…the rest is on the [task page]({page})\n\n<!-- solos-swarm message "
        )),
        "{}",
        &long.body[long.body.len() - 200..]
    );
    assert!(!long.body.contains("Cut short") && !long.body.contains("Task details"));
    assert_eq!(message_id(&long.body), "ms_long");
    assert_eq!(
        ask.body,
        format!(
            "Which branch?\n\n[Task details]({page})\n\n{}",
            marker(&MessageId::new("ms_ask"))
        )
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
    assert!(hub.posts[0].body.starts_with("Looking into it."));
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

// ---- One repository the App cannot act on ----

/// The ids of the Triggers stored so far, sorted.
fn stored(scratch: &Scratch) -> Vec<String> {
    let mut ids: Vec<String> = scratch
        .store()
        .triggers()
        .unwrap()
        .iter()
        .map(|trigger| trigger.id.to_string())
        .collect();
    ids.sort();
    ids
}

/// How many GETs the fake has had for `path`.
fn gets(hub: &Hub, path: &str) -> usize {
    hub.requests
        .iter()
        .filter(|r| r.method == "GET" && r.path == path)
        .count()
}

/// A comment from alice on issue `number`, written a minute before the fake
/// started.
fn mention(fake: &Fake, id: u64, number: u64) -> Value {
    let at = fake.t0 - 60_000;
    comment(id, number, "alice", "@solos-bot look", at, at, false)
}

/// An agent that reads `DOCS` and `REPO` through `minter`, until stopped.
fn two_repositories(fake: &Fake, scratch: &Scratch, minter: Arc<Minter>) -> Runtime {
    let api = fake.api_on(minter, scratch.pins());
    Agent {
        scratch,
        source: fake.source_of(api, &[repo(DOCS), repo(REPO)], scratch, true),
        channel: None,
        executor: Arc::new(ScriptedExecutor),
        until_idle: false,
    }
    .runtime()
}

fn widgets_listed(hub: &Hub) -> usize {
    gets(hub, "repos/acme/widgets/issues")
}

#[tokio::test]
async fn a_repository_without_the_app_is_left_out_and_the_others_are_read_at_the_usual_rate() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("one-missing");
    fake.hub().installed.remove(DOCS);
    fake.hub().comments = vec![mention(&fake, 101, 7)];
    fake.hub().docs = vec![mention(&fake, 201, 3)];
    // The wait is the default five minutes: a second look inside this test is
    // a repository that was not left out.
    let runtime = two_repositories(&fake, &scratch, fake.minter());
    let driver = async {
        until_triggers(&scratch, 1).await;
        let looked = gets(&fake.hub(), "repos/acme/docs/installation");
        fake.until("the installed repository to be read again", |hub| {
            (widgets_listed(hub) >= 6).then_some(())
        })
        .await;
        let hub = fake.hub();
        assert_eq!(
            gets(&hub, "repos/acme/docs/installation"),
            looked,
            "the missing installation is not asked about again before its wait is over"
        );
        assert!(
            hub.requests.iter().all(
                |r| !r.path.starts_with("repos/acme/docs") || r.path.ends_with("/installation")
            ),
            "nothing of the repository was listed"
        );
    };

    let (summary, ()) = drive(runtime, driver).await;

    assert_eq!(
        summary.triggers_received, 1,
        "the installed repository's comment arrived at once"
    );
    assert_eq!(stored(&scratch), ["github:acme/widgets:comment:101"]);
}

#[tokio::test]
async fn a_repository_the_app_is_installed_on_later_is_read_without_a_restart() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("installed-later");
    fake.hub().installed.remove(DOCS);
    fake.hub().comments = vec![mention(&fake, 101, 7)];
    fake.hub().docs = vec![mention(&fake, 201, 3)];
    let minter = fake.minter_with(KEY, |config| {
        config.suspended_wait = Duration::from_millis(100)
    });
    let runtime = two_repositories(&fake, &scratch, minter);
    let driver = async {
        until_triggers(&scratch, 1).await;
        assert_eq!(stored(&scratch), ["github:acme/widgets:comment:101"]);
        fake.hub().installed.insert(DOCS.to_owned(), 7_788);
        until_triggers(&scratch, 2).await;
    };

    let (summary, ()) = drive(runtime, driver).await;

    assert_eq!(summary.triggers_received, 2);
    assert_eq!(
        stored(&scratch),
        [
            "github:acme/docs:comment:201",
            "github:acme/widgets:comment:101"
        ],
        "the repository's comment arrived once the App was installed on it"
    );
}

#[tokio::test]
async fn a_repository_suspended_while_the_agent_runs_is_left_out_and_read_again_when_it_is_lifted()
{
    let fake = Fake::start().await;
    let scratch = Scratch::new("one-suspended");
    // Its own installation, as another account's is, so that it alone is suspended.
    fake.hub().installed.insert(DOCS.to_owned(), 7_790);
    fake.hub().comments = vec![mention(&fake, 101, 7)];
    fake.hub().docs = vec![mention(&fake, 201, 3)];
    let minter = fake.minter_with(KEY, |config| {
        config.suspended_wait = Duration::from_millis(100)
    });
    let runtime = two_repositories(&fake, &scratch, minter);
    let driver = async {
        until_triggers(&scratch, 2).await;
        fake.hub().suspended_installs.insert(7_790);
        let now = now_secs() * 1_000;
        {
            let mut hub = fake.hub();
            hub.comments.push(comment(
                102,
                7,
                "alice",
                "@solos-bot again",
                now,
                now,
                false,
            ));
            hub.docs.push(comment(
                202,
                3,
                "alice",
                "@solos-bot again",
                now,
                now,
                false,
            ));
        }
        until_triggers(&scratch, 3).await;
        assert_eq!(
            stored(&scratch),
            [
                "github:acme/docs:comment:201",
                "github:acme/widgets:comment:101",
                "github:acme/widgets:comment:102",
            ],
            "the other repository went on while this one was suspended"
        );
        assert!(
            fake.hub()
                .requests
                .iter()
                .any(|r| r.path.starts_with("repos/acme/docs/") && r.status == 403),
            "the suspension was met"
        );
        fake.hub().suspended_installs.clear();
        until_triggers(&scratch, 4).await;
    };

    let (summary, ()) = drive(runtime, driver).await;

    assert_eq!(summary.triggers_received, 4);
    assert_eq!(
        stored(&scratch),
        [
            "github:acme/docs:comment:201",
            "github:acme/docs:comment:202",
            "github:acme/widgets:comment:101",
            "github:acme/widgets:comment:102",
        ],
        "what was written during the suspension arrived once it was lifted"
    );
}

#[tokio::test]
async fn a_repository_the_app_cannot_read_is_left_out_and_the_others_are_read() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("one-unreadable");
    // Installed, but GitHub refuses the repository itself: it is not a wait.
    fake.hub().unreadable.insert(DOCS.to_owned());
    fake.hub().comments = vec![mention(&fake, 101, 7)];
    fake.hub().docs = vec![mention(&fake, 201, 3)];
    let runtime = two_repositories(&fake, &scratch, fake.minter());
    let driver = async {
        until_triggers(&scratch, 1).await;
        fake.until("the readable repository to be read again", |hub| {
            (widgets_listed(hub) >= 6).then_some(())
        })
        .await;
        let probes: Vec<u16> = fake
            .reads("repos/acme/docs")
            .iter()
            .map(|(_, status, _)| *status)
            .collect();
        assert_eq!(probes, [404], "asked once, refused, and left out");
        assert_eq!(gets(&fake.hub(), "repos/acme/docs/issues"), 0);
    };

    let (summary, ()) = drive(runtime, driver).await;

    assert_eq!(summary.triggers_received, 1);
    assert_eq!(stored(&scratch), ["github:acme/widgets:comment:101"]);
}

#[tokio::test]
async fn a_source_whose_every_repository_the_app_cannot_read_stops_and_the_agent_goes_on() {
    let fake = Fake::start().await;
    let scratch = Scratch::new("none-readable");
    fake.hub().unreadable.insert(DOCS.to_owned());
    let api = fake.api_on(fake.minter(), scratch.pins());
    let github = fake.source_of(api, &[repo(DOCS)], &scratch, true);
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
        .expect("a repository GitHub refuses stops the github source, not the agent");

    assert_eq!(
        summary.triggers_received, 1,
        "the other source kept working"
    );
    assert!(fake.reads("repos/acme/docs/issues").is_empty());
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
            ..RepoConfig::default()
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
    assert!(asked[0].starts_with("Looking into it."), "{}", asked[0]);
    assert!(asked[1].contains("It was a bad deploy."), "{}", asked[1]);
    let swept = on(1);
    assert_eq!(swept.len(), 2, "{swept:?}");
    assert!(swept[0].starts_with("Looking into it."), "{}", swept[0]);
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

// ---- Delivery: `ready` delivers a pull request ----

fn sandbox() -> bool {
    let found = cfg!(target_os = "linux")
        && std::path::Path::new("/usr/bin/bwrap").is_file()
        && std::path::Path::new("/usr/bin/prlimit").is_file();
    if !found {
        eprintln!("skipped: a worker needs Linux, /usr/bin/bwrap and /usr/bin/prlimit");
    }
    found
}

/// What the clone's git does as the worker: its own identity, none of the
/// test's.
fn as_worker(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn call_of(name: &str, arguments: &Value) -> Value {
    json!({"id":format!("call-{name}"),"type":"function",
           "function":{"name":name,"arguments":arguments.to_string()}})
}

/// The model of the Router and the workers: the Router accepts a request on
/// acme/widgets, and a worker's turn asks for nothing (its playbook has done
/// what it does).
struct Model {
    deliverable: Option<&'static str>,
    title: &'static str,
}

impl Provider for Model {
    fn dispatch(&self, body: Value) -> Dispatch {
        let prompt = body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .and_then(|last| last["content"].as_str())
            .unwrap_or_default()
            .to_owned();
        let calls: Vec<Value> = if prompt.contains(" new candidate") {
            prompt
                .lines()
                .filter_map(|line| line.strip_prefix("eval "))
                .map(|card| {
                    let eval = card.split(" | ").next().unwrap();
                    let mut args = json!({"eval":eval,"decision":"accept","reason":"alice asked",
                        "title":self.title,"repo":REPO,
                        "scope":"Make the change alice asked for on acme/widgets."});
                    if let Some(deliverable) = self.deliverable {
                        args["deliverable"] = json!(deliverable);
                    }
                    call_of("router_decide", &args)
                })
                .collect()
        } else {
            Vec::new()
        };
        Box::pin(async move {
            Ok(json!({"id":"model-1","created":1,"model":"deepseek-flash",
                      "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":calls},
                                  "finish_reason":"tool_calls"}],
                      "usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":0,"completion_tokens":50}}))
        })
    }
}

/// One tool call a worker makes, and what the broker answered.
type Step = (&'static str, Value);
type Answer = (String, u16, Value);

fn run_code(code: &str) -> Step {
    ("run_code", json!({"code": code}))
}

fn ready(text: &str) -> Step {
    ("worker_report", json!({"status":"ready","text":text}))
}

/// Plays one fixed round of tool calls for each worker the agent starts, in
/// the order they start, against the real broker over its socket, and
/// records every answer. The Router runs the scripted harness.
struct Played {
    inner: ScriptedHarness,
    rounds: Arc<Mutex<VecDeque<Vec<Step>>>>,
    seen: Arc<Mutex<Vec<Vec<Answer>>>>,
    prompts: Arc<Mutex<Vec<String>>>,
    /// What Pi's own sandbox would be started with, per worker.
    argvs: Arc<Mutex<Vec<Vec<String>>>>,
}

struct PlayedSession {
    inner: solos_swarm::harness::scripted::ScriptedSession,
    spec: solos_swarm::harness::SessionSpec,
    round: Option<Vec<Step>>,
    seen: Arc<Mutex<Vec<Vec<Answer>>>>,
    prompts: Arc<Mutex<Vec<String>>>,
    argvs: Arc<Mutex<Vec<Vec<String>>>>,
}

impl solos_swarm::harness::AgentHarness for Played {
    type Session = PlayedSession;
    async fn start(
        &self,
        spec: &solos_swarm::harness::SessionSpec,
        events: tokio::sync::mpsc::Sender<solos_swarm::harness::Observation>,
    ) -> Result<PlayedSession> {
        let worker = matches!(spec.role, solos_swarm::harness::Role::TicketWorker);
        let round = worker.then(|| self.rounds.lock().unwrap().pop_front().unwrap_or_default());
        Ok(PlayedSession {
            inner: self.inner.start(spec, events).await?,
            spec: spec.clone(),
            round,
            seen: self.seen.clone(),
            prompts: self.prompts.clone(),
            argvs: self.argvs.clone(),
        })
    }
}

impl PlayedSession {
    async fn post(&self, path: &str, body: Value) -> Result<(u16, Value)> {
        let client = reqwest::Client::builder()
            .unix_socket(self.spec.broker_socket.clone())
            .build()?;
        let response = client
            .post(format!("http://broker/{path}"))
            .bearer_auth(&self.spec.token)
            .json(&body)
            .send()
            .await?;
        let status = response.status().as_u16();
        Ok((status, response.json().await.unwrap_or(Value::Null)))
    }
}

impl solos_swarm::harness::Session for PlayedSession {
    fn id(&self) -> &str {
        solos_swarm::harness::Session::id(&self.inner)
    }
    async fn prompt(&mut self, text: &str) -> Result<()> {
        if let Some(steps) = self.round.take() {
            let (_, session) = self.post("session", json!({})).await?;
            self.prompts
                .lock()
                .unwrap()
                .push(session["system_prompt"].as_str().unwrap_or("").to_owned());
            let argv = solos_swarm::harness::PiHarness::command(&self.spec)
                .as_std()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            self.argvs.lock().unwrap().push(argv);
            let mut answers = Vec::new();
            for (tool, arguments) in steps {
                let (status, reply) = self.post(&format!("tool/{tool}"), arguments).await?;
                answers.push((tool.to_owned(), status, reply));
            }
            self.seen.lock().unwrap().push(answers);
        }
        solos_swarm::harness::Session::prompt(&mut self.inner, text).await
    }
    async fn stop(&mut self) -> Result<bool> {
        solos_swarm::harness::Session::stop(&mut self.inner).await
    }
    async fn join(self) -> usize {
        solos_swarm::harness::Session::join(self.inner).await
    }
}

/// An agent on the fake GitHub and a bare repository on this disk: the real
/// forge, mirror and push run, offline.
struct Shipping {
    fake: Fake,
    scratch: Scratch,
    /// `main`'s tip when the clone was made.
    tip: String,
    seen: Arc<Mutex<Vec<Vec<Answer>>>>,
    prompts: Arc<Mutex<Vec<String>>>,
    argvs: Arc<Mutex<Vec<Vec<String>>>>,
}

impl Shipping {
    /// `None` where this host cannot run a worker's sandbox. `repo` is added
    /// to the repository's terms in `agent.yaml`.
    async fn new(name: &str, repo: &str) -> Option<Self> {
        if !sandbox() {
            return None;
        }
        let fake = Fake::start().await;
        let scratch = Scratch::new(name);
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
        fake.hub().remote = Some(scratch.root.join("remote"));
        let pi = scratch.root.join("pi/bin");
        std::fs::create_dir_all(&pi).unwrap();
        std::fs::write(pi.join("node"), "").unwrap();
        let yaml = format!(
            "policy:
  people: [github:alice]
github:
  app_id: {APP_ID}
  key_file: github-app.pem
  api_base: {}
  git_base: file://{}/remote/
sources:
  - kind: github
channels:
  - kind: github
router:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {{input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}}
  max_output_tokens: 2048
executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {{input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}}
  max_output_tokens: 2048
  run_code: {{}}
forge:
  kind: github
  repos:
    {REPO}: {{about: \"Widgets, in Rust.\", base: main, owes: pull_request{repo}}}
budgets: {{session_microusd: {BUDGET}}}
",
            fake.api,
            scratch.root.display()
        );
        std::fs::write(scratch.root.join("agent.yaml"), yaml).unwrap();
        Some(Self {
            fake,
            scratch,
            tip,
            seen: Arc::default(),
            prompts: Arc::default(),
            argvs: Arc::default(),
        })
    }

    /// A request alice makes on issue `number`.
    fn ask(&self, number: u64, id: u64, text: &str) {
        let at = self.fake.t0 + id * 1_000;
        self.fake
            .hub()
            .comments
            .push(comment(id, number, "alice", text, at, at, false));
    }

    /// Run the agent until it is idle, with `rounds` for the workers it
    /// starts and a Router that accepts with `deliverable`.
    async fn serve(&self, deliverable: Option<&'static str>, rounds: Vec<Vec<Step>>) -> Summary {
        let key = self.scratch.root.join("github-app.pem");
        solos_swarm::sealed::private_file(&key, KEY.as_bytes(), true).unwrap();
        let config = AgentConfig::load(&self.scratch.root.join("agent.yaml")).unwrap();
        let harness = Played {
            inner: ScriptedHarness::new(BTreeMap::new()).every_turn(vec![Call::Replied]),
            rounds: Arc::new(Mutex::new(rounds.into())),
            seen: self.seen.clone(),
            prompts: self.prompts.clone(),
            argvs: self.argvs.clone(),
        };
        let pi_root = self.scratch.root.join("pi");
        let host = PiHost {
            harness,
            provider: Arc::new(Model {
                deliverable,
                title: "Fix the flaky retry test",
            }),
            node: pi_root.join("bin/node"),
            pi_root,
            browser: solos_swarm::browse::Launcher::Lightpanda,
        };
        let state = self.scratch.root.join("state");
        tokio::time::timeout(
            Duration::from_secs(120),
            serve_with(config, &state, true, Some(host)),
        )
        .await
        .expect("agent serve did not go idle")
        .unwrap()
    }

    fn bare(&self) -> PathBuf {
        self.scratch.root.join("remote/acme/widgets.git")
    }

    fn ticket(&self) -> solos_agent_core::model::Ticket {
        let tickets = self.scratch.store().tickets().unwrap();
        let [ticket] = &tickets[..] else {
            panic!("one ticket: {tickets:?}")
        };
        ticket.clone()
    }

    fn work_of(&self, ticket: &TicketId) -> PathBuf {
        self.scratch
            .root
            .join("state/tickets")
            .join(ticket.as_str())
            .join("work")
    }

    fn events(&self) -> Vec<solos_agent_core::model::DomainEvent> {
        self.scratch
            .store()
            .events()
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect()
    }

    /// The comments the agent posted on issue or pull request `number`.
    fn said_on(&self, number: u64) -> Vec<String> {
        self.fake
            .hub()
            .posts
            .iter()
            .filter(|p| p.number == number)
            .map(|p| p.body.clone())
            .collect()
    }

    /// The tools' answers for worker round `n`.
    fn answers(&self, n: usize) -> Vec<Answer> {
        self.seen.lock().unwrap()[n].clone()
    }
}

/// A commit in the clone, as the worker, in one `run_code`.
fn commit(file: &str, text: &str, message: &str) -> String {
    format!("echo '{text}' > {file} && git add -A && git commit -q -m '{message}'")
}

/// No token the fake minted is in any file under `dir`.
fn assert_no_token_on_disk(fake: &Fake, dir: &std::path::Path) {
    let minted: Vec<String> = fake.hub().tokens.keys().cloned().collect();
    assert!(!minted.is_empty(), "the test minted nothing to look for");
    let mut written = Vec::new();
    files(dir, &mut written);
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
}

/// Every mint the fake saw that could write names one repository.
fn assert_writes_are_narrowed(fake: &Fake) {
    let mints = fake.mints();
    let writes: Vec<&Mint> = mints
        .iter()
        .filter(|mint| mint.permissions["contents"] == "write")
        .collect();
    assert!(!writes.is_empty(), "nothing was written: {mints:?}");
    for mint in mints {
        assert_eq!(
            mint.repositories.as_array().map(Vec::len),
            Some(1),
            "{mint:?}"
        );
        if mint.permissions["contents"] == "write" {
            assert_eq!(mint.repositories, json!(["widgets"]), "{mint:?}");
        }
        assert!(mint.permissions.get("workflows").is_none());
        assert!(mint.permissions.get("administration").is_none());
    }
}

#[tokio::test]
async fn an_issue_becomes_a_ready_pull_request_by_the_bot_on_a_branch_of_exactly_the_workers_commits()
 {
    let Some(d) = Shipping::new("issue-to-pr", "").await else {
        return;
    };
    d.ask(12, 101, "@solos-bot fix the flaky retry test");
    let said = "Root cause: jobs.rs arms the wait after it reads the clock. Fix: read once.";

    let summary = d
        .serve(
            None,
            vec![vec![
                run_code(&commit(
                    "fix.txt",
                    "fixed",
                    "jobs: read the clock after arming",
                )),
                run_code(&commit("test.txt", "covered", "jobs: test it")),
                ready(said),
            ]],
        )
        .await;

    assert_eq!(summary.effects_pending, 0);
    let ticket = d.ticket();
    let branch = format!("solos/{}/fix-the-flaky-retry-test", ticket.id);
    let work = d.work_of(&ticket.id);
    let mine = as_worker(
        &work,
        &["rev-list", "--reverse", &format!("{}..HEAD", d.tip)],
    );
    let on_the_remote = git_dir(
        &d.bare(),
        &["rev-list", "--reverse", &format!("{}..{branch}", d.tip)],
    );
    assert_eq!(mine.lines().count(), 2);
    assert_eq!(
        on_the_remote, mine,
        "the remote branch holds exactly the worker's commit ids"
    );
    let head = mine.lines().last().unwrap().to_owned();

    // The worker's `ready` was admitted, in words it can act on.
    let answers = d.answers(0);
    let (tool, status, reply) = answers.last().unwrap();
    assert_eq!((tool.as_str(), *status), ("worker_report", 200), "{reply}");
    assert_eq!(reply["proposal"]["head"], head[..12]);
    assert_eq!(reply["proposal"]["commits"], 2);
    assert_eq!(reply["proposal"]["round"], 1);
    assert!(
        reply["next"]
            .as_str()
            .unwrap()
            .contains("opening the pull request as the bot"),
        "{reply}"
    );
    let prompts = d.prompts.lock().unwrap().clone();
    assert!(
        prompts[0].contains("You cannot push or open pull requests, and you hold no token."),
        "{}",
        prompts[0]
    );

    // The pull request: ready, by the bot, on the host's branch, tied to the Ticket.
    let (pulls, posts) = {
        let hub = d.fake.hub();
        (hub.pulls.clone(), hub.pull_posts.clone())
    };
    let [pull] = &pulls[..] else {
        panic!("one pull request: {pulls:?}")
    };
    assert_eq!(posts.len(), 1, "one POST /pulls");
    assert_eq!(posts[0].draft, json!(false), "ready, never a draft");
    assert_eq!(
        (
            posts[0].head.as_str(),
            posts[0].base.as_str(),
            posts[0].title.as_str()
        ),
        (branch.as_str(), "main", "Fix the flaky retry test")
    );
    assert_eq!(pull["user"]["login"], ME);
    assert_eq!(pull["head"]["sha"], head);
    let body = pull["body"].as_str().unwrap();
    assert!(body.starts_with(said), "{body}");
    assert!(body.contains("\nCloses #12\n"), "{body}");
    assert!(
        body.contains(&format!(
            "Requested by alice via github · ticket {}",
            ticket.id
        )),
        "{body}"
    );
    assert!(
        body.ends_with(&format!("<!-- solos-swarm ticket {} -->", ticket.id)),
        "{body}"
    );

    // The Ticket ended on the pull request, and says so once, on the issue.
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    let change = ticket.change.clone().unwrap();
    let delivered = change.delivered.clone().unwrap();
    assert_eq!(
        (delivered.pr.number, delivered.head.as_str()),
        (71, head.as_str())
    );
    assert_eq!(change.head.as_str(), head);
    assert!(ticket.threads.contains(&github::thread(REPO, 71)));
    let on_issue = d.said_on(12);
    assert_eq!(on_issue.len(), 2, "{on_issue:?}");
    assert!(
        on_issue[0].starts_with("Looking into it."),
        "{}",
        on_issue[0]
    );
    assert!(
        on_issue[1].contains(&format!(
            "{said}\n\nPR https://github.com/acme/widgets/pull/71 · head {} · opened",
            &head[..12]
        )),
        "{}",
        on_issue[1]
    );
    let numbers: Vec<&str> = ticket
        .channels
        .iter()
        .map(|c| c.address["number"].as_str())
        .collect();
    assert_eq!(numbers, ["71"], "the pull request replaces the issue");

    // The chain: one admission, one delivery, the same head.
    let events = d.events();
    let admitted: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            solos_agent_core::model::DomainEvent::ProposalAdmitted { head, round, .. } => {
                Some((head.as_str().to_owned(), *round))
            }
            _ => None,
        })
        .collect();
    assert_eq!(admitted, [(head.clone(), 1)]);
    let delivered_events = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                solos_agent_core::model::DomainEvent::Delivered { created: true, .. }
            )
        })
        .count();
    assert_eq!(delivered_events, 1);

    assert_writes_are_narrowed(&d.fake);
    assert_no_token_on_disk(&d.fake, &d.scratch.root);
    assert_settled(&d.scratch.store());
}

fn git_dir(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn said_in(answer: &Answer) -> String {
    answer.2["error"].as_str().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn a_ready_the_host_refuses_commits_nothing_and_says_what_to_do_and_the_fixed_one_is_admitted()
 {
    let Some(d) = Shipping::new("refusals", "").await else {
        return;
    };
    d.ask(12, 101, "@solos-bot fix the flaky retry test");
    let base = d.tip.clone();
    let gui = "git -c user.name=Gui -c user.email=gui@example.com";
    let steps = vec![
        // 0: no commits
        ready("Nothing yet."),
        // 1-2: a commit by Gui
        run_code(&format!(
            "echo a > a.txt && git add -A && {gui} commit -q -m 'by gui'"
        )),
        ready("Done."),
        // 3-5: a commit whose committer alone differs
        run_code(&format!(
            "git reset -q --hard {base} && echo a > a.txt && git add -A && \
             GIT_COMMITTER_NAME=Gui GIT_COMMITTER_EMAIL=gui@example.com git commit -q -m 'by me, committed by gui'"
        )),
        ready("Done."),
        // 5-6: the remedy the sentence names mends it
        run_code(&format!(
            "git rebase -q --exec 'git commit --amend --no-edit --reset-author' {base}"
        )),
        run_code("git log -1 --format='%an <%ae>|%cn <%ce>'"),
        // 7-8: a protected path in an intermediate commit, deleted again after
        run_code(&format!(
            "git reset -q --hard {base} && mkdir -p .github/workflows && echo x > .github/workflows/ci.yml && \
             git add -A && git commit -q -m 'add ci' && git rm -q .github/workflows/ci.yml && \
             echo b > b.txt && git add -A && git commit -q -m 'drop ci'"
        )),
        ready("Done."),
        // 8: an orphan branch
        run_code(
            "git checkout -q --orphan solo && git rm -rfq . && echo o > o.txt && git add -A && \
             git commit -q -m orphan",
        ),
        ready("Done."),
        // 10-11: back to the clone's branch with one honest commit
        run_code(&format!(
            "git checkout -q -f main && git reset -q --hard {base} && {}",
            commit("fix.txt", "fixed", "jobs: read the clock after arming")
        )),
        ready("Fixed it."),
    ];

    let summary = d.serve(None, vec![steps]).await;

    assert_eq!(summary.effects_pending, 0);
    let answers = d.answers(0);
    let code_ok = |n: usize| {
        let (_, status, reply) = &answers[n];
        assert_eq!(
            (*status, reply["exit_code"].clone()),
            (200, json!(0)),
            "{n}: {reply}"
        );
    };
    for n in [1, 3, 5, 6, 7, 9, 11] {
        code_ok(n);
    }
    let bot = "solos-bot[bot] <4242+solos-bot[bot]@users.noreply.github.com>";
    assert_eq!(
        answers[6].2["stdout"],
        format!("{bot}|{bot}\n"),
        "the remedy the sentence names leaves every commit the bot's"
    );
    let refused = |n: usize| {
        let (tool, status, _) = &answers[n];
        assert_eq!(
            (tool.as_str(), *status),
            ("worker_report", 409),
            "{n}: {:?}",
            answers[n]
        );
        said_in(&answers[n])
    };
    let nothing = refused(0);
    assert!(
        nothing.starts_with("Not accepted:") && nothing.contains("Commit your change"),
        "{nothing}"
    );
    let author = refused(2);
    assert!(
        author.contains("is by Gui <gui@example.com> as its author")
            && author.contains("every commit must be by solos-bot[bot] <4242+solos-bot[bot]@users.noreply.github.com>")
            && author.contains(&format!("--reset-author' {base}")),
        "{author}"
    );
    let committer = refused(4);
    assert!(
        committer.contains("Gui <gui@example.com> as its committer"),
        "{committer}"
    );
    let protected = refused(8);
    assert!(
        protected.contains(".github/workflows/ci.yml")
            && protected.contains("which is protected (.github/**)")
            && protected.contains(&format!("git reset --soft {base}")),
        "{protected}"
    );
    let orphan = refused(10);
    assert!(
        orphan.contains("does not descend from") && orphan.contains("report blocked"),
        "{orphan}"
    );
    let (_, status, accepted) = &answers[12];
    assert_eq!(*status, 200, "{accepted}");
    assert_eq!(accepted["proposal"]["commits"], 1, "only the honest commit");

    // Nothing the host refused was committed or pushed.
    let ticket = d.ticket();
    let reported: Vec<_> = d
        .events()
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                solos_agent_core::model::DomainEvent::Reported {
                    status: solos_agent_core::model::ReportStatus::Ready,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(reported.len(), 1, "{reported:?}");
    let branch = format!("solos/{}/fix-the-flaky-retry-test", ticket.id);
    let work = d.work_of(&ticket.id);
    let honest = as_worker(&work, &["rev-parse", "main"]);
    assert_eq!(
        git_dir(&d.bare(), &["rev-parse", &format!("refs/heads/{branch}")]),
        honest
    );
    assert_eq!(
        git_dir(
            &d.bare(),
            &["rev-list", "--count", &format!("{base}..{branch}")]
        ),
        "1"
    );
    assert_eq!(d.fake.hub().pull_posts.len(), 1);
    let branches = git_dir(
        &d.bare(),
        &["for-each-ref", "--format=%(refname)", "refs/heads"],
    );
    assert_eq!(
        branches.lines().count(),
        2,
        "main and the Ticket's branch: {branches}"
    );
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
}

#[tokio::test]
async fn a_pull_request_of_a_repository_that_wants_evidence_is_not_opened_without_it() {
    let Some(d) = Shipping::new("evidence", ", evidence: {command: \"./verify --json\"}").await
    else {
        return;
    };
    d.ask(12, 101, "@solos-bot fix the flaky retry test");
    let good = r#"{"ok":true,"sha":"HEAD","dirty":false,"versions":{"@biomejs/biome":"2.5.13"}}"#;
    let report = |json: &str| format!("Fixed it.\n\n## Evidence\n\n```json\n{json}\n```");
    let steps = vec![
        run_code(&commit(
            "fix.txt",
            "fixed",
            "jobs: read the clock after arming",
        )),
        // 1: none at all
        ready("Fixed it."),
        // 2: the checks failed
        ready(&report(r#"{"ok":false,"dirty":false}"#)),
        // 3: cut in the middle, as a long report is
        ready(&report(r#"{"ok":true,"steps":[{"name":"lint","ok":tr"#)),
        // 4: the command's output
        ready(&report(good)),
    ];

    let summary = d.serve(None, vec![steps]).await;

    assert_eq!(summary.effects_pending, 0);
    let answers = d.answers(0);
    for n in [1, 2, 3] {
        assert_eq!(
            (answers[n].0.as_str(), answers[n].1),
            ("worker_report", 409),
            "{n}"
        );
    }
    let none = said_in(&answers[1]);
    assert!(
        none.starts_with("Not accepted:")
            && none.contains("`./verify --json`")
            && none.contains("`## Evidence`")
            && none.contains("report blocked"),
        "{none}"
    );
    let failed = said_in(&answers[2]);
    assert!(failed.contains("does not say `\"ok\": true`"), "{failed}");
    let cut = said_in(&answers[3]);
    assert!(
        cut.contains("does not parse") && cut.contains("8,000"),
        "{cut}"
    );
    assert_eq!(
        (answers[4].0.as_str(), answers[4].1),
        ("worker_report", 200),
        "{:?}",
        answers[4]
    );
    let prompts = d.prompts.lock().unwrap().clone();
    assert!(
        prompts[0].contains("must carry evidence")
            && prompts[0].contains("run `./verify --json` in /work"),
        "{}",
        prompts[0]
    );

    let pulls = d.fake.hub().pulls.clone();
    let [pull] = &pulls[..] else {
        panic!("one pull request, and none for the refused readies: {pulls:?}")
    };
    let body = pull["body"].as_str().unwrap();
    assert!(
        body.starts_with(&report(good)),
        "the evidence reaches the description as it was written, `@biomejs/biome` and all: {body}"
    );
    assert_eq!(
        d.ticket().ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
}

/// A package manager stand-in: `install` leaves bun's layout, anything else
/// says it ran, so the test can tell `bun` and `node` came from the mount.
const FAKE_BUN: &str = "#!/bin/sh
set -eu
case \"${1:-}\" in
install)
  mkdir -p node_modules/.bun/x@1/node_modules/x apps/web/node_modules
  echo 1 > node_modules/.bun/x@1/node_modules/x/index.js
  ln -s .bun/x@1/node_modules/x node_modules/x
  ln -s ../../../node_modules/.bun/x@1/node_modules/x apps/web/node_modules/x
  ;;
*) echo \"fake-bun $*\" ;;
esac
";

/// Make the repository an installable one, with a package manager that is
/// `script`, and its terms say so.
fn installable(d: &Shipping, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let bun = d.scratch.root.join("bun");
    std::fs::write(&bun, script).unwrap();
    std::fs::set_permissions(&bun, std::fs::Permissions::from_mode(0o755)).unwrap();
    let upstream = d.scratch.root.join("upstream");
    for (path, text) in [
        ("package.json", r#"{"name":"acme","workspaces":["apps/*"]}"#),
        ("bun.lock", "lock one"),
        ("apps/web/package.json", r#"{"name":"web"}"#),
    ] {
        let at = upstream.join(path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, text).unwrap();
    }
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "--quiet", "-m", "manifests"]);
    git(
        &upstream,
        &["push", "--quiet", d.bare().to_str().unwrap(), "main"],
    );
    let path = d.scratch.root.join("agent.yaml");
    let yaml = std::fs::read_to_string(&path).unwrap();
    let asked = format!(
        "owes: pull_request, deps: {{kind: bun, binary: {}, timeout_secs: 60}}}}",
        bun.display()
    );
    assert!(yaml.contains("owes: pull_request}"));
    std::fs::write(&path, yaml.replace("owes: pull_request}", &asked)).unwrap();
}

#[tokio::test]
async fn a_worker_runs_the_repositorys_scripts_offline_on_a_cache_mounted_read_only() {
    let Some(d) = Shipping::new("deps", "").await else {
        return;
    };
    installable(&d, FAKE_BUN);
    d.ask(12, 101, "@solos-bot fix the flaky retry test");
    let steps = vec![
        // 0: bun and node are the host's, first on PATH, and HOME is not the clone
        run_code("bun --version; node -v; echo HOME=$HOME; echo PATH=$PATH"),
        // 1: the packages are there, and a workspace's links lead into them
        run_code(
            "ls /work/node_modules/.bun/x@1/node_modules/x && ls -l /work/apps/web/node_modules/x",
        ),
        // 2: the cache cannot be written to
        run_code("touch /work/node_modules/probe 2>&1; echo rc=$?"),
        run_code(
            "{ echo evil > /work/node_modules/.bun/x@1/node_modules/x/index.js; } 2>&1; echo rc=$?",
        ),
        // 4: what a tool writes in a workspace's node_modules goes nowhere
        run_code("touch /work/apps/web/node_modules/.vite-temp && echo writable"),
        // 5: the clone is clean: nothing the cache brings is untracked
        run_code("git status --porcelain; echo status-done"),
        run_code(&commit(
            "fix.txt",
            "fixed",
            "jobs: read the clock after arming",
        )),
        run_code("git ls-files | tr '\\n' ' '; echo"),
        ready("Fixed it."),
    ];

    let summary = d.serve(None, vec![steps]).await;

    assert_eq!(summary.effects_pending, 0);
    let answers = d.answers(0);
    let out = |n: usize| {
        let (_, status, reply) = &answers[n];
        assert_eq!(*status, 200, "{n}: {reply}");
        reply["stdout"].as_str().unwrap_or_default().to_owned()
    };
    let tools = out(0);
    assert!(
        tools.matches("fake-bun --version").count() == 1
            && tools.contains("fake-bun -v")
            && tools.contains("HOME=/tmp/home")
            && tools.contains("PATH=/runtime/bun:/venv/bin:/usr/local/bin:/usr/bin:/bin"),
        "{tools}"
    );
    let layout = out(1);
    assert!(
        layout.contains("index.js")
            && layout.contains(
                "/work/apps/web/node_modules/x -> ../../../node_modules/.bun/x@1/node_modules/x"
            ),
        "{layout}"
    );
    for n in [2, 3] {
        let probe = out(n);
        assert!(
            probe.contains("Read-only file system") && probe.contains("rc=1"),
            "{n}: {probe}"
        );
    }
    assert_eq!(out(4), "writable\n");
    assert_eq!(
        out(5),
        "status-done\n",
        "the tree is clean for an evidence check"
    );
    assert_eq!(
        out(7).lines().next().unwrap(),
        "README.md apps/web/package.json bun.lock fix.txt package.json ",
        "what the worker committed has no node_modules and no cache"
    );

    // What the worker was told, and what Pi's own sandbox does not hold.
    let prompts = d.prompts.lock().unwrap().clone();
    assert!(
        prompts[0].contains("Dependencies: the repository's are installed, read-only"),
        "{}",
        prompts[0]
    );
    let argv = d.argvs.lock().unwrap()[0].join("\n");
    assert!(
        argv.contains("--tmpfs\n/work\n")
            && !argv.contains("/work/node_modules")
            && !argv.contains("deps/acme")
            && !argv.contains("/runtime/bun"),
        "Pi's /work is a tmpfs for its settings; the clone is only in run_code: {argv}"
    );

    // The cache is the host's, keyed, whole, and the clone holds none of it.
    let repo = d.scratch.root.join("state/deps/acme/widgets");
    let caches: Vec<PathBuf> = std::fs::read_dir(&repo)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let [cache] = &caches[..] else {
        panic!("one cache and no scratch left: {caches:?}")
    };
    assert!(cache.join("manifest.json").is_file());
    assert!(
        cache
            .join("node_modules/.bun/x@1/node_modules/x/index.js")
            .is_file()
    );
    assert_eq!(
        std::fs::read_to_string(cache.join("node_modules/.bun/x@1/node_modules/x/index.js"))
            .unwrap(),
        "1\n",
        "the worker's write did not reach it"
    );
    let ticket = d.ticket();
    let work = d.work_of(&ticket.id);
    for mount in ["node_modules", "apps/web/node_modules"] {
        let empty =
            std::fs::read_dir(work.join(mount)).map_or(true, |mut left| left.next().is_none());
        assert!(empty, "{mount} in the clone is only the mount point");
    }
    let branch = format!("solos/{}/fix-the-flaky-retry-test", ticket.id);
    let pushed = git_dir(&d.bare(), &["ls-tree", "-r", "--name-only", &branch]);
    assert!(!pushed.contains("node_modules"), "{pushed}");
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
}

#[tokio::test]
async fn a_build_that_failed_is_told_to_the_worker_which_goes_on_without_dependencies() {
    let Some(d) = Shipping::new("deps-failed", "").await else {
        return;
    };
    installable(
        &d,
        "#!/bin/sh\necho 'error: GET https://registry.npmjs.org/x - 503' >&2\nexit 1\n",
    );
    d.ask(12, 101, "@solos-bot fix the flaky retry test");
    let steps = vec![
        run_code("ls /work/node_modules 2>&1; command -v bun || echo no-bun"),
        run_code(&commit(
            "fix.txt",
            "fixed",
            "jobs: read the clock after arming",
        )),
        ready("Fixed it, and I could not run the checks: no dependencies."),
    ];

    let summary = d.serve(None, vec![steps]).await;

    assert_eq!(summary.effects_pending, 0);
    let prompts = d.prompts.lock().unwrap().clone();
    assert!(
        prompts[0].contains("Dependencies: the host could not install the repository's (")
            && prompts[0].contains(
                "the install failed (exit status: 1): error: GET https://registry.npmjs.org/x - 503"
            )
            && prompts[0].contains("do not claim checks you could not run"),
        "{}",
        prompts[0]
    );
    let answers = d.answers(0);
    let seen = answers[0].2["stdout"].as_str().unwrap();
    assert!(
        seen.contains("No such file or directory") && seen.contains("no-bun"),
        "the sandbox has nothing of it: {seen}"
    );
    let ticket = d.ticket();
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed),
        "a build that fails does not fail the Ticket"
    );
    assert_eq!(d.fake.hub().pull_posts.len(), 1);
    let repo = d.scratch.root.join("state/deps/acme/widgets");
    assert_eq!(
        std::fs::read_dir(&repo).unwrap().count(),
        0,
        "no cache, no scratch directory"
    );
}

#[tokio::test]
async fn a_change_over_the_repositorys_size_limit_is_refused_and_a_smaller_one_is_not() {
    let Some(d) = Shipping::new("size", ", max_change_bytes: 3000").await else {
        return;
    };
    d.ask(12, 101, "@solos-bot add the data file");
    let steps = vec![
        run_code("head -c 20000 /dev/urandom > big.bin && git add -A && git commit -q -m 'big'"),
        ready("Done."),
        run_code(&format!(
            "git reset -q --hard {} && {}",
            d.tip,
            commit("small.txt", "small", "small")
        )),
        ready("Done."),
    ];

    d.serve(None, vec![steps]).await;

    let answers = d.answers(0);
    assert_eq!(answers[1].1, 409, "{:?}", answers[1]);
    let said = said_in(&answers[1]);
    assert!(
        said.contains("over this repository's limit of 3000") && said.contains("report blocked"),
        "{said}"
    );
    assert_eq!(answers[3].1, 200, "{:?}", answers[3]);
}

#[tokio::test]
async fn a_second_ready_while_the_first_is_still_being_delivered_is_refused() {
    let Some(d) = Shipping::new("in-flight", "").await else {
        return;
    };
    d.ask(12, 101, "@solos-bot fix the flaky retry test");
    // GitHub takes a moment over the pull request.
    d.fake.hub().pull_faults.push_back(PullFault::Delay);
    let steps = vec![
        run_code(&commit(
            "fix.txt",
            "fixed",
            "jobs: read the clock after arming",
        )),
        ready("Fixed it."),
        ready("Fixed it, again."),
    ];

    d.serve(None, vec![steps]).await;

    let answers = d.answers(0);
    assert_eq!(answers[1].1, 200, "{:?}", answers[1]);
    assert_eq!(answers[2].1, 409, "{:?}", answers[2]);
    let said = said_in(&answers[2]);
    assert!(
        said.contains("the host is still publishing") && said.contains("Report waiting"),
        "{said}"
    );
    let ticket = d.ticket();
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed),
        "the first still delivered"
    );
    assert_eq!(d.fake.hub().pulls.len(), 1);
}

#[tokio::test]
async fn a_head_past_the_push_limit_is_refused_and_a_second_head_moves_the_branch_and_keeps_the_pull_request()
 {
    let Some(d) = Shipping::new("limit", ", max_pushes: 2").await else {
        return;
    };
    d.ask(12, 101, "@solos-bot fix the flaky retry test");
    let round_one = vec![run_code(&commit("fix.txt", "v1", "first")), ready("First.")];
    d.serve(None, vec![round_one]).await;
    let first = d.ticket();
    let branch = first.change.as_ref().unwrap().branch.as_str().to_owned();
    let first_head = git_dir(&d.bare(), &["rev-parse", &format!("refs/heads/{branch}")]);

    // A listed person comments on the pull request; the Ticket resumes, and
    // its worker pushes a second head.
    let at = d.fake.t0 + 500_000;
    d.fake.hub().comments.push(comment(
        202,
        71,
        "alice",
        "please also cover the retry limit",
        at,
        at,
        true,
    ));
    let round_two = vec![
        run_code(&commit("fix.txt", "v2", "second")),
        ready("Second."),
    ];
    d.serve(None, vec![round_two]).await;

    let second = d.ticket();
    let change = second.change.clone().unwrap();
    let second_head = git_dir(&d.bare(), &["rev-parse", &format!("refs/heads/{branch}")]);
    assert_ne!(first_head, second_head, "the branch moved");
    assert_eq!(change.head.as_str(), second_head);
    assert_eq!(change.pushes, 2);
    assert_eq!(
        git_dir(
            &d.bare(),
            &["rev-list", "--count", &format!("{}..{branch}", d.tip)]
        ),
        "2",
        "the first commit is still under the second"
    );
    {
        let hub = d.fake.hub();
        assert_eq!(hub.pulls.len(), 1, "the pull request is kept");
        assert_eq!(hub.pull_posts.len(), 1);
        assert_eq!(shown(&hub, &hub.pulls[0])["head"]["sha"], second_head);
    }
    let said = d.said_on(71);
    assert!(
        said.iter()
            .any(|text| text.contains(&format!("head {} · updated", &second_head[..12]))),
        "the pull request's conversation hears it: {said:?}"
    );

    // A third head is over the limit.
    let at = d.fake.t0 + 900_000;
    d.fake
        .hub()
        .comments
        .push(comment(303, 71, "alice", "one more thing", at, at, true));
    let round_three = vec![run_code(&commit("fix.txt", "v3", "third")), ready("Third.")];
    d.serve(None, vec![round_three]).await;

    let answers = d.answers(2);
    assert_eq!(answers[1].1, 409, "{:?}", answers[1]);
    assert!(
        said_in(&answers[1]).contains("limit of 2 published heads"),
        "{:?}",
        answers[1]
    );
    assert_eq!(
        git_dir(&d.bare(), &["rev-parse", &format!("refs/heads/{branch}")]),
        second_head,
        "nothing more was pushed"
    );
}

#[tokio::test]
async fn a_ticket_that_owes_only_an_answer_is_unchanged_and_the_forge_is_never_touched() {
    let Some(d) = Shipping::new("answer-only", "").await else {
        return;
    };
    d.ask(12, 101, "@solos-bot what does the readme say?");

    // The Router lowers what the repository owes (a pull request) to an answer.
    d.serve(
        Some("answer"),
        vec![vec![ready("The readme says Widgets.")]],
    )
    .await;

    let ticket = d.ticket();
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    assert_eq!(ticket.change, None);
    assert_eq!(ticket.mandate.forge, None, "an answer takes no forge terms");
    let answers = d.answers(0);
    assert_eq!(answers[0].1, 200, "{:?}", answers[0]);
    assert!(answers[0].2.get("proposal").is_none(), "{:?}", answers[0]);
    let said = d.said_on(12);
    assert_eq!(said.len(), 2, "{said:?}");
    assert!(
        said[1].starts_with("The readme says Widgets.") && !said[1].contains("PR "),
        "{}",
        said[1]
    );
    assert!(d.fake.hub().pull_posts.is_empty());
    let branches = git_dir(
        &d.bare(),
        &["for-each-ref", "--format=%(refname)", "refs/heads"],
    );
    assert_eq!(branches, "refs/heads/main");
    let events = d.events();
    assert!(
        !events.iter().any(|e| matches!(
            e,
            solos_agent_core::model::DomainEvent::ProposalAdmitted { .. }
                | solos_agent_core::model::DomainEvent::Delivered { .. }
        )),
        "{events:?}"
    );
}

// ---- The forge's two halves, one at a time ----

use solos_agent_core::change::{BranchName, CommitSha, Harvest, Withheld};
use solos_agent_core::delivery::Observed;
use solos_swarm::agent::ports::{Execute, Forge, Proposal, Publish, Refusal, Workspace};

/// The real forge on the fake GitHub and a bare repository that logs each
/// push it takes. The test plays the worker in the clone, with the clone's
/// own git, and bundles what it committed as the host's sandbox would.
struct Forged {
    fake: Fake,
    scratch: Scratch,
    forge: Arc<dyn Forge>,
    upstream: PathBuf,
    tip: String,
}

/// A ticket's clone.
struct Cloned {
    id: TicketId,
    work: PathBuf,
    workspace: Workspace,
}

impl Forged {
    async fn new(name: &str, adjust: impl FnOnce(&mut RepoConfig)) -> Self {
        Self::with(name, true, |_| {}, adjust).await
    }

    async fn with(
        name: &str,
        writes: bool,
        minter: impl FnOnce(&mut MinterConfig),
        adjust: impl FnOnce(&mut RepoConfig),
    ) -> Self {
        let fake = Fake::start().await;
        let scratch = Scratch::new(name);
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
        let bare = scratch.root.join("remote/acme/widgets.git");
        let log = bare.join("pushes.log");
        let hooks = bare.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        for (name, body) in [
            (
                "pre-receive",
                format!(
                    "#!/bin/sh\nif [ -e '{}' ]; then echo 'refusing to allow a GitHub App to \
                     create or update workflow without workflows permission' >&2; exit 1; fi\n",
                    bare.join("reject").display()
                ),
            ),
            (
                "post-receive",
                format!(
                    "#!/bin/sh\nwhile read old new ref; do echo \"$old $new $ref\" >> '{}'; done\n",
                    log.display()
                ),
            ),
        ] {
            let path = hooks.join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        let tip = git(&upstream, &["rev-parse", "main"]);
        fake.hub().remote = Some(scratch.root.join("remote"));
        let mut config = MinterConfig::new(
            AppId(APP_ID),
            &fake.api,
            writes,
            [repo(REPO), repo(DOCS)].into(),
        );
        minter(&mut config);
        let clock = fake.shared.clock.clone();
        let now: github::Now = Arc::new(move || clock.now());
        let minter = Arc::new(Minter::new(config, AppKey::parse(KEY).unwrap(), now).unwrap());
        let api = fake.api_on(minter.clone(), scratch.pins());
        let app = GithubApp {
            api,
            minter,
            git_base: format!("file://{}/remote/", scratch.root.display()),
        };
        let mut terms = RepoConfig {
            about: "Widgets.".into(),
            base: "main".into(),
            owes: Deliverable::PullRequest,
            ..RepoConfig::default()
        };
        adjust(&mut terms);
        let forge = forge::open(
            &ForgeConfig::Github {
                repos: BTreeMap::from([(repo(REPO), terms)]),
            },
            scratch.root.join("state/forge"),
            Some(&app),
        )
        .unwrap();
        Self {
            fake,
            scratch,
            forge,
            upstream,
            tip,
        }
    }

    fn bare(&self) -> PathBuf {
        self.scratch.root.join("remote/acme/widgets.git")
    }

    fn pushes(&self) -> Vec<String> {
        std::fs::read_to_string(self.bare().join("pushes.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The host clones for a ticket.
    async fn clone_for(&self, id: &str) -> Cloned {
        let dir = self.scratch.root.join("tickets").join(id);
        let (work, host) = (dir.join("work"), dir.join("host"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&host).unwrap();
        let workspace = self
            .forge
            .prepare(
                &repo(REPO),
                TicketDirs {
                    work: &work,
                    host: &host,
                },
            )
            .await
            .unwrap();
        Cloned {
            id: TicketId::new(id),
            work,
            workspace,
        }
    }

    /// One commit by the worker, as the clone's identity.
    fn commit(&self, clone: &Cloned, file: &str, text: &str) -> String {
        let path = clone.work.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        as_worker(&clone.work, &["add", "-A"]);
        as_worker(
            &clone.work,
            &["commit", "-q", "-m", &format!("{file}: {text}")],
        );
        as_worker(&clone.work, &["rev-parse", "HEAD"])
    }

    /// What the host's sandbox would make of the worker's tree, then what the
    /// forge says of it.
    async fn admit(&self, clone: &Cloned) -> std::result::Result<Harvest, Refusal> {
        let head = as_worker(&clone.work, &["rev-parse", "HEAD"]);
        let out = self.scratch.root.join("out").join(clone.id.as_str());
        std::fs::create_dir_all(&out).unwrap();
        let bundle = out.join("proposal.bundle");
        let range = format!("{}..HEAD", clone.workspace.base_commit);
        let _ = std::fs::remove_file(&bundle);
        as_worker(
            &clone.work,
            &["bundle", "create", "-q", bundle.to_str().unwrap(), &range],
        );
        self.admit_bundle(clone, &bundle, &head).await
    }

    async fn admit_bundle(
        &self,
        clone: &Cloned,
        bundle: &std::path::Path,
        head: &str,
    ) -> std::result::Result<Harvest, Refusal> {
        let head = CommitSha::try_from(head.to_owned()).unwrap();
        let base_commit = CommitSha::try_from(clone.workspace.base_commit.clone()).unwrap();
        self.forge
            .admit(Proposal {
                ticket: &clone.id,
                repo: &repo(REPO),
                head: &head,
                bundle,
                base: "main",
                base_commit: &base_commit,
            })
            .await
            .unwrap()
    }

    fn branch(clone: &Cloned) -> BranchName {
        BranchName::for_ticket(&clone.id, "Fix the flaky retry test")
    }

    /// Run the Effect for `head`, with `lease` the head last delivered.
    async fn publish(&self, clone: &Cloned, head: &str, lease: Option<&str>) -> Result<Observed> {
        let head = CommitSha::try_from(head.to_owned()).unwrap();
        let lease = lease.map(|sha| CommitSha::try_from(sha.to_owned()).unwrap());
        let origin = {
            let mut origin = request("t1", 12, "@solos-bot fix the flaky retry test");
            origin.meta = json!({"kind":"comment","repo":REPO,"number":12,"pull_request":false});
            origin
        };
        let (repo, branch) = (repo(REPO), Self::branch(clone));
        self.forge
            .execute(Execute::Publish(Publish {
                ticket: &clone.id,
                repo: &repo,
                branch: &branch,
                base: "main",
                head: &head,
                lease: lease.as_ref(),
                title: "Fix the flaky retry test",
                text: "Root cause: the clock. Fix: read once.",
                requester: &Actor::person("alice"),
                source: "github",
                origin: &origin,
            }))
            .await
    }
}

fn published(observed: Observed) -> (u64, bool) {
    match observed {
        Observed::Published { pr, created, .. } => (pr.number, created),
        other => panic!("{other:?}"),
    }
}

fn withheld(observed: Observed) -> Withheld {
    match observed {
        Observed::Withheld { why, .. } => why,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn admit_judges_the_agents_own_commits_and_each_refusal_stands_alone() {
    let f = Forged::new("admit", |_| {}).await;
    let bot = "solos-bot[bot]";
    let bot_email = "4242+solos-bot[bot]@users.noreply.github.com";

    // Nothing of its own: people landed a commit on main, and the worker only
    // took it in.
    let empty = f.clone_for("tk_empty").await;
    std::fs::write(f.upstream.join("other.txt"), "theirs\n").unwrap();
    git(&f.upstream, &["add", "-A"]);
    git(&f.upstream, &["commit", "--quiet", "-m", "alice's work"]);
    git(
        &f.upstream,
        &["push", "--quiet", f.bare().to_str().unwrap(), "main"],
    );
    f.clone_for("tk_refresh").await;
    as_worker(
        &empty.work,
        &["fetch", "-q", f.bare().to_str().unwrap(), "main"],
    );
    as_worker(&empty.work, &["merge", "-q", "--ff-only", "FETCH_HEAD"]);
    assert_eq!(f.admit(&empty).await, Err(Refusal::NoCommits));

    // One honest commit is admitted, and again, the same.
    let honest = f.clone_for("tk_honest").await;
    let head = f.commit(&honest, "src/fix.txt", "fixed");
    let admitted = f.admit(&honest).await.unwrap();
    assert_eq!(
        (
            admitted.head.as_str(),
            admitted.base.as_str(),
            admitted.commits,
            admitted.files
        ),
        (head.as_str(), "main", 1, 1)
    );
    assert!(admitted.bytes > 0);
    assert_eq!(
        f.admit(&honest).await.unwrap(),
        admitted,
        "the same head is answered the same"
    );
    let mirror = f.scratch.root.join("state/forge/acme/widgets.git");
    let refs = git_dir(
        &mirror,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/solos/tk_honest",
        ],
    );
    assert_eq!(
        refs,
        format!("refs/solos/tk_honest/{head}"),
        "one staging ref for it"
    );
    assert!(
        f.scratch
            .root
            .join("state/forge-stage/tk_honest")
            .join(format!("{head}.bundle"))
            .is_file(),
        "the bundle is staged beside the mirrors, not in them"
    );

    // A commit by someone else, as its author; and as its committer alone.
    let gui = f.clone_for("tk_gui").await;
    std::fs::write(gui.work.join("a.txt"), "a").unwrap();
    as_worker(&gui.work, &["add", "-A"]);
    let by_gui = {
        let who = ["-c", "user.name=Gui", "-c", "user.email=gui@example.com"];
        let commit = [who.as_slice(), ["commit", "-q", "-m", "by gui"].as_slice()].concat();
        as_worker(&gui.work, &commit);
        as_worker(&gui.work, &["rev-parse", "HEAD"])
    };
    let Err(Refusal::Identity {
        commit,
        found,
        expected,
        ..
    }) = f.admit(&gui).await
    else {
        panic!("a commit by Gui was admitted")
    };
    assert_eq!(commit.as_str(), by_gui);
    assert_eq!(found, "Gui <gui@example.com> as its author");
    assert_eq!(expected, format!("{bot} <{bot_email}>"));
    let refs = git_dir(&mirror, &["for-each-ref", "refs/solos/tk_gui"]);
    assert_eq!(refs, "", "a refused proposal leaves no staging ref");

    let committer = f.clone_for("tk_committer").await;
    std::fs::write(committer.work.join("a.txt"), "a").unwrap();
    as_worker(&committer.work, &["add", "-A"]);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&committer.work)
        .args(["commit", "-q", "-m", "mine, committed by gui"])
        .env("GIT_COMMITTER_NAME", "Gui")
        .env("GIT_COMMITTER_EMAIL", "gui@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let Err(Refusal::Identity { found, .. }) = f.admit(&committer).await else {
        panic!("a commit committed by Gui was admitted")
    };
    assert_eq!(found, "Gui <gui@example.com> as its committer");

    // A protected path in an intermediate commit, whatever case, and deleted after.
    let protected = f.clone_for("tk_protected").await;
    f.commit(&protected, ".GitHub/Workflows/ci.yml", "on: push");
    std::fs::remove_file(protected.work.join(".GitHub/Workflows/ci.yml")).unwrap();
    as_worker(&protected.work, &["add", "-A"]);
    as_worker(&protected.work, &["commit", "-q", "-m", "drop it again"]);
    let Err(Refusal::Protected { paths, globs, .. }) = f.admit(&protected).await else {
        panic!("a protected path was admitted")
    };
    assert_eq!(
        (paths, globs),
        (
            vec![".GitHub/Workflows/ci.yml".to_owned()],
            vec![".github/**".to_owned()]
        )
    );

    // A tree that does not grow from the clone's base.
    let orphan = f.clone_for("tk_orphan").await;
    as_worker(&orphan.work, &["checkout", "-q", "--orphan", "solo"]);
    as_worker(&orphan.work, &["rm", "-rfq", "."]);
    f.commit(&orphan, "o.txt", "o");
    let Err(Refusal::NotDescendant { base }) = f.admit(&orphan).await else {
        panic!("an orphan was admitted")
    };
    assert_eq!(base.as_str(), orphan.workspace.base_commit);

    // Garbage, a link, and a bundle that holds another head than the one claimed.
    let bad = f.clone_for("tk_bad").await;
    let head = f.commit(&bad, "b.txt", "b");
    let out = f.scratch.root.join("out/bad");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("garbage.bundle"), "not a bundle at all").unwrap();
    let Err(Refusal::BadBundle(why)) = f
        .admit_bundle(&bad, &out.join("garbage.bundle"), &head)
        .await
    else {
        panic!("garbage was admitted")
    };
    assert!(!why.is_empty());
    std::os::unix::fs::symlink(f.scratch.root.join("agent.yaml"), out.join("link.bundle")).unwrap();
    let Err(Refusal::BadBundle(why)) = f.admit_bundle(&bad, &out.join("link.bundle"), &head).await
    else {
        panic!("a link was admitted")
    };
    assert!(why.contains("readable bundle"), "{why}");
    f.admit(&bad).await.unwrap();
    let other = f.commit(&bad, "c.txt", "c");
    let stale = f
        .scratch
        .root
        .join("out")
        .join("tk_bad")
        .join("proposal.bundle");
    let Err(Refusal::BadBundle(why)) = f.admit_bundle(&bad, &stale, &other).await else {
        panic!("a bundle of another head was admitted")
    };
    assert!(why.contains("does not hold the head"), "{why}");
}

#[tokio::test]
async fn a_bundle_whose_objects_are_malformed_is_refused_by_the_fsck_not_taken() {
    let f = Forged::new("fsck", |_| {}).await;
    let clone = f.clone_for("tk_1").await;
    let base = clone.workspace.base_commit.clone();
    let tree = as_worker(&clone.work, &["rev-parse", "HEAD^{tree}"]);
    let body = format!(
        "tree {tree}\nparent {base}\nauthor bad <> notanumber +0000\ncommitter bad <> notanumber +0000\n\nmsg\n"
    );
    let mut hash = std::process::Command::new("git")
        .arg("-C")
        .arg(&clone.work)
        .args([
            "hash-object",
            "-t",
            "commit",
            "--literally",
            "-w",
            "--stdin",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut hash.stdin.take().unwrap(), body.as_bytes()).unwrap();
    let bad = String::from_utf8(hash.wait_with_output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned();
    as_worker(&clone.work, &["update-ref", "HEAD", &bad]);

    let refused = f.admit(&clone).await;

    let Err(Refusal::BadBundle(why)) = refused else {
        panic!("a malformed commit was admitted: {refused:?}")
    };
    assert!(
        why.contains("fsck") || why.contains("badDate") || why.contains("bad date"),
        "{why}"
    );
}

#[tokio::test]
async fn a_conflict_repair_that_merged_a_newer_base_is_judged_on_its_own_commits_alone() {
    let f = Forged::new("repair", |_| {}).await;
    let clone = f.clone_for("tk_1").await;

    // Meanwhile people land commits on main, one under `.github/`, as Alice.
    std::fs::create_dir_all(f.upstream.join(".github/workflows")).unwrap();
    std::fs::write(f.upstream.join(".github/workflows/ci.yml"), "on: push\n").unwrap();
    std::fs::write(f.upstream.join("other.txt"), "theirs\n").unwrap();
    git(&f.upstream, &["add", "-A"]);
    git(&f.upstream, &["commit", "--quiet", "-m", "alice's work"]);
    git(
        &f.upstream,
        &["push", "--quiet", f.bare().to_str().unwrap(), "main"],
    );
    // Another ticket's clone refreshes the host's mirror.
    f.clone_for("tk_2").await;

    // The worker merged that base into its branch and added its own commit.
    as_worker(
        &clone.work,
        &["fetch", "-q", f.bare().to_str().unwrap(), "main"],
    );
    f.commit(&clone, "mine.txt", "mine");
    as_worker(&clone.work, &["merge", "-q", "--no-edit", "FETCH_HEAD"]);

    let admitted = f
        .admit(&clone)
        .await
        .expect("the base's commits are the base's, not the agent's");

    assert_eq!(admitted.commits, 2, "its own commit and its own merge");
}

#[tokio::test]
async fn a_change_over_the_repositorys_limit_is_refused_by_the_forge() {
    let f = Forged::new("too-big", |terms| terms.max_change_bytes = 3_000).await;
    let clone = f.clone_for("tk_1").await;
    let big: String = (0..2_000)
        .map(|n| format!("{:x}", n * 2_654_435_761u64))
        .collect();
    f.commit(&clone, "big.txt", &big);

    let Err(Refusal::TooLarge { bytes, limit }) = f.admit(&clone).await else {
        panic!("a big change was admitted")
    };

    assert_eq!(limit, 3_000);
    assert!(bytes > limit);
}

#[tokio::test]
async fn publish_pushes_exactly_the_commits_and_opens_one_ready_pull_request_however_often_it_runs()
{
    let f = Forged::new("publish", |_| {}).await;
    let clone = f.clone_for("tk_1").await;
    let head = f.commit(&clone, "fix.txt", "fixed");
    f.admit(&clone).await.unwrap();
    let branch = Forged::branch(&clone);

    let first = f.publish(&clone, &head, None).await.unwrap();
    // The first result is dropped, as a crash drops it; the Effect runs again.
    let again = f.publish(&clone, &head, None).await.unwrap();

    assert_eq!(published(first.clone()), (71, true));
    assert_eq!(
        published(again.clone()),
        (71, false),
        "found, not made again"
    );
    let Observed::Published {
        thread,
        reply_to,
        url,
        head: seen,
        ..
    } = first
    else {
        unreachable!()
    };
    assert_eq!(thread, github::thread(REPO, 71));
    assert_eq!(reply_to, github::reply_to(REPO, 71));
    assert_eq!(url, "https://github.com/acme/widgets/pull/71");
    assert_eq!(seen.as_str(), head);
    assert_eq!(f.pushes().len(), 1, "one branch update: {:?}", f.pushes());
    assert_eq!(
        git_dir(
            &f.bare(),
            &["rev-list", "--reverse", &format!("{}..{branch}", f.tip)]
        ),
        head
    );
    let hub = f.fake.hub();
    assert_eq!((hub.pull_posts.len(), hub.pulls.len()), (1, 1));
    assert_eq!(hub.pull_posts[0].draft, json!(false));
    assert!(
        hub.pull_posts[0].body.contains("\nCloses #12\n"),
        "{}",
        hub.pull_posts[0].body
    );
    assert_eq!(hub.pulls[0]["user"]["login"], ME);
    drop(hub);
    assert_writes_are_narrowed(&f.fake);
}

#[tokio::test]
async fn a_second_head_moves_the_branch_under_its_lease_and_keeps_the_pull_request() {
    let f = Forged::new("second-head", |_| {}).await;
    let clone = f.clone_for("tk_1").await;
    let first = f.commit(&clone, "fix.txt", "v1");
    f.admit(&clone).await.unwrap();
    f.publish(&clone, &first, None).await.unwrap();
    let second = f.commit(&clone, "fix.txt", "v2");
    f.admit(&clone).await.unwrap();

    let observed = f.publish(&clone, &second, Some(&first)).await.unwrap();

    assert_eq!(published(observed), (71, false), "the same pull request");
    assert_eq!(f.pushes().len(), 2);
    let branch = Forged::branch(&clone);
    assert_eq!(
        git_dir(&f.bare(), &["rev-parse", &format!("refs/heads/{branch}")]),
        second
    );
    let hub = f.fake.hub();
    assert_eq!((hub.pull_posts.len(), hub.pulls.len()), (1, 1));
    assert_eq!(shown(&hub, &hub.pulls[0])["head"]["sha"], second);
}

#[tokio::test]
async fn someone_elses_push_to_the_branch_is_withheld_and_never_overwritten() {
    let f = Forged::new("moved", |_| {}).await;
    let clone = f.clone_for("tk_1").await;
    let first = f.commit(&clone, "fix.txt", "v1");
    f.admit(&clone).await.unwrap();
    f.publish(&clone, &first, None).await.unwrap();
    let branch = Forged::branch(&clone);
    // A collaborator pushes to the bot's branch (GitHub's "Update branch" counts).
    git(
        &f.upstream,
        &[
            "fetch",
            "--quiet",
            f.bare().to_str().unwrap(),
            branch.as_str(),
        ],
    );
    git(&f.upstream, &["checkout", "--quiet", "FETCH_HEAD"]);
    std::fs::write(f.upstream.join("theirs.txt"), "theirs\n").unwrap();
    git(&f.upstream, &["add", "-A"]);
    git(&f.upstream, &["commit", "--quiet", "-m", "alice pushes"]);
    let theirs = git(&f.upstream, &["rev-parse", "HEAD"]);
    git(
        &f.upstream,
        &[
            "push",
            "--quiet",
            f.bare().to_str().unwrap(),
            &format!("HEAD:refs/heads/{branch}"),
        ],
    );
    let second = f.commit(&clone, "fix.txt", "v2");
    f.admit(&clone).await.unwrap();

    let observed = f.publish(&clone, &second, Some(&first)).await.unwrap();

    assert_eq!(
        withheld(observed),
        Withheld::BranchMoved {
            found: CommitSha::try_from(theirs.clone()).unwrap()
        }
    );
    assert_eq!(
        git_dir(&f.bare(), &["rev-parse", &format!("refs/heads/{branch}")]),
        theirs,
        "their commit is still the branch's"
    );
    assert_eq!(f.fake.hub().pull_posts.len(), 1);

    // A branch someone made before the first delivery is not ours either.
    let other = f.clone_for("tk_2").await;
    let head = f.commit(&other, "x.txt", "x");
    f.admit(&other).await.unwrap();
    let theirs_first = Forged::branch(&other);
    git(
        &f.upstream,
        &[
            "push",
            "--quiet",
            f.bare().to_str().unwrap(),
            &format!("main:refs/heads/{theirs_first}"),
        ],
    );
    let found = git(&f.upstream, &["rev-parse", "main"]);
    let observed = f.publish(&other, &head, None).await.unwrap();
    assert_eq!(
        withheld(observed),
        Withheld::BranchMoved {
            found: CommitSha::try_from(found).unwrap()
        }
    );
}

#[tokio::test]
async fn a_pull_request_a_person_closed_is_not_reopened_and_one_that_is_not_ours_is_refused() {
    let f = Forged::new("closed", |_| {}).await;
    let clone = f.clone_for("tk_1").await;
    let first = f.commit(&clone, "fix.txt", "v1");
    f.admit(&clone).await.unwrap();
    f.publish(&clone, &first, None).await.unwrap();
    f.fake.hub().pulls[0]["state"] = json!("closed");
    let second = f.commit(&clone, "fix.txt", "v2");
    f.admit(&clone).await.unwrap();

    let observed = f.publish(&clone, &second, Some(&first)).await.unwrap();

    assert_eq!(withheld(observed), Withheld::PrClosed);
    {
        let hub = f.fake.hub();
        assert_eq!(
            (hub.pull_posts.len(), hub.pulls.len()),
            (1, 1),
            "nothing reopened or made"
        );
        assert_eq!(hub.pulls[0]["state"], "closed");
    }

    // A pull request on the Ticket's branch name without its marker.
    let other = f.clone_for("tk_2").await;
    let head = f.commit(&other, "x.txt", "x");
    f.admit(&other).await.unwrap();
    let branch = Forged::branch(&other);
    f.fake.hub().pulls.push(json!({
        "number": 5, "state": "open", "draft": false, "merged_at": null, "title": "squatter",
        "body": "no marker here", "html_url": format!("https://github.com/{REPO}/pull/5"),
        "head": {"ref": branch.as_str(), "label": format!("acme:{branch}"), "sha": head},
    }));
    let observed = f.publish(&other, &head, None).await.unwrap();
    let Withheld::Forbidden { message } = withheld(observed) else {
        panic!("a pull request that is not ours was taken")
    };
    assert!(message.contains("not this ticket's"), "{message}");
}

#[tokio::test]
async fn a_remote_that_refuses_the_push_in_words_is_a_decided_no_with_those_words() {
    let f = Forged::new("rejected", |_| {}).await;
    let clone = f.clone_for("tk_1").await;
    let head = f.commit(&clone, "fix.txt", "v1");
    f.admit(&clone).await.unwrap();
    std::fs::write(f.bare().join("reject"), "").unwrap();

    let observed = f.publish(&clone, &head, None).await.unwrap();

    let Withheld::Forbidden { message } = withheld(observed) else {
        panic!("a refused push was taken for a push")
    };
    assert!(message.contains("workflows permission"), "{message}");
    assert!(f.pushes().is_empty());
    assert!(
        f.fake.hub().pull_posts.is_empty(),
        "no pull request for a branch that is not there"
    );
}

#[tokio::test]
async fn a_read_only_agent_pushes_nothing_and_mints_no_write_token() {
    let f = Forged::with("read-only", false, |_| {}, |_| {}).await;
    let clone = f.clone_for("tk_1").await;
    let head = f.commit(&clone, "fix.txt", "v1");
    f.admit(&clone).await.unwrap();

    let observed = f.publish(&clone, &head, None).await.unwrap();

    assert_eq!(withheld(observed), Withheld::ReadOnly);
    assert!(f.pushes().is_empty());
    assert!(
        f.fake
            .mints()
            .iter()
            .all(|mint| mint.permissions["contents"] == "read"),
        "{:?}",
        f.fake.mints()
    );
}

#[tokio::test]
async fn a_suspended_installation_is_a_wait_and_the_publish_converges_once_it_is_lifted() {
    let f = Forged::new("suspended", |_| {}).await;
    let clone = f.clone_for("tk_1").await;
    let head = f.commit(&clone, "fix.txt", "v1");
    f.admit(&clone).await.unwrap();
    f.fake.hub().suspended = true;

    let err = f.publish(&clone, &head, None).await.unwrap_err();

    assert_eq!(retry_after(&err), Some(SUSPENDED_WAIT), "{err:#}");
    assert!(f.pushes().is_empty());
    f.fake.hub().suspended = false;
    let observed = f.publish(&clone, &head, None).await.unwrap();
    assert_eq!(published(observed), (71, true));
    assert_eq!(f.pushes().len(), 1);
}

#[tokio::test]
async fn a_crash_after_the_push_and_before_the_pull_request_converges_on_the_repeat() {
    let f = Forged::new("crash-push", |_| {}).await;
    let clone = f.clone_for("tk_1").await;
    let head = f.commit(&clone, "fix.txt", "v1");
    f.admit(&clone).await.unwrap();
    f.fake
        .hub()
        .pull_faults
        .push_back(PullFault::Refuse(502, "Bad Gateway"));

    let first = f.publish(&clone, &head, None).await;

    assert!(first.is_err(), "the pull request was not made");
    assert_eq!(f.pushes().len(), 1);
    let observed = f.publish(&clone, &head, None).await.unwrap();
    assert_eq!(published(observed), (71, true));
    assert_eq!(
        f.pushes().len(),
        1,
        "the branch already held the head: no second push"
    );
    assert_eq!(f.fake.hub().pulls.len(), 1);
}

#[tokio::test]
async fn a_crash_after_the_pull_request_and_before_the_fact_finds_it_and_makes_no_second() {
    let f = Forged::new("crash-pr", |_| {}).await;
    let clone = f.clone_for("tk_1").await;
    let head = f.commit(&clone, "fix.txt", "v1");
    f.admit(&clone).await.unwrap();
    f.fake.hub().pull_faults.push_back(PullFault::Create500);

    let first = f.publish(&clone, &head, None).await;

    assert!(first.is_err(), "GitHub made it and answered 500");
    let observed = f.publish(&clone, &head, None).await.unwrap();
    assert_eq!(published(observed), (71, false), "found, not made again");
    let hub = f.fake.hub();
    assert_eq!((hub.pull_posts.len(), hub.pulls.len()), (1, 1));
    drop(hub);
    assert_eq!(f.pushes().len(), 1);
}

#[tokio::test]
async fn a_mirror_made_again_loses_the_staging_ref_and_the_publish_makes_it_from_the_staged_bundle()
{
    let f = Forged::new("rebuilt", |_| {}).await;
    let clone = f.clone_for("tk_1").await;
    let head = f.commit(&clone, "fix.txt", "v1");
    f.admit(&clone).await.unwrap();
    std::fs::remove_dir_all(f.scratch.root.join("state/forge")).unwrap();
    // The next ticket's clone makes the mirror again.
    f.clone_for("tk_2").await;
    let mirror = f.scratch.root.join("state/forge/acme/widgets.git");
    assert_eq!(
        git_dir(&mirror, &["for-each-ref", "refs/solos"]),
        "",
        "the cache is gone"
    );

    let observed = f.publish(&clone, &head, None).await.unwrap();

    assert_eq!(published(observed), (71, true));
    let branch = Forged::branch(&clone);
    assert_eq!(
        git_dir(&f.bare(), &["rev-parse", &format!("refs/heads/{branch}")]),
        head
    );
}

// ---- The runtime delivers what the Store owes ----

use solos_agent_core::Engine;
use solos_agent_core::change::{ForgeTerms, MergeMode};
use solos_agent_core::model::{
    Accept, Acknowledge, DomainEvent, Method, Plan, ReportIn, ReportStatus, Verdict,
};

/// A forge that does what it is asked and then never answers, as a process
/// that is killed the moment the push and the pull request are done.
struct HangsAfter(Arc<dyn Forge>);

impl Forge for HangsAfter {
    fn prepare<'a>(
        &'a self,
        repo: &'a RepoName,
        dirs: TicketDirs<'a>,
    ) -> BoxFuture<'a, Result<solos_swarm::agent::ports::Workspace>> {
        self.0.prepare(repo, dirs)
    }
    fn admit<'a>(
        &'a self,
        proposal: Proposal<'a>,
    ) -> BoxFuture<'a, Result<std::result::Result<Harvest, Refusal>>> {
        self.0.admit(proposal)
    }
    fn execute<'a>(&'a self, job: Execute<'a>) -> BoxFuture<'a, Result<Observed>> {
        Box::pin(async move {
            let _ = self.0.execute(job).await;
            std::future::pending().await
        })
    }
}

impl Forged {
    /// A Ticket on acme/widgets whose worker's `ready` was admitted: the
    /// commit that accepted it, with the Publish queued and every other
    /// Effect done. Its head.
    async fn proposed(&self) -> (TicketId, String) {
        let mut engine = Engine::new(self.scratch.store(), policy(), || 1_000u64);
        let mut ask = request("t1", 12, "@solos-bot fix the flaky retry test");
        ask.meta = json!({"kind":"comment","repo":REPO,"number":12,"pull_request":false});
        engine.receive(ask).unwrap();
        let eval = engine.evaluate_due().unwrap().remove(0);
        engine
            .decide(
                &eval,
                Verdict::Accept(Accept {
                    title: "Fix the flaky retry test".into(),
                    scope: "Fix it".into(),
                    reason: "alice asked".into(),
                    plan: Plan::Session {
                        harness: None,
                        environment: None,
                        method: Some(Method {
                            deliverable: Deliverable::PullRequest,
                        }),
                    },
                    budget_microusd: Some(BUDGET),
                    acknowledge: Acknowledge::Silent,
                    rules: Vec::new(),
                    repo: Some(repo(REPO)),
                    forge: Some(ForgeTerms {
                        merge: MergeMode::Ask,
                        ci_expected: true,
                        max_pushes: 6,
                        max_repairs: 2,
                    }),
                }),
            )
            .unwrap();
        let ticket = engine.repos().tickets().unwrap().remove(0);
        let assignment = ticket.assignments[0].id.clone();
        engine.assignment_started(&ticket.id, &assignment).unwrap();
        let clone = self.clone_for(ticket.id.as_str()).await;
        let head = self.commit(&clone, "fix.txt", "fixed");
        let harvest = self.admit(&clone).await.unwrap();
        engine
            .report(
                &ticket.id,
                &assignment,
                ReportIn {
                    status: ReportStatus::Ready,
                    text: "Root cause: the clock. Fix: read once.".into(),
                    question: None,
                    spent_microusd: 0,
                    harvest: Some(harvest),
                },
            )
            .unwrap();
        for queued in engine.repos().pending_effects().unwrap() {
            if !matches!(queued.effect, Effect::Forge(_)) {
                engine.repos().complete_effect(queued.id).unwrap();
            }
        }
        (ticket.id, head)
    }

    /// A runtime on the store, sending Results as the bot, with `forge`.
    fn runtime(&self, forge: Arc<dyn Forge>, paused: bool, until_idle: bool) -> Runtime {
        let adapters = Adapters {
            sources: Vec::new(),
            channels: BTreeMap::from([("github".to_owned(), self.fake.channel())]),
            router: Arc::new(ScriptedRouter::new(BUDGET)),
            executor: Arc::new(Hanging),
        };
        let options = RunOptions {
            until_idle,
            tick: Duration::from_millis(10),
            retry_base: Duration::from_millis(10),
            retry_max: Duration::from_millis(500),
        };
        let mut rules = policy();
        rules.paused = paused;
        Runtime::new(self.scratch.store(), rules, adapters, options).with_forge(Some(forge))
    }

    async fn settle(&self, forge: Arc<dyn Forge>) -> Summary {
        let runtime = self.runtime(forge, false, true);
        tokio::time::timeout(LIMIT, runtime.run(std::future::pending()))
            .await
            .expect("the run did not go idle")
            .unwrap()
    }

    fn delivered_events(&self) -> Vec<bool> {
        self.scratch
            .store()
            .events()
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.event {
                DomainEvent::Delivered { created, .. } => Some(created),
                _ => None,
            })
            .collect()
    }
}

#[tokio::test]
async fn a_crash_after_the_forge_was_made_to_match_and_before_the_fact_restarts_to_one_of_each() {
    let f = Forged::new("restart", |_| {}).await;
    let (ticket, head) = f.proposed().await;

    // The first run performs the Effect, and the process dies before it hears.
    let runtime = f.runtime(Arc::new(HangsAfter(f.forge.clone())), false, false);
    let cut = tokio::time::timeout(
        Duration::from_millis(3_000),
        runtime.run(std::future::pending()),
    )
    .await;
    assert!(cut.is_err(), "the run was cut short, as a crash cuts it");
    assert_eq!(f.pushes().len(), 1);
    assert_eq!(f.fake.hub().pull_posts.len(), 1);
    assert!(
        f.delivered_events().is_empty(),
        "the Store never heard of it"
    );
    let pending: Vec<_> = f.scratch.store().pending_effects().unwrap();
    assert!(
        pending.iter().any(|q| matches!(q.effect, Effect::Forge(_))),
        "{pending:?}"
    );

    // The restart redoes it on the same store, and converges.
    let summary = f.settle(f.forge.clone()).await;

    assert_eq!(summary.effects_pending, 0);
    assert_eq!(f.pushes().len(), 1, "one branch update: {:?}", f.pushes());
    assert_eq!(f.fake.hub().pull_posts.len(), 1, "one POST /pulls");
    assert_eq!(f.fake.hub().pulls.len(), 1);
    assert_eq!(
        f.delivered_events(),
        [false],
        "one Delivered, for the PR already made"
    );
    let store = f.scratch.store();
    let done = solos_agent_core::Repositories::ticket(&store, &ticket).unwrap();
    assert_eq!(
        done.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    assert_eq!(done.change.unwrap().head.as_str(), head);
    let told = f.fake.hub().posts.iter().filter(|p| p.number == 12).count();
    assert_eq!(told, 1, "the Result is posted once");
    assert_settled(&store);

    // And once more, restarted again: nothing changes.
    f.settle(f.forge.clone()).await;
    assert_eq!((f.pushes().len(), f.fake.hub().pull_posts.len()), (1, 1));
    assert_eq!(f.delivered_events().len(), 1);
}

#[tokio::test]
async fn a_pull_request_made_while_github_answered_500_is_found_by_the_retry_not_made_twice() {
    let f = Forged::new("restart-500", |_| {}).await;
    f.proposed().await;
    f.fake.hub().pull_faults.push_back(PullFault::Create500);

    let summary = f.settle(f.forge.clone()).await;

    assert_eq!((summary.effect_failures, summary.effects_pending), (1, 0));
    assert_eq!(f.fake.hub().pull_posts.len(), 1, "one POST /pulls");
    assert_eq!(f.fake.hub().pulls.len(), 1);
    assert_eq!(f.pushes().len(), 1);
    assert_eq!(f.delivered_events(), [false], "found, and recorded once");
}

#[tokio::test]
async fn a_pause_holds_the_push_and_the_pull_request_until_it_lifts() {
    let f = Forged::new("paused", |_| {}).await;
    f.proposed().await;

    let held = f.runtime(f.forge.clone(), true, true);
    let summary = tokio::time::timeout(LIMIT, held.run(std::future::pending()))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(summary.effects_pending, 1, "the Publish waits");
    assert!(f.pushes().is_empty());
    assert!(f.fake.hub().pull_posts.is_empty());
    let summary = f.settle(f.forge.clone()).await;
    assert_eq!(summary.effects_pending, 0);
    assert_eq!((f.pushes().len(), f.fake.hub().pulls.len()), (1, 1));
}

#[tokio::test]
async fn a_suspended_installation_holds_the_publish_and_it_converges_when_the_owner_lifts_it() {
    let f = Forged::with(
        "suspended-run",
        true,
        |config| config.suspended_wait = Duration::from_millis(50),
        |_| {},
    )
    .await;
    f.proposed().await;
    f.fake.hub().suspended = true;

    let runtime = f.runtime(f.forge.clone(), false, false);
    let driver = async {
        let deadline = Instant::now() + LIMIT;
        // It waits, failing and trying again, with nothing pushed.
        loop {
            let waiting = f
                .scratch
                .store()
                .pending_effects()
                .unwrap()
                .iter()
                .any(|q| matches!(q.effect, Effect::Forge(_)) && q.attempts >= 2);
            if waiting {
                break;
            }
            assert!(Instant::now() < deadline, "the Effect never retried");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(f.pushes().is_empty());
        f.fake.hub().suspended = false;
        while f.delivered_events().is_empty() {
            assert!(Instant::now() < deadline, "the Effect never converged");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    drive(runtime, driver).await;

    assert_eq!(f.pushes().len(), 1);
    assert_eq!(f.fake.hub().pulls.len(), 1);
    assert_eq!(f.delivered_events(), [true]);
}

// ---- A worker that writes its own clone's config ----

/// The inner half of `a_clone_that_plants_its_own_git_config_...`. It runs in
/// a child process whose PATH starts with a `git` that logs every call, so it
/// is ignored unless that test starts it.
#[tokio::test]
#[ignore = "started by a_clone_that_plants_its_own_git_config_never_runs_or_steers_the_hosts_git"]
async fn hostile_clone_inner() {
    let Some(report) = std::env::var_os("SHIPPING_REPORT").map(PathBuf::from) else {
        return;
    };
    let flag = PathBuf::from(std::env::var_os("SHIPPING_FLAG").unwrap());
    let Some(d) = Shipping::new("hostile", "").await else {
        return;
    };
    d.ask(12, 101, "@solos-bot fix the flaky retry test");
    let root = d.scratch.root.display().to_string();
    let marker = std::env::temp_dir().join(format!("solos-hostile-ran-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    // Where a redirected push would land.
    let evil = d.scratch.root.join("evil.git");
    git(
        &d.scratch.root,
        &["init", "--quiet", "--bare", evil.to_str().unwrap()],
    );
    let ran = marker.display();
    let plant = format!(
        "printf '#!/bin/sh\\necho ran >> {ran}\\n' > fsmon.sh && chmod +x fsmon.sh && \
         mkdir -p hooks && for h in pre-push pre-commit post-commit reference-transaction; do \
           printf '#!/bin/sh\\necho ran >> {ran}\\n' > hooks/$h; chmod +x hooks/$h; done && \
         git config core.fsmonitor ./fsmon.sh && git config core.hooksPath ./hooks && \
         git config core.sshCommand ./fsmon.sh && git config core.pager ./fsmon.sh && \
         git config credential.helper '!f() {{ echo ran >> {ran}; }}; f' && \
         git config url.file://{evil}/.insteadOf file://{root}/remote/acme/ && \
         git config remote.origin.pushurl file://{evil} && \
         head=$(git rev-parse HEAD) && decoy=$(git -c user.name=Gui -c user.email=gui@example.com \
           commit-tree -p {base} -m decoy 'HEAD^{{tree}}') && git replace -f $head $decoy",
        evil = evil.display(),
        base = d.tip,
    );
    let steps = vec![
        run_code(&commit(
            "fix.txt",
            "fixed",
            "jobs: read the clock after arming",
        )),
        run_code(&plant),
        ready("Fixed it."),
    ];

    d.serve(None, vec![steps]).await;
    // The log is only for what the host did while the worker's tree was its own.
    std::fs::remove_file(&flag).unwrap();

    let answers = d.answers(0);
    for (n, (_, status, reply)) in answers.iter().enumerate() {
        assert_eq!(*status, 200, "{n}: {reply}");
    }
    assert_eq!(answers[1].2["exit_code"], 0, "{}", answers[1].2);
    assert!(
        !marker.exists(),
        "something the clone's config named ran on the host"
    );
    let ticket = d.ticket();
    let branch = ticket.change.as_ref().unwrap().branch.as_str().to_owned();
    let work = d.work_of(&ticket.id);
    let real = std::process::Command::new("git")
        .arg("-C")
        .arg(&work)
        .args(["--no-replace-objects", "rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    let real = String::from_utf8(real.stdout).unwrap().trim().to_owned();
    assert_eq!(
        git_dir(&d.bare(), &["rev-parse", &format!("refs/heads/{branch}")]),
        real,
        "the branch holds the worker's own commit, not what a replace ref says"
    );
    assert_eq!(
        git_dir(
            &d.bare(),
            &[
                "log",
                "-1",
                "--format=%an <%ae>",
                &format!("refs/heads/{branch}")
            ]
        ),
        "solos-bot[bot] <4242+solos-bot[bot]@users.noreply.github.com>"
    );
    assert_eq!(
        git_dir(&evil, &["for-each-ref"]),
        "",
        "nothing was redirected to the host the clone named"
    );
    assert_eq!(d.fake.hub().pulls.len(), 1);
    assert_no_token_on_disk(&d.fake, &d.scratch.root);
    let tokens: Vec<String> = d.fake.hub().tokens.keys().cloned().collect();
    std::fs::write(
        &report,
        json!({"root": root, "ticket": ticket.id, "tokens": tokens}).to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn a_clone_that_plants_its_own_git_config_never_runs_or_steers_the_hosts_git() {
    if !sandbox() {
        return;
    }
    let dir = temp_dir("hostile-outer");
    let wrapper = dir.join("bin");
    std::fs::create_dir_all(&wrapper).unwrap();
    let real = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(real.stdout).unwrap().trim().to_owned();
    let (log, flag, report) = (
        dir.join("git.log"),
        dir.join("flag"),
        dir.join("report.json"),
    );
    std::fs::write(&flag, "").unwrap();
    let script = wrapper.join("git");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n[ -e '{}' ] && printf '%s\\t%s\\n' \"$PWD\" \"$*\" >> '{}'\nexec {real} \"$@\"\n",
            flag.display(),
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        wrapper.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "hostile_clone_inner",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("PATH", path)
        .env("SHIPPING_REPORT", &report)
        .env("SHIPPING_FLAG", &flag)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&child.stdout).into_owned()
        + &String::from_utf8_lossy(&child.stderr);
    assert!(child.status.success(), "{said}");

    let seen: Value = serde_json::from_slice(&std::fs::read(&report).expect(&said)).unwrap();
    let root = seen["root"].as_str().unwrap();
    let work = format!(
        "{root}/state/tickets/{}/work",
        seen["ticket"].as_str().unwrap()
    );
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        calls.lines().count() > 3,
        "the wrapper saw the host's git: {calls}"
    );
    for line in calls.lines() {
        let (cwd, args) = line.split_once('\t').unwrap();
        assert!(
            !cwd.starts_with(&work),
            "git ran in the worker's tree: {line}"
        );
        // Before the clone is whole the host makes it; after, it never names it.
        if args.contains(&work) {
            let command = args
                .split_whitespace()
                .find(|word| ["clone", "remote"].contains(word));
            assert!(command.is_some(), "git named the worker's tree: {line}");
        }
        for token in seen["tokens"].as_array().unwrap() {
            assert!(
                !line.contains(token.as_str().unwrap()),
                "a token is in git's argv: {line}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- A worker that answers in plain text, with no report ----

/// One turn of a worker: the tool calls it makes, then what it says.
struct Turn {
    steps: Vec<Step>,
    says: Option<&'static str>,
}

/// Plays a worker's turns in order, whatever prompts it, with the Router on
/// the scripted harness. A turn ends with the text it says, as Pi's does when
/// its model answers in plain text, or with nothing.
struct Talking {
    router: ScriptedHarness,
    turns: Arc<Mutex<VecDeque<Turn>>>,
    seen: Arc<Mutex<Vec<Answer>>>,
    prompts: Arc<Mutex<Vec<String>>>,
}

enum TalkingSession {
    Router(solos_swarm::harness::scripted::ScriptedSession),
    Worker(Box<TalkingWorker>),
}

struct TalkingWorker {
    id: String,
    spec: solos_swarm::harness::SessionSpec,
    events: tokio::sync::mpsc::Sender<solos_swarm::harness::Observation>,
    turns: Arc<Mutex<VecDeque<Turn>>>,
    seen: Arc<Mutex<Vec<Answer>>>,
    prompts: Arc<Mutex<Vec<String>>>,
}

impl solos_swarm::harness::AgentHarness for Talking {
    type Session = TalkingSession;
    async fn start(
        &self,
        spec: &solos_swarm::harness::SessionSpec,
        events: tokio::sync::mpsc::Sender<solos_swarm::harness::Observation>,
    ) -> Result<TalkingSession> {
        if !matches!(spec.role, solos_swarm::harness::Role::TicketWorker) {
            return Ok(TalkingSession::Router(
                self.router.start(spec, events).await?,
            ));
        }
        Ok(TalkingSession::Worker(Box::new(TalkingWorker {
            id: spec.id.clone(),
            spec: spec.clone(),
            events,
            turns: self.turns.clone(),
            seen: self.seen.clone(),
            prompts: self.prompts.clone(),
        })))
    }
}

impl solos_swarm::harness::Session for TalkingSession {
    fn id(&self) -> &str {
        match self {
            Self::Router(session) => solos_swarm::harness::Session::id(session),
            Self::Worker(worker) => &worker.id,
        }
    }
    async fn prompt(&mut self, text: &str) -> Result<()> {
        let Self::Worker(worker) = self else {
            let Self::Router(session) = self else {
                unreachable!()
            };
            return solos_swarm::harness::Session::prompt(session, text).await;
        };
        worker.prompts.lock().unwrap().push(text.to_owned());
        let turn = worker.turns.lock().unwrap().pop_front();
        let (id, spec, events) = (
            worker.id.clone(),
            worker.spec.clone(),
            worker.events.clone(),
        );
        let seen = worker.seen.clone();
        // The turn runs on its own task, as a model's does.
        tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .unix_socket(spec.broker_socket.clone())
                .build()
                .unwrap();
            if let Some(turn) = turn {
                for (tool, arguments) in turn.steps {
                    let response = client
                        .post(format!("http://broker/tool/{tool}"))
                        .bearer_auth(&spec.token)
                        .json(&arguments)
                        .send()
                        .await
                        .unwrap();
                    let status = response.status().as_u16();
                    let reply = response.json().await.unwrap_or(Value::Null);
                    seen.lock().unwrap().push((tool.to_owned(), status, reply));
                }
                if let Some(said) = turn.says {
                    let said = (
                        id.clone(),
                        Ok(solos_swarm::harness::SessionEvent::Said(said.into())),
                    );
                    events.send(said).await.ok();
                }
            }
            let ended = (id, Ok(solos_swarm::harness::SessionEvent::TurnEnded));
            events.send(ended).await.ok();
        });
        Ok(())
    }
    async fn stop(&mut self) -> Result<bool> {
        match self {
            Self::Router(session) => solos_swarm::harness::Session::stop(session).await,
            Self::Worker(_) => Ok(true),
        }
    }
    async fn join(self) -> usize {
        match self {
            Self::Router(session) => solos_swarm::harness::Session::join(session).await,
            Self::Worker(_) => 0,
        }
    }
}

impl Shipping {
    /// Run the agent until it is idle, with a worker that plays `turns`.
    async fn talk(&self, turns: Vec<Turn>) -> (Summary, Vec<Answer>, Vec<String>) {
        let key = self.scratch.root.join("github-app.pem");
        solos_swarm::sealed::private_file(&key, KEY.as_bytes(), true).unwrap();
        let config = AgentConfig::load(&self.scratch.root.join("agent.yaml")).unwrap();
        let (seen, prompts) = (
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
        );
        let harness = Talking {
            router: ScriptedHarness::new(BTreeMap::new()).every_turn(vec![Call::Replied]),
            turns: Arc::new(Mutex::new(turns.into())),
            seen: seen.clone(),
            prompts: prompts.clone(),
        };
        let pi_root = self.scratch.root.join("pi");
        let host = PiHost {
            harness,
            provider: Arc::new(Model {
                deliverable: None,
                title: "Fix the flaky retry test",
            }),
            node: pi_root.join("bin/node"),
            pi_root,
            browser: solos_swarm::browse::Launcher::Lightpanda,
        };
        let state = self.scratch.root.join("state");
        let summary = tokio::time::timeout(
            Duration::from_secs(120),
            serve_with(config, &state, true, Some(host)),
        )
        .await
        .expect("agent serve did not go idle")
        .unwrap();
        let (seen, prompts) = (
            seen.lock().unwrap().clone(),
            prompts.lock().unwrap().clone(),
        );
        (summary, seen, prompts)
    }
}

#[tokio::test]
async fn a_plain_text_done_on_a_pull_request_ticket_is_admitted_like_a_report_and_delivers_it() {
    let Some(d) = Shipping::new("plain-done", "").await else {
        return;
    };
    d.ask(12, 101, "@solos-bot fix the flaky retry test");
    let said = "Fixed it: the clock is read once, after arming the wait.";

    let (summary, tools, _) = d
        .talk(vec![Turn {
            steps: vec![run_code(&commit(
                "fix.txt",
                "fixed",
                "jobs: read the clock after arming",
            ))],
            says: Some(said),
        }])
        .await;

    assert_eq!(summary.effects_pending, 0);
    assert_eq!(
        tools.len(),
        1,
        "the worker never called worker_report: {tools:?}"
    );
    let ticket = d.ticket();
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    let change = ticket.change.clone().unwrap();
    let branch = change.branch.as_str().to_owned();
    let head = git_dir(&d.bare(), &["rev-parse", &format!("refs/heads/{branch}")]);
    assert_eq!(
        change.head.as_str(),
        head,
        "the host admitted what the worker committed"
    );
    assert_eq!(change.delivered.as_ref().map(|d| d.pr.number), Some(71));
    let (pulls, posts) = {
        let hub = d.fake.hub();
        (hub.pulls.len(), hub.pull_posts.len())
    };
    assert_eq!((pulls, posts), (1, 1), "a pull request exists");
    let told = d.said_on(12);
    assert!(
        told.last().unwrap().contains(&format!(
            "{said}\n\nPR https://github.com/acme/widgets/pull/71 · head {} · opened",
            &head[..12]
        )),
        "{told:?}"
    );
    let events = d.events();
    let admitted = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                solos_agent_core::model::DomainEvent::ProposalAdmitted { .. }
            )
        })
        .count();
    assert_eq!(admitted, 1);
}

#[tokio::test]
async fn a_plain_text_done_with_nothing_committed_ends_nothing_and_the_worker_is_told_why() {
    let Some(d) = Shipping::new("plain-nothing", "").await else {
        return;
    };
    d.ask(12, 101, "@solos-bot fix the flaky retry test");

    let (summary, tools, prompts) = d
        .talk(vec![
            Turn {
                steps: Vec::new(),
                says: Some("All done, it was already fixed."),
            },
            Turn {
                steps: vec![run_code(&commit(
                    "fix.txt",
                    "fixed",
                    "jobs: read the clock after arming",
                ))],
                says: Some("Now it is really done."),
            },
        ])
        .await;

    assert_eq!(summary.effects_pending, 0);
    assert_eq!(tools.len(), 1);
    // The first "done" ended nothing: the worker was prompted again, with why.
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert!(
        prompts[1].contains("did not accept it") && prompts[1].contains("Commit your change"),
        "{}",
        prompts[1]
    );
    // The second, with a commit, is admitted and delivers the pull request.
    let ticket = d.ticket();
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed)
    );
    assert_eq!(d.fake.hub().pulls.len(), 1);
    let told = d.said_on(12);
    assert!(
        told.iter()
            .all(|text| !text.contains("All done, it was already fixed.")),
        "the refused text was told to nobody: {told:?}"
    );
    assert!(
        told.last().unwrap().starts_with("Now it is really done."),
        "{told:?}"
    );
}
