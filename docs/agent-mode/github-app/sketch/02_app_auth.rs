//! Lives at: src/agent/github/app.rs (new), src/agent/github/identities.rs
//! (new) and src/agent/github/mod.rs (`GithubApi` rewired; `Token`,
//! `Token::take` and the `ghs_` refusal are deleted; the PAT prefix checks go
//! with them).
//!
//! Sketch only: not compiled. Candidate B's auth, amended by the synthesis:
//! the refresh margin is 15 minutes (longer than git's 10-minute timeout), a
//! suspended or uninstalled App is a wait and not a fatal preflight, and
//! candidate A's `Identities` pins every listed login to its account id.
//!
//! One way to authenticate: the App. The private key is held by exactly one
//! object, `Minter`. Everything else holds an `InstallToken`, which is a
//! bearer for ONE repository at ONE access level, for under an hour, in
//! memory only.
//!
//! Where the key lives (decision 1):
//!
//!   ~/agent/tokens/github-app.pem   mode 0600, the durable copy. It is a
//!   token file like `discord-token`: the unit's ExecStartPre copies it to the
//!   tmpfs runtime dir before every start, `serve` reads that copy once and
//!   deletes it (`AppKey::take`), and the process keeps the parsed key for its
//!   life. No sandbox binds `~/agent/tokens` or the runtime dir. Not in the
//!   vault: any vault `Key` is reachable by a worker through `fetch_url`'s
//!   `key`, and this key mints tokens. Not sealed under a second key file: the
//!   second key would sit on the same disk beside it and add nothing. The
//!   blast radius is bounded by the App's permissions and its installation
//!   (chosen repositories, no `workflows`, no `administration`), and the key
//!   rotates without downtime (GitHub allows several keys per App at once).

use crate::agent::ports::{BoxFuture, RetryAfter};
use anyhow::Result;
use serde_json::Value;
use solos_agent_core::change::RepoName;
use solos_agent_core::model::Actor;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use zeroize::Zeroizing;

/// Mint again when less than this remains on a cached token. Longer than
/// `forge::GIT_WAIT` (600 s), so a git fetch or push that starts with a token
/// always finishes before the token expires. (B had 5 minutes; the judge
/// found it shorter than git's own timeout.)
pub const REFRESH_MARGIN: Duration = Duration::from_secs(15 * 60);
const _: () = assert!(REFRESH_MARGIN.as_secs() > 600, "a push must never outlive its token");

/// How long every call for a repository waits while its installation is
/// suspended, removed or missing (the owner's kill switch on GitHub). A
/// wait, not a fatal error: unsuspending resumes work without a restart, and
/// nothing crash-loops. Graft from candidate A.
pub const SUSPENDED_WAIT: Duration = Duration::from_secs(300);

/// GitHub refuses an App JWT that lives longer than ten minutes.
const JWT_LIFE: Duration = Duration::from_secs(9 * 60);
/// Clock skew allowance: the JWT is issued this far in the past.
const JWT_SKEW: Duration = Duration::from_secs(60);

/// The App's numeric id. `iss` of every JWT.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppId(pub u64);

/// What an installation token may do. Chosen by the *call site's HTTP method*
/// (`Access::for_method`), never by a caller's argument and never by a model:
/// a GET cannot carry a write token, and no code path asks for `Write`
/// except the host's own push, PR, comment and merge calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Access {
    Read,
    Write,
}

impl Access {
    pub fn for_method(_method: &reqwest::Method) -> Access {
        todo!("GET and HEAD -> Read; everything else -> Write")
    }

    /// The narrowed permissions object sent with the mint request. A token
    /// can only be narrower than the App's grant, never broader.
    ///
    /// Read:  contents, pull_requests, issues, checks, actions, statuses: read; metadata: read
    /// Write: contents, pull_requests, issues: write; checks, actions, statuses: read; metadata: read
    ///
    /// `workflows` and `administration` appear in neither, and the App does
    /// not hold them, so a push that edits `.github/workflows/**` is refused
    /// by GitHub itself even if the host's own protected-path check were
    /// bypassed, and the App can never change branch protection.
    fn permissions(self) -> Value {
        todo!()
    }

    /// What the App must hold: `permissions(Write)`. The one source of the
    /// App's permission list. A test pins
    /// `docs/agent-mode/github-app/manifest.json`'s `default_permissions` to
    /// it (PR 1), and `agent github init` builds its manifest from it (PR 6),
    /// so the manual list, the manifest and `check` cannot disagree.
    pub fn app_permissions() -> Value {
        todo!()
    }
}

/// The App's private key, parsed. No `Clone`, no `Debug` content, and the PEM
/// bytes are zeroized once parsed. GitHub issues PKCS#1 ("BEGIN RSA PRIVATE
/// KEY"); `from_rsa_pem` accepts it and PKCS#8.
pub struct AppKey {
    key: jsonwebtoken::EncodingKey,
}

impl AppKey {
    /// For `serve`: read the file once and delete it, as every token file is.
    /// Refuses a symlink, a file open to group or others, and a PEM that does
    /// not parse as RSA. Also refused: a path under any directory a sandbox
    /// can read (`Exposure::view().hides(path)`), checked by the caller that
    /// has the `Exposure`. A refusal here fails `serve` at start, as an
    /// unreadable token file does today: only the owner can fix it.
    pub fn take(_path: &Path) -> Result<Self> {
        todo!()
    }

    /// For `agent github check` and tests: the same checks, no deletion.
    pub fn read(_path: &Path) -> Result<Self> {
        todo!()
    }

    /// RS256, `iss = app`, `iat = now - JWT_SKEW`, `exp = now + JWT_LIFE`.
    /// Crate-private: the JWT never leaves `Minter` and the App-level REST
    /// calls that need it.
    fn sign(&self, _app: AppId, _now: SystemTime) -> Result<Zeroizing<String>> {
        todo!()
    }
}

/// A bearer for one repository at one access level. `Debug` redacts. Lives
/// in memory only; never logged, never written, never put in argv.
#[derive(Clone)]
pub struct InstallToken {
    secret: Arc<Zeroizing<String>>,
    pub repo: RepoName,
    pub access: Access,
    expires: SystemTime,
}

impl InstallToken {
    pub(crate) fn bearer(&self) -> &str {
        &self.secret
    }

    /// `Authorization: Basic base64("x-access-token:<token>")`, the value git
    /// puts in `http.<git_base>.extraheader`. Handed to the *mirror's* git
    /// environment only (`GIT_CONFIG_COUNT/KEY_0/VALUE_0`), never argv and
    /// never a sandbox, per `forge.rs`'s existing rule.
    pub(crate) fn git_header(&self) -> Zeroizing<String> {
        todo!()
    }
}

impl std::fmt::Debug for InstallToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InstallToken({} {:?}, ..)", self.repo, self.access)
    }
}

/// The App's own identity on GitHub, learned once from `GET /app` and
/// `GET /users/<slug>[bot]`. The bot's login is the Source's "me" and the
/// commits' author: one derivation, nothing configured twice.
#[derive(Clone, Debug)]
pub struct Bot {
    pub slug: String,
    /// `<slug>[bot]`, compared case-insensitively.
    pub login: String,
    pub id: u64,
    /// `<id>+<slug>[bot]@users.noreply.github.com`.
    pub email: String,
}

impl Bot {
    /// A mention addresses the agent when the text holds `@<slug>` or
    /// `@<slug>[bot]` as a word, case-insensitively. People type the first;
    /// GitHub's autocomplete writes the second.
    pub fn mentioned_in(&self, _text: &str) -> bool {
        todo!()
    }
    /// What the clone is configured with and what `Forge::admit` requires of
    /// every commit's author and committer: `<login> <email>`.
    pub fn commit_identity(&self) -> (String, String) {
        (self.login.clone(), self.email.clone())
    }
}

/// Time as a function, so a test moves it and a mint near expiry is provable
/// without sleeping.
pub type Now = Arc<dyn Fn() -> SystemTime + Send + Sync>;

pub struct MinterConfig {
    pub app: AppId,
    pub api_base: String,
    /// `github.writes: false`: `token(.., Write)` is refused here, so a
    /// read-only agent is read-only *physically*, not by policy.
    pub writes: bool,
}

/// Mints, caches, narrows and refreshes installation tokens.
///
///   token(repo, Read|Write)
///     cache hit with > REFRESH_MARGIN left -> reuse (one cached token per
///                                             (repo, access); at most two per repo)
///     else, singleflight per (repo, access) -> JWT -> GET /repos/{repo}/installation
///                                              (cached per repo; dropped on a 404 so a
///                                              reinstall's new id is found) ->
///        POST /app/installations/{id}/access_tokens
///             {"repositories": ["<name>"], "permissions": <Access::permissions>}
///
///   the installation is suspended (403 on the mint), the App is not
///   installed on the repository (404), or the repository left the
///   installation (422 naming it)      -> Err(RetryAfter(SUSPENDED_WAIT)),
///                                        logged once per change of state
///
/// Narrowing means a bug in one code path cannot touch another repository,
/// and the mirror-refresh and observe paths never hold a write token.
pub struct Minter {
    _private: (),
}

impl Minter {
    pub fn new(_config: MinterConfig, _key: AppKey, _http: reqwest::Client, _now: Now) -> Self {
        todo!()
    }

    /// At `serve` start and in `agent github check`: for each configured
    /// repository, whether the App is installed on it, whether the
    /// installation is suspended, and which of `Access::app_permissions` it
    /// lacks. `serve` logs one line per repository and never stops on it: a
    /// missing or suspended installation is the same wait as a later
    /// suspension, so an owner who suspends the App and then restarts the box
    /// does not get a crash loop. `check` exits nonzero on any miss. Only a
    /// key GitHub rejects (401 on `GET /app`) stops the github Source's loop
    /// (`Fatal`, as a 401 does today); the rest of the agent keeps running.
    pub async fn preflight(&self, _repos: &[RepoName]) -> Result<Vec<Preflight>> {
        todo!()
    }

    pub async fn bot(&self) -> Result<&Bot> {
        todo!()
    }

    pub async fn token(&self, _repo: &RepoName, _access: Access) -> Result<InstallToken> {
        todo!()
    }

    /// After a 401 on a cached token (revoked, or the installation changed):
    /// drop it so the retry mints again. One retry, then the error stands.
    pub fn forget(&self, _token: &InstallToken) {}

    /// App-level REST (`/app`, `/repos/{r}/installation`): authenticated with
    /// a fresh JWT, which never leaves this module.
    pub(crate) async fn app_get(&self, _path: &str) -> Result<Value> {
        todo!()
    }
}

pub struct Preflight {
    pub repo: RepoName,
    pub state: InstallState,
}

pub enum InstallState {
    Installed {
        installation: u64,
        /// Permissions the installation grants, as GitHub reports them.
        permissions: Vec<(String, String)>,
        /// From `Access::app_permissions`; also any of `workflows`,
        /// `administration` that is present (refused by `check`).
        missing: Vec<String>,
        extra: Vec<String>,
    },
    Suspended,
    NotInstalled { install_url: String },
}

// ---------------------------------------------------------------------------
// Identities (identities.rs). Graft from candidate A (A/sketch/01_app.rs).
// ---------------------------------------------------------------------------

/// `<state>/github-identities.json`: listed logins pinned to account ids.
pub const IDENTITIES_FILE: &str = "github-identities.json";

/// GitHub logins the Policy lists (`policy.people`, `approvers`, `bots`),
/// pinned to the numeric account id each had when the agent first looked it
/// up. A login can be renamed away and claimed by another account; an
/// approval now merges code and a label or mention spends budget, so a
/// listed login with another id must not speak with the first account's
/// authority.
///
/// Rule: a GitHub user object whose login is listed and whose id equals the
/// pin is `Actor { id: login }`, exactly as today. One whose id differs is
/// `Actor { id: "<login>#<id>" }`, which no Policy lists, and the mismatch is
/// logged once. A listed login with no pin yet (its lookup failed) is
/// treated the same way until the pin exists, and the operator is told once.
/// Unlisted logins pass through unpinned (they are not admitted anyway).
///
/// Pins are made at start by `GET /users/<login>` for every listed login
/// without one, and again when the config gains a login. They are written
/// with `sealed::private_file` (0600, all or nothing) and never change by
/// themselves: re-listing a person who recreated their account means
/// deleting their pin (`agent github check` prints the file's path).
pub struct Identities {
    path: PathBuf,
    /// Lowercased `github:` logins from `policy.people`, `approvers`, `bots`.
    listed: BTreeSet<String>,
    pins: std::sync::Mutex<BTreeMap<String, u64>>,
}

impl Identities {
    pub fn load(_path: PathBuf, _listed: BTreeSet<String>) -> Result<Self> {
        todo!()
    }

    /// Look up and pin every listed login without a pin. A failure leaves
    /// that login unpinned (and so not admitted) and is retried each
    /// reconcile pass.
    pub async fn pin_missing(&self, _api: &GithubApi) -> Result<()> {
        todo!()
    }

    /// The `Actor` a GitHub user object (`login`, `id`, `type`) is. A `type`
    /// of `Bot` or a `[bot]` suffix is `ActorKind::Bot`. Used by every place
    /// that turns a payload's user into an actor: the feed, `observe`'s
    /// review verdicts, the issue events listing's labeler.
    pub fn actor(&self, _user: &Value) -> Option<Actor> {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// GithubApi after the change (mod.rs). It keeps what is already good: the
// redacting Debug, the one process-wide rate-limit hold shared by Source,
// Channel and Forge, `RetryAfter`, `Fatal`, ETag support. It loses its single
// token, `login()`'s `GET /user`, and the "URL must start with the base" check
// is kept (a token never leaves api_base).
// ---------------------------------------------------------------------------

pub struct GithubApi {
    _private: (),
}

impl GithubApi {
    pub fn new(_api_base: &str, _minter: Arc<Minter>, _identities: Arc<Identities>) -> Result<Self> {
        todo!()
    }

    /// `repo` selects the token; the method selects the access.
    /// 401 with a cached token: `forget` it and retry once. A suspended or
    /// missing installation is `RetryAfter(SUSPENDED_WAIT)`, which the
    /// Source's backoff and the runtime's Effect retry already honour.
    pub async fn get(&self, _repo: &RepoName, _path: &str, _etag: Option<&str>) -> Result<Got> {
        todo!()
    }

    /// POST, PATCH, PUT, DELETE: all `Write`. (`patch` and `put` are the two
    /// the old API lacked.)
    pub async fn send(
        &self,
        _repo: &RepoName,
        _method: reqwest::Method,
        _path: &str,
        _body: Option<Value>,
    ) -> Result<Got> {
        todo!()
    }

    /// "Me": the bot. Replaces the `GET /user` identity that an
    /// installation token cannot answer.
    pub async fn bot(&self) -> Result<&Bot> {
        todo!()
    }

    pub fn identities(&self) -> &Arc<Identities> {
        todo!()
    }
}

/// As today (`Got::{Ok, NotModified, Refused}`), unchanged.
pub enum Got {
    Ok { body: Value, etag: Option<String> },
    NotModified,
    Refused { status: u16, message: String },
}

/// As today: a failure retrying cannot fix stops the loop that met it.
#[derive(Debug)]
pub struct Fatal(pub String);

/// Kept to show the seam the rest of the design leans on: a 429, a
/// secondary-limit 403 and a suspended installation become `RetryAfter`,
/// which the runtime's Effect retry and the Source's backoff already honour.
pub fn _retry_after_is_unchanged(_: RetryAfter) {}

pub fn _box_future_is_unchanged(_: BoxFuture<'static, ()>) {}

// ---------------------------------------------------------------------------
// Behavior pinned by PR 1's offline test (tests/agent_github.rs, against the
// fake GitHub extended with the App endpoints; no real GitHub):
//  - the JWT is RS256, `iss` is the app id, `exp - iat <= 10 min`, signed by a
//    test key the fake holds the public half of; a wrong key is a 401 and
//    stops the github Source with the sentence while Discord keeps working.
//  - the mint request names exactly one repository and exactly Access's
//    permissions (the fake records every mint request), never `workflows`.
//  - 20 concurrent `token(repo, Read)` calls make one mint; a second repo
//    makes a second; Read and Write for one repo make two.
//  - with the clock moved to 16 minutes before expiry, the cached token is
//    reused; at 14 minutes, the next call mints again.
//  - a 401 on a cached token mints once more and retries; a second 401 is the
//    error.
//  - suspension: the fake suspends the installation; the Source and a queued
//    Effect each wait 300 s (manual clock), log once, and resume without a
//    restart when the fake lifts it. Uninstall then reinstall (new
//    installation id) is found again. `serve` started while suspended keeps
//    running.
//  - `writes: false`: `token(.., Write)` is an error; reads still work.
//  - `bot()` is `<slug>[bot]` from `GET /app`; the Source drops that login's
//    comments; `@slug` and `@slug[bot]` both address it; the commit identity
//    in a clone is the derived one.
//  - identities: `alice` listed and pinned 7; a comment by `alice` with id 99
//    is actor `alice#99`, rejected `not_listed`; the pin file still says 7
//    after a restart; a listed login whose lookup failed is not admitted.
//  - a canary token appears in no argv (a wrapper `git` logs /proc cmdline), no
//    transcript and no event; Debug of every credential type is redacted.
//  - a `key_file` under a sandbox-visible path, a symlink, or mode 0644 is
//    refused with the path named.
//  - config with `token_file` on the github source or forge fails with the
//    migration sentence, not serde's "unknown field".
//  - `docs/agent-mode/github-app/manifest.json`'s `default_permissions` equal
//    `Access::app_permissions()`.
// ---------------------------------------------------------------------------
