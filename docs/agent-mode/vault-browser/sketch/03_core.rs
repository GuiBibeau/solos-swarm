//! Lives at: crates/agent-core/src/trail.rs (new, `pub mod trail;` in lib.rs),
//! plus the additions to model.rs, engine.rs, ports.rs and memory.rs shown
//! in the sections below, plus crates/agent-store (V4 `trails` table).
//!
//! Pure data and pure rules only: serde, serde_json and sha2, which
//! tests/boundary.rs already allows. No crypto type crosses the fence: a
//! signature is two hex strings the core carries and never checks. The shell
//! verifies a signature at the boundary before it asks the Engine to record
//! a step (agent/browser.rs `verify`); the core checks everything that needs
//! no key: which Assignment may act, step numbering, that a step's outcome
//! answers the action that was asked, and who may answer an approval.
//!
//! Chain records this design adds (each wrapped as `Event { at_ms, ... }` and
//! hashed by the unchanged `chain::append`):
//!
//! ```json
//! {"at_ms":1730642531000,"type":"trail_asked","ticket":"tk_5f1e9a0c2b7d4e11","assignment":"as_9a..","n":3,
//!  "action":{"kind":"login","login":"github-gui","submit":true}}
//! {"at_ms":1730642533120,"type":"trail_done","ticket":"tk_5f1e9a0c2b7d4e11","assignment":"as_9a..","n":3,
//!  "outcome":{"kind":"signed",
//!    "body":{"v":1,"ticket":"tk_5f1e9a0c2b7d4e11","assignment":"as_9a..","n":3,
//!            "environment":"local","session":"7a3c0f9e12d45b60","session_step":2,"at_ms":1730642533100,
//!            "action":{"kind":"login","login":"github-gui","submit":true},
//!            "outcome":{"kind":"page","url":"https://github.com/","status":200,"title":"GitHub",
//!                       "snapshot":"sha256:8a0e..","view":"sha256:2f1c..",
//!                       "filled":["username","password"],"keyed":[]}},
//!    "signer":"ed25519:9c1d..","signature":"<128 hex>"}}
//! {"at_ms":..,"type":"secret_asked","ticket":"tk_..","secret":"local/github-gui"}
//! {"at_ms":..,"type":"secret_answered","ticket":"tk_..","secret":"local/github-gui",
//!  "by":{"id":"U024BE7LH","kind":"person","name":"gui"},"granted":true}
//! {"at_ms":..,"type":"key_used","ticket":"tk_..","assignment":"as_..","secret":"local/openai",
//!  "method":"GET","url":"https://api.openai.com/v1/models"}
//! ```
//!
//! What never appears in a record: a vault value (the shell never has one on
//! the path that builds records; the Pilot scrubs before it signs), typed
//! text (only its digest and length; the text is a blob), page content (only
//! digests; the content is a blob). The only model-written text in a record
//! is a URL the model opened (at most 2,048 characters), which the audit
//! needs.

use crate::model::{AssignmentId, Ms, TicketId};
use serde::{Deserialize, Serialize};

/// The version of [`StepBody`]'s shape. A verifier refuses one it does not
/// know rather than guessing. A later TLS-evidence field (zkTLS) is a v2.
pub const STEP_VERSION: u32 = 1;

/// `sha256:<64 lowercase hex>`, validated on every deserialize.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);

impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        todo!("sha2::Sha256::digest, hex")
    }
}
impl TryFrom<String> for Digest {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        todo!()
    }
}
impl From<Digest> for String {
    fn from(d: Digest) -> String {
        d.0
    }
}

/// What a worker asked the browser to do: the record of the request, built
/// by `browse::action_of(tool, args)` on the agent side and checked equal by
/// the browser that performs it. One type from the tool's arguments to the
/// chain to the wire: there is no separate "request" shape to drift.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TrailAction {
    Open {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    Act {
        action: PageAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        element: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<ActText>,
    },
    Login {
        login: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username_element: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password_element: Option<u32>,
        submit: bool,
    },
    Read {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        part: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        element: Option<u32>,
    },
    /// Asked by the broker, never by the model: at the Assignment's end,
    /// after `idle_secs` without a call, or at shutdown.
    Close { reason: CloseReason },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageAction {
    Click,
    Type,
    Select,
    Press,
    Scroll,
    Back,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActText {
    /// `type`: what was typed is a blob; the record keeps its digest and
    /// length, so the chain holds no permanent copy of it.
    Typed { digest: Digest, chars: u32 },
    /// `select`, `press`, `scroll`: a label, a key name or a direction,
    /// at most 100 characters.
    Named { text: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    AssignmentEnded,
    Idle,
    Shutdown,
}

/// What the environment that ran the browser signs. Built only by
/// `browse::session::release`, after scrubbing; signed with that
/// environment's Ed25519 key over `audit::signing_bytes("solos-swarm/trail-step/v1", body)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepBody {
    pub v: u32,
    pub ticket: TicketId,
    pub assignment: AssignmentId,
    /// The core's step number for this Assignment, from `trail_ask`.
    pub n: u32,
    /// `local` or the node's name: whose key must verify this.
    pub environment: String,
    /// New for every browser process: 16 random hex. A retry, a restart, an
    /// idle close or a crash of the browser starts a new session.
    pub session: String,
    /// The signer's own counter: 0, 1, 2 … within `session`, never reused.
    /// The agent cannot make it skip or repeat, so a dropped or replayed
    /// signed step shows as a gap or a duplicate whatever `n` says.
    pub session_step: u32,
    pub at_ms: Ms,
    /// Equal to the action the core recorded as asked for `n`.
    pub action: TrailAction,
    pub outcome: StepOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StepOutcome {
    Page(PageSeen),
    /// The login or key needs a person's answer on this Ticket first.
    NeedsApproval { secret: String, label: String },
    /// Not done, and why (scrubbed, at most 300 characters): not on the
    /// login's site, a stale element, the browser failed, no slot.
    Refused { why: String },
    /// The session's end; `steps` is its session_step count, so a verifier
    /// knows how long it was.
    Closed { steps: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageSeen {
    /// The main frame's URL after the action, as the engine reports it.
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub title: String,
    /// The canonical capture: url, title, status, markdown, elements and,
    /// when configured, HTML. Scrubbed.
    pub snapshot: Digest,
    /// The exact bytes the model got back.
    pub view: Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub png: Option<Digest>,
    /// The element acted on, as the page named it then: replay reads
    /// "click link 'Notifications'", not "click 14".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ElementSeen>,
    /// Login fields filled by this step: `username`, `password`. Never values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filled: Vec<String>,
    /// Requests that carried a vault key during this step, per key and host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keyed: Vec<Keyed>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElementSeen {
    pub role: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keyed {
    pub key: String,
    pub host: String,
    pub requests: u32,
}

/// A signed step as the chain stores it. `signer` is `ed25519:<64 hex>`,
/// `signature` 128 hex.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedStep {
    pub body: StepBody,
    pub signer: String,
    pub signature: String,
}

/// How an asked step ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TrailOutcome {
    /// Verified by the shell under the pinned key of `body.environment`.
    Signed(SignedStep),
    /// No signed answer: the node was lost mid-call, its answer did not
    /// verify, or the wire failed. Agent-attested.
    Lost { why: String },
}

/// One Assignment's trail position: its own small row (V4 `trails`), so the
/// dominant write, "append a step", rewrites a few bytes instead of the
/// Ticket's whole JSON body twice per browser action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrailCursor {
    pub ticket: TicketId,
    pub assignment: AssignmentId,
    /// The next `n` to hand out; starts at 1.
    pub next: u32,
    /// The step asked and not yet answered. At most one per Assignment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub n: u32,
    pub action: TrailAction,
    pub asked_ms: Ms,
}

/// Why `trail_done` refuses a signed step (beyond "nothing pending").
/// Pure checks; the signature itself was checked by the shell.
pub fn answers(pending: &Pending, ticket: &TicketId, assignment: &AssignmentId, step: &SignedStep) -> Result<(), String> {
    // body.v == STEP_VERSION
    // body.ticket == ticket && body.assignment == assignment
    // body.n == pending.n
    // body.action == pending.action          (the browser did what was asked)
    // Closed answers only a Close action; a Close action is answered Closed or Refused
    let _ = (pending, ticket, assignment, step);
    todo!()
}

// ---------------------------------------------------------------------------
// crates/agent-core/src/model.rs: additions
// ---------------------------------------------------------------------------

pub mod model_additions {
    use super::{TrailAction, TrailOutcome};
    use crate::model::{Actor, AssignmentId, Ms, TicketId};
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    /// `Ticket` gains (serde default, so stored Tickets read as before):
    ///
    /// ```ignore
    /// /// Approvals of vault items for this Ticket, by `<environment>/<name>`.
    /// /// Held for the Ticket's life, across its Assignments.
    /// #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    /// pub secrets: BTreeMap<String, SecretGrant>,
    /// ```
    pub type TicketSecrets = BTreeMap<String, SecretGrant>;

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "state", rename_all = "snake_case")]
    pub enum SecretGrant {
        Waiting {
            /// From the item's metadata, never model or page text.
            label: String,
            /// `source:id`s that may answer; empty: the Policy's approvers.
            #[serde(default)]
            answerable_by: Vec<String>,
            asked_ms: Ms,
        },
        Granted { by: Actor, at_ms: Ms },
        Denied { by: Actor, at_ms: Ms },
    }

    /// `InputBody` gains, so the answer wakes the worker as new input
    /// (`from_allowed_person: true`, delivered like any Input):
    ///
    /// ```ignore
    /// Secret { secret: String, granted: bool, by: Actor },
    /// ```
    pub struct InputBodySecret;

    /// `OutboundMessage` gains what a Channel with buttons offers for it,
    /// set by the Engine, so the runtime no longer recognizes the budget ask
    /// by comparing its text with `ticket.attention` (`runtime.rs:603-611`).
    /// Budget and secret approvals become one way:
    ///
    /// ```ignore
    /// #[serde(default, skip_serializing_if = "Option::is_none")]
    /// pub ask: Option<Ask>,
    /// ```
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum Ask {
        /// Approve or Stop the Ticket's budget.
        Budget,
        /// Allow or Deny one vault item for the Ticket.
        Secret { secret: String },
    }

    /// `DomainEvent` gains five variants:
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum DomainEventAdditions {
        TrailAsked { ticket: TicketId, assignment: AssignmentId, n: u32, action: TrailAction },
        TrailDone { ticket: TicketId, assignment: AssignmentId, n: u32, outcome: TrailOutcome },
        SecretAsked { ticket: TicketId, secret: String },
        SecretAnswered { ticket: TicketId, secret: String, by: Actor, granted: bool },
        /// Written before a keyed `fetch_url` leaves (write-ahead).
        KeyUsed { ticket: TicketId, assignment: AssignmentId, secret: String, method: String, url: String },
    }

    /// `ChangeSet` gains:
    ///
    /// ```ignore
    /// pub trails: BTreeMap<AssignmentId, TrailCursor>,
    /// ```
    pub struct ChangeSetTrails;
}

// ---------------------------------------------------------------------------
// crates/agent-core/src/ports.rs: Repositories gains one read
// ---------------------------------------------------------------------------
//
// pub trait Repositories {
//     ...
//     /// One Assignment's trail position, if it ever asked a step.
//     fn trail(&self, assignment: &AssignmentId) -> Option<TrailCursor>;
// }
//
// MemoryRepositories keeps a BTreeMap; the seeded differential walk
// (agent-store) compares the two after every command, trails included.

// ---------------------------------------------------------------------------
// crates/agent-core/src/engine.rs: five commands
// ---------------------------------------------------------------------------

pub mod engine_additions {
    use super::model_additions::SecretGrant;
    use super::{TrailAction, TrailOutcome};
    use crate::model::{Actor, AssignmentId, TicketId};
    use crate::ports::{Clock, Error, Policy, Repositories};

    pub struct Engine<R, P, C>(R, P, C);

    impl<R: Repositories, P: Policy, C: Clock> Engine<R, P, C> {
        /// Record that `assignment` is about to act, before it acts
        /// (write-ahead: the Effect rule "stored first, done after"). Returns
        /// the step's `n`.
        ///
        /// ```text
        /// t = require_ticket(ticket)
        /// cursor = repos.trail(assignment) or TrailCursor { ticket, assignment, next: 1, pending: None }
        /// cursor.ticket == ticket                     else Invalid
        /// cursor.pending is None                      else Refused("step {p.n} of {assignment} is still running")
        /// match action:
        ///   Close{..} => cursor.next > 1              else Refused("nothing to close")
        ///                (any Assignment state: a stopped worker's browser still closes)
        ///   _         => t.running_assignment().id == assignment
        ///                                             else Refused("{assignment} is not running")
        ///                (the core, not the broker, says which Assignment may still act)
        /// n = cursor.next; cursor.next += 1
        /// cursor.pending = Some(Pending { n, action: action.clone(), asked_ms: now })
        /// changes.trails.insert(assignment, cursor)
        /// event TrailAsked { ticket, assignment, n, action }
        /// ```
        pub fn trail_ask(&mut self, ticket: &TicketId, assignment: &AssignmentId, action: TrailAction) -> Result<u32, Error> {
            todo!()
        }

        /// Record how step `n` ended. Accepted whatever the Assignment's or
        /// the Ticket's state now: the action happened, so it is recorded.
        ///
        /// ```text
        /// cursor = repos.trail(assignment)            else Missing
        /// p = cursor.pending.filter(|p| p.n == n)     else Refused("step {n} of {assignment} is not waiting")
        /// if Signed(s) = &outcome: trail::answers(&p, ticket, assignment, s) else Invalid(why)
        /// cursor.pending = None; save; event TrailDone { ticket, assignment, n, outcome }
        /// ```
        ///
        /// No recovery code: a step asked by a process that died stays
        /// pending in its (ended) Assignment's cursor forever, and the
        /// verifier reports it as "asked, outcome unknown", which is the
        /// truth. A restart starts a new Assignment, so a new cursor.
        pub fn trail_done(&mut self, ticket: &TicketId, assignment: &AssignmentId, n: u32, outcome: TrailOutcome) -> Result<(), Error> {
            todo!()
        }

        /// Ask a person to approve `secret` (`<environment>/<name>`) for the
        /// Ticket, once. Idempotent: an existing grant, in any state, is
        /// returned unchanged and sends nothing, so a worker retrying
        /// `browser_login` while it waits cannot spam the thread.
        ///
        /// ```text
        /// t = open_ticket(ticket)
        /// if let Some(g) = t.secrets.get(secret): return g.clone()
        /// g = Waiting { label, answerable_by, asked_ms: now }
        /// t.secrets.insert(secret, g.clone())
        /// event SecretAsked { ticket, secret }
        /// send(t, Attention, format!("{title} wants to {label} for the first time."), ask: Some(Ask::Secret { secret }))
        /// save(t); return g
        /// ```
        pub fn ask_secret(&mut self, ticket: &TicketId, secret: &str, label: &str, answerable_by: &[String]) -> Result<SecretGrant, Error> {
            todo!()
        }

        /// A person's Allow or Deny, from a Source's own controls
        /// (`Fact::Command`, `Order::Secret`), never from message text.
        ///
        /// ```text
        /// t = open_ticket(ticket)
        /// Waiting { answerable_by, .. } = t.secrets[secret]   else Refused("{secret} does not wait for an answer")
        /// may = by.is_person() && if answerable_by.is_empty() { policy.may_approve(source, by) }
        ///                         else { answerable_by.contains(format!("{source}:{}", by.id)) }
        /// may                                                  else Refused("{by} may not answer for {secret}")
        /// t.secrets[secret] = Granted|Denied { by, at_ms: now }
        /// event SecretAnswered { ticket, secret, by, granted: allow }
        /// add_input(t, InputBody::Secret { secret, granted: allow, by }, true)   // wakes the worker
        /// save(t)
        /// ```
        pub fn answer_secret(&mut self, ticket: &TicketId, secret: &str, source: &str, by: Actor, allow: bool) -> Result<(), Error> {
            todo!()
        }

        /// Record a keyed `fetch_url` before it leaves.
        ///
        /// ```text
        /// t = open_ticket(ticket)
        /// t.running_assignment().id == assignment   else Refused("{assignment} is not running")
        /// !matches!(t.secrets.get(secret), Some(Denied { .. }))   else Refused   // defence in depth
        /// event KeyUsed { ticket, assignment, secret, method, url }
        /// ```
        pub fn use_key(&mut self, ticket: &TicketId, assignment: &AssignmentId, secret: &str, method: &str, url: &str) -> Result<(), Error> {
            todo!()
        }
    }
}

// ---------------------------------------------------------------------------
// crates/agent-store: schema V4 and the trails row
// ---------------------------------------------------------------------------
//
// pub(crate) const LATEST: u32 = 4;
//
// /// One row per Assignment that ever asked a browser step. Written only by
// /// `SqliteStore::write`, from `ChangeSet.trails`, in the same IMMEDIATE
// /// transaction as the TrailAsked/TrailDone event it goes with. Bumping
// /// LATEST also makes an older `agent status` or `bench` refuse this file
// /// cleanly instead of failing on an event variant it cannot read
// /// (grounding open question 9).
// const V4: &str = "
// CREATE TABLE IF NOT EXISTS trails (
//     assignment TEXT PRIMARY KEY NOT NULL,
//     ticket TEXT NOT NULL,
//     body TEXT NOT NULL
// ) STRICT;
// CREATE INDEX IF NOT EXISTS trails_ticket ON trails (ticket);
// ";
//
// In SqliteStore::write, before queue::append:
//   INSERT INTO trails (assignment, ticket, body) VALUES (?1, ?2, ?3)
//   ON CONFLICT (assignment) DO UPDATE SET body = excluded.body
// and `Repositories::trail` reads `SELECT body FROM trails WHERE assignment = ?1`.
//
// Tests: migrating twice changes nothing (V1..V4 each idempotent); a V3 file
// gains `trails` and keeps its rows; the differential walk covers
// trail_ask/trail_done/ask_secret/answer_secret/use_key.

#[cfg(test)]
mod tests {
    // trail_ask hands out 1, 2, 3 and refuses a second ask while one is pending.
    // trail_ask refuses a non-Close action for an Assignment that is not the running one,
    //   and admits Close for an ended Assignment.
    // trail_done refuses a signed step whose action differs from the asked one, whose n,
    //   ticket or assignment differ, or whose Closed-ness does not match the action.
    // trail_done records a step for an Assignment that ended while it ran.
    // ask_secret twice sends one message and returns Waiting both times.
    // answer_secret: a person in answerable_by may; an approver not in a non-empty
    //   answerable_by may not; a bot may not; text in a Trigger has no path to it.
    // answer_secret adds an Input that a running Assignment gets in a Deliver.
    // use_key refuses an Assignment that is not running and a Denied secret.
    // Stored Tickets, Events and OutboundMessages from before read unchanged.
}
