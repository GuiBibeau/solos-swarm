//! Nothing here prints or returns a value, and only the sealed file holds a
//! serialized one. A value leaves the
//! vault only inside [`Admission::Allowed`], as a [`LoginFill`] for one
//! password input or a [`KeyHeader`] for requests to the key's hosts. Every
//! value also feeds the [`Scrubber`], which cleans what a page or a server
//! echoes back.
//!
//! Each use decrypts the file and keeps nothing, so a write from the CLI
//! while the agent serves is seen by the next use.

mod file;
pub mod policy;
mod scrub;

pub use policy::{HostSpec, Origin};
pub use scrub::Scrubber;

use crate::sealed::Key;
use anyhow::{Context, Result, ensure};
use file::Stored;
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Write as _};
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use zeroize::Zeroizing;

pub const MIN_VALUE_BYTES: usize = crate::remote::redact::MIN_SECRET_BYTES;
pub const MAX_VALUE_BYTES: usize = 4_096;
const TEXT_CHARS: usize = 200;
const MAX_BINDINGS: usize = 16;
const SYSTEM_BINDS: [&str; 4] = ["/usr", "/etc", "/lib", "/lib64"];
const FORBIDDEN_HEADERS: [&str; 13] = [
    "connection",
    "content-length",
    "cookie",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "set-cookie",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultConfig {
    pub path: PathBuf,
    /// 32 random bytes, mode 0600, which `vault init` makes. Neither file
    /// may sit where a sandbox can read it.
    pub key_file: PathBuf,
}

impl VaultConfig {
    pub(crate) fn resolve(&mut self, base: &Path) {
        self.path = base.join(&self.path);
        self.key_file = base.join(&self.key_file);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SecretName(String);

impl TryFrom<String> for SecretName {
    type Error = String;
    fn try_from(name: String) -> Result<Self, String> {
        let word = name.len() <= 48
            && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if word {
            Ok(Self(name))
        } else {
            Err(format!(
                "{name:?} cannot name a vault item: use up to 48 of a-z, 0-9 and -, \
                 starting with a letter or a digit"
            ))
        }
    }
}

/// A secret value. Its `Debug` prints `SecretValue(..)`; it has no
/// `Display`, `Serialize` or `Clone`, and it is zeroed on drop. Every
/// accessor that yields a value is named `reveal…`, and
/// `tests/secret_flow.rs` keeps those names out of all other modules.
pub struct SecretValue(Zeroizing<String>);

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(..)")
    }
}

impl SecretValue {
    /// The CLI's only way in. `path` is read once and, when it is a regular
    /// file, deleted, even if the read fails; a device or a link such as
    /// `/dev/stdin` is left alone. `-` reads stdin. Neither may be a
    /// terminal: the vault never prompts. One trailing newline is dropped.
    pub fn read_once(path: &Path) -> Result<Self> {
        let text = if path == Path::new("-") {
            let mut stdin = std::io::stdin();
            ensure!(
                !stdin.is_terminal(),
                "the value comes on stdin or from a file, and stdin is a terminal; the vault \
                 never prompts"
            );
            let mut text = Zeroizing::new(String::new());
            stdin
                .read_to_string(&mut text)
                .context("cannot read the value from stdin")?;
            text
        } else {
            let shown = path.display();
            let regular = std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file());
            let result = read_file(path);
            if regular {
                let _ = std::fs::remove_file(path);
            }
            result.with_context(|| format!("cannot read the value from {shown}"))?
        };
        Self::parse(text)
    }

    fn parse(mut text: Zeroizing<String>) -> Result<Self> {
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        ensure!(
            text.len() >= MIN_VALUE_BYTES,
            "the value is shorter than {MIN_VALUE_BYTES} bytes, too short to scrub safely"
        );
        ensure!(
            text.len() <= MAX_VALUE_BYTES,
            "the value is longer than {MAX_VALUE_BYTES} bytes"
        );
        ensure!(
            !text.contains(['\0', '\r', '\n']),
            "the value holds a NUL, CR or LF"
        );
        Ok(Self(text))
    }

    fn reveal(&self) -> &str {
        &self.0
    }
}

/// Refused: a terminal, since the vault never prompts.
fn read_file(path: &Path) -> Result<Zeroizing<String>> {
    let mut file = std::fs::File::open(path)?;
    ensure!(
        !file.is_terminal(),
        "it is a terminal; the vault never prompts"
    );
    let mut text = Zeroizing::new(String::new());
    file.read_to_string(&mut text)?;
    Ok(text)
}

/// Whether a person must approve an item's first use on each Ticket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Approval {
    /// The first use on each Ticket asks in its thread.
    FirstUse,
    /// Unattended use, such as a schedule's, as the operator chose.
    None,
}

impl Approval {
    pub fn name(self) -> &'static str {
        match self {
            Self::FirstUse => "first-use",
            Self::None => "none",
        }
    }
}

impl FromStr for Approval {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        [Self::FirstUse, Self::None]
            .into_iter()
            .find(|approval| approval.name() == text)
            .ok_or_else(|| format!("{text:?} is not an approval: first-use or none"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyScheme {
    /// `Bearer <value>`.
    Bearer,
    /// The value as it is.
    Raw,
}

impl KeyScheme {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bearer => "bearer",
            Self::Raw => "raw",
        }
    }
}

impl FromStr for KeyScheme {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        [Self::Bearer, Self::Raw]
            .into_iter()
            .find(|scheme| scheme.name() == text)
            .ok_or_else(|| format!("{text:?} is not a scheme: bearer or raw"))
    }
}

/// The request header that carries a key, such as `authorization` or
/// `x-api-key`. Never a hop-by-hop, framing, host, cookie or proxy header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HeaderField(String);

impl TryFrom<String> for HeaderField {
    type Error = String;
    fn try_from(text: String) -> Result<Self, String> {
        let name = HeaderName::from_bytes(text.as_bytes())
            .map_err(|_| format!("{text:?} is not a header name"))?;
        if FORBIDDEN_HEADERS.contains(&name.as_str()) {
            return Err(format!("the {name} header cannot carry a key"));
        }
        Ok(Self(name.as_str().to_owned()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemKind {
    /// Filled only into a password input of a page on one of `sites`.
    Login {
        sites: Vec<Origin>,
        username: String,
        hint: String,
    },
    /// Sent only as `header` on requests to one of `hosts`.
    Key {
        hosts: Vec<HostSpec>,
        header: HeaderField,
        scheme: KeyScheme,
        about: String,
    },
}

impl ItemKind {
    fn noun(&self) -> &'static str {
        match self {
            Self::Login { .. } => "login",
            Self::Key { .. } => "key",
        }
    }
}

/// An item as `vault list` shows it: everything but its value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemInfo {
    pub name: SecretName,
    #[serde(flatten)]
    pub kind: ItemKind,
    /// Whose Tickets may use it, as the Policy names people
    /// (`slack:U024BE7LH`); empty means any requester. When set, they are
    /// also the only people its approval asks.
    pub users: BTreeSet<String>,
    pub approval: Approval,
    pub created_ms: u64,
    pub updated_ms: u64,
}

pub struct NewItem {
    pub name: SecretName,
    pub kind: ItemKind,
    pub users: BTreeSet<String>,
    pub approval: Approval,
    pub value: SecretValue,
}

impl NewItem {
    fn check(&self) -> Result<()> {
        for user in &self.users {
            ensure!(
                requester(user),
                "--for {user:?} is not a requester such as slack:U024BE7LH"
            );
        }
        let (bindings, notes) = match &self.kind {
            ItemKind::Login {
                sites,
                username,
                hint,
            } => {
                ensure!(
                    !username.is_empty() && note(username),
                    "the username must be 1 to {TEXT_CHARS} characters, none of them control \
                     characters"
                );
                (sites.len(), hint)
            }
            ItemKind::Key { hosts, about, .. } => {
                ensure!(
                    self.value
                        .reveal()
                        .bytes()
                        .all(|byte| byte == b' ' || byte.is_ascii_graphic()),
                    "a key's value must be printable ASCII, as a header carries it"
                );
                (hosts.len(), about)
            }
        };
        ensure!(
            (1..=MAX_BINDINGS).contains(&bindings),
            "a {} names 1 to {MAX_BINDINGS} places it may be used",
            self.kind.noun()
        );
        ensure!(
            note(notes),
            "the hint or about text must be at most {TEXT_CHARS} characters, none of them \
             control characters"
        );
        Ok(())
    }
}

fn note(text: &str) -> bool {
    text.chars().count() <= TEXT_CHARS && !text.chars().any(char::is_control)
}

fn requester(user: &str) -> bool {
    let Some((source, id)) = user.split_once(':') else {
        return false;
    };
    user.len() <= 128
        && !source.is_empty()
        && source
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && !id.is_empty()
        && !id.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UseContext {
    /// `Mandate.source` + `:` + `Mandate.requester.id`, as the Policy names
    /// people.
    pub requester: String,
    /// Where this vault is: `local` or a node's name.
    pub environment: String,
    pub grants: BTreeMap<SecretName, GrantState>,
    pub takes_approvals: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantState {
    Waiting,
    Granted,
    Denied,
}

/// The answer to "may this Ticket use this item for this, now?". A value
/// comes out only inside `Allowed`.
#[derive(Debug)]
pub enum Admission<T> {
    Allowed(T),
    /// A person must answer first. `secret` is `<environment>/<name>`, and
    /// `label` is built from the item's metadata only.
    NeedsApproval {
        secret: String,
        label: String,
        answerable_by: Vec<String>,
    },
    Refused(String),
}

impl<T> Admission<T> {
    fn map<U>(self, allowed: impl FnOnce(T) -> U) -> Admission<U> {
        match self {
            Self::Allowed(value) => Admission::Allowed(allowed(value)),
            Self::NeedsApproval {
                secret,
                label,
                answerable_by,
            } => Admission::NeedsApproval {
                secret,
                label,
                answerable_by,
            },
            Self::Refused(why) => Admission::Refused(why),
        }
    }
}

#[derive(Debug)]
pub struct LoginFill {
    pub username: String,
    /// Where the form may post: the login's sites.
    pub sites: Vec<Origin>,
    password: SecretValue,
}

impl LoginFill {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "browse::session's fill is its first caller")
    )]
    pub(crate) fn reveal_password(&self) -> &str {
        self.password.reveal()
    }
}

#[derive(Debug)]
pub struct KeyHeader {
    pub name: SecretName,
    hosts: Vec<HostSpec>,
    header: HeaderField,
    scheme: KeyScheme,
    value: SecretValue,
}

impl KeyHeader {
    pub fn admits(&self, url: &reqwest::Url) -> bool {
        policy::key_admits(&self.hosts, url)
    }

    /// The keyed request builder: the key's header, marked sensitive, goes
    /// on `request` when its own URL is one of the key's hosts, so each
    /// redirect hop is judged by where it goes.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "fetch_url's keyed requests are its first caller")
    )]
    pub(crate) fn reveal_into(&self, request: &mut reqwest::Request) {
        if !self.admits(request.url()) {
            return;
        }
        let text = Zeroizing::new(match self.scheme {
            KeyScheme::Bearer => format!("Bearer {}", self.value.reveal()),
            KeyScheme::Raw => self.value.reveal().to_owned(),
        });
        let (Ok(name), Ok(mut value)) = (
            HeaderName::from_bytes(self.header.as_str().as_bytes()),
            HeaderValue::from_str(&text),
        ) else {
            return;
        };
        value.set_sensitive(true);
        request.headers_mut().insert(name, value);
    }
}

pub struct Vault {
    path: PathBuf,
    key: Key,
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vault")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Vault {
    /// `vault init`: a new key file and an empty vault. An existing key or
    /// vault is kept, so a second run, or one after a crash, finishes the
    /// first.
    pub fn init(config: &VaultConfig, view: &SandboxView) -> Result<()> {
        let (path, key_file) = view.hidden_paths(config)?;
        let _lock = file::lock(&path)?;
        if !key_file.exists() {
            ensure!(
                !path.exists(),
                "the vault {} exists, but its key file {} does not; restore the key, or move \
                 the vault away to start over",
                path.display(),
                key_file.display()
            );
            Key::create(&key_file)?;
        }
        let key = Key::read(&key_file)?;
        if path.exists() {
            file::read(&path, &key)?;
        } else {
            file::create(&path, &key)?;
        }
        Ok(())
    }

    /// Refused: a key file a sandbox can see, or one that is a symlink, is
    /// open to group or others, or is not 32 bytes; a vault a sandbox can
    /// see, or one that does not open under the key.
    pub fn open(config: &VaultConfig, view: &SandboxView) -> Result<Self> {
        let (path, key_file) = view.hidden_paths(config)?;
        let key = Key::read(&key_file)?;
        file::read(&path, &key)?;
        Ok(Self { path, key })
    }

    /// Add an item, or with `replace` change one of the same kind.
    pub fn add(&self, item: NewItem, replace: bool) -> Result<()> {
        item.check()?;
        let now = crate::broker::now_ms();
        file::update(&self.path, &self.key, |contents| {
            let NewItem {
                name,
                kind,
                users,
                approval,
                value,
            } = item;
            let mut created_ms = now;
            if let Some(at) = contents.items.iter().position(|old| old.info.name == name) {
                let old = &contents.items[at].info;
                ensure!(
                    replace,
                    "the vault already holds {name}; add --replace to change it"
                );
                ensure!(
                    old.kind.noun() == kind.noun(),
                    "{name} is a {}; remove it first to make it a {}",
                    old.kind.noun(),
                    kind.noun()
                );
                created_ms = old.created_ms;
                contents.items.remove(at);
            }
            contents.items.push(Stored {
                info: ItemInfo {
                    name,
                    kind,
                    users,
                    approval,
                    created_ms,
                    updated_ms: now,
                },
                value,
            });
            contents
                .items
                .sort_by(|left, right| left.info.name.cmp(&right.info.name));
            Ok(())
        })
    }

    pub fn remove(&self, name: &SecretName) -> Result<bool> {
        file::update(&self.path, &self.key, |contents| {
            let before = contents.items.len();
            contents.items.retain(|item| &item.info.name != name);
            Ok(contents.items.len() < before)
        })
    }

    pub fn list(&self) -> Result<Vec<ItemInfo>> {
        let contents = file::read(&self.path, &self.key)?;
        Ok(contents.items.into_iter().map(|item| item.info).collect())
    }

    /// Admit `name` for a fill on a page whose origin is `page`, as the
    /// browser engine reports it, never as the model or the page says.
    pub fn admit_login(
        &self,
        name: &SecretName,
        page: &Origin,
        context: &UseContext,
    ) -> Result<Admission<LoginFill>> {
        let Some(Stored { info, value }) = self.find(name)? else {
            return Ok(Admission::Refused(format!(
                "there is no login {name} on {}",
                context.environment
            )));
        };
        let ItemKind::Login {
            sites, username, ..
        } = &info.kind
        else {
            return Ok(Admission::Refused(format!(
                "{name} is an API key, not a login"
            )));
        };
        if !policy::site_listed(sites, page) {
            return Ok(Admission::Refused(format!(
                "{page} is not one of {name}'s sites: {}",
                joined(sites, ", ")
            )));
        }
        let fill = LoginFill {
            username: username.clone(),
            sites: sites.clone(),
            password: value,
        };
        Ok(policy::use_rule(&info, context).map(|()| fill))
    }

    pub fn admit_key(
        &self,
        name: &SecretName,
        first: &reqwest::Url,
        context: &UseContext,
    ) -> Result<Admission<KeyHeader>> {
        let Some(Stored { info, value }) = self.find(name)? else {
            return Ok(Admission::Refused(format!(
                "there is no key {name} on {}",
                context.environment
            )));
        };
        let ItemKind::Key {
            hosts,
            header,
            scheme,
            ..
        } = &info.kind
        else {
            return Ok(Admission::Refused(format!(
                "{name} is a login, not an API key"
            )));
        };
        if !policy::key_admits(hosts, first) {
            return Ok(Admission::Refused(format!(
                "{name} goes only to {} over https, not to {first}",
                joined(hosts, ", ")
            )));
        }
        let key = KeyHeader {
            name: name.clone(),
            hosts: hosts.clone(),
            header: header.clone(),
            scheme: *scheme,
            value,
        };
        Ok(policy::use_rule(&info, context).map(|()| key))
    }

    /// A scrubber of every value the vault holds now. Build one for each
    /// output, so an item added since the last one is scrubbed too: a page
    /// can show any value, not only the one in use.
    pub fn scrubber(&self) -> Result<Scrubber> {
        Ok(Scrubber::new(&file::read(&self.path, &self.key)?.items))
    }

    fn find(&self, name: &SecretName) -> Result<Option<Stored>> {
        let contents = file::read(&self.path, &self.key)?;
        Ok(contents
            .items
            .into_iter()
            .find(|item| &item.info.name == name))
    }
}

pub struct SandboxView {
    visible: Vec<PathBuf>,
}

impl SandboxView {
    pub fn new(more: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            visible: SYSTEM_BINDS.iter().map(PathBuf::from).chain(more).collect(),
        }
    }

    /// Refused: `path`, after its symlinks, is under a visible path.
    pub fn hides(&self, path: &Path) -> Result<()> {
        let real = resolve_symlinks(path)?;
        for visible in &self.visible {
            let seen = resolve_symlinks(visible).unwrap_or_else(|_| visible.clone());
            ensure!(
                !real.starts_with(&seen) && !real.starts_with(visible),
                "{} is under {}, which a sandbox can read; keep it where no sandbox binds",
                path.display(),
                visible.display()
            );
        }
        Ok(())
    }

    /// The vault's and the key's absolute paths. Refused: either is under
    /// a visible path, or the vault is a symlink, whose writes would land
    /// at the link while the check judged its target.
    fn hidden_paths(&self, config: &VaultConfig) -> Result<(PathBuf, PathBuf)> {
        let path = std::path::absolute(&config.path)?;
        let key_file = std::path::absolute(&config.key_file)?;
        self.hides(&key_file)?;
        self.hides(&path)?;
        ensure!(
            !std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_symlink()),
            "the vault {} is a symlink",
            path.display()
        );
        Ok((path, key_file))
    }
}

fn resolve_symlinks(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut missing = Vec::new();
    let mut existing = absolute.as_path();
    loop {
        if let Ok(real) = existing.canonicalize() {
            return Ok(missing
                .iter()
                .rev()
                .fold(real, |real, part| real.join(part)));
        }
        missing.push(
            existing.file_name().with_context(|| {
                format!("{} holds .. after a missing directory", path.display())
            })?,
        );
        existing = existing
            .parent()
            .with_context(|| format!("{} has no existing directory", path.display()))?;
    }
}

pub fn table(items: &[ItemInfo]) -> String {
    let rows: Vec<[String; 8]> = items
        .iter()
        .map(|item| {
            let (kind, bindings, holder, remark) = match &item.kind {
                ItemKind::Login {
                    sites,
                    username,
                    hint,
                } => ("LOGIN", joined(sites, ","), username.clone(), hint),
                ItemKind::Key {
                    hosts,
                    header,
                    scheme,
                    about,
                } => {
                    let shown = match scheme {
                        KeyScheme::Bearer => format!("{header}: Bearer …"),
                        KeyScheme::Raw => format!("{header}: …"),
                    };
                    ("KEY", joined(hosts, ","), shown, about)
                }
            };
            let users = if item.users.is_empty() {
                "any requester".to_owned()
            } else {
                format!("for {}", joined(&item.users, ","))
            };
            let updated = crate::agent::utc::format(item.updated_ms);
            [
                kind.to_owned(),
                item.name.to_string(),
                bindings,
                holder,
                format!("{remark:?}"),
                item.approval.name().to_owned(),
                users,
                updated.get(..10).unwrap_or(&updated).to_owned(),
            ]
        })
        .collect();
    let mut widths = [0; 8];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut text = String::new();
    for row in &rows {
        let mut line = String::new();
        for (cell, width) in row.iter().zip(widths) {
            let _ = write!(line, "{cell:<width$}  ");
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

fn joined<T: Display>(items: impl IntoIterator<Item = T>, separator: &str) -> String {
    items
        .into_iter()
        .map(|item| item.to_string())
        .collect::<Vec<_>>()
        .join(separator)
}

macro_rules! text_newtype {
    ($($name:ident),*) => {$(
        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }
        impl Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl FromStr for $name {
            type Err = String;
            fn from_str(text: &str) -> Result<Self, String> {
                Self::try_from(text.to_owned())
            }
        }
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    )*};
}
use text_newtype;

text_newtype!(SecretName, HeaderField);

#[cfg(test)]
mod tests;
