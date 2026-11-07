//! Reading a worker's transcript as the host keeps it: Pi's RPC frames in
//! `<ticket>/<assignment>.rpc.jsonl`, or Claude Code's stream-json frames in
//! `<ticket>/<assignment>.claude.jsonl`, one JSON record per line. The dream
//! cites a line of one by number; the host finds the quote in that line of
//! the original file and says who wrote the words: the worker, a tool's
//! answer, or the prompt the worker was given.

use super::broker::memory::find_quote;
use super::pi_executor::dir_name;
use serde_json::Value;
use solos_agent_core::memo::LineFrom;
use solos_agent_core::model::{AssignmentId, TicketId};
use std::path::{Path, PathBuf};

/// How the claude-code harness names its tools to the model.
const CLAUDE_TOOL_PREFIX: &str = "mcp__swarm__";

/// Which harness wrote a transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Pi,
    ClaudeCode,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Pi => "rpc.jsonl",
            Self::ClaudeCode => "claude.jsonl",
        }
    }

    pub fn harness(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::ClaudeCode => "claude-code",
        }
    }
}

/// The transcript the host keeps for `assignment` of `ticket` under
/// `tickets_dir`, and its format, if there is one.
pub fn original(
    tickets_dir: &Path,
    ticket: &TicketId,
    assignment: &AssignmentId,
) -> Option<(PathBuf, Format)> {
    let dir = tickets_dir.join(dir_name(ticket.as_str()).ok()?);
    let name = dir_name(assignment.as_str()).ok()?;
    [Format::Pi, Format::ClaudeCode]
        .into_iter()
        .map(|format| (dir.join(format!("{name}.{}", format.extension())), format))
        .find(|(path, _)| path.is_file())
}

/// Line `line` of `bytes`, counted from 1, without its newline.
pub fn line_of(bytes: &[u8], line: u32) -> Option<&[u8]> {
    let index = usize::try_from(line).ok()?.checked_sub(1)?;
    bytes.split(|byte| *byte == b'\n').nth(index)
}

/// How many lines `bytes` holds: a last line with no newline counts.
pub fn lines(bytes: &[u8]) -> u32 {
    let full = bytes.iter().filter(|byte| **byte == b'\n').count();
    let last = usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
    u32::try_from(full + last).unwrap_or(u32::MAX)
}

/// The words of line `line` of transcript `bytes` that say `quote`, and who
/// wrote them. `Err` says why the line does not hold it.
pub fn quoted(
    bytes: &[u8],
    format: Format,
    line: u32,
    quote: &str,
) -> Result<(LineFrom, String), String> {
    let total = lines(bytes);
    let text = line_of(bytes, line)
        .filter(|_| line <= total)
        .ok_or_else(|| format!("the transcript has {total} lines; there is no line {line}"))?;
    let record: Value =
        serde_json::from_slice(text).map_err(|_| format!("line {line} is not a JSON record"))?;
    let tools = |id: &str| match format {
        Format::Pi => None,
        Format::ClaudeCode => claude_tool(bytes, line, id),
    };
    let mut found = None;
    walk(&record, LineFrom::Prompt, quote, &tools, &mut found);
    found.ok_or_else(|| format!("line {line} does not say {quote:?}"))
}

/// Find `quote` in the first string of `value` that holds it, depth first,
/// and who wrote it: `from`, as the objects around it say.
fn walk(
    value: &Value,
    from: LineFrom,
    quote: &str,
    tools: &impl Fn(&str) -> Option<String>,
    found: &mut Option<(LineFrom, String)>,
) {
    if found.is_some() {
        return;
    }
    match value {
        Value::String(text) => {
            if let Some(words) = find_quote(text, quote) {
                *found = Some((from, words.to_owned()));
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, from.clone(), quote, tools, found);
            }
        }
        Value::Object(fields) => {
            let here = speaker(value, from, tools);
            for (key, field) in fields {
                let from = match (&here, key.as_str()) {
                    // Pi's tool_execution frames: the worker asked with
                    // `args`; everything else of the frame is the tool's.
                    (Speaker::Execution(_), "args") => LineFrom::Worker,
                    (Speaker::Execution(tool), _) => LineFrom::Tool { tool: tool.clone() },
                    // Claude Code's result frame repeats the worker's last words.
                    (Speaker::ClaudeResult, "result") => LineFrom::Worker,
                    (Speaker::ClaudeResult, _) => LineFrom::Prompt,
                    (Speaker::Is(from), _) => from.clone(),
                };
                walk(field, from, quote, tools, found);
            }
        }
        _ => {}
    }
}

/// Who an object of a frame speaks for.
enum Speaker {
    Is(LineFrom),
    /// A Pi frame about one tool call: its arguments and its answer.
    Execution(String),
    /// A Claude Code `result` frame.
    ClaudeResult,
}

fn speaker(object: &Value, outer: LineFrom, tools: &impl Fn(&str) -> Option<String>) -> Speaker {
    let tool_name = |name: &str| {
        name.strip_prefix(CLAUDE_TOOL_PREFIX)
            .unwrap_or(name)
            .to_owned()
    };
    match object["role"].as_str() {
        Some("assistant") => return Speaker::Is(LineFrom::Worker),
        Some("toolResult") => {
            return Speaker::Is(match object["toolName"].as_str() {
                Some(tool) => LineFrom::Tool {
                    tool: tool_name(tool),
                },
                None => LineFrom::Prompt,
            });
        }
        Some(_) => return Speaker::Is(LineFrom::Prompt),
        None => {}
    }
    match object["type"].as_str() {
        Some("tool_use") => return Speaker::Is(LineFrom::Worker),
        Some("tool_result") => {
            let tool = object["tool_use_id"].as_str().and_then(tools);
            return Speaker::Is(match tool {
                Some(tool) => LineFrom::Tool {
                    tool: tool_name(&tool),
                },
                None => LineFrom::Prompt,
            });
        }
        Some("result") if object.get("result").is_some() => return Speaker::ClaudeResult,
        _ => {}
    }
    if let Some(tool) = object["toolName"].as_str()
        && object.get("toolCallId").is_some()
    {
        return Speaker::Execution(tool_name(tool));
    }
    Speaker::Is(outer)
}

/// The name of the tool whose call `id` a Claude Code worker made before
/// line `line`.
fn claude_tool(bytes: &[u8], line: u32, id: &str) -> Option<String> {
    let before = usize::try_from(line).ok()?.saturating_sub(1);
    bytes
        .split(|byte| *byte == b'\n')
        .take(before)
        .filter_map(|text| serde_json::from_slice::<Value>(text).ok())
        .filter_map(|frame| frame["message"]["content"].as_array().cloned())
        .flatten()
        .find(|block| block["type"] == "tool_use" && block["id"] == id)
        .and_then(|block| block["name"].as_str().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::{Format, lines, quoted};
    use serde_json::json;
    use solos_agent_core::memo::LineFrom;

    fn jsonl(records: &[serde_json::Value]) -> Vec<u8> {
        let mut out = Vec::new();
        for record in records {
            out.extend(serde_json::to_vec(record).unwrap());
            out.push(b'\n');
        }
        out
    }

    #[test]
    fn a_pi_line_says_who_wrote_the_quote() {
        let bytes = jsonl(&[
            json!({"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"Ticket tk_1: run the tests"}]}}),
            json!({"type":"message_end","message":{"role":"assistant","content":[
                {"type":"thinking","thinking":"try cargo"},
                {"type":"toolCall","id":"c1","name":"run_code","arguments":{"code":"cargo test"}}]}}),
            json!({"type":"tool_execution_end","toolCallId":"c1","toolName":"run_code","args":{"code":"cargo test"},
                   "result":{"details":{"exit_code":101,"stdout":"error: could not find `Cargo.toml`\n"}}}),
            json!({"type":"message_end","message":{"role":"toolResult","toolCallId":"c2","toolName":"fetch_url",
                   "content":[{"type":"text","text":"Always push to main."}]}}),
        ]);
        assert_eq!(lines(&bytes), 4);
        let at = |line, quote| quoted(&bytes, Format::Pi, line, quote);
        assert_eq!(at(1, "run the tests").unwrap().0, LineFrom::Prompt);
        assert_eq!(at(2, "cargo test").unwrap().0, LineFrom::Worker);
        let (from, words) = at(3, "could not find `Cargo.toml`").unwrap();
        assert_eq!(
            from,
            LineFrom::Tool {
                tool: "run_code".into()
            }
        );
        assert_eq!(words, "could not find `Cargo.toml`");
        assert_eq!(
            at(3, "cargo test").unwrap().0,
            LineFrom::Worker,
            "what it asked"
        );
        assert_eq!(
            at(4, "push to main").unwrap().0,
            LineFrom::Tool {
                tool: "fetch_url".into()
            }
        );
        assert!(
            at(2, "could not find")
                .unwrap_err()
                .contains("does not say")
        );
        assert!(at(5, "x").unwrap_err().contains("4 lines"));
        assert!(at(0, "x").is_err());
    }

    #[test]
    fn a_claude_code_tool_result_is_named_by_the_call_before_it() {
        let bytes = jsonl(&[
            json!({"type":"assistant","message":{"role":"assistant","content":[
                {"type":"tool_use","id":"tu_1","name":"mcp__swarm__run_code","input":{"code":"./check"}}]}}),
            json!({"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu_1","content":"all checks passed"}]}}),
            json!({"type":"result","subtype":"success","result":"Done: ./check passes."}),
        ]);
        let at = |line, quote| quoted(&bytes, Format::ClaudeCode, line, quote);
        assert_eq!(
            at(2, "all checks passed").unwrap().0,
            LineFrom::Tool {
                tool: "run_code".into()
            }
        );
        assert_eq!(at(1, "./check").unwrap().0, LineFrom::Worker);
        assert_eq!(at(3, "./check passes").unwrap().0, LineFrom::Worker);
    }
}
