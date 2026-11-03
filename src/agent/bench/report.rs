//! What a bench run measured, read from its Store once it stopped, and the
//! report that sums the runs up.

use super::Design;
use super::workload::{Workload, WorkloadSpec};
use crate::agent::Summary;
use crate::agent::broker::Account;
use crate::agent::simulated::TimeFactor;
use anyhow::Result;
use serde::Serialize;
use solos_agent_core::model::{
    DecisionKind, DomainEvent, MicroUsd, Ms, ReportStatus, Ticket, TicketId, TriggerId,
};
use solos_agent_store::{SpendState, SqliteStore};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;

/// Why a run stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stopped {
    /// Every request had its result and the Router had nothing left.
    Done,
    Deadline,
    /// The Router's next call could not fit under the run's cap.
    Budget,
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestResult {
    pub request: String,
    pub trigger: TriggerId,
    pub work_minutes: f64,
    /// When the agent stored it, in workload minutes from the run's start.
    pub arrived_minute: Option<f64>,
    /// From arrival to the `ready` Report that ended its Ticket.
    pub completion_minutes: Option<f64>,
    pub ticket: Option<TicketId>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Tokens {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunReport {
    pub design: Design,
    pub run: usize,
    pub seed: u64,
    /// Relative to the output directory.
    pub dir: PathBuf,
    pub stopped: Stopped,
    pub wall_secs: f64,
    pub triggers_fed: usize,
    pub requests: Vec<RequestResult>,
    pub unfinished: Vec<String>,
    pub p50_minutes: Option<f64>,
    pub p95_minutes: Option<f64>,
    pub evals: u64,
    pub evals_expired: u64,
    /// Decisions a Router made on an Eval: accept, ignore or stop.
    pub router_decisions: u64,
    pub router_decisions_by_kind: BTreeMap<String, u64>,
    /// Decisions the Policy gate made in code, with no Router.
    pub gate_decisions: u64,
    /// Evals that expired three times running.
    pub unavailable: u64,
    pub tickets: usize,
    /// Tickets no request started, such as chatter a model Router accepted.
    pub other_tickets: usize,
    /// Calls the ledger held and that reached the provider.
    pub model_calls: u64,
    /// Holds released before the provider billed them.
    pub released_calls: u64,
    pub tokens: Tokens,
    pub spend_microusd: MicroUsd,
    pub summary: Summary,
}

/// Nearest rank: the smallest value with at least `p` percent of the values
/// at or below it.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.clamp(1, sorted.len().max(1)) - 1).copied()
}

/// What a run's Store says once it stopped. `started_ms` is the wall time
/// the first Trigger was due.
pub struct Measure<'a> {
    pub design: Design,
    pub run: usize,
    pub dir: PathBuf,
    pub workload: &'a Workload,
    pub time_factor: TimeFactor,
    pub started_ms: Ms,
    pub stopped: Stopped,
    pub wall_secs: f64,
    pub triggers_fed: usize,
    pub summary: Summary,
}

impl Measure<'_> {
    pub fn read(self, store: &SqliteStore) -> Result<RunReport> {
        let minutes = |wall_ms: Ms| self.time_factor.workload_minutes(wall_ms);
        let mut received: BTreeMap<TriggerId, Ms> = BTreeMap::new();
        let (mut evals, mut evals_expired) = (0, 0);
        for event in store.events()? {
            match event.event {
                DomainEvent::TriggerReceived { trigger, .. } => {
                    received.entry(trigger).or_insert(event.at_ms);
                }
                DomainEvent::EvalStarted { .. } => evals += 1,
                DomainEvent::EvalExpired { .. } => evals_expired += 1,
                _ => {}
            }
        }
        let tickets = store.tickets()?;
        let mut by_origin: BTreeMap<&TriggerId, &Ticket> = BTreeMap::new();
        for ticket in &tickets {
            let first = by_origin.entry(&ticket.origin).or_insert(ticket);
            if ticket.created_ms < first.created_ms {
                *first = ticket;
            }
        }
        let mut requests = Vec::new();
        let mut unfinished = Vec::new();
        let mut done = Vec::new();
        for plan in &self.workload.requests {
            let arrived = received.get(&plan.trigger).copied();
            let ticket = by_origin.get(&plan.trigger).copied();
            let ready = ticket.and_then(first_ready);
            let completion = match (arrived, ready) {
                (Some(arrived), Some(ready)) => Some(minutes(ready.saturating_sub(arrived))),
                _ => None,
            };
            match completion {
                Some(minutes) => done.push(minutes),
                None => unfinished.push(plan.id.clone()),
            }
            requests.push(RequestResult {
                request: plan.id.clone(),
                trigger: plan.trigger.clone(),
                work_minutes: plan.work_ms as f64 / 60_000.0,
                arrived_minute: arrived.map(|at| minutes(at.saturating_sub(self.started_ms))),
                completion_minutes: completion,
                ticket: ticket.map(|t| t.id.clone()),
            });
        }
        let request_triggers: BTreeSet<&TriggerId> =
            self.workload.requests.iter().map(|r| &r.trigger).collect();
        let other_tickets = tickets
            .iter()
            .filter(|t| !request_triggers.contains(&t.origin))
            .count();

        let mut router_decisions_by_kind = BTreeMap::new();
        let (mut gate_decisions, mut unavailable) = (0, 0);
        for decision in store.decisions()? {
            match (decision.eval.is_some(), decision.kind) {
                (false, _) => gate_decisions += 1,
                (true, DecisionKind::Unavailable) => unavailable += 1,
                (true, kind) => {
                    let name = serde_json::to_value(kind)?
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    *router_decisions_by_kind.entry(name).or_insert(0) += 1;
                }
            }
        }

        let mut accounts = vec![Account::Router { daily_microusd: 0 }.key()];
        accounts.extend(tickets.iter().map(|t| Account::ticket_key(&t.id)));
        let (mut model_calls, mut released_calls, mut spend_microusd) = (0, 0, 0);
        let mut tokens = Tokens::default();
        for account in &accounts {
            spend_microusd += store.committed(account, 0)?;
            for row in store.spend(account)? {
                if row.state == SpendState::Released {
                    released_calls += 1;
                    continue;
                }
                model_calls += 1;
                if let Some(used) = row.tokens {
                    tokens.input += used.input;
                    tokens.cached_input += used.cached_input;
                    tokens.output += used.output;
                }
            }
        }
        Ok(RunReport {
            design: self.design,
            run: self.run,
            seed: self.workload.seed,
            dir: self.dir,
            stopped: self.stopped,
            wall_secs: self.wall_secs,
            triggers_fed: self.triggers_fed,
            p50_minutes: percentile(&done, 50.0),
            p95_minutes: percentile(&done, 95.0),
            requests,
            unfinished,
            evals,
            evals_expired,
            router_decisions: router_decisions_by_kind.values().sum(),
            router_decisions_by_kind,
            gate_decisions,
            unavailable,
            tickets: tickets.len(),
            other_tickets,
            model_calls,
            released_calls,
            tokens,
            spend_microusd,
            summary: self.summary,
        })
    }
}

/// When a `ready` Report first ended the Ticket's work.
fn first_ready(ticket: &Ticket) -> Option<Ms> {
    ticket
        .assignments
        .iter()
        .flat_map(|a| &a.reports)
        .find(|r| r.status == ReportStatus::Ready)
        .map(|r| r.at_ms)
}

#[derive(Clone, Debug, Serialize)]
pub struct DesignReport {
    pub design: Design,
    /// Over every request of every run.
    pub p50_minutes: Option<f64>,
    pub p95_minutes: Option<f64>,
    pub requests: usize,
    pub unfinished: usize,
    /// Means per run.
    pub router_decisions: f64,
    pub evals: f64,
    pub model_calls: f64,
    pub input_tokens: f64,
    pub cached_tokens: f64,
    pub output_tokens: f64,
    pub spend_microusd: f64,
    pub runs: Vec<RunReport>,
}

impl DesignReport {
    pub fn of(design: Design, runs: Vec<RunReport>) -> Self {
        let done: Vec<f64> = runs
            .iter()
            .flat_map(|run| &run.requests)
            .filter_map(|r| r.completion_minutes)
            .collect();
        let mean = |value: fn(&RunReport) -> f64| {
            runs.iter().map(value).sum::<f64>() / runs.len().max(1) as f64
        };
        Self {
            design,
            p50_minutes: percentile(&done, 50.0),
            p95_minutes: percentile(&done, 95.0),
            requests: runs.iter().map(|run| run.requests.len()).sum(),
            unfinished: runs.iter().map(|run| run.unfinished.len()).sum(),
            router_decisions: mean(|run| run.router_decisions as f64),
            evals: mean(|run| run.evals as f64),
            model_calls: mean(|run| run.model_calls as f64),
            input_tokens: mean(|run| run.tokens.input as f64),
            cached_tokens: mean(|run| run.tokens.cached_input as f64),
            output_tokens: mean(|run| run.tokens.output as f64),
            spend_microusd: mean(|run| run.spend_microusd as f64),
            runs,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BenchReport {
    pub router: String,
    pub workload: WorkloadSpec,
    pub runs_per_design: usize,
    pub deadline_minutes: f64,
    /// A paid run's cap per run, from `--max-spend-usd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_cap_microusd: Option<MicroUsd>,
    pub designs: Vec<DesignReport>,
}

fn minutes(value: Option<f64>) -> String {
    value.map_or_else(|| "-".into(), |m| format!("{m:.1}"))
}

fn usd(microusd: f64) -> String {
    format!("${:.4}", microusd / 1_000_000.0)
}

fn count(value: f64) -> String {
    if value >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value >= 10_000.0 {
        format!("{:.0}k", value / 1_000.0)
    } else {
        format!("{value:.0}")
    }
}

impl BenchReport {
    /// The article's table, one row per design, then each run.
    pub fn markdown(&self) -> String {
        let spec = &self.workload;
        let seeds = match self.runs_per_design {
            0 | 1 => format!("seed {}", spec.seed),
            n => format!("seeds {} to {}", spec.seed, spec.seed + n as u64 - 1),
        };
        let mut md = format!("# Agent bench ({} router)\n\n", self.router);
        let _ = writeln!(
            md,
            "Workload: {} messages at {} a minute ({:?} arrivals), {} requests with {} to {} \
             minutes of work, {} follow-ups, {} CI and bot messages; {seeds}; {} runs per \
             design. Time factor {} (a workload minute lasts {:.2} s). Minutes are workload \
             minutes.\n",
            spec.messages,
            spec.per_minute,
            spec.arrivals,
            spec.requests,
            spec.work_minutes.min,
            spec.work_minutes.max,
            spec.follow_ups,
            spec.ci_messages,
            self.runs_per_design,
            spec.time_factor.get(),
            spec.time_factor.get() * 60.0,
        );
        md.push_str(
            "| Design | Completion p50 / p95 (min) | Router decisions per run | Evals per run | \
             Model calls per run | Cache-read tokens per run | Spend per run | Unfinished |\n\
             |---|---|---|---|---|---|---|---|\n",
        );
        for design in &self.designs {
            let _ = writeln!(
                md,
                "| {} | {} / {} | {} | {} | {} | {} | {} | {} of {} |",
                design.design,
                minutes(design.p50_minutes),
                minutes(design.p95_minutes),
                count(design.router_decisions),
                count(design.evals),
                count(design.model_calls),
                count(design.cached_tokens),
                usd(design.spend_microusd),
                design.unfinished,
                design.requests,
            );
        }
        md.push_str(
            "\n## Runs\n\n| Design | Run | Seed | p50 / p95 (min) | Router decisions | Gate \
             decisions | Evals | Model calls | Input / cached / output tokens | Spend | \
             Unfinished | Stopped | Wall (s) |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
        );
        for run in self.designs.iter().flat_map(|design| &design.runs) {
            let unfinished = if run.unfinished.is_empty() {
                "0".to_owned()
            } else {
                run.unfinished.join(", ")
            };
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} / {} | {} | {} | {} | {} | {} / {} / {} | {} | {} | {:?} | {:.0} |",
                run.design,
                run.run,
                run.seed,
                minutes(run.p50_minutes),
                minutes(run.p95_minutes),
                run.router_decisions,
                run.gate_decisions,
                run.evals,
                run.model_calls,
                count(run.tokens.input as f64),
                count(run.tokens.cached_input as f64),
                count(run.tokens.output as f64),
                usd(run.spend_microusd as f64),
                unfinished,
                run.stopped,
                run.wall_secs,
            );
        }
        md.push_str(
            "\nCompletion runs from a request's arrival (when the agent stored it) to the `ready` \
             Report that ended its Ticket, pooled over every request of every run (nearest rank). \
             Router decisions are accepts, ignores and stops on an Eval; gate decisions cost no \
             model call. `serial` approximates one session with a subagent tool: one worker slot, \
             and the Router takes nothing new while work runs. report.json has every request.\n",
        );
        md
    }
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn percentiles_are_nearest_rank() {
        let values: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(percentile(&values, 50.0), Some(10.0));
        assert_eq!(percentile(&values, 95.0), Some(19.0));
        assert_eq!(percentile(&[3.0], 95.0), Some(3.0));
        assert_eq!(percentile(&[], 50.0), None);
    }
}
