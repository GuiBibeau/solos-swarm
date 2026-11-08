//! The GitHub App: the agent's one credential.
//!
//! The App's private key is held by exactly one object, [`Minter`]. Everything
//! else holds an [`InstallToken`], a bearer for ONE repository at ONE access
//! level, for under an hour, in memory only.
//!
//! Where the key lives: `~/agent/tokens/github-app.pem`, mode 0600, the
//! durable copy. It is a token file like `discord-token`: the production
//! unit copies it to its tmpfs runtime directory before every start,
//! `agent serve` reads that copy once and deletes it ([`AppKey::take`]), and
//! the process keeps the parsed key for its life. No sandbox binds
//! `~/agent/tokens` or the runtime directory, and `serve` refuses a
//! `key_file` under any path a sandbox can read. It is not a vault item
//! (any vault key is reachable by a worker through `fetch_url`, and this key
//! mints tokens) and it is not sealed under a second key (which would sit on
//! the same disk and add nothing). Its radius is the App's grant: chosen
//! repositories, no `workflows`, no `administration`; GitHub keeps several
//! keys per App, so it rotates without downtime.

use super::{Absent, Fatal, Got, Transport, suspended};
use crate::agent::utc;
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use reqwest::Method;
use rsa::RsaPrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use rsa::traits::PublicKeyParts as _;
use serde_json::{Value, json};
use sha2::Sha256;
use solos_agent_core::change::RepoName;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::OnceCell;
use zeroize::Zeroizing;

/// Mint again when less than this remains on a cached token. Longer than
/// git's own timeout, so a fetch that starts with a token always finishes
/// before the token expires.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(15 * 60);
const _: () = assert!(
    REFRESH_MARGIN.as_secs() > crate::agent::forge::GIT_WAIT.as_secs(),
    "a push must never outlive its token"
);

/// How long every call for a repository waits while its installation is
/// suspended, removed or missing: the owner's kill switch on GitHub. A wait,
/// not a failure, so unsuspending resumes work without a restart and nothing
/// crash-loops.
pub const SUSPENDED_WAIT: Duration = Duration::from_secs(300);

/// GitHub refuses an App JWT that lives longer than ten minutes.
const JWT_LIFE: Duration = Duration::from_secs(9 * 60);
/// Clock skew allowance: the JWT is issued this far in the past.
const JWT_SKEW: Duration = Duration::from_secs(60);
/// GitHub App keys are 2048-bit RSA.
const MIN_KEY_BYTES: usize = 256;

/// The App's numeric id: `iss` of every JWT.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppId(pub u64);

/// What an installation token may do. Chosen by the call site's HTTP method
/// ([`Access::for_method`]), never by an argument and never by a model: a GET
/// cannot carry a write token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Access {
    Read,
    Write,
}

/// What the App must hold, as `(permission, level)` in the manifest's words.
/// `workflows` and `administration` are in no list: the App does not hold
/// them, so GitHub itself refuses a push that edits `.github/workflows/**`
/// and the agent can never change branch protection.
const WRITE: [(&str, &str); 7] = [
    ("metadata", "read"),
    ("contents", "write"),
    ("pull_requests", "write"),
    ("issues", "write"),
    ("checks", "read"),
    ("statuses", "read"),
    ("actions", "read"),
];

/// Permissions the App must never hold. `agent github check` refuses an
/// installation that has one.
const FORBIDDEN: [&str; 2] = ["workflows", "administration"];

impl Access {
    /// GET and HEAD read; everything else writes.
    pub fn for_method(method: &Method) -> Self {
        if matches!(*method, Method::GET | Method::HEAD) {
            Self::Read
        } else {
            Self::Write
        }
    }

    /// The narrowed permissions sent with a mint request: a token can only
    /// be narrower than the App's grant, never broader.
    pub fn permissions(self) -> Value {
        let level = |name: &str, level: &str| {
            let read_only = self == Self::Read && level == "write";
            (
                name.to_owned(),
                Value::String(if read_only { "read" } else { level }.to_owned()),
            )
        };
        Value::Object(WRITE.iter().map(|(name, at)| level(name, at)).collect())
    }

    /// What the App must hold: the write token's permissions. The one source
    /// of the App's permission list: a test pins the docs' `manifest.json`
    /// to it, and `agent github check` compares an installation with it.
    pub fn app_permissions() -> Value {
        Self::Write.permissions()
    }
}

/// The App's private key, parsed. No `Clone`, no `Debug` content; the PEM
/// bytes are zeroized once parsed and the key zeroizes itself on drop. GitHub
/// issues PKCS#1 ("BEGIN RSA PRIVATE KEY"); PKCS#8 is accepted too.
pub struct AppKey {
    signer: SigningKey<Sha256>,
}

impl std::fmt::Debug for AppKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AppKey(..)")
    }
}

impl AppKey {
    /// For `serve`: read the file once and delete it, as every token file
    /// is. Refused: a symlink, a file open to group or others, and a PEM that
    /// is not a 2048-bit or larger RSA key. A refusal fails `serve` at start,
    /// as an unreadable token file does: only the owner can fix it.
    pub fn take(path: &Path) -> Result<Self> {
        Self::open(path, true)
    }

    /// For `agent github check` and tests: the same checks, no deletion.
    pub fn read(path: &Path) -> Result<Self> {
        Self::open(path, false)
    }

    fn open(path: &Path, delete: bool) -> Result<Self> {
        let shown = path.display();
        let meta = std::fs::symlink_metadata(path)
            .with_context(|| format!("cannot read the GitHub App key {shown}"))?;
        ensure!(
            !meta.file_type().is_symlink(),
            "the GitHub App key {shown} is a symlink"
        );
        ensure!(meta.is_file(), "the GitHub App key {shown} is not a file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                meta.permissions().mode() & 0o077 == 0,
                "the GitHub App key {shown} is open to group or others; chmod 600 it"
            );
        }
        let read = std::fs::read_to_string(path);
        if delete {
            let _ = std::fs::remove_file(path);
        }
        let pem = Zeroizing::new(
            read.with_context(|| format!("cannot read the GitHub App key {shown}"))?,
        );
        Self::parse(&pem).with_context(|| format!("the GitHub App key {shown}"))
    }

    /// The key a PEM holds.
    pub fn parse(pem: &str) -> Result<Self> {
        let key = RsaPrivateKey::from_pkcs1_pem(pem)
            .or_else(|_| RsaPrivateKey::from_pkcs8_pem(pem))
            .map_err(|_| {
                anyhow!(
                    "is not an RSA private key in PEM form (the file GitHub downloads starts \
                     with -----BEGIN RSA PRIVATE KEY-----)"
                )
            })?;
        ensure!(
            key.size() >= MIN_KEY_BYTES,
            "is an RSA key of {} bits, and GitHub App keys have 2048",
            key.size() * 8
        );
        Ok(Self {
            signer: SigningKey::new(key),
        })
    }

    /// RS256, `iss = app`, `iat = now - 60 s`, `exp = now + 9 min`. Private:
    /// the JWT never leaves the Minter's own calls.
    fn sign(&self, app: AppId, now: SystemTime) -> Result<Zeroizing<String>> {
        let at = now
            .duration_since(UNIX_EPOCH)
            .context("the clock is before 1970")?
            .as_secs();
        let claims = json!({
            "iat": at.saturating_sub(JWT_SKEW.as_secs()),
            "exp": at + JWT_LIFE.as_secs(),
            "iss": app.0,
        });
        let head = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let body = URL_SAFE_NO_PAD.encode(claims.to_string());
        let message = format!("{head}.{body}");
        // Blinded, so signing time does not depend on the key.
        let signature = self
            .signer
            .try_sign_with_rng(&mut rand::thread_rng(), message.as_bytes())
            .map_err(|_| anyhow!("the App key could not sign"))?;
        Ok(Zeroizing::new(format!(
            "{message}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        )))
    }
}

/// A bearer for one repository at one access level. `Debug` redacts. Lives in
/// memory only; never logged, never written, never put in argv.
#[derive(Clone)]
pub struct InstallToken {
    secret: Arc<Zeroizing<String>>,
    pub repo: RepoName,
    pub access: Access,
    expires: SystemTime,
}

impl InstallToken {
    pub(crate) fn new(
        secret: impl Into<String>,
        repo: RepoName,
        access: Access,
        expires: SystemTime,
    ) -> Self {
        Self {
            secret: Arc::new(Zeroizing::new(secret.into())),
            repo,
            access,
            expires,
        }
    }

    pub(crate) fn bearer(&self) -> &str {
        &self.secret
    }

    /// `Authorization: Basic base64("x-access-token:<token>")`, the value git
    /// puts in `http.<git_base>.extraheader`. Handed to the mirror's git
    /// environment only, never argv and never a sandbox.
    pub(crate) fn git_header(&self) -> Zeroizing<String> {
        let basic = Zeroizing::new(format!("x-access-token:{}", self.secret.as_str()));
        Zeroizing::new(format!(
            "Authorization: Basic {}",
            STANDARD.encode(basic.as_bytes())
        ))
    }

    pub fn expires(&self) -> SystemTime {
        self.expires
    }
}

impl std::fmt::Debug for InstallToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InstallToken({} {:?}, ..)", self.repo, self.access)
    }
}

/// The App's own identity on GitHub, learned once from `GET /app` and
/// `GET /users/<slug>[bot]`: the Source's "me", and the commits' author.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bot {
    pub slug: String,
    /// `<slug>[bot]`, compared case-insensitively.
    pub login: String,
    /// The bot account's own id, which its noreply address names.
    pub id: u64,
    /// `<id>+<slug>[bot]@users.noreply.github.com`.
    pub email: String,
}

impl Bot {
    pub fn new(slug: &str, id: u64) -> Self {
        let login = format!("{slug}[bot]");
        Self {
            slug: slug.to_owned(),
            email: format!("{id}+{login}@users.noreply.github.com"),
            login,
            id,
        }
    }

    /// Whether `login` is the bot's.
    pub fn is(&self, login: &str) -> bool {
        self.login.eq_ignore_ascii_case(login)
    }

    /// A mention addresses the agent when the text holds `@<slug>` or
    /// `@<slug>[bot]` as GitHub links it: not inside a word or an email
    /// address, and not as the start of a longer name or a team. People type
    /// the first; GitHub's autocomplete writes the second.
    pub fn mentioned_in(&self, text: &str) -> bool {
        let text = text.to_ascii_lowercase();
        let needle = format!("@{}", self.slug.to_ascii_lowercase());
        text.match_indices(&needle).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + needle.len()..].chars().next();
            !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
                && !after.is_some_and(name_char)
        })
    }

    /// The text without the `@slug` or `@slug[bot]` people open a comment to
    /// the agent with.
    pub fn unmention(&self, text: &str) -> String {
        let text = text.trim();
        for handle in [format!("@{}", self.login), format!("@{}", self.slug)] {
            let rest = text
                .get(..handle.len())
                .filter(|head| head.eq_ignore_ascii_case(&handle))
                .map(|_| &text[handle.len()..])
                .filter(|rest| !rest.chars().next().is_some_and(name_char));
            if let Some(rest) = rest {
                return rest.trim_start_matches([',', ':']).trim().to_owned();
            }
        }
        text.to_owned()
    }

    /// What the clone is configured with: `<login> <email>`.
    pub fn commit_identity(&self) -> (String, String) {
        (self.login.clone(), self.email.clone())
    }
}

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/')
}

/// Time as a function, so a test moves it and a mint near expiry is provable
/// without sleeping.
pub type Now = Arc<dyn Fn() -> SystemTime + Send + Sync>;

pub struct MinterConfig {
    pub app: AppId,
    /// The REST root, [`super::DEFAULT_API_BASE`] unless a test points it at
    /// a fake.
    pub api_base: String,
    /// `github.writes: false`: a write token is refused here, so a read-only
    /// agent is read-only physically, not by policy.
    pub writes: bool,
    /// `forge.repos`: the only repositories a token is minted for.
    pub repos: BTreeSet<RepoName>,
    /// How long a suspended or missing installation makes every call wait.
    pub suspended_wait: Duration,
}

impl MinterConfig {
    pub fn new(app: AppId, api_base: &str, writes: bool, repos: BTreeSet<RepoName>) -> Self {
        Self {
            app,
            api_base: api_base.to_owned(),
            writes,
            repos,
            suspended_wait: SUSPENDED_WAIT,
        }
    }
}

/// Where a repository's installation stands, as the log last said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    Working,
    Suspended,
    Missing,
}

/// One cached token per `(repository, access)`; the gate lets one mint at a
/// time for its key, so concurrent callers share the mint.
struct Slot {
    gate: tokio::sync::Mutex<()>,
    token: Mutex<Option<InstallToken>>,
}

/// Mints, caches, narrows and refreshes installation tokens.
///
/// `token(repo, access)`: a cached token with more than [`REFRESH_MARGIN`]
/// left is reused; otherwise, one caller per `(repo, access)` signs a JWT,
/// finds the installation (`GET /repos/{repo}/installation`, remembered, and
/// forgotten on a 404 so a reinstall's new id is found), and posts
/// `POST /app/installations/{id}/access_tokens` with `repositories: [one]` and
/// the access's permissions.
///
/// A suspended installation (403 on the mint, or `suspended_at` on the
/// lookup), an App not installed on the repository (404), or a repository
/// that left the installation (422 naming it) is `Err(Absent)`, a `RetryAfter`
/// too, logged once per change of state.
pub struct Minter {
    config: MinterConfig,
    key: AppKey,
    transport: Arc<Transport>,
    now: Now,
    slots: Mutex<BTreeMap<(RepoName, Access), Arc<Slot>>>,
    installations: Mutex<BTreeMap<RepoName, u64>>,
    standing: Mutex<BTreeMap<RepoName, Standing>>,
    slug: OnceCell<String>,
    bot: OnceCell<Bot>,
}

impl Minter {
    pub fn new(config: MinterConfig, key: AppKey, now: Now) -> Result<Self> {
        let transport = Arc::new(Transport::new(&config.api_base)?);
        Ok(Self {
            config,
            key,
            transport,
            now,
            slots: Mutex::default(),
            installations: Mutex::default(),
            standing: Mutex::default(),
            slug: OnceCell::new(),
            bot: OnceCell::new(),
        })
    }

    pub(super) fn transport(&self) -> Arc<Transport> {
        self.transport.clone()
    }

    pub fn allows(&self, repo: &RepoName) -> bool {
        self.config.repos.contains(repo)
    }

    /// Whether write tokens are minted at all (`github.writes`).
    pub fn writes(&self) -> bool {
        self.config.writes
    }

    pub fn repos(&self) -> &BTreeSet<RepoName> {
        &self.config.repos
    }

    /// A configured repository the App can read now: the first whose read
    /// token can be had. For what any token may ask, such as a public
    /// account, which should not wait on one repository's installation when
    /// another's works.
    pub(crate) async fn readable_repo(&self) -> Result<RepoName> {
        let mut last = None;
        for repo in &self.config.repos {
            match self.token(repo, Access::Read).await {
                Ok(_) => return Ok(repo.clone()),
                Err(err) if super::retry_after(&err).is_some() => last = Some(err),
                Err(err) => return Err(err),
            }
        }
        Err(last.unwrap_or_else(|| {
            anyhow!("the GitHub App has no repository to act on: forge.repos is empty")
        }))
    }

    /// A token for `repo` with `access`, minted or cached.
    pub async fn token(&self, repo: &RepoName, access: Access) -> Result<InstallToken> {
        ensure!(
            self.allows(repo),
            "{repo} is not in forge.repos, so no GitHub token is minted for it"
        );
        ensure!(
            access == Access::Read || self.config.writes,
            "github.writes is false, so no write token is minted"
        );
        let slot = self.slot(repo, access);
        if let Some(token) = self.fresh(&slot) {
            return Ok(token);
        }
        let _one_mint_at_a_time = slot.gate.lock().await;
        if let Some(token) = self.fresh(&slot) {
            return Ok(token);
        }
        let token = self.mint(repo, access).await?;
        *slot.token.lock().unwrap_or_else(PoisonError::into_inner) = Some(token.clone());
        Ok(token)
    }

    /// After a 401 on a cached token (revoked, or the installation changed):
    /// drop it, so the retry mints again.
    pub fn forget(&self, token: &InstallToken) {
        let slot = self.slot(&token.repo, token.access);
        let mut cached = slot.token.lock().unwrap_or_else(PoisonError::into_inner);
        if cached
            .as_ref()
            .is_some_and(|held| held.bearer() == token.bearer())
        {
            *cached = None;
        }
    }

    /// The error of a call that found the installation suspended: it waits.
    pub(super) fn suspended(&self, repo: &RepoName) -> anyhow::Error {
        self.waiting(repo, Standing::Suspended)
    }

    fn slot(&self, repo: &RepoName, access: Access) -> Arc<Slot> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots
            .entry((repo.clone(), access))
            .or_insert_with(|| {
                Arc::new(Slot {
                    gate: tokio::sync::Mutex::new(()),
                    token: Mutex::new(None),
                })
            })
            .clone()
    }

    fn fresh(&self, slot: &Slot) -> Option<InstallToken> {
        let now = (self.now)();
        let cached = slot.token.lock().unwrap_or_else(PoisonError::into_inner);
        cached
            .as_ref()
            .filter(|token| {
                token
                    .expires
                    .duration_since(now)
                    .is_ok_and(|left| left > REFRESH_MARGIN)
            })
            .cloned()
    }

    async fn mint(&self, repo: &RepoName, access: Access) -> Result<InstallToken> {
        let mut rediscovered = false;
        loop {
            let installation = self.installation(repo).await?;
            let path = format!("app/installations/{installation}/access_tokens");
            let body = json!({
                "repositories": [repo.name()],
                "permissions": access.permissions(),
            });
            match self.app_call(Method::POST, &path, Some(&body)).await? {
                Got::Ok(page) => {
                    let secret = page.body["token"]
                        .as_str()
                        .context("GitHub's answer holds no installation token")?;
                    let expires = page.body["expires_at"]
                        .as_str()
                        .and_then(utc::parse)
                        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms))
                        .context("GitHub's answer holds no expiry for the token")?;
                    self.note(repo, Standing::Working);
                    return Ok(InstallToken::new(secret, repo.clone(), access, expires));
                }
                Got::Refused { status, message } => match status.as_u16() {
                    403 if suspended(&message) => {
                        return Err(self.waiting(repo, Standing::Suspended));
                    }
                    // The installation this id named is gone: a reinstall has
                    // a new one.
                    404 if !rediscovered => {
                        rediscovered = true;
                        self.forget_installation(repo);
                    }
                    404 => return Err(self.waiting(repo, Standing::Missing)),
                    // The repository is not in the installation this id names:
                    // it left, or came back in another one. Find it again.
                    422 if message.to_ascii_lowercase().contains("repositor") => {
                        self.forget_installation(repo);
                        return Err(self.waiting(repo, Standing::Missing));
                    }
                    401 => return Err(rejected(&message)),
                    _ => bail!("github minting a token for {repo}: HTTP {status}: {message}"),
                },
                Got::NotModified => bail!("github minting a token for {repo}: an unasked 304"),
            }
        }
    }

    fn forget_installation(&self, repo: &RepoName) {
        self.installations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(repo);
    }

    /// The installation of the App on `repo`'s account that holds `repo`.
    async fn installation(&self, repo: &RepoName) -> Result<u64> {
        let known = self
            .installations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(repo)
            .copied();
        if let Some(id) = known {
            return Ok(id);
        }
        let path = format!("repos/{repo}/installation");
        match self.app_call(Method::GET, &path, None).await? {
            Got::Ok(page) => {
                let id = page.body["id"]
                    .as_u64()
                    .context("GitHub's installation answer holds no id")?;
                if !page.body["suspended_at"].is_null() {
                    return Err(self.waiting(repo, Standing::Suspended));
                }
                self.installations
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(repo.clone(), id);
                Ok(id)
            }
            Got::Refused { status, .. } if status.as_u16() == 404 => {
                Err(self.waiting(repo, Standing::Missing))
            }
            Got::Refused { status, message } if status.as_u16() == 401 => Err(rejected(&message)),
            Got::Refused { status, message } => {
                bail!("github GET {path}: HTTP {status}: {message}")
            }
            Got::NotModified => bail!("github GET {path}: an unasked 304"),
        }
    }

    /// A call authenticated by a fresh JWT, which never leaves this module.
    async fn app_call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Got> {
        let jwt = self.key.sign(self.config.app, (self.now)())?;
        self.transport
            .send(method, &self.transport.url(path), &jwt, None, body)
            .await
    }

    /// Record `standing` for `repo`, and say whether it changed.
    fn note(&self, repo: &RepoName, standing: Standing) -> bool {
        let mut seen = self.standing.lock().unwrap_or_else(PoisonError::into_inner);
        let before = seen.insert(repo.clone(), standing);
        let changed = before.unwrap_or(Standing::Working) != standing;
        if changed && standing == Standing::Working {
            eprintln!("agent: github: the App works on {repo} again");
        }
        changed
    }

    /// The error that makes every call for `repo` wait, logged once per
    /// change of state.
    fn waiting(&self, repo: &RepoName, standing: Standing) -> anyhow::Error {
        let why = match standing {
            Standing::Suspended => format!("the GitHub App is suspended on {repo}"),
            _ => format!("the GitHub App is not installed on {repo}"),
        };
        let wait = self.config.suspended_wait;
        if self.note(repo, standing) {
            eprintln!(
                "agent: github: {why}; waiting {}s and trying again (unsuspend or install it \
                 and work resumes by itself)",
                wait.as_secs()
            );
        }
        Absent::new(why, wait).into()
    }

    /// The App's slug, from `GET /app`, which only its JWT can ask.
    pub async fn slug(&self) -> Result<&String> {
        self.slug
            .get_or_try_init(|| async {
                match self.app_call(Method::GET, "app", None).await? {
                    Got::Ok(page) => page.body["slug"]
                        .as_str()
                        .map(str::to_owned)
                        .context("GitHub GET app named no slug"),
                    Got::Refused { status, message } if status.as_u16() == 401 => {
                        Err(rejected(&message))
                    }
                    Got::Refused { status, message } => {
                        bail!("github GET app: HTTP {status}: {message}")
                    }
                    Got::NotModified => bail!("github GET app: an unasked 304"),
                }
            })
            .await
    }

    /// The App's own account. Its login is `<slug>[bot]`; its id is the bot
    /// user's, which the commits' noreply address names.
    pub async fn bot(&self) -> Result<&Bot> {
        self.bot
            .get_or_try_init(|| async {
                let slug = self.slug().await?.clone();
                let login = format!("{slug}[bot]");
                let repo = self.readable_repo().await?;
                let token = self.token(&repo, Access::Read).await?;
                let url = self.transport.url(&format!("users/{login}"));
                let user = self
                    .transport
                    .send(Method::GET, &url, token.bearer(), None, None)
                    .await?;
                match user {
                    Got::Ok(page) => {
                        let id = page.body["id"]
                            .as_u64()
                            .context("GitHub's answer for the bot's account holds no id")?;
                        Ok(Bot::new(&slug, id))
                    }
                    Got::Refused { status, message } => {
                        bail!("github GET users/{login}: HTTP {status}: {message}")
                    }
                    Got::NotModified => bail!("github GET users/{login}: an unasked 304"),
                }
            })
            .await
    }

    /// For each of `repos`: whether the App is installed on it, whether the
    /// installation is suspended, and which permissions it lacks or must not
    /// have. `serve` logs one line per repository and never stops on it: a
    /// missing or suspended installation is the same wait as a later
    /// suspension. Only a key GitHub rejects is an error here.
    pub async fn preflight(&self, repos: &[RepoName]) -> Result<Vec<Preflight>> {
        let slug = self.slug().await?;
        let mut found = Vec::new();
        for repo in repos {
            let path = format!("repos/{repo}/installation");
            let state = match self.app_call(Method::GET, &path, None).await? {
                Got::Ok(page) if !page.body["suspended_at"].is_null() => InstallState::Suspended,
                Got::Ok(page) => {
                    let permissions: Vec<(String, String)> = page.body["permissions"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .filter_map(|(name, level)| {
                            Some((name.clone(), level.as_str()?.to_owned()))
                        })
                        .collect();
                    let (missing, extra) = audit(&permissions);
                    InstallState::Installed {
                        installation: page.body["id"].as_u64().unwrap_or_default(),
                        permissions,
                        missing,
                        extra,
                    }
                }
                Got::Refused { status, .. } if status.as_u16() == 404 => {
                    InstallState::NotInstalled {
                        install_url: format!("https://github.com/apps/{slug}/installations/new"),
                    }
                }
                Got::Refused { status, message } if status.as_u16() == 401 => {
                    return Err(rejected(&message));
                }
                Got::Refused { status, message } => {
                    bail!("github GET {path}: HTTP {status}: {message}")
                }
                Got::NotModified => bail!("github GET {path}: an unasked 304"),
            };
            found.push(Preflight {
                repo: repo.clone(),
                state,
            });
        }
        Ok(found)
    }
}

/// The permissions in [`Access::app_permissions`] the installation lacks, and
/// the ones it holds that the agent must never have.
fn audit(held: &[(String, String)]) -> (Vec<String>, Vec<String>) {
    let rank = |level: &str| match level {
        "read" => 1,
        "write" => 2,
        "admin" => 3,
        _ => 0,
    };
    let missing = WRITE
        .iter()
        .filter(|(name, need)| {
            !held
                .iter()
                .any(|(have, level)| have == name && rank(level) >= rank(need))
        })
        .map(|(name, need)| format!("{name}: {need}"))
        .collect();
    let extra = FORBIDDEN
        .iter()
        .filter(|name| held.iter().any(|(have, _)| have == *name))
        .map(|name| (*name).to_owned())
        .collect();
    (missing, extra)
}

/// GitHub refused the App's JWT: the App id or the key is wrong, or the
/// clock is far off. Retrying cannot help.
fn rejected(message: &str) -> anyhow::Error {
    Fatal(format!(
        "GitHub refused the App's key (HTTP 401: {message}); check github.app_id and key_file, \
         and that this machine's clock is right"
    ))
    .into()
}

/// One configured repository's installation, as `preflight` found it.
pub struct Preflight {
    pub repo: RepoName,
    pub state: InstallState,
}

pub enum InstallState {
    Installed {
        installation: u64,
        /// What the installation grants, as GitHub reports it.
        permissions: Vec<(String, String)>,
        /// From [`Access::app_permissions`], as `name: level`.
        missing: Vec<String>,
        /// `workflows` or `administration`, which the App must not hold.
        extra: Vec<String>,
    },
    Suspended,
    NotInstalled {
        install_url: String,
    },
}

#[cfg(test)]
mod tests {
    use super::{Access, Bot, Standing, audit};
    use reqwest::Method;
    use serde_json::json;

    #[test]
    fn a_get_reads_and_everything_else_writes() {
        for (method, access) in [
            (Method::GET, Access::Read),
            (Method::HEAD, Access::Read),
            (Method::POST, Access::Write),
            (Method::PATCH, Access::Write),
            (Method::PUT, Access::Write),
            (Method::DELETE, Access::Write),
        ] {
            assert_eq!(Access::for_method(&method), access, "{method}");
        }
    }

    #[test]
    fn a_read_token_is_narrower_than_a_write_token_and_neither_holds_workflows() {
        assert_eq!(
            Access::Read.permissions(),
            json!({
                "metadata": "read", "contents": "read", "pull_requests": "read",
                "issues": "read", "checks": "read", "statuses": "read", "actions": "read",
            })
        );
        assert_eq!(
            Access::Write.permissions(),
            json!({
                "metadata": "read", "contents": "write", "pull_requests": "write",
                "issues": "write", "checks": "read", "statuses": "read", "actions": "read",
            })
        );
        assert_eq!(Access::app_permissions(), Access::Write.permissions());
        for access in [Access::Read, Access::Write] {
            let permissions = access.permissions();
            assert!(permissions.get("workflows").is_none());
            assert!(permissions.get("administration").is_none());
        }
    }

    #[test]
    fn an_installation_is_judged_against_the_permission_list() {
        let held = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(name, level)| ((*name).to_owned(), (*level).to_owned()))
                .collect()
        };
        let all = held(&[
            ("metadata", "read"),
            ("contents", "write"),
            ("pull_requests", "write"),
            ("issues", "write"),
            ("checks", "read"),
            ("statuses", "read"),
            ("actions", "read"),
        ]);
        assert_eq!(audit(&all), (vec![], vec![]));
        let weak = held(&[
            ("metadata", "read"),
            ("contents", "read"),
            ("pull_requests", "write"),
            ("issues", "write"),
            ("checks", "write"),
            ("workflows", "write"),
        ]);
        let (missing, extra) = audit(&weak);
        assert_eq!(
            missing,
            ["contents: write", "statuses: read", "actions: read"]
        );
        assert_eq!(extra, ["workflows"]);
    }

    #[test]
    fn the_bot_is_the_slug_with_a_suffix_and_a_mention_is_either_handle() {
        let bot = Bot::new("solos-bot", 4242);
        assert_eq!(bot.login, "solos-bot[bot]");
        assert_eq!(bot.email, "4242+solos-bot[bot]@users.noreply.github.com");
        assert_eq!(
            bot.commit_identity(),
            (bot.login.clone(), bot.email.clone())
        );
        assert!(bot.is("Solos-Bot[BOT]"));
        for (text, mentioned) in [
            ("@solos-bot fix it", true),
            ("@solos-bot[bot] fix it", true),
            ("ping @SOLOS-BOT.", true),
            ("(@solos-bot)", true),
            ("mail alice@solos-bot.dev", false),
            ("@solos-bots fix it", false),
            ("@solos-bot-2 fix it", false),
            ("@solos-bot/team", false),
            ("no mention", false),
        ] {
            assert_eq!(bot.mentioned_in(text), mentioned, "{text}");
        }
        assert_eq!(bot.unmention("  @Solos-Bot, fix the test "), "fix the test");
        assert_eq!(bot.unmention("@solos-bot[bot]: do it"), "do it");
        assert_eq!(bot.unmention("@solos-bot: x"), "x");
        assert_eq!(bot.unmention("@solos-bots x"), "@solos-bots x");
        assert_eq!(bot.unmention("thanks @solos-bot"), "thanks @solos-bot");
        assert_eq!(bot.unmention("é"), "é");
    }

    #[test]
    fn a_change_of_standing_is_news_once() {
        use super::{AppId, AppKey, Minter, MinterConfig};
        use std::sync::Arc;
        use std::time::SystemTime;
        // No key signs here: only the log's bookkeeping is under test.
        let pem = include_str!("../../../tests/fixtures/github-app-test-key.pem");
        let key = AppKey::parse(pem).unwrap();
        let repo = solos_agent_core::change::RepoName::try_from("acme/widgets".to_owned()).unwrap();
        let config =
            MinterConfig::new(AppId(1), "http://127.0.0.1:1/", true, [repo.clone()].into());
        let minter = Minter::new(config, key, Arc::new(SystemTime::now)).unwrap();

        assert!(
            !minter.note(&repo, Standing::Working),
            "working is the start"
        );
        assert!(minter.note(&repo, Standing::Suspended));
        assert!(!minter.note(&repo, Standing::Suspended), "said once");
        assert!(minter.note(&repo, Standing::Missing));
        assert!(minter.note(&repo, Standing::Working), "and when it is over");
    }
}
