//! GitHub: a Source that polls the REST API for the configured repositories,
//! and a Channel that comments on issues and pull requests. Triggers name
//! source [`SOURCE`]; an issue or pull request is the Thread
//! `github:<owner>/<repo>#<number>`.

mod channel;
mod source;

pub use channel::GithubChannel;
pub use source::{GithubSource, OVERLAP};

use super::ports::RetryAfter;
use anyhow::{Context, Result, bail, ensure};
use reqwest::header::{ACCEPT, ETAG, HeaderMap, IF_NONE_MATCH, LINK, RETRY_AFTER};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde_json::Value;
use solos_agent_core::model::{ChannelRef, MessageId, ThreadKey};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::OnceCell;
use zeroize::Zeroizing;

/// The Source name GitHub Triggers and replies carry.
pub const SOURCE: &str = "github";
pub const DEFAULT_API_BASE: &str = "https://api.github.com/";
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

/// A GitHub token, held in memory only. Its `Debug` never shows it.
#[derive(Clone)]
pub struct Token(Arc<Zeroizing<String>>);

impl Token {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(Arc::new(Zeroizing::new(secret.into())))
    }

    /// Read the file once and delete it, so the token lives only in this
    /// process. It must be a personal access token, because the agent learns
    /// its own account from `GET /user`.
    pub fn take(path: &Path) -> Result<Self> {
        let read = std::fs::read_to_string(path);
        let _ = std::fs::remove_file(path);
        let raw =
            Zeroizing::new(read.with_context(|| {
                format!("cannot read the GitHub token file {}", path.display())
            })?);
        let token = raw.trim();
        ensure!(
            !token.starts_with("ghs_"),
            "the GitHub token in {} is an app installation token; use a personal access token \
             of the agent's own account",
            path.display()
        );
        let legacy = token.len() == 40 && token.bytes().all(|b| b.is_ascii_hexdigit());
        let personal = ["ghp_", "github_pat_", "gho_", "ghu_"]
            .iter()
            .any(|prefix| token.starts_with(prefix))
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_');
        ensure!(
            legacy || personal,
            "the GitHub token in {} is not a personal access token (ghp_ or github_pat_)",
            path.display()
        );
        Ok(Self::new(token))
    }

    fn secret(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

/// The REST API with one token, shared by the Source and the Channel so a
/// rate limit one meets holds the other back too.
pub struct GithubApi {
    base: String,
    http: reqwest::Client,
    token: Token,
    /// No request leaves before this instant.
    hold: Mutex<Option<Instant>>,
    login: OnceCell<String>,
}

/// An answer that arrived. Rate limits, 401 and 5xx are errors instead.
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

/// Retrying cannot help until the operator acts: GitHub refused the token,
/// or the token cannot see what the config names.
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

impl GithubApi {
    /// `base` is the REST root, [`DEFAULT_API_BASE`] unless a test points it
    /// at a fake.
    pub fn new(base: &str, token: Token) -> Result<Self> {
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
            token,
            hold: Mutex::new(None),
            login: OnceCell::new(),
        })
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// GET `url`, conditional on `etag` when given.
    pub(crate) async fn get(&self, url: &str, etag: Option<&str>) -> Result<Got> {
        let mut request = self.request(Method::GET, url)?;
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        self.send("GET", url, request).await
    }

    pub(crate) async fn post(&self, url: &str, body: &Value) -> Result<Got> {
        let request = self.request(Method::POST, url)?.json(body);
        self.send("POST", url, request).await
    }

    /// The agent's own account.
    pub(crate) async fn login(&self) -> Result<String> {
        let login = self
            .login
            .get_or_try_init(|| async {
                match self.get(&self.url("user"), None).await? {
                    Got::Ok(page) => page.body["login"]
                        .as_str()
                        .map(str::to_owned)
                        .context("github GET user named no login"),
                    Got::Refused { status, message } => Err(Fatal(format!(
                        "github GET user: HTTP {status}: {message}; the token must be a personal \
                         access token"
                    ))
                    .into()),
                    Got::NotModified => bail!("github GET user: an unasked 304"),
                }
            })
            .await?;
        Ok(login.clone())
    }

    fn request(&self, method: Method, url: &str) -> Result<RequestBuilder> {
        // Every URL comes from the base or a Link header checked against it,
        // so the token never leaves for another host.
        ensure!(url.starts_with(&self.base), "a GitHub URL off the API base");
        Ok(self
            .http
            .request(method, url)
            .bearer_auth(self.token.secret())
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION))
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

    async fn send(&self, method: &str, url: &str, request: RequestBuilder) -> Result<Got> {
        let path = url.strip_prefix(&self.base).unwrap_or(url);
        let what = format!("github {method} {}", path.split('?').next().unwrap_or(path));
        if let Some(wait) = self.held() {
            return Err(limited(wait, &what));
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
        if status == StatusCode::UNAUTHORIZED {
            return Err(Fatal(format!("{what}: GitHub refused the token (HTTP 401)")).into());
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
