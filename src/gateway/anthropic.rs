//! The Anthropic Messages wire on an OpenAI-compatible upstream, for a harness
//! that speaks only Messages, such as Claude Code. A request becomes a
//! chat-completions body, and the completion becomes a Message: JSON, or the
//! server-sent events a streaming client reads, framed from the whole reply.
//! Holds, receipts and prices stay the chat wire's; this only translates.

use super::pricing::completion_usage;
use super::upstream::upstream_body;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt::Write as _;

/// The MCP server name a harness gives the broker's tools, so its model sees
/// each one as `mcp__swarm__<tool>`.
pub const MCP_SERVER: &str = "swarm";

/// The broker tool a harness's tool name stands for: `<tool>` for
/// `mcp__swarm__<tool>`, else the name itself.
pub fn broker_tool(name: &str) -> &str {
    name.strip_prefix("mcp__")
        .and_then(|rest| rest.strip_prefix(MCP_SERVER))
        .and_then(|rest| rest.strip_prefix("__"))
        .unwrap_or(name)
}

/// A Messages request, as the chat wire carries it.
#[derive(Clone, Debug, PartialEq)]
pub struct Chat {
    /// The model the request names; the reply names it too.
    pub model: String,
    /// `model`, `messages` and `tools`, as `admit_request` and
    /// `upstream_body` read a chat request.
    pub request: Value,
    /// The request's own output ceiling.
    pub max_tokens: u64,
    pub stop: Vec<String>,
    /// Answer with server-sent events.
    pub stream: bool,
}

#[derive(Deserialize)]
struct Request {
    model: String,
    max_tokens: u64,
    messages: Vec<Message>,
    #[serde(default)]
    system: Option<Value>,
    #[serde(default)]
    tools: Vec<Tool>,
    #[serde(default)]
    stop_sequences: Vec<String>,
    #[serde(default)]
    stream: bool,
}

#[derive(Deserialize)]
struct Message {
    role: Role,
    content: Value,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    User,
    Assistant,
}

/// The content blocks the chat wire can carry. Thinking is dropped: the
/// upstream runs with reasoning off.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<Value>,
        #[serde(default)]
        is_error: bool,
    },
    Thinking {},
    RedactedThinking {},
}

/// A client tool. A server tool has no `input_schema`, so it is refused.
#[derive(Deserialize)]
struct Tool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    input_schema: Value,
}

impl Chat {
    /// Read a Messages request. What the chat wire cannot carry, such as an
    /// image, a server tool or a message out of place, is refused with why.
    pub fn read(body: &Value) -> Result<Self, String> {
        let request = Request::deserialize(body).map_err(|err| err.to_string())?;
        if request.max_tokens == 0 {
            return Err("max_tokens must be at least 1".into());
        }
        let mut messages = Vec::new();
        if let Some(system) = &request.system {
            let system = text_of(system, "the system prompt")?;
            if !system.is_empty() {
                messages.push(json!({"role": "system", "content": system}));
            }
        }
        for message in request.messages {
            let blocks = blocks(message.content)?;
            match message.role {
                Role::User => user(&mut messages, blocks)?,
                Role::Assistant => assistant(&mut messages, blocks)?,
            }
        }
        let tools: Vec<Value> = request
            .tools
            .into_iter()
            .map(|tool| {
                let mut function = json!({"name": tool.name, "parameters": tool.input_schema});
                if let Some(description) = tool.description {
                    function["description"] = json!(description);
                }
                json!({"type": "function", "function": function})
            })
            .collect();
        Ok(Self {
            request: json!({"model": request.model, "messages": messages, "tools": tools}),
            model: request.model,
            max_tokens: request.max_tokens,
            stop: request.stop_sequences,
            stream: request.stream,
        })
    }

    /// The body sent upstream: the shared `upstream_body`, under the lower
    /// of the request's ceiling and `max_output_tokens`, with the stop
    /// sequences.
    pub fn upstream(&self, model: &str, max_output_tokens: u64) -> Value {
        let ceiling = self.max_tokens.min(max_output_tokens);
        let mut body = upstream_body(model, &self.request, ceiling);
        if !self.stop.is_empty() {
            body["stop"] = json!(self.stop);
        }
        body
    }
}

fn blocks(content: Value) -> Result<Vec<Block>, String> {
    match content {
        Value::String(text) => Ok(vec![Block::Text { text }]),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| Block::deserialize(block).map_err(|err| format!("a content block: {err}")))
            .collect(),
        _ => Err("a message's content is text or a list of blocks".into()),
    }
}

/// The text of a system prompt or a tool result: a string, or text blocks.
fn text_of(content: &Value, what: &str) -> Result<String, String> {
    if let Value::String(text) = content {
        return Ok(text.clone());
    }
    let only_text = || format!("{what} carries only text");
    let mut texts = Vec::new();
    for block in content.as_array().ok_or_else(only_text)? {
        match Block::deserialize(block) {
            Ok(Block::Text { text }) => texts.push(text),
            _ => return Err(only_text()),
        }
    }
    Ok(texts.join("\n"))
}

/// A user message: each tool result becomes a tool message, which the chat
/// wire wants right after the call, then the text as one user message.
fn user(messages: &mut Vec<Value>, blocks: Vec<Block>) -> Result<(), String> {
    let mut texts = Vec::new();
    for block in blocks {
        match block {
            Block::Text { text } => texts.push(text),
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let mut text = match &content {
                    Some(content) => text_of(content, "a tool result")?,
                    None => String::new(),
                };
                if is_error {
                    text.insert_str(0, "Error: ");
                }
                messages
                    .push(json!({"role": "tool", "tool_call_id": tool_use_id, "content": text}));
            }
            Block::ToolUse { .. } => return Err("a user message carries a tool_use block".into()),
            Block::Thinking {} | Block::RedactedThinking {} => {}
        }
    }
    if !texts.is_empty() {
        messages.push(json!({"role": "user", "content": texts.join("\n")}));
    }
    Ok(())
}

/// An assistant message: its text, and each tool use as a tool call.
fn assistant(messages: &mut Vec<Value>, blocks: Vec<Block>) -> Result<(), String> {
    let mut texts = Vec::new();
    let mut calls = Vec::new();
    for block in blocks {
        match block {
            Block::Text { text } => texts.push(text),
            Block::ToolUse { id, name, input } => calls.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": input.to_string()},
            })),
            Block::ToolResult { .. } => {
                return Err("an assistant message carries a tool_result block".into());
            }
            Block::Thinking {} | Block::RedactedThinking {} => {}
        }
    }
    if texts.is_empty() && calls.is_empty() {
        return Ok(());
    }
    let content = if texts.is_empty() {
        Value::Null
    } else {
        json!(texts.join("\n"))
    };
    let mut message = json!({"role": "assistant", "content": content});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    messages.push(message);
    Ok(())
}

/// A chat completion as a Message naming `model`.
pub fn message(completion: &Value, model: &str) -> Result<Value, String> {
    let choice = completion["choices"]
        .get(0)
        .ok_or("the upstream reply has no choice")?;
    let reply = &choice["message"];
    let finish = choice["finish_reason"].as_str();
    let mut content = Vec::new();
    match &reply["content"] {
        Value::String(text) if !text.is_empty() => {
            content.push(json!({"type": "text", "text": text}));
        }
        Value::String(_) | Value::Null => {}
        _ => return Err("the upstream reply's content is not text".into()),
    }
    let calls = match &reply["tool_calls"] {
        Value::Null => &[][..],
        Value::Array(calls) => calls.as_slice(),
        _ => return Err("the upstream reply's tool_calls is not a list".into()),
    };
    let mut used = false;
    for call in calls {
        let input = match arguments(&call["function"]["arguments"]) {
            Ok(input) => input,
            // A call cut off by the output ceiling is dropped, as the
            // ceiling's stop reason says.
            Err(_) if finish == Some("length") => continue,
            Err(why) => return Err(why),
        };
        let id = call["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("an upstream tool call has no id")?;
        let name = call["function"]["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or("an upstream tool call has no name")?;
        content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
        used = true;
    }
    let stop_reason = match finish {
        Some("length") => "max_tokens",
        Some("content_filter") => "refusal",
        _ if used => "tool_use",
        _ => "end_turn",
    };
    Ok(json!({
        "id": completion["id"],
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage(&completion["usage"])?,
    }))
}

/// A tool call's arguments as the object a tool use carries. No arguments
/// is the empty object.
fn arguments(arguments: &Value) -> Result<Value, String> {
    let text = arguments.as_str().unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str(text) {
        Ok(input @ Value::Object(_)) => Ok(input),
        _ => Err("an upstream tool call's arguments are not a JSON object".into()),
    }
}

/// A completion's usage as a Message's: prompt cache hits are cache reads,
/// apart from the input, and the upstream writes no cache it bills.
pub fn usage(usage: &Value) -> Result<Value, String> {
    let usage = completion_usage(usage).map_err(|err| err.to_string())?;
    Ok(json!({
        "input_tokens": usage.input_tokens - usage.cached_input_tokens,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": usage.cached_input_tokens,
        "output_tokens": usage.output_tokens,
    }))
}

/// `message` as the events of a stream: its start, each block's start, one
/// delta with its whole text or input, and its stop, then the stop reason
/// and the usage.
pub fn events(message: &Value) -> String {
    let mut out = String::new();
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["usage"]["output_tokens"] = json!(0);
    frame(
        &mut out,
        "message_start",
        &json!({"type": "message_start", "message": start}),
    );
    let blocks = message["content"].as_array().map_or(&[][..], Vec::as_slice);
    for (index, block) in blocks.iter().enumerate() {
        let (opened, delta) = if block["type"] == "tool_use" {
            (
                json!({"type": "tool_use", "id": block["id"], "name": block["name"], "input": {}}),
                json!({"type": "input_json_delta", "partial_json": block["input"].to_string()}),
            )
        } else {
            (
                json!({"type": "text", "text": ""}),
                json!({"type": "text_delta", "text": block["text"]}),
            )
        };
        frame(
            &mut out,
            "content_block_start",
            &json!({"type": "content_block_start", "index": index, "content_block": opened}),
        );
        frame(
            &mut out,
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": index, "delta": delta}),
        );
        frame(
            &mut out,
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        );
    }
    frame(
        &mut out,
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": message["stop_reason"], "stop_sequence": message["stop_sequence"]},
            "usage": message["usage"],
        }),
    );
    frame(&mut out, "message_stop", &json!({"type": "message_stop"}));
    out
}

fn frame(out: &mut String, event: &str, data: &Value) {
    let _ = write!(out, "event: {event}\ndata: {data}\n\n");
}

/// The error body a Messages client reads for an HTTP status.
pub fn error(status: u16, message: &str) -> Value {
    let kind = match status {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    };
    json!({"type": "error", "error": {"type": kind, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code 2.1.231's requests, recorded in bwrap against a stub: a
    /// worker's first turn, and the turn after its `worker_report` call.
    const FIRST_TURN: &str =
        include_str!("../../tests/fixtures/anthropic/claude-code-first-turn.json");
    const TOOL_RESULT: &str =
        include_str!("../../tests/fixtures/anthropic/claude-code-tool-result.json");
    /// Completions shaped like DeepSeek's documented replies; a real one
    /// would cost money.
    const TOOL_CALL: &str = include_str!("../../tests/fixtures/anthropic/deepseek-tool-call.json");
    const TEXT: &str = include_str!("../../tests/fixtures/anthropic/deepseek-text.json");

    fn fixture(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    const SYSTEM: &str = "x-anthropic-billing-header: cc_version=2.1.231.db3; cc_entrypoint=sdk-cli;\n\
        You are a Claude agent, built on Anthropic's Claude Agent SDK.\n\
        You are a worker of a long-running agent. Report with worker_report.";

    fn reminder() -> String {
        fixture(FIRST_TURN)["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn a_text_turn_becomes_a_chat_request_under_the_brokers_terms() {
        let chat = Chat::read(&fixture(FIRST_TURN)).unwrap();
        assert_eq!(
            (chat.model.as_str(), chat.max_tokens, chat.stream),
            ("deepseek-v4-flash", 32_000, true)
        );
        assert!(chat.stop.is_empty());
        let schema = &fixture(FIRST_TURN)["tools"][0]["input_schema"];
        assert_eq!(
            chat.request,
            json!({
                "model": "deepseek-v4-flash",
                "messages": [
                    {"role": "system", "content": SYSTEM},
                    {"role": "user", "content": format!("{}\nReport that the ticket is ready.", reminder())},
                ],
                "tools": [{"type": "function", "function": {
                    "name": "mcp__swarm__worker_report",
                    "description": "Report the ticket's status.",
                    "parameters": schema,
                }}],
            })
        );
        let upstream = chat.upstream("deepseek-v4-flash", 2_048);
        assert_eq!(
            upstream["max_tokens"], 2_048,
            "the broker's ceiling is lower"
        );
        assert_eq!(
            (
                &upstream["temperature"],
                &upstream["thinking"],
                &upstream["stream"]
            ),
            (&json!(0), &json!({"type": "disabled"}), &json!(false))
        );
        assert_eq!(upstream["messages"], chat.request["messages"]);
        assert!(upstream.get("stop").is_none());

        let mut asks = fixture(FIRST_TURN);
        asks["max_tokens"] = json!(64);
        asks["stop_sequences"] = json!(["END"]);
        let upstream = Chat::read(&asks)
            .unwrap()
            .upstream("deepseek-v4-flash", 2_048);
        assert_eq!(
            (&upstream["max_tokens"], &upstream["stop"]),
            (&json!(64), &json!(["END"]))
        );
    }

    #[test]
    fn a_tool_use_and_its_result_become_a_tool_call_and_a_tool_message() {
        let chat = Chat::read(&fixture(TOOL_RESULT)).unwrap();
        let messages = chat.request["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[2],
            json!({"role": "assistant", "content": "Reporting.", "tool_calls": [{
                "id": "toolu_1", "type": "function",
                "function": {"name": "mcp__swarm__worker_report", "arguments": r#"{"status":"ready","text":"done"}"#},
            }]})
        );
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "toolu_1", "content": r#"{"recorded": true, "next": "Stop here."}"#})
        );

        let mut failed = fixture(TOOL_RESULT);
        failed["messages"][2]["content"] = json!([
            {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true, "content": "budget reached"},
            {"type": "text", "text": "Go on."},
        ]);
        failed["messages"][1]["content"]
            .as_array_mut()
            .unwrap()
            .insert(
                0,
                json!({"type": "thinking", "thinking": "hm", "signature": "s"}),
            );
        let messages = Chat::read(&failed).unwrap().request["messages"].clone();
        assert_eq!(messages[2]["content"], "Reporting.", "thinking is dropped");
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "toolu_1", "content": "Error: budget reached"})
        );
        assert_eq!(messages[4], json!({"role": "user", "content": "Go on."}));
    }

    #[test]
    fn a_tool_call_reply_round_trips_into_the_next_request() {
        let completion = fixture(TOOL_CALL);
        let message = message(&completion, "deepseek-v4-flash").unwrap();
        assert_eq!(
            message,
            json!({
                "id": "930c60df-bf64-41c9-a88e-3ec75f81e00e",
                "type": "message",
                "role": "assistant",
                "model": "deepseek-v4-flash",
                "content": [
                    {"type": "text", "text": "Reporting."},
                    {"type": "tool_use", "id": "call_00_t5Gx2k", "name": "mcp__swarm__worker_report",
                     "input": {"status": "ready", "text": "done"}},
                ],
                "stop_reason": "tool_use",
                "stop_sequence": null,
                "usage": {"input_tokens": 156, "cache_creation_input_tokens": 0,
                          "cache_read_input_tokens": 1024, "output_tokens": 32},
            })
        );

        let mut next = fixture(TOOL_RESULT);
        next["messages"][1]["content"] = message["content"].clone();
        next["messages"][2]["content"][0]["tool_use_id"] = json!("call_00_t5Gx2k");
        let messages = Chat::read(&next).unwrap().request["messages"].clone();
        let sent = &completion["choices"][0]["message"]["tool_calls"][0];
        let back = &messages[2]["tool_calls"][0];
        assert_eq!(
            (&back["id"], &back["function"]["name"]),
            (&sent["id"], &sent["function"]["name"])
        );
        let arguments = |call: &Value| -> Value {
            serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap()
        };
        assert_eq!(arguments(back), arguments(sent));
        assert_eq!(messages[2]["content"], "Reporting.");
        assert_eq!(messages[3]["tool_call_id"], "call_00_t5Gx2k");

        let text = message_of(TEXT);
        assert_eq!(
            (&text["content"], &text["stop_reason"]),
            (
                &json!([{"type": "text", "text": "Reported; the ticket has ended."}]),
                &json!("end_turn")
            )
        );
    }

    fn message_of(completion: &str) -> Value {
        message(&fixture(completion), "m").unwrap()
    }

    /// The events of a stream, by name, with their data.
    fn parse(stream: &str) -> Vec<(String, Value)> {
        assert!(stream.ends_with("\n\n"));
        stream
            .trim_end()
            .split("\n\n")
            .map(|frame| {
                let (event, data) = frame.split_once('\n').unwrap();
                let event = event.strip_prefix("event: ").unwrap();
                let data: Value =
                    serde_json::from_str(data.strip_prefix("data: ").unwrap()).unwrap();
                assert_eq!(data["type"], event);
                (event.to_owned(), data)
            })
            .collect()
    }

    #[test]
    fn a_stream_is_framed_from_the_whole_reply_and_reassembles_into_it() {
        let whole = message_of(TOOL_CALL);
        let framed = parse(&events(&whole));
        let names: Vec<&str> = framed.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let start = &framed[0].1["message"];
        assert_eq!(
            (&start["content"], &start["stop_reason"]),
            (&json!([]), &Value::Null)
        );
        assert_eq!(start["usage"]["cache_read_input_tokens"], 1024);
        assert_eq!(start["usage"]["output_tokens"], 0);

        let mut rebuilt: Vec<Value> = Vec::new();
        for (name, data) in &framed {
            let index = data["index"].as_u64().unwrap_or_default() as usize;
            match name.as_str() {
                "content_block_start" => {
                    assert_eq!(index, rebuilt.len());
                    rebuilt.push(data["content_block"].clone());
                }
                "content_block_delta" => match data["delta"]["type"].as_str().unwrap() {
                    "text_delta" => {
                        let text = format!(
                            "{}{}",
                            rebuilt[index]["text"].as_str().unwrap(),
                            data["delta"]["text"].as_str().unwrap()
                        );
                        rebuilt[index]["text"] = json!(text);
                    }
                    "input_json_delta" => {
                        rebuilt[index]["input"] =
                            serde_json::from_str(data["delta"]["partial_json"].as_str().unwrap())
                                .unwrap();
                    }
                    other => panic!("{other}"),
                },
                _ => {}
            }
        }
        assert_eq!(json!(rebuilt), whole["content"]);
        let delta = &framed[7].1;
        assert_eq!(
            delta["delta"],
            json!({"stop_reason": "tool_use", "stop_sequence": null})
        );
        assert_eq!(delta["usage"], whole["usage"]);

        let empty = json!({"id": "x", "content": [], "stop_reason": "end_turn", "stop_sequence": null, "usage": {}});
        let names: Vec<String> = parse(&events(&empty))
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["message_start", "message_delta", "message_stop"]);
        let multiline = message(
            &json!({"id": "x", "choices": [{"message": {"content": "a\nb\n\nc"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}}),
            "m",
        )
        .unwrap();
        assert_eq!(
            parse(&events(&multiline))[2].1["delta"]["text"],
            "a\nb\n\nc",
            "one data line per event"
        );
    }

    #[test]
    fn usage_maps_cache_hits_to_cache_reads_and_refuses_what_pricing_refuses() {
        for (usage, (input, read)) in [
            (
                json!({"prompt_tokens": 100, "completion_tokens": 7}),
                (100, 0),
            ),
            (
                json!({"prompt_tokens": 100, "completion_tokens": 7, "prompt_cache_hit_tokens": 90}),
                (10, 90),
            ),
            (
                json!({"prompt_tokens": 100, "completion_tokens": 7, "prompt_tokens_details": {"cached_tokens": 40}}),
                (60, 40),
            ),
            (
                json!({"prompt_tokens": 100, "completion_tokens": 7, "prompt_cache_miss_tokens": 25}),
                (25, 75),
            ),
        ] {
            assert_eq!(
                super::usage(&usage).unwrap(),
                json!({"input_tokens": input, "cache_creation_input_tokens": 0,
                       "cache_read_input_tokens": read, "output_tokens": 7}),
                "{usage}"
            );
        }
        for bad in [
            json!({"prompt_tokens": 100}),
            json!({"prompt_tokens": 100, "completion_tokens": 7, "prompt_cache_hit_tokens": 101}),
        ] {
            assert!(super::usage(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn stop_reasons_follow_the_finish_reason_and_a_cut_tool_call_is_dropped() {
        let reply = |finish: &str, calls: Value| {
            json!({"id": "r", "choices": [{"message": {"content": "partial", "tool_calls": calls}, "finish_reason": finish}],
                   "usage": {"prompt_tokens": 3, "completion_tokens": 2}})
        };
        let call = |arguments: &str| json!([{"id": "c1", "type": "function", "function": {"name": "f", "arguments": arguments}}]);
        let stop = |completion: Value| {
            message(&completion, "m").map(|m| {
                (
                    m["stop_reason"].clone(),
                    m["content"].as_array().unwrap().len(),
                )
            })
        };
        assert_eq!(stop(reply("stop", Value::Null)), Ok((json!("end_turn"), 1)));
        assert_eq!(
            stop(reply("length", Value::Null)),
            Ok((json!("max_tokens"), 1))
        );
        assert_eq!(
            stop(reply("content_filter", Value::Null)),
            Ok((json!("refusal"), 1))
        );
        assert_eq!(
            stop(reply("stop", call("{}"))),
            Ok((json!("tool_use"), 2)),
            "a call is a tool use whatever the finish"
        );
        assert_eq!(
            stop(reply("length", call(r#"{"text":"cut of"#))),
            Ok((json!("max_tokens"), 1))
        );
        let input =
            message(&reply("tool_calls", call("")), "m").unwrap()["content"][1]["input"].clone();
        assert_eq!(input, json!({}), "no arguments is the empty object");
    }

    #[test]
    fn what_the_chat_wire_cannot_carry_is_refused_with_why() {
        let refused = |edit: &dyn Fn(&mut Value)| {
            let mut body = fixture(TOOL_RESULT);
            edit(&mut body);
            Chat::read(&body).unwrap_err()
        };
        let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}});
        type Edit<'a> = &'a dyn Fn(&mut Value);
        let cases: [(&str, Edit); 9] = [
            ("unknown variant `image`", &|b| {
                b["messages"][0]["content"][1] = image.clone()
            }),
            ("a tool result carries only text", &|b| {
                b["messages"][2]["content"][0]["content"] = json!([image])
            }),
            ("a user message carries a tool_use block", &|b| {
                b["messages"][0]["content"] = b["messages"][1]["content"].clone()
            }),
            ("an assistant message carries a tool_result", &|b| {
                b["messages"][1]["content"] = b["messages"][2]["content"].clone()
            }),
            (
                "missing field `input_schema`",
                &|b| b["tools"] = json!([{"type": "web_search_20250305", "name": "web_search"}]),
            ),
            ("the system prompt carries only text", &|b| {
                b["system"] = json!([image])
            }),
            ("unknown variant `system`", &|b| {
                b["messages"][0]["role"] = json!("system")
            }),
            ("max_tokens must be at least 1", &|b| {
                b["max_tokens"] = json!(0)
            }),
            ("missing field `max_tokens`", &|b| {
                b.as_object_mut().unwrap().remove("max_tokens");
            }),
        ];
        for (why, edit) in cases {
            let error = refused(edit);
            assert!(error.contains(why), "{why}: {error}");
        }
    }

    #[test]
    fn a_reply_that_is_no_message_is_refused_with_why() {
        let usage = json!({"prompt_tokens": 1, "completion_tokens": 1});
        for (reply, why) in [
            (
                json!({"id": "r", "choices": [], "usage": usage}),
                "no choice",
            ),
            (
                json!({"id": "r", "choices": [{"message": {"content": [1]}}], "usage": usage}),
                "not text",
            ),
            (
                json!({"id": "r", "choices": [{"message": {"tool_calls": {}}}], "usage": usage}),
                "not a list",
            ),
            (
                json!({"id": "r", "choices": [{"message": {"tool_calls": [{"function": {"name": "f", "arguments": "{}"}}]}}], "usage": usage}),
                "no id",
            ),
            (
                json!({"id": "r", "choices": [{"message": {"tool_calls": [{"id": "c", "function": {"arguments": "{}"}}]}}], "usage": usage}),
                "no name",
            ),
            (
                json!({"id": "r", "choices": [{"message": {"tool_calls": [{"id": "c", "function": {"name": "f", "arguments": "[1]"}}]}, "finish_reason": "tool_calls"}], "usage": usage}),
                "not a JSON object",
            ),
            (
                json!({"id": "r", "choices": [{"message": {"tool_calls": [{"id": "c", "function": {"name": "f", "arguments": "{"}}]}, "finish_reason": "stop"}], "usage": usage}),
                "not a JSON object",
            ),
            (
                json!({"id": "r", "choices": [{"message": {"content": "x"}}]}),
                "input usage missing",
            ),
        ] {
            let error = message(&reply, "m").unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
        }
    }

    #[test]
    fn errors_carry_the_type_a_client_expects_for_the_status() {
        for (status, kind) in [
            (400, "invalid_request_error"),
            (403, "permission_error"),
            (404, "not_found_error"),
            (422, "invalid_request_error"),
            (503, "overloaded_error"),
            (502, "api_error"),
            (500, "api_error"),
        ] {
            assert_eq!(
                error(status, "why"),
                json!({"type": "error", "error": {"type": kind, "message": "why"}})
            );
        }
    }

    #[test]
    fn only_the_swarm_servers_prefix_names_a_broker_tool() {
        assert_eq!(broker_tool("mcp__swarm__worker_report"), "worker_report");
        assert_eq!(broker_tool("worker_report"), "worker_report");
        assert_eq!(
            broker_tool("mcp__other__worker_report"),
            "mcp__other__worker_report"
        );
        assert_eq!(
            broker_tool("mcp__swarmx__worker_report"),
            "mcp__swarmx__worker_report"
        );
        assert_eq!(broker_tool("Bash"), "Bash");
    }
}
