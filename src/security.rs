//! Deterministic trust-boundary decisions. All mutations join the root replicated transaction.
use crate::{
    audit::{AuditEvent, CryptoSigner, Hash, Signed, content_hash},
    identity::{VerifiedOperator, WorkloadId, verify_workload_certificate},
    ledger::{Ledger, Limits, PriceTable, ReservationRequest},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum CapabilityKind {
    #[serde(rename = "tool.call")]
    ToolCall,
    #[serde(rename = "board.read")]
    BoardRead,
    #[serde(rename = "board.post")]
    BoardPost,
    #[serde(rename = "finding.read")]
    FindingRead,
    #[serde(rename = "finding.append")]
    FindingAppend,
    #[serde(rename = "task.delegate")]
    TaskDelegate,
    #[serde(rename = "budget.spend")]
    BudgetSpend,
    #[serde(rename = "policy.request")]
    PolicyRequest,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfirmationTier {
    C0,
    C1,
    C2,
    C3,
}
impl ConfirmationTier {
    fn raised(self) -> Self {
        match self {
            Self::C0 => Self::C0,
            Self::C1 => Self::C2,
            Self::C2 | Self::C3 => Self::C3,
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum IsolationTier {
    W,
    P,
    G,
    V,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum OperationClass {
    ReadOnly,
    Reversible,
    External,
    Destructive,
}
impl OperationClass {
    fn tier(self) -> ConfirmationTier {
        match self {
            Self::ReadOnly => ConfirmationTier::C0,
            Self::Reversible => ConfirmationTier::C1,
            Self::External => ConfirmationTier::C2,
            Self::Destructive => ConfirmationTier::C3,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ResourceSelector {
    Exact(String),
    Prefix(String),
    Any,
}
impl ResourceSelector {
    fn valid(&self) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(s) | Self::Prefix(s) => {
                !s.is_empty() && s.len() <= 1024 && !s.contains('*') && !s.contains("..")
            }
        }
    }
    pub fn matches(&self, resource: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(s) => s == resource,
            Self::Prefix(s) => {
                s == resource
                    || resource
                        .strip_prefix(s)
                        .is_some_and(|rest| rest.starts_with('/'))
            }
        }
    }
    pub fn narrower_than(&self, parent: &Self) -> bool {
        match (self, parent) {
            (_, Self::Any) => true,
            (Self::Any, _) => false,
            (Self::Exact(c), p) => p.matches(c),
            (Self::Prefix(c), Self::Prefix(p)) => {
                c == p || c.strip_prefix(p).is_some_and(|r| r.starts_with('/'))
            }
            _ => false,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArgumentConstraint {
    Equal { value: Value },
    OneOf { values: Vec<Value> },
    Integer { minimum: i64, maximum: i64 },
    Prefix { value: String },
}
impl ArgumentConstraint {
    fn accepts(&self, value: &Value) -> bool {
        match self {
            Self::Equal { value: expected } => value == expected,
            Self::OneOf { values } => values.contains(value),
            Self::Integer { minimum, maximum } => value
                .as_i64()
                .is_some_and(|n| minimum <= &n && &n <= maximum),
            Self::Prefix { value: prefix } => value.as_str().is_some_and(|v| v.starts_with(prefix)),
        }
    }
    fn narrower_than(&self, parent: &Self) -> bool {
        match (self, parent) {
            (Self::Equal { value }, p) => p.accepts(value),
            (Self::OneOf { values }, p) => {
                !values.is_empty() && values.iter().all(|v| p.accepts(v))
            }
            (
                Self::Integer {
                    minimum: a,
                    maximum: b,
                },
                Self::Integer {
                    minimum: c,
                    maximum: d,
                },
            ) => a >= c && b <= d && a <= b,
            (Self::Prefix { value: a }, Self::Prefix { value: b }) => a.starts_with(b),
            _ => false,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Constraints {
    pub operations: BTreeSet<String>,
    pub resources: BTreeSet<String>,
    pub recipients: BTreeSet<String>,
    pub read_markings: BTreeSet<String>,
    pub max_payload_bytes: u64,
    pub arguments: BTreeMap<String, ArgumentConstraint>,
    pub deny_unknown_arguments: bool,
}
impl Constraints {
    fn narrower_than(&self, parent: &Self) -> bool {
        self.operations.is_subset(&parent.operations)
            && self.resources.is_subset(&parent.resources)
            && self.recipients.is_subset(&parent.recipients)
            && self.read_markings.is_subset(&parent.read_markings)
            && self.max_payload_bytes <= parent.max_payload_bytes
            && (!parent.deny_unknown_arguments || self.deny_unknown_arguments)
            && parent
                .arguments
                .iter()
                .all(|(name, p)| self.arguments.get(name).is_some_and(|c| c.narrower_than(p)))
            && (!parent.deny_unknown_arguments
                || self
                    .arguments
                    .keys()
                    .all(|name| parent.arguments.contains_key(name)))
    }
    fn check(&self, action: &Action) -> Result<()> {
        ensure!(
            self.operations.contains(&action.operation)
                && self.resources.contains(&action.resource),
            "operation/resource outside narrowed constraints"
        );
        ensure!(
            crate::audit::canonical(&action.arguments)?.len() as u64 <= self.max_payload_bytes,
            "payload exceeds Capability limit"
        );
        let args = action
            .arguments
            .as_object()
            .context("tool arguments must be an object")?;
        for (name, constraint) in &self.arguments {
            ensure!(
                args.get(name).is_some_and(|v| constraint.accepts(v)),
                "argument outside Capability constraints"
            );
        }
        if self.deny_unknown_arguments {
            ensure!(
                args.keys().all(|k| self.arguments.contains_key(k)),
                "unknown tool argument"
            );
        }
        if let Some(recipient) = &action.recipient {
            ensure!(
                self.recipients.contains(recipient),
                "recipient outside Capability constraints"
            );
        }
        Ok(())
    }
}
fn limits_narrower(child: &Limits, parent: &Limits) -> bool {
    child.total <= parent.total
        && child.per_call <= parent.per_call
        && child.per_tick <= parent.per_tick
        && child.per_day <= parent.per_day
        && child.tick_seconds == parent.tick_seconds
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub id: String,
    pub kind: CapabilityKind,
    pub resource: ResourceSelector,
    pub constraints: Constraints,
    pub budget: Limits,
    pub confirmation: ConfirmationTier,
    pub minimum_isolation: IsolationTier,
    pub not_before: u64,
    pub expiry: u64,
    pub goal_id: String,
    pub holder: String,
    pub parent: Option<String>,
    pub depth: u8,
    pub grant_version: u64,
}
impl Capability {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty()
                && self.resource.valid()
                && self.not_before < self.expiry
                && !self.goal_id.is_empty()
                && !self.holder.is_empty()
                && self.grant_version > 0,
            "incomplete Capability declaration"
        );
        ensure!(
            !self.constraints.operations.is_empty()
                && !self.constraints.resources.is_empty()
                && self.constraints.max_payload_bytes > 0
                && self
                    .constraints
                    .resources
                    .iter()
                    .all(|r| self.resource.matches(r)),
            "invalid/empty Capability constraints"
        );
        ensure!(
            self.kind != CapabilityKind::BudgetSpend || self.budget.validate().is_ok(),
            "spend Capability requires finite nonzero caps"
        );
        Ok(())
    }
    pub fn attenuates(&self, parent: &Self) -> bool {
        self.kind == parent.kind
            && self.resource.narrower_than(&parent.resource)
            && self.constraints.narrower_than(&parent.constraints)
            && limits_narrower(&self.budget, &parent.budget)
            && self.confirmation >= parent.confirmation
            && self.minimum_isolation >= parent.minimum_isolation
            && self.not_before >= parent.not_before
            && self.expiry <= parent.expiry
            && self.goal_id == parent.goal_id
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum Scope {
    Global,
    Tenant {
        tenant: String,
    },
    Goal {
        tenant: String,
        goal: String,
    },
    Agent {
        tenant: String,
        goal: String,
        holder: String,
    },
}
impl Scope {
    pub fn contains(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Global, _) => true,
            (
                Self::Tenant { tenant: a },
                Self::Tenant { tenant: b }
                | Self::Goal { tenant: b, .. }
                | Self::Agent { tenant: b, .. },
            ) => a == b,
            (
                Self::Goal { tenant: a, goal: g },
                Self::Goal { tenant: b, goal: h }
                | Self::Agent {
                    tenant: b, goal: h, ..
                },
            ) => a == b && g == h,
            (
                Self::Agent {
                    tenant: a,
                    goal: g,
                    holder: x,
                },
                Self::Agent {
                    tenant: b,
                    goal: h,
                    holder: y,
                },
            ) => a == b && g == h && x == y,
            _ => false,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Abac {
    pub not_before: u64,
    pub expiry: u64,
    pub max_cost: u64,
    pub max_confirmation: ConfirmationTier,
    pub minimum_isolation: IsolationTier,
    pub resources: BTreeSet<String>,
    pub operations: BTreeSet<String>,
}
impl Abac {
    fn permits(
        &self,
        verb: &str,
        resource: &str,
        cost: u64,
        tier: ConfirmationTier,
        isolation: IsolationTier,
        now: u64,
    ) -> bool {
        self.not_before <= now
            && now < self.expiry
            && cost <= self.max_cost
            && tier <= self.max_confirmation
            && isolation >= self.minimum_isolation
            && self.resources.contains(resource)
            && self.operations.contains(verb)
    }
    fn narrower_than(&self, p: &Self) -> bool {
        self.not_before >= p.not_before
            && self.expiry <= p.expiry
            && self.max_cost <= p.max_cost
            && self.max_confirmation <= p.max_confirmation
            && self.minimum_isolation >= p.minimum_isolation
            && self.resources.is_subset(&p.resources)
            && self.operations.is_subset(&p.operations)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RoleRule {
    pub verbs: BTreeSet<String>,
    pub resources: BTreeSet<String>,
    pub scope: Scope,
    pub conditions: Abac,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub id: String,
    pub rules: Vec<RoleRule>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub id: String,
    pub role_ref: String,
    pub subjects: BTreeSet<String>,
    pub groups: BTreeSet<String>,
    pub scope: Scope,
    pub conditions: Abac,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuthenticatedSubject {
    id: String,
    groups: BTreeSet<String>,
    human: bool,
    expires: u64,
    auth_time: u64,
    workload: Option<WorkloadId>,
    certificate_hash: Option<Hash>,
}
impl AuthenticatedSubject {
    pub fn operator(operator: &VerifiedOperator) -> Self {
        Self {
            id: operator.id(),
            groups: operator.groups().clone(),
            human: true,
            expires: operator.expires_unix(),
            auth_time: operator.auth_time(),
            workload: None,
            certificate_hash: None,
        }
    }
    pub fn workload(leaf: &[u8], ca: &[u8], id: WorkloadId, now: u64) -> Result<Self> {
        verify_workload_certificate(leaf, ca, &id, now)?;
        let (_, cert) = x509_parser::parse_x509_certificate(leaf)
            .map_err(|_| anyhow::anyhow!("invalid workload certificate"))?;
        Ok(Self {
            id: id.uri()?,
            groups: BTreeSet::new(),
            human: false,
            expires: cert.validity().not_after.timestamp().try_into()?,
            auth_time: now,
            workload: Some(id),
            certificate_hash: Some(crate::audit::hash(leaf)),
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn certificate_hash(&self) -> Option<Hash> {
        self.certificate_hash
    }
    pub fn ensure_current(&self, now: u64) -> Result<()> {
        self.valid(now)
    }
    pub fn is_human(&self) -> bool {
        self.human
    }
    fn valid(&self, now: u64) -> Result<()> {
        ensure!(now < self.expires, "authentication expired");
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TargetPermission {
    pub target: String,
    pub tenant: String,
    pub owner: String,
    pub operations: BTreeSet<String>,
    pub permission_document_hash: Hash,
    pub third_party: bool,
    pub not_before: u64,
    pub expiry: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HumanApproval {
    pub subject: String,
    pub groups: BTreeSet<String>,
    pub authority_epoch: u64,
    pub action_hash: Hash,
    pub evidence_hash: Hash,
    pub grant_version: u64,
    pub scope: Scope,
    pub issued: u64,
    pub expiry: u64,
    pub auth_time: u64,
    pub human: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GoalQa {
    pub goal_hash: Hash,
    pub inventory_hash: Hash,
    pub passed: bool,
    pub expiry: u64,
    pub qa_version: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaintExemption {
    pub kind: CapabilityKind,
    pub resource: ResourceSelector,
    pub operations: BTreeSet<String>,
    pub source_types: BTreeSet<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalPosture {
    pub id: String,
    pub version: u64,
    pub contract_hash: Hash,
    pub evaluator_public_key: Hash,
    pub tenant: String,
    pub intent: String,
    pub resources: BTreeSet<String>,
    pub classes: BTreeSet<OperationClass>,
    pub target_permissions: BTreeMap<String, Signed<TargetPermission>>,
    pub capability_ceiling: Vec<Capability>,
    pub limits: Limits,
    pub not_before: u64,
    pub expiry: u64,
    pub max_depth: u8,
    pub max_live_descendants: u64,
    pub max_open_tasks: u64,
    pub unattended_c2: bool,
    pub qa: Option<Signed<GoalQa>>,
    pub taint_exemptions: Vec<TaintExemption>,
}
/// Bind the complete typed Goal contract while avoiding its nested security posture.
pub fn goal_contract_hash<T: Serialize>(spec: &T) -> Result<Hash> {
    let mut value = serde_json::to_value(spec)?;
    let object = value
        .as_object_mut()
        .context("Goal contract must be an object")?;
    ensure!(
        object.contains_key("security"),
        "Goal contract lacks security field"
    );
    object.insert("security".into(), Value::Null);
    content_hash(&value)
}
pub fn bind_goal_contract<T: Serialize>(posture: &mut GoalPosture, spec: &T) -> Result<()> {
    let value = serde_json::to_value(spec)?;
    ensure!(
        value["goal_id"] == posture.id
            && value["intent"] == posture.intent
            && value["version"].as_u64() == Some(posture.version),
        "Goal contract identity/intent/version differs"
    );
    posture.contract_hash = goal_contract_hash(spec)?;
    Ok(())
}
impl GoalPosture {
    pub fn registration_hash(&self) -> Result<Hash> {
        let mut copy = self.clone();
        copy.qa = None;
        content_hash(&copy)
    }
    fn policy_scopes(&self) -> Result<Vec<Scope>> {
        let mut result = vec![];
        for cap in self.capability_ceiling.iter().filter(|c| {
            c.kind == CapabilityKind::PolicyRequest
                && (c.constraints.operations.contains("stop")
                    || c.constraints.operations.contains("flag"))
        }) {
            let values = match cap
                .constraints
                .arguments
                .get("scope")
                .context("policy intervention requires explicit scope constraints")?
            {
                ArgumentConstraint::Equal { value } => vec![value.clone()],
                ArgumentConstraint::OneOf { values } => values.clone(),
                _ => anyhow::bail!("policy scope must be an exact finite list"),
            };
            ensure!(
                !values.is_empty() && cap.constraints.arguments.contains_key("trigger"),
                "policy intervention scope/trigger missing"
            );
            for value in values {
                let scope: Scope = serde_json::from_value(value)?;
                match &scope {
                    Scope::Global => {}
                    Scope::Tenant { tenant } => {
                        ensure!(tenant == &self.tenant, "cross-tenant policy intervention")
                    }
                    Scope::Goal { tenant, goal } | Scope::Agent { tenant, goal, .. } => ensure!(
                        tenant == &self.tenant && goal == &self.id,
                        "cross-Goal policy intervention"
                    ),
                };
                result.push(scope);
            }
        }
        Ok(result)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentPolicy {
    pub version: String,
    pub tenant: String,
    pub classification: String,
    pub not_before: u64,
    pub expiry: u64,
    pub inventory: BTreeSet<String>,
    pub classes: BTreeSet<OperationClass>,
    pub global_limits: Limits,
    pub tenant_limits: Limits,
    pub max_depth: u8,
    pub max_live_descendants: u64,
    pub max_open_tasks: u64,
    pub high_impact_spend_threshold: u64,
    pub step_up_max_age: u64,
    pub approval_signers: BTreeSet<Hash>,
    pub price_signers: BTreeSet<Hash>,
    pub target_signers: BTreeSet<Hash>,
    pub qa_signers: BTreeSet<Hash>,
    pub provenance_signer: Hash,
    pub permit_signer: Hash,
    pub recipient_clearances: BTreeMap<String, BTreeSet<String>>,
    pub trusted_components: BTreeSet<String>,
    pub root_humans: BTreeSet<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub not_before: u64,
    pub expiry: u64,
    pub fencing_token: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDefinition {
    pub id: String,
    pub holder: WorkloadId,
    pub goal_id: String,
    pub grant_version: u64,
    pub capabilities: Vec<Capability>,
    pub lease: Lease,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub id: String,
    pub holder: WorkloadId,
    pub goal_id: String,
    pub version: u64,
    pub definition_hash: Hash,
    pub capabilities: Vec<Capability>,
    pub parent: Option<String>,
    pub lease: Lease,
    pub revoked: bool,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trust {
    Untrusted,
    Corroborated,
    Trusted,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRecord {
    pub id: String,
    pub source_type: String,
    pub hash: Hash,
    pub goal_id: String,
    pub producer: String,
    pub trust: Trust,
    pub markings: BTreeSet<String>,
    pub parents: BTreeSet<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub id: String,
    pub producer: String,
    pub goal_id: String,
    pub grant_version: u64,
    pub result_hash: Hash,
    pub inputs: BTreeSet<String>,
    pub trust: Trust,
    pub markings: BTreeSet<String>,
    pub timestamp: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retraction {
    pub finding_id: String,
    pub actor: String,
    pub reason_hash: Hash,
    pub timestamp: u64,
    pub epoch: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stop {
    pub scope: Scope,
    pub engaged: bool,
    pub actor: String,
    pub reason_hash: Hash,
    pub epoch: u64,
    pub timestamp: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub kind: CapabilityKind,
    pub resource: String,
    pub operation: String,
    pub arguments: Value,
    pub class: OperationClass,
    pub isolation: IsolationTier,
    pub recipient: Option<String>,
    pub evidence: BTreeSet<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    pub intent_id: String,
    pub grant_id: String,
    pub grant_version: u64,
    pub capability_id: String,
    pub budget_capability_id: Option<String>,
    pub envelope_id: Option<String>,
    pub usage_bound: BTreeMap<String, u64>,
    pub action: Action,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyIntervention {
    pub scope: Scope,
    pub trigger: String,
    pub reason_hash: Hash,
    pub confidence_milli: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DispatchPermit {
    pub intent_id: String,
    pub holder: String,
    pub certificate_hash: Option<Hash>,
    pub goal_id: String,
    pub grant_id: String,
    pub grant_version: u64,
    pub fencing_token: u64,
    pub action_hash: Hash,
    pub stop_epoch: u64,
    pub grant_epoch: u64,
    pub retraction_epoch: u64,
    pub evidence_hash: Hash,
    pub observed_hash: Hash,
    pub price_version: Option<String>,
    pub confirmation: ConfirmationTier,
    pub issued: u64,
    pub expiry: u64,
    pub billable: bool,
}
/// Exact-action review data derived only from current compiled authority and observed lineage.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfirmationReview {
    pub intent_id: String,
    pub holder: String,
    pub certificate_hash: Option<Hash>,
    pub grant_id: String,
    pub grant_version: u64,
    pub action_hash: Hash,
    pub evidence_hash: Hash,
    pub scope: Scope,
    pub tier: ConfirmationTier,
    pub required_humans: usize,
    pub expiry: u64,
    pub grant_epoch: u64,
    pub stop_epoch: u64,
    pub retraction_epoch: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    pub permit: Signed<DispatchPermit>,
    pub audit: AuditEvent,
}
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // Bounded permit/review DTOs stay intact for the caller's atomic application.
pub enum NonbillablePermitRefresh {
    Ready {
        permit: Signed<DispatchPermit>,
        audit: Option<AuditEvent>,
    },
    ConfirmationRequired(ConfirmationReview),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityState {
    pub policy: Signed<DeploymentPolicy>,
    pub roles: BTreeMap<String, Role>,
    pub bindings: BTreeMap<String, Binding>,
    pub goals: BTreeMap<String, GoalPosture>,
    pub grants: BTreeMap<String, Grant>,
    pub stops: Vec<Stop>,
    pub stop_epoch: u64,
    pub grant_epoch: u64,
    pub retraction_epoch: u64,
    #[serde(default)]
    pub goal_revisions: BTreeMap<String, u64>,
    #[serde(default)]
    pub goal_history: BTreeMap<String, Vec<GoalPosture>>,
    pub inputs: BTreeMap<String, InputRecord>,
    pub observed: BTreeMap<String, BTreeSet<String>>,
    pub findings: BTreeMap<String, Signed<Provenance>>,
    pub retractions: BTreeMap<String, Signed<Retraction>>,
    pending: BTreeMap<String, Signed<DispatchPermit>>,
    used: BTreeSet<String>,
}
impl SecurityState {
    pub fn new(policy: Signed<DeploymentPolicy>, trusted_key: &Hash, now: u64) -> Result<Self> {
        policy.verify("deployment-policy", trusted_key)?;
        let p = &policy.payload;
        p.global_limits.validate()?;
        p.tenant_limits.validate()?;
        ensure!(
            p.not_before <= now
                && now < p.expiry
                && p.max_depth <= 4
                && p.max_live_descendants > 0
                && p.max_live_descendants <= 500
                && p.max_open_tasks > 0
                && p.max_open_tasks <= 500
                && !p.root_humans.is_empty()
                && !p.inventory.is_empty()
                && !p.classes.is_empty()
                && p.step_up_max_age > 0,
            "incomplete deployment security policy"
        );
        Ok(Self {
            policy,
            roles: BTreeMap::new(),
            bindings: BTreeMap::new(),
            goals: BTreeMap::new(),
            grants: BTreeMap::new(),
            stops: vec![],
            stop_epoch: 0,
            grant_epoch: 0,
            retraction_epoch: 0,
            goal_revisions: BTreeMap::new(),
            goal_history: BTreeMap::new(),
            inputs: BTreeMap::new(),
            observed: BTreeMap::new(),
            findings: BTreeMap::new(),
            retractions: BTreeMap::new(),
            pending: BTreeMap::new(),
            used: BTreeSet::new(),
        })
    }
    fn policy_live(&self, now: u64) -> Result<()> {
        ensure!(
            self.policy.payload.not_before <= now && now < self.policy.payload.expiry,
            "deployment policy expired"
        );
        Ok(())
    }
    fn root(&self, subject: &AuthenticatedSubject) -> bool {
        subject.human && self.policy.payload.root_humans.contains(subject.id())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn permitted(
        &self,
        subject: &AuthenticatedSubject,
        verb: &str,
        resource: &str,
        scope: &Scope,
        cost: u64,
        tier: ConfirmationTier,
        isolation: IsolationTier,
        now: u64,
    ) -> bool {
        if subject.valid(now).is_err() || self.policy_live(now).is_err() {
            return false;
        }
        if self.root(subject) {
            return true;
        }
        if !subject.human
            && !self
                .policy
                .payload
                .trusted_components
                .contains(subject.id())
        {
            return false;
        }
        self.bindings.values().any(|b| {
            (b.subjects.contains(subject.id()) || !b.groups.is_disjoint(&subject.groups))
                && b.scope.contains(scope)
                && b.conditions
                    .permits(verb, resource, cost, tier, isolation, now)
                && self.roles.get(&b.role_ref).is_some_and(|role| {
                    role.rules.iter().any(|r| {
                        r.scope.contains(scope)
                            && r.verbs.contains(verb)
                            && r.resources.contains(resource)
                            && r.conditions
                                .permits(verb, resource, cost, tier, isolation, now)
                    })
                })
        })
    }
    pub fn role_change_binding(&self, role: &Role) -> Result<(Hash, Hash, Scope, u64)> {
        let evidence = content_hash(&(self.roles.get(&role.id), &self.policy))?;
        Ok((
            content_hash(&("role-change", role, evidence, self.grant_epoch))?,
            evidence,
            Scope::Global,
            self.grant_epoch,
        ))
    }
    pub fn binding_change_binding(&self, binding: &Binding) -> Result<(Hash, Hash, Scope, u64)> {
        let role = self.roles.get(&binding.role_ref).context("unknown role")?;
        let evidence = content_hash(&(self.bindings.get(&binding.id), role, &self.policy))?;
        Ok((
            content_hash(&("binding-change", binding, evidence, self.grant_epoch))?,
            evidence,
            binding.scope.clone(),
            self.grant_epoch,
        ))
    }
    pub fn create_role(
        &mut self,
        subject: &AuthenticatedSubject,
        role: Role,
        approvals: &[Signed<HumanApproval>],
        now: u64,
    ) -> Result<()> {
        ensure!(
            !role.id.is_empty() && !role.rules.is_empty(),
            "invalid role"
        );
        ensure!(
            self.permitted(
                subject,
                "create",
                "roles",
                &Scope::Global,
                0,
                ConfirmationTier::C3,
                IsolationTier::P,
                now
            ),
            "role creation denied"
        );
        self.step_up(subject, now)?;
        let (hash, evidence, scope, version) = self.role_change_binding(&role)?;
        ensure!(
            self.approvers(approvals, &hash, &evidence, version, &scope, now)?
                .len()
                >= 2,
            "role changes require two distinct exact-action human approvals"
        );
        let escalate = self.permitted(
            subject,
            "escalate",
            "roles",
            &Scope::Global,
            0,
            ConfirmationTier::C3,
            IsolationTier::P,
            now,
        );
        for rule in &role.rules {
            ensure!(
                !rule.verbs.is_empty()
                    && !rule.resources.is_empty()
                    && !rule.verbs.contains("*")
                    && !rule.resources.contains("*"),
                "wildcard/empty privileged role"
            );
            if !escalate {
                for verb in &rule.verbs {
                    for resource in &rule.resources {
                        ensure!(
                            self.rule_covered(subject, verb, resource, rule),
                            "role escalation denied"
                        );
                    }
                }
            }
        }
        self.roles.insert(role.id.clone(), role);
        self.grant_epoch = self
            .grant_epoch
            .checked_add(1)
            .context("authority epoch overflow")?;
        Ok(())
    }
    fn rule_covered(
        &self,
        subject: &AuthenticatedSubject,
        verb: &str,
        resource: &str,
        candidate: &RoleRule,
    ) -> bool {
        self.bindings.values().any(|b| {
            (b.subjects.contains(subject.id()) || !b.groups.is_disjoint(&subject.groups))
                && b.scope.contains(&candidate.scope)
                && candidate.conditions.narrower_than(&b.conditions)
                && self.roles.get(&b.role_ref).is_some_and(|role| {
                    role.rules.iter().any(|r| {
                        r.scope.contains(&candidate.scope)
                            && r.verbs.contains(verb)
                            && r.resources.contains(resource)
                            && candidate.conditions.narrower_than(&r.conditions)
                    })
                })
        })
    }
    pub fn bind_role(
        &mut self,
        subject: &AuthenticatedSubject,
        binding: Binding,
        approvals: &[Signed<HumanApproval>],
        now: u64,
    ) -> Result<()> {
        ensure!(
            self.permitted(
                subject,
                "create",
                "bindings",
                &binding.scope,
                0,
                ConfirmationTier::C3,
                IsolationTier::P,
                now
            ),
            "binding creation denied"
        );
        self.step_up(subject, now)?;
        let (hash, evidence, scope, version) = self.binding_change_binding(&binding)?;
        ensure!(
            self.approvers(approvals, &hash, &evidence, version, &scope, now)?
                .len()
                >= 2,
            "binding changes require two distinct exact-action human approvals"
        );
        let role = self.roles.get(&binding.role_ref).context("unknown role")?;
        if let Some(old) = self.bindings.get(&binding.id) {
            ensure!(old.role_ref == binding.role_ref, "immutable role reference");
        }
        for id in &binding.subjects {
            if id.starts_with("spiffe://") {
                ensure!(
                    self.policy.payload.trusted_components.contains(id),
                    "agents cannot hold RBAC roles"
                );
            }
        }
        let explicit = self.permitted(
            subject,
            "bind",
            "roles",
            &binding.scope,
            0,
            ConfirmationTier::C3,
            IsolationTier::P,
            now,
        );
        ensure!(
            explicit
                || role.rules.iter().all(|r| r.verbs.iter().all(|v| r
                    .resources
                    .iter()
                    .all(|resource| self.rule_covered(subject, v, resource, r)))),
            "binding escalation denied"
        );
        ensure!(
            !binding.id.is_empty()
                && (!binding.subjects.is_empty() || !binding.groups.is_empty())
                && binding.conditions.not_before < binding.conditions.expiry,
            "invalid binding"
        );
        self.bindings.insert(binding.id.clone(), binding);
        self.grant_epoch = self
            .grant_epoch
            .checked_add(1)
            .context("authority epoch overflow")?;
        Ok(())
    }
    fn step_up(&self, subject: &AuthenticatedSubject, now: u64) -> Result<()> {
        subject.valid(now)?;
        ensure!(
            subject.human
                && subject.auth_time <= now
                && now - subject.auth_time <= self.policy.payload.step_up_max_age,
            "fresh human step-up required"
        );
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn issue_approval(
        &self,
        operator: &VerifiedOperator,
        action_hash: Hash,
        evidence_hash: Hash,
        grant_version: u64,
        scope: Scope,
        expiry: u64,
        signer: &CryptoSigner,
        now: u64,
    ) -> Result<Signed<HumanApproval>> {
        let subject = AuthenticatedSubject::operator(operator);
        self.step_up(&subject, now)?;
        ensure!(
            self.permitted(
                &subject,
                "approve",
                "actions",
                &scope,
                0,
                ConfirmationTier::C3,
                IsolationTier::P,
                now
            ),
            "approval authority denied"
        );
        ensure!(
            self.policy
                .payload
                .approval_signers
                .contains(&signer.public_key())
                && now < expiry
                && expiry <= operator.expires_unix(),
            "approval signer/expiry invalid"
        );
        signer.sign(
            "human-approval",
            HumanApproval {
                subject: subject.id,
                groups: subject.groups,
                authority_epoch: self.grant_epoch,
                action_hash,
                evidence_hash,
                grant_version,
                scope,
                issued: now,
                expiry,
                auth_time: operator.auth_time(),
                human: true,
            },
        )
    }
    fn approvers(
        &self,
        approvals: &[Signed<HumanApproval>],
        action_hash: &Hash,
        evidence_hash: &Hash,
        grant_version: u64,
        scope: &Scope,
        now: u64,
    ) -> Result<BTreeSet<String>> {
        let mut subjects = BTreeSet::new();
        for approval in approvals {
            ensure!(
                self.policy
                    .payload
                    .approval_signers
                    .contains(&approval.signer),
                "untrusted approval gateway"
            );
            approval.verify("human-approval", &approval.signer)?;
            let a = &approval.payload;
            ensure!(
                a.human
                    && a.authority_epoch == self.grant_epoch
                    && a.action_hash == *action_hash
                    && a.evidence_hash == *evidence_hash
                    && a.grant_version == grant_version
                    && a.scope.contains(scope)
                    && a.issued <= now
                    && now < a.expiry
                    && a.auth_time <= a.issued
                    && a.issued - a.auth_time <= self.policy.payload.step_up_max_age,
                "invalid/stale/exact-action approval"
            );
            ensure!(
                a.auth_time <= now && now - a.auth_time <= self.policy.payload.step_up_max_age,
                "approval step-up is no longer fresh at consumption"
            );
            let subject = AuthenticatedSubject {
                id: a.subject.clone(),
                groups: a.groups.clone(),
                human: true,
                expires: a.expiry,
                auth_time: a.auth_time,
                workload: None,
                certificate_hash: None,
            };
            ensure!(
                self.permitted(
                    &subject,
                    "approve",
                    "actions",
                    scope,
                    0,
                    ConfirmationTier::C3,
                    IsolationTier::P,
                    now
                ),
                "approver no longer privileged in this scope"
            );
            subjects.insert(a.subject.clone());
        }
        Ok(subjects)
    }
    fn validate_posture(&self, goal: &GoalPosture, now: u64) -> Result<Hash> {
        let p = &self.policy.payload;
        ensure!(
            !goal.id.is_empty()
                && goal.version > 0
                && goal.contract_hash != [0; 32]
                && goal.evaluator_public_key != [0; 32]
                && !goal.intent.is_empty()
                && goal.tenant == p.tenant
                && goal.resources.is_subset(&p.inventory)
                && !goal.resources.is_empty()
                && goal.classes.is_subset(&p.classes)
                && !goal.classes.is_empty(),
            "Goal outside deployment policy"
        );
        ensure!(
            !ed25519_dalek::VerifyingKey::from_bytes(&goal.evaluator_public_key)?.is_weak(),
            "evaluator key must be a valid independent Ed25519 verification key"
        );
        goal.limits.validate()?;
        ensure!(
            limits_narrower(&goal.limits, &p.tenant_limits)
                && goal.not_before <= now
                && now < goal.expiry
                && goal.expiry <= p.expiry
                && goal.max_depth <= p.max_depth
                && goal.max_live_descendants > 0
                && goal.max_live_descendants <= p.max_live_descendants
                && goal.max_open_tasks > 0
                && goal.max_open_tasks <= p.max_open_tasks,
            "Goal caps/expiry invalid"
        );
        for (target, permission) in &goal.target_permissions {
            ensure!(
                p.target_signers.contains(&permission.signer),
                "unknown target authority"
            );
            permission.verify("target-permission", &permission.signer)?;
            let auth = &permission.payload;
            ensure!(
                auth.target == *target
                    && auth.tenant == goal.tenant
                    && goal.resources.contains(target)
                    && !auth.owner.is_empty()
                    && !auth.operations.is_empty()
                    && auth.permission_document_hash != [0; 32]
                    && auth.not_before <= now
                    && goal.expiry <= auth.expiry,
                "target permission invalid"
            );
        }
        ensure!(
            goal.resources
                .iter()
                .filter(|r| r.starts_with("target/"))
                .all(|r| goal.target_permissions.contains_key(r)),
            "target lacks recorded ownership/engagement permission"
        );
        for cap in &goal.capability_ceiling {
            cap.validate()?;
            ensure!(
                cap.goal_id == goal.id
                    && cap.grant_version == goal.version
                    && cap.constraints.resources.is_subset(&goal.resources)
                    && cap.expiry <= goal.expiry,
                "Capability ceiling outside Goal"
            );
        }
        ensure!(
            !goal.capability_ceiling.is_empty(),
            "missing agent authority definition"
        );
        goal.policy_scopes()?;
        let hash = goal.registration_hash()?;
        if goal.unattended_c2 {
            let qa = goal
                .qa
                .as_ref()
                .context("unattended mode requires linked real QA")?;
            ensure!(p.qa_signers.contains(&qa.signer), "unknown QA signer");
            qa.verify("goal-qa", &qa.signer)?;
            ensure!(
                qa.payload.passed
                    && qa.payload.goal_hash == hash
                    && qa.payload.inventory_hash == content_hash(&goal.resources)?
                    && now < qa.payload.expiry
                    && !qa.payload.qa_version.is_empty(),
                "QA scope/version mismatch; release result is not Goal QA"
            );
        }
        Ok(hash)
    }
    pub fn register_goal(
        &mut self,
        subject: &AuthenticatedSubject,
        goal: GoalPosture,
        approvals: &[Signed<HumanApproval>],
        ledger: &mut Ledger,
        now: u64,
    ) -> Result<()> {
        self.policy_live(now)?;
        subject.valid(now)?;
        let p = &self.policy.payload;
        let scope = Scope::Tenant {
            tenant: goal.tenant.clone(),
        };
        ensure!(
            subject.human
                && self.permitted(
                    subject,
                    "register",
                    "goals",
                    &scope,
                    goal.limits.total,
                    ConfirmationTier::C2,
                    IsolationTier::P,
                    now
                ),
            "only authorized human may admit a Goal"
        );
        ensure!(
            !self.goals.contains_key(&goal.id),
            "Goal identity already admitted"
        );
        ensure!(goal.version == 1, "initial Goal version must be one");
        let hash = self.validate_posture(&goal, now)?;
        let policy_scopes = goal.policy_scopes()?;
        ensure!(
            !policy_scopes.iter().any(|s| matches!(s, Scope::Global)) || self.root(subject),
            "global policy intervention requires root admission"
        );
        let signers = self.approvers(approvals, &hash, &[0; 32], 0, &scope, now)?;
        let high = policy_scopes
            .iter()
            .any(|s| matches!(s, Scope::Global | Scope::Tenant { .. }))
            || goal
                .target_permissions
                .values()
                .any(|p| p.payload.third_party)
            || goal.limits.total > p.high_impact_spend_threshold
            || goal.classes.contains(&OperationClass::Destructive)
            || goal
                .capability_ceiling
                .iter()
                .any(|c| c.confirmation == ConfirmationTier::C3);
        ensure!(
            signers.len() >= if high { 2 } else { 1 },
            "registration lacks distinct human approvals"
        );
        if !ledger.accounts.contains_key("global") {
            ledger.add_account("global".into(), p.global_limits.clone())?;
        }
        let tenant_account = format!("tenant/{}", goal.tenant);
        if !ledger.accounts.contains_key(&tenant_account) {
            ledger.add_account(tenant_account, p.tenant_limits.clone())?;
        }
        ledger.add_account(format!("goal/{}", goal.id), goal.limits.clone())?;
        self.goal_revisions.insert(goal.id.clone(), 1);
        self.goals.insert(goal.id.clone(), goal);
        Ok(())
    }
    pub fn amendment_binding(&self, goal: &GoalPosture) -> Result<(Hash, Hash, Scope, u64)> {
        let old = self.goals.get(&goal.id).context("unknown amendment Goal")?;
        ensure!(
            goal.version
                == old
                    .version
                    .checked_add(1)
                    .context("Goal version exhausted")?,
            "Goal amendment must advance exactly one version"
        );
        ensure!(
            goal.tenant == old.tenant,
            "amendment cannot change tenant identity"
        );
        let version = *self.goal_revisions.get(&goal.id).unwrap_or(&1);
        Ok((
            content_hash(&(
                "amend-goal",
                old.registration_hash()?,
                goal.registration_hash()?,
                version,
            ))?,
            old.registration_hash()?,
            Scope::Goal {
                tenant: goal.tenant.clone(),
                goal: goal.id.clone(),
            },
            version,
        ))
    }
    pub fn amend_goal(
        &mut self,
        subject: &AuthenticatedSubject,
        goal: GoalPosture,
        approvals: &[Signed<HumanApproval>],
        ledger: &mut Ledger,
        now: u64,
    ) -> Result<()> {
        self.policy_live(now)?;
        self.validate_posture(&goal, now)?;
        ensure!(
            !goal
                .policy_scopes()?
                .iter()
                .any(|s| matches!(s, Scope::Global))
                || self.root(subject),
            "global policy intervention requires root amendment"
        );
        let (hash, evidence, scope, version) = self.amendment_binding(&goal)?;
        ensure!(
            subject.human
                && self.permitted(
                    subject,
                    "amend",
                    "goals",
                    &scope,
                    goal.limits.total,
                    ConfirmationTier::C3,
                    IsolationTier::P,
                    now
                ),
            "Goal amendment denied"
        );
        self.step_up(subject, now)?;
        ensure!(
            self.approvers(approvals, &hash, &evidence, version, &scope, now)?
                .len()
                >= 2,
            "Goal amendment requires two distinct privileged humans"
        );
        let next_version = version.checked_add(1).context("Goal revision exhausted")?;
        let next_epoch = self
            .grant_epoch
            .checked_add(1)
            .context("authority epoch overflow")?;
        // Stage all money mutations before changing authority; callers commit both together.
        let mut staged = ledger.clone();
        staged.retire_goal_allocations(&goal.id, hash)?;
        staged.amend_account(&format!("goal/{}", goal.id), goal.limits.clone())?;
        staged.validate()?;
        for grant in self.grants.values_mut().filter(|g| g.goal_id == goal.id) {
            grant.revoked = true;
            grant.lease.fencing_token = grant.lease.fencing_token.saturating_add(1);
        }
        let old = self.goals.insert(goal.id.clone(), goal.clone()).unwrap();
        self.goal_history
            .entry(goal.id.clone())
            .or_default()
            .push(old);
        self.goal_revisions.insert(goal.id, next_version);
        self.grant_epoch = next_epoch;
        *ledger = staged;
        Ok(())
    }
    pub fn policy_install_binding(
        &self,
        policy: &Signed<DeploymentPolicy>,
    ) -> Result<(Hash, Hash, Scope, u64)> {
        Ok((
            content_hash(&(
                "install-policy",
                content_hash(&self.policy)?,
                content_hash(policy)?,
                self.grant_epoch,
            ))?,
            content_hash(&self.policy)?,
            Scope::Global,
            self.grant_epoch,
        ))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn authorize_configuration_change(
        &self,
        subject: &AuthenticatedSubject,
        action_hash: Hash,
        evidence_hash: Hash,
        version: u64,
        scope: Scope,
        approvals: &[Signed<HumanApproval>],
        now: u64,
    ) -> Result<()> {
        ensure!(
            version == self.grant_epoch
                && self.permitted(
                    subject,
                    "amend",
                    "policy",
                    &scope,
                    0,
                    ConfirmationTier::C3,
                    IsolationTier::P,
                    now
                ),
            "configuration change authority/version denied"
        );
        self.step_up(subject, now)?;
        ensure!(
            self.approvers(
                approvals,
                &action_hash,
                &evidence_hash,
                version,
                &scope,
                now
            )?
            .len()
                >= 2,
            "configuration change requires two distinct privileged humans"
        );
        Ok(())
    }
    pub fn install_policy(
        &mut self,
        subject: &AuthenticatedSubject,
        policy: Signed<DeploymentPolicy>,
        approvals: &[Signed<HumanApproval>],
        ledger: &mut Ledger,
        now: u64,
    ) -> Result<()> {
        ensure!(
            self.root(subject)
                && self.permitted(
                    subject,
                    "amend",
                    "policy",
                    &Scope::Global,
                    0,
                    ConfirmationTier::C3,
                    IsolationTier::P,
                    now
                ),
            "deployment policy change requires root human authority"
        );
        self.step_up(subject, now)?;
        let (hash, evidence, scope, version) = self.policy_install_binding(&policy)?;
        ensure!(
            self.approvers(approvals, &hash, &evidence, version, &scope, now)?
                .len()
                >= 2,
            "policy change requires two distinct privileged humans"
        );
        Self::new(policy.clone(), &self.policy.signer, now)?;
        ensure!(
            policy.payload.version != self.policy.payload.version
                && policy.payload.tenant == self.policy.payload.tenant,
            "policy version/tenant invalid"
        );
        let next_epoch = self
            .grant_epoch
            .checked_add(1)
            .context("authority epoch overflow")?;
        self.stop_epoch
            .checked_add(self.goals.len().try_into().context("Goal count overflow")?)
            .context("stop epoch overflow")?;
        let mut staged = ledger.clone();
        for goal in self.goals.keys() {
            staged.retire_goal_allocations(goal, hash)?;
        }
        if staged.accounts.contains_key("global") {
            staged.amend_account("global", policy.payload.global_limits.clone())?;
        }
        let tenant = format!("tenant/{}", policy.payload.tenant);
        if staged.accounts.contains_key(&tenant) {
            staged.amend_account(&tenant, policy.payload.tenant_limits.clone())?;
        }
        staged.validate()?;
        for grant in self.grants.values_mut() {
            grant.revoked = true;
            grant.lease.fencing_token = grant.lease.fencing_token.saturating_add(1);
        }
        self.policy = policy;
        self.grant_epoch = next_epoch;
        *ledger = staged;
        let goals: Vec<_> = self
            .goals
            .values()
            .map(|g| Scope::Goal {
                tenant: g.tenant.clone(),
                goal: g.id.clone(),
            })
            .collect();
        for scope in goals {
            self.set_stop(scope, true, subject.id.clone(), hash, now)?;
        }
        Ok(())
    }
    pub fn compile_definition(
        &mut self,
        definition: AgentDefinition,
        parent: Option<String>,
        ledger: &mut Ledger,
        now: u64,
    ) -> Result<Grant> {
        self.policy_live(now)?;
        let goal = self
            .goals
            .get(&definition.goal_id)
            .context("unregistered Goal")?;
        ensure!(
            definition.grant_version == goal.version,
            "agent definition is for another Goal version"
        );
        ensure!(
            now < goal.expiry
                && !definition.id.is_empty()
                && definition.grant_version > 0
                && !self.grants.contains_key(&definition.id)
                && !definition.capabilities.is_empty(),
            "invalid/missing agent definition"
        );
        self.validate_posture(goal, now)?;
        ensure!(
            definition.holder.tenant == goal.tenant
                && definition.holder.goal == goal.id
                && definition.lease.not_before <= now
                && now < definition.lease.expiry
                && definition.lease.expiry <= goal.expiry
                && definition.lease.fencing_token > 0,
            "workload lease/Goal mismatch"
        );
        let holder = definition.holder.uri()?;
        let scope = Scope::Agent {
            tenant: goal.tenant.clone(),
            goal: goal.id.clone(),
            holder: holder.clone(),
        };
        ensure!(!self.stopped(&scope), "allocation stopped");
        let live = self
            .grants
            .values()
            .filter(|g| g.goal_id == goal.id && !g.revoked && now < g.lease.expiry)
            .count() as u64;
        ensure!(
            live < goal.max_live_descendants,
            "live descendant cap exhausted"
        );
        let parents = if let Some(id) = &parent {
            let grant = self.live_grant(id, now)?;
            ensure!(
                grant.goal_id == goal.id && definition.lease.expiry <= grant.lease.expiry,
                "cross-Goal/extended-lease delegation"
            );
            &grant.capabilities
        } else {
            &goal.capability_ceiling
        };
        let mut ids = BTreeSet::new();
        for cap in &definition.capabilities {
            cap.validate()?;
            ensure!(
                ids.insert(&cap.id)
                    && !self
                        .grants
                        .values()
                        .any(|g| g.capabilities.iter().any(|c| c.id == cap.id))
                    && cap.holder == holder
                    && cap.goal_id == goal.id
                    && cap.grant_version == definition.grant_version
                    && cap.expiry <= definition.lease.expiry,
                "Capability identity/version/lease mismatch"
            );
            let ancestor = if parent.is_some() {
                let id = cap
                    .parent
                    .as_ref()
                    .context("delegated Capability parent required")?;
                parents
                    .iter()
                    .find(|c| &c.id == id)
                    .context("parent Capability outside single delegation chain")?
            } else {
                ensure!(
                    cap.parent.is_none() && cap.depth == 0,
                    "root Capability has delegation ancestry"
                );
                parents
                    .iter()
                    .find(|c| cap.attenuates(c))
                    .context("agent declaration exceeds Goal ceiling")?
            };
            ensure!(
                cap.attenuates(ancestor)
                    && cap.depth <= goal.max_depth
                    && (parent.is_none() || cap.depth == ancestor.depth + 1),
                "non-attenuating/deep delegation"
            );
            ensure!(
                cap.kind != CapabilityKind::TaskDelegate || cap.depth < goal.max_depth,
                "delegation authority at maximum depth"
            );
        }
        for cap in &definition.capabilities {
            if cap.budget.total > 0 {
                ledger.add_account(format!("cap/{}", cap.id), cap.budget.clone())?;
            }
        }
        let definition_hash = content_hash(&definition)?;
        let grant = Grant {
            id: definition.id.clone(),
            holder: definition.holder,
            goal_id: definition.goal_id,
            version: definition.grant_version,
            definition_hash,
            capabilities: definition.capabilities,
            parent,
            lease: definition.lease,
            revoked: false,
        };
        self.grant_epoch = self
            .grant_epoch
            .checked_add(1)
            .context("grant epoch overflow")?;
        self.grants.insert(grant.id.clone(), grant.clone());
        Ok(grant)
    }
    pub(crate) fn live_grant(&self, id: &str, now: u64) -> Result<&Grant> {
        let mut current = self.grants.get(id).context("unknown grant")?;
        let original = current;
        let mut visited = BTreeSet::new();
        loop {
            ensure!(
                visited.insert(current.id.clone())
                    && visited.len() <= 5
                    && !current.revoked
                    && current.lease.not_before <= now
                    && now < current.lease.expiry,
                "revoked/expired/cyclic grant ancestry"
            );
            if let Some(parent) = &current.parent {
                current = self.grants.get(parent).context("missing parent grant")?;
            } else {
                break;
            }
        }
        let goal = self.goals.get(&original.goal_id).context("missing Goal")?;
        ensure!(
            goal.not_before <= now
                && now < goal.expiry
                && !self.stopped(&Scope::Agent {
                    tenant: goal.tenant.clone(),
                    goal: goal.id.clone(),
                    holder: original.holder.uri()?
                }),
            "Goal or allocation stopped/expired"
        );
        Ok(original)
    }
    pub fn revoke_grant(&mut self, id: &str) -> Result<()> {
        ensure!(self.grants.contains_key(id), "unknown grant");
        self.grants.get_mut(id).unwrap().revoked = true;
        self.grant_epoch = self
            .grant_epoch
            .checked_add(1)
            .context("grant epoch overflow")?;
        Ok(())
    }
    pub fn stopped(&self, scope: &Scope) -> bool {
        self.stops
            .iter()
            .rev()
            .find(|s| s.scope == *scope)
            .is_some_and(|s| s.engaged)
            || self
                .stops
                .iter()
                .filter(|s| s.scope.contains(scope))
                .any(|s| {
                    self.stops
                        .iter()
                        .rev()
                        .find(|x| x.scope == s.scope)
                        .is_some_and(|x| x.engaged)
                })
    }
    pub fn engage_stop(
        &mut self,
        subject: &AuthenticatedSubject,
        scope: Scope,
        reason_hash: Hash,
        now: u64,
    ) -> Result<()> {
        let own = subject.workload.as_ref().is_some_and(|id| {
            scope
                == Scope::Agent {
                    tenant: id.tenant.clone(),
                    goal: id.goal.clone(),
                    holder: subject.id.clone(),
                }
        });
        ensure!(
            own || self.permitted(
                subject,
                "engage",
                "stops",
                &scope,
                0,
                ConfirmationTier::C0,
                IsolationTier::P,
                now
            ),
            "stop scope denied"
        );
        subject.valid(now)?;
        self.set_stop(scope, true, subject.id.clone(), reason_hash, now)
    }
    pub fn engage_ledger_stop(
        &mut self,
        tenant: String,
        goal: String,
        reason_hash: Hash,
        now: u64,
    ) -> Result<()> {
        self.set_stop(
            Scope::Goal { tenant, goal },
            true,
            "trusted-ledger".into(),
            reason_hash,
            now,
        )
    }
    /// The trusted actuator accepts only a safety stop whose scope and trigger are
    /// explicitly pinned in the operator-compiled policy Capability.
    pub fn policy_stop(
        &mut self,
        subject: &AuthenticatedSubject,
        request: ActionRequest,
        ledger: &mut Ledger,
        signer: &CryptoSigner,
        now: u64,
    ) -> Result<Decision> {
        let intervention = self.validate_policy_intervention(&request, "stop", now)?;
        let decision = self.authorize(subject, request, &[], None, ledger, signer, now)?;
        self.consume_permit(subject, &decision.permit, ledger, now)?;
        self.set_stop(
            intervention.scope,
            true,
            subject.id.clone(),
            intervention.reason_hash,
            now,
        )?;
        Ok(decision)
    }
    /// A typed flag records evidence without changing a stop, grant or monetary limit.
    pub fn policy_flag(
        &mut self,
        subject: &AuthenticatedSubject,
        request: ActionRequest,
        ledger: &mut Ledger,
        signer: &CryptoSigner,
        now: u64,
    ) -> Result<Decision> {
        self.validate_policy_intervention(&request, "flag", now)?;
        let decision = self.authorize(subject, request, &[], None, ledger, signer, now)?;
        self.consume_permit(subject, &decision.permit, ledger, now)?;
        Ok(decision)
    }
    fn validate_policy_intervention(
        &self,
        request: &ActionRequest,
        operation: &str,
        now: u64,
    ) -> Result<PolicyIntervention> {
        ensure!(
            request.action.kind == CapabilityKind::PolicyRequest
                && request.action.operation == operation
                && request.action.class == OperationClass::ReadOnly
                && request.budget_capability_id.is_none()
                && request.envelope_id.is_none()
                && request.usage_bound.is_empty(),
            "only a typed safety intervention is admitted"
        );
        let intervention: PolicyIntervention =
            serde_json::from_value(request.action.arguments.clone())?;
        ensure!(
            !intervention.trigger.is_empty()
                && intervention.reason_hash != [0; 32]
                && intervention.confidence_milli <= 1000,
            "invalid intervention evidence"
        );
        let grant = self.live_grant(&request.grant_id, now)?;
        ensure!(
            operation != "flag" || request.action.resource == format!("policy/{}", grant.goal_id),
            "flag resource must be the registered Goal policy"
        );
        let cap = grant
            .capabilities
            .iter()
            .find(|c| c.id == request.capability_id)
            .context("policy Capability absent")?;
        ensure!(
            cap.confirmation == ConfirmationTier::C0
                && cap.constraints.arguments.get("scope").is_some_and(|c| c
                    .accepts(&serde_json::to_value(&intervention.scope).unwrap_or(Value::Null)))
                && cap
                    .constraints
                    .arguments
                    .get("trigger")
                    .is_some_and(|c| c.accepts(&Value::String(intervention.trigger.clone()))),
            "policy scope/trigger must be explicitly registered"
        );
        Ok(intervention)
    }
    fn set_stop(
        &mut self,
        scope: Scope,
        engaged: bool,
        actor: String,
        reason_hash: Hash,
        now: u64,
    ) -> Result<()> {
        self.stop_epoch = self
            .stop_epoch
            .checked_add(1)
            .context("stop epoch overflow")?;
        self.stops.push(Stop {
            scope,
            engaged,
            actor,
            reason_hash,
            epoch: self.stop_epoch,
            timestamp: now,
        });
        Ok(())
    }
    pub fn release_stop(
        &mut self,
        subject: &AuthenticatedSubject,
        scope: Scope,
        approvals: &[Signed<HumanApproval>],
        now: u64,
    ) -> Result<()> {
        ensure!(
            self.permitted(
                subject,
                "release",
                "stops",
                &scope,
                0,
                ConfirmationTier::C3,
                IsolationTier::P,
                now
            ) && (!matches!(scope, Scope::Global) || self.root(subject)),
            "stop release denied"
        );
        self.step_up(subject, now)?;
        let digest = content_hash(&("release-stop", &scope, self.stop_epoch))?;
        let signers = self.approvers(approvals, &digest, &[0; 32], 0, &scope, now)?;
        ensure!(
            signers.len()
                >= if matches!(scope, Scope::Global | Scope::Tenant { .. }) {
                    2
                } else {
                    1
                },
            "stop release lacks distinct privileged humans"
        );
        self.set_stop(scope, false, subject.id.clone(), digest, now)
    }
    pub fn record_input(
        &mut self,
        observer: &AuthenticatedSubject,
        mut input: InputRecord,
        signer: &CryptoSigner,
        now: u64,
    ) -> Result<()> {
        ensure!(
            signer.public_key() == self.policy.payload.provenance_signer,
            "input observations require trusted broker authority"
        );
        observer.valid(now)?;
        ensure!(
            !input.id.is_empty()
                && !self.inputs.contains_key(&input.id)
                && !input.source_type.is_empty(),
            "invalid input provenance"
        );
        if let Some(workload) = &observer.workload {
            ensure!(input.goal_id == workload.goal, "cross-Goal input");
        }
        if input.trust != Trust::Untrusted {
            ensure!(
                self.policy.payload.root_humans.contains(&input.producer)
                    || self
                        .policy
                        .payload
                        .trusted_components
                        .contains(&input.producer),
                "source trust requires a registered trusted producer"
            );
        }
        for parent in &input.parents {
            let source = self.inputs.get(parent).context("unknown lineage source")?;
            ensure!(source.goal_id == input.goal_id, "cross-Goal lineage");
            input.trust = input.trust.min(source.trust);
            input.markings.extend(source.markings.iter().cloned());
        }
        self.observed
            .entry(observer.id.clone())
            .or_default()
            .insert(input.id.clone());
        self.inputs.insert(input.id.clone(), input);
        Ok(())
    }
    #[allow(clippy::type_complexity)]
    fn lineage(
        &self,
        holder: &str,
    ) -> Result<(BTreeSet<String>, Trust, BTreeSet<String>, BTreeSet<String>)> {
        let mut all = self.observed.get(holder).cloned().unwrap_or_default();
        let mut pending: Vec<_> = all.iter().cloned().collect();
        let mut trust = Trust::Trusted;
        let mut markings = BTreeSet::new();
        let mut types = BTreeSet::new();
        while let Some(id) = pending.pop() {
            let input = self.inputs.get(&id).context("missing recorded lineage")?;
            trust = trust.min(input.trust);
            markings.extend(input.markings.iter().cloned());
            types.insert(input.source_type.clone());
            for parent in &input.parents {
                if all.insert(parent.clone()) {
                    pending.push(parent.clone());
                }
            }
        }
        Ok((all, trust, markings, types))
    }
    pub fn append_finding(
        &mut self,
        subject: &AuthenticatedSubject,
        id: String,
        result_hash: Hash,
        grant_id: &str,
        signer: &CryptoSigner,
        now: u64,
    ) -> Result<Signed<Provenance>> {
        let grant = self.live_grant(grant_id, now)?;
        ensure!(
            grant.holder.uri()? == subject.id
                && !self.findings.contains_key(&id)
                && self.policy.payload.provenance_signer == signer.public_key(),
            "Finding producer/signer mismatch"
        );
        let (inputs, trust, markings, _) = self.lineage(&subject.id)?;
        let provenance = Provenance {
            id: id.clone(),
            producer: subject.id.clone(),
            goal_id: grant.goal_id.clone(),
            grant_version: grant.version,
            result_hash,
            inputs,
            trust,
            markings,
            timestamp: now,
        };
        let signed = signer.sign("finding-provenance", provenance)?;
        self.findings.insert(id, signed.clone());
        Ok(signed)
    }
    pub fn retract_finding(
        &mut self,
        subject: &AuthenticatedSubject,
        id: &str,
        reason_hash: Hash,
        signer: &CryptoSigner,
        now: u64,
    ) -> Result<Signed<Retraction>> {
        let finding = self.findings.get(id).context("unknown Finding")?;
        let goal = &self.goals[&finding.payload.goal_id];
        subject.valid(now)?;
        let own = finding.payload.producer == subject.id
            && subject.workload.is_some()
            && self.grants.values().any(|g| {
                g.goal_id == goal.id
                    && g.holder.uri().is_ok_and(|id| id == subject.id)
                    && self.live_grant(&g.id, now).is_ok()
                    && g.capabilities.iter().any(|c| {
                        c.not_before <= now
                            && now < c.expiry
                            && ((c.kind == CapabilityKind::FindingAppend
                                && c.constraints.operations.contains("retract"))
                                || (c.kind == CapabilityKind::ToolCall
                                    && c.constraints.operations.contains("finding_retract")))
                    })
            });
        ensure!(
            (own || self.permitted(
                subject,
                "retract",
                "findings",
                &Scope::Goal {
                    tenant: goal.tenant.clone(),
                    goal: goal.id.clone()
                },
                0,
                ConfirmationTier::C1,
                IsolationTier::P,
                now
            )) && signer.public_key() == self.policy.payload.provenance_signer,
            "retraction denied"
        );
        if let Some(existing) = self.retractions.get(id) {
            return Ok(existing.clone());
        }
        self.retraction_epoch = self
            .retraction_epoch
            .checked_add(1)
            .context("retraction epoch overflow")?;
        let record = signer.sign(
            "finding-retraction",
            Retraction {
                finding_id: id.into(),
                actor: subject.id.clone(),
                reason_hash,
                timestamp: now,
                epoch: self.retraction_epoch,
            },
        )?;
        self.retractions.insert(id.into(), record.clone());
        Ok(record)
    }
    fn evidence_valid(&self, ids: &BTreeSet<String>) -> Result<()> {
        let mut pending: Vec<_> = ids.iter().cloned().collect();
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            ensure!(!self.retractions.contains_key(&id), "evidence retracted");
            if let Some(f) = self.findings.get(&id) {
                f.verify("finding-provenance", &self.policy.payload.provenance_signer)?;
                pending.extend(f.payload.inputs.iter().cloned());
            } else {
                let source = self.inputs.get(&id).context("unknown evidence input")?;
                pending.extend(source.parents.iter().cloned());
            }
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn authorize(
        &mut self,
        subject: &AuthenticatedSubject,
        request: ActionRequest,
        approvals: &[Signed<HumanApproval>],
        prices: Option<&Signed<PriceTable>>,
        ledger: &mut Ledger,
        signer: &CryptoSigner,
        now: u64,
    ) -> Result<Decision> {
        subject.valid(now)?;
        self.policy_live(now)?;
        ensure!(
            !request.intent_id.is_empty()
                && signer.public_key() == self.policy.payload.permit_signer,
            "missing intent/dispatch signing authority"
        );
        let grant = self.live_grant(&request.grant_id, now)?.clone();
        let goal = self.goals[&grant.goal_id].clone();
        ensure!(
            grant.holder.uri()? == subject.id
                && grant.version == request.grant_version
                && !ledger.stopped_goals.contains(&goal.id),
            "request holder/version revoked or Goal stopped"
        );
        let cap = grant
            .capabilities
            .iter()
            .find(|c| c.id == request.capability_id)
            .context("unmatched Capability")?;
        cap.validate()?;
        ensure!(
            cap.kind == request.action.kind
                && cap.resource.matches(&request.action.resource)
                && cap.not_before <= now
                && now < cap.expiry
                && request.action.isolation >= cap.minimum_isolation
                && goal.resources.contains(&request.action.resource)
                && goal.classes.contains(&request.action.class),
            "request outside effective Capability/Goal"
        );
        cap.constraints.check(&request.action)?;
        let spend = request
            .budget_capability_id
            .as_ref()
            .map(|id| {
                grant
                    .capabilities
                    .iter()
                    .find(|c| &c.id == id)
                    .context("budget Capability missing")
            })
            .transpose()?;
        if let Some(spend) = spend {
            spend.validate()?;
            ensure!(
                spend.kind == CapabilityKind::BudgetSpend
                    && spend.resource.matches(&request.action.resource)
                    && spend.not_before <= now
                    && now < spend.expiry
                    && request.action.isolation >= spend.minimum_isolation,
                "unmatched spend authority/isolation"
            );
            spend.constraints.check(&request.action)?;
        }
        if request.action.resource.starts_with("target/") {
            let permission = goal
                .target_permissions
                .get(&request.action.resource)
                .context("target not authorized")?;
            ensure!(
                permission
                    .payload
                    .operations
                    .contains(&request.action.operation)
                    && now < permission.payload.expiry,
                "target operation/permission outside posture"
            );
        }
        let (observed, trust, markings, types) = self.lineage(&subject.id)?;
        ensure!(
            markings.is_subset(&cap.constraints.read_markings),
            "Capability lacks input marking clearance"
        );
        if let Some(recipient) = &request.action.recipient {
            let clearance = self
                .policy
                .payload
                .recipient_clearances
                .get(recipient)
                .context("recipient lacks registered clearance")?;
            ensure!(
                markings.is_subset(clearance),
                "payload disclosure outside recipient clearance"
            );
        }
        let mut evidence = observed.clone();
        evidence.extend(request.action.evidence.iter().cloned());
        self.evidence_valid(&evidence)?;
        let evidence_hash = content_hash(&evidence)?;
        let action_hash = content_hash(&(
            subject.id(),
            subject.certificate_hash(),
            &request,
            grant.definition_hash,
            &evidence,
        ))?;
        if let Some(existing) = self.pending.get(&request.intent_id) {
            ensure!(
                existing.payload.action_hash == action_hash
                    && !self.used.contains(&request.intent_id),
                "intent reused or already dispatched"
            );
            return Ok(Decision {
                permit: existing.clone(),
                audit: self.decision_event(subject, &existing.payload, now)?,
            });
        }
        let mut tier = cap.confirmation.max(request.action.class.tier()).max(
            spend
                .map(|c| c.confirmation)
                .unwrap_or(ConfirmationTier::C0),
        );
        let exempt = goal.taint_exemptions.iter().any(|e| {
            e.kind == cap.kind
                && e.resource.matches(&request.action.resource)
                && e.operations.contains(&request.action.operation)
                && types.is_subset(&e.source_types)
        });
        if trust == Trust::Untrusted && tier >= ConfirmationTier::C1 && !exempt {
            tier = tier.raised();
        }
        let scope = Scope::Agent {
            tenant: goal.tenant.clone(),
            goal: goal.id.clone(),
            holder: subject.id.clone(),
        };
        let humans = self.approvers(
            approvals,
            &action_hash,
            &evidence_hash,
            grant.version,
            &scope,
            now,
        )?;
        match tier {
            ConfirmationTier::C0 | ConfirmationTier::C1 => {}
            ConfirmationTier::C2 => ensure!(
                (goal.unattended_c2 && exempt) || !humans.is_empty(),
                "C2 exact-action confirmation required"
            ),
            ConfirmationTier::C3 => ensure!(
                humans.len() >= 2,
                "C3 requires two distinct exact-action human approvals"
            ),
        }
        if trust == Trust::Untrusted && tier >= ConfirmationTier::C2 {
            ensure!(
                (goal.unattended_c2 && exempt) || !humans.is_empty(),
                "untrusted evidence cannot grant C2/C3 authority"
            );
        }
        let mut price_version = None;
        let billable = request.budget_capability_id.is_some();
        ensure!(
            cap.kind != CapabilityKind::BudgetSpend
                || request.budget_capability_id.as_deref() == Some(cap.id.as_str()),
            "inference spend cannot bypass reservation"
        );
        if let Some(spend) = spend {
            let prices = prices.context("signed prices required")?;
            ensure!(
                self.policy.payload.price_signers.contains(&prices.signer),
                "untrusted price signer"
            );
            prices.verify("price-table", &prices.signer)?;
            prices.payload.validate(now)?;
            if request.action.operation == "inference" {
                ensure!(
                    request.action.resource
                        == format!(
                            "provider/{}/model/{}",
                            prices.payload.provider, prices.payload.model
                        ),
                    "price table belongs to another provider/model"
                );
            }
            let mut accounts = BTreeSet::new();
            for capability in [cap, spend] {
                let mut current = capability;
                loop {
                    accounts.insert(format!("cap/{}", current.id));
                    if let Some(parent) = &current.parent {
                        current = self
                            .grants
                            .values()
                            .flat_map(|g| g.capabilities.iter())
                            .find(|c| &c.id == parent)
                            .context("missing Capability budget ancestor")?;
                    } else {
                        break;
                    }
                }
            }
            ledger.reserve(
                ReservationRequest {
                    intent_id: request.intent_id.clone(),
                    envelope_id: request
                        .envelope_id
                        .clone()
                        .context("Task envelope required")?,
                    goal_id: goal.id.clone(),
                    holder: subject.id.clone(),
                    capability_accounts: accounts,
                    request_hash: action_hash,
                    usage_bound: request.usage_bound.clone(),
                    price_version: prices.payload.version.clone(),
                },
                prices,
                &prices.signer,
                now,
            )?;
            price_version = Some(prices.payload.version.clone());
        } else {
            ensure!(
                request.usage_bound.is_empty() && request.envelope_id.is_none(),
                "billable effect missing budget authority"
            );
        }
        let expiry = cap
            .expiry
            .min(spend.map(|c| c.expiry).unwrap_or(u64::MAX))
            .min(grant.lease.expiry)
            .min(goal.expiry)
            .min(subject.expires)
            .min(now.checked_add(30).context("permit expiry overflow")?);
        let permit = DispatchPermit {
            intent_id: request.intent_id.clone(),
            holder: subject.id.clone(),
            certificate_hash: subject.certificate_hash,
            goal_id: goal.id,
            grant_id: grant.id,
            grant_version: grant.version,
            fencing_token: grant.lease.fencing_token,
            action_hash,
            stop_epoch: self.stop_epoch,
            grant_epoch: self.grant_epoch,
            retraction_epoch: self.retraction_epoch,
            evidence_hash,
            observed_hash: content_hash(&observed)?,
            price_version,
            confirmation: tier,
            issued: now,
            expiry,
            billable,
        };
        let audit = self.decision_event(subject, &permit, now)?;
        let signed = signer.sign("dispatch-permit", permit)?;
        self.pending.insert(request.intent_id, signed.clone());
        Ok(Decision {
            permit: signed,
            audit,
        })
    }
    pub fn confirmation_review(
        &self,
        subject: &AuthenticatedSubject,
        request: &ActionRequest,
        ledger: &Ledger,
        now: u64,
    ) -> Result<ConfirmationReview> {
        subject.valid(now)?;
        self.policy_live(now)?;
        ensure!(
            !request.intent_id.is_empty(),
            "missing stable action intent"
        );
        let grant = self.live_grant(&request.grant_id, now)?.clone();
        let goal = self.goals[&grant.goal_id].clone();
        ensure!(
            grant.holder.uri()? == subject.id
                && grant.version == request.grant_version
                && !ledger.stopped_goals.contains(&goal.id),
            "request holder/version revoked or Goal stopped"
        );
        let cap = grant
            .capabilities
            .iter()
            .find(|c| c.id == request.capability_id)
            .context("unmatched Capability")?;
        cap.validate()?;
        ensure!(
            cap.kind == request.action.kind
                && cap.resource.matches(&request.action.resource)
                && cap.not_before <= now
                && now < cap.expiry
                && request.action.isolation >= cap.minimum_isolation
                && goal.resources.contains(&request.action.resource)
                && goal.classes.contains(&request.action.class),
            "request outside effective Capability/Goal"
        );
        cap.constraints.check(&request.action)?;
        let spend = request
            .budget_capability_id
            .as_ref()
            .map(|id| {
                grant
                    .capabilities
                    .iter()
                    .find(|c| &c.id == id)
                    .context("budget Capability missing")
            })
            .transpose()?;
        if let Some(spend) = spend {
            spend.validate()?;
            ensure!(
                spend.kind == CapabilityKind::BudgetSpend
                    && spend.resource.matches(&request.action.resource)
                    && spend.not_before <= now
                    && now < spend.expiry
                    && request.action.isolation >= spend.minimum_isolation,
                "unmatched spend authority/isolation"
            );
            spend.constraints.check(&request.action)?;
        }
        if request.action.resource.starts_with("target/") {
            let permission = goal
                .target_permissions
                .get(&request.action.resource)
                .context("target not authorized")?;
            ensure!(
                permission
                    .payload
                    .operations
                    .contains(&request.action.operation)
                    && now < permission.payload.expiry,
                "target operation/permission outside posture"
            );
        }
        let (observed, trust, markings, types) = self.lineage(&subject.id)?;
        ensure!(
            markings.is_subset(&cap.constraints.read_markings),
            "Capability lacks input marking clearance"
        );
        if let Some(recipient) = &request.action.recipient {
            let clearance = self
                .policy
                .payload
                .recipient_clearances
                .get(recipient)
                .context("recipient lacks registered clearance")?;
            ensure!(
                markings.is_subset(clearance),
                "payload disclosure outside recipient clearance"
            );
        }
        let mut evidence = observed.clone();
        evidence.extend(request.action.evidence.iter().cloned());
        self.evidence_valid(&evidence)?;
        let evidence_hash = content_hash(&evidence)?;
        let action_hash = content_hash(&(
            subject.id(),
            subject.certificate_hash(),
            &request,
            grant.definition_hash,
            &evidence,
        ))?;
        let mut tier = cap.confirmation.max(request.action.class.tier()).max(
            spend
                .map(|c| c.confirmation)
                .unwrap_or(ConfirmationTier::C0),
        );
        let exempt = goal.taint_exemptions.iter().any(|e| {
            e.kind == cap.kind
                && e.resource.matches(&request.action.resource)
                && e.operations.contains(&request.action.operation)
                && types.is_subset(&e.source_types)
        });
        if trust == Trust::Untrusted && tier >= ConfirmationTier::C1 && !exempt {
            tier = tier.raised();
        }
        let scope = Scope::Agent {
            tenant: goal.tenant.clone(),
            goal: goal.id.clone(),
            holder: subject.id.clone(),
        };
        let required_humans = match tier {
            ConfirmationTier::C0 | ConfirmationTier::C1 => 0,
            ConfirmationTier::C2 if goal.unattended_c2 && exempt => 0,
            ConfirmationTier::C2 => 1,
            ConfirmationTier::C3 => 2,
        };
        let expiry = cap
            .expiry
            .min(goal.expiry)
            .min(grant.lease.expiry)
            .min(spend.map_or(u64::MAX, |cap| cap.expiry));
        Ok(ConfirmationReview {
            intent_id: request.intent_id.clone(),
            holder: subject.id.clone(),
            certificate_hash: subject.certificate_hash,
            grant_id: grant.id,
            grant_version: grant.version,
            action_hash,
            evidence_hash,
            scope,
            tier,
            required_humans,
            expiry,
            grant_epoch: self.grant_epoch,
            stop_epoch: self.stop_epoch,
            retraction_epoch: self.retraction_epoch,
        })
    }
    /// Durable approvals are selected by their exact binding, then signatures and human authority are reverified.
    pub fn review_approvals(
        &self,
        review: &ConfirmationReview,
        approvals: impl IntoIterator<Item = Signed<HumanApproval>>,
        now: u64,
    ) -> Result<Vec<Signed<HumanApproval>>> {
        let selected: Vec<_> = approvals
            .into_iter()
            .filter(|signed| {
                let a = &signed.payload;
                a.action_hash == review.action_hash
                    && a.evidence_hash == review.evidence_hash
                    && a.grant_version == review.grant_version
                    && a.scope == review.scope
                    && a.authority_epoch == self.grant_epoch
                    && a.issued <= now
                    && now < a.expiry
                    && a.auth_time <= now
                    && now - a.auth_time <= self.policy.payload.step_up_max_age
            })
            .collect();
        self.approvers(
            &selected,
            &review.action_hash,
            &review.evidence_hash,
            review.grant_version,
            &review.scope,
            now,
        )?;
        Ok(selected)
    }
    pub fn review_confirmed(
        &self,
        review: &ConfirmationReview,
        approvals: &[Signed<HumanApproval>],
        now: u64,
    ) -> Result<bool> {
        ensure!(
            now < review.expiry
                && review.grant_epoch == self.grant_epoch
                && review.stop_epoch == self.stop_epoch
                && review.retraction_epoch == self.retraction_epoch,
            "action review is stale"
        );
        let humans = self.approvers(
            approvals,
            &review.action_hash,
            &review.evidence_hash,
            review.grant_version,
            &review.scope,
            now,
        )?;
        Ok(humans.len() >= review.required_humans)
    }
    pub fn approval_binding(
        &self,
        subject: &AuthenticatedSubject,
        request: &ActionRequest,
        now: u64,
    ) -> Result<(Hash, Hash, Scope, u64)> {
        subject.valid(now)?;
        let grant = self.live_grant(&request.grant_id, now)?;
        ensure!(
            grant.holder.uri()? == subject.id && grant.version == request.grant_version,
            "approval target identity/version mismatch"
        );
        let goal = &self.goals[&grant.goal_id];
        let (mut evidence, _, _, _) = self.lineage(subject.id())?;
        evidence.extend(request.action.evidence.iter().cloned());
        self.evidence_valid(&evidence)?;
        Ok((
            content_hash(&(
                subject.id(),
                subject.certificate_hash(),
                request,
                grant.definition_hash,
                &evidence,
            ))?,
            content_hash(&evidence)?,
            Scope::Agent {
                tenant: goal.tenant.clone(),
                goal: goal.id.clone(),
                holder: subject.id.clone(),
            },
            grant.version,
        ))
    }
    fn decision_event(
        &self,
        subject: &AuthenticatedSubject,
        permit: &DispatchPermit,
        now: u64,
    ) -> Result<AuditEvent> {
        Ok(AuditEvent {
            correlation: None,
            id: format!("authorize/{}", permit.intent_id),
            kind: "dispatch_authorized".into(),
            actor: subject.id.clone(),
            goal: Some(permit.goal_id.clone()),
            timestamp: now,
            decision: format!("{:?}", permit.confirmation),
            payload_hash: content_hash(permit)?,
        })
    }
    /// Re-review an unused local effect after unrelated authority changes, before its application.
    /// The caller must preserve and atomically recheck the original Task and allocation leases.
    #[allow(clippy::too_many_arguments)] // Original binding, current human authority and ledger share one review.
    pub fn refresh_nonbillable_permit(
        &mut self,
        subject: &AuthenticatedSubject,
        request: &ActionRequest,
        permit: &Signed<DispatchPermit>,
        approvals: &[Signed<HumanApproval>],
        ledger: &mut Ledger,
        signer: &CryptoSigner,
        now: u64,
    ) -> Result<NonbillablePermitRefresh> {
        permit.verify("dispatch-permit", &self.policy.payload.permit_signer)?;
        let original = &permit.payload;
        ensure!(
            !original.billable
                && original.price_version.is_none()
                && request.budget_capability_id.is_none()
                && request.envelope_id.is_none()
                && request.usage_bound.is_empty()
                && !ledger.intents.contains_key(&request.intent_id),
            "paid effects cannot refresh local application authority"
        );
        ensure!(
            original.intent_id == request.intent_id
                && original.grant_id == request.grant_id
                && original.grant_version == request.grant_version
                && original.holder == subject.id()
                && original.certificate_hash == subject.certificate_hash()
                && original.issued <= now
                && now < original.expiry
                && !self.used.contains(&request.intent_id),
            "refresh changed identity or reused consumed authority"
        );
        let saved = self
            .pending
            .get(&request.intent_id)
            .context("refresh requires the original pending permit")?;
        ensure!(
            content_hash(saved)? == content_hash(permit)?,
            "refresh requires the recorded signed permit"
        );
        let grant = self.live_grant(&request.grant_id, now)?;
        ensure!(
            grant.goal_id == original.goal_id
                && grant.lease.fencing_token == original.fencing_token,
            "refresh changed Goal or allocation fence"
        );
        let (observed, _, _, _) = self.lineage(subject.id())?;
        ensure!(
            content_hash(&observed)? == original.observed_hash,
            "refresh changed the originally observed input lineage"
        );
        let mut review = self.confirmation_review(subject, request, ledger, now)?;
        review.expiry = review.expiry.min(subject.expires).min(original.expiry);
        ensure!(
            review.action_hash == original.action_hash
                && review.evidence_hash == original.evidence_hash,
            "refresh changed the original action or evidence binding"
        );
        if original.grant_epoch == self.grant_epoch
            && original.stop_epoch == self.stop_epoch
            && original.retraction_epoch == self.retraction_epoch
            && original.confirmation == review.tier
            && now < original.expiry
        {
            return Ok(NonbillablePermitRefresh::Ready {
                permit: permit.clone(),
                audit: None,
            });
        }
        let selected = self.review_approvals(&review, approvals.iter().cloned(), now)?;
        if !self.review_confirmed(&review, &selected, now)? {
            return Ok(NonbillablePermitRefresh::ConfirmationRequired(review));
        }
        // authorize intentionally reuses pending permits, so replace it only in a staged state.
        // A failed review must leave both the original pending permit and all money unchanged.
        let mut staged = self.clone();
        let mut staged_ledger = ledger.clone();
        staged.pending.remove(&request.intent_id);
        let mut decision = staged.authorize(
            subject,
            request.clone(),
            &selected,
            None,
            &mut staged_ledger,
            signer,
            now,
        )?;
        ensure!(
            !decision.permit.payload.billable
                && content_hash(&staged_ledger)? == content_hash(ledger)?,
            "local authority refresh changed financial state"
        );
        let mut refreshed = decision.permit.payload;
        refreshed.expiry = refreshed.expiry.min(original.expiry);
        decision.audit.payload_hash = content_hash(&refreshed)?;
        decision.permit = signer.sign("dispatch-permit", refreshed)?;
        staged
            .pending
            .insert(request.intent_id.clone(), decision.permit.clone());
        let digest: String = content_hash(&decision.permit)?
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        decision.audit.id = format!("permit-refresh/{}/{digest}", request.intent_id);
        decision.audit.kind = "dispatch_permit_refreshed".into();
        *self = staged;
        Ok(NonbillablePermitRefresh::Ready {
            permit: decision.permit,
            audit: Some(decision.audit),
        })
    }
    /// Root commits this with the audit event before handing a permit to an executor.
    pub fn consume_permit(
        &mut self,
        subject: &AuthenticatedSubject,
        permit: &Signed<DispatchPermit>,
        ledger: &mut Ledger,
        now: u64,
    ) -> Result<()> {
        permit.verify("dispatch-permit", &self.policy.payload.permit_signer)?;
        let p = &permit.payload;
        subject.valid(now)?;
        self.policy_live(now)?;
        ensure!(
            p.holder == subject.id
                && p.certificate_hash == subject.certificate_hash
                && p.issued <= now
                && now < p.expiry
                && p.stop_epoch == self.stop_epoch
                && p.grant_epoch == self.grant_epoch
                && p.retraction_epoch == self.retraction_epoch
                && !self.used.contains(&p.intent_id),
            "stale/replayed dispatch authority"
        );
        let saved = self
            .pending
            .get(&p.intent_id)
            .context("unrecorded dispatch permit")?;
        ensure!(
            content_hash(saved)? == content_hash(permit)?,
            "forged dispatch permit"
        );
        let grant = self.live_grant(&p.grant_id, now)?;
        ensure!(
            grant.version == p.grant_version
                && grant.lease.fencing_token == p.fencing_token
                && !ledger.stopped_goals.contains(&p.goal_id),
            "dispatch grant revoked/fenced"
        );
        let (observed, _, _, _) = self.lineage(subject.id())?;
        ensure!(
            content_hash(&observed)? == p.observed_hash,
            "context lineage changed after approval"
        );
        self.evidence_valid(&observed)?;
        if p.billable {
            let reservation = ledger
                .intents
                .get(&p.intent_id)
                .context("missing action reservation")?;
            reservation.price.validate(now)?;
            ensure!(
                p.price_version.as_ref() == Some(&reservation.price.version),
                "dispatch price version mismatch"
            );
            ledger.mark_dispatched(&p.intent_id, content_hash(permit)?)?;
        }
        self.used.insert(p.intent_id.clone());
        Ok(())
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod test_support {
    use super::*;
    use crate::identity::{IdentityAuthority, IssuedIdentity, test_operator};
    pub struct Fixture {
        pub now: u64,
        pub signer: CryptoSigner,
        pub operators: Vec<VerifiedOperator>,
        pub policy: Signed<DeploymentPolicy>,
        pub posture: GoalPosture,
        pub approvals: Vec<Signed<HumanApproval>>,
        pub definition: AgentDefinition,
        pub identity: IssuedIdentity,
        pub subject: AuthenticatedSubject,
    }
    pub fn fixture() -> Fixture {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let signer = CryptoSigner::from_seed([44; 32]);
        let operators = vec![test_operator("admin-a", now), test_operator("admin-b", now)];
        let key = signer.public_key();
        let limits = Limits {
            total: 1_000_000,
            per_call: 200_000,
            per_tick: 500_000,
            tick_seconds: 10,
            per_day: 1_000_000,
        };
        let resources: BTreeSet<_> = [
            "tool/data/a",
            "finding/g",
            "board/g",
            "task/g",
            "provider/local/model/m1",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let policy = signer
            .sign(
                "deployment-policy",
                DeploymentPolicy {
                    version: "test-v1".into(),
                    tenant: "t".into(),
                    classification: "test".into(),
                    not_before: now,
                    expiry: now + 900,
                    inventory: resources.clone(),
                    classes: [
                        OperationClass::ReadOnly,
                        OperationClass::Reversible,
                        OperationClass::External,
                    ]
                    .into(),
                    global_limits: limits.clone(),
                    tenant_limits: limits.clone(),
                    max_depth: 4,
                    max_live_descendants: 500,
                    max_open_tasks: 500,
                    high_impact_spend_threshold: 1_000_000,
                    step_up_max_age: 300,
                    approval_signers: [key].into(),
                    price_signers: [key].into(),
                    target_signers: [key].into(),
                    qa_signers: [key].into(),
                    provenance_signer: key,
                    permit_signer: key,
                    recipient_clearances: [(
                        "provider/local/model/m1".into(),
                        ["internal".into()].into(),
                    )]
                    .into(),
                    trusted_components: BTreeSet::new(),
                    root_humans: operators.iter().map(VerifiedOperator::id).collect(),
                },
            )
            .unwrap();
        let ceiling: Vec<_> = [
            CapabilityKind::ToolCall,
            CapabilityKind::FindingAppend,
            CapabilityKind::FindingRead,
            CapabilityKind::BoardRead,
            CapabilityKind::BoardPost,
            CapabilityKind::TaskDelegate,
            CapabilityKind::BudgetSpend,
            CapabilityKind::PolicyRequest,
        ]
        .into_iter()
        .enumerate()
        .map(|(n, kind)| Capability {
            id: format!("ceiling-{n}"),
            kind,
            resource: ResourceSelector::Any,
            constraints: Constraints {
                operations: [
                    "read",
                    "append",
                    "call",
                    "claim",
                    "post",
                    "submit",
                    "retract",
                    "delegate",
                    "inference",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                resources: resources.clone(),
                recipients: ["provider/local/model/m1".into()].into(),
                read_markings: ["internal".into()].into(),
                max_payload_bytes: 65536,
                arguments: BTreeMap::new(),
                deny_unknown_arguments: false,
            },
            budget: limits.clone(),
            confirmation: ConfirmationTier::C0,
            minimum_isolation: IsolationTier::P,
            not_before: now,
            expiry: now + 600,
            goal_id: "g".into(),
            holder: "goal/g".into(),
            parent: None,
            depth: 0,
            grant_version: 1,
        })
        .collect();
        let mut posture = GoalPosture {
            id: "g".into(),
            version: 1,
            contract_hash: [0; 32],
            evaluator_public_key: CryptoSigner::from_seed([45; 32]).public_key(),
            tenant: "t".into(),
            intent: "test synthetic Goal".into(),
            resources,
            classes: [
                OperationClass::ReadOnly,
                OperationClass::Reversible,
                OperationClass::External,
            ]
            .into(),
            target_permissions: BTreeMap::new(),
            capability_ceiling: ceiling,
            limits,
            not_before: now,
            expiry: now + 600,
            max_depth: 4,
            max_live_descendants: 500,
            max_open_tasks: 500,
            unattended_c2: false,
            qa: None,
            taint_exemptions: vec![],
        };
        let contract = goal_spec_posture(&posture);
        bind_goal_contract(&mut posture, &contract).unwrap();
        let state = SecurityState::new(policy.clone(), &key, now).unwrap();
        let hash = posture.registration_hash().unwrap();
        let approvals = vec![
            state
                .issue_approval(
                    &operators[0],
                    hash,
                    [0; 32],
                    0,
                    Scope::Tenant { tenant: "t".into() },
                    now + 600,
                    &signer,
                    now,
                )
                .unwrap(),
        ];
        let ca = IdentityAuthority::generate("test.local").unwrap();
        let workload = WorkloadId {
            trust_domain: "test.local".into(),
            tenant: "t".into(),
            goal: "g".into(),
            node: "n1".into(),
            agent: "a1".into(),
            incarnation: "1".into(),
        };
        let identity = ca.issue(workload.clone(), now, 900).unwrap();
        let subject = AuthenticatedSubject::workload(
            &identity.certificate_der,
            &identity.ca_der,
            workload.clone(),
            now,
        )
        .unwrap();
        let capabilities = posture
            .capability_ceiling
            .iter()
            .enumerate()
            .map(|(n, cap)| {
                let mut c = cap.clone();
                c.id = format!("agent-cap-{n}");
                c.holder = subject.id().into();
                c
            })
            .collect();
        let definition = AgentDefinition {
            id: "agent-grant".into(),
            holder: workload,
            goal_id: "g".into(),
            grant_version: 1,
            capabilities,
            lease: Lease {
                not_before: now,
                expiry: now + 600,
                fencing_token: 1,
            },
        };
        Fixture {
            now,
            signer,
            operators,
            policy,
            posture,
            approvals,
            definition,
            identity,
            subject,
        }
    }
    pub fn goal_spec(f: &Fixture) -> crate::domain::GoalSpec {
        goal_spec_posture(&f.posture)
    }
    pub fn goal_spec_posture(posture: &GoalPosture) -> crate::domain::GoalSpec {
        crate::domain::GoalSpec {
            goal_id: posture.id.clone(),
            intent: posture.intent.clone(),
            version: posture.version,
            rubric_version: "test-rubric-v1".into(),
            evaluation_set_version: "test-set-v1".into(),
            evaluator_version: "test-evaluator-v1".into(),
            evaluator_id: WorkloadId {
                trust_domain: "test.local".into(),
                tenant: posture.tenant.clone(),
                goal: posture.id.clone(),
                node: "n1".into(),
                agent: "evaluator".into(),
                incarnation: "1".into(),
            }
            .uri()
            .unwrap(),
            security: serde_json::to_value(posture).unwrap(),
            capabilities: posture
                .capability_ceiling
                .iter()
                .map(|c| c.id.clone())
                .collect(),
            budget_microusd: posture.limits.total,
            max_depth: posture.max_depth.into(),
            max_agents: posture.max_live_descendants as usize,
            max_open_tasks: posture.max_open_tasks as usize,
            max_needs: 500,
            expiry_ms: posture.expiry * 1000,
            initial_needs: vec![crate::domain::NeedSpec {
                id: "answer".into(),
                parent_id: None,
                description: "produce the synthetic answer".into(),
                predicate: serde_json::json!({"answer":true}),
                version: 1,
                required: true,
                required_capabilities: BTreeSet::new(),
            }],
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn capability() -> Capability {
        Capability {
            id: "c".into(),
            kind: CapabilityKind::ToolCall,
            resource: ResourceSelector::Prefix("tool/data".into()),
            constraints: Constraints {
                operations: ["read".into(), "append".into()].into(),
                resources: ["tool/data/a".into()].into(),
                recipients: BTreeSet::new(),
                read_markings: ["internal".into()].into(),
                max_payload_bytes: 1024,
                arguments: [(
                    "n".into(),
                    ArgumentConstraint::Integer {
                        minimum: 0,
                        maximum: 100,
                    },
                )]
                .into(),
                deny_unknown_arguments: true,
            },
            budget: Limits {
                total: 1000,
                per_call: 100,
                per_tick: 100,
                tick_seconds: 10,
                per_day: 1000,
            },
            confirmation: ConfirmationTier::C1,
            minimum_isolation: IsolationTier::P,
            not_before: 10,
            expiry: 1000,
            goal_id: "g".into(),
            holder: "a".into(),
            parent: None,
            depth: 0,
            grant_version: 1,
        }
    }
    #[test]
    fn one_hundred_thousand_attenuation_cases() {
        let parent = capability();
        let mut seed = 7u64;
        for n in 0..100_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut child = parent.clone();
            child.budget.total = 1 + seed % 1000;
            child.expiry = 11 + seed % 989;
            child.constraints.operations = ["read".into()].into();
            child.resource = ResourceSelector::Exact("tool/data/a".into());
            child.confirmation = ConfirmationTier::C2;
            if n % 2 == 0 {
                assert!(child.attenuates(&parent));
            } else {
                match (seed >> 8) % 8 {
                    0 => child.expiry = 1001,
                    1 => child.budget.total = 1001,
                    2 => child.confirmation = ConfirmationTier::C0,
                    3 => child.resource = ResourceSelector::Prefix("tool".into()),
                    4 => {
                        child.constraints.operations.insert("write".into());
                    }
                    5 => child.minimum_isolation = IsolationTier::W,
                    6 => child.constraints.max_payload_bytes = 1025,
                    _ => child
                        .constraints
                        .arguments
                        .insert(
                            "n".into(),
                            ArgumentConstraint::Integer {
                                minimum: -1,
                                maximum: 100,
                            },
                        )
                        .map(|_| ())
                        .unwrap_or(()),
                };
                assert!(!child.attenuates(&parent));
            }
        }
        assert!(!ResourceSelector::Prefix("tool/data".into()).matches("tool/database"));
        assert!(serde_json::from_str::<CapabilityKind>("\"execute-anything\"").is_err());
    }
    fn registered() -> (test_support::Fixture, SecurityState, Ledger) {
        let f = test_support::fixture();
        let mut state =
            SecurityState::new(f.policy.clone(), &f.signer.public_key(), f.now).unwrap();
        let mut ledger = Ledger::new("microusd".into()).unwrap();
        state
            .register_goal(
                &AuthenticatedSubject::operator(&f.operators[0]),
                f.posture.clone(),
                &f.approvals,
                &mut ledger,
                f.now,
            )
            .unwrap();
        state
            .compile_definition(f.definition.clone(), None, &mut ledger, f.now)
            .unwrap();
        (f, state, ledger)
    }
    fn request(id: &str) -> ActionRequest {
        ActionRequest {
            intent_id: id.into(),
            grant_id: "agent-grant".into(),
            grant_version: 1,
            capability_id: "agent-cap-0".into(),
            budget_capability_id: None,
            envelope_id: None,
            usage_bound: BTreeMap::new(),
            action: Action {
                kind: CapabilityKind::ToolCall,
                resource: "tool/data/a".into(),
                operation: "call".into(),
                arguments: serde_json::json!({}),
                class: OperationClass::Reversible,
                isolation: IsolationTier::P,
                recipient: None,
                evidence: BTreeSet::new(),
            },
        }
    }
    fn compile_unrelated_grant(
        f: &test_support::Fixture,
        state: &mut SecurityState,
        ledger: &mut Ledger,
    ) {
        let mut definition = f.definition.clone();
        definition.id = "unrelated-grant".into();
        definition.holder.agent = "unrelated-agent".into();
        let holder = definition.holder.uri().unwrap();
        for cap in &mut definition.capabilities {
            cap.id = format!("unrelated-{}", cap.id);
            cap.holder = holder.clone();
        }
        state
            .compile_definition(definition, None, ledger, f.now)
            .unwrap();
    }
    fn action_approvals(
        f: &test_support::Fixture,
        state: &SecurityState,
        request: &ActionRequest,
        humans: usize,
    ) -> Vec<Signed<HumanApproval>> {
        let (action, evidence, scope, version) =
            state.approval_binding(&f.subject, request, f.now).unwrap();
        f.operators[..humans]
            .iter()
            .map(|operator| {
                state
                    .issue_approval(
                        operator,
                        action,
                        evidence,
                        version,
                        scope.clone(),
                        f.now + 100,
                        &f.signer,
                        f.now,
                    )
                    .unwrap()
            })
            .collect()
    }
    #[test]
    fn nonbillable_refresh_survives_unrelated_grant_change_once_without_extending_expiry() {
        let (f, mut state, mut ledger) = registered();
        let mut request = request("local-application");
        request.action.class = OperationClass::ReadOnly;
        let original = state
            .authorize(
                &f.subject,
                request.clone(),
                &[],
                None,
                &mut ledger,
                &f.signer,
                f.now,
            )
            .unwrap();
        assert_eq!(original.permit.payload.confirmation, ConfirmationTier::C0);
        compile_unrelated_grant(&f, &mut state, &mut ledger);
        assert!(
            state
                .consume_permit(&f.subject, &original.permit, &mut ledger, f.now + 1)
                .is_err()
        );
        let financial_hash = content_hash(&ledger).unwrap();
        let NonbillablePermitRefresh::Ready { permit, audit } = state
            .refresh_nonbillable_permit(
                &f.subject,
                &request,
                &original.permit,
                &[],
                &mut ledger,
                &f.signer,
                f.now + 1,
            )
            .unwrap()
        else {
            panic!("C0 refresh requested human authority");
        };
        let audit = audit.unwrap();
        assert_eq!(audit.kind, "dispatch_permit_refreshed");
        assert_eq!(audit.payload_hash, content_hash(&permit.payload).unwrap());
        assert_eq!(permit.payload.intent_id, original.permit.payload.intent_id);
        assert_eq!(
            permit.payload.action_hash,
            original.permit.payload.action_hash
        );
        assert_eq!(
            permit.payload.evidence_hash,
            original.permit.payload.evidence_hash
        );
        assert_eq!(
            permit.payload.observed_hash,
            original.permit.payload.observed_hash
        );
        assert_eq!(
            permit.payload.certificate_hash,
            original.permit.payload.certificate_hash
        );
        assert_eq!(
            permit.payload.fencing_token,
            original.permit.payload.fencing_token
        );
        assert_eq!(permit.payload.expiry, original.permit.payload.expiry);
        assert_eq!(permit.payload.grant_epoch, state.grant_epoch);
        assert_eq!(content_hash(&ledger).unwrap(), financial_hash);
        assert!(ledger.intents.is_empty());
        assert!(matches!(
            state
                .refresh_nonbillable_permit(
                    &f.subject,
                    &request,
                    &permit,
                    &[],
                    &mut ledger,
                    &f.signer,
                    f.now + 1,
                )
                .unwrap(),
            NonbillablePermitRefresh::Ready { audit: None, .. }
        ));
        state
            .consume_permit(&f.subject, &permit, &mut ledger, f.now + 1)
            .unwrap();
        assert!(
            state
                .refresh_nonbillable_permit(
                    &f.subject,
                    &request,
                    &permit,
                    &[],
                    &mut ledger,
                    &f.signer,
                    f.now + 1,
                )
                .is_err()
        );
        assert_eq!(content_hash(&ledger).unwrap(), financial_hash);
    }
    #[test]
    fn nonbillable_refresh_requires_fresh_c2_and_c3_humans_after_unrelated_epoch_change() {
        for humans in [1, 2] {
            let (f, mut state, mut ledger) = registered();
            let mut request = request(&format!("privileged-local-{humans}"));
            request.action.class = OperationClass::External;
            if humans == 2 {
                state
                    .record_input(
                        &f.subject,
                        InputRecord {
                            id: "untrusted-evidence".into(),
                            source_type: "external".into(),
                            hash: [9; 32],
                            goal_id: "g".into(),
                            producer: "outside".into(),
                            trust: Trust::Untrusted,
                            markings: BTreeSet::new(),
                            parents: BTreeSet::new(),
                        },
                        &f.signer,
                        f.now,
                    )
                    .unwrap();
            }
            let old_approvals = action_approvals(&f, &state, &request, humans);
            let original = state
                .authorize(
                    &f.subject,
                    request.clone(),
                    &old_approvals,
                    None,
                    &mut ledger,
                    &f.signer,
                    f.now,
                )
                .unwrap();
            assert_eq!(
                original.permit.payload.confirmation,
                if humans == 1 {
                    ConfirmationTier::C2
                } else {
                    ConfirmationTier::C3
                }
            );
            assert!(matches!(
                state
                    .refresh_nonbillable_permit(
                        &f.subject,
                        &request,
                        &original.permit,
                        &[],
                        &mut ledger,
                        &f.signer,
                        f.now,
                    )
                    .unwrap(),
                NonbillablePermitRefresh::Ready { audit: None, .. }
            ));
            compile_unrelated_grant(&f, &mut state, &mut ledger);
            let pending_hash = content_hash(&state).unwrap();
            let financial_hash = content_hash(&ledger).unwrap();
            let NonbillablePermitRefresh::ConfirmationRequired(review) = state
                .refresh_nonbillable_permit(
                    &f.subject,
                    &request,
                    &original.permit,
                    &old_approvals,
                    &mut ledger,
                    &f.signer,
                    f.now,
                )
                .unwrap()
            else {
                panic!("stale human approval refreshed privileged authority");
            };
            assert_eq!(review.required_humans, humans);
            assert_eq!(review.grant_epoch, state.grant_epoch);
            assert_eq!(review.action_hash, original.permit.payload.action_hash);
            assert_eq!(review.expiry, original.permit.payload.expiry);
            assert_eq!(content_hash(&state).unwrap(), pending_hash);
            assert_eq!(content_hash(&ledger).unwrap(), financial_hash);
            let approvals = action_approvals(&f, &state, &request, humans);
            let NonbillablePermitRefresh::Ready { permit, .. } = state
                .refresh_nonbillable_permit(
                    &f.subject,
                    &request,
                    &original.permit,
                    &approvals,
                    &mut ledger,
                    &f.signer,
                    f.now,
                )
                .unwrap()
            else {
                panic!("fresh distinct human approvals were not applied");
            };
            state
                .consume_permit(&f.subject, &permit, &mut ledger, f.now)
                .unwrap();
            assert_eq!(content_hash(&ledger).unwrap(), financial_hash);
        }
    }
    #[test]
    fn nonbillable_refresh_rejects_changed_bindings_expiry_stop_and_retracted_evidence() {
        for case in [
            "payload",
            "lineage",
            "budget",
            "expired",
            "revoked",
            "stopped",
            "retracted",
        ] {
            let (f, mut state, mut ledger) = registered();
            let mut request = request(&format!("refresh-rejected-{case}"));
            request.action.class = OperationClass::ReadOnly;
            if case == "retracted" {
                state
                    .append_finding(
                        &f.subject,
                        "original-evidence".into(),
                        [7; 32],
                        "agent-grant",
                        &f.signer,
                        f.now,
                    )
                    .unwrap();
                request.action.evidence.insert("original-evidence".into());
            }
            let original = state
                .authorize(
                    &f.subject,
                    request.clone(),
                    &[],
                    None,
                    &mut ledger,
                    &f.signer,
                    f.now,
                )
                .unwrap();
            compile_unrelated_grant(&f, &mut state, &mut ledger);
            match case {
                "payload" => request.action.arguments = serde_json::json!({"different":true}),
                "budget" => request.budget_capability_id = Some("agent-cap-6".into()),
                "lineage" => state
                    .record_input(
                        &f.subject,
                        InputRecord {
                            id: "new-input".into(),
                            source_type: "external".into(),
                            hash: [6; 32],
                            goal_id: "g".into(),
                            producer: "outside".into(),
                            trust: Trust::Untrusted,
                            markings: BTreeSet::new(),
                            parents: BTreeSet::new(),
                        },
                        &f.signer,
                        f.now,
                    )
                    .unwrap(),
                "revoked" => state.revoke_grant("agent-grant").unwrap(),
                "stopped" => {
                    state
                        .engage_stop(
                            &f.subject,
                            Scope::Agent {
                                tenant: "t".into(),
                                goal: "g".into(),
                                holder: f.subject.id().into(),
                            },
                            [5; 32],
                            f.now,
                        )
                        .unwrap();
                }
                "retracted" => {
                    state
                        .retract_finding(
                            &AuthenticatedSubject::operator(&f.operators[0]),
                            "original-evidence",
                            [8; 32],
                            &f.signer,
                            f.now,
                        )
                        .unwrap();
                }
                "expired" => {}
                _ => unreachable!(),
            }
            let state_hash = content_hash(&state).unwrap();
            let ledger_hash = content_hash(&ledger).unwrap();
            assert!(
                state
                    .refresh_nonbillable_permit(
                        &f.subject,
                        &request,
                        &original.permit,
                        &[],
                        &mut ledger,
                        &f.signer,
                        if case == "expired" {
                            original.permit.payload.expiry
                        } else {
                            f.now
                        },
                    )
                    .is_err(),
                "invalid refresh accepted: {case}"
            );
            assert_eq!(
                content_hash(&state).unwrap(),
                state_hash,
                "changed pending state: {case}"
            );
            assert_eq!(
                content_hash(&ledger).unwrap(),
                ledger_hash,
                "changed money: {case}"
            );
        }
    }
    #[test]
    fn real_oidc_registration_definition_exact_approval_stop_and_replay() {
        let (f, mut state, mut ledger) = registered();
        let first = state
            .authorize(
                &f.subject,
                request("first"),
                &[],
                None,
                &mut ledger,
                &f.signer,
                f.now,
            )
            .unwrap();
        state
            .consume_permit(&f.subject, &first.permit, &mut ledger, f.now)
            .unwrap();
        assert!(
            state
                .consume_permit(&f.subject, &first.permit, &mut ledger, f.now)
                .is_err()
        );
        let second = state
            .authorize(
                &f.subject,
                request("second"),
                &[],
                None,
                &mut ledger,
                &f.signer,
                f.now,
            )
            .unwrap();
        state
            .engage_stop(
                &f.subject,
                Scope::Agent {
                    tenant: "t".into(),
                    goal: "g".into(),
                    holder: f.subject.id().into(),
                },
                [1; 32],
                f.now,
            )
            .unwrap();
        assert!(
            state
                .consume_permit(&f.subject, &second.permit, &mut ledger, f.now)
                .is_err()
        );
        let restored: SecurityState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(restored.stopped(&Scope::Agent {
            tenant: "t".into(),
            goal: "g".into(),
            holder: f.subject.id().into()
        }));
        assert!(
            state
                .register_goal(
                    &f.subject,
                    f.posture.clone(),
                    &f.approvals,
                    &mut ledger,
                    f.now
                )
                .is_err()
        );
    }
    #[test]
    fn taint_markings_retraction_and_late_input_cannot_launder_authority() {
        let (f, mut state, mut ledger) = registered();
        let pending = state
            .authorize(
                &f.subject,
                request("late"),
                &[],
                None,
                &mut ledger,
                &f.signer,
                f.now,
            )
            .unwrap();
        state
            .record_input(
                &f.subject,
                InputRecord {
                    id: "source".into(),
                    source_type: "external".into(),
                    hash: [1; 32],
                    goal_id: "g".into(),
                    producer: "outside".into(),
                    trust: Trust::Untrusted,
                    markings: BTreeSet::new(),
                    parents: BTreeSet::new(),
                },
                &f.signer,
                f.now,
            )
            .unwrap();
        assert!(
            state
                .consume_permit(&f.subject, &pending.permit, &mut ledger, f.now)
                .is_err()
        );
        assert!(
            state
                .authorize(
                    &f.subject,
                    request("tainted"),
                    &[],
                    None,
                    &mut ledger,
                    &f.signer,
                    f.now
                )
                .is_err()
        );
        let (action_hash, evidence_hash, scope, version) = state
            .approval_binding(&f.subject, &request("approved"), f.now)
            .unwrap();
        let approval = state
            .issue_approval(
                &f.operators[0],
                action_hash,
                evidence_hash,
                version,
                scope,
                f.now + 100,
                &f.signer,
                f.now,
            )
            .unwrap();
        let permitted = state
            .authorize(
                &f.subject,
                request("approved"),
                &[approval],
                None,
                &mut ledger,
                &f.signer,
                f.now,
            )
            .unwrap();
        assert_eq!(permitted.permit.payload.confirmation, ConfirmationTier::C2);
        let finding = state
            .append_finding(
                &f.subject,
                "finding-1".into(),
                [4; 32],
                "agent-grant",
                &f.signer,
                f.now,
            )
            .unwrap();
        assert_eq!(finding.payload.trust, Trust::Untrusted);
        assert!(finding.payload.inputs.contains("source"));
        state
            .retract_finding(
                &AuthenticatedSubject::operator(&f.operators[0]),
                "finding-1",
                [5; 32],
                &f.signer,
                f.now,
            )
            .unwrap();
        assert!(
            state
                .consume_permit(&f.subject, &permitted.permit, &mut ledger, f.now)
                .is_err()
        );
        let mut dependent = request("dependent");
        dependent.action.evidence.insert("finding-1".into());
        assert!(
            state
                .authorize(
                    &f.subject,
                    dependent,
                    &[],
                    None,
                    &mut ledger,
                    &f.signer,
                    f.now
                )
                .is_err()
        );
    }
    #[test]
    fn duplicate_humans_and_agent_approvals_do_not_release_global_stop() {
        let (f, mut state, _) = registered();
        let root = AuthenticatedSubject::operator(&f.operators[0]);
        state
            .engage_stop(&root, Scope::Global, [1; 32], f.now)
            .unwrap();
        let digest = content_hash(&("release-stop", &Scope::Global, state.stop_epoch)).unwrap();
        let a = state
            .issue_approval(
                &f.operators[0],
                digest,
                [0; 32],
                0,
                Scope::Global,
                f.now + 100,
                &f.signer,
                f.now,
            )
            .unwrap();
        assert!(
            state
                .release_stop(&root, Scope::Global, &[a.clone(), a.clone()], f.now)
                .is_err()
        );
        let b = state
            .issue_approval(
                &f.operators[1],
                digest,
                [0; 32],
                0,
                Scope::Global,
                f.now + 100,
                &f.signer,
                f.now,
            )
            .unwrap();
        state
            .release_stop(&root, Scope::Global, &[a, b], f.now)
            .unwrap();
        assert!(!state.stopped(&Scope::Global));
    }
    #[test]
    fn amendments_preserve_unknown_and_settled_money_and_fence_old_authority() {
        use crate::ledger::{Envelope, IntentState, Rate, Receipt};
        let (f, mut state, mut ledger) = registered();
        let root = AuthenticatedSubject::operator(&f.operators[0]);
        ledger
            .allocate(Envelope {
                id: "e".into(),
                placement_intent_id: "p".into(),
                placement_hash: [1; 32],
                parent: None,
                goal_id: "g".into(),
                holder: f.subject.id().into(),
                accounts: ["global".into(), "tenant/t".into(), "goal/g".into()].into(),
                allocation: 1000,
                spent: 0,
                held: 0,
                active: true,
            })
            .unwrap();
        let prices = f
            .signer
            .sign(
                "price-table",
                PriceTable {
                    version: "v1".into(),
                    provider: "local".into(),
                    model: "m1".into(),
                    currency: "microusd".into(),
                    not_before: f.now,
                    expires: f.now + 600,
                    rates: [(
                        "tokens".into(),
                        Rate {
                            numerator: 1,
                            denominator: 1,
                        },
                    )]
                    .into(),
                },
            )
            .unwrap();
        for id in ["unknown", "settled", "reserved"] {
            ledger
                .reserve(
                    ReservationRequest {
                        intent_id: id.into(),
                        envelope_id: "e".into(),
                        goal_id: "g".into(),
                        holder: f.subject.id().into(),
                        capability_accounts: BTreeSet::new(),
                        request_hash: [2; 32],
                        usage_bound: [("tokens".into(), 50)].into(),
                        price_version: "v1".into(),
                    },
                    &prices,
                    &f.signer.public_key(),
                    f.now,
                )
                .unwrap();
        }
        ledger.mark_dispatched("unknown", [3; 32]).unwrap();
        ledger.mark_unknown("unknown").unwrap();
        ledger.mark_dispatched("settled", [4; 32]).unwrap();
        ledger
            .settle(Receipt {
                id: "receipt".into(),
                provider: "local".into(),
                intent_id: "settled".into(),
                request_hash: [2; 32],
                usage: [("tokens".into(), 40)].into(),
            })
            .unwrap();
        let pending = state
            .authorize(
                &f.subject,
                request("stale"),
                &[],
                None,
                &mut ledger,
                &f.signer,
                f.now,
            )
            .unwrap();
        let mut amended = f.posture.clone();
        amended.intent = "approved amended intent".into();
        amended.version = 2;
        for cap in &mut amended.capability_ceiling {
            cap.grant_version = 2;
        }
        amended.limits.total = 100;
        let contract = test_support::goal_spec_posture(&amended);
        bind_goal_contract(&mut amended, &contract).unwrap();
        let (hash, evidence, scope, version) = state.amendment_binding(&amended).unwrap();
        let approvals: Vec<_> = f
            .operators
            .iter()
            .map(|op| {
                state
                    .issue_approval(
                        op,
                        hash,
                        evidence,
                        version,
                        scope.clone(),
                        f.now + 100,
                        &f.signer,
                        f.now,
                    )
                    .unwrap()
            })
            .collect();
        assert!(
            state
                .amend_goal(&root, amended.clone(), &approvals[..1], &mut ledger, f.now)
                .is_err()
        );
        state
            .amend_goal(&root, amended, &approvals, &mut ledger, f.now)
            .unwrap();
        assert_eq!(
            ledger.accounts["goal/g"].total,
            crate::ledger::Counter {
                spent: 40,
                held: 50
            }
        );
        assert_eq!(ledger.intents["reserved"].state, IntentState::Released);
        assert_eq!(ledger.intents["unknown"].state, IntentState::Unknown);
        assert!(!ledger.envelopes["e"].active);
        ledger.validate().unwrap();
        assert_eq!(state.goal_revisions["g"], 2);
        assert_eq!(state.goal_history["g"].len(), 1);
        assert!(
            state
                .consume_permit(&f.subject, &pending.permit, &mut ledger, f.now)
                .is_err()
        );
        assert!(state.grants["agent-grant"].revoked);
        let restored: Ledger =
            serde_json::from_slice(&serde_json::to_vec(&ledger).unwrap()).unwrap();
        restored.validate().unwrap();
        assert_eq!(restored.accounts["goal/g"].total.held, 50);
    }
    #[test]
    fn producer_can_retract_only_with_live_explicit_authority() {
        let (f, mut state, mut ledger) = registered();
        state
            .append_finding(
                &f.subject,
                "mine".into(),
                [1; 32],
                "agent-grant",
                &f.signer,
                f.now,
            )
            .unwrap();
        state
            .retract_finding(&f.subject, "mine", [2; 32], &f.signer, f.now)
            .unwrap();
        let pending = state
            .authorize(
                &f.subject,
                request("pending"),
                &[],
                None,
                &mut ledger,
                &f.signer,
                f.now,
            )
            .unwrap();
        state
            .append_finding(
                &f.subject,
                "second".into(),
                [3; 32],
                "agent-grant",
                &f.signer,
                f.now,
            )
            .unwrap();
        state.revoke_grant("agent-grant").unwrap();
        assert!(
            state
                .retract_finding(&f.subject, "second", [4; 32], &f.signer, f.now)
                .is_err()
        );
        assert!(
            state
                .consume_permit(&f.subject, &pending.permit, &mut ledger, f.now)
                .is_err()
        );
    }
    #[test]
    fn policy_actor_stop_and_flag_are_scoped_and_never_release_or_widen() {
        let mut f = test_support::fixture();
        let scope = Scope::Goal {
            tenant: "t".into(),
            goal: "g".into(),
        };
        let args: BTreeMap<String, ArgumentConstraint> = [
            (
                "scope".into(),
                ArgumentConstraint::Equal {
                    value: serde_json::to_value(&scope).unwrap(),
                },
            ),
            (
                "trigger".into(),
                ArgumentConstraint::Equal {
                    value: Value::String("flagged".into()),
                },
            ),
        ]
        .into();
        for cap in [
            &mut f.posture.capability_ceiling[7],
            &mut f.definition.capabilities[7],
        ] {
            cap.constraints.operations = ["stop".into(), "flag".into()].into();
            cap.constraints.arguments = args.clone();
            cap.constraints.resources.insert("policy/g".into());
        }
        f.posture.resources.insert("policy/g".into());
        f.policy.payload.inventory.insert("policy/g".into());
        f.policy = f
            .signer
            .sign("deployment-policy", f.policy.payload)
            .unwrap();
        let contract = test_support::goal_spec(&f);
        bind_goal_contract(&mut f.posture, &contract).unwrap();
        let mut state =
            SecurityState::new(f.policy.clone(), &f.signer.public_key(), f.now).unwrap();
        let mut ledger = Ledger::new("microusd".into()).unwrap();
        let approval = state
            .issue_approval(
                &f.operators[0],
                f.posture.registration_hash().unwrap(),
                [0; 32],
                0,
                Scope::Tenant { tenant: "t".into() },
                f.now + 100,
                &f.signer,
                f.now,
            )
            .unwrap();
        state
            .register_goal(
                &AuthenticatedSubject::operator(&f.operators[0]),
                f.posture.clone(),
                &[approval],
                &mut ledger,
                f.now,
            )
            .unwrap();
        state
            .compile_definition(f.definition.clone(), None, &mut ledger, f.now)
            .unwrap();
        let intervention = PolicyIntervention {
            scope: scope.clone(),
            trigger: "flagged".into(),
            reason_hash: [7; 32],
            confidence_milli: 900,
        };
        let mut req = request("policy-stop");
        req.capability_id = "agent-cap-7".into();
        req.action.kind = CapabilityKind::PolicyRequest;
        req.action.operation = "stop".into();
        req.action.class = OperationClass::ReadOnly;
        req.action.arguments = serde_json::to_value(&intervention).unwrap();
        let mut flag = req.clone();
        flag.intent_id = "policy-flag".into();
        flag.action.operation = "flag".into();
        flag.action.resource = "policy/g".into();
        let stop_epoch = state.stop_epoch;
        let grant_hash = content_hash(&state.grants).unwrap();
        let goal_hash = content_hash(&state.goals).unwrap();
        let ledger_hash = content_hash(&ledger).unwrap();
        let decision = state
            .policy_flag(&f.subject, flag.clone(), &mut ledger, &f.signer, f.now)
            .unwrap();
        assert!(!decision.permit.payload.billable);
        assert_eq!(state.stop_epoch, stop_epoch);
        assert_eq!(content_hash(&state.grants).unwrap(), grant_hash);
        assert_eq!(content_hash(&state.goals).unwrap(), goal_hash);
        assert_eq!(content_hash(&ledger).unwrap(), ledger_hash);
        assert!(!state.stopped(&scope));
        let mut escaped_flag = flag.clone();
        escaped_flag.intent_id = "escaped-flag".into();
        escaped_flag.action.arguments["scope"] = serde_json::to_value(Scope::Global).unwrap();
        assert!(
            state
                .policy_flag(&f.subject, escaped_flag, &mut ledger, &f.signer, f.now)
                .is_err()
        );
        for operation in ["release", "refund", "amend", "widen"] {
            let mut forbidden = flag.clone();
            forbidden.intent_id = format!("forbidden-{operation}");
            forbidden.action.operation = operation.into();
            assert!(
                state
                    .policy_flag(&f.subject, forbidden, &mut ledger, &f.signer, f.now)
                    .is_err()
            );
        }
        let mut funded = flag;
        funded.intent_id = "funded-flag".into();
        funded.usage_bound.insert("input_tokens".into(), 1);
        assert!(
            state
                .policy_flag(&f.subject, funded, &mut ledger, &f.signer, f.now)
                .is_err()
        );
        assert_eq!(content_hash(&ledger).unwrap(), ledger_hash);
        let mut escaped = req.clone();
        escaped.intent_id = "escaped".into();
        escaped.action.arguments["scope"] = serde_json::to_value(Scope::Global).unwrap();
        assert!(
            state
                .policy_stop(&f.subject, escaped, &mut ledger, &f.signer, f.now)
                .is_err()
        );
        state
            .policy_stop(&f.subject, req.clone(), &mut ledger, &f.signer, f.now)
            .unwrap();
        assert!(state.stopped(&scope));
        assert!(
            state
                .policy_stop(&f.subject, req, &mut ledger, &f.signer, f.now)
                .is_err()
        );
        assert!(state.release_stop(&f.subject, scope, &[], f.now).is_err());
    }
    #[test]
    fn contract_hash_binds_need_predicates_rubric_and_evaluator() {
        let f = test_support::fixture();
        let spec = test_support::goal_spec(&f);
        assert_eq!(goal_contract_hash(&spec).unwrap(), f.posture.contract_hash);
        let mut changed = spec.clone();
        changed.initial_needs[0].predicate = serde_json::json!({"unconditionally":true});
        assert_ne!(
            goal_contract_hash(&changed).unwrap(),
            f.posture.contract_hash
        );
        changed = spec.clone();
        changed.rubric_version = "other-rubric".into();
        assert_ne!(
            goal_contract_hash(&changed).unwrap(),
            f.posture.contract_hash
        );
        changed = spec;
        changed.evaluator_id = "attacker".into();
        assert_ne!(
            goal_contract_hash(&changed).unwrap(),
            f.posture.contract_hash
        );
    }
    #[test]
    fn privilege_configuration_requires_two_distinct_exact_and_fresh_humans() {
        let (f, mut state, _) = registered();
        let root = AuthenticatedSubject::operator(&f.operators[0]);
        let scope = Scope::Goal {
            tenant: "t".into(),
            goal: "g".into(),
        };
        let conditions = Abac {
            not_before: f.now,
            expiry: f.now + 600,
            max_cost: 0,
            max_confirmation: ConfirmationTier::C0,
            minimum_isolation: IsolationTier::P,
            resources: ["state".into()].into(),
            operations: ["read".into()].into(),
        };
        let role = Role {
            id: "viewer".into(),
            rules: vec![RoleRule {
                verbs: ["read".into()].into(),
                resources: ["state".into()].into(),
                scope: scope.clone(),
                conditions: conditions.clone(),
            }],
        };
        let (hash, evidence, scope_for_approval, version) =
            state.role_change_binding(&role).unwrap();
        let approvals: Vec<_> = f
            .operators
            .iter()
            .map(|op| {
                state
                    .issue_approval(
                        op,
                        hash,
                        evidence,
                        version,
                        scope_for_approval.clone(),
                        f.now + 600,
                        &f.signer,
                        f.now,
                    )
                    .unwrap()
            })
            .collect();
        assert!(
            state
                .create_role(
                    &root,
                    role.clone(),
                    &[approvals[0].clone(), approvals[0].clone()],
                    f.now
                )
                .is_err()
        );
        state
            .create_role(&root, role.clone(), &approvals, f.now)
            .unwrap();
        let binding = Binding {
            id: "viewer-binding".into(),
            role_ref: role.id,
            subjects: ["https://test-issuer.invalid|viewer".into()].into(),
            groups: BTreeSet::new(),
            scope,
            conditions,
        };
        let (hash, evidence, scope, version) = state.binding_change_binding(&binding).unwrap();
        let approvals: Vec<_> = f
            .operators
            .iter()
            .map(|op| {
                state
                    .issue_approval(
                        op,
                        hash,
                        evidence,
                        version,
                        scope.clone(),
                        f.now + 600,
                        &f.signer,
                        f.now,
                    )
                    .unwrap()
            })
            .collect();
        assert!(
            state
                .bind_role(&root, binding.clone(), &approvals, f.now + 301)
                .is_err()
        );
        state.bind_role(&root, binding, &approvals, f.now).unwrap();
    }
}
