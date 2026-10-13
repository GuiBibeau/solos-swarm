//! Shared provider quota: adding a node never creates another allowance.
use crate::audit::{Hash, Signed};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaPauseKind {
    RetryAfter,
    Window,
    Concurrent,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaPaused {
    pub scope: String,
    /// Absolute UTC earliest recheck; zero means holdings must change, not clock expiry.
    pub retry_after_ms: u64,
    pub kind: QuotaPauseKind,
}
impl std::fmt::Display for QuotaPaused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "shared provider quota paused ({:?})", self.kind)
    }
}
impl std::error::Error for QuotaPaused {}
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
    /// A read-only check for new work. An existing intent never hides its own held allowance.
    pub fn check_available(
        &self,
        request: &QuotaRequest,
        provider: &str,
        now_ms: u64,
    ) -> Result<()> {
        let mut probe = request.clone();
        let mut suffix = 0_u64;
        while self.reservations.contains_key(&probe.intent) {
            probe.intent = format!("availability-check-{}-{suffix}", request.intent);
            suffix = suffix
                .checked_add(1)
                .context("quota availability namespace overflow")?;
        }
        self.clone().reserve(probe, provider, now_ms)
    }
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
                && request.input <= c.input_tokens
                && request.output <= c.output_tokens
                && now_ms < request.deadline_ms
                && request.deadline_ms <= c.expiry_ms,
            "request exceeds provider quota posture"
        );
        if let Some(until) = self
            .blocked_until_ms
            .get(&request.scope)
            .filter(|time| **time > now_ms)
        {
            return Err(QuotaPaused {
                scope: request.scope.clone(),
                retry_after_ms: *until,
                kind: QuotaPauseKind::RetryAfter,
            }
            .into());
        }
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
        if concurrent >= c.concurrent {
            return Err(QuotaPaused {
                scope: request.scope.clone(),
                retry_after_ms: 0,
                kind: QuotaPauseKind::Concurrent,
            }
            .into());
        }
        if !(requests < c.requests
            && input
                .checked_add(request.input)
                .is_some_and(|v| v <= c.input_tokens)
            && output
                .checked_add(request.output)
                .is_some_and(|v| v <= c.output_tokens))
        {
            return Err(QuotaPaused {
                scope: request.scope.clone(),
                retry_after_ms: window.saturating_add(1).saturating_mul(c.window_ms),
                kind: QuotaPauseKind::Window,
            }
            .into());
        }
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
        if let Some(until) = self
            .blocked_until_ms
            .get(&reservation.request.scope)
            .filter(|time| **time > now_ms)
        {
            return Err(QuotaPaused {
                scope: reservation.request.scope.clone(),
                retry_after_ms: *until,
                kind: QuotaPauseKind::RetryAfter,
            }
            .into());
        }
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
    fn configured(requests: u64, concurrent: u64) -> (Quotas, QuotaRequest) {
        let signer = CryptoSigner::from_seed([23; 32]);
        let mut quotas = Quotas::default();
        quotas
            .configure(
                signer
                    .sign(
                        "provider-quota",
                        QuotaConfig {
                            scope: "shared/model".into(),
                            providers: vec!["p".into()],
                            version: "v1".into(),
                            window_ms: 1000,
                            requests,
                            input_tokens: 100,
                            output_tokens: 100,
                            concurrent,
                            expiry_ms: 10_000,
                        },
                    )
                    .unwrap(),
                &signer.public_key(),
                1,
            )
            .unwrap();
        (
            quotas,
            QuotaRequest {
                intent: "first".into(),
                scope: "shared/model".into(),
                input: 40,
                output: 40,
                deadline_ms: 5000,
            },
        )
    }
    fn assert_pause(error: anyhow::Error, kind: QuotaPauseKind, retry_after_ms: u64) {
        let pause = error
            .downcast_ref::<QuotaPaused>()
            .expect("typed quota pause");
        assert_eq!(pause.scope, "shared/model");
        assert_eq!(pause.kind, kind);
        assert_eq!(pause.retry_after_ms, retry_after_ms);
    }
    #[test]
    fn window_pause_resumes_only_in_fresh_window_without_mutating_checks() {
        let (mut quotas, mut request) = configured(1, 2);
        quotas.reserve(request.clone(), "p", 1).unwrap();
        quotas.dispatch(&request.intent, 2).unwrap();
        quotas
            .finish(&request.intent, Some((20, 20)), false)
            .unwrap();
        request.intent = "other-node".into();
        let original = serde_json::to_value(&quotas).unwrap();
        assert_pause(
            quotas.reserve(request.clone(), "p", 3).unwrap_err(),
            QuotaPauseKind::Window,
            1000,
        );
        assert_pause(
            quotas.check_available(&request, "p", 999).unwrap_err(),
            QuotaPauseKind::Window,
            1000,
        );
        assert_eq!(serde_json::to_value(&quotas).unwrap(), original);
        quotas.check_available(&request, "p", 1000).unwrap();
        assert_eq!(serde_json::to_value(&quotas).unwrap(), original);
        quotas.reserve(request, "p", 1000).unwrap();
        assert_eq!(quotas.reservations.len(), 2);
    }
    #[test]
    fn concurrent_unknown_hold_never_resumes_by_clock_or_same_intent_shortcut() {
        let (mut quotas, request) = configured(10, 1);
        quotas.reserve(request.clone(), "p", 1).unwrap();
        quotas.dispatch(&request.intent, 2).unwrap();
        quotas.finish(&request.intent, None, false).unwrap();
        let original = serde_json::to_value(&quotas).unwrap();
        let mut next = request.clone();
        next.intent = "other-node".into();
        for now in [3, 1001, 4000] {
            assert_pause(
                quotas.reserve(next.clone(), "p", now).unwrap_err(),
                QuotaPauseKind::Concurrent,
                0,
            );
            assert_pause(
                quotas.check_available(&request, "p", now).unwrap_err(),
                QuotaPauseKind::Concurrent,
                0,
            );
        }
        assert_eq!(serde_json::to_value(&quotas).unwrap(), original);
        assert_eq!(quotas.reservations[&request.intent].state, "unknown");
        quotas
            .finish(&request.intent, Some((30, 20)), false)
            .unwrap();
        quotas.check_available(&next, "p", 4000).unwrap();
        quotas.reserve(next, "p", 4000).unwrap();
        assert_eq!(quotas.reservations.len(), 2);
    }
    #[test]
    fn retry_after_pauses_reserve_and_dispatch_until_exact_current_boundary() {
        let (mut quotas, request) = configured(10, 2);
        quotas.reserve(request.clone(), "p", 1).unwrap();
        quotas.retry_after(&request.scope, 2, 500).unwrap();
        let original = serde_json::to_value(&quotas).unwrap();
        assert_pause(
            quotas.dispatch(&request.intent, 3).unwrap_err(),
            QuotaPauseKind::RetryAfter,
            502,
        );
        let mut next = request.clone();
        next.intent = "other-node".into();
        assert_pause(
            quotas.reserve(next.clone(), "p", 501).unwrap_err(),
            QuotaPauseKind::RetryAfter,
            502,
        );
        assert_pause(
            quotas.check_available(&next, "p", 501).unwrap_err(),
            QuotaPauseKind::RetryAfter,
            502,
        );
        assert_eq!(serde_json::to_value(&quotas).unwrap(), original);
        quotas.check_available(&next, "p", 502).unwrap();
        assert_eq!(serde_json::to_value(&quotas).unwrap(), original);
        quotas.dispatch(&request.intent, 502).unwrap();
        quotas.reserve(next, "p", 502).unwrap();
        assert_eq!(quotas.reservations.len(), 2);
    }
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
