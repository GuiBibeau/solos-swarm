//! An operator's tools over an agent's state directory, so someone in an
//! MCP host can hand the agent work and read its answers: `agent_status`
//! (the `agent status` report, read-only), `agent_request` (files a request
//! as `mcp:<operator>` through the `mcp` Source) and `agent_replies` (what
//! the `mcp` Channel sent back on a thread).

use super::{Outcome, RpcError, Tool, Tools};
use crate::agent::mcp as inbox;
use crate::agent::{AgentConfig, status};
use crate::broker::now_ms;
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

pub struct OperatorTools {
    inner: Arc<Inner>,
}

struct Inner {
    state_dir: PathBuf,
    operator: String,
    /// For `agent_status`: the Router's cap, the Watchdog's thresholds and
    /// a pause set in agent.yaml.
    config: Option<AgentConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    text: String,
    #[serde(default)]
    thread: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepliesArgs {
    thread: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

impl OperatorTools {
    /// The tools of `operator`, whom the Policy knows as `mcp:<operator>`.
    pub fn new(state_dir: PathBuf, operator: String, config: Option<AgentConfig>) -> Result<Self> {
        ensure!(
            inbox::valid_name(&operator),
            "operator {operator:?} may hold only letters, digits, ., _, - and @"
        );
        Ok(Self {
            inner: Arc::new(Inner {
                state_dir,
                operator,
                config,
            }),
        })
    }
}

fn tools() -> Vec<Tool> {
    let tool = |name: &str, description: &str, input_schema: Value| Tool {
        name: name.into(),
        description: description.into(),
        input_schema,
    };
    vec![
        tool(
            "agent_status",
            "What the solos-swarm agent is doing and why: open tickets with their stage, spend and \
             latest report, recent decisions, pending and failing effects, the Router's spend \
             today, the pause and the alerts the Watchdog would raise now. Read-only.",
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "agent_request",
            "Hand the solos-swarm agent a request, as one message from you. Its policy decides \
             whether you may ask; its Router decides whether and how to take it on. Name a thread \
             to follow up on an earlier request; leave it out to start a new one. Returns the \
             thread, where the agent answers: read it with agent_replies.",
            json!({
                "type": "object",
                "required": ["text"],
                "properties": {
                    "text": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": inbox::REQUEST_CHARS,
                        "description": "The request, complete on its own.",
                    },
                    "thread": {
                        "type": "string",
                        "description": "An earlier request's thread, to follow up on it.",
                    },
                },
            }),
        ),
        tool(
            "agent_replies",
            "Read what the solos-swarm agent sent you on a thread, oldest first: its \
             acknowledgement, its questions and its result. Empty until it answers.",
            json!({
                "type": "object",
                "required": ["thread"],
                "properties": {
                    "thread": {"type": "string", "description": "The thread agent_request returned."},
                },
            }),
        ),
    ]
}

/// One call, its arguments read.
enum Call {
    Status,
    Request {
        text: String,
        thread: Option<String>,
    },
    Replies {
        thread: String,
    },
}

impl Call {
    /// The call, or why its arguments are refused.
    fn parse(name: &str, arguments: Value) -> Result<Self, String> {
        fn read<T: serde::de::DeserializeOwned>(arguments: Value) -> Result<T, String> {
            serde_json::from_value(arguments).map_err(|err| format!("bad arguments: {err}"))
        }
        Ok(match name {
            "agent_status" => {
                let NoArgs {} = read(arguments)?;
                Self::Status
            }
            "agent_request" => {
                let Request { text, thread } = read(arguments)?;
                Self::Request { text, thread }
            }
            "agent_replies" => {
                let RepliesArgs { thread } = read(arguments)?;
                Self::Replies { thread }
            }
            _ => return Err(format!("no tool {name}")),
        })
    }
}

impl Inner {
    /// Reads the Store and the outbox or appends to the inbox, so it runs
    /// off the async threads.
    fn run(&self, call: Call) -> Result<String> {
        Ok(match call {
            Call::Status => {
                let report = status::read(&self.state_dir, self.config.as_ref(), now_ms())?;
                serde_json::to_string(&report)?
            }
            Call::Request { text, thread } => {
                let filed = inbox::file(
                    &self.state_dir,
                    &self.operator,
                    thread.as_deref(),
                    &text,
                    now_ms(),
                )?;
                json!({
                    "filed": true,
                    "trigger": filed.trigger,
                    "thread": filed.thread,
                    "next": "The agent acknowledges and answers on this thread when it takes the \
                             request; read them with agent_replies. Nothing comes back if its \
                             policy does not admit you.",
                })
                .to_string()
            }
            Call::Replies { thread } => {
                let replies = inbox::replies(&self.state_dir, &self.operator, &thread)?;
                serde_json::to_string(&replies)?
            }
        })
    }
}

impl Tools for OperatorTools {
    fn instructions(&self) -> &'static str {
        "Hand work to a governed solos-swarm agent and read its answers. agent_request files a \
         request in your name; the agent's policy and Router decide what happens to it, and its \
         replies arrive on the request's thread, which agent_replies reads. agent_status shows \
         what the agent is doing."
    }

    async fn list(&self) -> Result<Vec<Tool>, RpcError> {
        Ok(tools())
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<Outcome, RpcError> {
        let call = match Call::parse(name, arguments) {
            Ok(call) => call,
            Err(refused) => return Ok(Outcome::error(refused)),
        };
        let inner = self.inner.clone();
        let answer = tokio::task::spawn_blocking(move || inner.run(call))
            .await
            .map_err(|err| RpcError::internal(format!("the call stopped: {err}")))?;
        Ok(answer.map_or_else(|err| Outcome::error(format!("{err:#}")), Outcome::ok))
    }
}
