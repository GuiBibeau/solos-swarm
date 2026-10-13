//! Bounded correlation metadata. Trace headers are diagnostics, never authorization or lineage.
use crate::{audit::hash, identity::WorkloadId};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceContext {
    pub trace_id: String,
    pub span_id: String,
    pub trace_flags: u8,
    pub tracestate: Option<String>,
}
impl TraceContext {
    /// Non-Task work gets its own diagnostic trace and no invented Task association.
    pub fn fresh_root() -> Self {
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        let span = hash(uuid::Uuid::new_v4().as_bytes());
        Self {
            trace_id: hex_bytes(&bytes),
            span_id: hex_bytes(&span[..8]),
            trace_flags: 1,
            tracestate: None,
        }
    }
    /// A Task keeps one trace across nodes, holders and fencing incarnations.
    pub fn for_task(deployment: &str, goal: &str, task: &str) -> Self {
        let digest = hash(format!("solos-task-trace-v1\0{deployment}\0{goal}\0{task}").as_bytes());
        Self {
            trace_id: hex_bytes(&digest[..16]),
            span_id: hex_bytes(&digest[16..24]),
            trace_flags: 1,
            tracestate: None,
        }
    }
    pub fn child(&self, operation: &str, intent: &str, attempt: u64) -> Self {
        let digest = hash(
            format!(
                "solos-task-span-v1\0{}\0{}\0{operation}\0{intent}\0{attempt}",
                self.trace_id, self.span_id
            )
            .as_bytes(),
        );
        Self {
            span_id: hex_bytes(&digest[..8]),
            ..self.clone()
        }
    }
    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            self.trace_id, self.span_id, self.trace_flags
        )
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            valid_hex(&self.trace_id, 32) && valid_hex(&self.span_id, 16),
            "invalid trace or span identity"
        );
        if let Some(state) = &self.tracestate {
            ensure!(
                !state.is_empty() && state.len() <= 512 && state.is_ascii(),
                "invalid tracestate bound"
            );
            let members: Vec<_> = state.split(',').collect();
            ensure!(members.len() <= 32, "too many tracestate members");
            let mut keys = std::collections::BTreeSet::new();
            for member in members {
                let (key, value) = member
                    .trim()
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("invalid tracestate member"))?;
                ensure!(
                    valid_state_key(key) && keys.insert(key),
                    "invalid or duplicate tracestate key"
                );
                ensure!(
                    !value.is_empty()
                        && value.len() <= 256
                        && !value.ends_with(' ')
                        && value
                            .bytes()
                            .all(|b| (0x20..=0x7e).contains(&b) && b != b'=' && b != b','),
                    "invalid tracestate value"
                );
            }
        }
        Ok(())
    }
    pub fn parse(parent: &str, state: Option<&str>) -> Result<Self> {
        let parts: Vec<_> = parent.split('-').collect();
        ensure!(
            parts.len() == 4
                && parts[0] == "00"
                && parent.len() == 55
                && parts[3]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid traceparent version or size"
        );
        let context = Self {
            trace_id: parts[1].into(),
            span_id: parts[2].into(),
            trace_flags: u8::from_str_radix(parts[3], 16)?,
            tracestate: state.map(str::to_owned),
        };
        context.validate()?;
        Ok(context)
    }
}
fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn valid_state_key(key: &str) -> bool {
    let chars = |text: &str| {
        text.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-*/".contains(&b))
    };
    if let Some((tenant, system)) = key.split_once('@') {
        !tenant.is_empty()
            && tenant.len() <= 241
            && tenant.as_bytes()[0].is_ascii_alphanumeric()
            && chars(tenant)
            && !system.is_empty()
            && system.len() <= 14
            && system.as_bytes()[0].is_ascii_lowercase()
            && chars(system)
    } else {
        !key.is_empty() && key.len() <= 256 && key.as_bytes()[0].is_ascii_lowercase() && chars(key)
    }
}
fn valid_hex(value: &str, size: usize) -> bool {
    value.len() == size
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && value.bytes().any(|byte| byte != b'0')
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Correlation {
    pub deployment_id: String,
    pub run_id: Option<String>,
    pub goal_id: Option<String>,
    pub task_id: Option<String>,
    pub intent_id: Option<String>,
    pub agent_id: Option<String>,
    pub incarnation: Option<u64>,
    pub node_id: Option<String>,
    pub execution_attempt: Option<u64>,
    pub trace_context: Option<TraceContext>,
}
impl Correlation {
    pub fn for_actor(deployment: &str, actor: &str) -> Self {
        let workload = WorkloadId::parse(actor).ok();
        Self {
            deployment_id: deployment.into(),
            run_id: None,
            goal_id: workload.as_ref().map(|id| id.goal.clone()),
            task_id: None,
            intent_id: None,
            agent_id: workload.as_ref().map(|id| id.agent.clone()),
            incarnation: workload.as_ref().and_then(|id| id.incarnation.parse().ok()),
            node_id: workload.as_ref().map(|id| id.node.clone()),
            execution_attempt: None,
            trace_context: None,
        }
    }
    pub fn with_task(mut self, goal: &str, task: &str) -> Self {
        self.goal_id = Some(goal.into());
        self.task_id = Some(task.into());
        self.trace_context = Some(TraceContext::for_task(&self.deployment_id, goal, task));
        self
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.deployment_id.is_empty() && self.deployment_id.len() <= 128,
            "invalid deployment correlation"
        );
        for value in [
            &self.run_id,
            &self.goal_id,
            &self.task_id,
            &self.intent_id,
            &self.agent_id,
            &self.node_id,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                !value.is_empty() && value.len() <= 256 && !value.contains(['\r', '\n', '\0']),
                "invalid correlation field"
            );
        }
        if let Some(trace) = &self.trace_context {
            trace.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DiagnosticReason {
    AllocationWaiting,
    StateCapacity,
    BudgetExhausted,
    ProviderUnavailable,
    ProviderOutcome {
        intent_id: String,
        outcome: crate::provider::Outcome,
    },
    RateLimited {
        retry_after_ms: u64,
        quota_kind: crate::quota::QuotaPauseKind,
    },
    LeaseSafety,
    MissingDependency {
        service: String,
    },
    Cleanup {
        completed: bool,
    },
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticRecord {
    pub goal_id: String,
    pub goal_version: u64,
    pub task_id: Option<String>,
    pub task_fence: Option<u64>,
    pub reason: DiagnosticReason,
    pub observed_revision: u64,
    pub recorded_ms: u64,
    pub markings: std::collections::BTreeSet<String>,
    pub correlation: Correlation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RatePause {
    pub goal_version: u64,
    pub request: crate::quota::QuotaRequest,
    pub provider: String,
    pub pause: crate::quota::QuotaPaused,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_trace_is_stable_across_nodes_and_action_spans_are_distinct() {
        let root = TraceContext::for_task("deployment", "goal", "task");
        let provider = root.child("provider", "intent-1", 1);
        let tool = root.child("tool", "intent-2", 1);
        assert_eq!(root.trace_id, provider.trace_id);
        assert_eq!(provider.trace_id, tool.trace_id);
        assert_ne!(provider.span_id, tool.span_id);
        assert_eq!(
            TraceContext::parse(&provider.traceparent(), None).unwrap(),
            provider
        );
        assert_ne!(
            root.trace_id,
            TraceContext::for_task("other-deployment", "goal", "task").trace_id
        );
    }
    #[test]
    fn malformed_trace_headers_fail_and_unknown_fields_are_explicit() {
        let good = TraceContext::for_task("deployment", "goal", "task");
        for parent in [
            good.traceparent().replace("00-", "ff-"),
            "00-00000000000000000000000000000000-0000000000000001-01".into(),
            good.traceparent().replace("-01", "-zz"),
            format!("{}\r\nInjected: 1", good.traceparent()),
        ] {
            assert!(TraceContext::parse(&parent, None).is_err());
        }
        for state in [
            "bad\nkey=value",
            "a=1,a=2",
            "a=value=bad",
            "0simple=value",
            "tenant@1system=value",
        ] {
            assert!(TraceContext::parse(&good.traceparent(), Some(state)).is_err());
        }
        let operator = Correlation::for_actor("deployment", "human");
        assert!(
            operator.task_id.is_none() && operator.agent_id.is_none() && operator.run_id.is_none()
        );
        operator.validate().unwrap();
    }
}
