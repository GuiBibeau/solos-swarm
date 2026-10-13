//! Shared provider quota: adding a node never creates another allowance.
use crate::audit::{Hash, Signed};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaConfig {
    pub scope: String,
    pub providers: Vec<String>,
    pub version: String,
    pub window_ms: u64,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub concurrent: u64,
    pub expiry_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaRequest {
    pub intent: String,
    pub scope: String,
    pub input: u64,
    pub output: u64,
    pub deadline_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reservation {
    pub request: QuotaRequest,
    pub window: u64,
    pub state: String,
    pub actual_input: u64,
    pub actual_output: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Quotas {
    pub configs: BTreeMap<String, Signed<QuotaConfig>>,
    pub reservations: BTreeMap<String, Reservation>,
    pub blocked_until_ms: BTreeMap<String, u64>,
}
impl Quotas {
    pub fn configure(
        &mut self,
        config: Signed<QuotaConfig>,
        trusted: &Hash,
        now_ms: u64,
    ) -> Result<()> {
        config.verify("provider-quota", trusted)?;
        let c = &config.payload;
        ensure!(
            !c.scope.is_empty()
                && !c.providers.is_empty()
                && !c.version.is_empty()
                && c.window_ms > 0
                && c.requests > 0
                && c.input_tokens > 0
                && c.output_tokens > 0
                && c.concurrent > 0
                && c.expiry_ms > now_ms,
            "provider quota bounds missing"
        );
        ensure!(
            !self.configs.contains_key(&c.scope),
            "quota replacement requires draining and a new scope version"
        );
        self.configs.insert(c.scope.clone(), config);
        Ok(())
    }
    pub fn reserve(&mut self, request: QuotaRequest, provider: &str, now_ms: u64) -> Result<()> {
        if let Some(old) = self.reservations.get(&request.intent) {
            ensure!(old.request == request, "quota intent conflict");
            return Ok(());
        }
        let c = &self
            .configs
            .get(&request.scope)
            .context("shared provider quota not configured")?
            .payload;
        ensure!(
            c.expiry_ms > now_ms
                && c.providers.iter().any(|p| p == provider)
                && request.input > 0
                && request.output > 0
                && now_ms < request.deadline_ms
                && request.deadline_ms <= c.expiry_ms,
            "request exceeds provider quota posture"
        );
        ensure!(
            self.blocked_until_ms
                .get(&request.scope)
                .is_none_or(|time| *time <= now_ms),
            "provider Retry-After active; agent must pause"
        );
        let window = now_ms / c.window_ms;
        let mut requests = 0u64;
        let mut input = 0u64;
        let mut output = 0u64;
        let mut concurrent = 0u64;
        for reservation in self
            .reservations
            .values()
            .filter(|r| r.request.scope == request.scope && r.state != "not_dispatched")
        {
            if matches!(
                reservation.state.as_str(),
                "reserved" | "dispatched" | "unknown"
            ) {
                concurrent += 1;
            }
            if reservation.window == window {
                requests += 1;
                input = input
                    .checked_add(if reservation.state == "done" {
                        reservation.actual_input
                    } else {
                        reservation.request.input
                    })
                    .context("quota counter overflow")?;
                output = output
                    .checked_add(if reservation.state == "done" {
                        reservation.actual_output
                    } else {
                        reservation.request.output
                    })
                    .context("quota counter overflow")?;
            }
        }
        ensure!(
            requests < c.requests
                && concurrent < c.concurrent
                && input
                    .checked_add(request.input)
                    .is_some_and(|v| v <= c.input_tokens)
                && output
                    .checked_add(request.output)
                    .is_some_and(|v| v <= c.output_tokens),
            "shared provider quota exhausted; agent must pause"
        );
        self.reservations.insert(
            request.intent.clone(),
            Reservation {
                request,
                window,
                state: "reserved".into(),
                actual_input: 0,
                actual_output: 0,
            },
        );
        Ok(())
    }
    pub fn dispatch(&mut self, intent: &str, now_ms: u64) -> Result<()> {
        let reservation = self
            .reservations
            .get_mut(intent)
            .context("provider allowance absent")?;
        ensure!(
            reservation.state == "reserved" && now_ms < reservation.request.deadline_ms,
            "provider allowance stale or dispatched"
        );
        ensure!(
            self.blocked_until_ms
                .get(&reservation.request.scope)
                .is_none_or(|time| *time <= now_ms),
            "provider Retry-After active"
        );
        reservation.state = "dispatched".into();
        Ok(())
    }
    pub fn finish(
        &mut self,
        intent: &str,
        usage: Option<(u64, u64)>,
        proven_not_dispatched: bool,
    ) -> Result<bool> {
        let reservation = self
            .reservations
            .get_mut(intent)
            .context("provider allowance absent")?;
        ensure!(
            matches!(reservation.state.as_str(), "dispatched" | "unknown")
                || proven_not_dispatched && reservation.state == "reserved",
            "quota outcome is not awaiting reconciliation"
        );
        if let Some((input, output)) = usage {
            reservation.actual_input = input;
            reservation.actual_output = output;
            reservation.state = "done".into();
            Ok(input > reservation.request.input || output > reservation.request.output)
        } else {
            reservation.state = if proven_not_dispatched {
                "not_dispatched"
            } else {
                "unknown"
            }
            .into();
            Ok(false)
        }
    }
    pub fn retry_after(&mut self, scope: &str, now_ms: u64, delay_ms: u64) -> Result<()> {
        ensure!(
            self.configs.contains_key(scope) && delay_ms > 0 && delay_ms <= 86_400_000,
            "invalid Retry-After"
        );
        let until = now_ms
            .checked_add(delay_ms)
            .context("Retry-After overflow")?;
        let stored = self.blocked_until_ms.entry(scope.into()).or_default();
        *stored = (*stored).max(until);
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::CryptoSigner;
    #[test]
    fn quota_is_shared_across_nodes_unknown_holds_and_receipts() {
        let signer = CryptoSigner::from_seed([23; 32]);
        let mut quotas = Quotas::default();
        quotas
            .configure(
                signer
                    .sign(
                        "provider-quota",
                        QuotaConfig {
                            scope: "organization/model".into(),
                            providers: vec!["p".into()],
                            version: "v1".into(),
                            window_ms: 1000,
                            requests: 2,
                            input_tokens: 100,
                            output_tokens: 100,
                            concurrent: 1,
                            expiry_ms: 10_000,
                        },
                    )
                    .unwrap(),
                &signer.public_key(),
                1,
            )
            .unwrap();
        let request = QuotaRequest {
            intent: "node-a-intent".into(),
            scope: "organization/model".into(),
            input: 40,
            output: 40,
            deadline_ms: 3000,
        };
        quotas.reserve(request.clone(), "p", 1).unwrap();
        quotas.dispatch(&request.intent, 2).unwrap();
        quotas.finish(&request.intent, None, false).unwrap();
        let mut next = request.clone();
        next.intent = "node-b-intent".into();
        assert!(quotas.reserve(next.clone(), "p", 3).is_err());
        quotas
            .finish(&request.intent, Some((30, 20)), false)
            .unwrap();
        quotas.reserve(next.clone(), "p", 3).unwrap();
        quotas.dispatch(&next.intent, 4).unwrap();
        assert!(quotas.finish(&next.intent, Some((41, 20)), false).unwrap());
        let mut third = next;
        third.intent = "third".into();
        assert!(quotas.reserve(third.clone(), "p", 5).is_err());
        quotas
            .retry_after("organization/model", 1001, 1000)
            .unwrap();
        assert!(quotas.reserve(third.clone(), "p", 1002).is_err());
        quotas.reserve(third, "p", 2002).unwrap();
    }
}
