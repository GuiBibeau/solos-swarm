//! The admitted upstreams, their one transport, and the model request
//! contract a broker enforces before it forwards a call.

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

pub type Dispatch = Pin<Box<dyn Future<Output = Result<Value>> + Send>>;
pub trait Provider: Send + Sync {
    fn dispatch(&self, body: Value) -> Dispatch;
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
                    bytes.len() + chunk.len() <= 262_144,
                    "provider response exceeds limit"
                );
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes).context("invalid provider response")
        })
    }
}
/// Models whose `reasoning_effort` field is meaningful on an OpenAI-compatible
/// gateway. Kept beside the catalog admission so adding a model to one without
/// the other is visible.
pub fn reasoning_model(model: &str) -> bool {
    model.starts_with("deepseek-v4-1-flash") || model.starts_with("zai-org-glm-5")
}

/// The admitted endpoint of each shipped provider profile. A profile's dialect
/// is fixed here rather than read from the Goal, so a Goal cannot make the
/// transport speak a dialect its provider does not.
pub fn endpoint(provider: &str) -> Result<&'static str> {
    match provider {
        "deepseek" => Ok("https://api.deepseek.com/chat/completions"),
        "venice" => Ok("https://api.venice.ai/api/v1/chat/completions"),
        other => anyhow::bail!("provider outside the deployment allowlist: {other}"),
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
        endpoint: endpoint(&upstream.provider)?,
        dialect: upstream.dialect,
        client,
        key,
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
        let other = Upstream {
            provider: "openrouter".into(),
            ..deepseek
        };
        assert!(provider_for(&other, reqwest::Client::new(), key()).is_err());
    }
}
