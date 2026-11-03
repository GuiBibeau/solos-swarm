//! Lives at: src/browse.rs (`pub mod browse;` in src/lib.rs), with private
//! submodules src/browse/{session.rs, cdp.rs, lightpanda.rs, snapshot.rs}
//! and a `#[doc(hidden)] pub mod fake` (src/browse/fake.rs) for tests, as
//! `harness::scripted` is.
//!
//! The browser, as one deep module that runs unchanged on the agent host
//! (owned by the agent broker) and in `solos-swarm node serve` (owned by the
//! node daemon). Its public surface is four calls: `Browsers::new`, `act`,
//! `catalog`, `shutdown`, plus the trail key types. Behind it:
//!
//! - one Lightpanda process per Assignment, in its own bwrap sandbox that
//!   shares the host network (`--unshare-all --share-net --die-with-parent`),
//!   with a cleared environment, telemetry off, private networks blocked,
//!   core dumps off, and only the files it needs bound (not all of `/etc`);
//! - a private CDP client over the process's Unix socket (tokio-tungstenite
//!   `client_async` over `tokio::net::UnixStream`, the pinned crate);
//! - numbered elements per view, form detection, the login fill and its
//!   checks, key injection on `Fetch.requestPaused`;
//! - capture, scrubbing, the model's View, content-addressed blobs;
//! - signing every outcome with this machine's trail key.
//!
//! `cdp` and `lightpanda` are private modules, so no code outside `browse`
//! can send a CDP command: raw CDP (Runtime.evaluate, DOM reads of a filled
//! field) is unreachable from the broker by construction, not by review.
//! Every value `act` returns is built in one function, `session::release`,
//! which scrubs every string and every text blob before it digests and signs.
//! There is no other constructor of a signed step in the shell.

use crate::vault::{Catalog, Scrubber, UseContext, Vault};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solos_agent_core::model::{AssignmentId, TicketId};
use solos_agent_core::trail::{Digest, SignedStep, StepBody, TrailAction};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The signature domain of a trail step (audit.rs `signing_bytes`).
pub const TRAIL_DOMAIN: &str = "solos-swarm/trail-step/v1";
/// The most a View may be: the model reads it in every later turn.
pub const VIEW_BYTES: usize = 65_536;
pub const VIEW_TEXT_BYTES: usize = 24_576;
pub const VIEW_ELEMENTS: usize = 200;
/// Bounds on what one step stores; all of them fit a node frame (16 MiB).
pub const SNAPSHOT_MARKDOWN_BYTES: usize = 524_288;
pub const SNAPSHOT_HTML_BYTES: usize = 2_097_152;
pub const PNG_BYTES: usize = 4_194_304;
/// One action's longest wait for the page (navigation, network idle).
pub const ACTION_WAIT: Duration = Duration::from_secs(30);

/// `browser:` in agent.yaml and in node.yaml: one struct, two embeddings.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserConfig {
    /// The Lightpanda binary (Linux x86_64, glibc 2.35+). A separate
    /// program, exec'd, never linked (AGPL-3.0). `agent serve` and `node
    /// serve` run `lightpanda version` in the sandbox at start and refuse to
    /// start without one that answers.
    pub lightpanda: PathBuf,
    /// This machine's Ed25519 trail key (32-byte seed, 0600), from
    /// `solos-swarm browser init-key`. Hidden from every sandbox, as the
    /// vault key is.
    pub audit_key: PathBuf,
    /// Session directories and their CDP sockets. Default on the agent:
    /// `<state>/browser`. Required on a node (its work root's parent is
    /// shared by every Ticket, grounding §5). Never under a Ticket's `work/`.
    #[serde(default)]
    pub runtime_dir: Option<PathBuf>,
    /// Browsers alive at once on this machine; a call past it is refused
    /// (signed, with a reason) rather than queued.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// A browser with no call for this long is closed (signed `close`).
    #[serde(default = "default_idle_secs")]
    pub idle_secs: u64,
    /// prlimit --as for the whole browser.
    #[serde(default = "default_memory")]
    pub memory_bytes: u64,
    #[serde(default = "default_heap_mb")]
    pub v8_heap_mb: u32,
    /// Also capture each page's HTML (default) and a PNG (default off,
    /// best effort, and never in a session after a login fill: an image
    /// cannot be scrubbed).
    #[serde(default)]
    pub capture: Capture,
    /// Frames off by default (`--disable-subframes`): the main frame is the
    /// only document, so "the page's origin" has one meaning for a fill.
    #[serde(default)]
    pub subframes: bool,
    #[serde(default)]
    pub obey_robots: bool,
    /// Off: `--block-private-networks`, because this box also runs
    /// production services on loopback and private ranges. Tests turn it on
    /// for their local fixture server.
    #[serde(default)]
    pub allow_private_networks: bool,
    #[serde(default)]
    pub block_cidrs: Vec<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default = "default_bwrap")]
    pub bwrap: PathBuf,
    #[serde(default = "default_prlimit")]
    pub prlimit: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    #[serde(default = "yes")]
    pub html: bool,
    #[serde(default)]
    pub png: bool,
}
impl Default for Capture {
    fn default() -> Self {
        Self { html: true, png: false }
    }
}
fn yes() -> bool { true }
fn default_max_sessions() -> usize { 2 }
fn default_idle_secs() -> u64 { 1_800 }
fn default_memory() -> u64 { 1 << 30 }
fn default_heap_mb() -> u32 { 512 }
fn default_bwrap() -> PathBuf { PathBuf::from("/usr/bin/bwrap") }
fn default_prlimit() -> PathBuf { PathBuf::from("/usr/bin/prlimit") }

/// An Ed25519 public key as config and records write it: `ed25519:<64 hex>`.
/// Pinned in `nodes.allow.<name>.audit_key`; printed by `browser init-key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TrailKey(pub [u8; 32]);
impl TryFrom<String> for TrailKey {
    type Error = String;
    fn try_from(s: String) -> std::result::Result<Self, String> {
        todo!()
    }
}
impl From<TrailKey> for String {
    fn from(k: TrailKey) -> String {
        todo!()
    }
}

/// This machine's signer: `audit::CryptoSigner` from the seed file. Only
/// `session::release` signs; the type has no other caller.
pub struct TrailSigner(crate::audit::CryptoSigner);

impl TrailSigner {
    /// `browser init-key --out`: 32 random bytes, create_new, 0600. Prints
    /// the public key. Refuses an existing file.
    pub fn init(path: &Path, visible: &crate::vault::SandboxView) -> Result<TrailKey> {
        todo!()
    }
    pub fn load(path: &Path, visible: &crate::vault::SandboxView) -> Result<Self> {
        todo!("refuse symlink, group/other bits, wrong length, a sandbox-visible path")
    }
    pub fn public(&self) -> TrailKey {
        TrailKey(self.0.public_key())
    }
    fn sign(&self, body: StepBody) -> Result<SignedStep> {
        // let signed = self.0.sign(TRAIL_DOMAIN, body)?;
        // SignedStep { body: signed.payload, signer: TrailKey(signed.signer).into(), signature: hex(signed.signature) }
        todo!()
    }
}

/// Check a signed step under `trusted`, the way `audit::Signed::verify`
/// does (`verify_strict`, the signer must equal the trusted key). Used by
/// the agent at the boundary and by `agent trail`.
pub fn verify_step(step: &SignedStep, trusted: &TrailKey) -> Result<()> {
    todo!()
}

/// One browser call, as the agent broker sends it to wherever the
/// Ticket's browser is (in process, or in a `Call::Browse` frame).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowseCall {
    pub ticket: TicketId,
    pub assignment: AssignmentId,
    /// The core's number for this step, already recorded as asked.
    pub n: u32,
    pub action: TrailAction,
    /// For `Act { action: Type, text: Typed { digest, .. } }` only: the
    /// text, which must hash to `digest` (checked, else refused).
    #[serde(default)]
    pub typed: Option<String>,
    pub context: UseContext,
}

/// What a browser answered. Built by `session::release` on the machine
/// that ran it; on the agent it is untrusted until `agent::browser::verify`
/// has checked the signature under the pinned key, the step against the
/// asked action, and every blob against its digest.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Browsed {
    pub step: SignedStep,
    /// Content-addressed; each digest named in `step.body.outcome` is here.
    pub blobs: Vec<Blob>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blob {
    pub digest: Digest,
    pub kind: BlobKind,
    /// base64 on the wire.
    #[serde(with = "b64")]
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobKind {
    /// `{url,title,status,markdown,elements[,html]}`, scrubbed.
    Snapshot,
    /// The exact JSON the model got.
    View,
    Png,
    /// Text a worker typed (stored by the agent before the step is asked).
    Typed,
}

mod b64 {}

/// The parse at the tool boundary, shared by every caller: tool name and
/// arguments to the action the chain records and the browser performs.
/// `deny_unknown_fields` argument structs per tool, as `HandbackArgs` is.
///
/// ```text
/// browser_open  {url, key?}    url parses, http(s), has a host, <= 2048   -> Open { url, key }
/// browser_act   {action, element?, text?}
///     click|type|select need element; type needs text (<= 4000) -> Act { Typed { digest, chars } }, typed = Some(text)
///     select|press|scroll text <= 100                            -> Act { Named { text } }
///     back takes neither
/// browser_login {login, username_element?, password_element?, submit? = true} -> Login { .. }
/// browser_read  {part?, element?}  not both                                    -> Read { .. }
/// ```
pub fn action_of(tool: &str, args: Value) -> std::result::Result<(TrailAction, Option<String>), String> {
    todo!()
}

/// How a session's browser is reached. Tests use `Socket` with
/// `fake::FakeCdp`; everything else uses `Lightpanda`.
#[derive(Clone, Debug)]
pub enum Launcher {
    Lightpanda,
    #[doc(hidden)]
    Socket(PathBuf),
}

/// This machine's browsers, one per Assignment that has used one.
pub struct Browsers {
    config: BrowserConfig,
    /// `local` or this node's name: what `StepBody.environment` says.
    environment: String,
    vault: Option<Arc<Vault>>,
    signer: TrailSigner,
    runtime_dir: PathBuf,
    launcher: Launcher,
    /// Live sessions bounded by `max_sessions`.
    slots: Arc<tokio::sync::Semaphore>,
    /// One async lock per session: a session runs one step at a time, so
    /// concurrent tool calls (Claude's MCP JoinSet) queue here as well as at
    /// the seat.
    sessions: std::sync::Mutex<BTreeMap<AssignmentId, Arc<tokio::sync::Mutex<session::Session>>>>,
}

impl Browsers {
    /// Empty `runtime_dir` of what a killed run left (stale sockets would
    /// answer the next connect), check the binary, load nothing else.
    pub fn new(
        config: &BrowserConfig,
        environment: &str,
        runtime_dir: &Path,
        vault: Option<Arc<Vault>>,
        signer: TrailSigner,
        launcher: Launcher,
    ) -> Result<Self> {
        todo!()
    }

    pub fn key(&self) -> TrailKey {
        self.signer.public()
    }

    /// Perform one step and answer it, signed. Never an `Err`: a refusal, a
    /// browser crash, a missing slot are all signed outcomes, so every
    /// asked step on the chain gets an answer from the machine that was
    /// asked.
    ///
    /// ```text
    /// check typed hashes to the action's digest            else signed Refused
    /// session = sessions[assignment]
    ///   none and action is Close                           -> signed Closed { steps: 0 } (session "-")
    ///   none: try_acquire slot                             else signed Refused("this machine runs its most browsers ({max})")
    ///         launch (lightpanda::start or Launcher::Socket), cdp::connect, Target.createTarget about:blank,
    ///         attachToTarget(flatten), Page.enable, scrubber = vault.scrubber()
    /// lock session; session.last_n < n                     else signed Refused("step {n} is not after {last}")
    /// raw = match action:
    ///   Open { url, key }  -> session.open(url, key admitted via vault.admit_key(key, url, ctx))
    ///   Act { .. }         -> session.act(..)
    ///   Login { .. }       -> session.login(.., ctx)       (see session::Session::login)
    ///   Read { .. }        -> session.read(..)
    ///   Close { .. }       -> session.close(); remove from sessions; drop the slot
    /// a CDP or process failure: mark the session dead, remove it (the next call opens a new one)
    ///   and answer signed Refused("the browser stopped: {scrubbed why}")
    /// session::release(&session, &self.signer, &call, raw)
    /// ```
    pub async fn act(&self, call: BrowseCall) -> Browsed {
        todo!()
    }

    /// This machine's vault as one Ticket may see it here.
    pub async fn catalog(&self, context: &UseContext) -> Result<Catalog> {
        todo!("vault.catalog(context, &[\"browser_open\"]) or empty")
    }

    /// Kill every browser, signing nothing. The agent closes sessions with
    /// signed steps before it calls this (executor shutdown ends every
    /// Assignment first); this is the backstop, with `--die-with-parent`
    /// behind it for SIGKILL.
    pub async fn shutdown(&self) {
        todo!()
    }

    /// The node daemon: its connection ended, so every session ends with it
    /// ("no session outlives its connection", daemon.rs:499).
    pub async fn drop_all(&self) {
        todo!()
    }
}

/// Lives at src/browse/session.rs.
mod session {
    use super::*;
    use crate::vault::{KeyHeader, LoginFill, Origin};
    use solos_agent_core::trail::{Keyed, PageSeen, StepOutcome};

    /// One browser process and its page. No `Debug` derive: it holds a
    /// scrubber and admitted key headers.
    pub(super) struct Session {
        pub(super) id: String,
        pub(super) next_step: u32,
        pub(super) last_n: u32,
        process: Option<tokio::process::Child>,
        cdp: super::cdp::Cdp,
        page: String,
        scrubber: Scrubber,
        /// The latest view's numbering: element id -> backendNodeId, and
        /// the view generation it belongs to. Ids from an older view are
        /// refused.
        elements: Elements,
        /// Keys attached by `browser_open { key }`, for the session's life.
        keys: Vec<KeyHeader>,
        /// The top-level URL this session itself is navigating to, so its
        /// own navigation to a bound host may carry a key.
        navigating: Option<reqwest::Url>,
        keyed: BTreeMap<(String, String), u32>,
        /// After a login fill, no PNG is taken in this session.
        filled_once: bool,
        pub(super) last_used: std::time::Instant,
    }

    struct Elements;

    /// What a step produced before scrubbing and signing.
    pub(super) enum Raw {
        Page { snapshot: Snapshot, part: u32, target: Option<(String, String)>, filled: Vec<String> },
        NeedsApproval { secret: String, label: String },
        Refused { why: String },
        Closed { steps: u32 },
    }

    pub(super) struct Snapshot;

    impl Session {
        /// Navigate the main frame. A key, when named, was admitted by the
        /// vault for this URL first and is attached for the session:
        /// `Fetch.enable` with one `urlPattern` per bound host, so pages on
        /// other hosts never pause.
        pub(super) async fn open(&mut self, url: &str, key: Option<KeyHeader>) -> Raw {
            todo!()
        }

        /// The `Fetch.requestPaused` handler, on its own task, for the
        /// session's life:
        ///
        /// ```text
        /// top = Origin::of(main frame url)
        /// own = paused.resource_type == Document && paused.frame == main && Some(url) == self.navigating
        /// for key in keys: if key.admits_in_browser(&url, &top, own)
        ///     headers = paused.headers minus any of key's header name, plus key.header()  (value via expose: allowed site)
        ///     keyed[(key, host)] += 1
        ///     Fetch.continueRequest { requestId, headers }; return
        /// Fetch.continueRequest { requestId }        // unchanged, no key
        /// ```
        /// Each redirect hop arrives as its own paused request, so the check
        /// runs per hop (grounding §5). A hop to an unbound host does not
        /// match the patterns and never pauses.
        async fn on_paused(&mut self) {
            todo!()
        }

        pub(super) async fn act(&mut self, action: solos_agent_core::trail::PageAction, element: Option<u32>, text: Option<&str>) -> Raw {
            // click: LP.clickNode; type: DOM.focus + Input.insertText (refused into an
            // <input type=password>: typed text is never a password); select: the option by
            // label; press: Input.dispatchKeyEvent; scroll: LP.scrollNode; back:
            // Page.getNavigationHistory + navigateToHistoryEntry. Then wait for the page
            // (ACTION_WAIT) and capture.
            todo!()
        }

        /// The secret path. Every check is on the engine's facts, under the
        /// session lock, immediately before the fill:
        ///
        /// ```text
        /// page = Origin::of(Page.getFrameTree().frame.url)       // the engine's URL, not the model's or the page's text
        /// match vault.admit_login(login, &page, ctx):
        ///   Refused(why)                   -> Raw::Refused(why)
        ///   NeedsApproval { secret, label } -> Raw::NeedsApproval
        ///   Allowed(fill) =>
        /// (user, pass) = the elements named, or LP.detectForms' login form on the main document
        /// pass: DOM.describeNode -> nodeName INPUT, attribute type=password      else Refused("…not a password field")
        /// user: absent, or INPUT of type text|email|tel                           else Refused
        /// form = the password field's form; its action resolves to an origin in fill.sites
        ///                                                                        else Refused("the form posts to {o}, not to {login}'s site")
        /// re-read the main frame URL; still `page`                               else Refused("the page moved before the fill")
        /// LP.fillNode(user, fill.username); LP.fillNode(pass, fill.password.expose())
        /// filled_once = true
        /// submit: LP.clickNode(the form's submit control) or Enter in `pass`; wait for the page
        /// capture (password inputs masked structurally; scrubber removes any echo) -> Raw::Page { filled: ["username","password"] }
        /// ```
        /// The password goes only into a password input of the login's own
        /// site whose form posts to that site, so a prompt-injected worker
        /// cannot fill it into a comment box on the right site or a form
        /// that posts elsewhere.
        pub(super) async fn login(&mut self, vault: &Vault, login: &str, user_el: Option<u32>, pass_el: Option<u32>, submit: bool, ctx: &UseContext) -> Raw {
            // uses vault::{LoginFill, Origin}
            todo!()
        }

        pub(super) async fn read(&mut self, part: Option<u32>, element: Option<u32>) -> Raw {
            todo!("capture again; the View is that part, or the element's full text bounded by VIEW_TEXT_BYTES")
        }

        pub(super) async fn close(&mut self) -> Raw {
            todo!("Browser.close; wait for the process; Raw::Closed { steps: next_step + 1 }")
        }
    }

    /// The one exit. Order matters: scrub, then digest, then sign.
    ///
    /// ```text
    /// Raw::Page { snapshot, .. }:
    ///   snapshot_json = snapshot as JSON; scrubber.clean_json(&mut snapshot_json)   (markdown, element names, html, url, title)
    ///   view_json     = snapshot::view(&snapshot_json, part, logins_for(origin), call.n); scrubber.clean_json(&mut view_json)
    ///   bound view_json to VIEW_BYTES (cut text, then elements; say so in the view)
    ///   blobs = [Snapshot(bytes), View(bytes), Png? (only if capture.png && !filled_once)]
    ///   outcome = Page(PageSeen { url, status, title, snapshot: Digest::of, view: Digest::of, png, target, filled, keyed: drain counts })
    /// Raw::Refused { why } -> Refused { why: scrubber.clean(why) cut to 300 }
    /// body = StepBody { v: STEP_VERSION, ticket, assignment, n: call.n, environment, session: id,
    ///                   session_step: next_step++, at_ms: now, action: call.action, outcome }
    /// as Value -> scrubber.clean_json -> back to StepBody    (the URL of a GET login form, say)
    /// Browsed { step: signer.sign(body), blobs }
    /// ```
    pub(super) fn release(session: &mut Session, signer: &TrailSigner, call: &BrowseCall, raw: Raw) -> Browsed {
        // builds trail::{PageSeen, Keyed, StepOutcome} and signs the StepBody
        todo!()
    }
}

/// Lives at src/browse/cdp.rs. Private to `browse`.
mod cdp {
    use serde_json::Value;
    use std::path::Path;
    use std::time::Duration;

    /// A CDP connection: requests by id, replies matched by a reader task,
    /// events on a channel. Method names are `&'static str`, so no method
    /// can come from a model's or a page's text; the module offers no
    /// `Runtime.evaluate` call at all.
    pub(super) struct Cdp;

    pub(super) enum Event {
        RequestPaused { id: String, url: String, resource_type: String, frame: String, headers: Vec<(String, String)> },
        LoadEventFired,
        ResponseReceived { frame: String, status: u16, url: String },
        DialogOpening,
        Detached,
    }

    impl Cdp {
        /// `UnixStream::connect(socket)`, then `tokio_tungstenite::client_async("ws://localhost/", stream)`
        /// (Lightpanda answers the upgrade on the socket; `ws+unix://<path>:/`).
        /// Messages above `--cdp-max-message-size` are refused by both ends.
        pub(super) async fn connect(socket: &Path) -> anyhow::Result<Self> {
            todo!()
        }
        pub(super) async fn call(&self, session: Option<&str>, method: &'static str, params: Value, wait: Duration) -> anyhow::Result<Value> {
            todo!()
        }
        pub(super) async fn next_event(&self) -> Option<Event> {
            todo!()
        }
    }
}

/// Lives at src/browse/lightpanda.rs. Private to `browse`.
mod lightpanda {
    use super::BrowserConfig;
    use std::path::Path;

    /// The browser's sandbox. The only caller of the new `Bwrap::share_net`
    /// (a source-scan test pins that; `code.rs`'s golden test still forbids
    /// `--share-net` for run_code).
    ///
    /// ```text
    /// prlimit --as=<memory_bytes> --core=0 --nofile=1024 --nproc=256 --
    /// bwrap --unshare-all --share-net --die-with-parent --new-session --clearenv
    ///   --ro-bind /usr /usr --ro-bind-try /lib /lib --ro-bind-try /lib64 /lib64
    ///   --ro-bind-try /etc/ssl /etc/ssl --ro-bind-try /etc/ca-certificates /etc/ca-certificates
    ///   --ro-bind-try /etc/resolv.conf /etc/resolv.conf --ro-bind-try /etc/hosts /etc/hosts
    ///   --ro-bind-try /etc/nsswitch.conf /etc/nsswitch.conf      (not all of /etc: no key under it is visible)
    ///   --proc /proc --dev /dev --tmpfs /tmp
    ///   --ro-bind <lightpanda> /lightpanda --bind <session dir> /s
    ///   --setenv LIGHTPANDA_DISABLE_TELEMETRY true --setenv HOME /tmp
    ///   /lightpanda serve --unix-socket /s/cdp.sock --cdp-max-connections 1
    ///     --cdp-max-message-size 16777216 --http-timeout 20000 --http-connect-timeout 10000
    ///     --http-max-response-size 16777216 --v8-max-heap-mb <v8_heap_mb> --watchdog-ms 60000
    ///     [--block-private-networks] [--block-cidrs ...] [--disable-subframes] --disable-workers
    ///     [--obey-robots] [--user-agent ...]
    /// stdin null; stdout null; stderr piped into a 16 KiB ring, scrubbed before it is ever shown
    /// kill_on_drop(true)
    /// ```
    /// The session dir is `<runtime_dir>/<8 hex>/`; the outside socket path
    /// is checked against `sockaddr_un`'s 108 bytes with a clear error
    /// (grounding gotcha 10). No proxy variable, token or key reaches the
    /// environment (`--clearenv`).
    pub(super) fn command(config: &BrowserConfig, session_dir: &Path) -> tokio::process::Command {
        todo!()
    }

    /// Wait (bounded, 10 s) for the socket to accept.
    pub(super) async fn ready(socket: &Path) -> anyhow::Result<()> {
        todo!()
    }
}

/// Lives at src/browse/snapshot.rs. Private to `browse`.
mod snapshot {
    // capture(cdp, page, capture) -> Snapshot:
    //   Page.getFrameTree -> url; LP.getPageStatus / the last main-frame ResponseReceived -> status;
    //   title; LP.getMarkdown (bounded SNAPSHOT_MARKDOWN_BYTES); LP.getInteractiveElements
    //   (role, name, backendNodeId; at most 500); DOM.getOuterHTML of the document when
    //   capture.html (bounded SNAPSHOT_HTML_BYTES); Page.captureScreenshot when allowed.
    //   Structural masking first: the value of every <input type=password> becomes
    //   "[hidden]" in elements and HTML, whatever the scrubber later finds.
    //
    // view(snapshot, part, logins, n) -> serde_json::Value: the View documented in
    //   01_registry.rs, derived only from the scrubbed snapshot, deterministic, so a
    //   verifier can re-derive it and compare with the View blob.
}

/// Tests: a CDP server on a Unix socket (tokio UnixListener +
/// tokio_tungstenite::accept_async) with a small page model (frames, forms,
/// inputs with types, navigation, paused requests), which records every
/// method it receives. `Browsers::new(.., Launcher::Socket(fake.socket()))`.
#[doc(hidden)]
pub mod fake {
    pub struct FakeCdp;
}

#[cfg(test)]
mod tests {
    // Against fake::FakeCdp, no network:
    // - open / act / read produce signed steps that verify_step accepts under key();
    //   session_step counts 0,1,2; a new session after a close starts at 0 with a new id.
    // - login fills only on an exact site origin; refuses http when the site is https,
    //   a subdomain, a non-password target, a text-area target, a form posting off-site,
    //   a page that navigated between check and fill (the fake moves it); fills nothing then.
    // - a filled value echoed by the fake in markdown, element names, HTML, a URL query
    //   (percent-encoded) and a title never appears in any blob, view or body.
    // - no PNG after a fill; no Runtime.* method is ever sent (the fake fails on one).
    // - a key header goes on paused requests to its hosts while the top origin is bound or
    //   on the session's own navigation, and not on a request from a page on another site.
    // - typed text whose digest does not match the action is refused before any CDP call.
    // - max_sessions: the third Assignment gets a signed refusal, not a queue.
    // Opt-in, real Lightpanda (tests/browse_lightpanda.rs, #[ignore], SOLOS_LIGHTPANDA=path,
    //   on the Linux box): a local hyper fixture with a login form, allow_private_networks,
    //   pins the LP.* shapes the fake mimics.
}
