//! `POST /v1/messages`: the Anthropic Messages facade, for a harness that
//! speaks only Messages, such as Claude Code. The same tokens, accounts,
//! holds, settlement, pause and model contract as `/model`, through the
//! same [`Call`]; `gateway::anthropic` translates each way.

use super::{AgentBroker, Call, Refusal, refuse};
use crate::gateway::{
    self,
    anthropic::{self, Chat},
};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

/// Tells an Anthropic client whether to try a refused call again.
const SHOULD_RETRY: HeaderName = HeaderName::from_static("x-should-retry");

pub(super) async fn messages(
    State(broker): State<AgentBroker>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match reply(&broker, &headers, &body).await {
        Ok(response) => response,
        Err(refusal) => refused(refusal),
    }
}

async fn reply(
    broker: &AgentBroker,
    headers: &HeaderMap,
    body: &Value,
) -> Result<Response, Refusal> {
    let grant = broker.authenticate(headers)?;
    let terms = &grant.terms;
    let chat = Chat::read(body).map_err(|why| refuse(StatusCode::BAD_REQUEST, why))?;
    let upstream = chat.upstream(&terms.upstream.model, terms.max_output_tokens);
    let call = Call {
        input_bytes: serde_json::to_vec(&upstream)
            .map_err(|why| refuse(StatusCode::BAD_REQUEST, why))?
            .len(),
        admitted: gateway::admit_request(&chat.request, &terms.upstream.model, |name| {
            grant
                .principal
                .tools()
                .contains(&anthropic::broker_tool(name))
        }),
        max_output_tokens: upstream["max_tokens"]
            .as_u64()
            .unwrap_or(terms.max_output_tokens),
        upstream,
    };
    let Json(completion) = call.send(broker, &grant).await?;
    // The call is settled by now, so a reply that cannot be read is told
    // as the upstream's fault, not to be tried again.
    let mut message = anthropic::message(&completion, &chat.model)
        .map_err(|why| refuse(StatusCode::BAD_GATEWAY, why))?;
    if chat.stream {
        let headers = [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ];
        return Ok((headers, anthropic::events(&message)).into_response());
    }
    message["swarm_cost_microusd"] = completion["swarm_cost_microusd"].clone();
    Ok(Json(message).into_response())
}

/// Claude Code's preconnect, `HEAD /api/hello`, whose answer it ignores.
pub(super) async fn hello() -> StatusCode {
    StatusCode::OK
}

/// Any other path: refused as a Messages client reads it, and told on
/// stderr, so a harness that starts calling something new shows it.
pub(super) async fn unrouted(method: Method, uri: Uri) -> Response {
    eprintln!("agent broker: no route for {method} {}", uri.path());
    let error = anthropic::error(404, "no such endpoint on the agent broker");
    (StatusCode::NOT_FOUND, Json(error)).into_response()
}

/// A broker refusal as a Messages client reads it. A pause keeps the swarm
/// reply's fields and says when to ask again.
fn refused((status, Json(body)): Refusal) -> Response {
    let why = body["error"]
        .as_str()
        .or_else(|| body["reason"].as_str())
        .unwrap_or("refused");
    let mut error = anthropic::error(status.as_u16(), why);
    let mut headers = HeaderMap::new();
    if body["paused"] == true {
        error["paused"] = json!(true);
        error["retry_after_s"] = body["retry_after_s"].clone();
        if let Some(wait) = body["retry_after_s"].as_u64() {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(wait));
        }
    }
    if status == StatusCode::BAD_GATEWAY {
        headers.insert(SHOULD_RETRY, HeaderValue::from_static("false"));
    }
    (status, headers, Json(error)).into_response()
}
