//! A worker that ends its turn by writing its answer as plain text, with no
//! `worker_report`, has still answered. The host files the Report in its
//! place, so the person hears it: a text that asks the person something is
//! `blocked` on that question, any other text is `ready`.
//!
//! Only text the worker plainly wrote for the person is used. Its reasoning
//! and any tool call it wrote out as text are not.

use super::broker::{REPORT_TEXT_CHARS, clip};
use serde_json::Value;
use solos_agent_core::model::ReportStatus;

/// The longest text asked as a question: the shortest limit of a Channel's
/// short message (Discord and Slack cut those at 500 characters), so the
/// question reaches the person whole. A longer text is a result, which every
/// Channel pages or cuts as it does a worker's own.
const QUESTION_CHARS: usize = 500;

/// What marks text that is a tool call written out, in the formats models
/// fall back on when they do not use the call channel.
const CALL_MARKERS: [&str; 6] = [
    "<tool_call",
    "<function_call",
    "<invoke",
    "<|tool",
    "<\u{ff5c}DSML\u{ff5c}",
    "[TOOL_CALLS]",
];

/// The keys of a JSON object that is a tool call, not an answer.
const CALL_KEYS: [&str; 6] = [
    "arguments",
    "parameters",
    "tool_calls",
    "tool_call",
    "function_call",
    "tool_name",
];

/// The Report the host files for a turn that ended in plain text.
#[derive(Debug, PartialEq, Eq)]
pub struct Answer {
    pub status: ReportStatus,
    pub text: String,
    pub question: Option<String>,
}

impl Answer {
    /// What a worker's last message said as a Report, or `None` when it said
    /// nothing for a person: no text, only reasoning, or a tool call written
    /// out as text.
    pub fn of(said: &str) -> Option<Self> {
        let text = without_thoughts(said)?;
        let text = text.trim();
        if text.is_empty() || is_a_call(text) {
            return None;
        }
        if asks(text) {
            return Some(Self {
                status: ReportStatus::Blocked,
                text: text.to_owned(),
                question: Some(text.to_owned()),
            });
        }
        Some(Self {
            status: ReportStatus::Ready,
            text: clip(text, REPORT_TEXT_CHARS).0,
            question: None,
        })
    }
}

/// `text` without its `<think>` blocks. A block that never closes is all
/// reasoning, and a closing tag with no opening ends it, so what is left is
/// what came after it.
fn without_thoughts(text: &str) -> Option<String> {
    let mut kept = String::new();
    let mut rest = text;
    while let Some(open) = rest.find("<think>") {
        kept.push_str(&rest[..open]);
        let thought = &rest[open + "<think>".len()..];
        let close = thought.find("</think>")?;
        rest = &thought[close + "</think>".len()..];
    }
    kept.push_str(rest);
    match kept.rsplit_once("</think>") {
        Some((_, after)) => Some(after.to_owned()),
        None => Some(kept),
    }
}

/// Whether `text` is a tool call a model wrote out: a known call marker, or
/// JSON (alone or in one code fence) whose object has a call's keys.
fn is_a_call(text: &str) -> bool {
    if CALL_MARKERS.iter().any(|marker| text.contains(marker)) {
        return true;
    }
    serde_json::from_str::<Value>(unfenced(text)).is_ok_and(|json| has_call_keys(&json))
}

fn has_call_keys(json: &Value) -> bool {
    match json {
        Value::Array(items) => items.iter().any(has_call_keys),
        Value::Object(map) => CALL_KEYS.iter().any(|key| map.contains_key(*key)),
        _ => false,
    }
}

/// The inside of a text that is one code fence, or the text.
fn unfenced(text: &str) -> &str {
    let Some(body) = text.strip_prefix("```") else {
        return text;
    };
    let body = body
        .split_once('\n')
        .map_or("", |(_, rest)| rest)
        .trim_end();
    body.strip_suffix("```").unwrap_or(body).trim()
}

/// Whether a short `text` ends on a question to the person.
fn asks(text: &str) -> bool {
    let closers = |c: char| c.is_whitespace() || "*_`)\"'\u{201d}\u{2019}".contains(c);
    text.chars().count() <= QUESTION_CHARS && text.trim_end_matches(closers).ends_with(['?', '？'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(text: &str) -> Option<Answer> {
        Some(Answer {
            status: ReportStatus::Ready,
            text: text.into(),
            question: None,
        })
    }

    fn blocked(question: &str) -> Option<Answer> {
        Some(Answer {
            status: ReportStatus::Blocked,
            text: question.into(),
            question: Some(question.into()),
        })
    }

    #[test]
    fn a_plain_text_is_the_answer_and_one_that_asks_the_person_is_a_question() {
        assert_eq!(
            Answer::of("  The release notes have 3 lines.\n"),
            ready("The release notes have 3 lines.")
        );
        let long = format!(
            "{}\nWant me to open a pull request?",
            "It passes. ".repeat(60)
        );
        assert!(long.chars().count() > QUESTION_CHARS);
        assert_eq!(
            Answer::of(&long),
            ready(long.trim()),
            "a long answer is a result, its closing offer included"
        );
        for question in [
            "Which environment should I use?",
            "Do you want the **staging** one?",
            "Should I go ahead (it deletes the branch)?\n",
            "Dois-je continuer ? ",
            "Voulez-vous le _staging_?_",
            "続けますか？",
            "Shall I use \"main\"?\"",
        ] {
            assert_eq!(Answer::of(question), blocked(question.trim()), "{question}");
        }
        for statement in [
            "Why does it matter? Because the retry hides the flake.",
            "The test passes.",
            "Done!",
            "What next? is for you to say.",
        ] {
            assert_eq!(Answer::of(statement), ready(statement), "{statement}");
        }
    }

    #[test]
    fn a_text_is_cut_to_the_length_of_a_workers_own_report() {
        let long = "x".repeat(REPORT_TEXT_CHARS + 500);
        let answer = Answer::of(&long).unwrap();
        assert_eq!(answer.status, ReportStatus::Ready);
        assert_eq!(answer.text, format!("{}...", &long[..REPORT_TEXT_CHARS]));
        assert_eq!(answer.question, None);
    }

    #[test]
    fn no_text_or_only_reasoning_says_nothing() {
        for nothing in [
            "",
            "  \n\t ",
            "<think>the user wants a count</think>",
            "<think>\nstill thinking, never closed",
            "I should check first</think>  ",
            "<think>a</think> <think>b</think>",
        ] {
            assert_eq!(Answer::of(nothing), None, "{nothing:?}");
        }
    }

    #[test]
    fn reasoning_written_into_the_text_is_not_sent() {
        assert_eq!(
            Answer::of("<think>count them</think>Three lines."),
            ready("Three lines.")
        );
        assert_eq!(
            Answer::of("Three <think>or four?</think>lines."),
            ready("Three lines.")
        );
        assert_eq!(
            Answer::of("count them first</think>\n\nThree lines."),
            ready("Three lines.")
        );
    }

    #[test]
    fn a_tool_call_written_out_as_text_is_never_sent() {
        let report = r#"{"name":"worker_report","arguments":{"status":"ready","text":"Done"}}"#;
        let calls = [
            report.to_owned(),
            format!("[{report}]"),
            format!("```json\n{report}\n```"),
            format!("```\n{report}\n```\n"),
            r#"{"tool_calls":[{"function":{"name":"run_code"}}]}"#.to_owned(),
            r#"{"tool_name":"run_code","parameters":{"code":"ls"}}"#.to_owned(),
            "<tool_call>{\"name\":\"run_code\"}</tool_call>".to_owned(),
            "I will report now.\n<function_calls><invoke name=\"worker_report\"></invoke>"
                .to_owned(),
            "<\u{ff5c}DSML\u{ff5c}function_calls>".to_owned(),
            "[TOOL_CALLS][{\"name\":\"run_code\"}]".to_owned(),
        ];
        for call in calls {
            assert_eq!(Answer::of(&call), None, "{call}");
        }
        // JSON that is not a call, and talk about calls, are answers.
        for answer in [
            r#"{"count":3,"files":["a.md"]}"#,
            "```json\n{\"count\":3}\n```",
            "[1, 2, 3]",
            "Call worker_report with arguments to finish.",
            "The `arguments` field is optional.",
        ] {
            assert_eq!(Answer::of(answer), ready(answer), "{answer}");
        }
    }
}
