//! Vercel Blob's REST interface, as `put` in `@vercel/blob` speaks it.
//!
//! Pinned from the package's own source, `@vercel/blob@2.8.2` (npm, unpacked
//! in a scratch directory; `dist/chunk-XKVPKJPO.js`, `createPutMethod`,
//! `requestApi`, `createPutHeaders`, `resolveBlobAuth`), and from
//! <https://vercel.com/docs/vercel-blob/private-storage>:
//!
//! ```text
//! PUT https://vercel.com/api/blob/?pathname=<pathname>      (query as URLSearchParams writes it)
//! authorization:           Bearer <BLOB_READ_WRITE_TOKEN>    vercel_blob_rw_<storeId>_<secret>
//! x-api-version:           13                                BLOB_API_VERSION
//! x-vercel-blob-store-id:  <storeId>                         the token's 4th `_` segment
//! x-vercel-blob-access:    private                           required by put() since 2.x
//! x-add-random-suffix:     0                                 the pathname is the address
//! x-allow-overwrite:       1                                 without it a second put is refused
//! x-content-type:          application/json                  the stored blob's type
//! x-cache-control-max-age: 60                                the least it takes; default is a month
//! x-api-blob-request-id / x-api-blob-request-attempt         diagnostics the SDK adds
//! body: the bytes, no content-type of its own
//! ```
//!
//! The answer is JSON, `{url, downloadUrl, pathname, contentType,
//! contentDisposition, etag}`; an error is `{"error": {"code", "message"}}`.
//! The SDK retries ten times on network errors and on `unknown_error`,
//! `service_unavailable` and `internal_server_error`, and stops at once on
//! the rest, `forbidden` (HTTP 403) among them. This client does the same
//! with a short exponential backoff, and also waits out a 429's
//! `retry-after`. A private blob is read back with the same token at
//! `https://<storeId>.private.blob.vercel-storage.com/<pathname>`; the
//! dashboard does that through `get(pathname, {access: "private"})`.
//!
//! `tests/agent_publish.rs` holds this to a fake server, header by header.

use anyhow::{Context, Result, ensure};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;
use zeroize::Zeroizing;

/// `BLOB_API_VERSION` in `@vercel/blob@2.8.2`.
pub const API_VERSION: &str = "13";
/// The cache lifetime a blob is stored with: the shortest the API takes.
/// Overwritten blobs are served stale for up to this long by the CDN, which
/// the dashboard avoids anyway by reading with `useCache: false`.
pub const CACHE_SECONDS: &str = "60";
/// `MAXIMUM_PATHNAME_LENGTH` in the SDK.
pub const MAX_PATHNAME_CHARS: usize = 950;

const TOKEN_PREFIX: &str = "vercel_blob_rw_";
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// A Blob store's read-write token, which never prints.
pub struct BlobToken {
    secret: Zeroizing<String>,
    store_id: String,
}

impl BlobToken {
    /// Read the file once and delete it, as every token file is read, so the
    /// token lives only in this process.
    pub fn take(path: &Path) -> Result<Self> {
        let read = std::fs::read_to_string(path);
        let _ = std::fs::remove_file(path);
        let raw = Zeroizing::new(
            read.with_context(|| format!("cannot read the Blob token file {}", path.display()))?,
        );
        Self::parse(raw.trim()).with_context(|| format!("the Blob token in {}", path.display()))
    }

    /// `vercel_blob_rw_<storeId>_<secret>`: the shape the SDK takes the
    /// store id from.
    pub fn parse(token: &str) -> Result<Self> {
        ensure!(
            token.starts_with(TOKEN_PREFIX),
            "is not a read-write token: it must start with {TOKEN_PREFIX}"
        );
        let mut parts = token.split('_').skip(3);
        let store_id = parts.next().unwrap_or_default();
        ensure!(
            !store_id.is_empty() && store_id.bytes().all(|b| b.is_ascii_alphanumeric()),
            "has no store id after {TOKEN_PREFIX}"
        );
        ensure!(
            parts.next().is_some_and(|secret| !secret.is_empty()),
            "has no secret after its store id"
        );
        ensure!(
            token.bytes().all(|b| b.is_ascii_graphic()),
            "has characters a header cannot carry"
        );
        Ok(Self {
            secret: Zeroizing::new(token.to_owned()),
            store_id: store_id.to_owned(),
        })
    }

    /// The store the token writes to.
    pub fn store_id(&self) -> &str {
        &self.store_id
    }
}

impl std::fmt::Debug for BlobToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BlobToken(store {}, ..)", self.store_id)
    }
}

/// How a failed request is retried: `attempts` tries in all, waiting `base`,
/// then twice that, and so on up to `max`.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    pub attempts: u32,
    pub base: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            attempts: 5,
            base: Duration::from_millis(500),
            max: Duration::from_secs(10),
        }
    }
}

impl Backoff {
    fn wait(&self, attempt: u32) -> Duration {
        self.base
            .checked_mul(2u32.saturating_pow(attempt))
            .map_or(self.max, |wait| wait.min(self.max))
    }
}

/// Why an upload did not happen.
#[derive(Debug)]
pub enum PutError {
    /// The API refused the token (401, 403). Retrying cannot help until the
    /// operator replaces it.
    Unauthorized(String),
    /// Out of attempts on a network error, a 5xx or a 429: the next look
    /// tries again.
    Transient(String),
    /// Another 4xx: this blob will not be taken as it is.
    Rejected(String),
}

impl std::fmt::Display for PutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized(why) | Self::Transient(why) | Self::Rejected(why) => {
                f.write_str(why)
            }
        }
    }
}

impl std::error::Error for PutError {}

/// What a successful put answers with.
#[derive(Clone, Debug, Deserialize)]
pub struct Put {
    pub url: String,
    pub pathname: String,
    #[serde(default)]
    pub etag: Option<String>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<ErrorDetail>,
}

#[derive(Deserialize)]
struct ErrorDetail {
    code: Option<String>,
    message: Option<String>,
}

/// Puts blobs into one private store.
pub struct BlobClient {
    http: reqwest::Client,
    api_url: String,
    token: BlobToken,
    backoff: Backoff,
}

impl BlobClient {
    /// `api_url` has no trailing slash.
    pub fn new(api_url: &str, token: BlobToken, backoff: Backoff) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            // A redirect would carry the token to wherever it points.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!(
                "solos-swarm/",
                env!("CARGO_PKG_VERSION"),
                " agent-publish"
            ))
            .build()
            .context("cannot build the HTTP client")?;
        Ok(Self {
            http,
            api_url: api_url.trim_end_matches('/').to_owned(),
            token,
            backoff,
        })
    }

    pub fn store_id(&self) -> &str {
        self.token.store_id()
    }

    /// Write `body` at `pathname`, replacing what is there.
    pub async fn put(
        &self,
        pathname: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Put, PutError> {
        check_pathname(pathname).map_err(|why| PutError::Rejected(format!("{pathname}: {why}")))?;
        let url = reqwest::Url::parse_with_params(
            &format!("{}/", self.api_url),
            [("pathname", pathname)],
        )
        .map_err(|err| PutError::Rejected(format!("{pathname}: {err}")))?;
        let request_id = format!(
            "{}:{}:{:x}",
            self.token.store_id,
            crate::broker::now_ms(),
            rand::random::<u32>()
        );
        let mut last = String::new();
        for attempt in 0..self.backoff.attempts.max(1) {
            if attempt > 0 {
                tokio::time::sleep(self.backoff.wait(attempt - 1)).await;
            }
            let headers = self
                .headers(content_type, &request_id, attempt)
                .map_err(|err| PutError::Rejected(format!("{pathname}: {err:#}")))?;
            let sent = self
                .http
                .put(url.clone())
                .headers(headers)
                .body(body.clone())
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(err) => {
                    last = format!("{pathname}: the request failed: {}", describe(err));
                    continue;
                }
            };
            let status = response.status();
            if status.is_success() {
                return response
                    .json::<Put>()
                    .await
                    .map_err(|err| PutError::Transient(format!("{pathname}: bad answer: {err}")));
            }
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(|seconds| Duration::from_secs(seconds).min(MAX_RETRY_AFTER));
            let detail = error_detail(response).await;
            let why = format!("{pathname}: Vercel Blob answered {status}{detail}");
            match status.as_u16() {
                401 | 403 => return Err(PutError::Unauthorized(why)),
                408 | 425 | 429 | 500..=599 => {
                    last = why;
                    if let Some(wait) = retry_after
                        && attempt + 1 < self.backoff.attempts
                    {
                        tokio::time::sleep(wait).await;
                    }
                }
                _ => return Err(PutError::Rejected(why)),
            }
        }
        Err(PutError::Transient(last))
    }

    fn headers(&self, content_type: &str, request_id: &str, attempt: u32) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        let mut put = |name: &'static str, value: &str| -> Result<()> {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_str(value).with_context(|| format!("header {name}"))?,
            );
            Ok(())
        };
        put("x-api-blob-request-id", request_id)?;
        put("x-vercel-blob-store-id", &self.token.store_id)?;
        put("x-api-blob-request-attempt", &attempt.to_string())?;
        put("x-api-version", API_VERSION)?;
        put("x-vercel-blob-access", "private")?;
        put("x-add-random-suffix", "0")?;
        put("x-allow-overwrite", "1")?;
        put("x-content-type", content_type)?;
        put("x-cache-control-max-age", CACHE_SECONDS)?;
        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {}", self.token.secret.as_str()))
                .context("the token as a header")?;
        authorization.set_sensitive(true);
        headers.insert(AUTHORIZATION, authorization);
        Ok(headers)
    }
}

/// `pathname`'s rules in the SDK: not empty, at most 950 characters, and no
/// `//`. A leading `/` would name a different blob than the dashboard reads.
fn check_pathname(pathname: &str) -> Result<()> {
    ensure!(!pathname.is_empty(), "the pathname is empty");
    ensure!(
        pathname.chars().count() <= MAX_PATHNAME_CHARS,
        "the pathname is longer than {MAX_PATHNAME_CHARS} characters"
    );
    ensure!(!pathname.contains("//"), "the pathname contains //");
    ensure!(!pathname.starts_with('/'), "the pathname starts with /");
    Ok(())
}

/// ` (code: message)` from an error answer, or nothing.
async fn error_detail(response: reqwest::Response) -> String {
    let Ok(text) = response.text().await else {
        return String::new();
    };
    let Ok(ErrorBody {
        error: Some(ErrorDetail { code, message }),
    }) = serde_json::from_str::<ErrorBody>(&text)
    else {
        return String::new();
    };
    let said: Vec<String> = [code, message].into_iter().flatten().collect();
    format!(" ({})", said.join(": "))
}

/// A transport error without its URL, which holds the query.
fn describe(err: reqwest::Error) -> String {
    err.without_url().to_string()
}

#[cfg(test)]
mod tests {
    use super::{Backoff, BlobToken, check_pathname};
    use std::time::Duration;

    #[test]
    fn a_token_names_its_store_and_never_prints() {
        let token = BlobToken::parse("vercel_blob_rw_AbC123_secretpart").unwrap();
        assert_eq!(token.store_id(), "AbC123");
        let shown = format!("{token:?}");
        assert!(!shown.contains("secretpart"), "{shown}");
        for bad in [
            "",
            "vercel_blob_ro_AbC123_secret",
            "vercel_blob_rw__secret",
            "vercel_blob_rw_AbC123",
            "vercel_blob_rw_AbC123_",
            "vercel_blob_rw_AbC123_sec ret",
            "vercel_blob_rw_Ab-C_secret",
            "xoxb-1234",
        ] {
            assert!(BlobToken::parse(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_pathname_follows_the_sdks_rules() {
        assert!(check_pathname("tickets/tk_0123.json").is_ok());
        assert!(check_pathname("").is_err());
        assert!(check_pathname("a//b").is_err());
        assert!(check_pathname("/a").is_err());
        assert!(check_pathname(&"a".repeat(951)).is_err());
        assert!(check_pathname(&"a".repeat(950)).is_ok());
    }

    #[test]
    fn the_wait_doubles_up_to_its_cap() {
        let backoff = Backoff {
            attempts: 9,
            base: Duration::from_millis(500),
            max: Duration::from_secs(4),
        };
        let waits: Vec<_> = (0..5).map(|n| backoff.wait(n).as_millis()).collect();
        assert_eq!(waits, [500, 1_000, 2_000, 4_000, 4_000]);
        assert_eq!(backoff.wait(200), Duration::from_secs(4));
    }
}
