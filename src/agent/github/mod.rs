//! GitHub: a Source that polls the REST API for the configured repositories,
//! and a Channel that comments on issues and pull requests. Triggers name
//! source [`SOURCE`]; an issue or pull request is the Thread
//! `github:<owner>/<repo>#<number>`.
//!
//! The agent acts as a GitHub App's bot and holds no other credential. Every
//! call names a repository and gets a token minted for that repository alone,
//! with the access its HTTP method needs (see [`app`]).

mod app;
mod channel;
mod check;
mod identities;
mod source;

pub use app::{
    Access, AppId, AppKey, Bot, InstallState, InstallToken, Minter, MinterConfig, Now, Preflight,
    REFRESH_MARGIN, SUSPENDED_WAIT,
};
pub use channel::GithubChannel;
pub use check::{CheckArgs, check};
pub use identities::{IDENTITIES_FILE, Identities};
pub use source::{GithubSource, OVERLAP};

use super::config::GithubConfig;
use super::ports::RetryAfter;
use anyhow::{Context, Result, bail, ensure};
use reqwest::header::{ACCEPT, ETAG, HeaderMap, IF_NONE_MATCH, LINK, RETRY_AFTER};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use solos_agent_core::change::RepoName;
use solos_agent_core::model::{ChannelRef, MessageId, ThreadKey};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The Source name GitHub Triggers and replies carry.
pub const SOURCE: &str = "github";
pub const DEFAULT_API_BASE: &str = "https://api.github.com/";
/// Where the host's mirrors fetch from, unless a test points `git_base` at a
/// directory of bare repositories.
pub const DEFAULT_GIT_BASE: &str = "https://github.com/";
/// The Source's read position, in the state directory.
pub const CURSOR_FILE: &str = "github-cursor.json";

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRY_AFTER: u64 = 3_600;
/// GitHub asks for at least a minute after a secondary rate limit that names
/// no wait.
const SECONDARY_WAIT: u64 = 60;
const API_VERSION: &str = "2022-11-28";

/// The Thread of an issue or pull request.
pub fn thread(repo: &str, number: u64) -> ThreadKey {
    ThreadKey::new(format!("{SOURCE}:{repo}#{number}"))
}

/// Where a comment on an issue or pull request goes.
pub fn reply_to(repo: &str, number: u64) -> ChannelRef {
    ChannelRef {
        source: SOURCE.into(),
        address: BTreeMap::from([
            ("repo".to_owned(), repo.to_owned()),
            ("number".to_owned(), number.to_string()),
        ]),
    }
}

/// `owner/name` as GitHub names repositories, so a name cannot reach another
/// API path.
pub fn valid_repo(repo: &str) -> bool {
    let part = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    repo.split_once('/')
        .is_some_and(|(owner, name)| part(owner) && part(name))
}

/// The hidden line that ends every comment the agent posts, naming its
/// `OutboundMessage`.
pub fn marker(id: &MessageId) -> String {
    format!("<!-- solos-swarm message {id} -->")
}

pub(crate) const MARKER_PREFIX: &str = "<!-- solos-swarm message ";

/// What `agent serve` builds from the `github:` section: the one Minter
/// (the only holder of the App's key), the REST client the Source and the
/// Channel share, and where the forge's mirrors fetch from.
pub struct GithubApp {
    pub api: Arc<GithubApi>,
    pub minter: Arc<Minter>,
    /// Ends in `/`; a mirror fetches `<git_base><owner>/<name>.git`.
    pub git_base: String,
}

impl GithubApp {
    /// Read the key file once and delete it, as every token file is. `repos`
    /// is `forge.repos`, the only repositories a token is minted for;
    /// `listed` the Policy's `github:` logins, whose account ids are pinned
    /// in `state_dir`.
    pub fn open(
        config: &GithubConfig,
        repos: BTreeSet<RepoName>,
        state_dir: &Path,
        listed: BTreeSet<String>,
    ) -> Result<Self> {
        let key = AppKey::take(&config.key_file).context("github.key_file")?;
        let minter =
            MinterConfig::new(AppId(config.app_id), &config.api_base, config.writes, repos);
        let minter = Arc::new(Minter::new(minter, key, Arc::new(SystemTime::now))?);
        let identities = Identities::load(state_dir.join(IDENTITIES_FILE), listed)?;
        let api = Arc::new(GithubApi::new(minter.clone(), Arc::new(identities))?);
        let mut git_base = config.git_base.trim().to_owned();
        if !git_base.ends_with('/') {
            git_base.push('/');
        }
        Ok(Self {
            api,
            minter,
            git_base,
        })
    }
}

/// The REST API, shared by the Source and the Channel so a rate limit one
/// meets holds the other back too (and the Minter's own calls with them).
/// `repo` selects the token and the HTTP method its access: a GET carries a
/// read token and nothing else can.
pub struct GithubApi {
    transport: Arc<Transport>,
    minter: Arc<Minter>,
    identities: Arc<Identities>,
}

/// An answer that arrived. Rate limits and 5xx are errors instead.
pub(crate) enum Got {
    Ok(Page),
    /// 304: the `If-None-Match` ETag still holds.
    NotModified,
    Refused {
        status: StatusCode,
        message: String,
    },
}

pub(crate) struct Page {
    pub(crate) body: Value,
    pub(crate) etag: Option<String>,
    /// The `Link` header's URLs by `rel`, only those on the API base.
    pub(crate) links: BTreeMap<String, String>,
}

/// Retrying cannot help until the operator acts: GitHub refused the App's
/// key, or the App cannot see what the config names.
#[derive(Debug)]
pub(crate) struct Fatal(pub(crate) String);

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Fatal {}

pub(crate) fn is_fatal(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.downcast_ref::<Fatal>().is_some())
}

pub(crate) fn retry_after(err: &anyhow::Error) -> Option<Duration> {
    err.chain()
        .find_map(|e| e.downcast_ref::<RetryAfter>())
        .map(|after| after.0)
}

/// Whether GitHub's refusal says the installation is suspended.
fn suspended(message: &str) -> bool {
    message.to_ascii_lowercase().contains("suspended")
}

impl GithubApi {
    pub fn new(minter: Arc<Minter>, identities: Arc<Identities>) -> Result<Self> {
        Ok(Self {
            transport: minter.transport(),
            minter,
            identities,
        })
    }

    pub(crate) fn url(&self, path: &str) -> String {
        self.transport.url(path)
    }

    /// Whether a token may be minted for `repo`: it is in `forge.repos`.
    pub(crate) fn allows(&self, repo: &RepoName) -> bool {
        self.minter.allows(repo)
    }

    /// GET `url` on behalf of `repo`, conditional on `etag` when given.
    pub(crate) async fn get(&self, repo: &RepoName, url: &str, etag: Option<&str>) -> Result<Got> {
        self.call(repo, Method::GET, url, etag, None).await
    }

    /// POST, PATCH, PUT or DELETE on behalf of `repo`: always a write token.
    pub(crate) async fn send(
        &self,
        repo: &RepoName,
        method: Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Got> {
        self.call(repo, method, url, None, body).await
    }

    /// A GET of something public, such as a user, with a read token of a
    /// configured repository the App can read now.
    pub(crate) async fn get_public(&self, path: &str) -> Result<Got> {
        let repo = self.minter.readable_repo().await?;
        self.get(&repo, &self.url(path), None).await
    }

    /// The agent's own account: the App's bot.
    pub(crate) async fn bot(&self) -> Result<Bot> {
        self.minter.bot().await.cloned()
    }

    pub(crate) fn identities(&self) -> &Arc<Identities> {
        &self.identities
    }

    pub(crate) fn minter(&self) -> &Arc<Minter> {
        &self.minter
    }

    /// One call with a token narrowed to `repo`. A 401 means the token was
    /// revoked or its installation changed: the cached one is dropped and
    /// the call is made once more with a fresh mint. A suspended
    /// installation is a wait, not a failure.
    async fn call(
        &self,
        repo: &RepoName,
        method: Method,
        url: &str,
        etag: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Got> {
        let access = Access::for_method(&method);
        let mut retried = false;
        loop {
            let token = self.minter.token(repo, access).await?;
            let got = self
                .transport
                .send(method.clone(), url, token.bearer(), etag, body)
                .await?;
            match got {
                Got::Refused { status, message } if status == StatusCode::UNAUTHORIZED => {
                    self.minter.forget(&token);
                    if !retried {
                        retried = true;
                        continue;
                    }
                    bail!(
                        "github {method} {url}: GitHub refused a fresh installation token (HTTP 401: {message})"
                    );
                }
                Got::Refused { status, message }
                    if status == StatusCode::FORBIDDEN && suspended(&message) =>
                {
                    self.minter.forget(&token);
                    return Err(self.minter.suspended(repo));
                }
                other => return Ok(other),
            }
        }
    }
}

/// HTTP to one API base with a bearer the caller chooses: an installation
/// token for the REST calls, the App's JWT for the Minter's. Holds the one
/// process-wide rate-limit pause.
pub(crate) struct Transport {
    base: String,
    http: reqwest::Client,
    /// No request leaves before this instant.
    hold: Mutex<Option<Instant>>,
}

impl Transport {
    /// `base` is the REST root, [`DEFAULT_API_BASE`] unless a test points it
    /// at a fake.
    pub(crate) fn new(base: &str) -> Result<Self> {
        let mut base = base.trim().to_owned();
        if !base.ends_with('/') {
            base.push('/');
        }
        reqwest::Url::parse(&base).with_context(|| format!("invalid GitHub API base {base}"))?;
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("solos-swarm/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            base,
            http,
            hold: Mutex::new(None),
        })
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn held(&self) -> Option<Duration> {
        let hold = self.hold.lock().unwrap_or_else(PoisonError::into_inner);
        hold.and_then(|until| until.checked_duration_since(Instant::now()))
            .filter(|wait| !wait.is_zero())
    }

    fn hold_for(&self, wait: Duration) {
        let until = Instant::now() + wait;
        let mut hold = self.hold.lock().unwrap_or_else(PoisonError::into_inner);
        *hold = Some(hold.map_or(until, |held| held.max(until)));
    }

    /// `method` `url` with `bearer`, conditional on `etag` when given. A 401
    /// is a [`Got::Refused`] like any other answer: what it means depends on
    /// what the bearer was.
    pub(crate) async fn send(
        &self,
        method: Method,
        url: &str,
        bearer: &str,
        etag: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Got> {
        // Every URL comes from the base or a Link header checked against it,
        // so a bearer never leaves for another host.
        ensure!(url.starts_with(&self.base), "a GitHub URL off the API base");
        let path = url.strip_prefix(&self.base).unwrap_or(url);
        let what = format!("github {method} {}", path.split('?').next().unwrap_or(path));
        if let Some(wait) = self.held() {
            return Err(limited(wait, &what));
        }
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(bearer)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION);
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("{what} failed"))?;
        let status = response.status();
        let headers = response.headers().clone();
        let asked = asked_wait(&headers);
        if status == StatusCode::NOT_MODIFIED {
            if let Some(wait) = asked {
                self.hold_for(wait);
            }
            return Ok(Got::NotModified);
        }
        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("{what}: the answer was cut off"))?;
        if status.is_success() {
            if let Some(wait) = asked {
                self.hold_for(wait);
            }
            let body = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("{what}: the answer is not JSON"))?
            };
            let etag = headers
                .get(ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let links = links(&headers, &self.base);
            return Ok(Got::Ok(Page { body, etag, links }));
        }
        let message = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|body| body["message"].as_str().map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).chars().take(200).collect());
        let rate_limited = status == StatusCode::TOO_MANY_REQUESTS
            || (status == StatusCode::FORBIDDEN
                && (asked.is_some() || message.to_ascii_lowercase().contains("rate limit")));
        if rate_limited {
            let wait = asked.unwrap_or(Duration::from_secs(SECONDARY_WAIT));
            self.hold_for(wait);
            return Err(limited(wait, &what));
        }
        if status.is_server_error() {
            bail!("{what}: HTTP {status}");
        }
        Ok(Got::Refused { status, message })
    }
}

fn limited(wait: Duration, what: &str) -> anyhow::Error {
    anyhow::Error::new(RetryAfter(wait)).context(what.to_owned())
}

/// What GitHub asks a client to wait: `Retry-After`, or until
/// `X-RateLimit-Reset` once nothing remains.
fn asked_wait(headers: &HeaderMap) -> Option<Duration> {
    let number = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
    };
    let seconds = if let Some(seconds) = number(RETRY_AFTER.as_str()) {
        seconds
    } else if number("x-ratelimit-remaining") == Some(0) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        number("x-ratelimit-reset")
            .map_or(SECONDARY_WAIT, |reset| reset.saturating_sub(now))
            .max(1)
    } else {
        return None;
    };
    Some(Duration::from_secs(seconds.min(MAX_RETRY_AFTER)))
}

/// `<url>; rel="next", <url>; rel="last"`. URLs off `base` are dropped.
fn links(headers: &HeaderMap, base: &str) -> BTreeMap<String, String> {
    let Some(value) = headers.get(LINK).and_then(|value| value.to_str().ok()) else {
        return BTreeMap::new();
    };
    value
        .split('<')
        .skip(1)
        .filter_map(|part| {
            let (url, params) = part.split_once('>')?;
            let rel = params
                .split(';')
                .find_map(|param| param.trim().strip_prefix("rel="))?
                .trim_end_matches([',', ' '])
                .trim_matches('"');
            url.starts_with(base)
                .then(|| (rel.to_owned(), url.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{links, valid_repo};
    use reqwest::header::{HeaderMap, HeaderValue, LINK};

    #[test]
    fn repository_names_cannot_reach_other_paths() {
        for good in ["acme/widgets", "a-b/c.d_e", "o/.github"] {
            assert!(valid_repo(good), "{good}");
        }
        for bad in [
            "acme",
            "acme/",
            "/widgets",
            "acme/widgets/issues",
            "../widgets",
            "acme/..",
            "acme/wid gets",
            "acme/widgets?x=1",
        ] {
            assert!(!valid_repo(bad), "{bad}");
        }
    }

    #[test]
    fn link_headers_keep_only_urls_on_the_api_base() {
        let mut headers = HeaderMap::new();
        headers.insert(
            LINK,
            HeaderValue::from_static(
                "<https://api.github.com/repositories/1/issues?labels=a,b&page=2>; rel=\"next\", \
                 <https://evil.example/x?page=9>; rel=\"last\"",
            ),
        );
        let found = links(&headers, "https://api.github.com/");
        assert_eq!(
            found.get("next").map(String::as_str),
            Some("https://api.github.com/repositories/1/issues?labels=a,b&page=2")
        );
        assert!(!found.contains_key("last"));
    }
}
