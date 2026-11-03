//! A Router and an Executor that need no model and no sandbox, so the whole
//! agent runs in tests and dry runs.

use super::ports::{BoxFuture, Delivery, Executor, Fact, FactSink, Launch, Router};
use anyhow::Result;
use solos_agent_core::model::{
    Accept, Acknowledge, AssignmentId, Candidate, MicroUsd, Plan, ReportIn, ReportStatus, Stage,
    Strength, TicketId, Verdict,
};

const TITLE_CHARS: usize = 72;

/// Accepts an explicit candidate as one worker session, titled by its first
/// line and scoped by its text; ignores the rest.
pub struct ScriptedRouter {
    budget_microusd: MicroUsd,
    ignore_follow_ups: bool,
}

impl ScriptedRouter {
    pub fn new(budget_microusd: MicroUsd) -> Self {
        Self {
            budget_microusd,
            ignore_follow_ups: false,
        }
    }

    /// Also ignore an explicit candidate on a Thread whose Ticket is still
    /// open: without pass-through a follow-up comes here as well as to its
    /// Ticket, which already has it. A handed-back follow-up is ignored
    /// too, so this suits a bench whose workers never hand back.
    pub fn ignoring_follow_ups(mut self, ignore: bool) -> Self {
        self.ignore_follow_ups = ignore;
        self
    }

    fn judge(&self, candidate: &Candidate) -> Verdict {
        if candidate.strength != Strength::Explicit {
            return Verdict::Ignore {
                reason: "the scripted router accepts explicit requests only".into(),
            };
        }
        if self.ignore_follow_ups
            && let Some(open) = candidate.tickets.iter().find(|t| t.stage != Stage::Ended)
        {
            return Verdict::Ignore {
                reason: format!("a follow-up on {}, which already has it", open.id),
            };
        }
        let texts: Vec<&str> = candidate
            .triggers
            .iter()
            .map(|t| t.text.trim())
            .filter(|t| !t.is_empty())
            .collect();
        let Some(first) = texts.first() else {
            return Verdict::Ignore {
                reason: "the request has no text".into(),
            };
        };
        let line = first.lines().next().unwrap_or_default().trim();
        let mut title: String = line.chars().take(TITLE_CHARS).collect();
        if line.chars().count() > TITLE_CHARS {
            title.push_str("...");
        }
        Verdict::Accept(Accept {
            title,
            scope: texts.join("\n"),
            reason: "an explicit request".into(),
            plan: Plan::Session {
                harness: None,
                environment: None,
            },
            budget_microusd: self.budget_microusd,
            acknowledge: Acknowledge::Standard,
            rules: Vec::new(),
        })
    }
}

impl Router for ScriptedRouter {
    fn assess(&self, candidate: Candidate, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        let verdict = self.judge(&candidate);
        Box::pin(async move {
            let eval = candidate.eval;
            facts.send(Fact::Verdict { eval, verdict }).await
        })
    }
}

/// Starts every Assignment and reports it `ready` at once, doing no work.
pub struct ScriptedExecutor;

impl Executor for ScriptedExecutor {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let Launch {
                ticket,
                assignment,
                title,
                ..
            } = launch;
            facts
                .send(Fact::Started {
                    ticket: ticket.clone(),
                    assignment: assignment.clone(),
                })
                .await?;
            let report = ReportIn {
                status: ReportStatus::Ready,
                text: format!("Done: {title}. (Scripted executor: no work ran.)"),
                question: None,
                spent_microusd: 0,
            };
            facts
                .send(Fact::Report {
                    ticket,
                    assignment,
                    report,
                })
                .await
        })
    }

    fn deliver(&self, _: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}
