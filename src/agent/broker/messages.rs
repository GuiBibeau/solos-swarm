//! `POST /v1/messages`: the Anthropic Messages facade, for a harness that
//! speaks only Messages, such as Claude Code. The same tokens, accounts,
//! holds, settlement, pause and model contract as `/model`, through the
//! same [`Call`]. On the chat wire `gateway::anthropic` translates each way;
//! on the Anthropic wire `gateway::forward` admits the request, and its
//! reply streams back from the provider's own Messages endpoint as it comes.

use super::{
    AgentBroker, Call, Closing, Grant, ModelFailure, Principal, Refusal, Reserved, cap_reached,
    failed, forbidden, refuse,
};
use crate::gateway::{
    self, AnthropicMessages,
    anthropic::{self, Chat},
    forward::{self, Forward, Tally, WebSearch},
};
use axum::{
    Json,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// Tells an Anthropic client whether to try a refused call again.
const SHOULD_RETRY: HeaderName = HeaderName::from_static("x-should-retry");

/// The most a whole Message from the provider may be. A thinking reply, or
/// one that carries search results, runs to megabytes.
const MESSAGE_LIMIT: usize = 16 * 1024 * 1024;

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
    match broker.inner.provider.messages() {
        Some(wire) => forward(broker, &grant, wire.clone(), body).await,
        None => translate(broker, &grant, body).await,
    }
}

/// The chat wire: the request translated into a chat completion, and the
/// completion back into a Message, or the events of one.
async fn translate(
    broker: &AgentBroker,
    grant: &Arc<Grant>,
    body: &Value,
) -> Result<Response, Refusal> {
    let terms = &grant.terms;
    let chat = Chat::read(body).map_err(|why| refuse(StatusCode::BAD_REQUEST, why))?;
    let upstream = terms.sample(chat.upstream(&terms.upstream.model, terms.max_output_tokens));
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
    let Json(completion) = call.send(broker, grant).await?;
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

/// The Anthropic wire: the request admitted as `/model` admits one and
/// forwarded on the broker's terms, with the broker's key; the reply comes
/// back as the provider sent it, and the call settles from its usage.
async fn forward(
    broker: &AgentBroker,
    grant: &Arc<Grant>,
    wire: AnthropicMessages,
    body: &Value,
) -> Result<Response, Refusal> {
    let terms = &grant.terms;
    let request = Forward::read(body).map_err(|why| refuse(StatusCode::BAD_REQUEST, why))?;
    let upstream = request.upstream(
        &terms.upstream.model,
        terms.max_output_tokens,
        terms.temperature,
        wire.web_search.as_ref(),
    );
    let call = Call {
        input_bytes: serde_json::to_vec(&upstream)
            .map_err(|why| refuse(StatusCode::BAD_REQUEST, why))?
            .len(),
        admitted: request.admit(&terms.upstream.model, |name| {
            grant
                .principal
                .tools()
                .contains(&anthropic::broker_tool(name))
        }),
        max_output_tokens: request.max_tokens.min(terms.max_output_tokens),
        upstream,
    };
    let mut named = None;
    let answer = relay(call, broker, grant, wire, request.stream, &mut named).await;
    if let Principal::Worker(seat) = &grant.principal {
        seat.model_called(answer.as_ref().map(|_| ()), named);
    }
    answer
}

/// Check and hold `call`, then hand it to its own task, so a session that
/// hangs up mid-call still gets its hold closed. The answer is the
/// provider's refusal, its whole Message, or its stream once it begins.
async fn relay(
    call: Call,
    broker: &AgentBroker,
    grant: &Arc<Grant>,
    wire: AnthropicMessages,
    stream: bool,
    named: &mut Option<ModelFailure>,
) -> Result<Response, Refusal> {
    call.check(broker, grant, named)?;
    let prices = &grant.terms.prices;
    let hold = match &wire.web_search {
        Some(search) => search.hold(prices, call.input_bytes, call.max_output_tokens),
        None => prices.request_hold(call.input_bytes, call.max_output_tokens),
    };
    let id = match broker.reserve(&grant.account, &grant.terms, hold).await? {
        Reserved::Held(id) => id,
        Reserved::CapReached { committed, cap } => {
            return Err(cap_reached(grant, named, committed, cap));
        }
    };
    let (head, answer) = oneshot::channel();
    let held = Held {
        broker: broker.clone(),
        grant: grant.clone(),
        id,
        hold,
        max_output_tokens: call.max_output_tokens,
        web_search: wire.web_search,
    };
    tokio::spawn(held.run(wire, call.upstream, stream, head));
    answer.await.map_err(failed)?
}

/// One held call on the Anthropic wire, from its post to its hold's close.
struct Held {
    broker: AgentBroker,
    grant: Arc<Grant>,
    id: solos_agent_store::HoldId,
    hold: u64,
    max_output_tokens: u64,
    web_search: Option<WebSearch>,
}

/// How a forwarded call ended, before its hold is closed.
enum Ended {
    /// The provider refused it or could not be reached.
    Failed(String),
    /// It may have been billed, for what is unknown.
    Unknown(String),
    /// It answered with this usage, and for a client that streams nothing,
    /// this Message.
    Answered {
        receipt: gateway::Receipt,
        searches: u64,
        message: Option<Value>,
    },
}

/// The answer a forwarded call's session is waiting for, until it is sent.
type Head = Option<oneshot::Sender<Result<Response, Refusal>>>;
/// Where a streaming answer's bytes go, once it began.
type Sink = Option<mpsc::Sender<Result<Bytes, std::io::Error>>>;

impl Held {
    async fn run(
        self,
        wire: AnthropicMessages,
        body: Value,
        stream: bool,
        head: oneshot::Sender<Result<Response, Refusal>>,
    ) {
        let (mut head, mut sink): (Head, Sink) = (Some(head), None);
        let exchange = self.exchange(&wire, &body, stream, &mut head, &mut sink);
        let ended = match self.grant.terms.call_limit {
            None => exchange.await,
            Some(limit) => match tokio::time::timeout(limit, exchange).await {
                Ok(ended) => ended,
                Err(_) => Ended::Unknown(format!("the model call took longer than {limit:?}")),
            },
        };
        let account = &self.grant.account;
        let (closing, why, message) = match ended {
            Ended::Failed(why) => (Closing::Failed, Some(why), None),
            Ended::Unknown(why) => (Closing::Unknown, Some(why), None),
            Ended::Answered {
                receipt,
                searches,
                message,
            } => {
                let searched = self.web_search.map_or(0, |search| search.cost(searches));
                let cost = receipt.cost(&self.grant.terms.prices) + searched;
                (
                    Closing::Answered { receipt, cost },
                    None,
                    message.map(|m| (m, cost)),
                )
            }
        };
        let closed = self
            .broker
            .close(account, self.id, self.hold, closing)
            .await;
        let refusal = match (closed, why) {
            (Err(refusal), _) => Some(refusal),
            (Ok(()), Some(why)) => Some(forbidden(why)),
            (Ok(()), None) => None,
        };
        let Some(refusal) = refusal else {
            if let (Some(head), Some((mut message, cost))) = (head, message) {
                message["swarm_cost_microusd"] = json!(cost);
                let _ = head.send(Ok(Json(message).into_response()));
            }
            return;
        };
        if let Some(head) = head {
            let _ = head.send(Err(refusal));
            return;
        }
        // The stream began, so its session is told by the stream's end, as
        // a provider error, and its seat by its failure.
        eprintln!(
            "agent: a streamed model call on {} ended badly: {}",
            account.key(),
            (refusal.1).0["error"].as_str().unwrap_or("refused")
        );
        if let Some(sink) = sink {
            let _ = sink
                .send(Err(std::io::Error::other("the model call ended badly")))
                .await;
        }
        if let Principal::Worker(seat) = &self.grant.principal {
            seat.model_called(Err(&refusal), None);
        }
    }

    /// Post `body` and read the answer: whole for a client that streams
    /// nothing, else passed through to the session as it arrives, with
    /// its usage read on the way.
    async fn exchange(
        &self,
        wire: &AnthropicMessages,
        body: &Value,
        stream: bool,
        head: &mut Head,
        sink: &mut Sink,
    ) -> Ended {
        let mut response = match wire.post(body).await {
            Ok(response) => response,
            Err(why) => return Ended::Failed(format!("{why:#}")),
        };
        if !stream {
            return self.whole(response).await;
        }
        let (bytes, piped) = mpsc::channel(64);
        let body = Body::from_stream(futures_util::stream::unfold(
            piped,
            |mut piped| async move { piped.recv().await.map(|bytes| (bytes, piped)) },
        ));
        let headers = [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ];
        if let Some(head) = head.take() {
            let _ = head.send(Ok((headers, body).into_response()));
        }
        let bytes = sink.insert(bytes);
        let mut tally = Tally::default();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    tally.feed(&chunk);
                    // A session that hung up still has its call read to
                    // the end, for its usage.
                    let _ = bytes.send(Ok(chunk)).await;
                }
                Ok(None) => break,
                Err(_) => return Ended::Unknown("provider response interrupted".into()),
            }
        }
        match tally.finish() {
            Some((id, usage)) => self.answered(&id, &usage, None),
            None => Ended::Unknown("the provider's stream ended without its usage".into()),
        }
    }

    /// A whole Message, read up to its limit.
    async fn whole(&self, mut response: reqwest::Response) -> Ended {
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) if bytes.len() + chunk.len() <= MESSAGE_LIMIT => {
                    bytes.extend_from_slice(&chunk);
                }
                Ok(Some(_)) => return Ended::Unknown("provider response exceeds limit".into()),
                Ok(None) => break,
                Err(_) => return Ended::Unknown("provider response interrupted".into()),
            }
        }
        let Ok(message) = serde_json::from_slice::<Value>(&bytes) else {
            return Ended::Unknown("invalid provider response".into());
        };
        let (id, usage) = (message["id"].clone(), message["usage"].clone());
        self.answered(&id, &usage, Some(message))
    }

    fn answered(&self, id: &Value, usage: &Value, message: Option<Value>) -> Ended {
        match forward::receipt(id, usage, self.max_output_tokens) {
            Ok((receipt, searches)) => Ended::Answered {
                receipt,
                searches,
                message,
            },
            Err(why) => Ended::Unknown(why.to_string()),
        }
    }
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
