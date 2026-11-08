//! The admitted upstreams, their one transport, and the model request
//! contract a broker enforces before it forwards a call.

use super::forward::WebSearch;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    /// DeepSeek hosted: `thinking` is a parameter on the same chat body.
    DeepSeek,
    /// Any other OpenAI-compatible gateway; reasoning is steered by `reasoning_effort`.
    OpenAi,
}
/// The exact admitted upstream a Goal spends against. The provider is named
/// explicitly because it is what the receipt, ledger and checker attribute.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    pub provider: String,
    pub model: String,
    pub dialect: Dialect,
}
impl Upstream {
    pub fn deepseek(model: &str) -> Self {
        Self {
            provider: "deepseek".into(),
            model: model.into(),
            dialect: Dialect::DeepSeek,
        }
    }
}
/// Model IDs of the admitted Venice catalog. Newer revisions carry a date
/// suffix, so selection is prefix-anchored rather than an exact match.
pub fn venice_model(model: &str) -> bool {
    model.starts_with("deepseek-v4-1-flash-uncensored") || model.starts_with("zai-org-glm-5")
}
/// Resolve a deployment provider id and model into its admitted upstream, so an
/// operator selects a shipped profile by name instead of restating its dialect.
/// An unknown provider fails rather than silently defaulting, because the wrong
/// dialect would send a vendored parameter openai-compatible gateways ignore.
pub fn upstream_profile(provider: &str, model: &str) -> Result<Upstream> {
    ensure!(!model.is_empty(), "provider model is empty");
    Ok(if provider == "deepseek" {
        Upstream::deepseek(model)
    } else if provider == "venice" {
        ensure!(
            venice_model(model),
            "model is outside the admitted Venice catalog"
        );
        Upstream {
            provider: provider.into(),
            model: model.into(),
            dialect: Dialect::OpenAi,
        }
    } else {
        bail!("provider outside the deployment allowlist: {provider}")
    })
}

/// How a harness's Anthropic Messages calls reach the provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    /// Translated onto the provider's chat completions (`anthropic::Chat`).
    #[default]
    Chat,
    /// Forwarded untranslated to the provider's own Messages endpoint
    /// (`forward::Forward`).
    Anthropic,
}

pub type Dispatch = Pin<Box<dyn Future<Output = Result<Value>> + Send>>;
pub trait Provider: Send + Sync {
    fn dispatch(&self, body: Value) -> Dispatch;

    /// The provider's own Messages endpoint, where a harness's Messages
    /// calls go untranslated. `None` translates them onto `dispatch`.
    fn messages(&self) -> Option<&AnthropicMessages> {
        None
    }
}

/// The version header every Messages call carries.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// A provider's own Anthropic Messages endpoint. The broker's key is the
/// only credential sent; nothing of a session's request but the body the
/// broker made from it goes upstream.
#[derive(Clone)]
pub struct AnthropicMessages {
    pub endpoint: String,
    pub client: reqwest::Client,
    pub key: Zeroizing<String>,
    /// The server-side web search the broker adds to every call, or `None`
    /// for no search.
    pub web_search: Option<WebSearch>,
}

impl AnthropicMessages {
    /// Post `body`, and answer the response once its status is a success.
    /// Any other status is an error naming it and the provider's reason.
    pub async fn post(&self, body: &Value) -> Result<reqwest::Response> {
        let response = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", self.key.as_str())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(body)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("provider transport failure; outcome unknown"))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let reason = error_reason(response).await;
        bail!("provider returned HTTP {}: {reason}", status.as_u16())
    }
}

/// The `error.message` of a provider's error body, which both the Messages
/// and the chat error formats carry, cut to a line.
async fn error_reason(mut response: reqwest::Response) -> String {
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > 16_384 {
            break;
        }
    }
    let body: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    match body["error"]["message"].as_str() {
        Some(message) => message.chars().take(300).collect(),
        None => "no reason given".into(),
    }
}

/// A provider on both of its wires: chat completions for every session, and
/// its own Messages endpoint for a harness that speaks Messages.
pub struct BothWires {
    pub chat: OpenAiCompatible,
    pub messages: AnthropicMessages,
}

impl Provider for BothWires {
    fn dispatch(&self, body: Value) -> Dispatch {
        self.chat.dispatch(body)
    }

    fn messages(&self) -> Option<&AnthropicMessages> {
        Some(&self.messages)
    }
}
/// An OpenAI-compatible hosted endpoint. `dialect` is the only difference
/// between the shipped profiles, so one transport serves both and neither
/// duplicates the receipt, bound or error handling.
pub struct OpenAiCompatible {
    pub endpoint: &'static str,
    pub dialect: Dialect,
    pub client: reqwest::Client,
    pub key: Zeroizing<String>,
}
impl Provider for OpenAiCompatible {
    fn dispatch(&self, body: Value) -> Dispatch {
        let endpoint = self.endpoint;
        let dialect = self.dialect;
        let client = self.client.clone();
        let key = self.key.clone();
        Box::pin(async move {
            // `thinking` is DeepSeek's own dialect and Venice rejects it outright
            // ("Invalid request parameters", HTTP 400), so an OpenAI-compatible
            // route REPLACES it rather than augmenting: the field is removed and
            // the intent re-expressed as `reasoning_effort`, because a reasoning
            // model would otherwise leave reasoning at its costlier default. A
            // non-reasoning model rejects `reasoning_effort: "none"`, so that is
            // only sent where the Goal declared reasoning off.
            let mut body = body;
            let limit = response_limit(&body);
            if dialect == Dialect::OpenAi {
                let disabled = body["thinking"]["type"] == json!("disabled");
                if let Some(object) = body.as_object_mut() {
                    object.remove("thinking");
                }
                if disabled && reasoning_model(body["model"].as_str().unwrap_or_default()) {
                    body["reasoning_effort"] = json!("none");
                }
            }
            let mut response = client
                .post(endpoint)
                .bearer_auth(key.as_str())
                .json(&body)
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("provider transport failure; outcome unknown"))?;
            ensure!(
                response.status().is_success(),
                "provider returned HTTP {}; outcome requires reconciliation",
                response.status().as_u16()
            );
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| anyhow::anyhow!("provider response interrupted"))?
            {
                ensure!(
                    bytes.len() + chunk.len() <= limit,
                    "provider response exceeds limit"
                );
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes).context("invalid provider response")
        })
    }
}
/// The most a provider's answer may be. A call with thinking on also brings
/// back its reasoning (`reasoning_content`), up to its whole output ceiling,
/// so it may be larger; every call with thinking off, as swarm mode and agent
/// sessions send, keeps the 256 KiB bound.
fn response_limit(body: &Value) -> usize {
    if body["thinking"]["type"] == json!("enabled") {
        4 * 1024 * 1024
    } else {
        262_144
    }
}

/// Models whose `reasoning_effort` field is meaningful on an OpenAI-compatible
/// gateway. Kept beside the catalog admission so adding a model to one without
/// the other is visible.
pub fn reasoning_model(model: &str) -> bool {
    model.starts_with("deepseek-v4-1-flash") || model.starts_with("zai-org-glm-5")
}

/// The admitted endpoint of each shipped provider profile on each wire. A
/// profile's dialect is fixed here rather than read from the Goal, so a Goal
/// cannot make the transport speak a dialect its provider does not.
pub fn endpoint(provider: &str, wire: Wire) -> Result<&'static str> {
    match (provider, wire) {
        ("deepseek", Wire::Chat) => Ok("https://api.deepseek.com/chat/completions"),
        ("deepseek", Wire::Anthropic) => Ok("https://api.deepseek.com/anthropic/v1/messages"),
        ("venice", Wire::Chat) => Ok("https://api.venice.ai/api/v1/chat/completions"),
        ("venice", Wire::Anthropic) => bail!("provider venice has no Anthropic wire"),
        (other, _) => bail!("provider outside the deployment allowlist: {other}"),
    }
}
/// Build the transport for a resolved upstream, so both shipped profiles share
/// one receipt, bound and error path.
pub fn provider_for(
    upstream: &Upstream,
    client: reqwest::Client,
    key: Zeroizing<String>,
) -> Result<OpenAiCompatible> {
    Ok(OpenAiCompatible {
        endpoint: endpoint(&upstream.provider, Wire::Chat)?,
        dialect: upstream.dialect,
        client,
        key,
    })
}

/// The Messages endpoint of a resolved upstream's provider.
pub fn messages_for(
    upstream: &Upstream,
    client: reqwest::Client,
    key: Zeroizing<String>,
    web_search: Option<WebSearch>,
) -> Result<AnthropicMessages> {
    Ok(AnthropicMessages {
        endpoint: endpoint(&upstream.provider, Wire::Anthropic)?.to_owned(),
        client,
        key,
        web_search,
    })
}

/// Refuse a session's model request unless it names `model`, carries at
/// least one message and offers only tools `granted` admits.
pub fn admit_request(body: &Value, model: &str, granted: impl Fn(&str) -> bool) -> Result<()> {
    ensure!(
        body["model"] == model && body["messages"].as_array().is_some_and(|m| !m.is_empty()),
        "model or message shape outside contract"
    );
    if let Some(tools) = body.get("tools") {
        let tools = tools.as_array().context("invalid tool schema")?;
        for tool in tools {
            let name = tool["function"]["name"]
                .as_str()
                .context("invalid tool name")?;
            ensure!(granted(name), "unadmitted provider tool");
        }
    }
    Ok(())
}

/// The body sent upstream for an admitted request: only its messages and
/// tools survive, and the broker sets the wire model, the output ceiling,
/// reasoning off, temperature 0 and no streaming.
pub fn upstream_body(model: &str, request: &Value, max_output_tokens: u64) -> Value {
    json!({"model":model,"messages":request["messages"],"tools":request.get("tools").cloned().unwrap_or(json!([])),"max_tokens":max_output_tokens,"thinking":{"type":"disabled"},"temperature":0,"stream":false})
}

/// The body sent upstream for an admitted request of a session that thinks,
/// which only agent mode's dream is: its messages and tools, DeepSeek's
/// thinking on at `effort` (`reasoning_effort`), the output ceiling, and no
/// streaming. No temperature: DeepSeek documents that thinking mode ignores
/// it. With tools, DeepSeek wants every earlier answer's `reasoning_content`
/// back on every later request, or it answers 400. Pi keeps and sends it; an
/// assistant message that came without one is sent with an empty one, as Pi
/// itself does for DeepSeek, rather than refused upstream. `upstream_body`,
/// which swarm mode and every other session send, is untouched.
pub fn thinking_body(model: &str, request: &Value, max_output_tokens: u64, effort: &str) -> Value {
    let mut messages = request["messages"].clone();
    if let Some(messages) = messages.as_array_mut() {
        for message in messages {
            if message["role"] == "assistant"
                && !message["reasoning_content"].is_string()
                && let Some(fields) = message.as_object_mut()
            {
                fields.insert("reasoning_content".into(), json!(""));
            }
        }
    }
    json!({"model":model,"messages":messages,"tools":request.get("tools").cloned().unwrap_or(json!([])),"max_tokens":max_output_tokens,"thinking":{"type":"enabled"},"reasoning_effort":effort,"stream":false})
}

/// A sampling temperature, 0 to 2 as DeepSeek's chat completions take it.
/// Always finite, so equality is total.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(try_from = "f64")]
pub struct Temperature(f64);

impl Eq for Temperature {}

impl TryFrom<f64> for Temperature {
    type Error = String;

    fn try_from(value: f64) -> std::result::Result<Self, String> {
        if value.is_finite() && (0.0..=2.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(format!("a temperature is 0 to 2, not {value}"))
        }
    }
}

impl Temperature {
    /// What agent mode's workers sample at unless `executor.temperature`
    /// says otherwise.
    pub const WORKER: Self = Self(0.6);

    pub fn get(self) -> f64 {
        self.0
    }
}

/// `body` (an [`upstream_body`]) sampled at `temperature` instead of 0. Only
/// agent mode's workers call this: `upstream_body` stays what swarm mode
/// pins byte for byte, and the Router judges at 0.
pub fn sampled(mut body: Value, temperature: Temperature) -> Value {
    body["temperature"] = json!(temperature.get());
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `/model` handler's rewrite as it was before the move.
    fn before(goal_model: &str, max_output_tokens: u64, body: &Value) -> Value {
        json!({"model":goal_model,"messages":body["messages"],"tools":body.get("tools").cloned().unwrap_or(json!([])),"max_tokens":max_output_tokens,"thinking":{"type":"disabled"},"temperature":0,"stream":false})
    }

    #[test]
    fn the_upstream_body_is_byte_identical_to_the_swarm_rewrite_it_replaced() {
        let bodies = [
            json!({"model":"deepseek-flash","messages":[{"role":"user","content":"test"}],"max_tokens":4096}),
            json!({"model":"deepseek-flash","messages":[{"role":"system","content":"s"},{"role":"user","content":"Ω\u{2028}"}],
                   "tools":[{"type":"function","function":{"name":"claim_task","parameters":{"type":"object"}}}],
                   "stream":true,"temperature":0.7,"thinking":{"type":"enabled"},"reasoning_effort":"high","extra":[1,2]}),
            json!({"model":"x","messages":[],"tools":null}),
            json!({"messages":{"not":"a list"}}),
            json!({}),
        ];
        for model in ["deepseek-flash", "deepseek-v4-1-flash-uncensored"] {
            for max in [1, 2048, 393_216] {
                for body in &bodies {
                    let new = serde_json::to_vec(&upstream_body(model, body, max)).unwrap();
                    let old = serde_json::to_vec(&before(model, max, body)).unwrap();
                    assert_eq!(new, old, "{body}");
                }
            }
        }
    }

    #[test]
    fn a_thinking_body_keeps_every_reasoning_and_differs_from_the_shared_one_only_in_thinking() {
        let request = json!({"model":"deepseek-flash","temperature":0.7,"stream":true,
            "messages":[
                {"role":"system","content":"s"},
                {"role":"user","content":"Dream."},
                {"role":"assistant","content":null,"reasoning_content":"R1",
                 "tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"c1","content":"ok"},
                {"role":"assistant","content":"done"},
            ],
            "tools":[{"type":"function","function":{"name":"read"}}]});
        let body = thinking_body("deepseek-flash", &request, 131_072, "max");
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert_eq!(body["reasoning_effort"], "max");
        assert_eq!(body["max_tokens"], 131_072);
        assert_eq!(body["stream"], false);
        assert!(
            body.get("temperature").is_none(),
            "thinking mode ignores it"
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages[2]["reasoning_content"], "R1",
            "passed back as it came"
        );
        assert_eq!(messages[4]["reasoning_content"], "", "never missing");
        assert!(messages[0].get("reasoning_content").is_none());
        assert!(messages[3].get("reasoning_content").is_none());
        assert_eq!(body["tools"], request["tools"]);
        assert_eq!(response_limit(&body), 4 * 1024 * 1024);
        // The same request on the shared body: thinking off, as before.
        let shared = upstream_body("deepseek-flash", &request, 131_072);
        assert_eq!(shared["thinking"], json!({"type": "disabled"}));
        assert_eq!(shared["messages"], request["messages"]);
    }

    #[test]
    fn only_a_call_with_thinking_on_may_bring_back_more_than_256_kib() {
        let shared = upstream_body("deepseek-flash", &json!({"messages": []}), 8_192);
        assert_eq!(response_limit(&shared), 262_144, "swarm mode and sessions");
        let thinking = json!({"model": "deepseek-flash", "thinking": {"type": "enabled"}});
        assert_eq!(response_limit(&thinking), 4 * 1024 * 1024);
    }

    #[test]
    fn a_sampled_body_differs_from_the_shared_one_only_in_its_temperature() {
        let request = json!({"messages":[{"role":"user","content":"x"}],
                             "tools":[{"type":"function","function":{"name":"claim_task"}}]});
        let shared = upstream_body("deepseek-flash", &request, 2_048);
        assert_eq!(shared["temperature"], json!(0), "the shared body is greedy");
        let warm = sampled(shared.clone(), Temperature::WORKER);
        assert_eq!(warm["temperature"], json!(0.6));
        let mut cooled = warm;
        cooled["temperature"] = json!(0);
        assert_eq!(
            serde_json::to_vec(&cooled).unwrap(),
            serde_json::to_vec(&shared).unwrap(),
            "nothing else moved"
        );
    }

    #[test]
    fn a_temperature_is_a_finite_number_from_zero_to_two() {
        for fine in [0.0, 0.6, 1.0, 2.0] {
            assert_eq!(Temperature::try_from(fine).unwrap().get(), fine);
        }
        for bad in [-0.1, 2.1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(Temperature::try_from(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_request_outside_the_contract_is_refused_with_the_swarm_messages() {
        let granted = |name: &str| name == "claim_task";
        let ok = json!({"model":"m","messages":[{"role":"user","content":"x"}],
                        "tools":[{"type":"function","function":{"name":"claim_task"}}]});
        admit_request(&ok, "m", granted).unwrap();
        let refused = |body: Value| admit_request(&body, "m", granted).unwrap_err().to_string();
        let shape = "model or message shape outside contract";
        assert_eq!(refused(json!({"model":"n","messages":[{}]})), shape);
        assert_eq!(refused(json!({"model":"m","messages":[]})), shape);
        assert_eq!(refused(json!({"model":"m","messages":{}})), shape);
        assert_eq!(refused(json!({"messages":[{}]})), shape);
        assert_eq!(
            refused(json!({"model":"m","messages":[{}],"tools":{}})),
            "invalid tool schema"
        );
        assert_eq!(
            refused(json!({"model":"m","messages":[{}],"tools":[{"function":{}}]})),
            "invalid tool name"
        );
        assert_eq!(
            refused(json!({"model":"m","messages":[{}],"tools":[{"function":{"name":"bash"}}]})),
            "unadmitted provider tool"
        );
    }

    #[test]
    fn each_shipped_profile_has_one_endpoint_and_others_have_none() {
        let key = || Zeroizing::new(String::new());
        let venice = upstream_profile("venice", "zai-org-glm-5").unwrap();
        let built = provider_for(&venice, reqwest::Client::new(), key()).unwrap();
        assert_eq!(
            (built.endpoint, built.dialect),
            (
                "https://api.venice.ai/api/v1/chat/completions",
                Dialect::OpenAi
            )
        );
        let deepseek = Upstream::deepseek("deepseek-flash");
        let built = provider_for(&deepseek, reqwest::Client::new(), key()).unwrap();
        assert_eq!(
            (built.endpoint, built.dialect),
            (
                "https://api.deepseek.com/chat/completions",
                Dialect::DeepSeek
            )
        );
        let messages = messages_for(&deepseek, reqwest::Client::new(), key(), None).unwrap();
        assert_eq!(
            messages.endpoint,
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        assert!(messages_for(&venice, reqwest::Client::new(), key(), None).is_err());
        let other = Upstream {
            provider: "openrouter".into(),
            ..deepseek
        };
        assert!(provider_for(&other, reqwest::Client::new(), key()).is_err());
        assert!(messages_for(&other, reqwest::Client::new(), key(), None).is_err());
    }
}
