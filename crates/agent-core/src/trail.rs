//! What a worker's browser was asked to do, and what the machine that ran it
//! signed. A signature, a key and a digest are validated hex strings here and no
//! crypto type: the shell checks a signature before it asks the Engine to
//! record a step, and the core checks what needs no key (which Assignment may
//! act, the numbering, and that a step answers the action that was asked).

use crate::model::{AssignmentId, Ms, TicketId};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The version of [`StepBody`]'s shape. A verifier refuses one it does not
/// know rather than guessing.
pub const STEP_VERSION: u32 = 1;
pub const MAX_URL_CHARS: usize = 2048;
pub const MAX_NAMED_CHARS: usize = 100;
pub const MAX_WHY_CHARS: usize = 300;

macro_rules! hex_newtype {
    ($($(#[$doc:meta])* $name:ident, $prefix:literal, $digits:literal),* $(,)?) => {$(
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
            pub fn digits(&self) -> &str {
                &self.0[$prefix.len()..]
            }
        }
        impl TryFrom<String> for $name {
            type Error = String;
            fn try_from(value: String) -> Result<Self, String> {
                let hex = value.strip_prefix($prefix).is_some_and(|rest| {
                    rest.len() == $digits
                        && rest.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                });
                if hex {
                    Ok(Self(value))
                } else {
                    Err(format!(
                        "expected the prefix {:?} and {} lowercase hex digits",
                        $prefix, $digits
                    ))
                }
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    )*};
}
hex_newtype!(
    /// `sha256:` and 64 hex digits.
    Digest, "sha256:", 64,
    /// The public key of the machine that signs: `ed25519:` and 64 hex digits.
    TrailKey, "ed25519:", 64,
    /// An Ed25519 signature: 128 hex digits.
    Signature, "", 128,
);

impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(format!("sha256:{:x}", Sha256::digest(bytes)))
    }
}

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
    /// Asked by the broker, never by the model.
    Close { reason: CloseReason },
}

impl TrailAction {
    /// Bounds every string a model writes into the chain, and refuses a Close
    /// for a crash, which only the browser reports.
    pub fn check(&self) -> Result<(), String> {
        let longer = |text: &str, max: usize| text.chars().count() > max;
        match self {
            Self::Open { url, .. } if longer(url, MAX_URL_CHARS) => {
                Err(format!("a url is at most {MAX_URL_CHARS} characters"))
            }
            Self::Open {
                key: Some(name), ..
            }
            | Self::Login { login: name, .. }
                if longer(name, MAX_NAMED_CHARS) =>
            {
                Err(format!("a name is at most {MAX_NAMED_CHARS} characters"))
            }
            Self::Act {
                text: Some(ActText::Named { text }),
                ..
            } if longer(text, MAX_NAMED_CHARS) => Err(format!(
                "an action's text is at most {MAX_NAMED_CHARS} characters"
            )),
            Self::Close {
                reason: CloseReason::Crashed,
            } => Err("a crash is reported by the browser, not asked for".into()),
            _ => Ok(()),
        }
    }
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
    /// What was typed lives in a blob. The chain keeps only its digest and
    /// length, so it holds no permanent copy of it.
    Typed { digest: Digest, chars: u32 },
    /// A label, a key name or a direction.
    Named { text: String },
}

/// The broker asks for the first three. A crash is the browser's own report,
/// so it only ever appears in an outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    AssignmentEnded,
    Idle,
    Shutdown,
    Crashed,
}

/// The build and policy a session ran under, carried by its first signed step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub binary_sha256: Digest,
    pub argv_sha256: Digest,
}

/// What the machine that ran the browser signs, over a canonical encoding of
/// the whole body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepBody {
    pub v: u32,
    pub ticket: TicketId,
    pub assignment: AssignmentId,
    /// The core's number for this step, from `trail_ask`.
    pub n: u32,
    /// `local` or the node's name: whose key must verify this.
    pub environment: String,
    /// New for every browser process, so a retry, an idle close or a crash
    /// starts a new session.
    pub session: String,
    /// The signer's own counter, 0, 1, 2 within `session`. The agent cannot
    /// make it skip or repeat, so a dropped or replayed signed step shows as
    /// a gap or a duplicate whatever `n` says.
    pub session_step: u32,
    pub at_ms: Ms,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    /// Equal to the action the core recorded as asked for `n`.
    pub action: TrailAction,
    pub outcome: StepOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StepOutcome {
    Page(PageSeen),
    NeedsApproval {
        secret: String,
        label: String,
    },
    Refused {
        why: String,
    },
    /// The session ended. `steps` is its `session_step` count, so a verifier
    /// knows how long it was.
    Closed {
        steps: u32,
        reason: CloseReason,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageSeen {
    /// The main frame's URL after the action, as the engine reports it.
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub title: String,
    /// The canonical capture of the page, scrubbed.
    pub snapshot: Digest,
    /// The exact bytes the model got back.
    pub view: Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ElementSeen>,
    /// Login fields this step filled: `username`, `password`. Never values.
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedStep {
    pub body: StepBody,
    pub signer: TrailKey,
    pub signature: Signature,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TrailOutcome {
    /// Verified by the shell under the pinned key of `body.environment`.
    Signed(Box<SignedStep>),
    /// No signed answer: the node was lost mid-call, its answer did not
    /// verify, or the wire failed. Agent-attested.
    Lost { why: String },
}

impl TrailOutcome {
    pub fn signed(step: SignedStep) -> Self {
        Self::Signed(Box::new(step))
    }

    /// Cut an unsigned reason to `MAX_WHY_CHARS`. The core cuts it instead of
    /// refusing it, because a refused answer leaves the step pending.
    pub fn bounded(self) -> Self {
        match self {
            Self::Lost { why } => Self::Lost {
                why: why.chars().take(MAX_WHY_CHARS).collect(),
            },
            signed => signed,
        }
    }
}

/// One Assignment's trail position. It has its own row, so appending a step,
/// the most frequent write, rewrites a few bytes instead of the Ticket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrailCursor {
    pub ticket: TicketId,
    pub assignment: AssignmentId,
    /// The next `n` to hand out. It starts at 1.
    pub next: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub n: u32,
    pub action: TrailAction,
    pub asked_ms: Ms,
}

impl TrailCursor {
    pub fn new(ticket: TicketId, assignment: AssignmentId) -> Self {
        Self {
            ticket,
            assignment,
            next: 1,
            pending: None,
        }
    }

    pub fn ask(&mut self, action: TrailAction, now: Ms) -> Result<u32, String> {
        if let Some(pending) = &self.pending {
            return Err(format!(
                "step {} of {} is still running",
                pending.n, self.assignment
            ));
        }
        let n = self.next;
        self.next += 1;
        self.pending = Some(Pending {
            n,
            action,
            asked_ms: now,
        });
        Ok(n)
    }

    pub fn answer(&mut self, n: u32) -> Result<Pending, String> {
        self.pending
            .take_if(|pending| pending.n == n)
            .ok_or_else(|| format!("step {n} of {} is not waiting", self.assignment))
    }
}

/// Pure checks: the shell checked the signature.
pub fn answers(
    pending: &Pending,
    ticket: &TicketId,
    assignment: &AssignmentId,
    step: &SignedStep,
) -> Result<(), String> {
    let body = &step.body;
    if body.v != STEP_VERSION {
        return Err(format!("step version {} is not {STEP_VERSION}", body.v));
    }
    if body.ticket != *ticket || body.assignment != *assignment {
        return Err(format!(
            "the step is for {} of {}, not {assignment} of {ticket}",
            body.assignment, body.ticket
        ));
    }
    if body.n != pending.n {
        return Err(format!("the step is number {}, not {}", body.n, pending.n));
    }
    if body.action != pending.action {
        return Err("the step did something other than what was asked".into());
    }
    if !fits(&pending.action, &body.outcome) {
        return Err("the step's outcome cannot answer that action".into());
    }
    Ok(())
}

fn fits(action: &TrailAction, outcome: &StepOutcome) -> bool {
    match (action, outcome) {
        (
            _,
            StepOutcome::Closed {
                reason: CloseReason::Crashed,
                ..
            },
        ) => true,
        (TrailAction::Close { reason: asked }, StepOutcome::Closed { reason, .. }) => {
            reason == asked
        }
        (TrailAction::Close { .. }, StepOutcome::Refused { .. }) => true,
        (TrailAction::Close { .. }, _) | (_, StepOutcome::Closed { .. }) => false,
        _ => true,
    }
}
