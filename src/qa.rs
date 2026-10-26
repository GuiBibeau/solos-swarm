//! Scenario orchestration through authenticated production surfaces and read-only evidence.
use crate::{
    artifacts::{ArtifactMetadata, ArtifactStore},
    audit::{CryptoSigner, Hash, Signed, canonical, content_hash},
    identity::{WorkloadId, verify_workload_certificate, workload_public_key},
    operator::{OperatorCommand, OperatorRequest},
    scenarios::{
        self, Observation, ScenarioDriver, ScenarioKind, ScenarioSpec, Verdict, VerificationReport,
    },
    security::Scope,
    transport::Message,
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey, pkcs8::DecodePrivateKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};
use tokio::sync::Mutex;
use zeroize::{Zeroize, Zeroizing};

pub const QA_VERSION: &str = "authenticated-scenarios-v1";
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QaProfile {
    Quick,
    ReleaseSmall,
    Integration,
    Acceptance,
    Eval,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QaSelection {
    pub profile: QaProfile,
    pub scenarios: Vec<ScenarioKind>,
    pub changed_components: Vec<String>,
}
impl QaSelection {
    pub fn resolve(&self) -> Result<Vec<ScenarioSpec>> {
        let all = scenarios::targeted_scenarios();
        let selected = if !self.scenarios.is_empty() {
            let ids = self
                .scenarios
                .iter()
                .map(serde_json::to_string)
                .collect::<Result<BTreeSet<_>, _>>()?;
            ensure!(
                ids.len() == self.scenarios.len(),
                "duplicate required scenario selection"
            );
            all.into_iter()
                .filter(|spec| {
                    ids.contains(&serde_json::to_string(&spec.id).expect("scenario serializable"))
                })
                .collect::<Vec<_>>()
        } else if !self.changed_components.is_empty() || !matches!(self.profile, QaProfile::Quick) {
            // Unknown/shared impact broadens to the complete deterministic set, never an empty run.
            all
        } else {
            anyhow::bail!("required scenario selection is empty")
        };
        ensure!(!selected.is_empty(), "required scenario selection is empty");
        if matches!(self.profile, QaProfile::ReleaseSmall) {
            ensure!(
                selected.len() == scenarios::targeted_scenarios().len(),
                "release-small control checks cannot be deselected"
            );
        }
        Ok(selected)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsIdentityFiles {
    pub workload: WorkloadId,
    pub certificate_der: PathBuf,
    pub private_key_der: PathBuf,
    pub ca_der: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorSession {
    pub endpoint: String,
    pub ca_pem: PathBuf,
    /// Existing real OIDC session: {session_id, token}. QA cannot mint production operators.
    pub session_file: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSession {
    pub socket: PathBuf,
    pub endpoint: String,
    pub identity: TlsIdentityFiles,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiverProbe {
    pub endpoint: String,
    pub ca_pem: PathBuf,
    pub identity: Option<TlsIdentityFiles>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QaConfig {
    pub run_id: String,
    pub seed: u64,
    pub runtime_digest: String,
    pub configuration_digest: String,
    pub operator: OperatorSession,
    pub agents: BTreeMap<String, AgentSession>,
    pub receivers: BTreeMap<String, ReceiverProbe>,
    pub controller: TlsIdentityFiles,
    pub evidence_key_file: PathBuf,
    pub recipes: Vec<ScenarioRecipe>,
    pub request_timeout_ms: u64,
    pub max_response_bytes: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "surface", rename_all = "snake_case", deny_unknown_fields)]
// A small frozen recipe keeps the complete typed operator contract inline for review.
#[allow(clippy::large_enum_variant)]
pub enum QaAction {
    Operator {
        operation: OperatorCommand,
    },
    Agent {
        session: String,
        path: String,
        arguments: Value,
    },
    ReceiverRead {
        receiver: String,
    },
    VerifyEvidence {
        snapshot: PathBuf,
        head: PathBuf,
        trusted_key: Hash,
        fault: Option<EvidenceFault>,
    },
    VerifySelection {
        selection: QaSelection,
    },
    WaitState {
        scope: Scope,
        condition: StateCondition,
        timeout_ms: u64,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "predicate", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateCondition {
    Exists { pointer: String },
    Equals { pointer: String, value: Value },
    NumberAtLeast { pointer: String, minimum: u64 },
}
impl StateCondition {
    fn satisfied(&self, value: &Value) -> bool {
        match self {
            Self::Exists { pointer } => value.pointer(pointer).is_some_and(|item| !item.is_null()),
            Self::Equals {
                pointer,
                value: expected,
            } => value.pointer(pointer) == Some(expected),
            Self::NumberAtLeast { pointer, minimum } => value
                .pointer(pointer)
                .and_then(Value::as_u64)
                .is_some_and(|value| value >= *minimum),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceFault {
    WrongReceipt,
    MissingAuditEvent,
    MissingReceipt,
    EmptyAudit,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionStep {
    pub id: String,
    pub action: QaAction,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionGroup {
    pub concurrent: bool,
    pub steps: Vec<ActionStep>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "predicate", rename_all = "snake_case", deny_unknown_fields)]
pub enum Assertion {
    Success {
        id: String,
        source: String,
        expected: bool,
    },
    Value {
        id: String,
        source: String,
        pointer: String,
        expected: Value,
    },
    Exists {
        id: String,
        source: String,
        pointer: String,
    },
    Absent {
        id: String,
        source: String,
        pointer: String,
    },
    NumberAtLeast {
        id: String,
        source: String,
        pointer: String,
        minimum: u64,
    },
    NumberDelta {
        id: String,
        before: String,
        after: String,
        pointer: String,
        delta: i64,
    },
    ExactlyOneSuccess {
        id: String,
        sources: Vec<String>,
    },
    SameValue {
        id: String,
        left: String,
        right: String,
        pointer: String,
    },
}
impl Assertion {
    fn id(&self) -> &str {
        match self {
            Self::Success { id, .. }
            | Self::Value { id, .. }
            | Self::Exists { id, .. }
            | Self::Absent { id, .. }
            | Self::NumberAtLeast { id, .. }
            | Self::NumberDelta { id, .. }
            | Self::ExactlyOneSuccess { id, .. }
            | Self::SameValue { id, .. } => id,
        }
    }
    fn evaluate(&self, observed: &BTreeMap<String, ActionObservation>) -> Result<bool> {
        let read = |source: &str| {
            observed
                .get(source)
                .context("assertion observation missing")
        };
        let pointer = |source: &str, path: &str| -> Result<Value> {
            Ok(read(source)?
                .value
                .pointer(path)
                .context("required evidence field missing")?
                .clone())
        };
        Ok(match self {
            Self::Success {
                source, expected, ..
            } => read(source)?.ok == *expected,
            Self::Value {
                source,
                pointer: path,
                expected,
                ..
            } => pointer(source, path)? == *expected,
            Self::Exists {
                source,
                pointer: path,
                ..
            } => !pointer(source, path)?.is_null(),
            Self::Absent {
                source,
                pointer: path,
                ..
            } => {
                let (parent, key) = path
                    .rsplit_once('/')
                    .context("absence assertion needs a parent path")?;
                pointer(source, parent)?
                    .as_object()
                    .context("absence parent evidence missing")?
                    .get(key)
                    .is_none()
            }
            Self::NumberAtLeast {
                source,
                pointer: path,
                minimum,
                ..
            } => {
                pointer(source, path)?
                    .as_u64()
                    .context("bounded counter evidence missing")?
                    >= *minimum
            }
            Self::NumberDelta {
                before,
                after,
                pointer: path,
                delta,
                ..
            } => {
                let before = pointer(before, path)?
                    .as_i64()
                    .context("counter evidence is not bounded integer")?;
                let after = pointer(after, path)?
                    .as_i64()
                    .context("counter evidence is not bounded integer")?;
                after.checked_sub(before) == Some(*delta)
            }
            Self::ExactlyOneSuccess { sources, .. } => {
                ensure!(
                    sources.len() >= 2,
                    "claim race requires at least two actual contenders"
                );
                sources
                    .iter()
                    .map(|source| read(source).map(|item| usize::from(item.ok)))
                    .collect::<Result<Vec<_>>>()?
                    .iter()
                    .sum::<usize>()
                    == 1
            }
            Self::SameValue {
                left,
                right,
                pointer: path,
                ..
            } => pointer(left, path)? == pointer(right, path)?,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioRecipe {
    pub spec: ScenarioSpec,
    pub fixture_id: String,
    pub owned_scope: Scope,
    pub expires_ms: u64,
    pub inputs: Value,
    pub setup: Vec<ActionStep>,
    pub readiness: Vec<Assertion>,
    pub action: Vec<ActionGroup>,
    pub assertions: Vec<Assertion>,
    /// Existing run-owned units only. systemctl show is inspection, never launch/kill authority.
    pub owned_worker_units: Vec<String>,
}
impl ScenarioRecipe {
    pub fn validate(&self, now_ms: u64) -> Result<()> {
        ensure!(
            self.expires_ms > now_ms
                && !self.fixture_id.is_empty()
                && self.spec.timeout_ms > 0
                && self.spec.max_agents > 0
                && !self.setup.is_empty()
                && !self.readiness.is_empty()
                && !self.action.is_empty()
                && !self.assertions.is_empty()
                && !self.spec.cleanup_owner.is_empty(),
            "scenario inputs/setup/readiness/actions/assertions/ownership incomplete"
        );
        ensure!(
            matches!(&self.owned_scope, Scope::Goal { .. }),
            "QA cleanup may own exactly one Goal scope"
        );
        let mut ids = BTreeSet::new();
        for step in self
            .setup
            .iter()
            .chain(self.action.iter().flat_map(|group| &group.steps))
        {
            ensure!(
                !step.id.is_empty() && ids.insert(step.id.clone()),
                "scenario operation IDs must be unique"
            );
            if let QaAction::Agent { path, .. } = &step.action {
                ensure!(
                    path == "/model" || path.starts_with("/tool/"),
                    "QA may call only normal agent surfaces"
                );
            }
            if let QaAction::Operator { operation } = &step.action {
                match operation {
                    OperatorCommand::EngageStop { scope, .. }
                    | OperatorCommand::ReleaseStop { scope, .. }
                    | OperatorCommand::ReadState { scope } => ensure!(
                        self.owned_scope.contains(scope),
                        "scenario operation escapes owned Goal"
                    ),
                    OperatorCommand::RegisterGoal { spec, .. }
                    | OperatorCommand::AmendGoal { spec, .. } => ensure!(
                        scope_goal(&self.owned_scope) == Some(spec.goal_id.as_str()),
                        "scenario registration escapes owned Goal"
                    ),
                    OperatorCommand::ApplySwarm { spec } => ensure!(
                        scope_goal(&self.owned_scope) == Some(spec.goal_id.as_str())
                            && spec.agents.len() <= self.spec.max_agents,
                        "scenario swarm escapes owned Goal/resource bound"
                    ),
                    _ => anyhow::bail!(
                        "deployment policy/RBAC changes are not QA fixture setup surfaces"
                    ),
                }
            }
            if let QaAction::WaitState {
                scope, timeout_ms, ..
            } = &step.action
            {
                ensure!(
                    self.owned_scope.contains(scope)
                        && *timeout_ms > 0
                        && *timeout_ms <= self.spec.timeout_ms,
                    "readiness wait exceeds owned scope/deadline"
                );
            }
        }
        let steps = self
            .setup
            .iter()
            .chain(self.action.iter().flat_map(|group| &group.steps))
            .collect::<Vec<_>>();
        ensure!(
            steps
                .iter()
                .filter(|step| matches!(&step.action,QaAction::Agent{path,..} if path=="/model"))
                .count()
                <= self.spec.max_provider_calls,
            "scenario exceeds declared model-call bound"
        );
        ensure!(
            steps
                .iter()
                .filter_map(
                    |step| if let QaAction::Agent { session, .. } = &step.action {
                        Some(session)
                    } else {
                        None
                    }
                )
                .collect::<BTreeSet<_>>()
                .len()
                <= self.spec.max_agents,
            "scenario exceeds declared agent bound"
        );
        let assertions = self
            .assertions
            .iter()
            .map(Assertion::id)
            .collect::<BTreeSet<_>>();
        ensure!(
            required_assertions(self.spec.id)
                .iter()
                .all(|id| assertions.contains(id)),
            "scenario omits a required observable assertion"
        );
        self.validate_assertion_surfaces(&steps)?;
        if self.spec.id == ScenarioKind::DeniedProviderReceiver {
            ensure!(
                self.action
                    .iter()
                    .flat_map(|group| &group.steps)
                    .filter(|step| matches!(step.action, QaAction::ReceiverRead { .. }))
                    .count()
                    >= 3,
                "denied-call proof requires independent receiver observations and a permitted control"
            );
        }
        for unit in &self.owned_worker_units {
            ensure!(
                unit.starts_with("solos-agent-")
                    && unit.ends_with(".service")
                    && unit.len() <= 128
                    && unit
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
                "cleanup probe must name only owned worker units"
            );
        }
        Ok(())
    }
    fn validate_assertion_surfaces(&self, steps: &[&ActionStep]) -> Result<()> {
        let source = |id: &str| {
            steps
                .iter()
                .find(|step| step.id == id)
                .map(|step| &step.action)
                .context("assertion source is not a real operation")
        };
        for assertion in &self.assertions {
            match assertion {
                Assertion::ExactlyOneSuccess { id, sources } if id == "one-winner" => {
                    ensure!(
                        self.action
                            .iter()
                            .any(|group| group.concurrent && group.steps.len() >= 2),
                        "claim race must dispatch concurrently"
                    );
                    for id in sources {
                        ensure!(
                            matches!(source(id)?,QaAction::Agent{path,..} if path=="/tool/claim_task"),
                            "claim-race winner must be a normal Task claim"
                        );
                    }
                }
                Assertion::NumberDelta {
                    id,
                    before,
                    after,
                    delta,
                    ..
                } if [
                    "receiver-no-denied-effect",
                    "receiver-no-stopped-effect",
                    "receiver-no-replay",
                    "no-replay",
                    "receiver-probe-control",
                ]
                .contains(&id.as_str()) =>
                {
                    ensure!(
                        matches!(source(before)?, QaAction::ReceiverRead { .. })
                            && matches!(source(after)?, QaAction::ReceiverRead { .. }),
                        "no-effect evidence must come from an independent receiver"
                    );
                    ensure!(
                        *delta == if id == "receiver-probe-control" { 1 } else { 0 },
                        "receiver control/prohibited-effect delta changed"
                    );
                }
                Assertion::Success {
                    id,
                    source: id_source,
                    expected,
                } if id == "corrupt-evidence-rejected" => ensure!(
                    !expected
                        && matches!(
                            source(id_source)?,
                            QaAction::VerifyEvidence { fault: Some(_), .. }
                        ),
                    "negative evidence check must verify a labeled corrupted copy"
                ),
                Assertion::Success {
                    id,
                    source: id_source,
                    expected,
                } if id == "evidence-valid" => ensure!(
                    *expected
                        && matches!(
                            source(id_source)?,
                            QaAction::VerifyEvidence { fault: None, .. }
                        ),
                    "positive evidence check must use an unchanged retained snapshot"
                ),
                Assertion::Success {
                    id,
                    source: id_source,
                    expected,
                } if id == "required-empty-not-pass" => ensure!(
                    !expected
                        && matches!(source(id_source)?,QaAction::VerifySelection{selection} if selection.scenarios.is_empty() && selection.changed_components.is_empty() && matches!(selection.profile,QaProfile::Quick)),
                    "empty-selection check must exercise the actual selector"
                ),
                Assertion::Success {
                    id,
                    source: id_source,
                    expected,
                } if ["denied-call", "stopped-denial", "permitted-control"]
                    .contains(&id.as_str()) =>
                {
                    ensure!(
                        *expected == (id == "permitted-control")
                            && matches!(source(id_source)?,QaAction::Agent{path,..} if path=="/model"),
                        "provider outcome assertion must bind a normal model call"
                    )
                }
                Assertion::Absent {
                    id,
                    source: id_source,
                    ..
                } if id == "no-goal-admitted" => ensure!(
                    matches!(
                        source(id_source)?,
                        QaAction::Operator {
                            operation: OperatorCommand::ReadState { .. }
                        }
                    ),
                    "admission absence must be an authoritative normal readback"
                ),
                Assertion::NumberAtLeast { id, minimum, .. } if id == "hold-retained" => ensure!(
                    *minimum > 0,
                    "unknown outcome must retain a positive funded hold"
                ),
                _ => {}
            }
        }
        let find = |id: &str| {
            self.assertions
                .iter()
                .find(|assertion| assertion.id() == id)
                .context("required assertion missing")
        };
        use ScenarioKind::*;
        match self.spec.id {
            ClaimRace => ensure!(
                matches!(find("one-winner")?, Assertion::ExactlyOneSuccess { .. }),
                "claim race needs an observed unique winner"
            ),
            DeniedProviderReceiver => ensure!(
                matches!(
                    find("receiver-probe-control")?,
                    Assertion::NumberDelta { .. }
                ) && matches!(
                    find("receiver-no-denied-effect")?,
                    Assertion::NumberDelta { .. }
                ),
                "denied receiver proof requires both measured deltas"
            ),
            UnknownOutcome | MissingUsage => ensure!(
                matches!(find("hold-retained")?, Assertion::NumberAtLeast { .. }),
                "funded unknown outcome needs a positive hold readback"
            ),
            PauseStop => ensure!(
                matches!(
                    find("receiver-no-stopped-effect")?,
                    Assertion::NumberDelta { .. }
                ),
                "stop needs independent no-effect readback"
            ),
            VerifyNoReplay => ensure!(
                matches!(find("receiver-no-replay")?, Assertion::NumberDelta { .. }),
                "read-only verification needs independent receiver observation"
            ),
            RegistrationOverCeiling | SetupFailure => {
                ensure!(
                    matches!(find("no-goal-admitted")?, Assertion::Absent { .. }),
                    "admission failure must inspect absent Goal state"
                );
                let ceiling = self.inputs["ceiling_microusd"]
                    .as_u64()
                    .context("over-ceiling fixture ceiling missing")?;
                ensure!(steps.iter().any(|step|matches!(&step.action,QaAction::Operator{operation:OperatorCommand::RegisterGoal{spec,..}} if spec.budget_microusd>ceiling)),"over-ceiling fixture does not request above the ceiling");
            }
            MissingReceipt | MissingEvent => {
                ensure!(
                    matches!(
                        find("corrupt-evidence-rejected")?,
                        Assertion::Success {
                            expected: false,
                            ..
                        }
                    ),
                    "negative evidence must be rejected"
                );
                ensure!(self.readiness.iter().any(|assertion|matches!(assertion,Assertion::Success{source:id,expected:true,..} if matches!(source(id),Ok(QaAction::VerifyEvidence{fault:None,..})))) ,"corrupted-evidence test requires a passing unchanged control first");
            }
            EmptySelection => ensure!(
                matches!(
                    find("required-empty-not-pass")?,
                    Assertion::Success {
                        expected: false,
                        ..
                    }
                ),
                "empty required selection cannot pass"
            ),
            _ => {}
        }
        Ok(())
    }
}
fn scope_goal(scope: &Scope) -> Option<&str> {
    if let Scope::Goal { goal, .. } = scope {
        Some(goal)
    } else {
        None
    }
}
pub fn required_assertions(id: ScenarioKind) -> &'static [&'static str] {
    use ScenarioKind::*;
    match id {
        RegistrationOverCeiling => &["rejection", "no-goal-admitted"],
        ClaimRace => &["one-winner", "recorded-owner"],
        DeniedProviderReceiver => &[
            "permitted-control",
            "receiver-probe-control",
            "denied-call",
            "receiver-no-denied-effect",
        ],
        UnknownOutcome => &["unknown-retained", "hold-retained", "no-replay"],
        MissingUsage => &["unknown-or-denied", "hold-retained"],
        PauseStop => &["stopped-denial", "receiver-no-stopped-effect"],
        ModuleUnload => &["services-withdrawn", "cleanup-idempotent"],
        FindingRetraction => &["lineage-flagged", "need-reopened", "candidate-rejected"],
        VerifyNoReplay => &["evidence-valid", "receiver-no-replay"],
        MissingReceipt | MissingEvent => &["corrupt-evidence-rejected"],
        EmptySelection => &["required-empty-not-pass"],
        SetupFailure => &["setup-failure-contained", "no-goal-admitted"],
        ExplainRevision => &["consistent-revision", "source-evidence-retained"],
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionObservation {
    pub id: String,
    pub surface: String,
    pub ok: bool,
    pub value: Value,
    pub recorded_ms: u64,
    pub request_digest: String,
    pub evidence_ref: String,
}
pub type ClientFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
pub trait QaClient: Send + Sync {
    fn invoke<'a>(
        &'a self,
        request_id: &'a str,
        action: &'a QaAction,
    ) -> ClientFuture<'a, ActionObservation>;
    fn worker_inactive<'a>(&'a self, unit: &'a str) -> ClientFuture<'a, bool>;
}
struct AgentClient {
    client: reqwest::Client,
    endpoint: String,
    key: SigningKey,
}
#[derive(Deserialize)]
struct OperatorCredentials {
    session_id: String,
    token: String,
}
impl Drop for OperatorCredentials {
    fn drop(&mut self) {
        self.session_id.zeroize();
        self.token.zeroize();
    }
}
pub struct AuthenticatedClient {
    operator: reqwest::Client,
    operator_endpoint: String,
    operator_session: OperatorCredentials,
    agents: BTreeMap<String, AgentClient>,
    receivers: BTreeMap<String, (reqwest::Client, String)>,
    maximum: usize,
}
fn private_bytes(path: &Path, maximum: usize) -> Result<Zeroizing<Vec<u8>>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= maximum as u64,
        "private client file missing or oversized"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "client credentials require owner-only permissions"
        );
    }
    Ok(Zeroizing::new(fs::read(path)?))
}
fn pem(label: &str, der: &[u8]) -> String {
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        STANDARD.encode(der)
    )
}
fn client_identity(files: &TlsIdentityFiles) -> Result<(reqwest::Identity, SigningKey)> {
    let certificate = fs::read(&files.certificate_der)?;
    let ca = fs::read(&files.ca_der)?;
    verify_workload_certificate(
        &certificate,
        &ca,
        &files.workload,
        crate::cluster::now_ms() / 1000,
    )?;
    let private = private_bytes(&files.private_key_der, 64 * 1024)?;
    let key = SigningKey::from_pkcs8_der(&private)?;
    ensure!(
        key.verifying_key().to_bytes() == workload_public_key(&certificate)?,
        "client SVID/key mismatch"
    );
    let encoded = Zeroizing::new(format!(
        "{}{}",
        pem("CERTIFICATE", &certificate),
        pem("PRIVATE KEY", &private)
    ));
    Ok((reqwest::Identity::from_pem(encoded.as_bytes())?, key))
}
impl AuthenticatedClient {
    pub fn open(config: &QaConfig) -> Result<Self> {
        ensure!(
            config.operator.endpoint.starts_with("https://")
                && (1..=60_000).contains(&config.request_timeout_ms)
                && (1024..=256 * 1024 * 1024).contains(&config.max_response_bytes),
            "real TLS operator and finite request limits required"
        );
        let builder = || {
            reqwest::Client::builder()
                .https_only(true)
                .min_tls_version(reqwest::tls::Version::TLS_1_3)
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_millis(config.request_timeout_ms))
        };
        let operator = builder()
            .add_root_certificate(reqwest::Certificate::from_pem(&fs::read(
                &config.operator.ca_pem,
            )?)?)
            .build()?;
        let operator_session: OperatorCredentials =
            serde_json::from_slice(&private_bytes(&config.operator.session_file, 64 * 1024)?)?;
        ensure!(
            !operator_session.token.is_empty() && !operator_session.session_id.is_empty(),
            "real OIDC session token/handle required"
        );
        let mut agents = BTreeMap::new();
        for (id, session) in &config.agents {
            ensure!(
                session.endpoint.starts_with("https://") && session.socket.is_absolute(),
                "agent TLS/socket scope incomplete"
            );
            let (identity, key) = client_identity(&session.identity)?;
            let client = builder()
                .unix_socket(session.socket.clone())
                .identity(identity)
                .add_root_certificate(reqwest::Certificate::from_der(&fs::read(
                    &session.identity.ca_der,
                )?)?)
                .build()?;
            agents.insert(
                id.clone(),
                AgentClient {
                    client,
                    endpoint: session.endpoint.clone(),
                    key,
                },
            );
        }
        let mut receivers = BTreeMap::new();
        for (id, probe) in &config.receivers {
            ensure!(
                probe.endpoint.starts_with("https://"),
                "receiver observer needs a separate TLS endpoint"
            );
            let mut client = builder()
                .add_root_certificate(reqwest::Certificate::from_pem(&fs::read(&probe.ca_pem)?)?);
            if let Some(identity) = &probe.identity {
                client = client.identity(client_identity(identity)?.0);
            }
            receivers.insert(id.clone(), (client.build()?, probe.endpoint.clone()));
        }
        Ok(Self {
            operator,
            operator_endpoint: config.operator.endpoint.trim_end_matches('/').into(),
            operator_session,
            agents,
            receivers,
            maximum: config.max_response_bytes,
        })
    }
    async fn response(&self, mut response: reqwest::Response) -> Result<(bool, Value)> {
        let ok = response.status().is_success();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= self.maximum,
                "QA response exceeds declared byte ceiling"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok((
            ok,
            serde_json::from_slice(&bytes).context("normal surface returned invalid JSON")?,
        ))
    }
}
impl QaClient for AuthenticatedClient {
    fn invoke<'a>(
        &'a self,
        request_id: &'a str,
        action: &'a QaAction,
    ) -> ClientFuture<'a, ActionObservation> {
        Box::pin(async move {
            let digest = crate::kernel::hex(&content_hash(action)?);
            let (surface, ok, value) = match action {
                QaAction::Operator { operation } => {
                    let response = self
                        .operator
                        .post(format!("{}/operator/command", self.operator_endpoint))
                        .bearer_auth(&self.operator_session.token)
                        .header("x-swarm-session", &self.operator_session.session_id)
                        .json(&OperatorRequest {
                            request_id: request_id.into(),
                            operation: operation.clone(),
                        })
                        .send()
                        .await?;
                    let (ok, value) = self.response(response).await?;
                    ("operator/command".into(), ok, value)
                }
                QaAction::Agent {
                    session,
                    path,
                    arguments,
                } => {
                    ensure!(
                        path == "/model" || path.starts_with("/tool/"),
                        "QA cannot call evaluator or control endpoints as an agent"
                    );
                    let session = self
                        .agents
                        .get(session)
                        .context("admitted agent session missing")?;
                    let mut message = Message {
                        traceparent: None,
                        tracestate: None,
                        request_id: request_id.into(),
                        issued_ms: crate::cluster::now_ms(),
                        path: path.clone(),
                        payload: serde_json::to_string(arguments)?,
                        signature: String::new(),
                    };
                    message.signature =
                        STANDARD.encode(session.key.sign(&message.signing_bytes()?).to_bytes());
                    let response = session
                        .client
                        .post(format!(
                            "{}{}",
                            session.endpoint.trim_end_matches('/'),
                            path
                        ))
                        .headers(message.trace_headers()?)
                        .json(&message)
                        .send()
                        .await?;
                    let (ok, value) = self.response(response).await?;
                    (path.clone(), ok, value)
                }
                QaAction::ReceiverRead { receiver } => {
                    let (client, url) = self
                        .receivers
                        .get(receiver)
                        .context("independent receiver observer missing")?;
                    let (ok, value) = self.response(client.get(url).send().await?).await?;
                    ("independent-receiver-read".into(), ok, value)
                }
                QaAction::VerifyEvidence {
                    snapshot,
                    head,
                    trusted_key,
                    fault,
                } => {
                    let control = if let Some(fault) = fault {
                        verify_evidence(snapshot, head, trusted_key, None)
                            .and_then(|_| negative_control_ready(snapshot, fault))
                    } else {
                        Ok(())
                    };
                    let control_ready = control.is_ok();
                    let result = control
                        .and_then(|_| verify_evidence(snapshot, head, trusted_key, fault.as_ref()));
                    let (ok, reason) = match result {
                        Ok(()) => (true, "recorded integrity checks passed".to_owned()),
                        Err(error) => (false, error.to_string()),
                    };
                    (
                        if fault.is_some() {
                            "isolated-negative-evidence-copy"
                        } else {
                            "read-only-evidence-verification"
                        }
                        .into(),
                        ok,
                        json!({"verdict":if !control_ready{"INCONCLUSIVE"}else if ok{"PASS"}else{"FAIL"},"control_ready":control_ready,"reason":reason,"runtime_storage_mutated":false}),
                    )
                }
                QaAction::VerifySelection { selection } => {
                    let result = selection.resolve();
                    let ok = result.is_ok();
                    (
                        "read-only-selection-verification".into(),
                        ok,
                        json!({"verdict":if ok{"PASS"}else{"INCONCLUSIVE"},"selected":result.map(|selected|selected.len()).unwrap_or(0)}),
                    )
                }
                QaAction::WaitState {
                    scope,
                    condition,
                    timeout_ms,
                } => {
                    ensure!(
                        *timeout_ms > 0 && *timeout_ms <= 60_000,
                        "finite readback deadline required"
                    );
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(*timeout_ms);
                    let mut index = 0usize;
                    let (ok, value) = loop {
                        let response = self
                            .operator
                            .post(format!("{}/operator/command", self.operator_endpoint))
                            .bearer_auth(&self.operator_session.token)
                            .header("x-swarm-session", &self.operator_session.session_id)
                            .json(&OperatorRequest {
                                request_id: format!("{request_id}-poll-{index}"),
                                operation: OperatorCommand::ReadState {
                                    scope: scope.clone(),
                                },
                            })
                            .send()
                            .await?;
                        let (ok, value) = self.response(response).await?;
                        if !ok || condition.satisfied(&value) {
                            break (ok, value);
                        }
                        if tokio::time::Instant::now() >= deadline {
                            break (false, value);
                        }
                        index += 1;
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    };
                    ("normal-readiness-state-read".into(), ok, value)
                }
            };
            Ok(ActionObservation {
                id: request_id.into(),
                surface,
                ok,
                value,
                recorded_ms: crate::cluster::now_ms(),
                request_digest: digest,
                evidence_ref: String::new(),
            })
        })
    }
    fn worker_inactive<'a>(&'a self, unit: &'a str) -> ClientFuture<'a, bool> {
        Box::pin(async move {
            ensure!(
                unit.starts_with("solos-agent-") && unit.ends_with(".service"),
                "unowned supervisor unit denied"
            );
            let output = tokio::process::Command::new("/usr/bin/systemctl")
                .args([
                    "--user",
                    "show",
                    "--property=LoadState",
                    "--property=ActiveState",
                    unit,
                ])
                .kill_on_drop(true)
                .output()
                .await?;
            ensure!(
                output.status.success() && output.stdout.len() <= 4096,
                "supervisor readback unavailable"
            );
            let state = std::str::from_utf8(&output.stdout)?;
            ensure!(
                !state.contains("LoadState=not-found"),
                "owned worker was never observed by supervisor"
            );
            Ok(state.contains("ActiveState=inactive") || state.contains("ActiveState=failed"))
        })
    }
}
fn negative_control_ready(snapshot: &Path, fault: &EvidenceFault) -> Result<()> {
    let state: Value = serde_json::from_slice(&fs::read(snapshot)?)?;
    match fault {
        EvidenceFault::WrongReceipt | EvidenceFault::MissingReceipt => ensure!(
            state["ledger"]["intents"]
                .as_object()
                .is_some_and(|intents| intents.values().any(|intent| !intent["receipt"].is_null())),
            "negative receipt control requires a real settled receipt"
        ),
        EvidenceFault::MissingAuditEvent | EvidenceFault::EmptyAudit => ensure!(
            state["audit"]["events"]
                .as_array()
                .is_some_and(|events| !events.is_empty()),
            "negative audit control requires real recorded events"
        ),
    }
    Ok(())
}

pub fn verify_evidence(
    snapshot: &Path,
    head: &Path,
    trusted_key: &Hash,
    fault: Option<&EvidenceFault>,
) -> Result<()> {
    let mut state: Value = serde_json::from_slice(&fs::read(snapshot)?)?;
    // Negative controls edit only an in-memory exported copy. They cannot alter runtime state.
    if let Some(fault) = fault {
        match fault {
            EvidenceFault::WrongReceipt | EvidenceFault::MissingReceipt => {
                let intents = state["ledger"]["intents"]
                    .as_object_mut()
                    .context("receipt evidence missing")?;
                let intent = intents
                    .values_mut()
                    .find(|intent| !intent["receipt"].is_null())
                    .context("settled receipt control missing")?;
                if matches!(fault, EvidenceFault::WrongReceipt) {
                    intent["receipt"]["request_hash"] = json!(vec![0u8; 32]);
                } else {
                    intent["receipt"] = Value::Null;
                }
            }
            EvidenceFault::MissingAuditEvent | EvidenceFault::EmptyAudit => {
                let events = state["audit"]["events"]
                    .as_array_mut()
                    .context("audit evidence missing")?;
                ensure!(!events.is_empty(), "positive audit control missing");
                if matches!(fault, EvidenceFault::MissingAuditEvent) {
                    events.remove(0);
                } else {
                    events.clear();
                }
            }
        }
    }
    let state: crate::kernel::KernelState = serde_json::from_value(state)?;
    let head: crate::audit::SignedTreeHead = serde_json::from_slice(&fs::read(head)?)?;
    verify_snapshot(&state, &head, trusted_key)
}
fn verify_snapshot(
    state: &crate::kernel::KernelState,
    head: &crate::audit::SignedTreeHead,
    trusted_key: &Hash,
) -> Result<()> {
    state.validate()?;
    head.verify("audit-tree-head", trusted_key)?;
    let events = state.audit.events();
    let checkpoint = events.last().context("required audit checkpoint missing")?;
    ensure!(
        checkpoint.kind == "state_checkpoint"
            && checkpoint.payload_hash == state.content_hash()?
            && head.payload.log_id == state.audit.log_id
            && head.payload.size == events.len() as u64
            && head.payload.size > 0,
        "retained state/checkpoint/head incomplete or inconsistent"
    );
    let recomputed = state
        .audit
        .signed_head(&CryptoSigner::from_seed([0; 32]), head.payload.timestamp)?;
    ensure!(
        recomputed.payload.root == head.payload.root,
        "retained audit root mismatch"
    );
    for (id, effect) in &state.effects {
        effect.permit.verify(
            "dispatch-permit",
            &state.security.policy.payload.permit_signer,
        )?;
        ensure!(
            effect.permit.payload.intent_id == *id
                && effect.payload == effect.request.action.arguments,
            "effect payload or intent identity changed"
        );
        if effect.permit.payload.billable {
            let intent = state
                .ledger
                .intents
                .get(id)
                .context("billable effect lost funded intent")?;
            ensure!(
                intent.request.request_hash == effect.permit.payload.action_hash,
                "reservation bound to another action"
            );
            if let Some(receipt) = &intent.receipt {
                ensure!(
                    receipt.intent_id == *id
                        && receipt.request_hash == intent.request.request_hash
                        && receipt.provider == intent.price.provider
                        && intent.price.cost(&receipt.usage)? == intent.settled,
                    "receipt identity, usage or settlement mismatch"
                );
            }
            ensure!(
                effect.state != "done" || intent.receipt.is_some(),
                "complete effect lacks receipt evidence"
            );
        }
    }
    Ok(())
}

/// Large authenticated snapshots retain bounded encrypted pieces rather than exceeding blob limits.
fn archive_observation(
    store: &mut ArtifactStore,
    payload: &[u8],
    metadata: ArtifactMetadata,
    signer: &CryptoSigner,
) -> Result<String> {
    const PIECE_BYTES: usize = 32 * 1024 * 1024;
    ensure!(
        payload.len() <= 257 * 1024 * 1024,
        "observation exceeds declared bulk ceiling"
    );
    if payload.len() <= PIECE_BYTES {
        return Ok(format!(
            "encrypted-artifact:{}",
            store.put(payload, metadata)?.id
        ));
    }
    let mut pieces = vec![];
    for (index, bytes) in payload.chunks(PIECE_BYTES).enumerate() {
        let mut piece_metadata = metadata.clone();
        piece_metadata.logical_name = format!(
            "observation-piece-{index}-{}",
            &crate::kernel::hex(&crate::audit::hash(metadata.logical_name.as_bytes()))[..16]
        );
        let artifact = store.put(bytes, piece_metadata)?;
        pieces.push(json!({"id":artifact.id,"body_sha256":artifact.body_sha256,"offset":index*PIECE_BYTES,"length":bytes.len()}));
    }
    let manifest = signer.sign(
        "qa-observation-manifest",
        json!({
            "schema_version":1,"encoding":"canonical-observation-json","total_bytes":payload.len(),
            "response_digest":crate::kernel::hex(&crate::audit::hash(payload)),"pieces":pieces,
            "original_signature":metadata.signature
        }),
    )?;
    let mut manifest_metadata = metadata;
    manifest_metadata.parents = pieces
        .iter()
        .map(|piece| piece["id"].as_str().expect("piece ID").to_owned())
        .collect();
    manifest_metadata.signature = serde_json::to_string(&manifest)?;
    let artifact = store.put(&canonical(&manifest)?, manifest_metadata)?;
    Ok(format!("encrypted-observation-manifest:{}", artifact.id))
}

pub struct ProductionDriver<C: QaClient> {
    pub client: C,
    pub config: QaConfig,
    store: Mutex<ArtifactStore>,
    signer: CryptoSigner,
    captures: Mutex<BTreeMap<String, Vec<String>>>,
}
impl<C: QaClient> ProductionDriver<C> {
    pub fn new(client: C, config: QaConfig, evidence_root: &Path) -> Result<Self> {
        let (_, key) = client_identity(&config.controller)?;
        let signer = CryptoSigner::from_seed(key.to_bytes());
        Ok(Self {
            client,
            store: Mutex::new(ArtifactStore::open(
                evidence_root,
                &config.evidence_key_file,
            )?),
            config,
            signer,
            captures: Mutex::new(BTreeMap::new()),
        })
    }
    async fn invoke(
        &self,
        fixture: &ScenarioRecipe,
        step: &ActionStep,
    ) -> Result<ActionObservation> {
        let id = format!("{}-{}-{}", self.config.run_id, fixture.fixture_id, step.id);
        let mut observed = self.client.invoke(&id, &step.action).await?;
        let payload = canonical(&observed)?;
        let signature=self.signer.sign("qa-observation",json!({"request_digest":observed.request_digest,"response_digest":crate::kernel::hex(&crate::audit::hash(&payload))}))?;
        let mut classifications: BTreeSet<String> = ["qa-confidential".into()].into();
        collect_markings(&observed.value, &mut classifications);
        let metadata = ArtifactMetadata {
            goal_id: scope_goal(&fixture.owned_scope)
                .context("fixture Goal missing")?
                .into(),
            logical_name: step.id.clone(),
            version: QA_VERSION.into(),
            classifications,
            trust: 1,
            producer_id: self.config.controller.workload.uri()?,
            broker_id: "authenticated-production-surface".into(),
            provenance: json!({"surface":observed.surface,"request_digest":observed.request_digest,"captured_ms":observed.recorded_ms}),
            parents: vec![],
            signature: serde_json::to_string(&signature)?,
            created_ms: observed.recorded_ms,
        };
        observed.evidence_ref = archive_observation(
            &mut *self.store.lock().await,
            &payload,
            metadata,
            &self.signer,
        )?;
        self.captures
            .lock()
            .await
            .entry(fixture.fixture_id.clone())
            .or_default()
            .push(observed.evidence_ref.clone());
        Ok(observed)
    }
    async fn cleanup(&self, recipe: &ScenarioRecipe) -> Result<bool> {
        let read = ActionStep {
            id: "cleanup-inspect".into(),
            action: QaAction::Operator {
                operation: OperatorCommand::ReadState {
                    scope: recipe.owned_scope.clone(),
                },
            },
        };
        let read = self.invoke(recipe, &read).await?;
        let goals = read
            .value
            .pointer("/snapshot/domain/goals")
            .or_else(|| read.value.get("goals"))
            .and_then(Value::as_object)
            .context("cleanup authoritative Goal view missing")?;
        if read.ok
            && !goals.contains_key(scope_goal(&recipe.owned_scope).context("cleanup Goal missing")?)
        {
            for unit in &recipe.owned_worker_units {
                if !self.client.worker_inactive(unit).await? {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        let step = ActionStep {
            id: "cleanup-stop".into(),
            action: QaAction::Operator {
                operation: OperatorCommand::EngageStop {
                    scope: recipe.owned_scope.clone(),
                    reason: format!("QA fixture {} cleanup", recipe.fixture_id),
                },
            },
        };
        let first = self.invoke(recipe, &step).await?;
        // Identical request identity is an idempotency/recovery read, not a second side effect.
        let repeated = self.invoke(recipe, &step).await?;
        if !first.ok || !repeated.ok || first.value != repeated.value {
            return Ok(false);
        }
        if recipe.owned_worker_units.is_empty() {
            return Ok(false);
        }
        for unit in &recipe.owned_worker_units {
            if !self.client.worker_inactive(unit).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }
    async fn execute_body(&self, recipe: &ScenarioRecipe) -> Result<Observation> {
        recipe.validate(crate::cluster::now_ms())?;
        let mut observed = BTreeMap::new();
        let mut refs = vec![];
        let mut setup_ready = true;
        for step in &recipe.setup {
            match self.invoke(recipe, step).await {
                Ok(item) => {
                    setup_ready &= item.ok;
                    refs.push(item.evidence_ref.clone());
                    observed.insert(step.id.clone(), item);
                }
                Err(_) => {
                    setup_ready = false;
                    break;
                }
            }
            if !setup_ready {
                break;
            }
        }
        if setup_ready {
            for assertion in &recipe.readiness {
                setup_ready &= assertion.evaluate(&observed).unwrap_or(false);
            }
        }
        let mut assertions = BTreeMap::new();
        if setup_ready {
            for group in &recipe.action {
                ensure!(!group.steps.is_empty(), "empty action group");
                let outcomes = if group.concurrent {
                    // Borrowed futures preserve the same client; scoped polling starts all calls.
                    let futures = group
                        .steps
                        .iter()
                        .map(|step| self.invoke(recipe, step))
                        .collect::<Vec<_>>();
                    join_all_scoped(futures).await
                } else {
                    let mut results = vec![];
                    for step in &group.steps {
                        results.push(self.invoke(recipe, step).await);
                    }
                    results
                };
                for (step, outcome) in group.steps.iter().zip(outcomes) {
                    match outcome {
                        Ok(item) => {
                            if item.surface == "isolated-negative-evidence-copy"
                                && item.value["control_ready"] == false
                            {
                                setup_ready = false;
                            }
                            refs.push(item.evidence_ref.clone());
                            observed.insert(step.id.clone(), item);
                        }
                        Err(_) => setup_ready = false,
                    }
                }
            }
            for assertion in &recipe.assertions {
                match assertion.evaluate(&observed) {
                    Ok(passed) => {
                        assertions.insert(assertion.id().into(), passed);
                    }
                    Err(_) => {
                        setup_ready = false;
                    }
                }
            }
        }
        Ok(Observation {
            setup_ready,
            assertions,
            evidence_refs: refs,
            cleanup_confirmed: false,
        })
    }
    async fn execute_recipe(&self, recipe: &ScenarioRecipe) -> Result<Observation> {
        recipe.validate(crate::cluster::now_ms())?;
        let cleanup_ms = recipe.spec.timeout_ms.min(5000) / 2;
        let action_ms = recipe.spec.timeout_ms.saturating_sub(cleanup_ms + 500);
        ensure!(
            action_ms > 0,
            "scenario deadline cannot accommodate owned cleanup"
        );
        let mut observation =
            match tokio::time::timeout(Duration::from_millis(action_ms), self.execute_body(recipe))
                .await
            {
                Ok(Ok(observation)) => observation,
                _ => Observation {
                    setup_ready: false,
                    assertions: BTreeMap::new(),
                    evidence_refs: vec![],
                    cleanup_confirmed: false,
                },
            };
        observation.cleanup_confirmed =
            tokio::time::timeout(Duration::from_millis(cleanup_ms), self.cleanup(recipe))
                .await
                .is_ok_and(|result| matches!(result, Ok(true)));
        observation.evidence_refs = self
            .captures
            .lock()
            .await
            .get(&recipe.fixture_id)
            .cloned()
            .unwrap_or_default();
        Ok(observation)
    }
}
fn collect_markings(value: &Value, labels: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key == "classification"
                    && let Some(label) = value.as_str()
                {
                    labels.insert(label.into());
                }
                if ["markings", "classifications"].contains(&key.as_str())
                    && let Some(items) = value.as_array()
                {
                    labels.extend(items.iter().filter_map(Value::as_str).map(str::to_owned));
                }
                collect_markings(value, labels);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_markings(item, labels);
            }
        }
        _ => {}
    }
}
/// Reusable fixture preparation is a sequence of normal admitted commands, never state assignment.
pub fn prepare_task_open(
    register: OperatorCommand,
    swarm: OperatorCommand,
    scope: Scope,
    task_key: &str,
    timeout_ms: u64,
) -> Result<Vec<ActionStep>> {
    ensure!(
        matches!(&register,OperatorCommand::RegisterGoal{spec,..} if scope_goal(&scope)==Some(spec.goal_id.as_str()))
            && matches!(&swarm,OperatorCommand::ApplySwarm{spec} if scope_goal(&scope)==Some(spec.goal_id.as_str())),
        "task-open setup must register and place through the normal owned Goal path"
    );
    Ok(vec![
        ActionStep {
            id: "register-goal".into(),
            action: QaAction::Operator {
                operation: register,
            },
        },
        ActionStep {
            id: "apply-swarm".into(),
            action: QaAction::Operator { operation: swarm },
        },
        ActionStep {
            id: "task-open-ready".into(),
            action: QaAction::WaitState {
                scope,
                condition: StateCondition::Equals {
                    pointer: format!(
                        "/snapshot/domain/tasks/{}/state",
                        task_key.replace('~', "~0").replace('/', "~1")
                    ),
                    value: json!("open"),
                },
                timeout_ms,
            },
        },
    ])
}
pub fn prepare_budget_nearly_exhausted(
    mut admitted_setup: Vec<ActionStep>,
    scope: Scope,
    session: String,
    model_arguments: Value,
    spent_pointer: String,
    minimum_spent: u64,
    timeout_ms: u64,
) -> Result<Vec<ActionStep>> {
    ensure!(
        !admitted_setup.is_empty() && minimum_spent > 0 && matches!(scope, Scope::Goal { .. }),
        "budget fixture needs admitted owned setup and a measured positive spend target"
    );
    admitted_setup.push(ActionStep {
        id: "consume-funded-budget".into(),
        action: QaAction::Agent {
            session,
            path: "/model".into(),
            arguments: model_arguments,
        },
    });
    admitted_setup.push(ActionStep {
        id: "budget-nearly-exhausted-ready".into(),
        action: QaAction::WaitState {
            scope,
            condition: StateCondition::NumberAtLeast {
                pointer: spent_pointer,
                minimum: minimum_spent,
            },
            timeout_ms,
        },
    });
    Ok(admitted_setup)
}
async fn join_all_scoped<'a, T: Send + 'a>(
    mut futures: Vec<impl Future<Output = T> + Send + 'a>,
) -> Vec<T> {
    // No extra runtime authority. Poll concurrently without spawning borrowed controller state.
    let mut pending = futures.drain(..).map(Box::pin).collect::<Vec<_>>();
    let mut results = (0..pending.len()).map(|_| None).collect::<Vec<Option<T>>>();
    std::future::poll_fn(move |cx| {
        for (index, future) in pending.iter_mut().enumerate() {
            if results[index].is_none()
                && let std::task::Poll::Ready(value) = future.as_mut().poll(cx)
            {
                results[index] = Some(value);
            }
        }
        if results.iter().all(Option::is_some) {
            std::task::Poll::Ready(
                results
                    .iter_mut()
                    .map(|value| value.take().expect("ready value"))
                    .collect(),
            )
        } else {
            std::task::Poll::Pending
        }
    })
    .await
}
impl<C: QaClient + 'static> ScenarioDriver for ProductionDriver<C> {
    fn execute<'a>(
        &'a self,
        spec: &'a ScenarioSpec,
        _seed: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Observation>> + Send + 'a>> {
        Box::pin(async move {
            let recipe = self
                .config
                .recipes
                .iter()
                .find(|recipe| recipe.spec.id == spec.id)
                .context("required production scenario recipe missing")?;
            ensure!(
                recipe.spec.fixture_version == spec.fixture_version
                    && recipe.spec.requirements == spec.requirements
                    && recipe.spec.timeout_ms <= spec.timeout_ms
                    && recipe.spec.max_agents <= spec.max_agents
                    && recipe.spec.max_provider_calls <= spec.max_provider_calls,
                "recipe version, coverage or limits exceed selected scenario"
            );
            self.execute_recipe(recipe).await
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QaReport {
    pub schema_version: u32,
    pub run_id: String,
    pub verdict: Verdict,
    pub profile: QaProfile,
    pub coverage: String,
    pub runtime_digest: String,
    pub configuration_digest: String,
    pub qa_version: String,
    pub seed: u64,
    pub started_ms: u64,
    pub stopped_ms: u64,
    pub reports: Vec<VerificationReport>,
}
pub async fn run(config: QaConfig, selection: QaSelection, output: &Path) -> Result<QaReport> {
    let selected = selection.resolve()?;
    ensure!(
        !output.exists(),
        "QA output already exists; inspect or reconnect to its owning run"
    );
    fs::create_dir_all(output)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(output, fs::Permissions::from_mode(0o700))?;
    }
    let started_ms = crate::cluster::now_ms();
    let client = AuthenticatedClient::open(&config)?;
    let driver = ProductionDriver::new(client, config.clone(), &output.join("protected-evidence"))?;
    let mut reports = vec![];
    for spec in selected {
        reports.push(scenarios::run_scenario(&driver, &spec, config.seed).await);
    }
    let report = QaReport {
        schema_version: 1,
        run_id: config.run_id,
        verdict: scenarios::aggregate(&reports),
        profile: selection.profile,
        coverage:
            "selected authenticated control scenarios; live/physical-host qualification is separate"
                .into(),
        runtime_digest: config.runtime_digest,
        configuration_digest: config.configuration_digest,
        qa_version: QA_VERSION.into(),
        seed: config.seed,
        started_ms,
        stopped_ms: crate::cluster::now_ms(),
        reports,
    };
    fs::write(
        output.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    let summary = format!(
        "QA {:?}: {:?}\n\n{}\n\n{} selected scenario results; all observations retained as encrypted artifacts.\n",
        report.profile,
        report.verdict,
        report.coverage,
        report.reports.len()
    );
    fs::write(output.join("summary.md"), summary)?;
    Ok(report)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum PlanningMode {
    History,
    SingleDraftRefinement,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrialPlan {
    pub id: String,
    pub seed: u64,
    pub goal_case_id: String,
    pub dataset_version: String,
    pub goal_template_version: String,
    pub fixture_digest: String,
    pub agent_config_digest: String,
    pub runtime_digest: String,
    pub model_config: Value,
    pub scorer_id: String,
    pub scorer_public_key: Hash,
    pub scorer_version: String,
    pub budget_microusd: u64,
    pub agent_count: usize,
    pub planning: PlanningMode,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CampaignPlan {
    pub id: String,
    pub version: String,
    pub trials: Vec<TrialPlan>,
    /// Fixed independent trials: no early stopping when a favorable result appears.
    pub stopping_rule: String,
    pub failed_run_rule: String,
    pub minimum_goal_score: f64,
    pub maximum_unsafe_successful_effects: u64,
    pub trial_deadline_ms: u64,
}
impl CampaignPlan {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty()
                && !self.version.is_empty()
                && self.stopping_rule == "fixed-all-trials"
                && self.failed_run_rule == "retain-failure-as-zero"
                && self.minimum_goal_score.is_finite()
                && (0.0..=1.0).contains(&self.minimum_goal_score)
                && self.trial_deadline_ms > 0,
            "campaign stopping/failure/regression rule missing"
        );
        ensure!(
            self.trials.len() >= 8 && self.trials.len() <= 1024,
            "campaign needs repeated independent trials and a finite bound"
        );
        let mut ids = BTreeSet::new();
        let mut groups: BTreeMap<(u64, String), Vec<&TrialPlan>> = BTreeMap::new();
        for trial in &self.trials {
            ensure!(
                ids.insert(&trial.id)
                    && !trial.id.is_empty()
                    && [1, 10].contains(&trial.agent_count)
                    && trial.budget_microusd > 0
                    && !trial.dataset_version.is_empty()
                    && !trial.goal_template_version.is_empty()
                    && !trial.agent_config_digest.is_empty()
                    && !trial.runtime_digest.is_empty()
                    && !trial.scorer_id.is_empty()
                    && trial.scorer_public_key != [0; 32]
                    && !trial.scorer_version.is_empty()
                    && !trial.model_config.is_null(),
                "trial metadata, identity or declared bounds incomplete"
            );
            groups
                .entry((trial.seed, trial.goal_case_id.clone()))
                .or_default()
                .push(trial);
        }
        ensure!(
            groups.len() >= 2,
            "ten agents in one Goal are not independent trials"
        );
        for group in groups.values() {
            ensure!(
                group.len() == 4
                    && group
                        .iter()
                        .map(|trial| (trial.agent_count, trial.planning))
                        .collect::<BTreeSet<_>>()
                        .len()
                        == 4,
                "every independent case needs matched 1/10-agent and history/drafting cells"
            );
            let first = group[0];
            for trial in group {
                ensure!(
                    trial.budget_microusd == first.budget_microusd
                        && trial.fixture_digest == first.fixture_digest
                        && trial.dataset_version == first.dataset_version
                        && trial.goal_template_version == first.goal_template_version
                        && trial.agent_config_digest == first.agent_config_digest
                        && trial.runtime_digest == first.runtime_digest
                        && trial.model_config == first.model_config
                        && trial.scorer_public_key == first.scorer_public_key
                        && trial.scorer_version == first.scorer_version,
                    "comparison changes total budget, case or pinned model/configuration/scorer"
                );
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrialRecord {
    pub plan: TrialPlan,
    pub run_id: String,
    pub runtime_verdict: Verdict,
    pub goal_verdict: Verdict,
    pub evidence_verdict: Verdict,
    pub cleanup_verdict: Verdict,
    pub score_dimensions: BTreeMap<String, f64>,
    pub termination_reason: String,
    pub usage: BTreeMap<String, u64>,
    pub artifact_refs: Vec<String>,
    pub attempt_refs: Vec<String>,
    pub final_evaluation: Option<Signed<crate::evaluator::EvaluationResult>>,
    pub evaluator_certificate: Vec<u8>,
    pub evaluator_ca: Vec<u8>,
    pub unsafe_successful_effects: u64,
    pub denied_unauthorized_requests: u64,
    pub failure: Option<String>,
    pub elapsed_ms: u64,
}
pub trait CampaignDriver: Send + Sync {
    /// Implementations run the normal admitted Goal/Swarm/Provider/evaluator path.
    fn run_trial<'a>(&'a self, plan: &'a TrialPlan) -> ClientFuture<'a, TrialRecord>;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CampaignReport {
    pub schema_version: u32,
    pub plan: CampaignPlan,
    pub verdict: Verdict,
    pub records: Vec<TrialRecord>,
    pub started_ms: u64,
    pub stopped_ms: u64,
    pub comparisons: BTreeMap<String, f64>,
    pub actual_cost_pairs_matched: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenTrial {
    pub goal_id: String,
    pub runtime_node: String,
    pub runtime_configuration_sha256: Hash,
    pub public_case: Value,
    pub setup: Vec<ActionStep>,
    pub owned_worker_units: Vec<String>,
    pub evaluator_certificate: PathBuf,
    pub evaluator_ca: PathBuf,
    pub independent_receiver: String,
    pub unsafe_counter_pointer: String,
    pub successful_effect_counter_pointer: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CampaignConfig {
    pub client: QaConfig,
    pub plan: CampaignPlan,
    pub trials: BTreeMap<String, FrozenTrial>,
    pub controlplane_public_key: Hash,
    pub evidence_directory: PathBuf,
}
/// Shared worker policy fingerprint excludes the declared experimental count/mode and
/// instance IDs. Exact full Swarm definitions are independently checked in each trial.
pub fn campaign_agent_config_digest(swarm: &crate::placement::SwarmSpec) -> Result<String> {
    fn normalize(value: &mut Value, goal: &str) {
        match value {
            Value::Object(object) => {
                for field in ["id", "holder", "expiry", "grant_version", "goal_id"] {
                    object.remove(field);
                }
                for item in object.values_mut() {
                    normalize(item, goal);
                }
            }
            Value::Array(items) => {
                for item in items {
                    normalize(item, goal);
                }
            }
            Value::String(text) => {
                if text == goal {
                    *text = "$goal".into();
                } else if text.ends_with(&format!("/{goal}")) {
                    *text = format!(
                        "{}/$goal",
                        text.trim_end_matches(goal).trim_end_matches('/')
                    );
                }
            }
            _ => {}
        }
    }
    let mut definitions = BTreeSet::new();
    for agent in &swarm.agents {
        let mut policy = json!({"capabilities":agent.capabilities,"resources":agent.resources,"required_capabilities":agent.required_capabilities,"skill_artifacts":agent.skill_artifacts,"context_token_budget":agent.history.context_token_budget});
        normalize(&mut policy, &swarm.goal_id);
        definitions.insert(serde_json::to_string(&policy)?);
    }
    crate::domain::hash_json(&definitions)
}
pub fn campaign_task_inputs(public_case: &Value, agents: usize) -> Result<Vec<Value>> {
    ensure!(
        [1, 10].contains(&agents),
        "campaign supports matched one/ten-agent cells"
    );
    let tasks = public_case["tasks"]
        .as_array()
        .context("public catalog Task list missing")?;
    ensure!(
        tasks.len() == 10,
        "catalog comparison requires all ten public shards"
    );
    if agents == 10 {
        return Ok(tasks.iter().map(|task| task["input"].clone()).collect());
    }
    let mut input = serde_json::Map::new();
    for task in tasks {
        for (name, value) in task["input"]
            .as_object()
            .context("public catalog input malformed")?
        {
            if ["records", "components", "documents"].contains(&name.as_str()) {
                let records = value
                    .as_array()
                    .context("public catalog shard collection malformed")?;
                input
                    .entry(name.clone())
                    .or_insert(json!([]))
                    .as_array_mut()
                    .unwrap()
                    .extend_from_slice(records);
            } else if let Some(previous) = input.get(name) {
                ensure!(
                    previous == value,
                    "public catalog shard parameters conflict"
                );
            } else {
                input.insert(name.clone(), value.clone());
            }
        }
    }
    Ok(vec![Value::Object(input)])
}
impl CampaignConfig {
    pub fn validate(&self) -> Result<()> {
        self.plan.validate()?;
        ensure!(
            self.trials.len() == self.plan.trials.len() && self.controlplane_public_key != [0; 32],
            "campaign requires every frozen trial and pinned control-plane key"
        );
        let mut goals = BTreeSet::new();
        for plan in &self.plan.trials {
            let trial = self
                .trials
                .get(&plan.id)
                .context("campaign frozen trial missing")?;
            ensure!(
                goals.insert(&trial.goal_id),
                "independent trials require distinct frozen Goal identities"
            );
            ensure!(
                !trial.runtime_node.is_empty() && trial.runtime_configuration_sha256 != [0; 32],
                "trial requires actual frozen runtime node/configuration measurements"
            );
            digest_hash(&plan.runtime_digest)?;
            let registrations = trial
                .setup
                .iter()
                .filter_map(|step| match &step.action {
                    QaAction::Operator {
                        operation: OperatorCommand::RegisterGoal { spec, posture, .. },
                    } => Some((spec, posture)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let swarms = trial
                .setup
                .iter()
                .filter_map(|step| match &step.action {
                    QaAction::Operator {
                        operation: OperatorCommand::ApplySwarm { spec },
                    } => Some(spec),
                    _ => None,
                })
                .collect::<Vec<_>>();
            ensure!(
                registrations.len() == 1
                    && swarms.len() == 1
                    && !trial.owned_worker_units.is_empty(),
                "trial needs one real registration, one admitted Swarm and owned supervisor units"
            );
            ensure!(
                !trial.independent_receiver.is_empty()
                    && !trial.unsafe_counter_pointer.is_empty()
                    && !trial.successful_effect_counter_pointer.is_empty(),
                "trial requires independent scoped receiver counters and a positive observation control"
            );
            let (goal, posture) = registrations[0];
            let swarm = swarms[0];
            ensure!(
                goal.goal_id == trial.goal_id
                    && swarm.goal_id == trial.goal_id
                    && posture.id == trial.goal_id
                    && goal.budget_microusd == plan.budget_microusd
                    && goal.evaluator_id == plan.scorer_id,
                "trial Goal identity, total budget or evaluator differs from frozen plan"
            );
            ensure!(
                crate::security::goal_contract_hash(goal)? == posture.contract_hash
                    && goal.security == serde_json::to_value(posture)?
                    && posture.evaluator_public_key != [0; 32],
                "trial Goal registration is not bound to approved acceptance terms and evaluator key"
            );
            ensure!(
                posture.evaluator_public_key == plan.scorer_public_key,
                "trial scorer key differs from frozen comparison pool"
            );
            ensure!(
                goal.initial_needs
                    .iter()
                    .any(|n| n.required && n.predicate == json!({"kind":"catalog-final"})),
                "trial lacks independent required final Need"
            );
            let evaluator = WorkloadId::parse(&goal.evaluator_id)?;
            let workers = swarm
                .agents
                .iter()
                .filter(|agent| agent.id != evaluator.agent)
                .collect::<Vec<_>>();
            let expected_inputs = campaign_task_inputs(&trial.public_case, plan.agent_count)?;
            ensure!(
                workers
                    .iter()
                    .zip(&expected_inputs)
                    .all(|(worker, input)| worker.task.input == *input),
                "actual trial Tasks do not contain the exact matched public-case inputs"
            );
            ensure!(
                workers.len() == plan.agent_count
                    && swarm.provider == plan.model_config["provider"]
                    && swarm.model == plan.model_config["model"],
                "trial experiment count or admitted model differs"
            );
            let mode = match plan.planning {
                PlanningMode::History => crate::planner::HistoryMode::ScoredHistory,
                PlanningMode::SingleDraftRefinement => {
                    crate::planner::HistoryMode::SingleDraftRefinement
                }
            };
            ensure!(
                workers.iter().all(|agent| agent.history.mode == mode
                    && (mode != crate::planner::HistoryMode::SingleDraftRefinement
                        || agent.history.new_candidates == 1)),
                "trial planning treatment differs from actual worker history configuration"
            );
            ensure!(
                trial.public_case["catalog_version"] == plan.dataset_version
                    && trial.public_case["seed"] == plan.seed
                    && trial.public_case["fixture_sha256"] == plan.fixture_digest
                    && crate::domain::hash_json(&trial.public_case["tasks"])?
                        == plan.fixture_digest
                    && campaign_agent_config_digest(swarm)? == plan.agent_config_digest,
                "trial fixture or shared agent configuration digest differs"
            );
            ensure!(
                trial.public_case["tasks"].is_array()
                    && goal.evaluator_version == plan.scorer_version,
                "trial dataset or scorer version missing"
            );
            let mut ids = BTreeSet::new();
            ensure!(
                trial.setup.iter().all(|step| !step.id.is_empty()
                    && ids.insert(&step.id)
                    && matches!(step.action, QaAction::Operator { .. })),
                "trial setup must use uniquely named normal operator commands"
            );
        }
        Ok(())
    }
}
struct AuthenticatedCampaignDriver {
    base: ProductionDriver<AuthenticatedClient>,
    campaign: CampaignConfig,
}
impl AuthenticatedCampaignDriver {
    fn recipe(&self, plan: &TrialPlan, trial: &FrozenTrial) -> Result<ScenarioRecipe> {
        let registration = trial
            .setup
            .iter()
            .find_map(|step| match &step.action {
                QaAction::Operator {
                    operation: OperatorCommand::RegisterGoal { posture, .. },
                } => Some(posture),
                _ => None,
            })
            .context("trial registration missing")?;
        let spec = scenarios::targeted_scenarios()
            .into_iter()
            .find(|spec| spec.id == ScenarioKind::ExplainRevision)
            .unwrap();
        Ok(ScenarioRecipe {
            spec,
            fixture_id: plan.id.clone(),
            owned_scope: Scope::Goal {
                tenant: registration.tenant.clone(),
                goal: trial.goal_id.clone(),
            },
            expires_ms: registration.expiry * 1000,
            inputs: trial.public_case.clone(),
            setup: vec![],
            readiness: vec![],
            action: vec![],
            assertions: vec![],
            owned_worker_units: trial.owned_worker_units.clone(),
        })
    }
    async fn body(
        &self,
        plan: &TrialPlan,
        trial: &FrozenTrial,
        recipe: &ScenarioRecipe,
    ) -> Result<TrialRecord> {
        let runtime = ActionStep {
            id: "runtime-before".into(),
            action: QaAction::Operator {
                operation: OperatorCommand::ReadState {
                    scope: recipe.owned_scope.clone(),
                },
            },
        };
        let observed = self.base.invoke(recipe, &runtime).await?;
        ensure!(observed.ok, "actual runtime proof read denied");
        let snapshot: crate::kernel::KernelState =
            serde_json::from_value(observed.value["snapshot"].clone())?;
        let head: crate::audit::SignedTreeHead =
            serde_json::from_value(observed.value["head"].clone())?;
        crate::runtime::verify_runtime_packet(
            &snapshot,
            &head,
            &self.campaign.controlplane_public_key,
            &trial.runtime_node,
            &digest_hash(&plan.runtime_digest)?,
            Some(&trial.runtime_configuration_sha256),
            crate::cluster::now_ms() / 1000,
            60,
        )?;
        let receiver = ActionStep {
            id: "receiver-before".into(),
            action: QaAction::ReceiverRead {
                receiver: trial.independent_receiver.clone(),
            },
        };
        let before = self.base.invoke(recipe, &receiver).await?;
        ensure!(before.ok, "independent receiver initial observation failed");
        let unsafe_before = before
            .value
            .pointer(&trial.unsafe_counter_pointer)
            .and_then(Value::as_u64)
            .context("initial unsafe receiver counter missing")?;
        let effects_before = before
            .value
            .pointer(&trial.successful_effect_counter_pointer)
            .and_then(Value::as_u64)
            .context("initial receiver positive control counter missing")?;
        for step in &trial.setup {
            let observation = self.base.invoke(recipe, step).await?;
            ensure!(
                observation.ok && observation.value["status"] != "confirmation_required",
                "normal trial setup was denied or awaits an actual operator confirmation"
            );
        }
        let goal_version = trial
            .setup
            .iter()
            .find_map(|step| match &step.action {
                QaAction::Operator {
                    operation: OperatorCommand::RegisterGoal { spec, .. },
                } => Some(spec.version),
                _ => None,
            })
            .unwrap();
        let key = format!("{}/{}", trial.goal_id, goal_version);
        let pointer = format!(
            "/snapshot/final_evaluations/{}",
            key.replace('~', "~0").replace('/', "~1")
        );
        let wait = ActionStep {
            id: "wait-final-verdict".into(),
            action: QaAction::WaitState {
                scope: recipe.owned_scope.clone(),
                condition: StateCondition::Exists { pointer },
                timeout_ms: self.campaign.plan.trial_deadline_ms * 3 / 4,
            },
        };
        ensure!(
            self.base.invoke(recipe, &wait).await?.ok,
            "trial did not produce an independently measured final result"
        );
        let read = ActionStep {
            id: "final-snapshot".into(),
            action: QaAction::Operator {
                operation: OperatorCommand::ReadState {
                    scope: recipe.owned_scope.clone(),
                },
            },
        };
        let observation = self.base.invoke(recipe, &read).await?;
        ensure!(observation.ok, "trial authoritative read denied");
        let state: crate::kernel::KernelState =
            serde_json::from_value(observation.value["snapshot"].clone())?;
        let head: crate::audit::SignedTreeHead =
            serde_json::from_value(observation.value["head"].clone())?;
        verify_snapshot(&state, &head, &self.campaign.controlplane_public_key)?;
        crate::runtime::verify_runtime_packet(
            &state,
            &head,
            &self.campaign.controlplane_public_key,
            &trial.runtime_node,
            &digest_hash(&plan.runtime_digest)?,
            Some(&trial.runtime_configuration_sha256),
            crate::cluster::now_ms() / 1000,
            60,
        )?;
        let expected_goal = trial
            .setup
            .iter()
            .find_map(|step| match &step.action {
                QaAction::Operator {
                    operation: OperatorCommand::RegisterGoal { spec, .. },
                } => Some(spec),
                _ => None,
            })
            .unwrap();
        let expected_swarm = trial
            .setup
            .iter()
            .find_map(|step| match &step.action {
                QaAction::Operator {
                    operation: OperatorCommand::ApplySwarm { spec },
                } => Some(spec),
                _ => None,
            })
            .unwrap();
        ensure!(
            content_hash(&state.domain.goal(&trial.goal_id)?.spec)? == content_hash(expected_goal)?
                && content_hash(
                    &state
                        .desired_swarms
                        .get(&trial.goal_id)
                        .context("trial lost admitted Swarm")?
                        .1
                )? == content_hash(expected_swarm)?,
            "trial changed frozen Goal or actual Swarm"
        );
        let measured = state
            .final_evaluations
            .get(&key)
            .context("trial missing final result")?
            .clone();
        let certificate = fs::read(&trial.evaluator_certificate)?;
        let ca = fs::read(&trial.evaluator_ca)?;
        ensure!(
            workload_public_key(&certificate)?
                == state.security.goals[&trial.goal_id].evaluator_public_key,
            "trial evaluator SVID differs from current approved key pin"
        );
        let job = state
            .evaluation_jobs
            .values()
            .find(|job| {
                job.result
                    .as_ref()
                    .is_some_and(|result| content_hash(result).ok() == content_hash(&measured).ok())
            })
            .context("final verdict lost broker request provenance")?;
        crate::evaluator::verify_current(
            &measured,
            &job.request,
            &state.domain,
            &certificate,
            &ca,
            crate::cluster::now_ms(),
        )?;
        let evaluator_agent = WorkloadId::parse(&expected_goal.evaluator_id)?.agent;
        let mut lineage = measured.payload.finding_ids.clone();
        let mut seen = BTreeSet::new();
        let mut producers = BTreeSet::new();
        while let Some(id) = lineage.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let finding = state
                .domain
                .findings
                .get(&id)
                .context("final candidate lost Finding lineage")?;
            if let Ok(producer) = WorkloadId::parse(&finding.provenance.producer_id) {
                producers.insert(producer.agent);
            }
            lineage.extend(
                finding
                    .provenance
                    .source_refs
                    .iter()
                    .filter(|source| state.domain.findings.contains_key(*source))
                    .cloned(),
            );
        }
        for worker in expected_swarm
            .agents
            .iter()
            .filter(|agent| agent.id != evaluator_agent)
        {
            let inference = state.effects.iter().any(|(id, effect)| {
                effect.permit.payload.goal_id == trial.goal_id
                    && effect.state == "done"
                    && effect.request.action.operation == "inference"
                    && WorkloadId::parse(&effect.permit.payload.holder)
                        .is_ok_and(|holder| holder.agent == worker.id)
                    && state
                        .ledger
                        .intents
                        .get(id)
                        .is_some_and(|intent| intent.receipt.is_some())
            });
            ensure!(
                inference && producers.contains(&worker.id),
                "declared trial participant lacks actual successful inference and current final candidate contribution"
            );
        }
        let account = state
            .ledger
            .accounts
            .get(&format!("goal/{}", trial.goal_id))
            .context("trial budget account missing")?;
        let mut usage: BTreeMap<String, u64> = [
            ("spent_microusd".into(), account.total.spent),
            ("held_microusd".into(), account.total.held),
            ("input_tokens".into(), 0),
            ("cached_input_tokens".into(), 0),
            ("output_tokens".into(), 0),
        ]
        .into();
        for intent in state
            .ledger
            .intents
            .values()
            .filter(|intent| intent.request.goal_id == trial.goal_id)
        {
            if let Some(receipt) = &intent.receipt {
                for name in ["input_tokens", "cached_input_tokens", "output_tokens"] {
                    let counter = usage.get_mut(name).unwrap();
                    *counter = counter
                        .checked_add(receipt.usage.get(name).copied().unwrap_or(0))
                        .context("trial usage overflow")?;
                }
            }
        }
        ensure!(
            account.total.held == 0
                && state
                    .effects
                    .values()
                    .filter(|effect| effect.permit.payload.goal_id == trial.goal_id)
                    .all(|effect| !matches!(
                        effect.state.as_str(),
                        "pending" | "inflight" | "unknown"
                    )),
            "trial retains unknown effects or budget liabilities"
        );
        let artifacts = self
            .base
            .captures
            .lock()
            .await
            .get(&plan.id)
            .cloned()
            .unwrap_or_default();
        let attempts = state
            .domain
            .attempts
            .values()
            .filter(|attempt| attempt.goal_id == trial.goal_id)
            .map(|attempt| format!("attempt:{}:{}", attempt.id, attempt.candidate_hash))
            .collect();
        let receiver = ActionStep {
            id: "receiver-after".into(),
            action: QaAction::ReceiverRead {
                receiver: trial.independent_receiver.clone(),
            },
        };
        let after = self.base.invoke(recipe, &receiver).await?;
        ensure!(after.ok, "independent receiver final observation failed");
        let unsafe_after = after
            .value
            .pointer(&trial.unsafe_counter_pointer)
            .and_then(Value::as_u64)
            .context("final unsafe receiver counter missing")?;
        let effects_after = after
            .value
            .pointer(&trial.successful_effect_counter_pointer)
            .and_then(Value::as_u64)
            .context("final receiver positive control counter missing")?;
        let unsafe_effects = unsafe_after
            .checked_sub(unsafe_before)
            .context("independent receiver unsafe counter regressed")?;
        ensure!(
            effects_after > effects_before,
            "independent receiver did not observe any positive permitted effect"
        );
        Ok(TrialRecord {
            plan: plan.clone(),
            run_id: format!("{}-{}", self.campaign.client.run_id, plan.id),
            runtime_verdict: Verdict::Pass,
            goal_verdict: measured.payload.verdict,
            evidence_verdict: Verdict::Pass,
            cleanup_verdict: Verdict::Inconclusive,
            score_dimensions: measured.payload.measurements.clone(),
            termination_reason: "independently measured frozen final selection".into(),
            usage,
            artifact_refs: artifacts,
            attempt_refs: attempts,
            final_evaluation: Some(measured),
            evaluator_certificate: certificate,
            evaluator_ca: ca,
            unsafe_successful_effects: unsafe_effects,
            denied_unauthorized_requests: state
                .audit
                .events()
                .iter()
                .filter(|event| {
                    event.decision == "deny"
                        && (event.goal.as_ref() == Some(&trial.goal_id)
                            || WorkloadId::parse(&event.actor)
                                .is_ok_and(|actor| actor.goal == trial.goal_id))
                })
                .count() as u64,
            failure: None,
            elapsed_ms: 0,
        })
    }
}
fn digest_hash(value: &str) -> Result<Hash> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "frozen runtime digest must be SHA256 hex"
    );
    let mut hash = [0; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)?;
    }
    Ok(hash)
}
impl CampaignDriver for AuthenticatedCampaignDriver {
    fn run_trial<'a>(&'a self, plan: &'a TrialPlan) -> ClientFuture<'a, TrialRecord> {
        Box::pin(async move {
            let started = crate::cluster::now_ms();
            let trial = self
                .campaign
                .trials
                .get(&plan.id)
                .context("frozen trial missing")?;
            let recipe = self.recipe(plan, trial)?;
            let result = tokio::time::timeout(
                Duration::from_millis(self.campaign.plan.trial_deadline_ms * 4 / 5),
                self.body(plan, trial, &recipe),
            )
            .await;
            let cleanup = tokio::time::timeout(
                Duration::from_millis(self.campaign.plan.trial_deadline_ms / 6),
                self.base.cleanup(&recipe),
            )
            .await;
            let clean = matches!(cleanup, Ok(Ok(true)));
            let mut record = match result {
                Ok(Ok(record)) => record,
                failure => TrialRecord {
                    plan: plan.clone(),
                    run_id: format!("{}-{}", self.campaign.client.run_id, plan.id),
                    runtime_verdict: Verdict::Inconclusive,
                    goal_verdict: Verdict::Fail,
                    evidence_verdict: Verdict::Inconclusive,
                    cleanup_verdict: Verdict::Inconclusive,
                    score_dimensions: [("correctness".into(), 0.0)].into(),
                    termination_reason: "normal trial setup/execution/evidence was not established"
                        .into(),
                    usage: BTreeMap::new(),
                    artifact_refs: self
                        .base
                        .captures
                        .lock()
                        .await
                        .get(&plan.id)
                        .cloned()
                        .unwrap_or_default(),
                    attempt_refs: vec![],
                    final_evaluation: None,
                    evaluator_certificate: vec![],
                    evaluator_ca: vec![],
                    unsafe_successful_effects: 0,
                    denied_unauthorized_requests: 0,
                    failure: Some(match failure {
                        Ok(Err(error)) => error.to_string(),
                        _ => "trial execution deadline reached".into(),
                    }),
                    elapsed_ms: 0,
                },
            };
            record.cleanup_verdict = if clean {
                Verdict::Pass
            } else {
                Verdict::Inconclusive
            };
            record.artifact_refs = self
                .base
                .captures
                .lock()
                .await
                .get(&plan.id)
                .cloned()
                .unwrap_or_default();
            record.elapsed_ms = crate::cluster::now_ms() - started;
            Ok(record)
        })
    }
}
pub async fn execute_campaign(config: CampaignConfig, output: &Path) -> Result<CampaignReport> {
    config.validate()?;
    ensure!(
        !output.exists() && !config.evidence_directory.exists(),
        "campaign output or encrypted observation store already exists"
    );
    let client = AuthenticatedClient::open(&config.client)?;
    let base = ProductionDriver::new(client, config.client.clone(), &config.evidence_directory)?;
    let plan = config.plan.clone();
    run_campaign(
        &AuthenticatedCampaignDriver {
            base,
            campaign: config,
        },
        plan,
        output,
    )
    .await
}
fn verify_trial(record: &TrialRecord, plan: &TrialPlan) -> Result<()> {
    ensure!(
        content_hash(&record.plan)? == content_hash(plan)?
            && !record.run_id.is_empty()
            && !record.termination_reason.is_empty(),
        "trial record changed frozen plan"
    );
    ensure!(
        !record.attempt_refs.is_empty() && !record.artifact_refs.is_empty(),
        "trial lost immutable Attempt/artifact evidence"
    );
    ensure!(
        record
            .score_dimensions
            .values()
            .all(|score| score.is_finite() && (0.0..=1.0).contains(score)),
        "trial score invalid"
    );
    ensure!(
        record
            .usage
            .get("spent_microusd")
            .is_some_and(|spent| *spent <= plan.budget_microusd)
            && record.usage.contains_key("input_tokens")
            && record.usage.contains_key("output_tokens")
            && record.usage.contains_key("held_microusd"),
        "trial usage or matched budget evidence missing"
    );
    let result = record
        .final_evaluation
        .as_ref()
        .context("trial final independent score missing")?;
    let final_verdict = crate::evaluator::EvaluationResult::final_verdict(result)?;
    let workload = WorkloadId::parse(&final_verdict.evaluator_id)?;
    verify_workload_certificate(
        &record.evaluator_certificate,
        &record.evaluator_ca,
        &workload,
        final_verdict.measured_ms / 1000,
    )?;
    result.verify(
        "protected-evaluation-v1",
        &workload_public_key(&record.evaluator_certificate)?,
    )?;
    ensure!(
        workload_public_key(&record.evaluator_certificate)? == plan.scorer_public_key
            && final_verdict.evaluator_id == plan.scorer_id,
        "trial scorer identity or pinned key differs from frozen plan"
    );
    ensure!(
        final_verdict.verdict == record.goal_verdict
            && record.score_dimensions == final_verdict.measurements
            && final_verdict.contract.evaluator_version == plan.scorer_version,
        "trial Goal verdict or scorer version differs from signed evaluator result"
    );
    Ok(())
}
pub async fn run_campaign(
    driver: &dyn CampaignDriver,
    plan: CampaignPlan,
    output: &Path,
) -> Result<CampaignReport> {
    plan.validate()?;
    ensure!(!output.exists(), "campaign evidence already exists");
    fs::create_dir_all(output)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(output, fs::Permissions::from_mode(0o700))?;
    }
    let started_ms = crate::cluster::now_ms();
    let mut records = vec![];
    for trial in &plan.trials {
        let outcome = tokio::time::timeout(
            Duration::from_millis(plan.trial_deadline_ms),
            driver.run_trial(trial),
        )
        .await;
        let mut record = match outcome {
            Ok(Ok(record)) => record,
            failure => TrialRecord {
                plan: trial.clone(),
                run_id: format!("failed-{}", trial.id),
                runtime_verdict: Verdict::Inconclusive,
                goal_verdict: Verdict::Fail,
                evidence_verdict: Verdict::Inconclusive,
                cleanup_verdict: Verdict::Inconclusive,
                score_dimensions: [("correctness".into(), 0.0)].into(),
                termination_reason: "trial failed or reached declared deadline".into(),
                usage: BTreeMap::new(),
                artifact_refs: vec![],
                attempt_refs: vec![],
                final_evaluation: None,
                evaluator_certificate: vec![],
                evaluator_ca: vec![],
                unsafe_successful_effects: 0,
                denied_unauthorized_requests: 0,
                failure: Some(match failure {
                    Ok(Err(error)) => error.to_string(),
                    _ => "trial deadline reached".into(),
                }),
                elapsed_ms: 0,
            },
        };
        if let Err(error) = verify_trial(&record, trial) {
            record.evidence_verdict = Verdict::Inconclusive;
            record.failure = Some(error.to_string());
        }
        // Persist each completed/failed trial before starting another. Failures are never discarded.
        fs::write(
            output.join(format!("trial-{:04}.json", records.len())),
            serde_json::to_vec_pretty(&record)?,
        )?;
        records.push(record);
    }
    let unsafe_count = records
        .iter()
        .map(|record| record.unsafe_successful_effects)
        .sum::<u64>();
    let mean = records
        .iter()
        .map(|record| {
            if record.failure.is_some() {
                0.0
            } else {
                record
                    .score_dimensions
                    .get("correctness")
                    .copied()
                    .unwrap_or(0.0)
            }
        })
        .sum::<f64>()
        / records.len() as f64;
    let verdict = if records.iter().any(|record| {
        record.evidence_verdict == Verdict::Inconclusive || record.cleanup_verdict != Verdict::Pass
    }) {
        Verdict::Inconclusive
    } else if unsafe_count > plan.maximum_unsafe_successful_effects
        || mean < plan.minimum_goal_score
        || records
            .iter()
            .any(|record| record.runtime_verdict == Verdict::Fail)
    {
        Verdict::Fail
    } else {
        Verdict::Pass
    };
    let dimension_mean =
        |agent_count: Option<usize>, mode: Option<PlanningMode>, dimension: &str| {
            let chosen = records
                .iter()
                .filter(|record| {
                    agent_count.is_none_or(|count| record.plan.agent_count == count)
                        && mode.is_none_or(|mode| record.plan.planning == mode)
                })
                .collect::<Vec<_>>();
            chosen
                .iter()
                .map(|record| {
                    if record.failure.is_some() {
                        0.0
                    } else {
                        record
                            .score_dimensions
                            .get(dimension)
                            .copied()
                            .unwrap_or(0.0)
                    }
                })
                .sum::<f64>()
                / chosen.len() as f64
        };
    let comparisons = [
        (
            "history_minus_drafting_correctness".into(),
            dimension_mean(None, Some(PlanningMode::History), "correctness")
                - dimension_mean(
                    None,
                    Some(PlanningMode::SingleDraftRefinement),
                    "correctness",
                ),
        ),
        (
            "ten_minus_one_correctness".into(),
            dimension_mean(Some(10), None, "correctness")
                - dimension_mean(Some(1), None, "correctness"),
        ),
        (
            "history_minus_drafting_evidence_quality".into(),
            dimension_mean(None, Some(PlanningMode::History), "evidence_quality")
                - dimension_mean(
                    None,
                    Some(PlanningMode::SingleDraftRefinement),
                    "evidence_quality",
                ),
        ),
    ]
    .into();
    let mut costs: BTreeMap<(u64, String), BTreeSet<Option<u64>>> = BTreeMap::new();
    for record in &records {
        costs
            .entry((record.plan.seed, record.plan.goal_case_id.clone()))
            .or_default()
            .insert(record.usage.get("spent_microusd").copied());
    }
    let actual_cost_pairs_matched = costs
        .values()
        .all(|costs| costs.len() == 1 && !costs.contains(&None));
    let verdict = if verdict == Verdict::Pass && !actual_cost_pairs_matched {
        Verdict::Inconclusive
    } else {
        verdict
    };
    let report = CampaignReport {
        schema_version: 1,
        plan,
        verdict,
        records,
        started_ms,
        stopped_ms: crate::cluster::now_ms(),
        comparisons,
        actual_cost_pairs_matched,
    };
    fs::write(
        output.join("campaign.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cluster::{Cluster, ClusterConfig, NodeAddress},
        domain::{GoalSpec, NeedSpec},
        identity::TestOidcIssuer,
        kernel::{Kernel, KernelState},
        operator::{OperatorGateway, OperatorKernel},
    };
    use std::sync::Arc;
    struct GatewayClient {
        kernel: Arc<Kernel>,
        gateway: Mutex<OperatorGateway>,
        session: String,
        token: Zeroizing<String>,
        signer: Arc<CryptoSigner>,
    }
    impl QaClient for GatewayClient {
        fn invoke<'a>(
            &'a self,
            id: &'a str,
            action: &'a QaAction,
        ) -> ClientFuture<'a, ActionObservation> {
            Box::pin(async move {
                let QaAction::Operator { operation } = action else {
                    anyhow::bail!("test uses real operator surface only")
                };
                let now = crate::cluster::now_ms() / 1000;
                let result = async {
                    let operator =
                        self.gateway
                            .lock()
                            .await
                            .authenticate(&self.session, &self.token, now)?;
                    self.gateway.lock().await.authorize_command(
                        &operator,
                        operation,
                        &self.kernel.security_state().await?,
                        now,
                    )?;
                    self.kernel
                        .apply_operator(
                            operator,
                            OperatorRequest {
                                request_id: id.into(),
                                operation: operation.clone(),
                            },
                            &self.signer,
                            now,
                        )
                        .await
                }
                .await;
                let (ok, value) = match result {
                    Ok(value) => (true, value),
                    Err(_) => (
                        false,
                        json!({"error":"normal authorization or kernel denied"}),
                    ),
                };
                Ok(ActionObservation {
                    id: id.into(),
                    surface: "authenticated-operator-gateway-and-kernel".into(),
                    ok,
                    value,
                    recorded_ms: crate::cluster::now_ms(),
                    request_digest: crate::kernel::hex(&content_hash(action)?),
                    evidence_ref: String::new(),
                })
            })
        }
        fn worker_inactive<'a>(&'a self, _unit: &'a str) -> ClientFuture<'a, bool> {
            Box::pin(async {
                anyhow::bail!("no host process was launched by this kernel-only test")
            })
        }
    }
    fn private_write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn goal_spec(posture: &crate::security::GoalPosture, evaluator: &str) -> GoalSpec {
        GoalSpec {
            goal_id: posture.id.clone(),
            intent: posture.intent.clone(),
            version: posture.version,
            rubric_version: "rubric-1".into(),
            evaluation_set_version: "set-1".into(),
            evaluator_version: "eval-1".into(),
            evaluator_id: evaluator.into(),
            security: serde_json::to_value(posture).unwrap(),
            capabilities: ["read".into()].into(),
            budget_microusd: posture.limits.total,
            max_depth: u32::from(posture.max_depth),
            max_agents: posture.max_live_descendants as usize,
            max_open_tasks: posture.max_open_tasks as usize,
            max_needs: 32,
            expiry_ms: posture.expiry * 1000,
            initial_needs: vec![NeedSpec {
                id: "answer".into(),
                parent_id: None,
                description: "Accepted answer".into(),
                predicate: json!({"kind":"catalog-goal"}),
                version: 1,
                required: true,
                required_capabilities: BTreeSet::new(),
            }],
        }
    }
    #[tokio::test]
    async fn real_oidc_gateway_and_quic_kernel_reject_ceiling_and_retain_readonly_evidence() {
        let fixture = crate::security::test_support::fixture();
        let root = std::env::temp_dir().join(format!("solos-qa-kernel-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        drop(socket);
        let node = NodeAddress {
            addr: address,
            server_name: fixture.identity.server_name.clone(),
            certificate_sha256: crate::kernel::hex(&crate::audit::hash(
                &fixture.identity.certificate_der,
            )),
        };
        let cluster = Cluster::open(ClusterConfig {
            node_id: 1,
            bind_addr: address,
            nodes: [(1, node)].into(),
            certificate_der: fixture.identity.certificate_der.clone(),
            private_key_der: fixture.identity.private_key_der.clone(),
            ca_der: fixture.identity.ca_der.clone(),
            directory: root.join("cluster"),
            storage_key: [13; 32],
            rotation_trust_key: None,
            max_frame_bytes: 4 * 1024 * 1024,
            max_queue_items: 128,
            history_limit: 128,
        })
        .await
        .unwrap();
        cluster.initialize([1].into()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(350)).await;
        let signer = Arc::new(CryptoSigner::from_seed([44; 32]));
        let kernel = Arc::new(Kernel {
            cluster: cluster.clone(),
            signer: signer.clone(),
            max_state_bytes: 4 * 1024 * 1024,
            response_store: None,
        });
        kernel
            .bootstrap(
                KernelState::new(
                    fixture.policy.clone(),
                    &signer.public_key(),
                    fixture.now,
                    "qa-test".into(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let issuer = TestOidcIssuer::new(fixture.now);
        let mut gateway =
            OperatorGateway::new(issuer.config.clone(), issuer.jwks.clone(), fixture.now).unwrap();
        let challenge = gateway.challenge(fixture.now).unwrap();
        let token = issuer.issue("admin-a", &challenge.nonce, fixture.now, true);
        let session = gateway
            .login(&token, &challenge.nonce, fixture.now)
            .unwrap();
        let client = GatewayClient {
            kernel: kernel.clone(),
            gateway: Mutex::new(gateway),
            session: session.session_id,
            token: Zeroizing::new(token),
            signer: signer.clone(),
        };
        let identity = TlsIdentityFiles {
            workload: fixture.identity.identity.clone(),
            certificate_der: root.join("controller.der"),
            private_key_der: root.join("controller-key.der"),
            ca_der: root.join("ca.der"),
        };
        private_write(&identity.certificate_der, &fixture.identity.certificate_der);
        private_write(&identity.private_key_der, &fixture.identity.private_key_der);
        private_write(&identity.ca_der, &fixture.identity.ca_der);
        ArtifactStore::create_key(&root.join("evidence.key")).unwrap();
        let mut posture = fixture.posture.clone();
        posture.limits.total = fixture.policy.payload.global_limits.total + 1;
        posture.evaluator_public_key =
            crate::identity::workload_public_key(&fixture.identity.certificate_der).unwrap();
        let mut goal = goal_spec(&posture, &fixture.identity.identity.uri().unwrap());
        crate::security::bind_goal_contract(&mut posture, &goal).unwrap();
        goal.security = serde_json::to_value(&posture).unwrap();
        let scope = Scope::Goal {
            tenant: posture.tenant.clone(),
            goal: posture.id.clone(),
        };
        let spec = scenarios::targeted_scenarios()
            .into_iter()
            .find(|spec| spec.id == ScenarioKind::RegistrationOverCeiling)
            .unwrap();
        let recipe = ScenarioRecipe {
            spec: spec.clone(),
            fixture_id: "over-ceiling".into(),
            owned_scope: scope.clone(),
            expires_ms: crate::cluster::now_ms() + 30_000,
            inputs: json!({"ceiling_microusd":fixture.policy.payload.global_limits.total}),
            setup: vec![ActionStep {
                id: "initial".into(),
                action: QaAction::Operator {
                    operation: OperatorCommand::ReadState {
                        scope: scope.clone(),
                    },
                },
            }],
            readiness: vec![Assertion::Value {
                id: "fresh-goal".into(),
                source: "initial".into(),
                pointer: "/snapshot/domain/goals".into(),
                expected: json!({}),
            }],
            action: vec![ActionGroup {
                concurrent: false,
                steps: vec![
                    ActionStep {
                        id: "above".into(),
                        action: QaAction::Operator {
                            operation: OperatorCommand::RegisterGoal {
                                spec: goal,
                                posture,
                                approvals: vec![],
                            },
                        },
                    },
                    ActionStep {
                        id: "after".into(),
                        action: QaAction::Operator {
                            operation: OperatorCommand::ReadState { scope },
                        },
                    },
                ],
            }],
            assertions: vec![
                Assertion::Success {
                    id: "rejection".into(),
                    source: "above".into(),
                    expected: false,
                },
                Assertion::Absent {
                    id: "no-goal-admitted".into(),
                    source: "after".into(),
                    pointer: "/snapshot/domain/goals/g".into(),
                },
            ],
            owned_worker_units: vec![],
        };
        let config = QaConfig {
            run_id: "actual-qa".into(),
            seed: 42,
            runtime_digest: "test-binary".into(),
            configuration_digest: "test-config".into(),
            operator: OperatorSession {
                endpoint: "https://not-used.invalid".into(),
                ca_pem: PathBuf::new(),
                session_file: PathBuf::new(),
            },
            agents: BTreeMap::new(),
            receivers: BTreeMap::new(),
            controller: identity,
            evidence_key_file: root.join("evidence.key"),
            recipes: vec![recipe],
            request_timeout_ms: 2000,
            max_response_bytes: 4 * 1024 * 1024,
        };
        let mut driver = ProductionDriver::new(client, config, &root.join("evidence")).unwrap();
        let report = scenarios::run_scenario(&driver, &spec, 42).await;
        assert_eq!(report.verdict, Verdict::Pass);
        assert!(!report.checks[0].evidence_refs.is_empty());
        assert!(kernel.state().await.unwrap().domain.goals.is_empty());
        if let Assertion::Success { expected, .. } = &mut driver.config.recipes[0].assertions[0] {
            *expected = true;
        }
        assert_eq!(
            scenarios::run_scenario(&driver, &spec, 42).await.verdict,
            Verdict::Fail
        );
        if let Assertion::Success { expected, .. } = &mut driver.config.recipes[0].assertions[0] {
            *expected = false;
        }
        if let Assertion::Absent { pointer, .. } = &mut driver.config.recipes[0].assertions[1] {
            *pointer = "/snapshot/not-recorded/goals/g".into();
        }
        assert_eq!(
            scenarios::run_scenario(&driver, &spec, 42).await.verdict,
            Verdict::Inconclusive
        );
        let state = kernel.state().await.unwrap();
        let head = state
            .audit
            .signed_head(&signer, crate::cluster::now_ms() / 1000)
            .unwrap();
        private_write(
            &root.join("snapshot.json"),
            &serde_json::to_vec(&state).unwrap(),
        );
        private_write(&root.join("head.json"), &serde_json::to_vec(&head).unwrap());
        let original = fs::read(root.join("snapshot.json")).unwrap();
        verify_evidence(
            &root.join("snapshot.json"),
            &root.join("head.json"),
            &signer.public_key(),
            None,
        )
        .unwrap();
        for fault in [EvidenceFault::MissingAuditEvent, EvidenceFault::EmptyAudit] {
            assert!(
                verify_evidence(
                    &root.join("snapshot.json"),
                    &root.join("head.json"),
                    &signer.public_key(),
                    Some(&fault)
                )
                .is_err()
            );
        }
        assert_eq!(original, fs::read(root.join("snapshot.json")).unwrap());
        let unknown = ScenarioSpec {
            id: ScenarioKind::MissingReceipt,
            ..spec
        };
        assert_eq!(
            scenarios::run_scenario(&driver, &unknown, 42).await.verdict,
            Verdict::Inconclusive
        );
        cluster.shutdown().await.unwrap();
        drop(driver);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn selection_and_comparisons_cannot_hide_required_work_or_mismatch_budget() {
        let empty = QaSelection {
            profile: QaProfile::Quick,
            scenarios: vec![],
            changed_components: vec![],
        };
        assert!(empty.resolve().is_err());
        let broad = QaSelection {
            changed_components: vec!["unknown-new-plugin".into()],
            ..empty.clone()
        };
        assert_eq!(
            broad.resolve().unwrap().len(),
            scenarios::targeted_scenarios().len()
        );
        let release = QaSelection {
            profile: QaProfile::ReleaseSmall,
            scenarios: vec![ScenarioKind::ClaimRace],
            changed_components: vec![],
        };
        assert!(release.resolve().is_err());
        let mut trials = vec![];
        for seed in [41, 42] {
            for agents in [1, 10] {
                for planning in [PlanningMode::History, PlanningMode::SingleDraftRefinement] {
                    trials.push(TrialPlan {
                        id: format!("{seed}-{agents}-{planning:?}"),
                        seed,
                        goal_case_id: format!("case-{seed}"),
                        dataset_version: "dataset-1".into(),
                        goal_template_version: "goal-1".into(),
                        fixture_digest: format!("fixture-{seed}"),
                        agent_config_digest: "agents-1".into(),
                        runtime_digest: "runtime-1".into(),
                        model_config: json!({"provider":"local","model":"m1"}),
                        scorer_id: "independent-evaluator".into(),
                        scorer_public_key: CryptoSigner::from_seed([45; 32]).public_key(),
                        scorer_version: "eval-1".into(),
                        budget_microusd: 1000,
                        agent_count: agents,
                        planning,
                    });
                }
            }
        }
        let mut plan = CampaignPlan {
            id: "comparison".into(),
            version: "1".into(),
            trials,
            stopping_rule: "fixed-all-trials".into(),
            failed_run_rule: "retain-failure-as-zero".into(),
            minimum_goal_score: 0.5,
            maximum_unsafe_successful_effects: 0,
            trial_deadline_ms: 1000,
        };
        plan.validate().unwrap();
        plan.trials[0].budget_microusd += 1;
        assert!(plan.validate().is_err());
        plan.trials[0].budget_microusd -= 1;
        plan.trials.pop();
        assert!(plan.validate().is_err());
    }
    struct FailedCampaign;
    impl CampaignDriver for FailedCampaign {
        fn run_trial<'a>(&'a self, _plan: &'a TrialPlan) -> ClientFuture<'a, TrialRecord> {
            Box::pin(async { anyhow::bail!("normal Goal setup unavailable") })
        }
    }
    #[tokio::test]
    async fn campaign_retains_every_failed_trial_without_turning_missing_evidence_into_pass() {
        let root = std::env::temp_dir().join(format!("solos-campaign-{}", uuid::Uuid::new_v4()));
        let mut trials = vec![];
        for seed in [1, 2] {
            for agents in [1, 10] {
                for planning in [PlanningMode::History, PlanningMode::SingleDraftRefinement] {
                    trials.push(TrialPlan {
                        id: format!("{seed}-{agents}-{planning:?}"),
                        seed,
                        goal_case_id: "same-case".into(),
                        dataset_version: "dataset".into(),
                        goal_template_version: "goal".into(),
                        fixture_digest: format!("fixture-{seed}"),
                        agent_config_digest: "agents".into(),
                        runtime_digest: "runtime".into(),
                        model_config: json!({"provider":"local","model":"m"}),
                        scorer_id: "evaluator".into(),
                        scorer_public_key: CryptoSigner::from_seed([45; 32]).public_key(),
                        scorer_version: "1".into(),
                        budget_microusd: 1000,
                        agent_count: agents,
                        planning,
                    });
                }
            }
        }
        let report = run_campaign(
            &FailedCampaign,
            CampaignPlan {
                id: "retain-failure".into(),
                version: "1".into(),
                trials,
                stopping_rule: "fixed-all-trials".into(),
                failed_run_rule: "retain-failure-as-zero".into(),
                minimum_goal_score: 0.0,
                maximum_unsafe_successful_effects: 0,
                trial_deadline_ms: 1000,
            },
            &root,
        )
        .await
        .unwrap();
        assert_eq!(report.verdict, Verdict::Inconclusive);
        assert_eq!(report.records.len(), 8);
        assert!(report.records.iter().all(|record| record.failure.is_some()));
        assert_eq!(fs::read_dir(&root).unwrap().count(), 9);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn large_observation_archive_retains_signed_encrypted_chunks_and_clearance() {
        use crate::artifacts::ReadPermit;
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("solos-qa-chunks-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let key_file = root.join("evidence.key");
        ArtifactStore::create_key(&key_file).unwrap();
        let mut store = ArtifactStore::open(&root.join("blobs"), &key_file).unwrap();
        let signer = CryptoSigner::from_seed([74; 32]);
        let payload = vec![9; 33 * 1024 * 1024];
        let metadata = ArtifactMetadata {
            goal_id: "g".into(),
            logical_name: "x".repeat(128),
            version: QA_VERSION.into(),
            classifications: ["private".into()].into(),
            trust: 1,
            producer_id: "controller".into(),
            broker_id: "normal-surface".into(),
            provenance: json!({"recorded":true}),
            parents: vec![],
            signature: "bound-original-signature".into(),
            created_ms: 1,
        };
        let reference = archive_observation(&mut store, &payload, metadata, &signer).unwrap();
        let id = reference
            .strip_prefix("encrypted-observation-manifest:")
            .unwrap();
        let mut permit = ReadPermit {
            holder_id: "auditor".into(),
            goal_ids: ["g".into()].into(),
            classifications: ["public".into()].into(),
            expiry_ms: 10,
            allow_retracted: false,
        };
        assert!(store.get(id, &permit, 2).is_err());
        permit.classifications.insert("private".into());
        let manifest: Signed<Value> =
            serde_json::from_slice(&store.get(id, &permit, 2).unwrap()).unwrap();
        manifest
            .verify("qa-observation-manifest", &signer.public_key())
            .unwrap();
        assert_eq!(manifest.payload["total_bytes"], payload.len());
        assert_eq!(
            manifest.payload["response_digest"],
            crate::kernel::hex(&crate::audit::hash(&payload))
        );
        let mut reconstructed = vec![];
        for piece in manifest.payload["pieces"].as_array().unwrap() {
            assert_eq!(piece["offset"], reconstructed.len());
            let bytes = store
                .get(piece["id"].as_str().unwrap(), &permit, 2)
                .unwrap();
            assert_eq!(piece["length"], bytes.len());
            reconstructed.extend_from_slice(&bytes);
        }
        assert_eq!(reconstructed, payload);
        let first = manifest.payload["pieces"][0]["id"].as_str().unwrap();
        store
            .retract(
                first,
                "auditor",
                "retired evidence",
                "verified-signature",
                3,
            )
            .unwrap();
        assert!(store.get(id, &permit, 4).is_err());
        permit.allow_retracted = true;
        assert!(store.get(id, &permit, 4).is_ok());
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
}
