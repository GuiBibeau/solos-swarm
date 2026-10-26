//! Deterministic observations of measured egress. Flags never grant actuator authority.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FlowConfig {
    pub window_ms: u64,
    pub max_records: usize,
    pub max_flags: usize,
    pub fan_out_threshold: usize,
    pub fan_in_threshold: usize,
}
impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            window_ms: 60_000,
            max_records: 10_000,
            max_flags: 1000,
            fan_out_threshold: 16,
            fan_in_threshold: 16,
        }
    }
}
impl FlowConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1000..=3_600_000).contains(&self.window_ms)
                && (1..=100_000).contains(&self.max_records)
                && (1..=10_000).contains(&self.max_flags)
                && (1..=4096).contains(&self.fan_out_threshold)
                && (1..=10_000).contains(&self.fan_in_threshold),
            "egress observation limits must be finite and nonzero"
        );
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    Provider,
    Target,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowOutcome {
    DispatchAttempt,
    Completed,
    Unknown,
    DefinitelyNotDispatched,
    ReceiptWithoutCompletion,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservedFlow {
    pub intent_id: String,
    pub goal_id: String,
    pub task_id: String,
    pub holder: String,
    pub recipient: String,
    pub destination: String,
    pub operation: String,
    pub resource: String,
    pub route: RouteKind,
    pub opened_ms: u64,
    pub closed_ms: Option<u64>,
    pub contacted: bool,
    pub outcome: FlowOutcome,
    pub payload_request_bytes: u64,
    pub payload_response_bytes: u64,
}
impl ObservedFlow {
    fn identity(&self) -> (&str, &str, &str, &str, &str, &str, &str, RouteKind) {
        (
            &self.goal_id,
            &self.task_id,
            &self.holder,
            &self.recipient,
            &self.destination,
            &self.operation,
            &self.resource,
            self.route,
        )
    }
    fn validate(&mut self, now_ms: u64) -> Result<()> {
        for text in [
            &self.intent_id,
            &self.goal_id,
            &self.task_id,
            &self.holder,
            &self.recipient,
            &self.operation,
            &self.resource,
        ] {
            ensure!(
                !text.is_empty() && text.len() <= 4096 && !text.contains(['\n', '\r', '\0']),
                "invalid egress attribution"
            );
        }
        ensure!(
            self.destination.len() <= 4096,
            "egress destination exceeds bound"
        );
        let url = reqwest::Url::parse(&self.destination)
            .context("egress destination must be an actual configured HTTP receiver")?;
        ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "unsupported egress receiver"
        );
        // Retain receiver attribution without URL credentials, query secrets, or per-request paths.
        self.destination = url.origin().ascii_serialization();
        ensure!(
            self.opened_ms <= now_ms.saturating_add(1000)
                && self.closed_ms.is_none_or(
                    |closed| closed >= self.opened_ms && closed <= now_ms.saturating_add(1000)
                ),
            "egress timestamps invalid"
        );
        ensure!(
            self.outcome != FlowOutcome::DefinitelyNotDispatched || !self.contacted,
            "undispatched flow cannot claim receiver contact"
        );
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnomalyKind {
    FanOut,
    FanIn,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AnomalyFlag {
    pub id: String,
    pub kind: AnomalyKind,
    pub goal_id: String,
    pub intent_id: String,
    pub holder: String,
    pub destination: String,
    pub operation: String,
    pub resource: String,
    pub observed_ms: u64,
    pub count: usize,
    pub threshold: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FlowMonitor {
    pub config: FlowConfig,
    records: BTreeMap<String, ObservedFlow>,
    flags: BTreeMap<String, AnomalyFlag>,
}
impl Default for FlowMonitor {
    fn default() -> Self {
        Self::new(FlowConfig::default()).expect("finite default")
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalFlows {
    pub records: Vec<ObservedFlow>,
    pub flags: Vec<AnomalyFlag>,
}
impl FlowMonitor {
    pub fn new(config: FlowConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            records: BTreeMap::new(),
            flags: BTreeMap::new(),
        })
    }
    pub fn configure(&mut self, config: FlowConfig, now_ms: u64) -> Result<()> {
        config.validate()?;
        self.config = config;
        self.prune(now_ms);
        Ok(())
    }
    fn prune(&mut self, now_ms: u64) {
        let floor = now_ms.saturating_sub(self.config.window_ms);
        self.records
            .retain(|_, flow| flow.closed_ms.unwrap_or(flow.opened_ms) >= floor);
        // Flags remain observable after the measurement window closes. The broker also writes
        // each new flag into its permanent signed audit before this bounded view can evict it.
        while self.records.len() > self.config.max_records {
            let key = self
                .records
                .iter()
                .min_by_key(|(id, flow)| (flow.closed_ms.unwrap_or(flow.opened_ms), *id))
                .map(|(id, _)| id.clone())
                .expect("nonempty");
            self.records.remove(&key);
        }
        while self.flags.len() > self.config.max_flags {
            let key = self
                .flags
                .iter()
                .min_by_key(|(id, flag)| (flag.observed_ms, *id))
                .map(|(id, _)| id.clone())
                .expect("nonempty");
            self.flags.remove(&key);
        }
    }
    pub fn record(&mut self, mut flow: ObservedFlow, now_ms: u64) -> Result<Vec<AnomalyFlag>> {
        self.config.validate()?;
        flow.validate(now_ms)?;
        self.prune(now_ms);
        if flow.closed_ms.unwrap_or(flow.opened_ms) < now_ms.saturating_sub(self.config.window_ms) {
            return Ok(vec![]);
        }
        if let Some(previous) = self.records.get(&flow.intent_id) {
            ensure!(
                previous.identity() == flow.identity(),
                "egress intent reused for different authority/receiver"
            );
            ensure!(
                flow.opened_ms >= previous.opened_ms,
                "egress observation moved backwards"
            );
            flow.opened_ms = previous.opened_ms;
            if previous == &flow {
                return Ok(vec![]);
            }
            ensure!(
                previous.closed_ms.is_none()
                    || previous.outcome == FlowOutcome::Unknown
                    || previous.outcome == FlowOutcome::ReceiptWithoutCompletion,
                "completed egress observation cannot be rewritten"
            );
            ensure!(
                (!previous.contacted || flow.contacted)
                    && flow.payload_request_bytes >= previous.payload_request_bytes
                    && flow.payload_response_bytes >= previous.payload_response_bytes,
                "egress observation moved backwards"
            );
        }
        self.records.insert(flow.intent_id.clone(), flow.clone());
        let mut added = vec![];
        if flow.route == RouteKind::Target && flow.contacted {
            let targets = self
                .records
                .values()
                .filter(|other| {
                    other.route == RouteKind::Target
                        && other.contacted
                        && other.holder == flow.holder
                })
                .map(|other| other.destination.as_str())
                .collect::<BTreeSet<_>>()
                .len();
            let actors = self
                .records
                .values()
                .filter(|other| {
                    other.route == RouteKind::Target
                        && other.contacted
                        && other.destination == flow.destination
                })
                .map(|other| other.holder.as_str())
                .collect::<BTreeSet<_>>()
                .len();
            for (kind, count, threshold) in [
                (AnomalyKind::FanOut, targets, self.config.fan_out_threshold),
                (AnomalyKind::FanIn, actors, self.config.fan_in_threshold),
            ] {
                if count > threshold {
                    let id = format!(
                        "{}:{}",
                        flow.intent_id,
                        if kind == AnomalyKind::FanOut {
                            "fan-out"
                        } else {
                            "fan-in"
                        }
                    );
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        self.flags.entry(id.clone())
                    {
                        let flag = AnomalyFlag {
                            id: id.clone(),
                            kind,
                            goal_id: flow.goal_id.clone(),
                            intent_id: flow.intent_id.clone(),
                            holder: flow.holder.clone(),
                            destination: flow.destination.clone(),
                            operation: flow.operation.clone(),
                            resource: flow.resource.clone(),
                            observed_ms: now_ms,
                            count,
                            threshold,
                        };
                        entry.insert(flag.clone());
                        added.push(flag);
                    }
                }
            }
        }
        self.prune(now_ms);
        Ok(added)
    }
    /// Broker supplies an authorized Goal filter; no foreign-holder lists or payload bodies escape.
    pub fn goal_view(&self, goal: &str, limit: usize) -> GoalFlows {
        let limit = limit.min(1000);
        let mut records = self
            .records
            .values()
            .filter(|flow| flow.goal_id == goal)
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by_key(|flow| (flow.opened_ms, flow.intent_id.clone()));
        records.reverse();
        records.truncate(limit);
        let mut flags = self
            .flags
            .values()
            .filter(|flag| flag.goal_id == goal)
            .cloned()
            .collect::<Vec<_>>();
        flags.sort_by_key(|flag| (flag.observed_ms, flag.id.clone()));
        flags.reverse();
        flags.truncate(limit);
        GoalFlows { records, flags }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn event(id: &str, actor: &str, receiver: &str, route: RouteKind) -> ObservedFlow {
        ObservedFlow {
            intent_id: id.into(),
            goal_id: "goal".into(),
            task_id: "task".into(),
            holder: actor.into(),
            recipient: "declared-receiver".into(),
            destination: receiver.into(),
            operation: "target_read".into(),
            resource: "owned-target".into(),
            route,
            opened_ms: 1000,
            closed_ms: Some(1100),
            contacted: true,
            outcome: FlowOutcome::Completed,
            payload_request_bytes: 4,
            payload_response_bytes: 8,
        }
    }
    #[test]
    fn actual_contacts_flag_fan_patterns_shared_providers_and_replays_are_exempt() {
        let mut monitor = FlowMonitor::new(FlowConfig {
            fan_in_threshold: 1,
            fan_out_threshold: 1,
            ..Default::default()
        })
        .unwrap();
        assert!(
            monitor
                .record(
                    event(
                        "one",
                        "agent-a",
                        "https://user:secret@one.test/path?key=secret",
                        RouteKind::Target
                    ),
                    1100
                )
                .unwrap()
                .is_empty()
        );
        let flags = monitor
            .record(
                event("two", "agent-a", "https://two.test/", RouteKind::Target),
                1100,
            )
            .unwrap();
        assert_eq!(flags[0].kind, AnomalyKind::FanOut);
        let flags = monitor
            .record(
                event("three", "agent-b", "https://one.test/", RouteKind::Target),
                1100,
            )
            .unwrap();
        assert_eq!(flags[0].kind, AnomalyKind::FanIn);
        assert!(
            monitor
                .record(
                    event("three", "agent-b", "https://one.test/", RouteKind::Target),
                    1100
                )
                .unwrap()
                .is_empty()
        );
        for index in 0..20 {
            assert!(
                monitor
                    .record(
                        event(
                            &format!("provider-{index}"),
                            &format!("worker-{index}"),
                            "https://api.provider.test/",
                            RouteKind::Provider
                        ),
                        1100
                    )
                    .unwrap()
                    .is_empty()
            );
        }
        let view = monitor.goal_view("goal", 32);
        assert_eq!(view.flags.len(), 2);
        assert!(
            view.records
                .iter()
                .all(|flow| !flow.destination.contains("secret"))
        );
        assert!(monitor.goal_view("foreign-goal", 32).records.is_empty());
    }
    #[test]
    fn attempts_do_not_claim_contact_updates_are_monotonic_and_storage_is_bounded() {
        let mut monitor = FlowMonitor::new(FlowConfig {
            window_ms: 1000,
            max_records: 2,
            max_flags: 1,
            fan_out_threshold: 1,
            ..Default::default()
        })
        .unwrap();
        let mut attempt = event("one", "a", "https://one.test/", RouteKind::Target);
        attempt.contacted = false;
        attempt.closed_ms = None;
        attempt.outcome = FlowOutcome::DispatchAttempt;
        attempt.payload_response_bytes = 0;
        assert!(monitor.record(attempt, 1100).unwrap().is_empty());
        assert!(
            monitor
                .record(
                    event("one", "a", "https://one.test/", RouteKind::Target),
                    1100
                )
                .unwrap()
                .is_empty()
        );
        assert!(
            monitor
                .record(
                    event("one", "another", "https://one.test/", RouteKind::Target),
                    1100
                )
                .is_err()
        );
        for (id, url) in [
            ("two", "https://two.test/"),
            ("three", "https://three.test/"),
        ] {
            let _ = monitor
                .record(event(id, "a", url, RouteKind::Target), 1100)
                .unwrap();
        }
        assert!(monitor.records.len() <= 2 && monitor.flags.len() <= 1);
        monitor.configure(monitor.config.clone(), 3000).unwrap();
        assert!(monitor.records.is_empty());
        assert_eq!(monitor.flags.len(), 1);
    }
}
