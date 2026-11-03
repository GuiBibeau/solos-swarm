//! `solos-swarm mcp`: a Model Context Protocol server on stdin and stdout,
//! so any MCP host (Claude Code, Codex) can call governed tools. JSON-RPC
//! 2.0, one message per line. It answers `initialize`, `ping`, `tools/list`
//! and `tools/call`, and takes notifications without a reply. Two sets of
//! tools: one principal's, through its broker ([`proxy`]), and an operator's,
//! over an agent's state directory ([`operator`]).

pub mod operator;
pub mod proxy;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::future::Future;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

/// Newest first. An `initialize` that asks for one of these gets it; any
/// other gets the newest, and the host decides whether it can go on.
pub const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const LINE_LIMIT: usize = 16 * 1024 * 1024;

pub const PARSE_ERROR: i64 = -32_700;
pub const INVALID_REQUEST: i64 = -32_600;
pub const METHOD_NOT_FOUND: i64 = -32_601;
pub const INVALID_PARAMS: i64 = -32_602;
pub const INTERNAL_ERROR: i64 = -32_603;

/// One tool as `tools/list` shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// A tool's answer. An error is still a result: the model reads why and
/// can correct its call.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub text: String,
    pub is_error: bool,
}

impl Outcome {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// A JSON-RPC error, for a request that could not be served at all.
#[derive(Debug, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: INTERNAL_ERROR,
            message: message.into(),
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: INVALID_PARAMS,
            message: message.into(),
        }
    }
}

/// What a server offers. [`serve`] checks a call's tool is listed before it
/// reaches `call`.
pub trait Tools: Send + Sync + 'static {
    /// The `instructions` an `initialize` returns, which a host may show its
    /// model.
    fn instructions(&self) -> &'static str;
    fn list(&self) -> impl Future<Output = Result<Vec<Tool>, RpcError>> + Send;
    fn call(
        &self,
        name: &str,
        arguments: Value,
    ) -> impl Future<Output = Result<Outcome, RpcError>> + Send;
}

/// Serve `tools` on this process's stdin and stdout until stdin ends.
pub async fn serve_stdio(tools: impl Tools) -> Result<()> {
    serve(
        tools,
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
}

/// Serve `tools` on `input` and `output` until `input` ends. Requests run
/// at once, so a call that waits out a pause does not hold up a `ping`;
/// their answers go out as each finishes, then the server exits.
pub async fn serve(
    tools: impl Tools,
    mut input: impl AsyncBufRead + Unpin,
    output: impl AsyncWrite + Unpin + Send + 'static,
) -> Result<()> {
    let tools = Arc::new(tools);
    let (replies, mut outgoing) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(reply) = outgoing.recv().await {
            let mut line = serde_json::to_vec(&reply)?;
            line.push(b'\n');
            output.write_all(&line).await?;
            output.flush().await?;
        }
        anyhow::Ok(())
    });
    let mut calls = JoinSet::new();
    while let Some(line) = crate::harness::bounded_line(&mut input, LINE_LIMIT)
        .await
        .context("cannot read the host's next MCP message")?
    {
        while calls.try_join_next().is_some() {}
        if line.trim_ascii().is_empty() {
            continue;
        }
        let Some((id, method, params)) = parse(&line, &replies) else {
            continue;
        };
        let (tools, replies) = (tools.clone(), replies.clone());
        calls.spawn(async move {
            let reply = match respond(&*tools, &method, params).await {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(error) => failure(id, &error),
            };
            let _ = replies.send(reply);
        });
    }
    while calls.join_next().await.is_some() {}
    drop(replies);
    writer.await.context("the MCP writer stopped")?
}

fn failure(id: Value, error: &RpcError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": error.code, "message": error.message}})
}

/// A request's id, method and params. Notifications, and responses to
/// requests this server never sends, get no answer; a line that is not a
/// request gets its error now.
fn parse(line: &[u8], replies: &mpsc::UnboundedSender<Value>) -> Option<(Value, String, Value)> {
    let reject = |code, message: &str| {
        let error = RpcError {
            code,
            message: message.into(),
        };
        let _ = replies.send(failure(Value::Null, &error));
        None
    };
    let message = match serde_json::from_slice::<Value>(line) {
        Ok(Value::Object(message)) => message,
        Ok(_) => return reject(INVALID_REQUEST, "Invalid Request: not a JSON object"),
        Err(_) => return reject(PARSE_ERROR, "Parse error"),
    };
    let method = message.get("method").and_then(Value::as_str);
    match (message.get("id"), method) {
        (None, _) => None,
        (Some(_), None) if is_response(&message) => None,
        (Some(id), Some(method)) if id.is_string() || id.is_number() => Some((
            id.clone(),
            method.to_owned(),
            message.get("params").cloned().unwrap_or_else(|| json!({})),
        )),
        _ => reject(INVALID_REQUEST, "Invalid Request"),
    }
}

fn is_response(message: &Map<String, Value>) -> bool {
    message.contains_key("result") || message.contains_key("error")
}

async fn respond(tools: &impl Tools, method: &str, params: Value) -> Result<Value, RpcError> {
    match method {
        "initialize" => Ok(initialize(&params, tools.instructions())),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools.list().await?})),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("tools/call names no tool"))?;
            let arguments = match params.get("arguments") {
                None | Some(Value::Null) => json!({}),
                Some(arguments @ Value::Object(_)) => arguments.clone(),
                Some(_) => return Err(RpcError::invalid_params("arguments must be an object")),
            };
            if !tools.list().await?.iter().any(|tool| tool.name == name) {
                return Err(RpcError::invalid_params(format!("Unknown tool: {name}")));
            }
            let outcome = tools.call(name, arguments).await?;
            Ok(json!({
                "content": [{"type": "text", "text": outcome.text}],
                "isError": outcome.is_error,
            }))
        }
        _ => Err(RpcError {
            code: METHOD_NOT_FOUND,
            message: format!("Method not found: {method}"),
        }),
    }
}

fn initialize(params: &Value, instructions: &str) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = PROTOCOL_VERSIONS
        .into_iter()
        .find(|version| Some(*version) == asked)
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "solos-swarm", "version": env!("CARGO_PKG_VERSION")},
        "instructions": instructions,
    })
}

#[cfg(test)]
mod tests {
    use super::{Outcome, RpcError, Tool, Tools, serve};
    use serde_json::{Value, json};

    struct Echo;

    impl Tools for Echo {
        fn instructions(&self) -> &'static str {
            "echo"
        }
        async fn list(&self) -> Result<Vec<Tool>, RpcError> {
            Ok(vec![Tool {
                name: "echo".into(),
                description: "Echo.".into(),
                input_schema: json!({"type": "object"}),
            }])
        }
        async fn call(&self, _: &str, arguments: Value) -> Result<Outcome, RpcError> {
            Ok(Outcome::ok(arguments.to_string()))
        }
    }

    async fn exchange(input: &str) -> Vec<Value> {
        let (output, mut read) = tokio::io::duplex(1 << 16);
        serve(Echo, input.as_bytes(), output).await.unwrap();
        let mut text = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut read, &mut text)
            .await
            .unwrap();
        let mut replies: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        replies.sort_by_key(|reply| reply["id"].to_string());
        replies
    }

    #[tokio::test]
    async fn requests_get_answers_and_notifications_get_none() {
        let replies = exchange(concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n\n",
            r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo","arguments":{"a":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"other"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":5,"result":{}}"#,
            "\n",
            "not json\n",
            r#"[1]"#,
        ))
        .await;
        let ids: Vec<String> = replies.iter().map(|r| r["id"].to_string()).collect();
        assert_eq!(ids, ["\"p\"", "1", "2", "3", "4", "null", "null"]);
        assert_eq!(replies[1]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(
            replies[1]["result"]["capabilities"]["tools"],
            json!({"listChanged": false})
        );
        assert_eq!(replies[0]["result"], json!({}));
        assert_eq!(
            replies[2]["result"],
            json!({"content": [{"type": "text", "text": "{\"a\":1}"}], "isError": false})
        );
        assert_eq!(replies[3]["error"]["code"], -32_602);
        assert_eq!(replies[4]["error"]["code"], -32_601);
        let mut codes: Vec<i64> = replies[5..]
            .iter()
            .map(|r| r["error"]["code"].as_i64().unwrap())
            .collect();
        codes.sort_unstable();
        assert_eq!(codes, [-32_700, -32_600]);
    }

    #[tokio::test]
    async fn an_unknown_protocol_version_gets_the_newest() {
        let replies = exchange(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        )
        .await;
        assert_eq!(
            replies[0]["result"]["protocolVersion"],
            super::PROTOCOL_VERSIONS[0]
        );
    }
}
