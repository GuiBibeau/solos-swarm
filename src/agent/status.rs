//! `solos-swarm agent status`: what the agent is doing and why, for an
//! operator. It opens the Store read-only, so it is safe while `agent serve`
//! runs and changes nothing.

use super::config::{AgentConfig, WatchdogConfig};
use super::watchdog::{
    self, Alert, Known, Pause, Snapshot, Thresholds, assess, clip, describe, duration, plan_label,
    stage_name, usd,
};
use super::{STORE_FILE, utc};
use anyhow::{Context, Result};
use serde::Serialize;
use solos_agent_core::model::{
    Actor, ActorKind, AssignmentId, AssignmentState, Decision, DecisionKind, MicroUsd, Ms, Plan,
    Report, ReportStatus, Stage, ThreadKey, TicketId, TriggerId,
};
use solos_agent_store::SqliteStore;
use std::path::{Path, PathBuf};

/// How many of the latest Decisions the report lists.
const RECENT_DECISIONS: usize = 10;

#[derive(Debug, Serialize)]
pub struct Status {
    pub at_ms: Ms,
    pub state_dir: PathBuf,
    pub schema_version: u32,
    pub pause: Option<Pause>,
    pub router: RouterSpend,
    pub tickets: Vec<TicketStatus>,
    /// The latest, newest first.
    pub decisions: Vec<Decision>,
    /// Effects not done yet, in commit order.
    pub effects: Vec<EffectStatus>,
    /// What the Watchdog would raise now, with these thresholds.
    pub thresholds: Thresholds,
    pub alerts: Vec<Alert>,
}

#[derive(Debug, Serialize)]
pub struct RouterSpend {
    pub committed_today_microusd: MicroUsd,
    /// From the config; `None` is no cap where `cap_known`.
    pub daily_cap_microusd: Option<MicroUsd>,
    /// Whether the config was read, so that no cap is known to be none.
    pub cap_known: bool,
}

#[derive(Debug, Serialize)]
pub struct TicketStatus {
    pub id: TicketId,
    pub title: String,
    pub stage: Stage,
    pub created_ms: Ms,
    pub age_ms: Ms,
    pub plan: Plan,
    /// `None`: the Ticket has no cap.
    pub budget_microusd: Option<MicroUsd>,
    pub committed_microusd: MicroUsd,
    /// The question waiting for a person.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attention: Option<String>,
    /// The Trigger that started it.
    pub request: Option<Request>,
    pub assignment: Option<AssignmentStatus>,
}

#[derive(Debug, Serialize)]
pub struct Request {
    pub trigger: TriggerId,
    pub source: String,
    pub actor: Actor,
    pub thread: Option<ThreadKey>,
    pub at_ms: Ms,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct AssignmentStatus {
    pub id: AssignmentId,
    pub state: AssignmentState,
    pub created_ms: Ms,
    pub reports: usize,
    pub last_report: Option<Report>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EffectStatus {
    pub id: i64,
    pub what: String,
    /// Failed attempts so far.
    pub attempts: u32,
    pub due_ms: Ms,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Read the agent in `state_dir` at `now`. With `config`, the Router's cap,
/// the Watchdog's thresholds and a pause set in agent.yaml count too.
pub fn read(state_dir: &Path, config: Option<&AgentConfig>, now: Ms) -> Result<Status> {
    let path = state_dir.join(STORE_FILE);
    let store = SqliteStore::open_read_only(&path)
        .with_context(|| format!("cannot open {} to read", path.display()))?;
    let known = config.map(Known::of).unwrap_or_default();
    let thresholds = config
        .and_then(|config| config.watchdog.as_ref())
        .map_or_else(Thresholds::default, WatchdogConfig::thresholds);
    let snapshot = Snapshot::read(&store, state_dir, now, known, &thresholds)?;
    let alerts = assess(&snapshot, now, &thresholds);
    let committed = |ticket: Option<&TicketId>| {
        snapshot
            .accounts
            .iter()
            .find(|account| account.ticket.as_ref() == ticket)
            .map_or(0, watchdog::Spending::committed)
    };
    let mut tickets = Vec::new();
    for ticket in &snapshot.tickets {
        let request = store.read_trigger(&ticket.origin)?.map(|trigger| Request {
            thread: trigger.threads.first().cloned(),
            trigger: trigger.id,
            source: trigger.source,
            actor: trigger.actor,
            at_ms: trigger.at_ms,
            text: trigger.text,
        });
        let assignment = ticket.latest_assignment().map(|a| AssignmentStatus {
            id: a.id.clone(),
            state: a.state,
            created_ms: a.created_ms,
            reports: a.reports.len(),
            last_report: a.reports.last().cloned(),
            ended_reason: a.ended_reason.clone(),
        });
        tickets.push(TicketStatus {
            id: ticket.id.clone(),
            title: ticket.title.clone(),
            stage: ticket.stage(),
            created_ms: ticket.created_ms,
            age_ms: now.saturating_sub(ticket.created_ms),
            plan: ticket.plan.clone(),
            budget_microusd: ticket.mandate.budget_microusd,
            committed_microusd: committed(Some(&ticket.id)),
            attention: ticket.attention.clone(),
            request,
            assignment,
        });
    }
    let effects = snapshot
        .effects
        .iter()
        .map(|queued| EffectStatus {
            id: queued.id.0,
            what: describe(&queued.effect),
            attempts: queued.attempts,
            due_ms: queued.due_ms,
            last_error: queued.last_error.clone(),
        })
        .collect();
    Ok(Status {
        at_ms: now,
        state_dir: state_dir.to_owned(),
        schema_version: store.schema_version()?,
        pause: snapshot.pause.clone(),
        router: RouterSpend {
            committed_today_microusd: committed(None),
            daily_cap_microusd: known.router_daily_microusd,
            cap_known: config.is_some(),
        },
        tickets,
        decisions: store.recent_decisions(0, RECENT_DECISIONS)?,
        effects,
        thresholds,
        alerts,
    })
}

/// The report as text.
pub fn render(status: &Status) -> String {
    let now = status.at_ms;
    let ago = |at: Ms| duration(now.saturating_sub(at));
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };
    line(format!(
        "agent status at {} (state {}, store schema {})",
        utc::format(now),
        status.state_dir.display(),
        status.schema_version
    ));
    line(match &status.pause {
        None => "pause: none".to_owned(),
        Some(Pause {
            by,
            since_ms: Some(since),
        }) => format!(
            "pause: held by {by} since {} ({})",
            utc::format(*since),
            ago(*since)
        ),
        Some(Pause { by, since_ms: None }) => format!("pause: held by {by}"),
    });
    let spent = usd(status.router.committed_today_microusd);
    line(match status.router.daily_cap_microusd {
        Some(cap) => format!(
            "router: {spent} committed today (UTC) of a {} daily cap",
            usd(cap)
        ),
        None if status.router.cap_known => {
            format!("router: {spent} committed today (UTC), no cap")
        }
        None => format!("router: {spent} committed today (UTC), no daily cap known"),
    });

    line(format!("open tickets: {}", status.tickets.len()));
    for ticket in &status.tickets {
        line(format!(
            "  {} {}, {} old: \"{}\"",
            ticket.id,
            stage_name(ticket.stage),
            ago(ticket.created_ms),
            ticket.title
        ));
        line(match ticket.budget_microusd {
            Some(budget) => format!(
                "    plan: {}; committed {} of a {} budget",
                plan_label(&ticket.plan),
                usd(ticket.committed_microusd),
                usd(budget)
            ),
            None => format!(
                "    plan: {}; committed {}, no cap",
                plan_label(&ticket.plan),
                usd(ticket.committed_microusd)
            ),
        });
        if let Some(request) = &ticket.request {
            let thread = request
                .thread
                .as_ref()
                .map_or(String::new(), |thread| format!(" on {thread}"));
            line(format!(
                "    asked by {}:{} ({}){thread} at {}: {}",
                request.source,
                request.actor.id,
                actor_kind(request.actor.kind),
                utc::format(request.at_ms),
                clip(&request.text)
            ));
        }
        if let Some(assignment) = &ticket.assignment {
            let state = match assignment.state {
                AssignmentState::Pending => "pending".to_owned(),
                AssignmentState::Running => "running".to_owned(),
                AssignmentState::Ended => format!(
                    "ended ({})",
                    assignment.ended_reason.as_deref().unwrap_or("no reason")
                ),
            };
            let report = match &assignment.last_report {
                None => "no report yet".to_owned(),
                Some(report) => format!(
                    "last report {} {} ago: {}",
                    report_name(report.status),
                    ago(report.at_ms),
                    clip(&report.text)
                ),
            };
            line(format!(
                "    assignment {} {state}, launched {} ago, {report}",
                assignment.id,
                ago(assignment.created_ms)
            ));
        }
        if let Some(attention) = &ticket.attention {
            line(format!("    waits for a person: {}", clip(attention)));
        }
    }

    line(format!("recent decisions: {}", status.decisions.len()));
    for decision in &status.decisions {
        let ticket = decision
            .ticket
            .as_ref()
            .map_or(String::new(), |ticket| format!(" -> {ticket}"));
        line(format!(
            "  {} {} on {}{ticket}: {}",
            utc::format(decision.at_ms),
            decision_name(decision.kind),
            decision.thread,
            clip(&decision.reason)
        ));
    }

    let failing = status.effects.iter().filter(|e| e.attempts > 0).count();
    line(format!(
        "effects: {} pending, {failing} of them failing",
        status.effects.len()
    ));
    for effect in &status.effects {
        let failed = match &effect.last_error {
            Some(error) if effect.attempts > 0 => format!(
                ", {} failed attempt(s), next at {}: {}",
                effect.attempts,
                utc::format(effect.due_ms),
                clip(error)
            ),
            _ => String::new(),
        };
        line(format!("  #{} {}{failed}", effect.id, effect.what));
    }

    line(format!("alerts now: {}", status.alerts.len()));
    for alert in &status.alerts {
        line(format!("  [{}] {}", alert.kind.as_str(), alert.text));
    }
    out
}

fn actor_kind(kind: ActorKind) -> &'static str {
    match kind {
        ActorKind::Person => "person",
        ActorKind::Bot => "bot",
        ActorKind::System => "system",
    }
}

fn report_name(status: ReportStatus) -> &'static str {
    match status {
        ReportStatus::Progress => "progress",
        ReportStatus::Waiting => "waiting",
        ReportStatus::Ready => "ready",
        ReportStatus::Blocked => "blocked",
        ReportStatus::Failed => "failed",
        ReportStatus::Dropped => "dropped",
    }
}

fn decision_name(kind: DecisionKind) -> &'static str {
    match kind {
        DecisionKind::Accept => "accept",
        DecisionKind::Ignore => "ignore",
        DecisionKind::Stop => "stop",
        DecisionKind::Unavailable => "unavailable",
        DecisionKind::Declined => "declined",
        DecisionKind::Answered => "answered",
    }
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::agent::watchdog::Thresholds;
    use solos_agent_core::model::{Plan, Stage, TicketId};

    #[test]
    fn an_empty_agent_renders_every_section() {
        let status = super::Status {
            at_ms: 1_793_611_805_000,
            state_dir: "/srv/agent".into(),
            schema_version: 3,
            pause: None,
            router: super::RouterSpend {
                committed_today_microusd: 1_200,
                daily_cap_microusd: Some(1_000_000),
                cap_known: true,
            },
            tickets: Vec::new(),
            decisions: Vec::new(),
            effects: Vec::new(),
            thresholds: Thresholds::default(),
            alerts: Vec::new(),
        };
        assert_eq!(
            render(&status),
            "agent status at 2026-11-02T09:30:05Z (state /srv/agent, store schema 3)
pause: none
router: $0.0012 committed today (UTC) of a $1.00 daily cap
open tickets: 0
recent decisions: 0
effects: 0 pending, 0 of them failing
alerts now: 0
"
        );
    }

    fn ticket(budget_microusd: Option<u64>) -> super::TicketStatus {
        super::TicketStatus {
            id: TicketId::new("tk_1"),
            title: "Fix it".into(),
            stage: Stage::Working,
            created_ms: 1_793_611_805_000,
            age_ms: 0,
            plan: Plan::Session {
                harness: None,
                environment: None,
                method: None,
            },
            budget_microusd,
            committed_microusd: 1_250_000,
            attention: None,
            request: None,
            assignment: None,
        }
    }

    fn status(daily_cap_microusd: Option<u64>, cap_known: bool) -> super::Status {
        super::Status {
            at_ms: 1_793_611_805_000,
            state_dir: "/srv/agent".into(),
            schema_version: 3,
            pause: None,
            router: super::RouterSpend {
                committed_today_microusd: 1_200,
                daily_cap_microusd,
                cap_known,
            },
            tickets: vec![ticket(Some(2_000_000)), ticket(None)],
            decisions: Vec::new(),
            effects: Vec::new(),
            thresholds: Thresholds::default(),
            alerts: Vec::new(),
        }
    }

    #[test]
    fn an_account_with_no_cap_says_so_instead_of_of_a_budget() {
        let text = render(&status(None, true));
        assert!(
            text.contains("router: $0.0012 committed today (UTC), no cap\n"),
            "{text}"
        );
        assert!(
            text.contains("plan: one worker session; committed $1.25 of a $2.00 budget\n"),
            "a capped Ticket reads as before: {text}"
        );
        assert!(
            text.contains("plan: one worker session; committed $1.25, no cap\n"),
            "{text}"
        );
        assert!(!text.contains("of a $0.00"), "{text}");
        let unread = render(&status(None, false));
        assert!(
            unread.contains("router: $0.0012 committed today (UTC), no daily cap known\n"),
            "without the config, no cap is not known to be none: {unread}"
        );
    }
}
