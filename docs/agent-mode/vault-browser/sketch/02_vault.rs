//! Lives at: src/vault.rs, with private submodules src/vault/file.rs (the
//! encrypted envelope, the write lock, atomic replace) and src/vault/scrub.rs,
//! and a public pure submodule src/vault/policy.rs (origins, hosts, use rules).
//! `pub mod vault;` in src/lib.rs.
//!
//! One vault per machine that uses secrets: the agent host (`vault:` in
//! agent.yaml) and each node (`vault:` in node.yaml), the same code. A value
//! is used only on the machine that holds it: the agent host's vault serves
//! `fetch_url` and a local browser; a node's serves only the node's browser.
//! Node vault values never cross the wire.
//!
//! Write-only: the CLI can add, replace and remove an item and list its
//! metadata. Nothing prints, returns or serializes a value. A value leaves
//! this module in exactly two shapes, and only through an [`Admission`]:
//! - [`LoginFill`], which `browse` fills into one `<input type=password>`;
//! - [`KeyHeader`], which becomes one request header on a bound host.
//!
//! Every value of the vault also feeds a [`Scrubber`], which every byte a
//! browser or a keyed fetch returns passes through before anyone sees it.
//! Scrubbing is the second line; the first is that values are never put
//! where the model reads.

// The submodules are written inline at the bottom of this sketch; in the
// repo they are src/vault/{policy,scrub,file}.rs.

pub use policy::{HostSpec, Origin};
pub use scrub::Scrubber;

use anyhow::Result;
use serde::Deserialize;
use solos_agent_core::model::TicketId;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// The shortest value the vault takes: shorter ones cannot be scrubbed
/// safely (`remote/redact.rs`'s `MIN_SECRET_BYTES`, same reason).
pub const MIN_VALUE_BYTES: usize = 8;
pub const MAX_VALUE_BYTES: usize = 4_096;
const HINT_CHARS: usize = 200;

/// `vault:` in agent.yaml and in node.yaml: one struct, two embeddings.
/// Paths only (config structs derive `Debug`, grounding constraint 6).
/// Relative paths resolve against the config file's directory, after
/// `validate` (grounding gotcha 13).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultConfig {
    /// The encrypted vault file. Created by `solos-swarm vault init`.
    pub path: PathBuf,
    /// 32 random bytes, mode 0600, made by `vault init`. Must not be under
    /// any path a sandbox binds (`/usr`, `/etc`, `/lib`, `/lib64`, run_code
    /// mounts, `uv_root`, the Tickets' directory): every sandbox ro-binds
    /// all of `/etc` (`harness.rs:143`, `code.rs:529`), so a key under
    /// `/etc` is a key the model can `cat`. `open` refuses it.
    pub key_file: PathBuf,
}

/// An item's name: a short word (`[a-z0-9][a-z0-9-]{0,47}`), the only
/// handle a model ever has on a secret. Parsed at every boundary.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SecretName(String);

impl TryFrom<String> for SecretName {
    type Error = String;
    fn try_from(name: String) -> std::result::Result<Self, String> {
        todo!("short lowercase word; else \"{name:?} cannot name a vault item\"")
    }
}
impl From<SecretName> for String {
    fn from(name: SecretName) -> String {
        name.0
    }
}

/// A secret value. No `Debug` beyond `SecretValue(..)`, no `Display`, no
/// `Serialize`, no `Clone` outside this module, zeroized on drop.
///
/// `expose` is `pub(crate)`, and `tests/secret_flow.rs` scans the source:
/// it may be called only in src/vault/ and src/browse/session.rs (the CDP
/// fill and the CDP header). A new call site fails the build's tests, so
/// the shortest path that compiles is not a path that ships.
pub struct SecretValue(Zeroizing<String>);

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretValue(..)")
    }
}

impl SecretValue {
    /// The CLI's only way in: `path` is read once and deleted, as every
    /// other secret file is (`main.rs:296-301`); `-` reads stdin. One
    /// trailing newline is trimmed. Refused: shorter than
    /// [`MIN_VALUE_BYTES`], longer than [`MAX_VALUE_BYTES`], a NUL, CR or LF
    /// inside. Never from argv (`/proc/<pid>/cmdline`, grounding gotcha 11).
    pub fn read_once(path: &Path) -> Result<Self> {
        todo!()
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

/// Whether a person must approve an item's first use on each Ticket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Approval {
    /// The default: the first use on each Ticket asks in its thread.
    #[default]
    FirstUse,
    /// Unattended use (schedules), as the operator chose.
    None,
}

/// `vault add-login`'s input.
pub struct NewLogin {
    pub name: SecretName,
    /// Exact origins it is filled on: `https://github.com`. https only,
    /// except loopback hosts (tests, local tools).
    pub sites: Vec<Origin>,
    /// Shown to the model and in Slack; not a secret.
    pub username: String,
    pub hint: String,
    /// Whose Tickets may use it, as the Policy names people
    /// (`slack:U024BE7LH`, `schedule:digest`); empty: any requester.
    /// When set, only these people may answer its approval.
    pub users: BTreeSet<String>,
    pub approval: Approval,
    pub password: SecretValue,
}

/// `vault add-key`'s input.
pub struct NewKey {
    pub name: SecretName,
    /// Exact hosts (optionally with a port) it is sent to, over https.
    pub hosts: Vec<HostSpec>,
    /// The header that carries it, such as `authorization` or `x-api-key`.
    pub header: String,
    pub scheme: KeyScheme,
    pub about: String,
    pub users: BTreeSet<String>,
    pub approval: Approval,
    pub value: SecretValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyScheme {
    /// `Authorization: Bearer <value>`.
    Bearer,
    /// The header's value is the value.
    Raw,
}

/// What `vault list` prints and `vault_list` derives from: metadata only.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ItemInfo {
    pub name: SecretName,
    pub kind: ItemKindInfo,
    pub users: BTreeSet<String>,
    pub approval: Approval,
    pub created_ms: u64,
    pub updated_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemKindInfo {
    Login { sites: Vec<Origin>, username: String, hint: String },
    Key { hosts: Vec<HostSpec>, header: String, scheme: KeyScheme, about: String },
}

/// What one Ticket's use of this vault is judged against. Built by the agent
/// broker from the Ticket row it reads (requester, grants) and sent with the
/// call, so a node's vault judges with the same facts as the agent's.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UseContext {
    pub ticket: TicketId,
    /// `Mandate.source` + `:` + `Mandate.requester.id`, the Policy's form.
    pub requester: String,
    /// Where this vault is: `local` or a node's name. Grants are keyed
    /// `<environment>/<name>` in the Ticket.
    pub environment: String,
    /// This Ticket's approval states for items of `environment`.
    pub grants: BTreeMap<SecretName, GrantState>,
    /// Whether this agent can take a person's approval at all
    /// (`AgentConfig::takes_approvals`).
    pub approvals: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantState {
    Waiting,
    Granted,
    Denied,
}

/// The answer to "may this Ticket use this item for this, now?". A value
/// comes out only inside `Allowed`.
pub enum Admission<T> {
    Allowed(T),
    /// A person must answer first. `secret` is `<environment>/<name>`;
    /// `label` is built from the item's metadata only (never model or page
    /// text): "sign in to https://github.com as gui (login github-gui)".
    NeedsApproval {
        secret: String,
        label: String,
        answerable_by: Vec<String>,
    },
    Refused(String),
}

/// A login admitted for one fill, on a page whose origin the check saw.
pub struct LoginFill {
    pub username: String,
    pub password: SecretValue,
    /// The origins the form may post to (the login's sites).
    pub sites: Vec<Origin>,
}

/// A key admitted for one use: a header on bound hosts only.
pub struct KeyHeader {
    pub name: SecretName,
    hosts: Vec<HostSpec>,
    header: reqwest::header::HeaderName,
    /// Scheme applied (`Bearer …`), marked sensitive, so reqwest's `Debug`
    /// prints `Sensitive`.
    value: reqwest::header::HeaderValue,
}

impl KeyHeader {
    /// `fetch_url`: on every hop, the header goes on iff this is true for
    /// the hop's own URL (grounding constraint 5).
    pub fn admits(&self, url: &reqwest::Url) -> bool {
        policy::key_admits(&self.hosts, url)
    }

    /// The browser: a paused request carries the key iff its URL is on the
    /// hosts, and either the page's top origin is on the hosts too or the
    /// request is the top-level navigation `browse` itself started. A page
    /// on another site cannot ride the key with `fetch()` or a form.
    pub fn admits_in_browser(&self, request: &reqwest::Url, top: &Origin, own_navigation: bool) -> bool {
        policy::key_admits(&self.hosts, request)
            && (own_navigation || policy::host_listed(&self.hosts, top))
    }

    pub fn header(&self) -> (&reqwest::header::HeaderName, &reqwest::header::HeaderValue) {
        (&self.header, &self.value)
    }
}

/// What `vault_list` shows of one vault for one Ticket.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct Catalog {
    pub logins: Vec<CatalogLogin>,
    pub keys: Vec<CatalogKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct CatalogLogin {
    pub login: SecretName,
    pub sites: Vec<Origin>,
    pub username: String,
    pub hint: String,
    #[serde(rename = "where")]
    pub environment: String,
    /// none | needed | waiting | granted | denied, for this Ticket.
    pub approval: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct CatalogKey {
    pub key: SecretName,
    pub hosts: Vec<HostSpec>,
    pub about: String,
    #[serde(rename = "where")]
    pub environment: String,
    /// `fetch_url` (agent host only) and/or `browser_open`.
    #[serde(rename = "use")]
    pub uses: Vec<String>,
    pub approval: String,
}

/// The open vault. Holds the key and the file's path, never decrypted
/// items: each use decrypts the file (a few KiB; microseconds), so a CLI
/// write while the agent serves is seen by the next use with no cache to
/// invalidate. Writers take an exclusive `std::fs::File::lock` on
/// `<path>.lock`; readers rely on the atomic rename and take no lock.
pub struct Vault {
    path: PathBuf,
    key: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault").field("path", &self.path).finish_non_exhaustive()
    }
}

impl Vault {
    /// `vault init`: a new key file (create_new, 0600) and an empty vault.
    /// Refuses to replace either. Provisioning only; `open` never invents a
    /// key (the `ArtifactStore::create_key` rule).
    pub fn init(config: &VaultConfig, visible: &SandboxView) -> Result<()> {
        todo!()
    }

    /// Open for use. Refused: a key file that is a symlink, is readable by
    /// group or others, is not 32 bytes, or is under a path `visible` lists;
    /// a vault file that does not authenticate under the key.
    pub fn open(config: &VaultConfig, visible: &SandboxView) -> Result<Self> {
        todo!()
    }

    /// Add, or with `replace` overwrite, an item. Replace is the only way to
    /// change a value; there is no read-back to compare against.
    pub fn add_login(&self, login: NewLogin, replace: bool) -> Result<()> {
        todo!("lock; read; refuse a name clash unless replace; insert; revision += 1; write")
    }

    pub fn add_key(&self, key: NewKey, replace: bool) -> Result<()> {
        todo!("as add_login; also parse header as a HeaderName and refuse hop-by-hop and Host/Cookie")
    }

    /// Whether it was there.
    pub fn remove(&self, name: &SecretName) -> Result<bool> {
        todo!()
    }

    /// Metadata only, for `vault list`.
    pub fn list(&self) -> Result<Vec<ItemInfo>> {
        todo!()
    }

    /// What `context`'s Ticket may name here, with its approval state.
    /// Items whose `users` exclude the requester are left out.
    pub fn catalog(&self, context: &UseContext, uses_for_keys: &[&str]) -> Result<Catalog> {
        todo!()
    }

    /// Admit `name` for a fill on a page whose origin is `page`, as the
    /// browser engine reports it (never as the model or the page says).
    ///
    /// ```text
    /// item = find(name)                  else Refused("there is no login {name} on {env}")
    /// item is a Login                    else Refused("{name} is an API key; use it as key")
    /// policy::site_listed(sites, page)   else Refused("{page} is not one of {name}'s sites: {sites}")
    /// policy::use_rule(item, context)    -> Allowed(()) | NeedsApproval | Refused
    /// Allowed => LoginFill { username, password: value, sites }
    /// ```
    pub fn admit_login(&self, name: &SecretName, page: &Origin, context: &UseContext) -> Result<Admission<LoginFill>> {
        todo!()
    }

    /// Admit `name` as a header for requests to `first` (fetch_url's URL,
    /// or the URL the browser opens). Same shape as `admit_login`, with
    /// `policy::key_admits(hosts, first)` as the binding check.
    pub fn admit_key(&self, name: &SecretName, first: &reqwest::Url, context: &UseContext) -> Result<Admission<KeyHeader>> {
        todo!()
    }

    /// Every value in the vault, in every form a page or a server can echo
    /// it (see [`Scrubber`]). Built per browser session and per keyed fetch,
    /// from the whole vault, not only the item in use: a logged-in page can
    /// show another key (an "API keys" settings page does).
    pub fn scrubber(&self) -> Result<Scrubber> {
        todo!()
    }
}

/// The paths some sandbox on this machine binds, so a key file under one is
/// refused. Built from `crate::bwrap::SYSTEM_BINDS` (the `/usr`, `/etc`,
/// `/lib`, `/lib64` list every sandbox builder now takes from one const,
/// instead of repeating literals in harness.rs, claude_code.rs and code.rs)
/// plus the configured run_code mounts, `uv_root` and the Tickets' directory.
pub struct SandboxView {
    pub visible: Vec<PathBuf>,
}

impl SandboxView {
    pub fn hides(&self, path: &Path) -> Result<()> {
        todo!("canonicalize; refuse if it starts with any visible path")
    }
}

/// The pure rules. Public for `browse`, for the agent's `fetch_url`, and so
/// they are tested as a table without files or keys.
pub mod policy {
    use super::{Admission, Approval, GrantState, UseContext};
    use serde::{Deserialize, Serialize};

    /// scheme://host[:port], lowercase, default port elided. https only,
    /// except loopback hosts (`localhost`, `127.0.0.0/8`, `::1`).
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
    #[serde(try_from = "String", into = "String")]
    pub struct Origin(String);

    impl Origin {
        /// The origin of `url` as the URL parser (WHATWG, IDNA) normalizes it.
        pub fn of(url: &reqwest::Url) -> Result<Self, String> {
            todo!()
        }
        pub fn host(&self) -> &str {
            todo!()
        }
    }
    impl TryFrom<String> for Origin {
        type Error = String;
        fn try_from(s: String) -> Result<Self, String> {
            todo!("parse as a URL with no path, query or fragment, then Origin::of")
        }
    }
    impl From<Origin> for String {
        fn from(o: Origin) -> String {
            o.0
        }
    }

    /// `host` or `host:port`, no wildcard. https only, except loopback.
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
    #[serde(try_from = "String", into = "String")]
    pub struct HostSpec(String);
    impl TryFrom<String> for HostSpec {
        type Error = String;
        fn try_from(s: String) -> Result<Self, String> {
            todo!()
        }
    }
    impl From<HostSpec> for String {
        fn from(h: HostSpec) -> String {
            h.0
        }
    }

    /// Exact origin equality. `https://github.com` does not admit
    /// `https://gist.github.com` or `http://github.com`.
    pub fn site_listed(sites: &[Origin], page: &Origin) -> bool {
        sites.contains(page)
    }

    /// `url` is https (or http on a loopback spec), its host and port are
    /// one of `hosts`. Userinfo in the URL is refused outright.
    pub fn key_admits(hosts: &[HostSpec], url: &reqwest::Url) -> bool {
        todo!()
    }

    pub fn host_listed(hosts: &[HostSpec], origin: &Origin) -> bool {
        todo!()
    }

    /// Who may cause a use, and whether a person must answer first.
    ///
    /// ```text
    /// users non-empty and requester not in users -> Refused("{name} is not for this ticket's requester")
    /// approval None                              -> Allowed
    /// grant Granted                              -> Allowed
    /// grant Denied                               -> Refused("a person denied {name} for this ticket")
    /// grant Waiting or absent, !context.approvals-> Refused("{name} needs a person's approval, and this agent takes none")
    /// grant Waiting or absent                    -> NeedsApproval { secret: "{env}/{name}", label, answerable_by: users }
    /// ```
    pub fn use_rule(
        name: &str,
        users: &std::collections::BTreeSet<String>,
        approval: Approval,
        label: impl FnOnce() -> String,
        context: &UseContext,
    ) -> Admission<()> {
        let _ = (name, users, approval, label, context, GrantState::Waiting);
        todo!()
    }
}

/// Lives at src/vault/scrub.rs.
mod scrub {
    use serde_json::Value;
    use zeroize::Zeroizing;

    /// Replaces every copy of every vault value with `[vault:<name>]`.
    ///
    /// Forms looked for, per value (and per `username:password` for logins):
    /// raw; percent-encoded as `application/x-www-form-urlencoded` writes it
    /// (a login form sent by GET puts it in a URL); JSON-string-escaped;
    /// HTML-entity-escaped; standard base64 (Basic auth echoes). Longest
    /// needle first, so a form that contains another is replaced whole.
    ///
    /// Never sees what it cannot match (a page that transforms a value with
    /// its own script), which is why it is the second line, not the first.
    pub struct Scrubber {
        needles: Vec<(Zeroizing<String>, String)>,
    }

    impl Scrubber {
        pub fn clean(&self, text: &str) -> String {
            todo!()
        }
        /// Every string leaf and every object key of `value`, in place.
        pub fn clean_json(&self, value: &mut Value) {
            todo!()
        }
        /// UTF-8 text blobs (markdown, HTML); invalid UTF-8 is replaced
        /// first, so nothing passes unscanned.
        pub fn clean_text_bytes(&self, bytes: &[u8]) -> Vec<u8> {
            todo!()
        }
    }

    impl std::fmt::Debug for Scrubber {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Scrubber({} needles)", self.needles.len())
        }
    }
}

/// Lives at src/vault/file.rs. Private: nothing outside `vault` sees the
/// format.
///
/// The file: `{"v":1,"nonce":"<b64 12 bytes>","ciphertext":"<b64>"}`,
/// AES-256-GCM (`aes-gcm`, already a dependency), AAD
/// `solos-swarm/vault/v1`. Plaintext: `{"revision": n, "items": [StoredItem]}`
/// where `StoredItem` is a private serde type holding `Zeroizing<String>`
/// values; it exists only between decrypt and drop.
///
/// Write: lock `<path>.lock` (exclusive `std::fs::File::lock`, stable since
/// Rust 1.89; with an older toolchain, a `create_new` lock file with the
/// writer's pid), read and decrypt,
/// change, revision + 1, encrypt with a fresh nonce, write `<path>.tmp-<uuid>`
/// (create_new, 0600), fsync, rename over `<path>`, fsync the directory,
/// unlock. A crash leaves the old file or the new one, never half of one.
mod file {}
