//! The agent broker's tools, each with its description and input schema:
//! what `GET /tools` serves and what `solos-swarm mcp` lists. The Pi
//! extension keeps its own copy; `tests/fixtures/agent-tools.json` is held
//! equal to both, by a Rust test and a Node test.

use serde::Serialize;
use serde_json::{Value, json};

/// Which set of tools a principal is offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    Router,
    Worker,
}

/// One tool as a model sees it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema of the arguments.
    pub input_schema: Value,
}

/// The tools of `kind`, in the order a session is offered them.
pub fn tools(kind: PrincipalKind) -> Vec<ToolSpec> {
    match kind {
        PrincipalKind::Router => vec![router_decide(), router_read()],
        PrincipalKind::Worker => vec![worker_report(), worker_handback(), run_code(), fetch_url()],
    }
}

fn string(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn required_string(description: &str) -> Value {
    json!({"type": "string", "minLength": 1, "description": description})
}

fn router_decide() -> ToolSpec {
    ToolSpec {
        name: "router_decide",
        description: "Decide one candidate by its eval id: accept it as work, ignore it, or stop one \
                      of the Tickets on its thread. Call it once per candidate. A refused decision \
                      comes back as an error that says why; correct the call and try again.",
        input_schema: json!({
            "type": "object",
            "required": ["eval", "decision", "reason"],
            "properties": {
                "eval": required_string("The eval id on the candidate's first line."),
                "decision": {"type": "string", "enum": ["accept", "ignore", "stop"]},
                "reason": required_string("One sentence: why."),
                "title": string("accept: a short title for the work."),
                "scope": string("accept: what the worker must do, in your words and complete on its own."),
                "plan": {
                    "type": "string",
                    "enum": ["session", "swarm"],
                    "description": "accept: session (one worker, the default) or swarm (wide research and tournaments only).",
                },
                "agents": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 30,
                    "description": "accept with plan swarm: how many agents.",
                },
                "budget_microusd": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "accept: the Ticket's whole budget in microusd. Omit it for the default.",
                },
                "acknowledge": string("accept: the first reply, instead of the standard one. An empty string sends none."),
                "rules": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "accept of inferred work: the proactive rules it matches.",
                },
                "ticket": string("stop: the id of the Ticket to stop."),
            },
        }),
    }
}

fn router_read() -> ToolSpec {
    ToolSpec {
        name: "router_read",
        description: "Read one trigger, thread or ticket by id. The answer names its neighbours by \
                      id; follow them one hop at a time when a candidate needs more context.",
        input_schema: json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "minLength": 1},
                "kind": {
                    "type": "string",
                    "enum": ["trigger", "thread", "ticket"],
                    "description": "Which kind of id it is, when it could be more than one.",
                },
            },
        }),
    }
}

fn worker_report() -> ToolSpec {
    ToolSpec {
        name: "worker_report",
        description: "Report on your ticket. progress: still working, nobody is told. waiting: your \
                      turn is over and nothing is outstanding. ready: the result, sent to whoever \
                      asked; it ends the ticket. blocked or failed: you need a person; say why. \
                      dropped: nothing to do; ends the ticket silently. A refused report comes back \
                      as an error that says why.",
        input_schema: json!({
            "type": "object",
            "required": ["status", "text"],
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["progress", "waiting", "ready", "blocked", "failed", "dropped"],
                },
                "text": required_string("For ready, the complete result for the person who asked; otherwise what happened, briefly."),
                "question": string("One concrete question for a person, when only a person can answer it."),
            },
        }),
    }
}

fn worker_handback() -> ToolSpec {
    ToolSpec {
        name: "worker_handback",
        description: "Hand back a new message that reached your ticket but is not yours to handle: \
                      a request beyond your scope, other work, or a stop. The Router judges it; do \
                      not act on it yourself.",
        input_schema: json!({
            "type": "object",
            "required": ["trigger", "reason"],
            "properties": {
                "trigger": required_string("The trigger id of the message."),
                "reason": required_string("One sentence: why it is not yours."),
            },
        }),
    }
}

fn run_code() -> ToolSpec {
    ToolSpec {
        name: "run_code",
        description: "Run one bash command in an isolated, offline Linux sandbox. /work is your \
                      ticket's directory: the only writable path, kept between calls. Other paths \
                      the operator mounted are read-only. No network, broker socket, host home or \
                      credentials. Files are written into /work before the command. Returns \
                      bounded {exit_code,stdout,stderr,timed_out,duration_ms,truncated}. \
                      timeout_seconds is 1-300 (default 120).",
        input_schema: json!({
            "type": "object",
            "required": ["code"],
            "properties": {
                "code": {"type": "string", "minLength": 1},
                "files": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": {"type": "string"},
                },
                "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 300},
                "stdin": {"type": "string"},
            },
        }),
    }
}

fn fetch_url() -> ToolSpec {
    ToolSpec {
        name: "fetch_url",
        description: "Fetch one live http(s) URL through the broker; returns status, headers and \
                      bounded body. Your only network egress.",
        input_schema: json!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "url": {"type": "string", "minLength": 1, "maxLength": 2048},
                "method": {"type": "string"},
                "headers": {"type": "object", "properties": {}, "additionalProperties": true},
                "body": {"type": "string"},
                "max_bytes": {"type": "number"},
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{PrincipalKind, tools};
    use crate::agent::broker::{ROUTER_TOOLS, WORKER_TOOLS};
    use serde_json::Value;

    /// `tests/broker.test.ts` holds the Pi extension's schemas to the same file.
    #[test]
    fn agent_tool_registry_matches_its_fixture() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/agent-tools.json")).unwrap();
        for (kind, key) in [
            (PrincipalKind::Router, "router"),
            (PrincipalKind::Worker, "worker"),
        ] {
            let registry = serde_json::to_value(tools(kind)).unwrap();
            assert_eq!(
                registry,
                fixture[key],
                "the {key} registry and tests/fixtures/agent-tools.json differ; the registry is\n{}",
                serde_json::to_string_pretty(&registry).unwrap()
            );
        }
    }

    #[test]
    fn the_registry_names_exactly_the_tools_each_principal_may_call() {
        let names = |kind| tools(kind).iter().map(|t| t.name).collect::<Vec<_>>();
        assert_eq!(names(PrincipalKind::Router), ROUTER_TOOLS);
        assert_eq!(names(PrincipalKind::Worker), WORKER_TOOLS);
    }
}
