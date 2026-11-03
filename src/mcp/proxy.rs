//! One principal's tools, through its broker: `GET /tools` lists them and
//! `POST /tool/{name}` calls them, with the principal's bearer token, as the
//! Pi extension does. The broker checks every call against the token's
//! grants, so this proxy adds no authority of its own.

use super::{Outcome, RpcError, Tool, Tools};
use anyhow::{Context, Result};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use std::fmt::Write;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::OnceCell;
use zeroize::Zeroizing;

/// As the Pi extension's: a `run_code` may take its whole 300 s and more.
const CALL_TIMEOUT: Duration = Duration::from_secs(660);
/// The default for how long one call waits out a paused broker.
pub const DEFAULT_PAUSE_WAIT: Duration = Duration::from_secs(45);

pub struct BrokerTools {
    client: reqwest::Client,
    token: Zeroizing<String>,
    pause_wait: Duration,
    /// A token's tools do not change, so they are asked for once.
    listed: OnceCell<Vec<Tool>>,
}

#[derive(Deserialize)]
struct Listing {
    tools: Vec<Listed>,
}

#[derive(Deserialize)]
struct Listed {
    name: String,
    description: String,
    input_schema: Value,
}

impl BrokerTools {
    /// A proxy to the broker on `socket` for the principal `token` names. A
    /// call the broker answers as paused waits and asks again, for at most
    /// `pause_wait` in all.
    pub fn new(socket: &Path, token: Zeroizing<String>, pause_wait: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .unix_socket(socket)
            .timeout(CALL_TIMEOUT)
            .build()
            .context("cannot build the broker client")?;
        Ok(Self {
            client,
            token,
            pause_wait,
            listed: OnceCell::new(),
        })
    }

    async fn fetch_tools(&self) -> Result<Vec<Tool>, RpcError> {
        let response = self
            .client
            .get("http://broker/tools")
            .bearer_auth(self.token.as_str())
            .send()
            .await
            .map_err(|err| RpcError::internal(transport_error(&err)))?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(RpcError::internal(format!(
                "the broker refused the tool list: {}",
                reason(&body, status)
            )));
        }
        let listing: Listing = serde_json::from_value(body).map_err(|err| {
            RpcError::internal(format!("the broker's tool list is unreadable: {err}"))
        })?;
        Ok(listing
            .tools
            .into_iter()
            .map(|tool| Tool {
                name: tool.name,
                description: tool.description,
                input_schema: tool.input_schema,
            })
            .collect())
    }
}

impl Tools for BrokerTools {
    fn instructions(&self) -> &'static str {
        "The tools of one solos-swarm agent session, served by its broker. The broker checks each \
         call against the session's grants and budget; a refused call comes back as an error that \
         says why."
    }

    async fn list(&self) -> Result<Vec<Tool>, RpcError> {
        self.listed
            .get_or_try_init(|| self.fetch_tools())
            .await
            .cloned()
    }

    /// The broker's answer as text. A refusal is an error result with the
    /// broker's reason; a pause is waited out, then reported.
    async fn call(&self, name: &str, arguments: Value) -> Result<Outcome, RpcError> {
        let deadline = Instant::now() + self.pause_wait;
        loop {
            let sent = self
                .client
                .post(format!("http://broker/tool/{name}"))
                .bearer_auth(self.token.as_str())
                .json(&arguments)
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(err) => return Ok(Outcome::error(transport_error(&err))),
            };
            let status = response.status();
            let body: Value = response.json().await.unwrap_or(Value::Null);
            if status.is_success() {
                return Ok(Outcome::ok(body.to_string()));
            }
            if status != StatusCode::SERVICE_UNAVAILABLE || body["paused"] != true {
                return Ok(Outcome::error(reason(&body, status)));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let why = body["reason"].as_str().unwrap_or("operator pause");
                return Ok(Outcome::error(format!(
                    "the broker is paused ({why}), so {name} did nothing; call it again later"
                )));
            }
            let wait = body["retry_after_s"]
                .as_f64()
                .unwrap_or(30.0)
                .clamp(0.1, 300.0);
            tokio::time::sleep(Duration::from_secs_f64(wait).min(left)).await;
        }
    }
}

/// The broker's `error`, or the status when it gave none.
fn reason(body: &Value, status: StatusCode) -> String {
    body["error"]
        .as_str()
        .map_or_else(|| format!("HTTP {status}"), str::to_owned)
}

fn transport_error(err: &reqwest::Error) -> String {
    let mut text = format!("the broker is unreachable: {err}");
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        let _ = write!(text, ": {cause}");
        source = cause.source();
    }
    text
}
