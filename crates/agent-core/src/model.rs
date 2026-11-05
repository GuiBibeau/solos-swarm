use crate::change::{Deliverable, RepoName};
use crate::trail::{TrailAction, TrailCursor, TrailOutcome};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Milliseconds on the shell's clock.
pub type Ms = u64;
/// Money, in the ledger's unit (millionths of a US dollar).
pub type MicroUsd = u64;

macro_rules! ids {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {$(
        $(#[$doc])*
        #[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    )*};
}
ids!(
    /// Stable per Source, so a restart or a redelivery cannot create a second Trigger.
    TriggerId,
    /// A Source's name for a conversation: a Slack thread, a pull request, an email chain.
    ThreadKey,
    EvalId,
    DecisionId,
    TicketId,
    AssignmentId,
    InputId,
    /// Channels send each id at most once.
    MessageId,
);

/// The longest [`SecretId`]. Discord's `custom_id` holds 100 characters and
/// carries the action, the Ticket id and the secret.
pub const MAX_SECRET_CHARS: usize = 60;

/// A vault item as a Ticket names it: `<environment>/<name>`, such as
/// `local/github-gui`. Both sides are ASCII letters, digits, `-` and `_`, so
/// no `:` splits a Discord `custom_id` wrongly and nothing needs escaping.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SecretId(String);

impl SecretId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for SecretId {
    type Error = String;
    fn try_from(value: String) -> Result<Self, String> {
        let part = |side: &str| {
            !side.is_empty()
                && side
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        };
        let shaped = value
            .split_once('/')
            .is_some_and(|(environment, name)| part(environment) && part(name));
        if shaped && value.len() <= MAX_SECRET_CHARS {
            Ok(Self(value))
        } else {
            Err(format!(
                "expected <environment>/<name> of ASCII letters, digits, '-' and '_', \
                 at most {MAX_SECRET_CHARS} characters"
            ))
        }
    }
}
impl From<SecretId> for String {
    fn from(value: SecretId) -> String {
        value.0
    }
}
impl std::fmt::Display for SecretId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Person,
    Bot,
    /// The host itself: a schedule the operator configured, or the Watchdog.
    System,
}

/// Who caused a Trigger, in the Source's own identities. Identities on
/// different Sources are never mapped to each other by the core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub id: String,
    pub kind: ActorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}
impl Actor {
    pub fn person(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: ActorKind::Person,
            name: None,
        }
    }
    pub fn bot(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: ActorKind::Bot,
            name: None,
        }
    }
    pub fn system(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: ActorKind::System,
            name: None,
        }
    }
    pub fn is_person(&self) -> bool {
        self.kind == ActorKind::Person
    }
}

/// Whether the message spoke to the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Addressed {
    Mention,
    Direct,
    Unaddressed,
}
impl Addressed {
    pub fn is_addressed(self) -> bool {
        self != Self::Unaddressed
    }
}

/// The write side of a Thread: where a Source can post a reply.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChannelRef {
    pub source: String,
    pub address: BTreeMap<String, String>,
}

/// One immutable event from a Source. Its text is data: it can never change a
/// permission, a budget or a Policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    pub id: TriggerId,
    /// The Source instance that produced it, such as `slack`.
    pub source: String,
    pub at_ms: Ms,
    pub actor: Actor,
    pub addressed: Addressed,
    pub text: String,
    /// The Threads it belongs to, primary first. Never empty.
    pub threads: Vec<ThreadKey>,
    #[serde(default)]
    pub reply_to: Vec<ChannelRef>,
    /// Source-owned; the core never reads it.
    #[serde(default)]
    pub meta: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    /// Proactive work the agent noticed; needs a matching rule and `auto` mode.
    Inferred,
    /// An allowed person asked, or followed up on their work.
    Explicit,
}

/// The single running judgement of a Thread's batch of Triggers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eval {
    pub id: EvalId,
    pub triggers: Vec<TriggerId>,
    /// The admissible Trigger that gave the batch its strength.
    pub primary: TriggerId,
    pub strength: Strength,
    pub policy_version: String,
    pub started_ms: Ms,
}

/// Triggers in time order. At most one Eval runs on a Thread; Triggers that
/// arrive meanwhile join the next batch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub key: ThreadKey,
    pub triggers: Vec<TriggerId>,
    /// Waiting for the next Eval.
    pub queued: Vec<TriggerId>,
    pub eval: Option<Eval>,
    pub evals_started: u32,
    pub consecutive_expiries: u32,
    /// Tickets that receive this Thread's Triggers as context.
    /// Triggers a Ticket handed back here. Each is judged once after a
    /// handback, however many Tickets hand it back.
    #[serde(default)]
    pub handed_back: BTreeSet<TriggerId>,
    pub subscriptions: BTreeSet<TicketId>,
    pub decisions: Vec<DecisionId>,
}
impl Thread {
    pub fn new(key: ThreadKey) -> Self {
        Self {
            key,
            ..Self::default()
        }
    }
    pub fn is_active(&self) -> bool {
        self.eval.is_some() || !self.queued.is_empty()
    }
}

/// How an accepted Ticket is executed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Plan {
    /// One isolated worker session with its own directory.
    Session {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        harness: Option<String>,
        /// Where its commands run: a remote node by name, or `local`.
        /// `None` is the executor's default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        environment: Option<String>,
        /// How its work is held to account, fixed at accept. `None`: it
        /// owes an answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<Method>,
    },
    /// The Ticket's scope becomes a Goal run by a governed swarm of `agents`
    /// sandboxed workers under the Ticket's budget.
    Swarm {
        agents: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        goal_template: Option<String>,
    },
}

/// A session's method. The Router names what the work owes; the
/// repository's default stands in when it names nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Method {
    pub deliverable: Deliverable,
}

/// What an accept grants, fixed at accept time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mandate {
    pub policy_version: String,
    pub scope: String,
    pub strength: Strength,
    /// Proactive rules the Router matched, for inferred work.
    #[serde(default)]
    pub rules: Vec<String>,
    pub requester: Actor,
    pub source: String,
    /// The Ticket's whole envelope across all of its Assignments. `None`:
    /// the Ticket has no cap, so it needs no approval and its account has
    /// no limit (a swarm plan always has one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_microusd: Option<MicroUsd>,
    /// The one repository its worker reads and changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoName>,
}

/// Whether a person has to approve the Ticket's budget before it launches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Approval {
    NotNeeded,
    Waiting,
    Granted { by: Actor, at_ms: Ms },
}

/// A person's decision on one vault item for one Ticket. Granted and Denied
/// are final for the Ticket's life.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SecretGrant {
    Waiting {
        /// From the item's metadata, never model or page text.
        label: String,
        /// `source:id`s that may answer; empty: the Policy's approvers.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        answerable_by: Vec<String>,
        asked_ms: Ms,
    },
    Granted {
        by: Actor,
        at_ms: Ms,
    },
    Denied {
        by: Actor,
        at_ms: Ms,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputBody {
    /// A Trigger fanned out from a subscribed Thread.
    Trigger {
        trigger: TriggerId,
    },
    /// A fact from the Router about other work. Grants nothing.
    Note {
        text: String,
    },
    Approved {
        by: Actor,
    },
    /// A person's click on a vault item's Allow or Deny. Typed text never
    /// becomes one.
    Secret {
        secret: SecretId,
        granted: bool,
        by: Actor,
    },
}

/// Context delivered to a Ticket's worker, in order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    pub id: InputId,
    pub seq: u64,
    pub body: InputBody,
    /// A person the Policy admits explicitly wrote it. Only such an Input
    /// resumes ended work, ends a hold, or can be handed back.
    pub from_allowed_person: bool,
    pub at_ms: Ms,
    /// The Assignment it was handed to; `None` while it waits.
    pub sent_to: Option<AssignmentId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    /// Still working; stays on the Ticket, nobody is told.
    Progress,
    /// Turn over, nothing outstanding; a Trigger can continue it.
    Waiting,
    /// A result a person can use; ends the Ticket.
    Ready,
    /// Cannot continue without a person.
    Blocked,
    Failed,
    /// Nothing to do (did not reproduce, superseded); ends silently.
    Dropped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub seq: u64,
    pub status: ReportStatus,
    pub text: String,
    /// One concrete question for a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// Cumulative spend the ledger settled for this Assignment.
    #[serde(default)]
    pub spent_microusd: MicroUsd,
    pub at_ms: Ms,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentState {
    Pending,
    Running,
    Ended,
}

/// The binding of a Ticket to one Executor run. A Ticket has several over time
/// (resume, retry, restart).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub id: AssignmentId,
    pub state: AssignmentState,
    pub created_ms: Ms,
    #[serde(default)]
    pub reports: Vec<Report>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndedBy {
    Person,
    Executor,
    Policy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketOutcome {
    Completed,
    Dropped,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ended {
    pub at_ms: Ms,
    pub by: EndedBy,
    pub outcome: TicketOutcome,
    pub reason: String,
}

/// A unit of authorized work. Only an accepted Eval creates one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    pub id: TicketId,
    pub title: String,
    pub origin: TriggerId,
    pub decision: DecisionId,
    pub mandate: Mandate,
    pub plan: Plan,
    pub channels: Vec<ChannelRef>,
    pub threads: BTreeSet<ThreadKey>,
    #[serde(default)]
    pub inputs: Vec<Input>,
    #[serde(default)]
    pub assignments: Vec<Assignment>,
    pub approval: Approval,
    /// Approvals of vault items for this Ticket, held across its Assignments.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<SecretId, SecretGrant>,
    /// The question waiting for a person, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<Ended>,
    pub created_ms: Ms,
    /// Orders Inputs and Reports on one counter.
    pub seq: u64,
    /// Triggers its worker handed back to the Router, each at most once.
    #[serde(default)]
    pub handed_back: BTreeSet<TriggerId>,
}
impl Ticket {
    pub fn latest_assignment(&self) -> Option<&Assignment> {
        self.assignments.last()
    }
    pub fn running_assignment(&self) -> Option<&Assignment> {
        self.latest_assignment()
            .filter(|a| a.state == AssignmentState::Running)
    }
    pub fn latest_report(&self) -> Option<&Report> {
        self.latest_assignment().and_then(|a| a.reports.last())
    }
    pub fn has_live_assignment(&self) -> bool {
        self.latest_assignment()
            .is_some_and(|a| a.state != AssignmentState::Ended)
    }
    pub fn stopped_by_person(&self) -> bool {
        self.ended.as_ref().is_some_and(|e| e.by == EndedBy::Person)
    }
    /// Whether `ask` still stands: the Ticket is open and its question has
    /// not been answered.
    pub fn awaits(&self, ask: &Ask) -> bool {
        self.ended.is_none()
            && match ask {
                Ask::Budget => self.approval == Approval::Waiting,
                Ask::Secret { secret } => {
                    matches!(self.secrets.get(secret), Some(SecretGrant::Waiting { .. }))
                }
            }
    }
    /// The latest Report is `blocked` or `failed` and no person wrote since:
    /// automatic Inputs (bots, CI) are held until someone does.
    pub fn waits_for_person(&self) -> bool {
        let Some(report) = self.latest_report() else {
            return false;
        };
        matches!(report.status, ReportStatus::Blocked | ReportStatus::Failed)
            && !self
                .inputs
                .iter()
                .any(|input| input.from_allowed_person && input.seq > report.seq)
    }
    pub fn stage(&self) -> Stage {
        if self.ended.is_some() {
            return Stage::Ended;
        }
        if self.approval == Approval::Waiting {
            return Stage::Approval;
        }
        if self.attention.is_some() {
            return Stage::Answer;
        }
        if self.waits_for_person() {
            return match self.latest_report().map(|r| r.status) {
                Some(ReportStatus::Failed) => Stage::Failed,
                _ => Stage::Blocked,
            };
        }
        match self.latest_assignment() {
            None => Stage::Queued,
            Some(a) if a.state == AssignmentState::Pending => Stage::Queued,
            Some(a) if a.state == AssignmentState::Ended => Stage::Idle,
            Some(_) if self.latest_report().map(|r| r.status) == Some(ReportStatus::Waiting) => {
                Stage::Idle
            }
            Some(_) => Stage::Working,
        }
    }
}

/// What an open Ticket waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// A person must approve the budget.
    Approval,
    /// A person must answer a question.
    Answer,
    Blocked,
    Failed,
    Queued,
    /// Turn over; a Trigger on a subscribed Thread continues it.
    Idle,
    Working,
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Accept,
    Ignore,
    Stop,
    Unavailable,
}

/// The recorded result of an Eval, or of a gate that admitted nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub id: DecisionId,
    pub thread: ThreadKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval: Option<EvalId>,
    pub triggers: Vec<TriggerId>,
    pub kind: DecisionKind,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<TicketId>,
    #[serde(default)]
    pub would_accept: bool,
    pub policy_version: String,
    pub at_ms: Ms,
}

/// The Router's judgement of a candidate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verdict {
    Accept(Accept),
    Ignore {
        reason: String,
    },
    /// A person on this Thread asked to stop one of its Tickets.
    Stop {
        ticket: TicketId,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accept {
    pub title: String,
    pub scope: String,
    pub reason: String,
    pub plan: Plan,
    /// `None` for a session where the operator sets no budget; a swarm plan
    /// must name one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_microusd: Option<MicroUsd>,
    #[serde(default)]
    pub acknowledge: Acknowledge,
    #[serde(default)]
    pub rules: Vec<String>,
    /// The repository the work reads and changes, for a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoName>,
}

/// The first reply on an accepted Ticket. A rule can ask for silence.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "text", rename_all = "snake_case")]
pub enum Acknowledge {
    #[default]
    Standard,
    Silent,
    Text(String),
}

/// A worker's report as it arrives, before the core numbers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportIn {
    pub status: ReportStatus,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(default)]
    pub spent_microusd: MicroUsd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Acknowledgement,
    Result,
    /// A person must decide, answer or approve.
    Attention,
}

/// What a message asks a person, so a Channel with buttons can offer them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ask {
    /// Approve or Stop the Ticket's budget.
    Budget,
    /// Allow or Deny one vault item for the Ticket.
    Secret { secret: SecretId },
}

/// A message for a Channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub id: MessageId,
    pub channel: ChannelRef,
    pub kind: MessageKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<TicketId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<Ask>,
}

/// What the Router sees of a Ticket when it judges a candidate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketSummary {
    pub id: TicketId,
    pub title: String,
    pub scope: String,
    pub stage: Stage,
}

/// Bounded facts for the Router, never authority.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub eval: EvalId,
    pub thread: ThreadKey,
    pub strength: Strength,
    pub policy_version: String,
    pub triggers: Vec<Trigger>,
    /// Every Ticket on the Thread, ended ones too, so duplicates can be judged.
    pub tickets: Vec<TicketSummary>,
}

/// External work a command asks for. The Store keeps it in the same commit
/// as the state change; adapters perform it afterwards and report back
/// through Engine commands.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub enum Effect {
    /// Ask the Router to judge a candidate; the answer returns via `Engine::decide`.
    Assess(Candidate),
    /// Start an Assignment; reports back via `assignment_started` and `report`.
    Launch {
        ticket: TicketId,
        assignment: AssignmentId,
        plan: Plan,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget_microusd: Option<MicroUsd>,
        /// A resume or restart: the worker checks its directory before repeating outside actions.
        recovery: bool,
    },
    /// Hand waiting Inputs to a running Assignment, in order.
    Deliver {
        ticket: TicketId,
        assignment: AssignmentId,
        inputs: Vec<InputId>,
    },
    /// End an Assignment's work, after the domain already ended it.
    Stop {
        ticket: TicketId,
        assignment: AssignmentId,
        reason: String,
    },
    Send(OutboundMessage),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DomainEvent {
    TriggerReceived {
        trigger: TriggerId,
        threads: Vec<ThreadKey>,
        reached: Vec<TicketId>,
        queued: bool,
    },
    EvalStarted {
        eval: EvalId,
        thread: ThreadKey,
        triggers: Vec<TriggerId>,
        strength: Strength,
    },
    EvalExpired {
        eval: EvalId,
        thread: ThreadKey,
    },
    DecisionRecorded {
        decision: DecisionId,
        kind: DecisionKind,
        thread: ThreadKey,
    },
    TicketCreated {
        ticket: TicketId,
        decision: DecisionId,
        plan: Plan,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget_microusd: Option<MicroUsd>,
    },
    TicketApproved {
        ticket: TicketId,
        by: Actor,
    },
    TicketResumed {
        ticket: TicketId,
        trigger: TriggerId,
    },
    TicketEnded {
        ticket: TicketId,
        by: EndedBy,
        outcome: TicketOutcome,
    },
    AssignmentStarted {
        ticket: TicketId,
        assignment: AssignmentId,
    },
    AssignmentEnded {
        ticket: TicketId,
        assignment: AssignmentId,
        reason: String,
    },
    Reported {
        ticket: TicketId,
        assignment: AssignmentId,
        status: ReportStatus,
    },
    HandedBack {
        ticket: TicketId,
        trigger: TriggerId,
        reason: String,
    },
    /// Written before the browser acts, so an action never goes unrecorded.
    TrailAsked {
        ticket: TicketId,
        assignment: AssignmentId,
        n: u32,
        action: TrailAction,
    },
    TrailDone {
        ticket: TicketId,
        assignment: AssignmentId,
        n: u32,
        outcome: TrailOutcome,
    },
    SecretAsked {
        ticket: TicketId,
        secret: SecretId,
    },
    SecretAnswered {
        ticket: TicketId,
        secret: SecretId,
        by: Actor,
        granted: bool,
    },
    /// Written before a keyed `fetch_url` leaves.
    KeyUsed {
        ticket: TicketId,
        assignment: AssignmentId,
        secret: SecretId,
        method: String,
        url: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub at_ms: Ms,
    #[serde(flatten)]
    pub event: DomainEvent,
}

/// Everything one command changed, committed atomically by the Store.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChangeSet {
    /// New Triggers only; Triggers are immutable.
    pub triggers: Vec<Trigger>,
    pub threads: BTreeMap<ThreadKey, Thread>,
    pub tickets: BTreeMap<TicketId, Ticket>,
    pub decisions: Vec<Decision>,
    pub trails: BTreeMap<AssignmentId, TrailCursor>,
    pub effects: Vec<Effect>,
    pub events: Vec<Event>,
}
