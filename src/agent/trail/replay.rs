use super::check::{AssignmentTrail, Found, Problem, SecretUse, Step, TrailReport, escaped};
use crate::agent::utc;
use crate::browse::NO_SESSION;
use solos_agent_core::model::{Ms, TicketId};
use solos_agent_core::trail::{
    ActText, CloseReason, Digest, PageAction, PageSeen, StepOutcome, TrailAction, TrailOutcome,
};
use std::fmt::Write as _;

const ACTION_PAD_MAX: usize = 56;

/// `report` as text. With `only`, each of its Assignments' steps print one
/// by one; without, one summary per Ticket.
pub fn render(report: &TrailReport, only: Option<&TicketId>) -> String {
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };
    let Some(chain) = &report.chain else {
        for found in &report.problems {
            if let Problem::Chain { seq, reason } = &found.problem {
                line(format!("chain broken: event {seq} {}", escaped(reason)));
            }
            line(found.to_string());
        }
        return out;
    };
    let head = chain.head.as_str();
    line(format!(
        "chain ok: {}, head {}…{}",
        count(chain.events, "event"),
        head.get(..4).unwrap_or(head),
        head.get(head.len().saturating_sub(2)..).unwrap_or_default()
    ));
    if report.tickets.is_empty() {
        line(match only {
            Some(ticket) => format!("{ticket} has no browser steps"),
            None => "no browser steps".to_owned(),
        });
    }
    for ticket in &report.tickets {
        line(match &ticket.title {
            Some(title) => format!("{}  \"{}\"", ticket.ticket, escaped(title)),
            None => ticket.ticket.to_string(),
        });
        if !ticket.secrets.is_empty() {
            let uses: Vec<String> = ticket.secrets.iter().map(secret).collect();
            line(format!("  secrets: {}", uses.join(" · ")));
        }
        if only.is_some() {
            for assignment in &ticket.assignments {
                for text in assignment_lines(assignment) {
                    line(text);
                }
            }
        }
        let mine = |found: &&Found| found.problem.ticket() == Some(&ticket.ticket);
        let problems = report.problems.iter().filter(mine);
        let (fatal, warned): (Vec<_>, Vec<_>) = problems.clone().partition(|found| found.fatal);
        let mut tally = format!(
            "  {}, {} ok, {} ok, {}",
            count(ticket.tally.steps, "step"),
            count(ticket.tally.signatures_ok, "signature"),
            count(ticket.tally.blobs_ok, "blob"),
            count(fatal.len(), "problem"),
        );
        if !warned.is_empty() {
            let _ = write!(tally, ", {}", count(warned.len(), "warning"));
        }
        line(tally);
        for found in problems {
            line(format!("  {found}"));
        }
    }
    out
}

fn count(n: impl TryInto<u64>, noun: &str) -> String {
    let n = n.try_into().unwrap_or(u64::MAX);
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn short(digest: &Digest) -> String {
    let digits = digest.digits();
    format!("{}…", digits.get(..4).unwrap_or(digits))
}

fn clock(ms: Ms) -> String {
    let full = utc::format(ms);
    full.get(11..19).unwrap_or(&full).to_owned()
}

fn secret(used: &SecretUse) -> String {
    match used {
        SecretUse::Asked { at_ms, secret } => format!("asked {secret} {}", clock(*at_ms)),
        SecretUse::Answered {
            at_ms,
            secret,
            by,
            granted,
        } => {
            let verdict = if *granted { "allowed" } else { "denied" };
            format!(
                "{verdict} {secret} by {} {}",
                escaped(&by.id),
                clock(*at_ms)
            )
        }
        SecretUse::KeyUsed {
            at_ms,
            secret,
            method,
            url,
            ..
        } => format!(
            "key {secret} used on {} {} {}",
            escaped(method),
            escaped(url),
            clock(*at_ms)
        ),
    }
}

fn assignment_lines(assignment: &AssignmentTrail) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(match &assignment.ended {
        Some(reason) => format!("  {}, ended: {}", assignment.assignment, escaped(reason)),
        None => format!("  {}, not ended", assignment.assignment),
    });
    let actions: Vec<String> = assignment.steps.iter().map(action).collect();
    let width = actions
        .iter()
        .map(|said| said.chars().count())
        .max()
        .unwrap_or(0)
        .min(ACTION_PAD_MAX);
    let mut last = None;
    for (step, said) in assignment.steps.iter().zip(&actions) {
        let signed = match &step.answer {
            Some(TrailOutcome::Signed(signed)) => Some(signed),
            _ => None,
        };
        if let Some(signed) = signed {
            let body = &signed.body;
            let now = (&body.environment, &body.session);
            if last != Some(now) {
                lines.push(format!(
                    "   on {} (ed25519:{}…), {}",
                    escaped(&body.environment),
                    signed.signer.digits().get(..8).unwrap_or_default(),
                    if body.session == NO_SESSION {
                        "no browser".to_owned()
                    } else {
                        format!(
                            "session {}…",
                            escaped(body.session.get(..8).unwrap_or(&body.session))
                        )
                    }
                ));
                last = Some(now);
            }
        }
        let at = signed.map_or(step.asked_ms, |signed| signed.body.at_ms);
        let label = format!("n{}", step.n);
        lines.push(format!(
            "   {label:<3} {}  {said:<width$}  {}",
            clock(at),
            outcome(step)
        ));
    }
    lines
}

fn action(step: &Step) -> String {
    let said = match &step.action {
        TrailAction::Open { url, key } => {
            let key = key
                .as_ref()
                .map_or(String::new(), |key| format!(" with key {}", escaped(key)));
            format!("open {}{key}", escaped(url))
        }
        TrailAction::Act {
            action,
            element,
            text,
        } => {
            let mut said = match action {
                PageAction::Click => "click",
                PageAction::Type => "type",
                PageAction::Select => "select",
                PageAction::Press => "press",
                PageAction::Scroll => "scroll",
                PageAction::Back => "back",
            }
            .to_owned();
            if let Some(element) = element {
                let _ = write!(said, " {element}");
            }
            match text {
                Some(ActText::Typed { chars, .. }) => {
                    let _ = write!(said, " ({chars} characters)");
                }
                Some(ActText::Named { text }) => {
                    let _ = write!(said, " \"{}\"", escaped(text));
                }
                None => {}
            }
            said
        }
        TrailAction::Login {
            login,
            username_element,
            password_element,
            submit,
        } => {
            let mut said = format!("login {}", escaped(login));
            for (what, element) in [
                ("username", username_element),
                ("password", password_element),
            ] {
                if let Some(element) = element {
                    let _ = write!(said, " {what} {element}");
                }
            }
            if !submit {
                said.push_str(", without submitting");
            }
            said
        }
        TrailAction::Read { part, element } => {
            let mut said = "read".to_owned();
            if let Some(part) = part {
                let _ = write!(said, " part {part}");
            }
            if let Some(element) = element {
                let _ = write!(said, " element {element}");
            }
            said
        }
        TrailAction::Close { reason } => format!("close ({})", close_reason(*reason)),
    };
    match step.answer.as_ref().and_then(seen) {
        Some(page) => {
            let target = page.target.as_ref().map_or(String::new(), |target| {
                format!(" ({} \"{}\")", escaped(&target.role), escaped(&target.name))
            });
            let filled = if page.filled.is_empty() {
                String::new()
            } else {
                format!(" → {}", escaped(&page.filled.join(", ")))
            };
            format!("{said}{target}{filled}")
        }
        None => said,
    }
}

fn seen(answer: &TrailOutcome) -> Option<&PageSeen> {
    match answer {
        TrailOutcome::Signed(signed) => match &signed.body.outcome {
            StepOutcome::Page(page) => Some(page),
            _ => None,
        },
        TrailOutcome::Lost { .. } => None,
    }
}

fn close_reason(reason: CloseReason) -> &'static str {
    match reason {
        CloseReason::AssignmentEnded => "assignment ended",
        CloseReason::Idle => "idle",
        CloseReason::Shutdown => "shutdown",
        CloseReason::Crashed => "crashed",
    }
}

fn outcome(step: &Step) -> String {
    let signed = match &step.answer {
        None => return "no answer".to_owned(),
        Some(TrailOutcome::Lost { why }) => return format!("lost: {}", escaped(why)),
        Some(TrailOutcome::Signed(signed)) => signed,
    };
    match &signed.body.outcome {
        StepOutcome::Page(page) => {
            let status = page
                .status
                .map_or_else(|| "-".to_owned(), |status| status.to_string());
            let mut said = format!(
                "{status} {} \"{}\"  view {} snapshot {}",
                escaped(&page.url),
                escaped(&page.title),
                short(&page.view),
                short(&page.snapshot)
            );
            for keyed in &page.keyed {
                let _ = write!(
                    said,
                    "  key {} to {} x{}",
                    escaped(&keyed.key),
                    escaped(&keyed.host),
                    keyed.requests
                );
            }
            said
        }
        StepOutcome::NeedsApproval { secret, .. } => {
            format!("needs approval of {}", escaped(secret))
        }
        StepOutcome::Refused { why } => format!("refused: {}", escaped(why)),
        StepOutcome::Closed { steps, reason } => match reason {
            CloseReason::Crashed => format!("closed, crashed after {}", count(*steps, "step")),
            _ => format!("closed, {}", count(*steps, "step")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::count;

    #[test]
    fn a_count_is_plural_except_for_one() {
        assert_eq!(count(0_u32, "step"), "0 steps");
        assert_eq!(count(1_u32, "step"), "1 step");
        assert_eq!(count(5_u64, "blob"), "5 blobs");
    }
}
