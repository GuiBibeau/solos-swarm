use super::read_blob;
use crate::agent::environment::LOCAL;
use crate::agent::{AgentConfig, HOST_RESTARTED, STORE_FILE};
use crate::browse::{BadSignature, NO_SESSION, TrailSigner, verify_signature};
use crate::vault::SandboxView;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use solos_agent_core::model::{Actor, AssignmentId, DomainEvent, Event, Ms, SecretId, TicketId};
use solos_agent_core::trail::{
    ActText, Digest, Pending, SignedStep, StepBody, StepOutcome, TrailAction, TrailKey,
    TrailOutcome, answers,
};
use solos_agent_store::{SqliteStore, StoreError};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::str::FromStr;

/// Which key verifies each environment's steps.
#[derive(Clone, Debug, Default)]
pub struct Trust(BTreeMap<String, TrailKey>);

/// `--trust <environment>=ed25519:<hex>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trusted {
    pub environment: String,
    pub key: TrailKey,
}

impl FromStr for Trusted {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let (environment, key) = text
            .split_once('=')
            .ok_or_else(|| "expected <environment>=ed25519:<hex>".to_owned())?;
        if environment.is_empty() {
            return Err("the environment before '=' is empty".to_owned());
        }
        Ok(Self {
            environment: environment.to_owned(),
            key: TrailKey::try_from(key.to_owned())?,
        })
    }
}

impl Trust {
    /// Each `trusted` adds or replaces one environment. `local` is the
    /// public half of `browser.audit_key`, and only when no `trusted` names
    /// it, so a verifier that holds public keys never reads a seed.
    pub fn new(config: Option<&AgentConfig>, trusted: Vec<Trusted>) -> Result<Self> {
        let mut keys = BTreeMap::new();
        let named = trusted.iter().any(|each| each.environment == LOCAL);
        if let Some(browser) = config.and_then(|config| config.browser.as_ref())
            && !named
        {
            let signer = TrailSigner::load(&browser.audit_key, &SandboxView::new([]))
                .context("browser.audit_key")?;
            keys.insert(LOCAL.to_owned(), signer.public());
        }
        for each in trusted {
            keys.insert(each.environment, each.key);
        }
        Ok(Self(keys))
    }

    fn key(&self, environment: &str) -> Option<&TrailKey> {
        self.0.get(environment)
    }
}

#[derive(Debug, Serialize)]
pub struct TrailReport {
    /// True when none of `problems` is fatal.
    pub ok: bool,
    pub strict: bool,
    /// `None` when the chain is broken: then the only problem is the
    /// chain's and no Ticket is read.
    pub chain: Option<ChainHead>,
    pub tickets: Vec<TicketTrail>,
    pub problems: Vec<Found>,
}

#[derive(Debug, Serialize)]
pub struct ChainHead {
    pub events: u64,
    pub head: String,
}

#[derive(Debug, Serialize)]
pub struct TicketTrail {
    pub ticket: TicketId,
    /// `None` when the Store has no row for it.
    pub title: Option<String>,
    pub secrets: Vec<SecretUse>,
    /// In the order of their first event on the chain.
    pub assignments: Vec<AssignmentTrail>,
    pub tally: Tally,
}

#[derive(Debug, Serialize)]
pub struct AssignmentTrail {
    pub assignment: AssignmentId,
    /// The reason the chain ended it with, once it has.
    pub ended: Option<String>,
    /// In the order of their `TrailAsked`.
    pub steps: Vec<Step>,
}

#[derive(Debug, Serialize)]
pub struct Step {
    pub n: u32,
    pub asked_ms: Ms,
    pub action: TrailAction,
    /// `None`: asked and never answered.
    pub answer: Option<TrailOutcome>,
}

#[derive(Debug, Default, Serialize)]
pub struct Tally {
    pub steps: u32,
    pub signatures_ok: u32,
    /// Distinct blobs of the Ticket that were read and hash to their digest.
    pub blobs_ok: u32,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SecretUse {
    Asked {
        at_ms: Ms,
        secret: SecretId,
    },
    Answered {
        at_ms: Ms,
        secret: SecretId,
        by: Actor,
        granted: bool,
    },
    KeyUsed {
        at_ms: Ms,
        assignment: AssignmentId,
        secret: SecretId,
        method: String,
        url: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Found {
    pub fatal: bool,
    #[serde(flatten)]
    pub problem: Problem,
}

impl fmt::Display for Found {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = if self.fatal { "problem" } else { "warning" };
        write!(f, "{label}: {}", self.problem)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StepAt {
    pub ticket: TicketId,
    pub assignment: AssignmentId,
    pub n: u32,
}

/// Everything `agent trail` can find wrong.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Problem {
    Chain {
        seq: u64,
        reason: String,
    },
    /// A `TrailDone` with no unanswered `TrailAsked` of its Assignment and
    /// number before it.
    Unpaired {
        at: StepAt,
    },
    /// An Assignment's asks run 1, 2, 3 in chain order, and `at.n` came
    /// where `expected` should have.
    StepGap {
        at: StepAt,
        expected: u32,
    },
    /// An Assignment's last ask, never answered, which is what a host that
    /// stops in the middle of a step leaves.
    Unanswered {
        at: StepAt,
    },
    /// Never answered, though the Assignment asked step `by` after it.
    Overtaken {
        at: StepAt,
        by: u32,
    },
    /// Answered with no signed step, on the agent's word alone.
    Lost {
        at: StepAt,
        why: String,
    },
    /// `solos_agent_core::trail::answers` refused the signed step.
    Mismatch {
        at: StepAt,
        why: String,
    },
    /// No key is trusted for the step's environment. Once per Ticket and
    /// environment.
    Untrusted {
        at: StepAt,
        environment: String,
    },
    Foreign {
        at: StepAt,
        signer: TrailKey,
    },
    BadSignature {
        at: StepAt,
    },
    SessionGap {
        at: StepAt,
        session: String,
        expected: u32,
        found: u32,
    },
    NoProvenance {
        at: StepAt,
        session: String,
    },
    /// `at` is the session's last signed step.
    Unclosed {
        at: StepAt,
        session: String,
    },
    /// Once per Ticket and digest, at the first step that names it.
    BlobMissing {
        at: StepAt,
        digest: Digest,
    },
    BlobCorrupt {
        at: StepAt,
        digest: Digest,
    },
}

impl Problem {
    /// Only an Assignment's last unanswered ask and a lost answer are
    /// warnings, and `strict` counts them too.
    pub fn fatal(&self, strict: bool) -> bool {
        match self {
            Self::Unanswered { .. } | Self::Lost { .. } => strict,
            _ => true,
        }
    }

    pub(super) fn ticket(&self) -> Option<&TicketId> {
        match self {
            Self::Chain { .. } => None,
            Self::Unpaired { at }
            | Self::StepGap { at, .. }
            | Self::Unanswered { at }
            | Self::Overtaken { at, .. }
            | Self::Lost { at, .. }
            | Self::Mismatch { at, .. }
            | Self::Untrusted { at, .. }
            | Self::Foreign { at, .. }
            | Self::BadSignature { at }
            | Self::SessionGap { at, .. }
            | Self::NoProvenance { at, .. }
            | Self::Unclosed { at, .. }
            | Self::BlobMissing { at, .. }
            | Self::BlobCorrupt { at, .. } => Some(&at.ticket),
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let step = |at: &StepAt| format!("step {} of {}: ", at.n, at.assignment);
        match self {
            Self::Chain { seq, .. } => write!(
                f,
                "the event log fails verification at event {seq}, so no trail in it is checked"
            ),
            Self::Unpaired { at } => write!(
                f,
                "{}an answer came with no unanswered step before it",
                step(at)
            ),
            Self::StepGap { at, expected } => write!(
                f,
                "{}asked where step {expected} should have been",
                step(at)
            ),
            Self::Unanswered { at } => write!(
                f,
                "{}asked and never answered, as when the host stops in the middle of a step",
                step(at)
            ),
            Self::Overtaken { at, by } => write!(
                f,
                "{}asked and never answered, and step {by} was asked after it",
                step(at)
            ),
            Self::Lost { at, why } => write!(
                f,
                "{}answered with no signed step, on the agent's word alone: {}",
                step(at),
                escaped(why)
            ),
            Self::Mismatch { at, why } => write!(
                f,
                "{}the signed step does not answer what was asked: {}",
                step(at),
                escaped(why)
            ),
            Self::Untrusted { at, environment } => write!(
                f,
                "{}no key is trusted for environment {}; pass --trust {}=<key>",
                step(at),
                escaped(environment),
                escaped(environment)
            ),
            Self::Foreign { at, signer } => write!(
                f,
                "{}signed by {signer}, which is not the trusted key",
                step(at)
            ),
            Self::BadSignature { at } => {
                write!(f, "{}the signature does not verify", step(at))
            }
            Self::SessionGap {
                at,
                session,
                expected,
                found,
            } => write!(
                f,
                "{}session {} counted {found} where {expected} was next",
                step(at),
                escaped(session)
            ),
            Self::NoProvenance { at, session } => write!(
                f,
                "{}session {} began with no record of the build it ran",
                step(at),
                escaped(session)
            ),
            Self::Unclosed { at, session } => write!(
                f,
                "{}session {} was never closed after this step",
                step(at),
                escaped(session)
            ),
            Self::BlobMissing { at, digest } => {
                write!(f, "{}the blob {digest} is missing", step(at))
            }
            Self::BlobCorrupt { at, digest } => write!(
                f,
                "{}the blob {digest} is corrupt: its bytes hash to another digest",
                step(at)
            ),
        }
    }
}

pub(super) fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() || changes_direction(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// The Unicode marks, embeddings, overrides and isolates that reorder how a
/// terminal shows the text around them.
fn changes_direction(c: char) -> bool {
    matches!(
        c,
        '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// Check the trail of `only`, or of every Ticket, in the Store of
/// `state_dir`, with its blobs under `blobs`. Read-only, so it is safe
/// beside a serving agent.
pub fn verify(
    state_dir: &Path,
    blobs: &Path,
    trust: &Trust,
    only: Option<&TicketId>,
    strict: bool,
) -> Result<TrailReport> {
    let path = state_dir.join(STORE_FILE);
    let store = SqliteStore::open_read_only(&path)
        .with_context(|| format!("cannot open {} to read", path.display()))?;
    let (chain, events) = match store.verified_events() {
        Ok(verified) => verified,
        Err(StoreError::Chain { seq, reason }) => {
            let problem = Problem::Chain {
                seq,
                reason: reason.to_owned(),
            };
            return Ok(report(None, Vec::new(), vec![problem], strict));
        }
        Err(err) => return Err(err).context("cannot verify the event chain"),
    };
    let Walked {
        mut tickets,
        mut problems,
    } = walk(events, only);
    for ticket in &mut tickets {
        ticket.title = store.read_ticket(&ticket.ticket)?.map(|row| row.title);
        check(ticket, trust, blobs, &mut problems)?;
    }
    let head = ChainHead {
        events: chain.len,
        head: chain.head,
    };
    Ok(report(Some(head), tickets, problems, strict))
}

fn report(
    chain: Option<ChainHead>,
    tickets: Vec<TicketTrail>,
    problems: Vec<Problem>,
    strict: bool,
) -> TrailReport {
    let problems: Vec<Found> = problems
        .into_iter()
        .map(|problem| Found {
            fatal: problem.fatal(strict),
            problem,
        })
        .collect();
    TrailReport {
        ok: !problems.iter().any(|found| found.fatal),
        strict,
        chain,
        tickets,
        problems,
    }
}

struct Walked {
    tickets: Vec<TicketTrail>,
    problems: Vec<Problem>,
}

fn walk(events: Vec<Event>, only: Option<&TicketId>) -> Walked {
    let wanted = |ticket: &TicketId| only.is_none_or(|only| only == ticket);
    let mut walk = Walk::default();
    for Event { at_ms, event } in events {
        match event {
            DomainEvent::TrailAsked {
                ticket,
                assignment,
                n,
                action,
            } if wanted(&ticket) => {
                let step = Step {
                    n,
                    asked_ms: at_ms,
                    action,
                    answer: None,
                };
                let problem = walk.ticket(&ticket).assignment(&assignment).ask(step);
                walk.problems.extend(problem);
            }
            DomainEvent::TrailDone {
                ticket,
                assignment,
                n,
                outcome,
            } if wanted(&ticket) => {
                let problem = walk
                    .ticket(&ticket)
                    .assignment(&assignment)
                    .answer(n, outcome);
                walk.problems.extend(problem);
            }
            DomainEvent::AssignmentEnded {
                ticket,
                assignment,
                reason,
            } if wanted(&ticket) => walk.end(&ticket, &assignment, reason),
            DomainEvent::SecretAsked { ticket, secret } if wanted(&ticket) => {
                walk.ticket(&ticket)
                    .secrets
                    .push(SecretUse::Asked { at_ms, secret });
            }
            DomainEvent::SecretAnswered {
                ticket,
                secret,
                by,
                granted,
            } if wanted(&ticket) => walk.ticket(&ticket).secrets.push(SecretUse::Answered {
                at_ms,
                secret,
                by,
                granted,
            }),
            DomainEvent::KeyUsed {
                ticket,
                assignment,
                secret,
                method,
                url,
            } if wanted(&ticket) => walk.ticket(&ticket).secrets.push(SecretUse::KeyUsed {
                at_ms,
                assignment,
                secret,
                method,
                url,
            }),
            _ => {}
        }
    }
    Walked {
        tickets: walk.tickets,
        problems: walk.problems,
    }
}

#[derive(Default)]
struct Walk {
    tickets: Vec<TicketTrail>,
    at: BTreeMap<TicketId, usize>,
    problems: Vec<Problem>,
}

impl Walk {
    /// Only an Assignment that browsed has a trail to end, and the core asks
    /// no step of an Assignment that has ended, save its close.
    fn end(&mut self, ticket: &TicketId, assignment: &AssignmentId, reason: String) {
        let Some(&at) = self.at.get(ticket) else {
            return;
        };
        let trail = self.tickets[at]
            .assignments
            .iter_mut()
            .find(|each| each.assignment == *assignment);
        if let Some(trail) = trail {
            trail.ended = Some(reason);
        }
    }

    fn ticket(&mut self, id: &TicketId) -> &mut TicketTrail {
        let next = self.tickets.len();
        let at = *self.at.entry(id.clone()).or_insert(next);
        if at == next {
            self.tickets.push(TicketTrail {
                ticket: id.clone(),
                title: None,
                secrets: Vec::new(),
                assignments: Vec::new(),
                tally: Tally::default(),
            });
        }
        &mut self.tickets[at]
    }
}

impl TicketTrail {
    fn assignment(&mut self, id: &AssignmentId) -> AssignmentAt<'_> {
        let at = match self
            .assignments
            .iter()
            .position(|assignment| assignment.assignment == *id)
        {
            Some(at) => at,
            None => {
                self.assignments.push(AssignmentTrail {
                    assignment: id.clone(),
                    ended: None,
                    steps: Vec::new(),
                });
                self.assignments.len() - 1
            }
        };
        AssignmentAt {
            ticket: &self.ticket,
            trail: &mut self.assignments[at],
        }
    }
}

struct AssignmentAt<'a> {
    ticket: &'a TicketId,
    trail: &'a mut AssignmentTrail,
}

impl AssignmentAt<'_> {
    fn at(&self, n: u32) -> StepAt {
        StepAt {
            ticket: self.ticket.clone(),
            assignment: self.trail.assignment.clone(),
            n,
        }
    }

    fn ask(self, step: Step) -> Option<Problem> {
        let expected = self
            .trail
            .steps
            .last()
            .map_or(1, |last| last.n.saturating_add(1));
        let gap = (step.n != expected).then(|| Problem::StepGap {
            at: self.at(step.n),
            expected,
        });
        self.trail.steps.push(step);
        gap
    }

    fn answer(self, n: u32, outcome: TrailOutcome) -> Option<Problem> {
        let waiting = self
            .trail
            .steps
            .iter_mut()
            .rev()
            .find(|step| step.n == n && step.answer.is_none());
        match waiting {
            Some(step) => {
                step.answer = Some(outcome);
                None
            }
            None => Some(Problem::Unpaired { at: self.at(n) }),
        }
    }
}

fn check(
    ticket: &mut TicketTrail,
    trust: &Trust,
    blobs: &Path,
    found: &mut Vec<Problem>,
) -> Result<()> {
    let mut checking = Checking {
        trust,
        blobs,
        ticket: &ticket.ticket,
        tally: &mut ticket.tally,
        untrusted: BTreeSet::new(),
        named: BTreeSet::new(),
        found,
    };
    for assignment in &ticket.assignments {
        let mut sessions = Sessions::default();
        for (at, step) in assignment.steps.iter().enumerate() {
            checking.tally.steps += 1;
            let here = StepAt {
                ticket: ticket.ticket.clone(),
                assignment: assignment.assignment.clone(),
                n: step.n,
            };
            match &step.answer {
                None => {
                    let problem = match assignment.steps.get(at + 1) {
                        Some(next) => Problem::Overtaken {
                            at: here,
                            by: next.n,
                        },
                        None => Problem::Unanswered { at: here },
                    };
                    checking.found.push(problem);
                    sessions.hole();
                }
                Some(TrailOutcome::Lost { why }) => {
                    let why = why.clone();
                    checking.found.push(Problem::Lost { at: here, why });
                    sessions.hole();
                }
                Some(TrailOutcome::Signed(signed)) => {
                    checking.signed(here, step, signed, &mut sessions)?;
                }
            }
        }
        sessions.end(assignment.ended.as_deref(), checking.found);
    }
    Ok(())
}

struct Checking<'a> {
    trust: &'a Trust,
    blobs: &'a Path,
    ticket: &'a TicketId,
    tally: &'a mut Tally,
    untrusted: BTreeSet<String>,
    named: BTreeSet<Digest>,
    found: &'a mut Vec<Problem>,
}

impl Checking<'_> {
    fn signed(
        &mut self,
        at: StepAt,
        step: &Step,
        signed: &SignedStep,
        sessions: &mut Sessions,
    ) -> Result<()> {
        let pending = Pending {
            n: step.n,
            action: step.action.clone(),
            asked_ms: step.asked_ms,
        };
        let answered = match answers(&pending, &at.ticket, &at.assignment, signed) {
            Ok(()) => true,
            Err(why) => {
                self.found.push(Problem::Mismatch {
                    at: at.clone(),
                    why,
                });
                false
            }
        };
        let verified = self.signature(&at, signed);
        for digest in named_blobs(&signed.body) {
            self.blob(&at, digest)?;
        }
        if verified && answered {
            sessions.signed(at, signed, self.found);
        } else {
            sessions.hole();
        }
        Ok(())
    }

    fn signature(&mut self, at: &StepAt, signed: &SignedStep) -> bool {
        let environment = &signed.body.environment;
        let Some(key) = self.trust.key(environment) else {
            if self.untrusted.insert(environment.clone()) {
                self.found.push(Problem::Untrusted {
                    at: at.clone(),
                    environment: environment.clone(),
                });
            }
            return false;
        };
        match verify_signature(signed, key) {
            Ok(()) => {
                self.tally.signatures_ok += 1;
                true
            }
            Err(BadSignature::Foreign { signer, .. }) => {
                self.found.push(Problem::Foreign {
                    at: at.clone(),
                    signer,
                });
                false
            }
            Err(BadSignature::Invalid) => {
                self.found.push(Problem::BadSignature { at: at.clone() });
                false
            }
        }
    }

    fn blob(&mut self, at: &StepAt, digest: &Digest) -> Result<()> {
        if !self.named.insert(digest.clone()) {
            return Ok(());
        }
        match read_blob(self.blobs, self.ticket, digest)? {
            None => self.found.push(Problem::BlobMissing {
                at: at.clone(),
                digest: digest.clone(),
            }),
            Some(bytes) if Digest::of(&bytes) != *digest => {
                self.found.push(Problem::BlobCorrupt {
                    at: at.clone(),
                    digest: digest.clone(),
                });
            }
            Some(_) => self.tally.blobs_ok += 1,
        }
        Ok(())
    }
}

/// A refusal echoes its asked action, typed digest and all, but a full
/// store refuses before it keeps the typed text.
fn named_blobs(body: &StepBody) -> Vec<&Digest> {
    let mut named = Vec::new();
    if let StepOutcome::Page(page) = &body.outcome {
        named.extend([&page.snapshot, &page.view]);
    }
    if let TrailAction::Act {
        text: Some(ActText::Typed { digest, .. }),
        ..
    } = &body.action
        && !matches!(body.outcome, StepOutcome::Refused { .. })
    {
        named.push(digest);
    }
    named
}

/// A step with no verified answer to its ask is a hole: its signer may have
/// counted it, so a hole may stand in a gap of `session_step`, and one after
/// a session's last step may be where the session ended.
#[derive(Default)]
struct Sessions {
    current: Option<Open>,
    holes: u32,
}

struct Open {
    id: String,
    next: u32,
    last: StepAt,
    closed: bool,
}

impl Sessions {
    fn hole(&mut self) {
        self.holes += 1;
    }

    fn signed(&mut self, at: StepAt, step: &SignedStep, found: &mut Vec<Problem>) {
        let body = &step.body;
        if body.session == NO_SESSION {
            return;
        }
        let open = match self.current.take() {
            Some(open) if open.id == body.session => self.current.insert(open),
            previous => {
                if let Some(previous) = previous
                    && !previous.closed
                    && self.holes == 0
                {
                    found.push(Problem::Unclosed {
                        at: previous.last,
                        session: previous.id,
                    });
                }
                self.current.insert(Open {
                    id: body.session.clone(),
                    next: 0,
                    last: at.clone(),
                    closed: false,
                })
            }
        };
        let counted = body.session_step;
        if counted < open.next || counted - open.next > self.holes {
            found.push(Problem::SessionGap {
                at: at.clone(),
                session: body.session.clone(),
                expected: open.next,
                found: counted,
            });
        }
        if counted == 0 && body.provenance.is_none() {
            found.push(Problem::NoProvenance {
                at: at.clone(),
                session: body.session.clone(),
            });
        }
        open.next = counted.saturating_add(1);
        open.last = at;
        open.closed = matches!(body.outcome, StepOutcome::Closed { .. });
        self.holes = 0;
    }

    /// While the chain has not ended its Assignment, an open browser is a
    /// live one, and a restarted host lost its browser with it.
    fn end(self, ended: Option<&str>, found: &mut Vec<Problem>) {
        let over = ended.is_some_and(|reason| reason != HOST_RESTARTED);
        if let Some(open) = self.current
            && !open.closed
            && self.holes == 0
            && over
        {
            found.push(Problem::Unclosed {
                at: open.last,
                session: open.id,
            });
        }
    }
}

/// Which blob of a step `--step` prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Shown {
    /// The exact bytes the model got.
    View,
    /// The page as captured, scrubbed.
    Snapshot,
}

/// The bytes of step `n`'s blob, when they hash to the digest its signed
/// step names. Whether that step verifies is the report's to say.
pub fn shown(
    report: &TrailReport,
    blobs: &Path,
    assignment: Option<&AssignmentId>,
    n: u32,
    what: Shown,
) -> Result<Vec<u8>> {
    let [ticket] = report.tickets.as_slice() else {
        bail!("name one Ticket that has browser steps");
    };
    let steps: Vec<_> = ticket
        .assignments
        .iter()
        .filter(|each| assignment.is_none_or(|named| *named == each.assignment))
        .filter_map(|each| {
            let step = each.steps.iter().find(|step| step.n == n)?;
            Some((&each.assignment, step))
        })
        .collect();
    let (assignment, step) = match steps.as_slice() {
        [] => bail!("{} has no step {n}", ticket.ticket),
        [one] => *one,
        many => {
            let names: Vec<_> = many.iter().map(|(each, _)| each.as_str()).collect();
            bail!(
                "step {n} is in {} Assignments of {}, so name one with --assignment: {}",
                many.len(),
                ticket.ticket,
                names.join(", ")
            );
        }
    };
    let signed = match &step.answer {
        Some(TrailOutcome::Signed(signed)) => signed,
        Some(TrailOutcome::Lost { why }) => {
            bail!(
                "step {n} of {assignment} has no signed answer: {}",
                escaped(why)
            )
        }
        None => bail!("step {n} of {assignment} was never answered"),
    };
    let StepOutcome::Page(page) = &signed.body.outcome else {
        bail!("step {n} of {assignment} captured no page");
    };
    let (label, digest) = match what {
        Shown::View => ("view", &page.view),
        Shown::Snapshot => ("snapshot", &page.snapshot),
    };
    let Some(bytes) = read_blob(blobs, &ticket.ticket, digest)? else {
        bail!("the {label} blob {digest} of step {n} of {assignment} is missing");
    };
    if Digest::of(&bytes) != *digest {
        bail!("the {label} blob {digest} of step {n} of {assignment} does not hash to its digest");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{Trusted, escaped};

    const KEY: &str = "ed25519:9c1d4f2a5b6c7d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f506";

    #[test]
    fn a_trust_flag_names_an_environment_and_its_key() {
        let trusted: Trusted = format!("laptop={KEY}").parse().unwrap();
        assert_eq!(trusted.environment, "laptop");
        assert_eq!(trusted.key.as_str(), KEY);

        assert!(
            KEY.parse::<Trusted>()
                .unwrap_err()
                .contains("<environment>="),
            "no '=' names no environment"
        );
        assert!(
            format!("={KEY}")
                .parse::<Trusted>()
                .unwrap_err()
                .contains("empty"),
            "an empty environment is refused"
        );
        assert!(
            "laptop=ed25519:9c1d"
                .parse::<Trusted>()
                .unwrap_err()
                .contains("64 lowercase hex digits"),
            "a short key is refused"
        );
    }

    #[test]
    fn words_a_page_wrote_cannot_start_a_line_or_drive_a_terminal() {
        assert_eq!(escaped("Sign in"), "Sign in");
        assert_eq!(
            escaped("a\n  problem: none\u{1b}[2J\u{202e}"),
            "a\\n  problem: none\\u{1b}[2J\\u{202e}"
        );
    }
}
