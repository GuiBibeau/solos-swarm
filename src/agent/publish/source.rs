//! Reads the Store, read-only, and builds the snapshots. It keeps its
//! connection open and a position in the event log and the Effect queue, so a
//! look costs one pragma when nothing was written and reads only what is new
//! when something was. The Store is never written: the connection is opened
//! with [`SqliteStore::open_read_only`], which SQLite itself enforces.

use super::snapshot::{
    self, AssignmentBrief, AssignmentView, BrowserView, Delivery, FORMAT, Index, IndexRow,
    MAX_REPORTS, PageView, PlanView, TicketSnapshot, Turn,
};
use crate::agent::broker::Account;
use crate::agent::watchdog::stage_name;
use anyhow::{Context, Result, bail};
use solos_agent_core::ids::key;
use solos_agent_core::model::{
    AssignmentId, DomainEvent, Effect, Input, InputBody, MessageKind, Ms, OutboundMessage, Plan,
    Stage, Ticket, TicketId, Trigger,
};
use solos_agent_core::trail::{StepOutcome, TrailAction, TrailOutcome};
use solos_agent_store::{EffectId, SqliteStore};
use std::collections::{BTreeMap, VecDeque};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// How many ended Tickets the index lists.
pub const ENDED_IN_INDEX: usize = 100;
/// The pages of a Ticket's browser trail that are kept.
const MAX_PAGES: usize = 40;
/// Rows read from the log or the queue at once.
const PAGE: usize = 2_000;

/// What one look found: the index and one snapshot per Ticket in it.
#[derive(Clone, Debug)]
pub struct Built {
    pub index: Index,
    pub tickets: Vec<(TicketId, TicketSnapshot)>,
}

/// An agent.db opened to read, and what was folded from its logs so far.
pub struct Source {
    path: PathBuf,
    store: SqliteStore,
    /// The file the connection is on; a replaced file needs a new [`Source`].
    identity: Option<(u64, u64)>,
    /// `data_version` when the last build began.
    version: Option<i64>,
    events_seq: u64,
    effects_id: i64,
    /// When each Ticket that ended last ended, by the event log. A Ticket
    /// resumed since is not in it.
    ended_at: BTreeMap<TicketId, Ms>,
    traces: BTreeMap<TicketId, Trace>,
    replies: BTreeMap<TicketId, Vec<Reply>>,
}

/// What the event log says about one Ticket.
#[derive(Default)]
struct Trace {
    last_ms: Ms,
    assignments: BTreeMap<AssignmentId, Span>,
    browser: Browse,
}

#[derive(Default)]
struct Span {
    started: Option<Ms>,
    ended: Option<Ms>,
}

#[derive(Default)]
struct Browse {
    steps: u32,
    refused: u32,
    lost: u32,
    closed: u32,
    needs_approval: Vec<String>,
    /// Steps asked and not yet answered, and what they asked for.
    asked: BTreeMap<(AssignmentId, u32), String>,
    pages: VecDeque<PageView>,
    omitted: u32,
}

/// An agent reply, as the Effect queue holds it.
struct Reply {
    id: EffectId,
    message: OutboundMessage,
}

impl Source {
    /// Open `<state_dir>/agent.db` to read. A missing file is an error, and
    /// no file is created.
    pub fn open(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(crate::agent::STORE_FILE);
        let identity = identity(&path);
        if identity.is_none() {
            bail!("{} does not exist yet", path.display());
        }
        let store = SqliteStore::open_read_only(&path)
            .with_context(|| format!("cannot open {} to read", path.display()))?;
        Ok(Self {
            path,
            store,
            identity,
            version: None,
            events_seq: 0,
            effects_id: 0,
            ended_at: BTreeMap::new(),
            traces: BTreeMap::new(),
            replies: BTreeMap::new(),
        })
    }

    /// Whether the file at the path is no longer the one this holds open: a
    /// restore of a backup replaces it, and a connection on the old file
    /// would go on reading the old Store.
    pub fn replaced(&self) -> bool {
        identity(&self.path) != self.identity
    }

    /// Whether anything was committed since the last [`Self::build`].
    pub fn changed(&self) -> Result<bool> {
        Ok(self.version != Some(self.store.data_version()?))
    }

    /// Fold what is new in the logs and build every snapshot.
    pub fn build(&mut self) -> Result<Built> {
        // Before reading, so a commit that lands during the build counts as
        // a change next time.
        let version = self.store.data_version()?;
        self.catch_up()?;
        let pending: BTreeMap<EffectId, (u32, Option<String>)> = self
            .store
            .pending_effects()?
            .into_iter()
            .map(|queued| (queued.id, (queued.attempts, queued.last_error)))
            .collect();

        let mut tickets = Vec::new();
        for id in self.store.open_tickets()? {
            tickets.extend(self.store.read_ticket(&id)?);
        }
        let open = tickets.len();
        let mut newest: Vec<(&TicketId, &Ms)> = self.ended_at.iter().collect();
        newest.sort_by(|a, b| b.1.cmp(a.1).then(b.0.cmp(a.0)));
        let mut ended = Vec::new();
        for (id, _) in newest {
            if ended.len() == ENDED_IN_INDEX {
                break;
            }
            if let Some(ticket) = self.store.read_ticket(id)?
                && ticket.ended.is_some()
            {
                ended.push(ticket);
            }
        }
        tickets.extend(ended);

        let mut rows = Vec::new();
        let mut snapshots = Vec::new();
        for ticket in &tickets {
            let (row, snapshot) = self.ticket(ticket, &pending)?;
            rows.push(row);
            snapshots.push((ticket.id.clone(), snapshot));
        }
        // Open Tickets by what moved last, then ended ones the same way.
        let split = open.min(rows.len());
        let (open_rows, ended_rows) = rows.split_at_mut(split);
        open_rows.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then(a.id.cmp(&b.id)));
        ended_rows.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then(a.id.cmp(&b.id)));
        self.version = Some(version);
        Ok(Built {
            index: Index {
                v: FORMAT,
                generated_at: None,
                open,
                tickets: rows,
            },
            tickets: snapshots,
        })
    }

    fn catch_up(&mut self) -> Result<()> {
        loop {
            let events = self.store.events_after(self.events_seq, PAGE)?;
            let Some(&(last, _)) = events.last() else {
                break;
            };
            for (_, event) in &events {
                self.fold_event(event.at_ms, &event.event);
            }
            self.events_seq = last;
        }
        loop {
            let effects = self.store.effects_after(EffectId(self.effects_id), PAGE)?;
            let Some(last) = effects.last().map(|queued| queued.id) else {
                break;
            };
            for queued in effects {
                if let Effect::Send(message) = queued.effect
                    && let Some(ticket) = message.ticket.clone()
                {
                    self.replies.entry(ticket).or_default().push(Reply {
                        id: queued.id,
                        message,
                    });
                }
            }
            self.effects_id = last.0;
        }
        Ok(())
    }

    fn fold_event(&mut self, at_ms: Ms, event: &DomainEvent) {
        let touch = |source: &mut Self, ticket: &TicketId| {
            let trace = source.traces.entry(ticket.clone()).or_default();
            trace.last_ms = trace.last_ms.max(at_ms);
        };
        match event {
            DomainEvent::TriggerReceived { reached, .. } => {
                for ticket in reached {
                    touch(self, ticket);
                }
            }
            DomainEvent::EvalStarted { .. }
            | DomainEvent::EvalExpired { .. }
            | DomainEvent::DecisionRecorded { .. }
            | DomainEvent::MemoProposed { .. }
            | DomainEvent::MemoActivated { .. }
            | DomainEvent::MemoRetired { .. }
            | DomainEvent::MemoForgotten { .. }
            | DomainEvent::MemosShown { .. } => {}
            DomainEvent::TicketCreated { ticket, .. }
            | DomainEvent::TicketApproved { ticket, .. }
            | DomainEvent::HandedBack { ticket, .. }
            | DomainEvent::SecretAsked { ticket, .. }
            | DomainEvent::SecretAnswered { ticket, .. }
            | DomainEvent::KeyUsed { ticket, .. }
            | DomainEvent::ProposalAdmitted { ticket, .. }
            | DomainEvent::Delivered { ticket, .. }
            | DomainEvent::Withheld { ticket, .. }
            | DomainEvent::Reminded { ticket, .. }
            | DomainEvent::Reported { ticket, .. } => touch(self, ticket),
            DomainEvent::TicketResumed { ticket, .. } => {
                touch(self, ticket);
                self.ended_at.remove(ticket);
            }
            DomainEvent::TicketEnded { ticket, .. } => {
                touch(self, ticket);
                self.ended_at.insert(ticket.clone(), at_ms);
            }
            DomainEvent::AssignmentStarted { ticket, assignment } => {
                touch(self, ticket);
                let span = self.span(ticket, assignment);
                span.started = Some(at_ms);
            }
            DomainEvent::AssignmentEnded {
                ticket, assignment, ..
            } => {
                touch(self, ticket);
                let span = self.span(ticket, assignment);
                span.ended = Some(at_ms);
            }
            DomainEvent::TrailAsked {
                ticket,
                assignment,
                n,
                action,
            } => {
                touch(self, ticket);
                let browse = &mut self.trace(ticket).browser;
                browse.steps += 1;
                browse
                    .asked
                    .insert((assignment.clone(), *n), action_label(action));
            }
            DomainEvent::TrailDone {
                ticket,
                assignment,
                n,
                outcome,
            } => {
                touch(self, ticket);
                let browse = &mut self.trace(ticket).browser;
                let action = browse
                    .asked
                    .remove(&(assignment.clone(), *n))
                    .unwrap_or_default();
                browse.answer(*n, action, outcome);
            }
        }
    }

    fn trace(&mut self, ticket: &TicketId) -> &mut Trace {
        self.traces.entry(ticket.clone()).or_default()
    }

    fn span(&mut self, ticket: &TicketId, assignment: &AssignmentId) -> &mut Span {
        self.trace(ticket)
            .assignments
            .entry(assignment.clone())
            .or_default()
    }

    /// One Ticket's index row and snapshot.
    fn ticket(
        &self,
        ticket: &Ticket,
        pending: &BTreeMap<EffectId, (u32, Option<String>)>,
    ) -> Result<(IndexRow, TicketSnapshot)> {
        let trace = self.traces.get(&ticket.id);
        let stage = ticket.stage();
        let updated_ms = snapshot::touched_ms(ticket).max(trace.map_or(0, |trace| trace.last_ms));
        let spent = self
            .store
            .committed(&Account::ticket_key(&ticket.id), 0)
            .with_context(|| format!("spend of {}", ticket.id))?;
        let origin = self.store.read_trigger(&ticket.origin)?;
        let requester = snapshot::Person::of(&ticket.mandate.requester);
        let ended_ms = ticket.ended.as_ref().map(|ended| ended.at_ms);
        let waiting_for = ticket
            .attention
            .as_deref()
            .map(|text| snapshot::one_line(text, 300))
            .or_else(|| {
                (stage == Stage::Approval).then(|| "A person must approve the budget".to_owned())
            });
        let assignment = ticket.latest_assignment().map(|latest| AssignmentBrief {
            state: snapshot::assignment_state(latest.state),
            report: latest
                .reports
                .last()
                .map(|report| snapshot::report_status(report.status)),
            reports: latest.reports.len(),
        });
        let row = IndexRow {
            id: ticket.id.to_string(),
            title: ticket.title.clone(),
            stage: stage_name(stage),
            open: ticket.ended.is_none(),
            outcome: ticket
                .ended
                .as_ref()
                .map(|ended| snapshot::outcome(ended.outcome)),
            created_ms: ticket.created_ms,
            created_at: snapshot::iso(ticket.created_ms),
            updated_ms,
            updated_at: snapshot::iso(updated_ms),
            duration_ms: ended_ms.map(|at| at.saturating_sub(ticket.created_ms)),
            source: ticket.mandate.source.clone(),
            source_url: origin.as_ref().and_then(snapshot::source_url),
            asker: requester.clone(),
            spent_microusd: spent,
            budget_microusd: ticket.mandate.budget_microusd,
            assignment,
            waiting_for,
        };

        let request = origin.as_ref().map(|trigger| turn_of(trigger, "request"));
        let mut items: Vec<(u64, Turn)> = Vec::new();
        for input in &ticket.inputs {
            items.extend(self.input_turn(input)?.map(|turn| (input.seq, turn)));
        }
        items.extend(self.reply_turns(ticket, pending));
        // The counter is the order the Ticket's inputs, reports and replies
        // happened in; a reply whose number is unknown goes last.
        items.sort_by_key(|(seq, _)| *seq);

        let snapshot = TicketSnapshot {
            v: FORMAT,
            id: ticket.id.to_string(),
            title: ticket.title.clone(),
            stage: stage_name(stage),
            open: ticket.ended.is_none(),
            created_ms: ticket.created_ms,
            created_at: snapshot::iso(ticket.created_ms),
            updated_ms,
            updated_at: snapshot::iso(updated_ms),
            ended: ticket.ended.as_ref().map(|ended| snapshot::EndedView {
                at_ms: ended.at_ms,
                at: snapshot::iso(ended.at_ms),
                by: match ended.by {
                    solos_agent_core::model::EndedBy::Person => "person",
                    solos_agent_core::model::EndedBy::Executor => "executor",
                    solos_agent_core::model::EndedBy::Policy => "policy",
                },
                outcome: snapshot::outcome(ended.outcome),
                reason: ended.reason.clone(),
            }),
            scope: ticket.mandate.scope.clone(),
            plan: plan_view(&ticket.plan),
            repo: ticket.mandate.repo.as_ref().map(ToString::to_string),
            deliverable: match &ticket.plan {
                Plan::Session {
                    method: Some(method),
                    ..
                } => method.deliverable.as_str(),
                _ => "answer",
            },
            approval: snapshot::approval_view(&ticket.approval),
            budget_microusd: ticket.mandate.budget_microusd,
            spent_microusd: spent,
            attention: ticket.attention.clone(),
            source: ticket.mandate.source.clone(),
            source_url: origin.as_ref().and_then(snapshot::source_url),
            requester,
            request,
            conversation: items.into_iter().map(|(_, turn)| turn).collect(),
            decisions: self.decisions(ticket)?,
            assignments: assignments(ticket, trace),
            browser: trace.and_then(|trace| trace.browser.view()),
        };
        Ok((row, snapshot))
    }

    /// A Trigger the Ticket received, or the host's own note to it.
    fn input_turn(&self, input: &Input) -> Result<Option<Turn>> {
        let turn = |from, who, kind, text: String| {
            let (text, truncated) = snapshot::clip_text(&text);
            Turn {
                at_ms: input.at_ms,
                at: snapshot::iso(input.at_ms),
                from,
                who,
                kind,
                text,
                truncated,
                channel: None,
                delivery: None,
            }
        };
        let who = |actor: &solos_agent_core::model::Actor| {
            Some(snapshot::Person::of(actor).label().to_owned())
        };
        Ok(match &input.body {
            InputBody::Trigger { trigger } => self
                .store
                .read_trigger(trigger)?
                .map(|trigger| turn_of(&trigger, "message")),
            InputBody::Note { text } => Some(turn("router", None, "note", text.clone())),
            InputBody::Approved { by } => Some(turn(
                "person",
                who(by),
                "approval",
                "Approved the budget.".to_owned(),
            )),
            InputBody::Secret {
                secret,
                granted,
                by,
            } => Some(turn(
                "person",
                who(by),
                "secret",
                format!("{} {secret}.", if *granted { "Allowed" } else { "Denied" }),
            )),
        })
    }

    /// Every message the agent sent for this Ticket, with where it stands in
    /// the Ticket's own order.
    fn reply_turns(
        &self,
        ticket: &Ticket,
        pending: &BTreeMap<EffectId, (u32, Option<String>)>,
    ) -> Vec<(u64, Turn)> {
        let Some(replies) = self.replies.get(&ticket.id) else {
            return Vec::new();
        };
        // A message's id is a hash of the Ticket, its number on the Ticket's
        // counter and its channel, so the counter can be found again.
        let channels = ticket.channels.len().max(1);
        let mut numbers: BTreeMap<String, u64> = BTreeMap::new();
        for seq in 1..=ticket.seq {
            for n in 0..channels {
                let id = key(
                    "ms",
                    &[ticket.id.as_str(), &seq.to_string(), &n.to_string()],
                );
                numbers.insert(id, seq);
            }
        }
        // What had happened by each number: the time of the latest input or
        // report before it, which is what caused the reply.
        let mut timeline: Vec<(u64, Ms)> = ticket
            .inputs
            .iter()
            .map(|input| (input.seq, input.at_ms))
            .chain(
                ticket
                    .assignments
                    .iter()
                    .flat_map(|a| &a.reports)
                    .map(|report| (report.seq, report.at_ms)),
            )
            .collect();
        timeline.sort_unstable();
        replies
            .iter()
            .map(|reply| {
                let message = &reply.message;
                let seq = numbers.get(message.id.as_str()).copied();
                let at_ms = timeline
                    .iter()
                    .rev()
                    .find(|(earlier, _)| seq.is_some_and(|seq| *earlier < seq))
                    .map_or(ticket.created_ms, |(_, at)| *at);
                let (text, truncated) = snapshot::clip_text(&message.text);
                let (attempts, error) = pending.get(&reply.id).cloned().unzip();
                let turn = Turn {
                    at_ms,
                    at: snapshot::iso(at_ms),
                    from: "agent",
                    who: None,
                    kind: match message.kind {
                        MessageKind::Acknowledgement => "acknowledgement",
                        MessageKind::Result => "result",
                        MessageKind::Attention => "attention",
                    },
                    text,
                    truncated,
                    channel: Some(message.channel.source.clone()),
                    delivery: Some(Delivery {
                        delivered: attempts.is_none(),
                        attempts: attempts.unwrap_or(0),
                        error: error.flatten(),
                    }),
                };
                (seq.unwrap_or(u64::MAX), turn)
            })
            .collect()
    }

    /// The Decisions about this Ticket: the one that made it, and the ones on
    /// its Threads that name it.
    fn decisions(&self, ticket: &Ticket) -> Result<Vec<snapshot::DecisionView>> {
        let mut seen = std::collections::BTreeSet::new();
        let mut found = Vec::new();
        let mut add = |decision: solos_agent_core::model::Decision| {
            if seen.insert(decision.id.clone()) {
                found.push(decision);
            }
        };
        if let Some(decision) = self.store.read_decision(&ticket.decision)? {
            add(decision);
        }
        for key in &ticket.threads {
            let Some(thread) = self.store.read_thread(key)? else {
                continue;
            };
            for id in &thread.decisions {
                if let Some(decision) = self.store.read_decision(id)?
                    && decision.ticket.as_ref() == Some(&ticket.id)
                {
                    add(decision);
                }
            }
        }
        found.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then(a.id.cmp(&b.id)));
        Ok(found.iter().map(snapshot::decision_view).collect())
    }
}

impl Browse {
    fn answer(&mut self, n: u32, action: String, outcome: &TrailOutcome) {
        let TrailOutcome::Signed(step) = outcome else {
            self.lost += 1;
            return;
        };
        match &step.body.outcome {
            StepOutcome::Page(seen) => {
                let Some(url) = snapshot::scrub_url(&seen.url) else {
                    return;
                };
                self.pages.push_back(PageView {
                    n,
                    at: snapshot::iso(step.body.at_ms),
                    action,
                    url,
                    title: snapshot::one_line(&seen.title, 200),
                    status: seen.status,
                });
                if self.pages.len() > MAX_PAGES {
                    self.pages.pop_front();
                    self.omitted += 1;
                }
            }
            StepOutcome::NeedsApproval { label, .. } => {
                let label = snapshot::one_line(label, 100);
                if !self.needs_approval.contains(&label) {
                    self.needs_approval.push(label);
                }
            }
            StepOutcome::Refused { .. } => self.refused += 1,
            StepOutcome::Closed { .. } => self.closed += 1,
        }
    }

    fn view(&self) -> Option<BrowserView> {
        if self.steps == 0 {
            return None;
        }
        Some(BrowserView {
            steps: self.steps,
            refused: self.refused,
            lost: self.lost,
            closed: self.closed,
            needs_approval: self.needs_approval.clone(),
            pages: self.pages.iter().cloned().collect(),
            pages_omitted: self.omitted,
        })
    }
}

fn action_label(action: &TrailAction) -> String {
    use solos_agent_core::trail::PageAction;
    match action {
        TrailAction::Open { .. } => "open".into(),
        TrailAction::Act { action, .. } => match action {
            PageAction::Click => "click",
            PageAction::Type => "type",
            PageAction::Select => "select",
            PageAction::Press => "press",
            PageAction::Scroll => "scroll",
            PageAction::Back => "back",
        }
        .into(),
        TrailAction::Login { .. } => "login".into(),
        TrailAction::Read { .. } => "read".into(),
        TrailAction::Close { .. } => "close".into(),
    }
}

fn plan_view(plan: &Plan) -> PlanView {
    match plan {
        Plan::Session {
            harness,
            environment,
            ..
        } => PlanView {
            kind: "session",
            harness: harness.clone(),
            environment: environment.clone(),
            agents: None,
        },
        Plan::Swarm { agents, .. } => PlanView {
            kind: "swarm",
            harness: None,
            environment: None,
            agents: Some(*agents),
        },
    }
}

fn assignments(ticket: &Ticket, trace: Option<&Trace>) -> Vec<AssignmentView> {
    ticket
        .assignments
        .iter()
        .map(|assignment| {
            let span = trace.and_then(|trace| trace.assignments.get(&assignment.id));
            let skipped = assignment.reports.len().saturating_sub(MAX_REPORTS);
            AssignmentView {
                id: assignment.id.to_string(),
                state: snapshot::assignment_state(assignment.state),
                created_ms: assignment.created_ms,
                created_at: snapshot::iso(assignment.created_ms),
                started_ms: span.and_then(|span| span.started),
                ended_ms: span.and_then(|span| span.ended),
                ended_reason: assignment.ended_reason.clone(),
                report_count: assignment.reports.len(),
                reports: assignment
                    .reports
                    .iter()
                    .skip(skipped)
                    .map(snapshot::report_view)
                    .collect(),
            }
        })
        .collect()
}

/// A Trigger as a message in the conversation.
fn turn_of(trigger: &Trigger, kind: &'static str) -> Turn {
    let who = snapshot::Person::of(&trigger.actor);
    let (text, truncated) = snapshot::clip_text(&trigger.text);
    Turn {
        at_ms: trigger.at_ms,
        at: snapshot::iso(trigger.at_ms),
        from: who.kind,
        who: Some(who.label().to_owned()),
        kind,
        text,
        truncated,
        channel: None,
        delivery: None,
    }
}

/// Which file a path is, so a replaced one is noticed.
fn identity(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}
