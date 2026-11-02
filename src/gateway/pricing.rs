//! What a model call may cost before it runs (its hold) and what it cost after
//! (its settlement), and how a completion's usage and receipt are read.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Bytes added to a request's UTF-8 size before it is priced as input tokens.
/// UTF-8 bytes plus chat framing overbounds byte-BPE input; usage above this
/// bound is refused at settlement.
pub const REQUEST_FRAMING_BYTES: u64 = 4096;

/// Prices in microusd per million tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceTable {
    pub input_microusd_per_million: u64,
    pub output_microusd_per_million: u64,
    /// Settlement rate for prompt tokens the provider reports as cache hits.
    /// Absent settles every input token at the input rate. Holds are always
    /// priced all-miss, so a settlement can never exceed its hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_microusd_per_million: Option<u64>,
}

impl PriceTable {
    /// The admitted bounds: no lower than the conservative peak rates, no
    /// higher than an obvious typo.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.input_microusd_per_million >= 300_000
                && self.output_microusd_per_million >= 1_200_000,
            "price table below admitted conservative peak rates"
        );
        ensure!(
            self.input_microusd_per_million <= 10_000_000
                && self.output_microusd_per_million <= 10_000_000,
            "invalid price table"
        );
        if let Some(hit) = self.cache_hit_microusd_per_million {
            ensure!(
                (6_000..=self.input_microusd_per_million).contains(&hit),
                "cache-hit rate outside admitted peak prices"
            );
        }
        Ok(())
    }

    /// The most `input` and `output` tokens can cost.
    pub fn hold(&self, input: u64, output: u64) -> u64 {
        // Charge all input at peak cache-miss rates; this deliberately overstates the invoice.
        (input * self.input_microusd_per_million).div_ceil(1_000_000)
            + (output * self.output_microusd_per_million).div_ceil(1_000_000)
    }

    /// The hold for one request of `input_bytes` that may answer with up to
    /// `max_output_tokens`.
    pub fn request_hold(&self, input_bytes: usize, max_output_tokens: u64) -> u64 {
        self.hold(
            input_bytes as u64 + REQUEST_FRAMING_BYTES,
            max_output_tokens,
        )
    }

    /// What reported usage costs, with `cached` of the `input` tokens at the
    /// cache-hit rate.
    pub fn settled(&self, input: u64, cached: u64, output: u64) -> u64 {
        let hit_rate = self
            .cache_hit_microusd_per_million
            .unwrap_or(self.input_microusd_per_million);
        // Round the combined input charge once so splitting a prompt never increases its cost.
        ((input - cached) * self.input_microusd_per_million + cached * hit_rate).div_ceil(1_000_000)
            + (output * self.output_microusd_per_million).div_ceil(1_000_000)
    }
}

/// A completion's usage and receipt id, within the request's contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub request_id: String,
}

impl Receipt {
    pub fn read(response: &Value, max_output_tokens: u64) -> Result<Self> {
        let usage = completion_usage(&response["usage"])?;
        let input = usage.input_tokens;
        let output = usage.output_tokens;
        let cached = usage.cached_input_tokens;
        ensure!(
            input <= 1_000_000 && output <= max_output_tokens,
            "provider usage exceeds contract"
        );
        let request_id = response["id"]
            .as_str()
            .filter(|v| !v.is_empty())
            .context("missing provider receipt")?
            .to_owned();
        Ok(Self {
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            request_id,
        })
    }

    pub fn cost(&self, prices: &PriceTable) -> u64 {
        prices.settled(
            self.input_tokens,
            self.cached_input_tokens,
            self.output_tokens,
        )
    }

    /// The usage record a ledger keeps.
    pub fn usage(&self) -> Value {
        let (input, cached, output) = (
            self.input_tokens,
            self.cached_input_tokens,
            self.output_tokens,
        );
        json!({"input_tokens":input,"cached_input_tokens":cached,"uncached_input_tokens":input-cached,"output_tokens":output})
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub categories: BTreeMap<String, u64>,
}

/// Parse reported cache usage once for both hosted inference brokers.
pub fn completion_usage(usage: &Value) -> Result<Usage> {
    let input = usage["prompt_tokens"]
        .as_u64()
        .context("input usage missing")?;
    let output = usage["completion_tokens"]
        .as_u64()
        .context("output usage missing")?;
    let counter = |value: Option<&Value>| -> Result<Option<u64>> {
        value
            .map(|v| v.as_u64().context("invalid cache usage"))
            .transpose()
    };
    let hit = counter(usage.get("prompt_cache_hit_tokens"))?;
    let details = usage.get("prompt_tokens_details");
    ensure!(
        details.is_none_or(|v| v.is_null() || v.is_object()),
        "invalid prompt token details"
    );
    let nested_hit = counter(details.and_then(|v| v.get("cached_tokens")))?;
    let miss = counter(usage.get("prompt_cache_miss_tokens"))?;
    if let (Some(hit), Some(nested)) = (hit, nested_hit) {
        ensure!(hit == nested, "conflicting cache usage");
    }
    if let Some(miss) = miss {
        ensure!(miss <= input, "cache miss usage exceeds total input");
    }
    let cached = hit
        .or(nested_hit)
        .or_else(|| miss.map(|v| input - v))
        .unwrap_or(0);
    // A reported output overrun is still a real receipt: settlement must retain its charge and engage the ledger stop.
    ensure!(cached <= input, "cached usage exceeds total input");
    if let Some(miss) = miss {
        ensure!(
            miss == input - cached,
            "cache usage does not match total input"
        );
    }
    let mut categories = BTreeMap::new();
    categories.insert("input_tokens".into(), input - cached);
    categories.insert("cached_input_tokens".into(), cached);
    categories.insert("output_tokens".into(), output);
    Ok(Usage {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached,
        categories,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Goal::cost` and `Goal::settled_cost` as they were before the move,
    /// for the parity check.
    struct Before {
        input: u64,
        output: u64,
        hit: Option<u64>,
    }
    impl Before {
        fn cost(&self, input: u64, output: u64) -> u64 {
            (input * self.input).div_ceil(1_000_000) + (output * self.output).div_ceil(1_000_000)
        }
        fn settled_cost(&self, input: u64, cached: u64, output: u64) -> u64 {
            let hit_rate = self.hit.unwrap_or(self.input);
            ((input - cached) * self.input + cached * hit_rate).div_ceil(1_000_000)
                + (output * self.output).div_ceil(1_000_000)
        }
    }

    #[test]
    fn holds_and_settlements_match_the_swarm_formulas_they_replaced() {
        let tables = [
            (300_000, 1_200_000, None),
            (300_000, 1_200_000, Some(6_000)),
            (300_000, 1_200_000, Some(300_000)),
            (1_740_000, 3_480_000, Some(145_000)),
            (10_000_000, 10_000_000, Some(9_999_999)),
        ];
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        for (input, output, hit) in tables {
            let prices = PriceTable {
                input_microusd_per_million: input,
                output_microusd_per_million: output,
                cache_hit_microusd_per_million: hit,
            };
            let before = Before { input, output, hit };
            for _ in 0..2_000 {
                let (tokens, out) = (next(2_000_000), next(400_000));
                let cached = next(tokens + 1);
                assert_eq!(prices.hold(tokens, out), before.cost(tokens, out));
                assert_eq!(
                    prices.settled(tokens, cached, out),
                    before.settled_cost(tokens, cached, out)
                );
                let bytes = next(16_777_216) as usize;
                assert_eq!(
                    prices.request_hold(bytes, out),
                    before.cost(bytes as u64 + 4096, out)
                );
            }
            for edge in [0, 1, 999_999, 1_000_000, 1_000_001] {
                assert_eq!(prices.hold(edge, edge), before.cost(edge, edge));
                assert_eq!(
                    prices.settled(edge, edge, edge),
                    before.settled_cost(edge, edge, edge)
                );
            }
        }
    }

    #[test]
    fn a_receipt_reads_usage_and_refuses_what_the_contract_does_not_allow() {
        let response = json!({"id":"r1","usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":90,"completion_tokens":50}});
        let receipt = Receipt::read(&response, 2048).unwrap();
        assert_eq!(
            receipt,
            Receipt {
                input_tokens: 100,
                cached_input_tokens: 90,
                output_tokens: 50,
                request_id: "r1".into()
            }
        );
        assert_eq!(
            receipt.usage(),
            json!({"input_tokens":100,"cached_input_tokens":90,"uncached_input_tokens":10,"output_tokens":50})
        );
        let prices = PriceTable {
            input_microusd_per_million: 300_000,
            output_microusd_per_million: 1_200_000,
            cache_hit_microusd_per_million: Some(6_000),
        };
        assert_eq!(receipt.cost(&prices), 64);
        let refused = |response: Value| Receipt::read(&response, 2048).unwrap_err().to_string();
        assert_eq!(
            refused(json!({"id":"r1","usage":{"prompt_tokens":1,"completion_tokens":2049}})),
            "provider usage exceeds contract"
        );
        assert_eq!(
            refused(json!({"id":"r1","usage":{"prompt_tokens":1_000_001,"completion_tokens":1}})),
            "provider usage exceeds contract"
        );
        assert_eq!(
            refused(json!({"id":"","usage":{"prompt_tokens":1,"completion_tokens":1}})),
            "missing provider receipt"
        );
        assert_eq!(refused(json!({"id":"r1"})), "input usage missing");
    }

    #[test]
    fn the_price_table_keeps_the_goal_bounds() {
        let mut prices = PriceTable {
            input_microusd_per_million: 300_000,
            output_microusd_per_million: 1_200_000,
            cache_hit_microusd_per_million: Some(6_000),
        };
        prices.validate().unwrap();
        let refused = |prices: PriceTable| prices.validate().unwrap_err().to_string();
        prices.input_microusd_per_million = 299_999;
        assert_eq!(
            refused(prices),
            "price table below admitted conservative peak rates"
        );
        prices.input_microusd_per_million = 10_000_001;
        assert_eq!(refused(prices), "invalid price table");
        prices.input_microusd_per_million = 300_000;
        prices.cache_hit_microusd_per_million = Some(5_999);
        assert_eq!(
            refused(prices),
            "cache-hit rate outside admitted peak prices"
        );
        prices.cache_hit_microusd_per_million = Some(300_001);
        assert_eq!(
            refused(prices),
            "cache-hit rate outside admitted peak prices"
        );
    }

    #[test]
    fn cached_usage_is_disjoint_and_provider_formats_agree() {
        for fields in [
            json!({"prompt_cache_hit_tokens":90}),
            json!({"prompt_tokens_details":{"cached_tokens":90}}),
            json!({"prompt_cache_miss_tokens":10}),
            json!({"prompt_cache_hit_tokens":90,"prompt_cache_miss_tokens":10,"prompt_tokens_details":{"cached_tokens":90}}),
        ] {
            let mut usage = json!({"prompt_tokens":100,"completion_tokens":50});
            usage
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let parsed = completion_usage(&usage).unwrap();
            assert_eq!(parsed.input_tokens, 100);
            assert_eq!(parsed.cached_input_tokens, 90);
            assert_eq!(parsed.categories["input_tokens"], 10);
            assert_eq!(parsed.categories["cached_input_tokens"], 90);
        }
        assert_eq!(
            completion_usage(&json!({"prompt_tokens":100,"completion_tokens":0}))
                .unwrap()
                .cached_input_tokens,
            0
        );
    }

    #[test]
    fn malformed_or_conflicting_cache_usage_cannot_reduce_a_charge() {
        for fields in [
            json!({"prompt_cache_hit_tokens":101}),
            json!({"prompt_cache_hit_tokens":null}),
            json!({"prompt_cache_hit_tokens":"90"}),
            json!({"prompt_cache_hit_tokens":-1}),
            json!({"prompt_cache_hit_tokens":1.5}),
            json!({"prompt_cache_miss_tokens":101}),
            json!({"prompt_cache_hit_tokens":90,"prompt_cache_miss_tokens":11}),
            json!({"prompt_cache_hit_tokens":90,"prompt_tokens_details":{"cached_tokens":80}}),
            json!({"prompt_tokens_details":[]}),
        ] {
            let mut usage = json!({"prompt_tokens":100,"completion_tokens":50});
            usage
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            assert!(completion_usage(&usage).is_err(), "{usage}");
        }
    }
}
