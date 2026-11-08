//! A Messages request forwarded untranslated to a provider's own Messages
//! endpoint, such as DeepSeek's: the body the broker makes of it, the web
//! search the broker may add and what that may cost, and the receipt read
//! from the reply or, as it streams by, from its events.

use super::pricing::{PriceTable, REQUEST_FRAMING_BYTES, Receipt, Usage};
use super::upstream::Temperature;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// The suffix Claude Code gives a model it may use with a 1M-token context.
/// It strips it before a call, but a call that keeps it names the same model.
pub const LONG_CONTEXT: &str = "[1m]";

/// The request fields sent upstream as the session wrote them. The broker
/// sets `model`, `max_tokens`, `stream` and `temperature`, and drops every
/// other field, such as `metadata`, `mcp_servers` or `container`.
const FORWARDED: [&str; 7] = [
    "messages",
    "system",
    "tools",
    "tool_choice",
    "stop_sequences",
    "thinking",
    "output_config",
];

/// A Messages request as the broker reads it before it forwards it.
#[derive(Clone, Debug, PartialEq)]
pub struct Forward {
    body: Map<String, Value>,
    /// The request's own output ceiling.
    pub max_tokens: u64,
    /// Answer with server-sent events.
    pub stream: bool,
}

impl Forward {
    /// Read a Messages request. A body that is no request is refused with why.
    pub fn read(body: &Value) -> Result<Self, String> {
        let fields = body
            .as_object()
            .ok_or("a Messages request is a JSON object")?;
        let max_tokens = match fields.get("max_tokens") {
            None => return Err("missing field `max_tokens`".into()),
            Some(max) => max.as_u64().filter(|max| *max > 0),
        }
        .ok_or("max_tokens must be at least 1")?;
        let stream = match fields.get("stream") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(stream)) => *stream,
            Some(_) => return Err("stream is true or false".into()),
        };
        Ok(Self {
            body: fields.clone(),
            max_tokens,
            stream,
        })
    }

    /// Refuse the request unless it names `model` and carries at least one
    /// message, and every tool it offers is a client tool `granted` admits.
    /// A server tool is refused whatever its name: the only one a call may
    /// carry is the web search the broker adds itself.
    pub fn admit(&self, model: &str, granted: impl Fn(&str) -> bool) -> Result<()> {
        let named = self.body.get("model").and_then(Value::as_str);
        let named = named.map(|name| name.strip_suffix(LONG_CONTEXT).unwrap_or(name));
        let messages = self.body.get("messages").and_then(Value::as_array);
        ensure!(
            named == Some(model) && messages.is_some_and(|messages| !messages.is_empty()),
            "model or message shape outside contract"
        );
        if let Some(tools) = self.body.get("tools") {
            for tool in tools.as_array().context("invalid tool schema")? {
                let name = tool["name"].as_str().context("invalid tool name")?;
                let client = matches!(tool.get("type"), None | Some(Value::Null))
                    || tool["type"] == "custom";
                ensure!(client && granted(name), "unadmitted provider tool");
            }
        }
        Ok(())
    }

    /// The body sent upstream: the forwarded fields, on `model`, under the
    /// lower of the request's ceiling and `max_output_tokens`, with
    /// `web_search`'s tool added. `temperature` applies where the request
    /// does not think, since a thinking call fixes its own.
    pub fn upstream(
        &self,
        model: &str,
        max_output_tokens: u64,
        temperature: Option<Temperature>,
        web_search: Option<&WebSearch>,
    ) -> Value {
        let mut body = Map::new();
        body.insert("model".into(), json!(model));
        body.insert(
            "max_tokens".into(),
            json!(self.max_tokens.min(max_output_tokens)),
        );
        for field in FORWARDED {
            if let Some(value) = self.body.get(field) {
                body.insert(field.into(), value.clone());
            }
        }
        if let Some(search) = web_search {
            let tools = body.entry("tools").or_insert_with(|| json!([]));
            if let Some(tools) = tools.as_array_mut() {
                tools.push(search.tool());
            }
        }
        body.insert("stream".into(), json!(self.stream));
        let thinks = matches!(
            body.get("thinking")
                .and_then(|thinking| thinking["type"].as_str()),
            Some("enabled" | "adaptive")
        );
        if let Some(temperature) = temperature.filter(|_| !thinks) {
            body.insert("temperature".into(), json!(temperature.get()));
        }
        Value::Object(body)
    }
}

/// The web search server tool the broker adds, in the version DeepSeek
/// documents.
pub const SEARCH_TOOL: &str = "web_search_20250305";

/// The provider's own web search, which the broker adds to every forwarded
/// call as a server tool when the operator turns it on. Each search is
/// billed apart from the tokens, and its results come back as input that
/// the provider reads in a further round.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearch {
    /// The most searches one call may run.
    #[serde(default = "default_max_uses")]
    pub max_uses: u64,
    /// What one search costs.
    #[serde(default = "default_search_price")]
    pub microusd_per_search: u64,
    /// The most input tokens one search's results may add, for the hold.
    #[serde(default = "default_result_tokens")]
    pub result_tokens: u64,
}

fn default_max_uses() -> u64 {
    1
}

/// DeepSeek's price: $0.75 per 1,000 searches that return results.
fn default_search_price() -> u64 {
    750
}

/// One search measured on DeepSeek added about 56,000 input tokens.
fn default_result_tokens() -> u64 {
    131_072
}

impl WebSearch {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=8).contains(&self.max_uses),
            "web_search.max_uses is 1 to 8"
        );
        ensure!(
            (default_search_price()..=1_000_000).contains(&self.microusd_per_search),
            "web_search.microusd_per_search is 750 to 1000000"
        );
        ensure!(
            (1..=1_000_000).contains(&self.result_tokens),
            "web_search.result_tokens is 1 to 1000000"
        );
        Ok(())
    }

    /// The server tool the broker adds to a call.
    pub fn tool(&self) -> Value {
        json!({"type": SEARCH_TOOL, "name": "web_search", "max_uses": self.max_uses})
    }

    /// The hold of a call of `input_bytes` that may answer
    /// `max_output_tokens` and search. Each search may run a further round
    /// that reads the request and every result so far again, so the input
    /// held is the request once per round and each result once per round
    /// after it; every search is held at its price.
    pub fn hold(&self, prices: &PriceTable, input_bytes: usize, max_output_tokens: u64) -> u64 {
        let uses = self.max_uses;
        let request = input_bytes as u64 + REQUEST_FRAMING_BYTES;
        let input = (uses + 1) * request + self.result_tokens * uses * (uses + 1) / 2;
        prices.hold(input, max_output_tokens) + self.cost(uses)
    }

    /// What `searches` cost.
    pub fn cost(&self, searches: u64) -> u64 {
        searches * self.microusd_per_search
    }
}

/// The receipt of a Message's `id` and `usage`, and the web searches it
/// bills. Its input is every input token: the cache reads among them are
/// priced at the cache-hit rate and the cache writes at the input rate,
/// since the price table has no rate of its own for them. The searches are
/// the ones that returned results, which are the ones billed.
pub fn receipt(id: &Value, usage: &Value, max_output_tokens: u64) -> Result<(Receipt, u64)> {
    let count = |value: Option<&Value>, what: &str| -> Result<u64> {
        match value {
            None | Some(Value::Null) => Ok(0),
            Some(value) => value.as_u64().with_context(|| format!("invalid {what}")),
        }
    };
    let input = usage["input_tokens"]
        .as_u64()
        .context("input usage missing")?;
    let output = usage["output_tokens"]
        .as_u64()
        .context("output usage missing")?;
    let read = count(usage.get("cache_read_input_tokens"), "cache read usage")?;
    let written = count(
        usage.get("cache_creation_input_tokens"),
        "cache write usage",
    )?;
    let searches = count(
        usage
            .get("server_tool_use")
            .and_then(|tools| tools.get("tool_search")),
        "web search usage",
    )?;
    let total = input
        .checked_add(read)
        .and_then(|total| total.checked_add(written))
        .context("invalid input usage")?;
    let usage = Usage {
        input_tokens: total,
        output_tokens: output,
        cached_input_tokens: read,
        ..Usage::default()
    };
    Ok((Receipt::within(usage, id, max_output_tokens)?, searches))
}

/// The most one event of a stream may hold before the stream is read as
/// having no usage. One search's results arrive in one event of a few
/// hundred KiB.
const EVENT_LIMIT: usize = 8 * 1024 * 1024;

/// Reads a Messages stream as it passes through, for the message's id and
/// its final usage. Only the events that carry neither are skipped unread.
#[derive(Debug, Default)]
pub struct Tally {
    line: Vec<u8>,
    /// The rest of the line is a data line of an event that is skipped.
    skipping: bool,
    event: Option<String>,
    data: String,
    id: Value,
    usage: Map<String, Value>,
    /// A `message_delta` came, whose usage is the call's final usage.
    ended: bool,
    /// An error came, or an event could not be read.
    failed: bool,
}

impl Tally {
    /// Read the next bytes of the stream, which may end anywhere.
    pub fn feed(&mut self, mut bytes: &[u8]) {
        while !self.failed && !bytes.is_empty() {
            let end = bytes.iter().position(|byte| *byte == b'\n');
            let piece = &bytes[..end.unwrap_or(bytes.len())];
            bytes = &bytes[end.map_or(bytes.len(), |at| at + 1)..];
            if !self.skipping {
                self.line.extend_from_slice(piece);
                if !self.bears_on_usage() && self.line.starts_with(b"data") {
                    self.skipping = true;
                    self.line.clear();
                }
            }
            if end.is_none() {
                self.failed = self.line.len() > EVENT_LIMIT;
            } else if self.skipping {
                self.skipping = false;
            } else {
                let line = std::mem::take(&mut self.line);
                self.read_line(&line);
            }
        }
    }

    fn read_line(&mut self, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return self.close_event();
        }
        let Ok(line) = std::str::from_utf8(line) else {
            self.failed = true;
            return;
        };
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" if self.bears_on_usage() => {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.failed = self.data.len() > EVENT_LIMIT;
            }
            _ => {}
        }
    }

    /// Whether the event being read may carry the id, the usage or an
    /// error: every event but the content's and the pings.
    fn bears_on_usage(&self) -> bool {
        !matches!(
            self.event.as_deref(),
            Some("content_block_start" | "content_block_delta" | "content_block_stop" | "ping")
        )
    }

    fn close_event(&mut self) {
        self.event = None;
        let data = std::mem::take(&mut self.data);
        if data.is_empty() {
            return;
        }
        let Ok(data) = serde_json::from_str::<Value>(&data) else {
            self.failed = true;
            return;
        };
        match data["type"].as_str() {
            Some("message_start") => {
                self.id = data["message"]["id"].clone();
                if let Some(usage) = data["message"]["usage"].as_object() {
                    self.usage = usage.clone();
                }
            }
            // Each field it has replaces the start's, as Anthropic's clients
            // read it; DeepSeek's carries every field.
            Some("message_delta") => {
                if let Some(usage) = data["usage"].as_object() {
                    for (field, value) in usage.iter().filter(|(_, value)| !value.is_null()) {
                        self.usage.insert(field.clone(), value.clone());
                    }
                }
                self.ended = true;
            }
            Some("error") => self.failed = true,
            // DeepSeek's frame for a call that timed out before it began.
            None if data.get("error").is_some() => self.failed = true,
            _ => {}
        }
    }

    /// The message's id and final usage, or `None` when the stream failed
    /// or ended before its usage came.
    pub fn finish(self) -> Option<(Value, Value)> {
        (self.ended && !self.failed).then_some((self.id, Value::Object(self.usage)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code 2.1.231's first worker turn, as recorded.
    const FIRST_TURN: &str =
        include_str!("../../tests/fixtures/anthropic/claude-code-first-turn.json");

    fn first_turn() -> Value {
        let mut body: Value = serde_json::from_str(FIRST_TURN).unwrap();
        body["model"] = json!("deepseek-flash");
        body
    }

    fn worker(name: &str) -> bool {
        name == "mcp__swarm__worker_report"
    }

    const SEARCH: WebSearch = WebSearch {
        max_uses: 2,
        microusd_per_search: 750,
        result_tokens: 1_000,
    };

    #[test]
    fn the_upstream_body_is_the_sessions_own_under_the_brokers_terms() {
        let asked = first_turn();
        let forward = Forward::read(&asked).unwrap();
        assert_eq!((forward.max_tokens, forward.stream), (32_000, true));
        forward.admit("deepseek-flash", worker).unwrap();
        let sent = forward.upstream(
            "deepseek-flash",
            2_048,
            Temperature::try_from(0.6).ok(),
            None,
        );
        let mut expected = json!({"model": "deepseek-flash", "max_tokens": 2_048, "stream": true});
        for field in ["messages", "system", "tools", "thinking", "output_config"] {
            expected[field] = asked[field].clone();
        }
        assert_eq!(
            sent, expected,
            "metadata and context_management are dropped, and a thinking call carries no temperature"
        );

        let mut plain = asked.clone();
        plain["thinking"] = json!({"type": "disabled"});
        plain["max_tokens"] = json!(64);
        plain["stop_sequences"] = json!(["END"]);
        plain["tool_choice"] = json!({"type": "auto"});
        for field in [
            "mcp_servers",
            "container",
            "metadata",
            "top_k",
            "service_tier",
        ] {
            plain[field] = json!([{"url": "https://example.com"}]);
        }
        let sent = Forward::read(&plain).unwrap().upstream(
            "deepseek-flash",
            2_048,
            Temperature::try_from(0.6).ok(),
            None,
        );
        assert_eq!(
            (
                &sent["max_tokens"],
                &sent["temperature"],
                &sent["stop_sequences"],
                &sent["tool_choice"]
            ),
            (
                &json!(64),
                &json!(0.6),
                &json!(["END"]),
                &json!({"type": "auto"})
            )
        );
        let mut fields: Vec<&str> = sent
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "max_tokens",
                "messages",
                "model",
                "output_config",
                "stop_sequences",
                "stream",
                "system",
                "temperature",
                "thinking",
                "tool_choice",
                "tools"
            ]
        );
    }

    #[test]
    fn web_search_is_a_tool_the_broker_adds_and_a_session_cannot_name() {
        let forward = Forward::read(&first_turn()).unwrap();
        let sent = forward.upstream("deepseek-flash", 2_048, None, Some(&SEARCH));
        let tools = sent["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "mcp__swarm__worker_report");
        assert_eq!(
            tools[1],
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 2})
        );
        let mut none = first_turn();
        none.as_object_mut().unwrap().remove("tools");
        let sent =
            Forward::read(&none)
                .unwrap()
                .upstream("deepseek-flash", 2_048, None, Some(&SEARCH));
        assert_eq!(sent["tools"], json!([SEARCH.tool()]));

        let mut asks = first_turn();
        asks["tools"] =
            json!([{"type": "web_search_20250305", "name": "web_search", "max_uses": 8}]);
        let refused = Forward::read(&asks)
            .unwrap()
            .admit("deepseek-flash", |_| true)
            .unwrap_err();
        assert_eq!(refused.to_string(), "unadmitted provider tool");
    }

    #[test]
    fn only_the_contracts_model_tools_and_shape_are_admitted() {
        let admitted = |edit: &dyn Fn(&mut Value)| {
            let mut body = first_turn();
            edit(&mut body);
            Forward::read(&body)
                .unwrap()
                .admit("deepseek-flash", worker)
                .map_err(|err| err.to_string())
        };
        assert_eq!(admitted(&|_| {}), Ok(()));
        assert_eq!(
            admitted(&|b| b["model"] = json!("deepseek-flash[1m]")),
            Ok(()),
            "the long-context alias names the same model"
        );
        assert_eq!(
            admitted(&|b| b["tools"][0]["type"] = json!("custom")),
            Ok(())
        );
        let shape = "model or message shape outside contract";
        type Edit<'a> = &'a dyn Fn(&mut Value);
        let cases: [(&str, Edit); 8] = [
            (shape, &|b| b["model"] = json!("claude-opus-5")),
            (shape, &|b| b["model"] = json!("deepseek-v4-pro[1m]")),
            (shape, &|b| b["messages"] = json!([])),
            (shape, &|b| {
                b.as_object_mut().unwrap().remove("model");
            }),
            ("unadmitted provider tool", &|b| {
                b["tools"][0]["name"] = json!("Bash")
            }),
            ("unadmitted provider tool", &|b| {
                b["tools"][0]["type"] = json!("bash_20250124")
            }),
            ("invalid tool schema", &|b| b["tools"] = json!({})),
            ("invalid tool name", &|b| b["tools"] = json!([{}])),
        ];
        for (why, edit) in cases {
            assert_eq!(admitted(edit), Err(why.to_owned()));
        }
        let read = |body: Value| Forward::read(&body).unwrap_err();
        assert_eq!(read(json!([])), "a Messages request is a JSON object");
        assert_eq!(
            read(json!({"max_tokens": 0})),
            "max_tokens must be at least 1"
        );
        assert_eq!(
            read(json!({"max_tokens": "1"})),
            "max_tokens must be at least 1"
        );
        assert_eq!(read(json!({})), "missing field `max_tokens`");
        assert_eq!(
            read(json!({"max_tokens": 1, "stream": "yes"})),
            "stream is true or false"
        );
    }

    #[test]
    fn a_search_call_holds_every_round_and_every_search() {
        let prices = PriceTable {
            input_microusd_per_million: 300_000,
            output_microusd_per_million: 1_200_000,
            cache_hit_microusd_per_million: Some(6_000),
        };
        let one = WebSearch {
            max_uses: 1,
            ..SEARCH
        };
        // Two rounds of the request and one of the result, and the search.
        let request = 10_000 + REQUEST_FRAMING_BYTES;
        assert_eq!(
            one.hold(&prices, 10_000, 2_048),
            prices.hold(2 * request + 1_000, 2_048) + 750
        );
        // Three rounds; the first result is read twice, the second once.
        assert_eq!(
            SEARCH.hold(&prices, 10_000, 2_048),
            prices.hold(3 * request + 3 * 1_000, 2_048) + 1_500
        );
        assert!(SEARCH.hold(&prices, 10_000, 2_048) > prices.request_hold(10_000, 2_048));
        assert_eq!(SEARCH.cost(3), 2_250);

        let read = |yaml: &str| serde_yaml::from_str::<WebSearch>(yaml);
        let defaults = read("{}").unwrap();
        assert_eq!(
            defaults,
            WebSearch {
                max_uses: 1,
                microusd_per_search: 750,
                result_tokens: 131_072
            }
        );
        defaults.validate().unwrap();
        for bad in [
            "{max_uses: 0}",
            "{max_uses: 9}",
            "{microusd_per_search: 749}",
            "{result_tokens: 0}",
        ] {
            assert!(read(bad).unwrap().validate().is_err(), "{bad}");
        }
        assert!(read("{max_use: 1}").is_err(), "an unknown field");
    }

    #[test]
    fn a_receipt_counts_cache_reads_and_writes_as_input_and_bills_found_searches() {
        let usage = json!({"input_tokens": 206, "cache_creation_input_tokens": 4,
            "cache_read_input_tokens": 256, "output_tokens": 40, "service_tier": "standard",
            "server_tool_use": {"web_search_requests": 3, "tool_search": 2}});
        let (read, searches) = receipt(&json!("msg_1"), &usage, 2_048).unwrap();
        assert_eq!(
            read,
            Receipt {
                input_tokens: 466,
                cached_input_tokens: 256,
                output_tokens: 40,
                request_id: "msg_1".into(),
            }
        );
        assert_eq!(searches, 2, "an attempt that found nothing is not billed");
        let plain = json!({"input_tokens": 12, "output_tokens": 1});
        assert_eq!(receipt(&json!("m"), &plain, 2_048).unwrap().1, 0);

        let refused =
            |usage: Value, max: u64| receipt(&json!("m"), &usage, max).unwrap_err().to_string();
        assert_eq!(
            refused(json!({"output_tokens": 1}), 8),
            "input usage missing"
        );
        assert_eq!(
            refused(json!({"input_tokens": 1}), 8),
            "output usage missing"
        );
        assert_eq!(
            refused(json!({"input_tokens": 1, "output_tokens": 9}), 8),
            "provider usage exceeds contract"
        );
        assert_eq!(
            refused(
                json!({"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": -1}),
                8
            ),
            "invalid cache read usage"
        );
        assert_eq!(
            refused(
                json!({"input_tokens": 1, "output_tokens": 1, "server_tool_use": {"tool_search": "2"}}),
                8
            ),
            "invalid web search usage"
        );
        assert_eq!(
            receipt(&json!(""), &plain, 8).unwrap_err().to_string(),
            "missing provider receipt"
        );
    }

    /// A stream as DeepSeek sends it, from the recorded shape of a live
    /// call: a thinking block, a ping, a text block, then the usage.
    fn stream() -> String {
        let events = [
            (
                "message_start",
                json!({"type": "message_start", "message": {"id": "56ee", "type": "message",
                "role": "assistant", "model": "deepseek-flash", "content": [], "stop_reason": null,
                "usage": {"input_tokens": 58, "cache_creation_input_tokens": 0,
                          "cache_read_input_tokens": 0, "output_tokens": 0}}}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            ),
            ("ping", json!({"type": "ping"})),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "The user wants"}}),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 1,
                "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1,
                "delta": {"type": "text_delta", "text": "1\n2"}}),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 1}),
            ),
            (
                "message_delta",
                json!({"type": "message_delta",
                "delta": {"stop_reason": "max_tokens", "stop_sequence": null},
                "usage": {"input_tokens": 58, "cache_creation_input_tokens": 0,
                          "cache_read_input_tokens": 0, "output_tokens": 40}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ];
        events
            .iter()
            .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
            .collect()
    }

    fn tally(chunks: &[&[u8]]) -> Option<(Value, Value)> {
        let mut tally = Tally::default();
        for chunk in chunks {
            tally.feed(chunk);
        }
        tally.finish()
    }

    #[test]
    fn a_stream_read_in_any_pieces_gives_its_id_and_final_usage() {
        let whole = stream();
        let usage = json!({"input_tokens": 58, "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0, "output_tokens": 40});
        let expected = Some((json!("56ee"), usage));
        assert_eq!(tally(&[whole.as_bytes()]), expected);
        let bytes = whole.as_bytes();
        for size in [1, 2, 7, 64, 1_000] {
            let chunks: Vec<&[u8]> = bytes.chunks(size).collect();
            assert_eq!(tally(&chunks), expected, "chunks of {size}");
        }
        let crlf = whole.replace('\n', "\r\n");
        assert_eq!(tally(&[crlf.as_bytes()]), expected, "CRLF lines");
        let commented = format!(": keep-alive\n\n{whole}");
        assert_eq!(tally(&[commented.as_bytes()]), expected, "a comment");
        let anthropic = whole.replace(
            r#""usage":{"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"input_tokens":58,"output_tokens":40}"#,
            r#""usage":{"output_tokens":40,"cache_read_input_tokens":null}"#,
        );
        assert_ne!(anthropic, whole);
        assert_eq!(
            tally(&[anthropic.as_bytes()]),
            expected,
            "a delta's fields replace the start's, and its nulls do not"
        );
    }

    #[test]
    fn a_stream_that_fails_or_ends_early_has_no_usage() {
        let whole = stream();
        let before_usage = &whole[..whole.find("event: message_delta").unwrap()];
        assert_eq!(
            tally(&[before_usage.as_bytes()]),
            None,
            "cut before its usage"
        );
        let error = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n";
        let failed = format!("{before_usage}{error}");
        assert_eq!(tally(&[failed.as_bytes()]), None, "an error event");
        let timed_out = "data: {\"error\":{\"message\":\"timed out\"}}\n\n";
        assert_eq!(
            tally(&[timed_out.as_bytes()]),
            None,
            "DeepSeek's unnamed error"
        );
        let garbled = whole.replace(
            "\"type\":\"message_delta\",",
            "\"type\":\"message_delta\",,",
        );
        assert_eq!(tally(&[garbled.as_bytes()]), None, "an unreadable usage");
        let huge = format!(
            "{before_usage}event: message_delta\ndata: {}",
            "x".repeat(EVENT_LIMIT + 1)
        );
        assert_eq!(tally(&[huge.as_bytes()]), None, "an event past the limit");
        let skipped = format!(
            "event: content_block_delta\ndata: {}\n\n{whole}",
            "y".repeat(EVENT_LIMIT + 1)
        );
        assert!(
            tally(&[skipped.as_bytes()]).is_some(),
            "a large content event is skipped unread"
        );
    }
}
