//! The Watchdog: a control loop beside the runtime that notices trouble in
//! the infrastructure and work that has stopped making progress, and tells
//! an operator. [`assess`] is pure: it reads a [`Snapshot`] of what the Store
//! already holds and needs no model. [`Watchdog`] runs it on an interval
//! inside `agent serve`, sends each new alert to the operator's channel once,
//! and can retry a stalled Assignment once per Ticket through the runtime,
//! which stays the Engine's one writer. It never widens a Mandate or a budget.
//!
//! It also looks at the external jobs the config lists (see
//! [`external_jobs`]), which are not Tickets: a job that dies, stalls or runs
//! its disk low is one alert, once, and a job that comes back is told once.

use super::STORE_FILE;
use super::broker::Account;
use super::config::{AgentConfig, JobConfig, WatchdogConfig};
use super::dream;
use super::external_jobs::{self, Checker, Record, Recovered, Status};
use super::links::TicketLinks;
use super::ports::{Channel, Fact, FactSink, SendOptions, SendOutcome};
use crate::broker::now_ms;
use crate::gateway;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use solos_agent_core::delivery::ForgeJob;
use solos_agent_core::ids::key;
use solos_agent_core::model::{
    AssignmentId, ChannelRef, Decision, DecisionKind, Effect, MessageId, MessageKind, MicroUsd, Ms,
    OutboundMessage, Plan, Stage, Ticket, TicketId,
};
use solos_agent_store::{AlertRow, QueuedEffect, SpendState, SqliteStore};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

const MINUTE_MS: Ms = 60_000;
const HOUR_MS: Ms = 60 * MINUTE_MS;
/// Bounds the Decisions one assessment reads.
const MAX_DECISIONS: usize = 1_000;
/// Bounds what an alert quotes from a report or a question.
const DETAIL_CHARS: usize = 200;

/// When each signal fires. The defaults suit a person who reads the
/// operator channel a few times a day.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Thresholds {
    /// A Ticket that has waited this long for a person: an approval, an
    /// answer, or a look at its blocked or failed work.
    pub waiting_ms: Ms,
    /// A working Assignment that has sent no Report for this long.
    pub stalled_ms: Ms,
    /// An Effect that has failed this many attempts.
    pub failed_attempts: u32,
    /// This many `unavailable` Decisions within `unavailable_within_ms`.
    pub unavailable: u32,
    pub unavailable_within_ms: Ms,
    /// An account that has committed this share of its cap, in percent.
    pub spend_percent: u64,
    /// A pause the state directory has held this long.
    pub paused_ms: Ms,
    /// How often a problem that lasts is told again.
    pub repeat_ms: Ms,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            waiting_ms: HOUR_MS,
            stalled_ms: 30 * MINUTE_MS,
            failed_attempts: 3,
            unavailable: 1,
            unavailable_within_ms: HOUR_MS,
            spend_percent: 80,
            paused_ms: HOUR_MS,
            repeat_ms: 6 * HOUR_MS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    /// A Ticket waits for a person.
    Waiting,
    /// A working Assignment sends no Report.
    Stalled,
    /// An Effect keeps failing.
    Failing,
    /// The Router keeps failing to decide.
    Unavailable,
    /// An account nears its cap.
    Spend,
    /// A pause has lasted.
    Paused,
    /// An external job is down, stalled or low on disk.
    Job,
}

impl AlertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Stalled => "stalled",
            Self::Failing => "failing",
            Self::Unavailable => "unavailable",
            Self::Spend => "spend",
            Self::Paused => "paused",
            Self::Job => "job",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Alert {
    /// See [`alert_id`].
    pub id: String,
    pub kind: AlertKind,
    /// What the alert is about, with its start time when it has one, so a
    /// problem that comes back is a new alert.
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<TicketId>,
    /// When the problem started, when the Store tells.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<Ms>,
    /// One line for the operator.
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stalled: Option<Stalled>,
}

/// What the retry action needs to know it retries what was judged stalled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Stalled {
    pub assignment: AssignmentId,
    /// The Reports it had sent.
    pub reports: usize,
}

/// `wd_` and a hash of the kind, the subject and a bucket of `repeat_ms`.
/// A problem with a start time counts buckets from when it crossed its
/// threshold, so it is told then and once each `repeat_ms` after; one
/// without counts them on the clock.
pub fn alert_id(
    kind: AlertKind,
    subject: &str,
    crossed_ms: Option<Ms>,
    now: Ms,
    repeat_ms: Ms,
) -> String {
    let elapsed = crossed_ms.map_or(now, |at| now.saturating_sub(at));
    let bucket = elapsed / repeat_ms.max(1);
    key("wd", &[kind.as_str(), subject, &bucket.to_string()])
}

/// What one assessment reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// The open Tickets.
    pub tickets: Vec<Ticket>,
    /// Decisions within the `unavailable` window.
    pub decisions: Vec<Decision>,
    /// Effects not done yet.
    pub effects: Vec<QueuedEffect>,
    pub accounts: Vec<Spending>,
    pub pause: Option<Pause>,
    /// When the Watchdog last saw a pause lift. Workers cannot call a model
    /// while the agent is paused, so their quiet time counts from here.
    pub unpaused_ms: Option<Ms>,
    /// The external jobs, as the Watchdog last found them.
    pub jobs: Vec<Record>,
}

/// One ledger account and what counts against its cap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Spending {
    /// The ledger's key: `router` or `ticket:<id>`.
    pub account: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<TicketId>,
    /// What the account is capped at, if it is: a Ticket's from its Mandate,
    /// the Router's from the config, not the Store. Without a cap, no spend
    /// alert is raised for it.
    pub cap_microusd: Option<MicroUsd>,
    /// Spend from this time on counts.
    pub since_ms: Ms,
    /// When each call was held and what it counts, oldest first: an open
    /// hold at its hold, a closed one at its cost.
    pub charges: Vec<(Ms, MicroUsd)>,
}

impl Spending {
    pub fn committed(&self) -> MicroUsd {
        self.charges
            .iter()
            .fold(0, |sum, (_, charge)| sum.saturating_add(*charge))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Pause {
    /// What holds it.
    pub by: String,
    /// When the file that holds it last changed. A pause in agent.yaml or
    /// the peak window has no such time and raises no alert.
    pub since_ms: Option<Ms>,
}

/// What the Store does not hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Known {
    pub router_daily_microusd: Option<MicroUsd>,
    /// `policy.paused` in agent.yaml.
    pub configured_pause: bool,
}

impl Known {
    pub fn of(config: &AgentConfig) -> Self {
        Self {
            router_daily_microusd: config.budgets.router_daily_microusd,
            configured_pause: config.policy.paused,
        }
    }
}

impl Snapshot {
    pub fn read(
        store: &SqliteStore,
        state_dir: &Path,
        now: Ms,
        known: Known,
        limits: &Thresholds,
    ) -> Result<Self> {
        let mut tickets = Vec::new();
        for id in store.open_tickets()? {
            tickets.extend(store.read_ticket(&id)?);
        }
        let window = now.saturating_sub(limits.unavailable_within_ms);
        let router = Account::Router {
            daily_microusd: known.router_daily_microusd,
        };
        let (today, _) = router.allowance(now);
        let mut accounts = vec![Spending {
            account: router.key(),
            ticket: None,
            cap_microusd: known.router_daily_microusd,
            since_ms: today,
            charges: charges(store, &router.key(), today)?,
        }];
        for ticket in &tickets {
            let account = Account::Ticket {
                id: ticket.id.clone(),
                budget_microusd: ticket.mandate.budget_microusd,
            };
            accounts.push(Spending {
                charges: charges(store, &account.key(), 0)?,
                account: account.key(),
                ticket: Some(ticket.id.clone()),
                cap_microusd: ticket.mandate.budget_microusd,
                since_ms: 0,
            });
        }
        Ok(Self {
            tickets,
            decisions: store.recent_decisions(window, MAX_DECISIONS)?,
            effects: store.pending_effects()?,
            accounts,
            pause: read_pause(state_dir, known.configured_pause, now),
            unpaused_ms: None,
            jobs: external_jobs::State::load(state_dir).map_or_else(Vec::new, |state| state.jobs),
        })
    }
}

fn charges(store: &SqliteStore, account: &str, since_ms: Ms) -> Result<Vec<(Ms, MicroUsd)>> {
    Ok(store
        .spend_since(account, since_ms)?
        .into_iter()
        .map(|row| {
            let charge = match row.state {
                SpendState::Held => row.hold_microusd,
                SpendState::Settled | SpendState::Released | SpendState::Unknown => {
                    row.cost_microusd
                }
            };
            (row.at_ms, charge)
        })
        .collect())
}

/// What holds the agent paused, as the runtime decides it: the state
/// directory's pause gate or agent.yaml's `policy.paused`.
pub fn read_pause(state_dir: &Path, configured: bool, now: Ms) -> Option<Pause> {
    let gate = gateway::effective_control(state_dir, now / 1_000).is_some();
    if !gate && !configured {
        return None;
    }
    let control = state_dir.join("control.json");
    let mode = std::fs::read_to_string(&control)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value["mode"].as_str().map(str::to_owned));
    let pause_file = state_dir.join("PAUSE");
    let (by, file) = match mode.as_deref() {
        Some(mode @ ("pause" | "drain")) => (format!("control.json (mode {mode})"), Some(control)),
        _ if pause_file.exists() => ("the PAUSE file".to_owned(), Some(pause_file)),
        _ if configured => ("policy.paused in agent.yaml".to_owned(), None),
        _ => ("the peak window (SWARM_REFUSE_PEAK)".to_owned(), None),
    };
    Some(Pause {
        by,
        since_ms: file.as_deref().and_then(modified_ms),
    })
}

fn modified_ms(path: &Path) -> Option<Ms> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    u64::try_from(modified.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

/// The alerts `snapshot` raises at `now`.
pub fn assess(snapshot: &Snapshot, now: Ms, limits: &Thresholds) -> Vec<Alert> {
    let mut alerts = Vec::new();
    let paused = snapshot.pause.is_some();
    for ticket in &snapshot.tickets {
        alerts.extend(waiting(ticket, now, limits));
        // A pause stops every worker; the pause alert covers that.
        if !paused {
            alerts.extend(stalled(ticket, snapshot.unpaused_ms, now, limits));
        }
    }
    alerts.extend(
        snapshot
            .effects
            .iter()
            .filter_map(|queued| failing(queued, now, limits)),
    );
    alerts.extend(unavailable(&snapshot.decisions, now, limits));
    alerts.extend(
        snapshot
            .accounts
            .iter()
            .filter_map(|account| spending(account, now, limits)),
    );
    alerts.extend(
        snapshot
            .pause
            .as_ref()
            .and_then(|pause| paused_long(pause, now, limits)),
    );
    alerts.extend(snapshot.jobs.iter().filter_map(job));
    alerts
}

/// What names one incident of a job: its status and when that began, so a job
/// that comes back and fails again is a new alert.
fn job_subject(record: &Record) -> String {
    format!(
        "job:{}:{}:{}",
        record.name,
        record.status.as_str(),
        record.since_ms
    )
}

/// The id of the alert for the incident `record` is in. The bucket is fixed,
/// so a job is told once however long it stays down: `repeat` does not apply.
fn job_alert_id(record: &Record) -> String {
    alert_id(
        AlertKind::Job,
        &job_subject(record),
        Some(record.since_ms),
        0,
        Ms::MAX,
    )
}

fn job(record: &Record) -> Option<Alert> {
    let what = match record.status {
        Status::Up => return None,
        Status::Down => "is down",
        Status::Stalled => "has stalled",
        Status::LowDisk => "is low on disk",
    };
    Some(Alert {
        id: job_alert_id(record),
        kind: AlertKind::Job,
        subject: job_subject(record),
        ticket: None,
        since_ms: Some(record.since_ms),
        text: format!(
            "The job \"{}\" ({}) {what}: {}",
            record.name,
            record.check,
            ending(&record.detail)
        ),
        stalled: None,
    })
}

fn waiting(ticket: &Ticket, now: Ms, limits: &Thresholds) -> Option<Alert> {
    let stage = ticket.stage();
    if !matches!(
        stage,
        Stage::Approval | Stage::Answer | Stage::Blocked | Stage::Failed
    ) {
        return None;
    }
    let since = waiting_since(ticket, stage);
    let waited = now
        .checked_sub(since)
        .filter(|waited| *waited >= limits.waiting_ms)?;
    let waited_for = duration(waited);
    let what = match stage {
        Stage::Approval => format!("has been waiting {waited_for} for a budget approval"),
        Stage::Answer => format!("has been waiting {waited_for} for an answer"),
        Stage::Blocked => format!("has been blocked for {waited_for}"),
        _ => format!("failed {waited_for} ago and is waiting on a person"),
    };
    let detail = ticket
        .attention
        .clone()
        .or_else(|| ticket.latest_report().map(|r| r.text.clone()))
        .unwrap_or_default();
    let said = match (stage, detail.trim().is_empty()) {
        (_, true) => String::new(),
        (Stage::Answer, false) => format!(" It asked: {}", ending(&clip(&detail))),
        (Stage::Approval, false) => format!(" {}", ending(&clip(&detail))),
        (_, false) => format!(" It said: {}", ending(&clip(&detail))),
    };
    let nobody = if ticket.channels.is_empty() {
        " Its request named no reply channel, so nobody else was asked."
    } else {
        ""
    };
    let subject = format!("ticket:{}:{}:{since}", ticket.id, stage_name(stage));
    Some(Alert {
        id: alert_id(
            AlertKind::Waiting,
            &subject,
            Some(since.saturating_add(limits.waiting_ms)),
            now,
            limits.repeat_ms,
        ),
        kind: AlertKind::Waiting,
        subject,
        ticket: Some(ticket.id.clone()),
        since_ms: Some(since),
        text: format!("\"{}\" ({}) {what}.{said}{nobody}", ticket.title, ticket.id),
        stalled: None,
    })
}

/// When the Ticket began to wait. A Report that asks, blocks or fails
/// starts a wait. The Store keeps no time for an approval asked at a later
/// launch, so an approval counts from the Ticket's latest step: its
/// creation, its latest Assignment or Report, or a person's latest Input.
fn waiting_since(ticket: &Ticket, stage: Stage) -> Ms {
    let reports = || ticket.assignments.iter().flat_map(|a| &a.reports);
    let asked = match stage {
        Stage::Answer => reports()
            .filter(|r| r.question.is_some())
            .map(|r| r.at_ms)
            .max(),
        Stage::Blocked | Stage::Failed => ticket.latest_report().map(|r| r.at_ms),
        _ => None,
    };
    asked.unwrap_or_else(|| {
        let steps = [
            ticket.assignments.last().map(|a| a.created_ms),
            reports().map(|r| r.at_ms).max(),
            ticket
                .inputs
                .iter()
                .filter(|i| i.from_allowed_person)
                .map(|i| i.at_ms)
                .max(),
        ];
        steps.into_iter().flatten().fold(ticket.created_ms, Ms::max)
    })
}

fn stalled(ticket: &Ticket, unpaused: Option<Ms>, now: Ms, limits: &Thresholds) -> Option<Alert> {
    if ticket.stage() != Stage::Working {
        return None;
    }
    let assignment = ticket.running_assignment()?;
    let since = assignment
        .reports
        .last()
        .map_or(assignment.created_ms, |r| r.at_ms)
        .max(unpaused.unwrap_or_default());
    let quiet = now
        .checked_sub(since)
        .filter(|quiet| *quiet >= limits.stalled_ms)?;
    let subject = format!("assignment:{}:{since}", assignment.id);
    Some(Alert {
        id: alert_id(
            AlertKind::Stalled,
            &subject,
            Some(since.saturating_add(limits.stalled_ms)),
            now,
            limits.repeat_ms,
        ),
        kind: AlertKind::Stalled,
        subject,
        ticket: Some(ticket.id.clone()),
        since_ms: Some(since),
        text: format!(
            "\"{}\" ({}) has gone quiet: its worker ({}) hasn't reported in {}.",
            ticket.title,
            ticket.id,
            assignment.id,
            duration(quiet)
        ),
        stalled: Some(Stalled {
            assignment: assignment.id.clone(),
            reports: assignment.reports.len(),
        }),
    })
}

/// Whether `assignment` is still the Ticket's working Assignment with the
/// Reports it had when the Watchdog judged it stalled.
pub fn still_stalled(ticket: &Ticket, assignment: &AssignmentId, reports: usize) -> bool {
    ticket.stage() == Stage::Working
        && ticket
            .running_assignment()
            .is_some_and(|a| a.id == *assignment && a.reports.len() == reports)
}

fn failing(queued: &QueuedEffect, now: Ms, limits: &Thresholds) -> Option<Alert> {
    if queued.done || queued.attempts < limits.failed_attempts {
        return None;
    }
    let subject = format!("effect:{}", queued.id);
    let trouble = match &queued.effect {
        Effect::Send(_) => "I couldn't deliver",
        _ => "I'm stuck on",
    };
    Some(Alert {
        id: alert_id(AlertKind::Failing, &subject, None, now, limits.repeat_ms),
        kind: AlertKind::Failing,
        subject,
        ticket: effect_ticket(&queued.effect).cloned(),
        since_ms: None,
        text: format!(
            "{trouble} {} (effect {}): it has failed {}. The last error: {}.",
            describe(&queued.effect),
            queued.id,
            times(u64::from(queued.attempts)),
            clip(queued.last_error.as_deref().unwrap_or("no error recorded"))
        ),
        stalled: None,
    })
}

fn unavailable(decisions: &[Decision], now: Ms, limits: &Thresholds) -> Option<Alert> {
    let recent: Vec<&Decision> = decisions
        .iter()
        .filter(|d| d.kind == DecisionKind::Unavailable)
        .filter(|d| now.saturating_sub(d.at_ms) <= limits.unavailable_within_ms)
        .collect();
    let needed = usize::try_from(limits.unavailable).unwrap_or(usize::MAX);
    if recent.is_empty() || recent.len() < needed {
        return None;
    }
    let latest = recent.iter().max_by_key(|d| d.at_ms)?;
    let threads: BTreeSet<&str> = recent.iter().map(|d| d.thread.as_str()).collect();
    let subject = "router".to_owned();
    Some(Alert {
        id: alert_id(
            AlertKind::Unavailable,
            &subject,
            None,
            now,
            limits.repeat_ms,
        ),
        kind: AlertKind::Unavailable,
        subject,
        ticket: None,
        since_ms: None,
        text: format!(
            "I couldn't decide what to do with new messages {} in the last {} (threads {}). \
             The latest reason: {}.",
            times(recent.len() as u64),
            duration(limits.unavailable_within_ms),
            threads.into_iter().collect::<Vec<_>>().join(", "),
            clip(&latest.reason)
        ),
        stalled: None,
    })
}

fn spending(account: &Spending, now: Ms, limits: &Thresholds) -> Option<Alert> {
    let cap = account.cap_microusd.filter(|cap| *cap > 0)?;
    let share = u128::from(cap) * u128::from(limits.spend_percent) / 100;
    let limit = MicroUsd::try_from(share).unwrap_or(MicroUsd::MAX).max(1);
    let mut committed: MicroUsd = 0;
    let mut crossed = None;
    for (at, charge) in &account.charges {
        committed = committed.saturating_add(*charge);
        if crossed.is_none() && committed >= limit {
            crossed = Some(*at);
        }
    }
    let crossed = crossed?;
    let whose = match &account.ticket {
        Some(ticket) => format!("{ticket} has committed"),
        None => "The Router has committed".to_owned(),
    };
    let of = if account.ticket.is_some() {
        "its budget"
    } else {
        "its daily cap"
    };
    let subject = format!("spend:{}:{}", account.account, account.since_ms);
    Some(Alert {
        id: alert_id(
            AlertKind::Spend,
            &subject,
            Some(crossed),
            now,
            limits.repeat_ms,
        ),
        kind: AlertKind::Spend,
        subject,
        ticket: account.ticket.clone(),
        since_ms: Some(crossed),
        text: format!(
            "{whose} {} of {of} of {}, {}% or more.",
            usd(committed),
            usd(cap),
            limits.spend_percent
        ),
        stalled: None,
    })
}

fn paused_long(pause: &Pause, now: Ms, limits: &Thresholds) -> Option<Alert> {
    let since = pause.since_ms?;
    let held = now
        .checked_sub(since)
        .filter(|held| *held >= limits.paused_ms)?;
    let subject = format!("pause:{since}");
    Some(Alert {
        id: alert_id(
            AlertKind::Paused,
            &subject,
            Some(since.saturating_add(limits.paused_ms)),
            now,
            limits.repeat_ms,
        ),
        kind: AlertKind::Paused,
        subject,
        ticket: None,
        since_ms: Some(since),
        text: format!(
            "I've been paused by {} for {}, so I'm not picking up new messages or starting any \
             work until that is lifted.",
            pause.by,
            duration(held)
        ),
        stalled: None,
    })
}

pub fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::Approval => "approval",
        Stage::Answer => "answer",
        Stage::Blocked => "blocked",
        Stage::Failed => "failed",
        Stage::Delivering => "delivering",
        Stage::Queued => "queued",
        Stage::Idle => "idle",
        Stage::Working => "working",
        Stage::Ended => "ended",
    }
}

pub fn plan_label(plan: &Plan) -> String {
    match plan {
        Plan::Session { .. } => "one worker session".into(),
        Plan::Swarm { agents, .. } => format!("a swarm of {agents} agents"),
    }
}

fn effect_ticket(effect: &Effect) -> Option<&TicketId> {
    match effect {
        Effect::Launch { ticket, .. }
        | Effect::Deliver { ticket, .. }
        | Effect::Stop { ticket, .. } => Some(ticket),
        Effect::Send(message) => message.ticket.as_ref(),
        Effect::Forge(job) => Some(job.ticket()),
        Effect::Assess(_) => None,
    }
}

/// An Effect in a few words.
pub fn describe(effect: &Effect) -> String {
    match effect {
        Effect::Assess(candidate) => {
            format!("judging {} on {}", candidate.eval, candidate.thread)
        }
        Effect::Launch {
            ticket, assignment, ..
        } => format!("the launch of {assignment} for {ticket}"),
        Effect::Deliver {
            assignment, inputs, ..
        } => format!("{} input(s) for {assignment}", inputs.len()),
        Effect::Stop { assignment, .. } => format!("the stop of {assignment}"),
        Effect::Forge(ForgeJob::Publish { ticket, head }) => {
            format!("the publication of {} for {ticket}", head.short())
        }
        Effect::Send(message) => {
            let kind = match message.kind {
                MessageKind::Acknowledgement => "an acknowledgement",
                MessageKind::Result => "a result",
                MessageKind::Attention => "a question for a person",
            };
            let address: Vec<&str> = message
                .channel
                .address
                .values()
                .map(String::as_str)
                .collect();
            format!("{kind} to {} {}", message.channel.source, address.join("/"))
        }
    }
}

/// Money as dollars, to the microdollar but with no trailing zeros past
/// the cents: `$0.50`, `$0.00009`.
pub fn usd(microusd: MicroUsd) -> String {
    let mut fraction = format!("{:06}", microusd % 1_000_000);
    while fraction.len() > 2 && fraction.ends_with('0') {
        fraction.pop();
    }
    format!("${}.{fraction}", microusd / 1_000_000)
}

/// `45s`, `12m`, `2h 05m`, `3d 04h`.
pub fn duration(ms: Ms) -> String {
    let seconds = ms / 1_000;
    let (minutes, hours, days) = (seconds / 60, seconds / 3_600, seconds / 86_400);
    if days > 0 {
        format!("{days}d {:02}h", hours % 24)
    } else if hours > 0 {
        format!("{hours}h {:02}m", minutes % 60)
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

/// `text` as a sentence: it ends in a full stop unless it already ends in a
/// question or exclamation mark.
fn ending(text: &str) -> String {
    match text.chars().last() {
        Some('?' | '!' | '.') => text.to_owned(),
        _ => format!("{text}."),
    }
}

/// "1 time", "3 times".
fn times(n: u64) -> String {
    match n {
        1 => "1 time".to_owned(),
        n => format!("{n} times"),
    }
}

/// One line, cut to [`DETAIL_CHARS`].
pub fn clip(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = line.trim_end_matches('.');
    if line.chars().count() <= DETAIL_CHARS {
        return line.to_owned();
    }
    let mut cut: String = line.chars().take(DETAIL_CHARS).collect();
    cut.push_str("...");
    cut
}

/// The Watchdog's loop. It has its own Store connection and writes only its
/// alerts table; Engine commands go through the runtime.
pub struct Watchdog {
    store: SqliteStore,
    state_dir: PathBuf,
    operator: ChannelRef,
    channel: Arc<dyn Channel>,
    /// The runtime's fact sink, for the retry action; without it there is
    /// no action.
    retry: Option<FactSink>,
    limits: Thresholds,
    known: Known,
    every: Duration,
    /// Alerts this loop recorded and has not tried to send.
    fresh: HashSet<String>,
    paused: bool,
    unpaused_ms: Option<Ms>,
    /// Where a Ticket's page is, for the dream's digest.
    links: TicketLinks,
    /// The external jobs to look at, and how.
    jobs: Vec<JobConfig>,
    checker: Checker,
}

/// How far back a dream that ended may still be told.
const DREAM_TOLD_WITHIN_MS: Ms = 7 * 24 * HOUR_MS;

impl Watchdog {
    /// A Watchdog over the Store in `state_dir` that tells `channel`, which
    /// serves `settings.operator`.
    pub fn open(
        state_dir: &Path,
        settings: &WatchdogConfig,
        known: Known,
        channel: Arc<dyn Channel>,
        retry: Option<FactSink>,
    ) -> Result<Self> {
        let store = SqliteStore::open(state_dir.join(STORE_FILE))
            .context("cannot open the store for the watchdog")?;
        Ok(Self {
            store,
            state_dir: state_dir.to_owned(),
            operator: settings.operator(),
            channel,
            retry,
            limits: settings.thresholds(),
            known,
            every: settings.every(),
            fresh: HashSet::new(),
            paused: false,
            unpaused_ms: None,
            links: TicketLinks::none(),
            jobs: settings.jobs.clone(),
            checker: Checker::default(),
        })
    }

    /// Look at the external jobs with `checker` instead of the host's
    /// `systemctl` and `tmux`.
    #[must_use]
    pub fn with_checker(mut self, checker: Checker) -> Self {
        self.checker = checker;
        self
    }

    /// Link the dream's digest to Tickets' pages on the dashboard.
    #[must_use]
    pub fn with_links(mut self, links: TicketLinks) -> Self {
        self.links = links;
        self
    }

    /// Record the digest of each dream that ended and has not been told,
    /// once its proposals are decided (see `dream::digest`): one message a
    /// dream, so one a day, as an alert whose id names the dream, so a
    /// restart neither repeats it nor loses it.
    fn dream_digests(&mut self, now: Ms) -> Result<()> {
        for dream in self.store.dream_tickets()? {
            let Some(ended) = &dream.ended else {
                continue;
            };
            if now.saturating_sub(ended.at_ms) > DREAM_TOLD_WITHIN_MS {
                continue;
            }
            let id = format!("wd_dream_{}", dream.id);
            if self.store.has_alert(&id)? {
                continue;
            }
            let Some(text) = dream::digest(&self.store, &dream, &self.links, now)? else {
                continue;
            };
            let row = AlertRow {
                id: id.clone(),
                kind: "dream".into(),
                subject: format!("dream:{}", dream.id),
                ticket: Some(dream.id.clone()),
                retry: false,
                raised_ms: now,
                message: OutboundMessage {
                    id: MessageId(id),
                    channel: self.operator.clone(),
                    kind: MessageKind::Result,
                    text,
                    ticket: Some(dream.id.clone()),
                    ask: None,
                },
                sent_ms: None,
            };
            if self.store.record_alert(&row)? {
                eprintln!("agent: watchdog: the digest of {} is ready", dream.id);
                self.fresh.insert(row.id);
            }
        }
        Ok(())
    }

    /// Tell the operator that each job in `recovered` is back, once, if the
    /// operator was told it was in trouble.
    fn job_recoveries(&mut self, recovered: Vec<Recovered>, now: Ms) -> Result<()> {
        for recovered in recovered {
            if !self.store.has_alert(&job_alert_id(&recovered.was))? {
                continue;
            }
            let subject = format!("{}:recovered", job_subject(&recovered.was));
            let id = key("wd", &["job_up", &subject]);
            let row = AlertRow {
                id: id.clone(),
                kind: "job_up".into(),
                subject,
                ticket: None,
                retry: false,
                raised_ms: now,
                message: OutboundMessage {
                    id: MessageId(id),
                    channel: self.operator.clone(),
                    kind: MessageKind::Result,
                    text: recovered.text(now),
                    ticket: None,
                    ask: None,
                },
                sent_ms: None,
            };
            if self.store.record_alert(&row)? {
                eprintln!("agent: watchdog: {}", row.message.text);
                self.fresh.insert(row.id);
            }
        }
        Ok(())
    }

    /// Assess every `every`, the first time one interval after the start,
    /// once the runtime has recovered what the last run left. A failed tick
    /// is logged and the next one tries again.
    pub async fn run(mut self, mut stop: oneshot::Receiver<()>) {
        let first = tokio::time::Instant::now() + self.every;
        let mut tick = tokio::time::interval_at(first, self.every);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = &mut stop => return,
                _ = tick.tick() => {}
            }
            if let Err(err) = self.tick(now_ms()).await {
                eprintln!("agent: watchdog: {err:#}");
            }
        }
    }

    /// Assess at `now`, record each new alert, send every alert not sent
    /// yet, then retry what the action covers. Returns how many alerts the
    /// operator's channel took.
    pub async fn tick(&mut self, now: Ms) -> Result<usize> {
        match self.checker.look(&self.jobs, &self.state_dir, now).await {
            Ok(recovered) => self.job_recoveries(recovered, now)?,
            Err(err) => eprintln!("agent: watchdog: the external jobs were not checked: {err:#}"),
        }
        let mut snapshot =
            Snapshot::read(&self.store, &self.state_dir, now, self.known, &self.limits)?;
        let paused = snapshot.pause.is_some();
        if self.paused && !paused {
            self.unpaused_ms = Some(now);
        }
        self.paused = paused;
        snapshot.unpaused_ms = self.unpaused_ms;
        let mut retries = Vec::new();
        for alert in assess(&snapshot, now, &self.limits) {
            let retry = match (&self.retry, &alert.stalled, &alert.ticket) {
                (Some(_), Some(stalled), Some(ticket))
                    if !self.store.watchdog_retried(ticket)? =>
                {
                    Some((ticket.clone(), stalled.clone()))
                }
                _ => None,
            };
            let mut text = alert.text;
            if retry.is_some() {
                text.push_str(" I'll retry it once.");
            }
            let row = AlertRow {
                id: alert.id.clone(),
                kind: alert.kind.as_str().to_owned(),
                subject: alert.subject,
                ticket: alert.ticket.clone(),
                retry: retry.is_some(),
                raised_ms: now,
                message: OutboundMessage {
                    id: MessageId(alert.id),
                    channel: self.operator.clone(),
                    kind: MessageKind::Attention,
                    text,
                    ticket: alert.ticket,
                    ask: None,
                },
                sent_ms: None,
            };
            if self.store.record_alert(&row)? {
                eprintln!("agent: watchdog: {}", row.message.text);
                self.fresh.insert(row.id);
                retries.extend(retry);
            }
        }
        self.dream_digests(now)?;
        // Told before acting, so the operator hears of a retry even when the
        // retried work ends the run at once.
        let sent = self.flush(now).await?;
        if let Some(facts) = &self.retry {
            for (ticket, stalled) in retries {
                retry(facts, ticket, stalled).await?;
            }
        }
        Ok(sent)
    }

    /// Send unsent alerts oldest first, stopping at the first failure so the
    /// next tick keeps their order.
    async fn flush(&mut self, now: Ms) -> Result<usize> {
        let mut sent = 0;
        for row in self.store.unsent_alerts()? {
            // One recorded by an earlier run, or tried before, may have gone out.
            let repeat = !self.fresh.remove(&row.id);
            let options = SendOptions { repeat, ask: None };
            match self.channel.send(row.message, options).await {
                Ok(outcome) => {
                    if outcome == SendOutcome::Gone {
                        eprintln!(
                            "agent: watchdog: the operator's conversation is gone; alert {} was dropped",
                            row.id
                        );
                    } else {
                        sent += 1;
                    }
                    self.store.mark_alert_sent(&row.id, now)?;
                }
                Err(err) => {
                    eprintln!(
                        "agent: watchdog: alert {} was not sent; the next tick tries again: {err:#}",
                        row.id
                    );
                    break;
                }
            }
        }
        Ok(sent)
    }
}

/// Ask the runtime to retry a stalled Assignment. It checks first that the
/// Assignment has not moved on since.
async fn retry(facts: &FactSink, ticket: TicketId, stalled: Stalled) -> Result<()> {
    let assignment = stalled.assignment.clone();
    let fact = Fact::Retry {
        ticket: ticket.clone(),
        assignment: stalled.assignment,
        reports: stalled.reports,
    };
    match facts.ask(fact).await? {
        Ok(_) => eprintln!("agent: watchdog: retried {assignment} of {ticket}"),
        Err(why) => {
            eprintln!("agent: watchdog: the retry of {assignment} of {ticket} was refused: {why}");
        }
    }
    Ok(())
}

/// A Watchdog running beside the runtime.
pub struct Running {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Running {
    /// The Watchdog `config` asks for, if any, on the channel that serves
    /// its operator.
    pub(crate) fn start(
        config: &AgentConfig,
        state_dir: &Path,
        channels: &BTreeMap<String, Arc<dyn Channel>>,
        facts: &FactSink,
    ) -> Result<Option<Self>> {
        let Some(settings) = &config.watchdog else {
            return Ok(None);
        };
        let channel = channels
            .get(&settings.operator.source)
            .cloned()
            .with_context(|| {
                format!(
                    "no channel serves the watchdog's operator source {}",
                    settings.operator.source
                )
            })?;
        let retry = settings.retry_stalled.then(|| facts.clone());
        let links = match &config.web {
            Some(web) => TicketLinks::new(&web.public_url)?,
            None => TicketLinks::none(),
        };
        let watchdog = Watchdog::open(state_dir, settings, Known::of(config), channel, retry)?
            .with_links(links);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(watchdog.run(stopped));
        Ok(Some(Self { stop, task }))
    }

    /// Stop once the tick in progress, if any, is over.
    pub async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Alert, AlertKind, Pause, Snapshot, Spending, Thresholds, alert_id, assess, duration,
        still_stalled, usd,
    };
    use crate::agent::external_jobs::{Record, Status};
    use solos_agent_core::model::{
        Actor, Approval, Assignment, AssignmentId, AssignmentState, ChannelRef, Decision,
        DecisionId, DecisionKind, Effect, Input, InputBody, InputId, Mandate, MessageId,
        MessageKind, Ms, OutboundMessage, Plan, Report, ReportStatus, Stage, Strength, ThreadKey,
        Ticket, TicketId, TriggerId,
    };
    use solos_agent_store::{EffectId, QueuedEffect};
    use std::collections::{BTreeMap, BTreeSet};

    const T0: Ms = 1_793_600_000_000;
    const MIN: Ms = 60_000;
    const HOUR: Ms = 60 * MIN;

    fn limits() -> Thresholds {
        Thresholds::default()
    }

    fn ticket(id: &str, created_ms: Ms) -> Ticket {
        Ticket {
            id: TicketId::new(id),
            title: format!("Work {id}"),
            origin: TriggerId::new(format!("t-{id}")),
            decision: DecisionId::new(format!("dc-{id}")),
            mandate: Mandate {
                policy_version: "v1".into(),
                scope: "the scope".into(),
                strength: Strength::Explicit,
                rules: Vec::new(),
                requester: Actor::person("alice"),
                source: "slack".into(),
                budget_microusd: Some(1_000_000),
                repo: None,
                forge: None,
            },
            plan: Plan::Session {
                harness: None,
                environment: None,
                method: None,
            },
            channels: vec![ChannelRef {
                source: "slack".into(),
                address: BTreeMap::from([("channel".to_owned(), "C1".to_owned())]),
            }],
            threads: BTreeSet::from([ThreadKey::new("T1")]),
            inputs: Vec::new(),
            assignments: Vec::new(),
            approval: Approval::NotNeeded,
            secrets: BTreeMap::new(),
            attention: None,
            ended: None,
            created_ms,
            seq: 0,
            handed_back: BTreeSet::new(),
            change: None,
        }
    }

    fn running(mut ticket: Ticket, created_ms: Ms) -> Ticket {
        let n = ticket.assignments.len() + 1;
        ticket.assignments.push(Assignment {
            id: AssignmentId::new(format!("as-{}-{n}", ticket.id)),
            state: AssignmentState::Running,
            created_ms,
            reports: Vec::new(),
            ended_reason: None,
        });
        ticket
    }

    fn reported(
        mut ticket: Ticket,
        status: ReportStatus,
        at_ms: Ms,
        question: Option<&str>,
    ) -> Ticket {
        ticket.seq += 1;
        let seq = ticket.seq;
        if let Some(question) = question {
            ticket.attention = Some(question.to_owned());
        }
        let latest = ticket.assignments.last_mut().unwrap();
        latest.reports.push(Report {
            seq,
            status,
            text: format!("{status:?} text"),
            question: question.map(str::to_owned),
            spent_microusd: 0,
            at_ms,
        });
        ticket
    }

    fn of(tickets: Vec<Ticket>) -> Snapshot {
        Snapshot {
            tickets,
            ..Snapshot::default()
        }
    }

    fn kinds(alerts: &[Alert]) -> Vec<AlertKind> {
        alerts.iter().map(|a| a.kind).collect()
    }

    /// The alert fires at `fires` and not a millisecond before.
    fn fires_at(snapshot: &Snapshot, fires: Ms, kind: AlertKind) -> Alert {
        assert!(
            !kinds(&assess(snapshot, fires - 1, &limits())).contains(&kind),
            "{kind:?} before its threshold"
        );
        let alerts = assess(snapshot, fires, &limits());
        let found: Vec<_> = alerts.into_iter().filter(|a| a.kind == kind).collect();
        let [alert] = &found[..] else {
            panic!("{kind:?} at its threshold: {found:?}")
        };
        alert.clone()
    }

    #[test]
    fn a_ticket_waiting_for_a_person_alerts_after_the_threshold_in_each_stage() {
        let limits = limits();
        let base = running(ticket("tk1", T0), T0);
        let cases = [
            (
                "answer",
                reported(
                    base.clone(),
                    ReportStatus::Waiting,
                    T0 + MIN,
                    Some("Which branch?"),
                ),
                T0 + MIN,
            ),
            (
                "blocked",
                reported(base.clone(), ReportStatus::Blocked, T0 + 2 * MIN, None),
                T0 + 2 * MIN,
            ),
            (
                "failed",
                reported(base.clone(), ReportStatus::Failed, T0 + 3 * MIN, None),
                T0 + 3 * MIN,
            ),
        ];
        for (stage, ticket, since) in cases {
            assert_eq!(super::stage_name(ticket.stage()), stage);
            let alert = fires_at(
                &of(vec![ticket]),
                since + limits.waiting_ms,
                AlertKind::Waiting,
            );
            assert_eq!(alert.since_ms, Some(since), "{stage}");
            assert!(alert.subject.contains(stage), "{}", alert.subject);
        }
        let mut approval = ticket("tk2", T0);
        approval.approval = Approval::Waiting;
        approval.attention = Some("Work tk2 needs approval first".into());
        assert_eq!(approval.stage(), Stage::Approval);
        let alert = fires_at(
            &of(vec![approval]),
            T0 + limits.waiting_ms,
            AlertKind::Waiting,
        );
        assert!(alert.text.contains("budget approval"), "{}", alert.text);
        assert!(
            alert.text.contains("needs approval first"),
            "{}",
            alert.text
        );
    }

    #[test]
    fn alerts_are_written_as_the_agent_telling_the_operator_not_as_log_lines() {
        let limits = limits();
        let base = running(ticket("tk1", T0), T0);

        let asked = reported(
            base.clone(),
            ReportStatus::Waiting,
            T0,
            Some("Which branch?"),
        );
        let alert = fires_at(&of(vec![asked]), T0 + limits.waiting_ms, AlertKind::Waiting);
        assert_eq!(
            alert.text,
            "\"Work tk1\" (tk1) has been waiting 1h 00m for an answer. It asked: Which branch?",
            "a question keeps its own mark and gets no second full stop"
        );

        let failed = reported(base.clone(), ReportStatus::Failed, T0, None);
        let alert = fires_at(
            &of(vec![failed]),
            T0 + limits.waiting_ms,
            AlertKind::Waiting,
        );
        assert_eq!(
            alert.text,
            "\"Work tk1\" (tk1) failed 1h 00m ago and is waiting on a person. \
             It said: Failed text."
        );

        let quiet = fires_at(&of(vec![base]), T0 + limits.stalled_ms, AlertKind::Stalled);
        assert_eq!(
            quiet.text,
            "\"Work tk1\" (tk1) has gone quiet: its worker (as-tk1-1) hasn't reported in 30m."
        );

        let paused = Snapshot {
            pause: Some(Pause {
                by: "control.json (mode pause)".into(),
                since_ms: Some(T0),
            }),
            ..Snapshot::default()
        };
        let alert = fires_at(&paused, T0 + limits.paused_ms, AlertKind::Paused);
        assert_eq!(
            alert.text,
            "I've been paused by control.json (mode pause) for 1h 00m, so I'm not picking up \
             new messages or starting any work until that is lifted."
        );
    }

    #[test]
    fn an_answer_from_a_person_ends_the_wait_and_bot_input_does_not_restart_it() {
        let asked = reported(
            running(ticket("tk1", T0), T0),
            ReportStatus::Blocked,
            T0 + MIN,
            None,
        );
        let mut later = asked.clone();
        later.inputs.push(Input {
            id: InputId::new("in-1"),
            seq: 9,
            body: InputBody::Note { text: "ci".into() },
            from_allowed_person: false,
            at_ms: T0 + 30 * MIN,
            sent_to: None,
        });
        assert_eq!(later.stage(), Stage::Blocked);
        fires_at(
            &of(vec![later.clone()]),
            T0 + MIN + HOUR,
            AlertKind::Waiting,
        );
        later.inputs[0].from_allowed_person = true;
        assert_eq!(later.stage(), Stage::Working, "a person answered");
        let after = assess(&of(vec![later]), T0 + 10 * HOUR, &limits());
        assert!(!kinds(&after).contains(&AlertKind::Waiting), "{after:?}");
    }

    #[test]
    fn a_schedule_ticket_with_no_reply_channel_says_nobody_was_asked() {
        let mut quiet = reported(
            running(ticket("tk1", T0), T0),
            ReportStatus::Failed,
            T0,
            None,
        );
        quiet.channels.clear();
        let alert = fires_at(&of(vec![quiet]), T0 + HOUR, AlertKind::Waiting);
        assert!(
            alert.text.contains("nobody else was asked"),
            "{}",
            alert.text
        );
    }

    #[test]
    fn a_working_assignment_with_no_report_stalls_after_the_threshold() {
        let limits = limits();
        let fresh = running(ticket("tk1", T0), T0 + MIN);
        let alert = fires_at(
            &of(vec![fresh.clone()]),
            T0 + MIN + limits.stalled_ms,
            AlertKind::Stalled,
        );
        let stalled = alert.stalled.clone().unwrap();
        assert_eq!((stalled.reports, alert.since_ms), (0, Some(T0 + MIN)));
        assert!(still_stalled(&fresh, &stalled.assignment, 0));

        let progressed = reported(fresh.clone(), ReportStatus::Progress, T0 + 20 * MIN, None);
        let again = fires_at(
            &of(vec![progressed.clone()]),
            T0 + 20 * MIN + limits.stalled_ms,
            AlertKind::Stalled,
        );
        assert_ne!(
            again.id, alert.id,
            "a progress report starts a new quiet spell"
        );
        assert!(
            !still_stalled(&progressed, &stalled.assignment, 0),
            "it reported since"
        );

        let idle = reported(fresh, ReportStatus::Waiting, T0 + 2 * MIN, None);
        assert_eq!(idle.stage(), Stage::Idle);
        assert!(
            assess(&of(vec![idle]), T0 + 10 * HOUR, &limits).is_empty(),
            "a turn that ended waits for input"
        );
    }

    #[test]
    fn a_pause_holds_stall_alerts_and_its_end_restarts_the_quiet_time() {
        let limits = limits();
        let working = running(ticket("tk1", T0), T0);
        let mut snapshot = of(vec![working]);
        snapshot.pause = Some(Pause {
            by: "the PAUSE file".into(),
            since_ms: Some(T0 + MIN),
        });
        let during = assess(&snapshot, T0 + 2 * HOUR, &limits);
        assert_eq!(kinds(&during), [AlertKind::Paused]);
        snapshot.pause = None;
        snapshot.unpaused_ms = Some(T0 + 2 * HOUR);
        fires_at(
            &snapshot,
            T0 + 2 * HOUR + limits.stalled_ms,
            AlertKind::Stalled,
        );
    }

    #[test]
    fn a_pause_held_by_a_file_alerts_after_the_threshold_and_a_configured_one_never() {
        let limits = limits();
        let mut snapshot = Snapshot {
            pause: Some(Pause {
                by: "control.json (mode pause)".into(),
                since_ms: Some(T0),
            }),
            ..Snapshot::default()
        };
        let alert = fires_at(&snapshot, T0 + limits.paused_ms, AlertKind::Paused);
        assert!(alert.text.contains("control.json"), "{}", alert.text);
        snapshot.pause = Some(Pause {
            by: "policy.paused in agent.yaml".into(),
            since_ms: None,
        });
        assert!(assess(&snapshot, T0 + 100 * HOUR, &limits).is_empty());
    }

    fn effect(id: i64, attempts: u32) -> QueuedEffect {
        QueuedEffect {
            id: EffectId(id),
            effect: Effect::Send(OutboundMessage {
                id: MessageId::new("ms-1"),
                channel: ChannelRef {
                    source: "slack".into(),
                    address: BTreeMap::from([
                        ("channel".to_owned(), "C1".to_owned()),
                        ("thread_ts".to_owned(), "T1".to_owned()),
                    ]),
                },
                kind: MessageKind::Result,
                text: "done".into(),
                ticket: Some(TicketId::new("tk1")),
                ask: None,
            }),
            attempts,
            due_ms: T0,
            last_error: Some("slack chat.postMessage: channel_not_found".into()),
            done: false,
        }
    }

    #[test]
    fn an_effect_alerts_at_its_third_failed_attempt() {
        let limits = limits();
        let at = |attempts| Snapshot {
            effects: vec![effect(7, attempts)],
            ..Snapshot::default()
        };
        assert!(assess(&at(limits.failed_attempts - 1), T0, &limits).is_empty());
        let alerts = assess(&at(limits.failed_attempts), T0, &limits);
        assert_eq!(kinds(&alerts), [AlertKind::Failing]);
        assert_eq!(alerts[0].ticket, Some(TicketId::new("tk1")));
        assert!(
            alerts[0].text.contains("a result to slack C1/T1"),
            "{}",
            alerts[0].text
        );
        assert!(
            alerts[0].text.contains("channel_not_found"),
            "{}",
            alerts[0].text
        );
        assert!(
            alerts[0].text.starts_with(
                "I couldn't deliver a result to slack C1/T1 (effect 7): it has failed 3 times."
            ),
            "{}",
            alerts[0].text
        );
        let later = assess(&at(limits.failed_attempts + 4), T0 + MIN, &limits);
        assert_eq!(
            later[0].id, alerts[0].id,
            "more attempts in the bucket tell nothing new"
        );
    }

    fn unavailable(n: usize, at_ms: Ms) -> Decision {
        Decision {
            id: DecisionId::new(format!("dc-{n}")),
            thread: ThreadKey::new(format!("T{n}")),
            eval: None,
            triggers: Vec::new(),
            kind: DecisionKind::Unavailable,
            reason: "the Router did not decide in time".into(),
            ticket: None,
            would_accept: false,
            policy_version: "v1".into(),
            at_ms,
        }
    }

    #[test]
    fn unavailable_decisions_alert_at_their_count_within_the_window() {
        let limits = Thresholds {
            unavailable: 2,
            ..limits()
        };
        let mut ignore = unavailable(9, T0);
        ignore.kind = DecisionKind::Ignore;
        let mut snapshot = Snapshot {
            decisions: vec![unavailable(1, T0), ignore],
            ..Snapshot::default()
        };
        assert!(
            assess(&snapshot, T0, &limits).is_empty(),
            "one is below the count"
        );
        snapshot.decisions.push(unavailable(2, T0 + 10 * MIN));
        let alerts = assess(&snapshot, T0 + 10 * MIN, &limits);
        assert_eq!(kinds(&alerts), [AlertKind::Unavailable]);
        assert!(alerts[0].text.contains("2 times"), "{}", alerts[0].text);
        assert!(alerts[0].text.contains("T1, T2"), "{}", alerts[0].text);
        let edge = T0 + limits.unavailable_within_ms;
        assert_eq!(
            assess(&snapshot, edge, &limits).len(),
            1,
            "both are still within the window"
        );
        assert!(
            assess(&snapshot, edge + 1, &limits).is_empty(),
            "the first left the window"
        );
    }

    #[test]
    fn spend_alerts_when_an_account_crosses_its_share_of_the_cap() {
        let limits = limits();
        let account = |charges: Vec<(Ms, u64)>, ticket: Option<&str>| Spending {
            account: ticket.map_or("router".into(), |t| format!("ticket:{t}")),
            ticket: ticket.map(TicketId::new),
            cap_microusd: Some(1_000_000),
            since_ms: T0,
            charges,
        };
        let below = Snapshot {
            accounts: vec![account(vec![(T0, 500_000), (T0 + MIN, 299_999)], None)],
            ..Snapshot::default()
        };
        assert!(assess(&below, T0 + HOUR, &limits).is_empty());
        let crossed = vec![(T0, 500_000), (T0 + MIN, 300_000), (T0 + 2 * MIN, 1)];
        let router = Snapshot {
            accounts: vec![account(crossed.clone(), None)],
            ..Snapshot::default()
        };
        let alerts = assess(&router, T0 + HOUR, &limits);
        assert_eq!(kinds(&alerts), [AlertKind::Spend]);
        assert_eq!(alerts[0].since_ms, Some(T0 + MIN), "the call that crossed");
        assert!(
            alerts[0]
                .text
                .starts_with("The Router has committed $0.800001 of its daily cap of $1.00"),
            "{}",
            alerts[0].text
        );
        let ticket = Snapshot {
            accounts: vec![account(crossed, Some("tk1"))],
            ..Snapshot::default()
        };
        let alerts = assess(&ticket, T0 + HOUR, &limits);
        assert!(
            alerts[0].text.starts_with("tk1 has committed"),
            "{}",
            alerts[0].text
        );
        let unknown = Snapshot {
            accounts: vec![Spending {
                cap_microusd: None,
                ..account(vec![(T0, 5_000_000)], None)
            }],
            ..Snapshot::default()
        };
        assert!(
            assess(&unknown, T0, &limits).is_empty(),
            "no cap, no share of it"
        );
    }

    #[test]
    fn an_alert_id_is_stable_within_its_bucket_and_new_in_the_next() {
        let limits = limits();
        let working = of(vec![running(ticket("tk1", T0), T0)]);
        let fires = T0 + limits.stalled_ms;
        let first = assess(&working, fires, &limits)[0].id.clone();
        let within = assess(&working, fires + limits.repeat_ms - 1, &limits)[0]
            .id
            .clone();
        let next = assess(&working, fires + limits.repeat_ms, &limits)[0]
            .id
            .clone();
        assert_eq!(first, within);
        assert_ne!(first, next);
        assert!(first.starts_with("wd_") && first.len() == 19, "{first}");
        let other = of(vec![running(ticket("tk2", T0), T0)]);
        assert_ne!(
            assess(&other, fires, &limits)[0].id,
            first,
            "the subject is part of it"
        );
        assert_eq!(
            alert_id(
                AlertKind::Failing,
                "effect:7",
                None,
                limits.repeat_ms - 1,
                limits.repeat_ms
            ),
            alert_id(AlertKind::Failing, "effect:7", None, 0, limits.repeat_ms),
            "without a start time the buckets are the clock's"
        );
        assert_ne!(
            alert_id(AlertKind::Failing, "effect:7", None, 0, limits.repeat_ms),
            alert_id(AlertKind::Stalled, "effect:7", None, 0, limits.repeat_ms),
            "the kind is part of it"
        );
    }

    fn job_record(name: &str, status: Status, since_ms: Ms) -> Record {
        Record {
            name: name.into(),
            check: "systemd unit l2book-download.service".into(),
            status,
            detail: "inactive (dead)".into(),
            since_ms,
        }
    }

    fn with_jobs(jobs: Vec<Record>) -> Snapshot {
        Snapshot {
            jobs,
            ..Snapshot::default()
        }
    }

    #[test]
    fn a_job_in_trouble_is_one_alert_for_as_long_as_the_trouble_lasts() {
        let limits = limits();
        let down = with_jobs(vec![job_record("l2book", Status::Down, T0)]);
        let alerts = assess(&down, T0 + MIN, &limits);
        assert_eq!(kinds(&alerts), [AlertKind::Job]);
        assert_eq!(
            alerts[0].text,
            "The job \"l2book\" (systemd unit l2book-download.service) is down: inactive (dead)."
        );
        assert_eq!(
            (alerts[0].since_ms, alerts[0].ticket.as_ref()),
            (Some(T0), None)
        );
        assert_eq!(alerts[0].subject, format!("job:l2book:down:{T0}"));
        for later in [HOUR, limits.repeat_ms, 100 * limits.repeat_ms] {
            assert_eq!(
                assess(&down, T0 + later, &limits)[0].id,
                alerts[0].id,
                "`repeat` does not tell a job again: it is told once an incident"
            );
        }

        let again = with_jobs(vec![job_record("l2book", Status::Down, T0 + HOUR)]);
        assert_ne!(
            assess(&again, T0 + 2 * HOUR, &limits)[0].id,
            alerts[0].id,
            "a job that came back and failed again is a new alert"
        );
        let stalled = with_jobs(vec![job_record("l2book", Status::Stalled, T0)]);
        assert_ne!(
            assess(&stalled, T0 + MIN, &limits)[0].id,
            alerts[0].id,
            "a new kind of trouble is a new alert"
        );
        let other = with_jobs(vec![job_record("build", Status::Down, T0)]);
        assert_ne!(assess(&other, T0 + MIN, &limits)[0].id, alerts[0].id);
    }

    #[test]
    fn a_job_that_is_up_raises_nothing_and_each_kind_of_trouble_reads_plainly() {
        let limits = limits();
        let up = with_jobs(vec![job_record("l2book", Status::Up, T0)]);
        assert!(assess(&up, T0 + 100 * HOUR, &limits).is_empty());
        for (status, says) in [
            (Status::Stalled, "has stalled: inactive (dead)."),
            (Status::LowDisk, "is low on disk: inactive (dead)."),
        ] {
            let alerts = assess(
                &with_jobs(vec![job_record("l2book", status, T0)]),
                T0,
                &limits,
            );
            assert!(alerts[0].text.ends_with(says), "{}", alerts[0].text);
        }
        let mut asked = job_record("l2book", Status::Down, T0);
        asked.detail = "is it running?".into();
        let alerts = assess(&with_jobs(vec![asked]), T0, &limits);
        assert!(
            alerts[0].text.ends_with("is down: is it running?"),
            "{}",
            alerts[0].text
        );
    }

    #[test]
    fn a_pause_does_not_hold_job_alerts() {
        let mut snapshot = with_jobs(vec![job_record("l2book", Status::Down, T0)]);
        snapshot.pause = Some(Pause {
            by: "the PAUSE file".into(),
            since_ms: None,
        });
        assert_eq!(
            kinds(&assess(&snapshot, T0 + MIN, &limits())),
            [AlertKind::Job]
        );
    }

    #[test]
    fn ended_and_quiet_tickets_raise_nothing() {
        let mut done = reported(
            running(ticket("tk1", T0), T0),
            ReportStatus::Ready,
            T0,
            None,
        );
        done.ended = Some(solos_agent_core::model::Ended {
            at_ms: T0,
            by: solos_agent_core::model::EndedBy::Executor,
            outcome: solos_agent_core::model::TicketOutcome::Completed,
            reason: "ready".into(),
        });
        let queued = ticket("tk2", T0);
        assert_eq!(queued.stage(), Stage::Queued);
        assert!(assess(&of(vec![done, queued]), T0 + 100 * HOUR, &limits()).is_empty());
    }

    #[test]
    fn amounts_and_durations_read_as_an_operator_writes_them() {
        assert_eq!(usd(500_000), "$0.50");
        assert_eq!(usd(90), "$0.00009");
        assert_eq!(usd(1_000_000), "$1.00");
        assert_eq!(usd(12_345_678), "$12.345678");
        assert_eq!(duration(45_000), "45s");
        assert_eq!(duration(12 * MIN + 5_000), "12m");
        assert_eq!(duration(2 * HOUR + 5 * MIN), "2h 05m");
        assert_eq!(duration(76 * HOUR), "3d 04h");
    }
}
