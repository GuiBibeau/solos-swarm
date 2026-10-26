//! Per-agent progress: budget notices and the summary a partial finding carries.
//!
//! Evidence (arch-risk-a3): 15 of 18 findings were empty `defer` rows because
//! an agent only learned about its envelope when the provider call was refused.
//! These helpers are pure so the thresholds and wording are unit-tested.

use serde_json::{Value, json};

/// Fraction of the envelope (or turn ceiling) at which the first warning fires.
pub const SOFT_FRACTION: f64 = 0.70;
/// Fraction at which fetch_url/run_code are refused and only filing remains.
/// The last 10% of the envelope is therefore a finalization allowance.
pub const FINAL_FRACTION: f64 = 0.90;
/// Progress notes kept per agent, and the size of each.
pub const MAX_NOTES: usize = 8;
const NOTE_CODE_CHARS: usize = 110;
const NOTE_OUT_CHARS: usize = 140;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Soft,
    Final,
}

pub fn level(fraction: f64) -> Option<Level> {
    if fraction >= FINAL_FRACTION {
        Some(Level::Final)
    } else if fraction >= SOFT_FRACTION {
        Some(Level::Soft)
    } else {
        None
    }
}

/// The larger of the money fraction and the turn fraction, so either limit warns.
pub fn fraction(committed: u64, envelope: u64, calls: usize, max_calls: usize) -> f64 {
    let money = committed as f64 / envelope.max(1) as f64;
    let turns = calls as f64 / max_calls.max(1) as f64;
    money.max(turns)
}

pub fn notice(fraction: f64) -> Option<String> {
    let pct = (fraction * 100.0).round() as u32;
    Some(match level(fraction)? {
        Level::Soft => format!(
            "BUDGET WARNING: about {pct}% of your task budget is used. Stop exploring, finish the \
             experiment you are on, and plan to call submit_finding soon with what you have."
        ),
        Level::Final => format!(
            "BUDGET FINAL: about {pct}% of your task budget is used. fetch_url and run_code are \
             now refused. Call submit_finding NOW with your results so far (partial results and \
             open questions are valuable; an unfiled task is not)."
        ),
    })
}

fn clip(text: &str, limit: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        return flat;
    }
    let mut clipped: String = flat.chars().take(limit).collect();
    clipped.push('…');
    clipped
}

/// One line describing a finished run_code call: command head, exit, output tail.
pub fn work_note(code: &str, response: &Value) -> String {
    let stdout = response["stdout"].as_str().unwrap_or_default();
    let tail: String = stdout
        .chars()
        .rev()
        .take(NOTE_OUT_CHARS * 2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!(
        "run_code exit={} `{}` -> {}",
        response["exit_code"],
        clip(code, NOTE_CODE_CHARS),
        clip(&tail, NOTE_OUT_CHARS)
    )
}

pub struct Summary<'a> {
    pub reason: &'a str,
    pub model_calls: usize,
    pub run_code_calls: u32,
    pub committed_microusd: u64,
    pub envelope_microusd: u64,
    pub board_posts: Vec<String>,
    pub notes: &'a [String],
}

/// Text for a partial finding. `None` when the agent did no work, so the
/// historical empty-defer wording is kept for a task that never started.
pub fn partial_text(summary: &Summary) -> Option<String> {
    if summary.model_calls <= 1 && summary.run_code_calls == 0 && summary.board_posts.is_empty() {
        return None;
    }
    let mut text = format!(
        "PARTIAL, not a clean defer: {}. Work done before the stop: {} model calls, {} run_code \
         calls, ${:.2} of a ${:.2} task envelope. This record was filed by the host, not the \
         agent; it summarises the agent's Board posts and last commands. Retry the task from \
         them rather than from scratch.",
        summary.reason,
        summary.model_calls,
        summary.run_code_calls,
        summary.committed_microusd as f64 / 1e6,
        summary.envelope_microusd as f64 / 1e6
    );
    if !summary.board_posts.is_empty() {
        text += &format!(" Board posts: {}", summary.board_posts.join(" | "));
    }
    if !summary.notes.is_empty() {
        text += &format!(" Last work: {}", summary.notes.join(" | "));
    }
    Some(text)
}

pub fn blocked(fraction: f64) -> Value {
    json!({"blocked": true, "budget_notice": notice(fraction).unwrap_or_default()})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_warn_then_block() {
        assert_eq!(level(0.5), None);
        assert_eq!(level(0.70), Some(Level::Soft));
        assert_eq!(level(0.89), Some(Level::Soft));
        assert_eq!(level(0.90), Some(Level::Final));
        assert!(notice(0.4).is_none());
        assert!(notice(0.75).unwrap().contains("WARNING"));
        assert!(notice(0.95).unwrap().contains("submit_finding NOW"));
    }

    #[test]
    fn turn_ceiling_warns_even_when_money_is_low() {
        assert!(fraction(10, 1000, 80, 100) > 0.79);
        assert!(fraction(900, 1000, 1, 100) > 0.89);
        assert!(fraction(0, 0, 0, 0) < 0.01);
    }

    #[test]
    fn work_notes_are_flat_and_bounded() {
        let long = "x".repeat(5000);
        let note = work_note(
            &format!("cd /regimes;\n grep -rn '28' {long}"),
            &json!({"exit_code":0,"stdout":format!("line1\nline2 {long}")}),
        );
        assert!(!note.contains('\n'));
        assert!(note.chars().count() < 340, "{}", note.chars().count());
        assert!(note.starts_with("run_code exit=0"));
    }

    #[test]
    fn idle_agent_keeps_the_plain_defer_and_busy_agent_gets_a_summary() {
        let notes = vec!["run_code exit=0 `ls` -> a".to_owned()];
        let mut summary = Summary {
            reason: "Task budget envelope exhausted",
            model_calls: 1,
            run_code_calls: 0,
            committed_microusd: 5,
            envelope_microusd: 10,
            board_posts: vec![],
            notes: &[],
        };
        assert!(partial_text(&summary).is_none());
        summary.model_calls = 130;
        summary.run_code_calls = 120;
        summary.board_posts = vec!["agent-04: plan".into()];
        summary.notes = &notes;
        let text = partial_text(&summary).unwrap();
        assert!(text.contains("PARTIAL") && text.contains("130 model calls"));
        assert!(text.contains("agent-04: plan") && text.contains("`ls`"));
    }
}
