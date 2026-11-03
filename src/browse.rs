//! A headless browser for agent mode whose every step is signed by the
//! machine that ran it.
//!
//! Each Assignment that browses gets one Lightpanda process in its own
//! sandbox, driven over CDP on a Unix socket. `cdp` and `lightpanda` are
//! private, so no code outside `browse` can send a CDP command, and the
//! client has no `Runtime` method. Everything [`Browsers::act`] returns is
//! built by `session::release`, which scrubs every vault value out of it,
//! then digests and signs.

mod cdp;
#[doc(hidden)]
pub mod fake;
mod lightpanda;
mod session;
mod snapshot;

use crate::audit::{CryptoSigner, Signed};
use crate::vault::{ItemInfo, SandboxView, UseContext, Vault};
use anyhow::{Result, ensure};
use serde::Deserialize;
use session::{Raw, Session};
use solos_agent_core::model::{AssignmentId, TicketId};
use solos_agent_core::trail::{
    ActText, Digest, PageAction, Pending, SignedStep, StepOutcome, TrailAction, TrailKey, answers,
};
use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// The signature domain of a trail step (`audit::signing_bytes`).
pub const TRAIL_DOMAIN: &str = "solos-swarm/trail-step/v1";
/// The most a View may be: the model reads it in every later turn.
pub const VIEW_BYTES: usize = 65_536;
pub const VIEW_TEXT_BYTES: usize = 24_576;
pub const VIEW_ELEMENTS: usize = 200;
/// Bounds on what one step stores; all of them fit a node frame (16 MiB).
pub const SNAPSHOT_MARKDOWN_BYTES: usize = 524_288;
pub const SNAPSHOT_HTML_BYTES: usize = 2_097_152;
/// One action's longest wait for the page, and for any one CDP answer.
pub const ACTION_WAIT: Duration = Duration::from_secs(30);
/// The most the browser reads of any one HTTP response.
const RESPONSE_BYTES: usize = 16_777_216;
/// The session a step names when no browser ran it: a refusal before one
/// started, or a close with nothing open.
pub const NO_SESSION: &str = "-";
/// `sockaddr_un.sun_path` is 108 bytes, with a trailing NUL.
const SUN_PATH_LIMIT: usize = 108;

/// `browser:` in agent.yaml and in node.yaml.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserConfig {
    /// The Lightpanda binary. A separate program, run in its own sandbox
    /// and never linked (AGPL-3.0).
    pub lightpanda: PathBuf,
    /// This machine's trail key, from `solos-swarm browser init-key`.
    pub audit_key: PathBuf,
    /// Session directories and their CDP sockets. Default on the agent:
    /// `<state>/browser`.
    #[serde(default)]
    pub runtime_dir: Option<PathBuf>,
    /// Browsers alive at once on this machine. A call past it is refused,
    /// signed, rather than queued.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// A browser with no call for this long is closed by the agent.
    #[serde(default = "default_idle_secs")]
    pub idle_secs: u64,
    /// `prlimit --as`. V8 reserves about 40 GiB of address space before it
    /// loads a page, so this bounds runaway mappings, and `v8_heap_mb`
    /// bounds the JavaScript heap.
    #[serde(default = "default_address_space_bytes")]
    pub address_space_bytes: u64,
    #[serde(default = "default_v8_heap_mb")]
    pub v8_heap_mb: u32,
    /// Off: frames are not loaded, so a page's main document is the only
    /// document a fill can target.
    #[serde(default)]
    pub subframes: bool,
    /// Off: `--block-private-networks`, because this box also runs
    /// production services on loopback and private ranges.
    #[serde(default)]
    pub allow_private_networks: bool,
    #[serde(default = "default_bwrap")]
    pub bwrap: PathBuf,
    #[serde(default = "default_prlimit")]
    pub prlimit: PathBuf,
}

fn default_max_sessions() -> usize {
    2
}
fn default_idle_secs() -> u64 {
    1_800
}
fn default_address_space_bytes() -> u64 {
    64 << 30
}
fn default_v8_heap_mb() -> u32 {
    512
}
fn default_bwrap() -> PathBuf {
    PathBuf::from("/usr/bin/bwrap")
}
fn default_prlimit() -> PathBuf {
    PathBuf::from("/usr/bin/prlimit")
}

/// This machine's trail signer: `audit::CryptoSigner` from a 32-byte seed
/// file.
pub struct TrailSigner(CryptoSigner);

impl TrailSigner {
    /// `browser init-key`: a new seed at `path`, mode 0600, where no
    /// sandbox can read it. An existing file is refused.
    pub fn init(path: &Path, view: &SandboxView) -> Result<TrailKey> {
        view.hides(path)?;
        ensure!(
            std::fs::symlink_metadata(path).is_err(),
            "{} exists, and a trail key is never replaced",
            path.display()
        );
        crate::sealed::create_key_file(path)?;
        Ok(Self::load(path, view)?.public())
    }

    /// Refused: a path a sandbox can see, a symlink, a file open to group or
    /// others, or one that is not 32 bytes.
    pub fn load(path: &Path, view: &SandboxView) -> Result<Self> {
        view.hides(path)?;
        let seed = crate::sealed::read_key_file(path)?;
        Ok(Self(CryptoSigner::from_seed(*seed)))
    }

    pub fn public(&self) -> TrailKey {
        trail_key(&self.0.public_key())
    }
}

fn trail_key(public: &[u8; 32]) -> TrailKey {
    TrailKey::try_from(format!("ed25519:{}", hex(public)))
        .expect("an Ed25519 public key is 32 bytes")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(digits: &str) -> Vec<u8> {
    digits
        .as_bytes()
        .chunks(2)
        .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// Check `browsed` as the answer to `call`: signed by `trusted`, answering
/// the action that was asked, and carrying exactly the blobs its outcome
/// names, each matching its digest.
pub fn verify_step(call: &BrowseCall, browsed: &Browsed, trusted: &TrailKey) -> Result<(), String> {
    let step = &browsed.step;
    if step.signer != *trusted {
        return Err(format!("signed by {}, not by {trusted}", step.signer));
    }
    let key: [u8; 32] = unhex(trusted.digits())
        .try_into()
        .map_err(|_| "the trusted key is not 32 bytes".to_owned())?;
    let signed = Signed {
        payload: &step.body,
        signer: key,
        signature: unhex(step.signature.as_str()),
    };
    signed
        .verify(TRAIL_DOMAIN, &key)
        .map_err(|_| "the signature does not verify".to_owned())?;
    let asked = Pending {
        n: call.n,
        action: call.action.clone(),
        asked_ms: 0,
    };
    answers(&asked, &call.ticket, &call.assignment, step)?;
    let named = match &step.body.outcome {
        StepOutcome::Page(page) => vec![
            (BlobKind::Snapshot, &page.snapshot),
            (BlobKind::View, &page.view),
        ],
        _ => Vec::new(),
    };
    if browsed.blobs.len() != named.len() {
        return Err(format!(
            "the step names {} blobs and carries {}",
            named.len(),
            browsed.blobs.len()
        ));
    }
    for (kind, digest) in named {
        let matches = browsed.blobs.iter().any(|blob| {
            blob.kind == kind && blob.digest == *digest && Digest::of(&blob.bytes) == *digest
        });
        if !matches {
            return Err(format!("no {kind:?} blob matches {digest}"));
        }
    }
    Ok(())
}

/// One browser step, as the agent asks for it once the core has recorded
/// it as asked.
#[derive(Clone, Debug)]
pub struct BrowseCall {
    pub ticket: TicketId,
    pub assignment: AssignmentId,
    /// The core's number for this step.
    pub n: u32,
    pub action: TrailAction,
    /// For `Act { action: Type, text: Typed { digest, chars } }` only: the
    /// text, which must hash to `digest`.
    pub typed: Option<String>,
    pub context: UseContext,
}

/// A browser's answer. Untrusted until [`verify_step`] accepts it.
#[derive(Clone, Debug)]
pub struct Browsed {
    pub step: SignedStep,
    /// Each digest the step's outcome names, with its bytes.
    pub blobs: Vec<Blob>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blob {
    pub digest: Digest,
    pub kind: BlobKind,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobKind {
    /// The page as captured, scrubbed: `{url, title, status, markdown,
    /// elements, html}`.
    Snapshot,
    /// The exact JSON the model got.
    View,
}

/// How a session's browser is reached.
#[derive(Clone, Debug)]
pub enum Launcher {
    /// `lightpanda serve` in its own sandbox, one per session.
    Lightpanda,
    /// A CDP server that already listens, such as [`fake::FakeCdp`].
    #[doc(hidden)]
    Socket(PathBuf),
}

/// This machine's browsers, one per Assignment that has used one.
pub struct Browsers {
    config: BrowserConfig,
    environment: String,
    runtime_dir: PathBuf,
    vault: Option<Arc<Vault>>,
    signer: TrailSigner,
    launcher: Launcher,
    sessions: Mutex<BTreeMap<AssignmentId, Arc<tokio::sync::Mutex<Slot>>>>,
}

enum Slot {
    Fresh,
    Live(Box<Session>),
    Gone,
}

impl Browsers {
    /// Refused: a socket path a Unix socket cannot have, or a missing
    /// program. Session directories a killed run left are removed.
    pub fn new(
        config: &BrowserConfig,
        environment: &str,
        runtime_dir: &Path,
        vault: Option<Arc<Vault>>,
        signer: TrailSigner,
        launcher: Launcher,
    ) -> Result<Self> {
        ensure!(
            config.max_sessions > 0,
            "browser.max_sessions must be at least 1"
        );
        let runtime_dir = std::path::absolute(runtime_dir)?;
        let socket = match &launcher {
            Launcher::Lightpanda => {
                for program in [&config.lightpanda, &config.bwrap, &config.prlimit] {
                    ensure!(program.is_file(), "{} is not a file", program.display());
                }
                lightpanda::socket(&runtime_dir.join(session::id_placeholder()))
            }
            Launcher::Socket(socket) => socket.clone(),
        };
        let bytes = socket.as_os_str().as_bytes();
        ensure!(
            !bytes.contains(&b':'),
            "the browser's socket {} holds ':', which Lightpanda refuses in --unix-socket",
            socket.display()
        );
        ensure!(
            bytes.len() < SUN_PATH_LIMIT,
            "the browser's socket {} is {} bytes, and a Unix socket path is at most {}; \
             set browser.runtime_dir to a shorter directory",
            socket.display(),
            bytes.len(),
            SUN_PATH_LIMIT - 1
        );
        lightpanda::clear_stale_sessions(&runtime_dir)?;
        Ok(Self {
            config: config.clone(),
            environment: environment.to_owned(),
            runtime_dir,
            vault,
            signer,
            launcher,
            sessions: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn key(&self) -> TrailKey {
        self.signer.public()
    }

    /// Perform one step and answer it, signed. Never an error: a refusal,
    /// a missing slot and a browser that stopped are all signed outcomes,
    /// so every asked step gets an answer from the machine that was asked.
    pub async fn act(&self, call: BrowseCall) -> Browsed {
        if let Err(why) = typed_text(&call) {
            return self.answer(&call, None, Raw::Refused(why));
        }
        loop {
            let slot = match self.slot(&call) {
                Ok(slot) => slot,
                Err(raw) => return self.answer(&call, None, raw),
            };
            let mut held = slot.lock().await;
            let mut session = match std::mem::replace(&mut *held, Slot::Gone) {
                Slot::Gone => {
                    self.forget(&call.assignment, &slot);
                    continue;
                }
                Slot::Live(session) => session,
                Slot::Fresh => {
                    match Session::start(&self.config, &self.runtime_dir, &self.launcher).await {
                        Ok(session) => Box::new(session),
                        Err(why) => {
                            self.forget(&call.assignment, &slot);
                            let why = format!("the browser did not start: {why:#}");
                            return self.answer(&call, None, Raw::Refused(why));
                        }
                    }
                }
            };
            let raw = session.step(self.vault.as_deref(), &call).await;
            let ended = matches!(raw, Raw::Closed(_));
            let browsed = self.answer(&call, Some(&mut *session), raw);
            if ended {
                session.end().await;
                self.forget(&call.assignment, &slot);
            } else {
                *held = Slot::Live(session);
            }
            return browsed;
        }
    }

    /// The vault items one Ticket may use on this machine, without values.
    pub fn catalog(&self, context: &UseContext) -> Result<Vec<ItemInfo>> {
        catalog(self.vault.as_deref(), context)
    }

    /// Kill every browser, signing nothing. The agent closes sessions with
    /// signed steps before this; it is the backstop, with
    /// `--die-with-parent` behind it.
    pub async fn shutdown(&self) {
        let slots =
            std::mem::take(&mut *self.sessions.lock().unwrap_or_else(PoisonError::into_inner));
        for slot in slots.into_values() {
            if let Slot::Live(session) = std::mem::replace(&mut *slot.lock().await, Slot::Gone) {
                session.end().await;
            }
        }
    }

    fn slot(&self, call: &BrowseCall) -> Result<Arc<tokio::sync::Mutex<Slot>>, Raw> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = sessions.get(&call.assignment) {
            return Ok(Arc::clone(slot));
        }
        if let TrailAction::Close { reason } = call.action {
            return Err(Raw::Closed(reason));
        }
        if sessions.len() >= self.config.max_sessions {
            return Err(Raw::Refused(format!(
                "this machine already runs its most browsers ({})",
                self.config.max_sessions
            )));
        }
        let slot = Arc::new(tokio::sync::Mutex::new(Slot::Fresh));
        sessions.insert(call.assignment.clone(), Arc::clone(&slot));
        Ok(slot)
    }

    fn forget(&self, assignment: &AssignmentId, slot: &Arc<tokio::sync::Mutex<Slot>>) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if sessions
            .get(assignment)
            .is_some_and(|held| Arc::ptr_eq(held, slot))
        {
            sessions.remove(assignment);
        }
    }

    fn answer(&self, call: &BrowseCall, session: Option<&mut Session>, raw: Raw) -> Browsed {
        session::release(
            &self.signer,
            &self.environment,
            self.vault.as_deref(),
            call,
            session,
            raw,
        )
    }
}

fn catalog(vault: Option<&Vault>, context: &UseContext) -> Result<Vec<ItemInfo>> {
    let Some(vault) = vault else {
        return Ok(Vec::new());
    };
    Ok(vault
        .list()?
        .into_iter()
        .filter(|item| item.users.is_empty() || item.users.contains(&context.requester))
        .collect())
}

fn typed_text(call: &BrowseCall) -> Result<(), String> {
    let typing = match &call.action {
        TrailAction::Act {
            action: PageAction::Type,
            text: Some(ActText::Typed { digest, chars }),
            ..
        } => Some((digest, *chars)),
        _ => None,
    };
    match (typing, &call.typed) {
        (Some((digest, chars)), Some(text)) => {
            let counted = u32::try_from(text.chars().count()).ok();
            if Digest::of(text.as_bytes()) == *digest && counted == Some(chars) {
                Ok(())
            } else {
                Err("the typed text does not match the digest the step was asked with".into())
            }
        }
        (Some(_), None) => Err("the typed text did not come with the step".into()),
        (None, Some(_)) => Err("typed text came with a step that types nothing".into()),
        (None, None) => Ok(()),
    }
}
