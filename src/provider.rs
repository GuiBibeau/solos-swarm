//! The same bounded transport and outcome contract for hosted and local inference.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Semaphore, watch};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Features {
    pub provider: String,
    pub model: String,
    pub context_tokens: u64,
    pub output_tokens: u64,
    pub modalities: BTreeSet<String>,
    pub tools: bool,
    pub streaming: bool,
    pub local: bool,
    pub billing_scope: String,
    pub idempotent_upstream: bool,
    pub outcome_lookup: bool,
    pub revision: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceRequest {
    pub intent_id: String,
    pub goal_id: String,
    pub task_id: String,
    pub holder: String,
    pub grant_version: u64,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub max_output_tokens: u64,
    pub deadline_ms: u64,
    pub stream: bool,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
    pub temperature: f64,
    pub disable_thinking: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub categories: BTreeMap<String, u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    ReceiptWithoutCompletion,
    DefinitelyNotDispatched,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceResult {
    pub intent_id: String,
    pub outcome: Outcome,
    pub request_id: Option<String>,
    pub model_revision: Option<String>,
    pub usage: Option<Usage>,
    pub completion: Option<Value>,
    /// Quarantined receipt evidence, never a model answer.
    #[serde(default)]
    pub evidence: Option<Value>,
    pub reason: Option<String>,
    pub retry_after_ms: Option<u64>,
    #[serde(default)]
    pub flow: Option<FlowRecord>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlowRecord {
    pub intent_id: String,
    pub goal_id: String,
    pub task_id: String,
    pub holder: String,
    pub recipient: String,
    pub destination: String,
    pub payload_request_bytes: u64,
    pub payload_response_bytes: u64,
    pub opened_ms: u64,
    pub closed_ms: u64,
    pub contacted: bool,
    pub outcome: Outcome,
}
pub type InferenceFuture = Pin<Box<dyn Future<Output = InferenceResult> + Send>>;
pub type SlotFuture = Pin<Box<dyn Future<Output = Result<ProviderSlot>> + Send>>;
pub struct ProviderSlot {
    provider: String,
    model: String,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
pub trait Provider: Send + Sync {
    fn features(&self) -> &Features;
    fn validate(&self, request: &InferenceRequest, now_ms: u64) -> Result<()>;
    fn acquire(&self, deadline_ms: u64, cancel: watch::Receiver<bool>) -> SlotFuture;
    fn dispatch(
        &self,
        request: InferenceRequest,
        cancel: watch::Receiver<bool>,
        slot: ProviderSlot,
    ) -> InferenceFuture;
}

pub struct OpenAiProvider {
    features: Features,
    endpoint: String,
    credential: Option<Arc<zeroize::Zeroizing<String>>>,
    client: reqwest::Client,
    max_request_bytes: usize,
    max_response_bytes: usize,
    slots: Arc<Semaphore>,
}
impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("features", &self.features)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}
impl OpenAiProvider {
    pub fn new(
        features: Features,
        endpoint: String,
        credential: Option<String>,
        concurrency: usize,
        max_request_bytes: usize,
        max_response_bytes: usize,
    ) -> Result<Self> {
        Self::new_with_ca(
            features,
            endpoint,
            credential,
            concurrency,
            max_request_bytes,
            max_response_bytes,
            None,
        )
    }
    pub fn new_with_ca(
        features: Features,
        endpoint: String,
        credential: Option<String>,
        concurrency: usize,
        max_request_bytes: usize,
        max_response_bytes: usize,
        ca_pem: Option<&[u8]>,
    ) -> Result<Self> {
        let url = reqwest::Url::parse(&endpoint)?;
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "provider URL contains credentials or unsupported routing"
        );
        ensure!(
            url.path().ends_with("/chat/completions"),
            "provider must expose the declared chat endpoint"
        );
        ensure!(
            !features.provider.is_empty()
                && !features.model.is_empty()
                && !features.billing_scope.is_empty()
                && !features.revision.is_empty(),
            "provider declaration incomplete"
        );
        ensure!(
            features.context_tokens > 0
                && features.output_tokens > 0
                && features.output_tokens < features.context_tokens
                && concurrency > 0
                && max_request_bytes > 0
                && max_response_bytes > 0,
            "provider has missing bounds"
        );
        if features.local {
            ensure!(
                credential.is_none(),
                "local provider must have no hosted key"
            );
            ensure!(
                url.scheme() == "https"
                    || url.scheme() == "http"
                        && url
                            .host_str()
                            .is_some_and(|h| matches!(h, "127.0.0.1" | "localhost" | "[::1]")),
                "cleartext local provider must be loopback-confined"
            );
        } else {
            ensure!(url.scheme() == "https", "hosted inference requires TLS");
            ensure!(
                credential.as_ref().is_some_and(|k| !k.is_empty()),
                "hosted broker credential missing"
            );
        }
        let mut client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never());
        if let Some(ca) = ca_pem {
            ensure!(
                url.scheme() == "https" && ca.len() <= 65_536,
                "provider CA requires a bounded HTTPS configuration"
            );
            client = client
                .tls_built_in_root_certs(false)
                .add_root_certificate(reqwest::Certificate::from_pem(ca)?);
        }
        let client = client.build()?;
        Ok(Self {
            features,
            endpoint,
            credential: credential.map(|key| Arc::new(zeroize::Zeroizing::new(key))),
            client,
            max_request_bytes,
            max_response_bytes,
            slots: Arc::new(Semaphore::new(concurrency)),
        })
    }
}
fn body(request: &InferenceRequest, features: &Features) -> Value {
    let mut value = json!({"model":request.model,"messages":request.messages,"max_tokens":request.max_output_tokens,"temperature":request.temperature,"stream":request.stream});
    if !request.tools.is_empty() {
        value["tools"] = json!(request.tools);
    }
    if request.stream {
        value["stream_options"] = json!({"include_usage":true});
    }
    if features.provider == "deepseek" {
        value["thinking"] = json!({"type":"disabled"});
    }
    if request.disable_thinking {
        value["chat_template_kwargs"] = json!({"enable_thinking":false});
    }
    value
}
impl Provider for OpenAiProvider {
    fn features(&self) -> &Features {
        &self.features
    }
    fn validate(&self, request: &InferenceRequest, now_ms: u64) -> Result<()> {
        ensure!(
            !request.intent_id.is_empty()
                && !request.goal_id.is_empty()
                && !request.task_id.is_empty()
                && !request.holder.is_empty()
                && request.grant_version > 0,
            "provider invocation is not bound to an admitted effect"
        );
        ensure!(
            request.provider == self.features.provider && request.model == self.features.model,
            "provider/model is not declared"
        );
        ensure!(
            request.input_tokens > 0
                && request.max_output_tokens > 0
                && request.max_output_tokens <= self.features.output_tokens
                && request
                    .input_tokens
                    .checked_add(request.max_output_tokens)
                    .is_some_and(|n| n <= self.features.context_tokens),
            "provider context/output limit exceeded"
        );
        ensure!(
            !request.stream || self.features.streaming,
            "streaming unsupported"
        );
        ensure!(
            request.tools.is_empty() || self.features.tools,
            "model does not advertise tool support"
        );
        ensure!(
            request.temperature.is_finite() && (0.0..=2.0).contains(&request.temperature),
            "invalid generation setting"
        );
        ensure!(
            request.deadline_ms > now_ms && request.deadline_ms - now_ms <= 600_000,
            "invalid inference deadline"
        );
        ensure!(
            !request.messages.is_empty()
                && request.messages.iter().all(|m| m.is_object()
                    && m["role"]
                        .as_str()
                        .is_some_and(|r| matches!(r, "system" | "user" | "assistant" | "tool"))),
            "invalid message contract"
        );
        for message in &request.messages {
            if let Some(parts) = message["content"].as_array() {
                ensure!(
                    parts
                        .iter()
                        .all(|part| part["type"] == "text" && part["text"].is_string()),
                    "unsupported modality must not reach reservation or dispatch"
                );
            } else {
                ensure!(
                    message["content"].is_string()
                        || (message["role"] == "assistant" && message["tool_calls"].is_array()),
                    "invalid text message"
                );
            }
        }
        ensure!(
            request.tools.iter().all(|tool| tool["type"] == "function"
                && tool["function"]["name"].is_string()
                && tool["function"]["parameters"].is_object()),
            "unsupported tool schema"
        );
        ensure!(
            serde_json::to_vec(&body(request, &self.features))?.len() <= self.max_request_bytes,
            "provider request exceeds bytes ceiling"
        );
        Ok(())
    }
    fn acquire(&self, deadline_ms: u64, mut cancel: watch::Receiver<bool>) -> SlotFuture {
        let slots = self.slots.clone();
        let provider = self.features.provider.clone();
        let model = self.features.model.clone();
        Box::pin(async move {
            ensure!(
                !*cancel.borrow() && deadline_ms > now_ms(),
                "provider queue stopped or expired before dispatch"
            );
            let permit = tokio::select! {
                value = slots.acquire_owned() => value?,
                _ = cancel.changed() => anyhow::bail!("cancelled before external dispatch"),
                _ = tokio::time::sleep(Duration::from_millis(deadline_ms.saturating_sub(now_ms()))) => anyhow::bail!("expired before external dispatch"),
            };
            ensure!(
                !*cancel.borrow() && deadline_ms > now_ms(),
                "provider stopped before dispatch"
            );
            Ok(ProviderSlot {
                provider,
                model,
                _permit: permit,
            })
        })
    }
    fn dispatch(
        &self,
        request: InferenceRequest,
        mut cancel: watch::Receiver<bool>,
        slot: ProviderSlot,
    ) -> InferenceFuture {
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let credential = self.credential.clone();
        let features = self.features.clone();
        let max_bytes = self.max_response_bytes;
        let validation = self.validate(&request, now_ms());
        Box::pin(async move {
            let id = request.intent_id.clone();
            let opened_ms = now_ms();
            let received = std::sync::atomic::AtomicU64::new(0);
            let request_bytes = serde_json::to_vec(&body(&request, &features))
                .map_or(0, |bytes| bytes.len() as u64);
            let flow = |outcome: &Outcome| FlowRecord {
                intent_id: id.clone(),
                goal_id: request.goal_id.clone(),
                task_id: request.task_id.clone(),
                holder: request.holder.clone(),
                recipient: format!("provider/{}/model/{}", request.provider, request.model),
                destination: endpoint.clone(),
                payload_request_bytes: if *outcome == Outcome::DefinitelyNotDispatched {
                    0
                } else {
                    request_bytes
                },
                payload_response_bytes: received.load(std::sync::atomic::Ordering::Relaxed),
                opened_ms,
                closed_ms: now_ms(),
                contacted: *outcome != Outcome::DefinitelyNotDispatched,
                outcome: outcome.clone(),
            };
            let result = |outcome, reason: String| InferenceResult {
                intent_id: id.clone(),
                flow: Some(flow(&outcome)),
                outcome,
                request_id: None,
                model_revision: None,
                usage: None,
                completion: None,
                evidence: None,
                reason: Some(reason),
                retry_after_ms: None,
            };
            let unknown_response = |reason: String, bytes: &[u8]| {
                let mut failure = result(Outcome::Unknown, reason);
                let mut text = String::from_utf8_lossy(bytes).into_owned();
                if let Some(key) = &credential {
                    text = text.replace(key.as_str(), "[REDACTED]");
                }
                failure.evidence = Some(
                    json!({"response_sha256":format!("{:x}",sha2::Sha256::digest(bytes)),"response_text":text}),
                );
                failure
            };
            if let Err(error) = validation {
                return result(Outcome::DefinitelyNotDispatched, error.to_string());
            }
            if slot.provider != features.provider || slot.model != features.model {
                return result(
                    Outcome::DefinitelyNotDispatched,
                    "concurrency slot belongs to a different backend".into(),
                );
            }
            let _slot = slot;
            if *cancel.borrow() {
                return result(
                    Outcome::DefinitelyNotDispatched,
                    "stopped before dispatch".into(),
                );
            }
            if *cancel.borrow() || now_ms() >= request.deadline_ms {
                return result(
                    Outcome::DefinitelyNotDispatched,
                    "stopped before dispatch".into(),
                );
            }
            let mut http = client
                .post(&endpoint)
                .timeout(Duration::from_millis(
                    request.deadline_ms.saturating_sub(now_ms()),
                ))
                .json(&body(&request, &features));
            if let Some(key) = &credential {
                http = http.bearer_auth(key.as_str());
            }
            if features.idempotent_upstream {
                http = http.header("Idempotency-Key", &request.intent_id);
            }
            // Once send() is polled, cancellation and transport errors may still be billed.
            let receipt = tokio::select! {
                response=http.send()=>response,
                _=cancel.changed()=>return result(Outcome::Unknown,"cancelled after dispatch admission".into()),
            };
            let mut response = match receipt {
                Ok(r) => r,
                Err(_) => {
                    return result(
                        Outcome::Unknown,
                        "provider transport ended without a receipt".into(),
                    );
                }
            };
            if !response.status().is_success() {
                let mut failure = result(
                    Outcome::Unknown,
                    format!(
                        "provider returned HTTP {}; reconcile before retry",
                        response.status().as_u16()
                    ),
                );
                failure.retry_after_ms = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|value| retry_after(value, std::time::SystemTime::now()));
                return failure;
            }
            let mut bytes = Vec::new();
            let mut interrupted = false;
            loop {
                let chunk = tokio::select! {
                    value=response.chunk()=>value,
                    _=cancel.changed()=>{
                        if request.stream && !bytes.is_empty() {interrupted=true;break;}
                        return unknown_response("cancelled while receiving provider response".into(),&bytes);
                    },
                };
                match chunk {
                    Ok(Some(chunk)) => {
                        received
                            .fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::Relaxed);
                        if bytes.len() + chunk.len() > max_bytes {
                            return unknown_response(
                                "provider response byte limit exceeded".into(),
                                &bytes,
                            );
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(_) => {
                        if request.stream && !bytes.is_empty() {
                            interrupted = true;
                            break;
                        }
                        return unknown_response(
                            "partial provider response; usage not reconciled".into(),
                            &bytes,
                        );
                    }
                }
            }
            let completion = if request.stream {
                parse_sse_receipt(&bytes, false)
            } else {
                serde_json::from_slice(&bytes).context("invalid provider JSON")
            };
            let mut completion = match completion {
                Ok(v) => v,
                Err(error) => return unknown_response(error.to_string(), &bytes),
            };
            if interrupted {
                completion["choices"] = json!([]);
            }
            if let Some(key) = &credential {
                redact_secret(&mut completion, key.as_str());
            }
            match financial_receipt(&completion) {
                Ok((request_id, usage)) => {
                    let valid = validate_completion(&completion);
                    let outcome = if valid.is_ok() {
                        Outcome::Completed
                    } else {
                        Outcome::ReceiptWithoutCompletion
                    };
                    InferenceResult {
                        flow: Some(flow(&outcome)),
                        intent_id: id,
                        outcome,
                        request_id: Some(request_id),
                        model_revision: completion["system_fingerprint"]
                            .as_str()
                            .map(str::to_owned),
                        usage: Some(usage),
                        evidence: valid.is_err().then(|| completion.clone()),
                        completion: valid.is_ok().then_some(completion),
                        reason: valid.err().map(|error| error.to_string()),
                        retry_after_ms: None,
                    }
                }
                Err(error) => {
                    let mut failure = result(Outcome::Unknown, error.to_string());
                    failure.request_id = completion["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned);
                    failure.evidence = Some(completion);
                    failure
                }
            }
        })
    }
}
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
fn redact_secret(value: &mut Value, secret: &str) {
    match value {
        Value::String(text) => {
            if !secret.is_empty() && text.contains(secret) {
                *text = text.replace(secret, "[REDACTED]");
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_secret(value, secret);
            }
        }
        Value::Object(values) => {
            let original = std::mem::take(values);
            for (key, mut value) in original {
                redact_secret(&mut value, secret);
                values.insert(
                    if secret.is_empty() {
                        key
                    } else {
                        key.replace(secret, "[REDACTED]")
                    },
                    value,
                );
            }
        }
        _ => {}
    }
}
fn retry_after(value: &str, now: std::time::SystemTime) -> Option<u64> {
    let delay = value
        .parse::<u64>()
        .ok()
        .and_then(|seconds| seconds.checked_mul(1000))
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()?
                .duration_since(now)
                .ok()
                .and_then(|d| d.as_millis().try_into().ok())
        })?;
    (delay > 0 && delay <= 86_400_000).then_some(delay)
}
pub fn validate_receipt(value: &Value, _maximum_output: u64) -> Result<(String, Usage)> {
    let receipt = financial_receipt(value)?;
    validate_completion(value)?;
    Ok(receipt)
}
fn financial_receipt(value: &Value) -> Result<(String, Usage)> {
    let id = value["id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("provider receipt ID missing")?
        .to_owned();
    let input = value["usage"]["prompt_tokens"]
        .as_u64()
        .context("input usage missing")?;
    let output = value["usage"]["completion_tokens"]
        .as_u64()
        .context("output usage missing")?;
    let cached = value["usage"]["prompt_cache_hit_tokens"]
        .as_u64()
        .or_else(|| value["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64())
        .unwrap_or(0);
    // A reported output overrun is still a real receipt: settlement must retain its charge and engage the ledger stop.
    ensure!(cached <= input, "cached usage exceeds total input");
    let mut categories = BTreeMap::new();
    categories.insert("input_tokens".into(), input - cached);
    categories.insert("cached_input_tokens".into(), cached);
    categories.insert("output_tokens".into(), output);
    Ok((
        id,
        Usage {
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: cached,
            categories,
        },
    ))
}
fn validate_completion(value: &Value) -> Result<()> {
    let choices = value["choices"]
        .as_array()
        .context("provider choices missing")?;
    ensure!(
        choices.len() == 1
            && choices[0]["message"]["role"] == "assistant"
            && (choices[0]["message"]["content"].is_string()
                || choices[0]["message"]["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty())),
        "exactly one model candidate required per funded intent"
    );
    let finish = choices[0]["finish_reason"]
        .as_str()
        .context("terminal finish reason missing")?;
    ensure!(
        matches!(
            finish,
            "stop"
                | "length"
                | "tool_calls"
                | "content_filter"
                | "aborted"
                | "insufficient_system_resource"
        ),
        "unknown terminal finish reason"
    );
    Ok(())
}
pub fn parse_sse(bytes: &[u8]) -> Result<Value> {
    parse_sse_receipt(bytes, true)
}
fn parse_sse_receipt(bytes: &[u8], require_complete: bool) -> Result<Value> {
    let text = std::str::from_utf8(bytes).context("stream not valid UTF-8")?;
    let mut id = None;
    let mut model = None;
    let mut fingerprint = None;
    let mut content = String::new();
    let mut calls: BTreeMap<usize, Value> = BTreeMap::new();
    let mut finish = None;
    let mut usage = None;
    let mut done = false;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            done = true;
            continue;
        }
        ensure!(!done, "data after terminal stream frame");
        let frame: Value = serde_json::from_str(data).context("invalid streaming frame")?;
        let next = frame["id"]
            .as_str()
            .context("stream frame has no receipt ID")?;
        if let Some(old) = &id {
            ensure!(old == next, "stream mixed upstream receipts");
        } else {
            id = Some(next.to_owned());
        }
        if frame["model"].is_string() {
            model = Some(frame["model"].clone());
        }
        if frame["system_fingerprint"].is_string() {
            fingerprint = Some(frame["system_fingerprint"].clone());
        }
        if frame["usage"].is_object() {
            usage = Some(frame["usage"].clone());
        }
        let Some(choice) = frame["choices"].as_array().and_then(|c| c.first()) else {
            continue;
        };
        if let Some(part) = choice["delta"]["content"].as_str() {
            content.push_str(part);
        }
        if let Some(parts) = choice["delta"]["tool_calls"].as_array() {
            for part in parts {
                let index = part["index"]
                    .as_u64()
                    .context("tool stream index missing")? as usize;
                ensure!(index < 128, "stream tool count exceeded");
                let call = calls.entry(index).or_insert_with(
                    || json!({"id":"","type":"function","function":{"name":"","arguments":""}}),
                );
                for path in ["id", "name", "arguments"] {
                    let addition = if path == "id" {
                        &part[path]
                    } else {
                        &part["function"][path]
                    };
                    if let Some(addition) = addition.as_str() {
                        let slot = if path == "id" {
                            &mut call[path]
                        } else {
                            &mut call["function"][path]
                        };
                        let value = slot
                            .as_str()
                            .context("invalid assembled tool call")?
                            .to_owned()
                            + addition;
                        *slot = json!(value);
                    }
                }
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            finish = Some(reason.to_owned());
        }
    }
    ensure!(
        usage.is_some(),
        "stream ended without usage; outcome unknown"
    );
    let complete = done && finish.is_some();
    ensure!(
        !require_complete || complete,
        "stream ended without terminal completion"
    );
    let mut message = json!({"role":"assistant","content":content});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls.into_values().collect::<Vec<_>>());
    }
    // An incomplete answer cannot erase an explicit, consistently attributed financial receipt.
    let choices = if complete {
        json!([{"index":0,"message":message,"finish_reason":finish}])
    } else {
        json!([])
    };
    Ok(
        json!({"id":id.context("empty stream")?,"object":"chat.completion","model":model,"system_fingerprint":fingerprint,"choices":choices,"usage":usage}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unusable_completion_cannot_discard_a_paid_usage_receipt() {
        let mut value = json!({"id":"paid-invalid","usage":{"prompt_tokens":4,"completion_tokens":100},"choices":[]});
        assert_eq!(
            financial_receipt(&value).unwrap().1.categories["output_tokens"],
            100
        );
        assert!(validate_completion(&value).is_err());
        value["choices"] =
            json!([{"message":{"role":"assistant","content":"x"},"finish_reason":"unrecognized"}]);
        assert_eq!(financial_receipt(&value).unwrap().0, "paid-invalid");
        assert!(validate_completion(&value).is_err());
    }
    #[test]
    fn terminal_stream_usage_survives_a_missing_done_frame() {
        let bytes = b"data: {\"id\":\"paid-stream\",\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\ndata: {\"id\":\"paid-stream\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n";
        assert!(parse_sse(bytes).is_err());
        let retained = parse_sse_receipt(bytes, false).unwrap();
        let (id, usage) = financial_receipt(&retained).unwrap();
        assert_eq!(id, "paid-stream");
        assert_eq!(usage.output_tokens, 2);
        assert!(validate_completion(&retained).is_err());
    }
    #[test]
    fn upstream_usage_overruns_remain_receipts_for_actual_charge_and_stop() {
        let value = json!({"id":"overrun","usage":{"prompt_tokens":4,"completion_tokens":100},"choices":[{"message":{"role":"assistant","content":"bounded response"},"finish_reason":"stop"}]});
        assert_eq!(
            validate_receipt(&value, 1).unwrap().1.categories["output_tokens"],
            100
        );
    }
    #[test]
    fn echoed_broker_credentials_are_redacted_from_nested_completion_and_tool_arguments() {
        let mut value = json!({"choices":[{"message":{"content":"echo fixture-secret","tool_calls":[{"function":{"arguments":"{\"secret\":\"fixture-secret\"}"}}]}}],"usage":{"prompt_tokens":3,"completion_tokens":1},"fixture-secret":"nested fixture-secret"});
        redact_secret(&mut value, "fixture-secret");
        assert!(!value.to_string().contains("fixture-secret"));
        assert_eq!(value["usage"]["prompt_tokens"], 3);
        assert!(value.to_string().contains("[REDACTED]"));
    }
    #[test]
    fn retry_after_accepts_bounded_seconds_and_http_dates_without_automatic_retry() {
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        assert_eq!(retry_after("2", now), Some(2000));
        assert_eq!(
            retry_after(
                &httpdate::fmt_http_date(now + std::time::Duration::from_secs(3)),
                now
            ),
            Some(3000)
        );
        for value in ["0", "86401", "-1", "garbage"] {
            assert_eq!(retry_after(value, now), None);
        }
    }
    #[test]
    fn stream_usage_and_multichunk_tool_calls() {
        let frames = [
            json!({"id":"r1","model":"m","choices":[{"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"read","arguments":"{\"a\":"}}]},"finish_reason":null}]}),
            json!({"id":"r1","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":4}}),
        ];
        let text = frames
            .iter()
            .map(|f| format!("data: {f}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n";
        let result = parse_sse(text.as_bytes()).unwrap();
        assert_eq!(
            result["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
        assert_eq!(validate_receipt(&result, 10).unwrap().1.output_tokens, 4);
        assert!(parse_sse(text.replace("data: [DONE]", "").as_bytes()).is_err());
    }
    #[test]
    fn missing_usage_and_mixed_receipts_keep_outcome_unknown() {
        assert!(
            validate_receipt(
                &json!({"id":"r","choices":[{"message":{},"finish_reason":"stop"}]}),
                100
            )
            .is_err()
        );
        let stream = "data: {\"id\":\"a\",\"choices\":[]}\n\ndata: {\"id\":\"b\",\"choices\":[]}\n\ndata: [DONE]\n";
        assert!(parse_sse(stream.as_bytes()).is_err());
    }
    #[test]
    fn hosted_plaintext_and_local_credentials_are_rejected() {
        let features = Features {
            provider: "p".into(),
            model: "m".into(),
            context_tokens: 4096,
            output_tokens: 1024,
            modalities: ["text".into()].into(),
            tools: true,
            streaming: true,
            local: false,
            billing_scope: "account".into(),
            idempotent_upstream: false,
            outcome_lookup: false,
            revision: "1".into(),
        };
        assert!(
            OpenAiProvider::new(
                features.clone(),
                "http://example.com/chat/completions".into(),
                Some("key".into()),
                1,
                10000,
                10000
            )
            .is_err()
        );
        let mut local = features;
        local.local = true;
        assert!(
            OpenAiProvider::new(
                local.clone(),
                "ftp://127.0.0.1/chat/completions".into(),
                None,
                1,
                10000,
                10000
            )
            .is_err()
        );
        assert!(
            OpenAiProvider::new(
                local.clone(),
                "http://127.0.0.1:1/chat/completions".into(),
                Some("key".into()),
                1,
                10000,
                10000
            )
            .is_err()
        );
        assert!(
            OpenAiProvider::new(
                local,
                "http://127.0.0.1:1/chat/completions".into(),
                None,
                1,
                10000,
                10000
            )
            .is_ok()
        );
    }
}
