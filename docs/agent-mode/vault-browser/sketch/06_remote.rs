//! Lives at: src/remote/wire.rs (protocol 2), src/remote/listener.rs
//! (`RemoteNode` as a `BrowserPlace`), src/remote/daemon.rs (the node runs
//! `browse::Browsers` with its own vault and trail key),
//! src/agent/config/nodes.rs (`audit_key`).
//!
//! A node's browser is the same `browse` module as the agent host's. What
//! crosses the wire is the domain call (`BrowseCall`: an action already
//! recorded as asked, plus the Ticket's use context) and the signed answer
//! (`Browsed`). The node's vault values, its trail key and its CDP socket
//! never leave the node; the blobs it sends were scrubbed there.
//!
//! "The node that runs the browser is the node that signs its trail": the
//! node signs with its own key, and the agent checks it against the key the
//! operator pinned in agent.yaml, not against anything the node says about
//! itself.

use crate::browse::{BrowseCall, Browsed, TrailKey};
use crate::vault::{Catalog, UseContext};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// src/remote/wire.rs
// ---------------------------------------------------------------------------

/// Protocol 2 adds the browser calls. The agent accepts a hello of 1 or 2
/// (`listener.rs:174` becomes `(1..=PROTOCOL).contains(&hello.protocol)`)
/// and never sends a v2 call on a v1 link, so a node that has not upgraded
/// keeps running run_code and jobs as before, with no browser. A v2 node
/// dialing a v1 agent is refused by that agent, as any mismatch is today.
pub const PROTOCOL: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub name: String,
    pub protocol: u32,
    /// v2: this node has `browser:` in its node.yaml and its Lightpanda
    /// answered `version`. A v1 hello has no such field.
    #[serde(default)]
    pub browser: bool,
}

/// The new calls: in the repo these are two more variants of `Call`, beside
/// Run, JobStart, JobWait, JobStop and Ping (and two of `Answer`); shown as
/// their own enums here only to keep the sketch short.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CallV2 {
    /// One step. `call.n` is already on the agent's chain as asked.
    /// Answered with `Answer::Browsed`, or `Answer::Refused` only when the
    /// node has no browser at all.
    Browse { call: BrowseCall },
    /// The node's vault as this Ticket may see it. Answered with
    /// `Answer::Catalog`.
    VaultList { context: UseContext },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AnswerV2 {
    Browsed(Browsed),
    Catalog(Catalog),
}

// A `Browsed` frame is bounded by construction: View 64 KiB, snapshot
// markdown 512 KiB and HTML 2 MiB, PNG 4 MiB, base64 overhead 4/3: under
// 9 MiB, inside MAX_FRAME (16 MiB). `browse` enforces the bounds before it
// signs, and `wire::encode` still refuses anything larger.
//
// The longest a node may take on a Browse: BROWSE_WAIT = 90 s (one action's
// page wait is 30 s; a fresh browser's start adds up to 10 s). CALL_WAIT
// (30 s) stays for the other calls.

// ---------------------------------------------------------------------------
// src/remote/listener.rs
// ---------------------------------------------------------------------------

/// `Link` gains `protocol: u32` and `browser: bool` from the hello.
/// `RemoteNode` gains `audit_key: Option<TrailKey>` from `AllowedNode`.
///
/// `impl BrowserPlace for RemoteNode`, offered by `Environments` only for
/// nodes with a pinned `audit_key`:
///
/// ```text
/// name()     -> self.name
/// trusted()  -> self.audit_key (Some, or the node is not a place)
/// ready()    -> live link, protocol >= 2, link.browser            else Offline("node laptop has no browser" | "… is offline")
/// act(call)  -> link.call(CallV2::Browse { call }, BROWSE_WAIT)
///                 Answer::Browsed(b)    -> Ok(b)                  (verified by the agent, not here)
///                 Answer::Refused{why}  -> Err(Offline(why))
///                 link lost / timeout   -> Err(Offline("node laptop was lost during the step"))
/// catalog(c) -> link.call(CallV2::VaultList { context: c }, CALL_WAIT) -> Answer::Catalog
/// ```
pub struct RemoteNodeAdditions;

// ---------------------------------------------------------------------------
// src/remote/daemon.rs
// ---------------------------------------------------------------------------

/// `NodeConfig` gains (both optional; `deny_unknown_fields` stays):
///
/// ```yaml
/// vault: {path: vault.enc, key_file: /var/lib/solos-node-keys/vault.key}
/// browser:
///   lightpanda: /opt/lightpanda/lightpanda
///   audit_key: /var/lib/solos-node-keys/audit.key
///   runtime_dir: /var/lib/solos-node/browser      # required here
/// ```
///
/// `Node::new` opens them with the `SandboxView` of the node's own sandbox
/// (its `/usr`, `/etc`, `/lib*`, `sandbox.mounts`, `uv_root`, `work_root`),
/// so neither key can sit where the node's run_code can read it, and builds
/// `Browsers::new(&browser, &config.name, runtime_dir, vault, signer,
/// Launcher::Lightpanda)`. The hello says `browser: true` iff it exists.
///
/// `answer()` gains:
///
/// ```text
/// CallV2::Browse { call }      -> host.browsers? .act(call).await -> Answer::Browsed
///                                 (no browser -> Answer::Refused("this node has no browser"))
/// CallV2::VaultList { context } -> host.browsers?.catalog(&context) -> Answer::Catalog
/// ```
///
/// and `serve_connection`'s end calls `browsers.drop_all()` beside stopping
/// jobs: no session outlives its connection. Nothing the node redacts with
/// `remote/redact.rs` changes: vault values are scrubbed by `browse`, and
/// they are not node `secrets` (those still go into the run_code sandbox,
/// as P9 built them).
pub struct DaemonAdditions;

/// `solos-swarm node init` also writes `audit.key` beside the TLS key and
/// prints both lines the agent pins:
///
/// ```text
/// fingerprint: sha256:3b…
/// audit_key:   ed25519:5d…
/// ```
/// (`solos-swarm browser init-key --out` does the same for an existing node.)
pub struct NodeInit;

// ---------------------------------------------------------------------------
// src/agent/config/nodes.rs
// ---------------------------------------------------------------------------

/// `AllowedNode` gains the key that must verify this node's browser steps.
/// Without it, the node's browser is never offered, even if the node has one:
/// an unpinned signature proves nothing.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedNode {
    pub fingerprint: crate::remote::Fingerprint,
    #[serde(default)]
    pub about: Option<String>,
    /// As `node init` printed it. Checked: no two nodes, and not the
    /// agent's own key, pin the same one.
    #[serde(default)]
    pub audit_key: Option<TrailKey>,
}

#[cfg(test)]
mod tests {
    // tests/agent_nodes.rs gains, over the existing loopback TLS pair, with fake::FakeCdp on
    // the node side:
    // - a Ticket on a v2 node with a browser: each step's Done is signed by the node's key and
    //   verifies under the pinned one; the agent's chain and blob store hold everything;
    //   the node's vault value appears in no frame the agent receives (the test greps every
    //   frame for each encoding).
    // - a node answer signed by another key (the test swaps the node's audit.key) is
    //   recorded Lost and never reaches the model.
    // - a v1 node (hello without `browser`, protocol 1) still runs run_code; browser tools
    //   are refused for its Tickets with "node laptop has no browser".
    // - the link drops mid-step: Done is Lost; the next call opens a new session whose
    //   session_step starts at 0.
    // - wire round trip of CallV2/AnswerV2 under deny_unknown_fields; a Browsed frame at
    //   every bound fits MAX_FRAME.
}
