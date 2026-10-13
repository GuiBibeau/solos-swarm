//! Protected evaluator process. Proposers receive signed scores, never fixtures or tool logs.
use crate::{
    audit::{CryptoSigner, Hash, Signed, canonical, content_hash},
    domain::{DomainCommand, DomainState, EvaluationContract, NeedSpec, Score, hash_json},
    identity::{WorkloadId, verify_workload_certificate, workload_public_key},
    sandbox::{self, MicroVmAssets, NativeToolSpec, PinnedAsset, ProcessLimits, ToolLimits},
    scenarios::{self, CatalogCase, CatalogFamily, Verdict},
    transport::Message,
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey, pkcs8::DecodePrivateKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, Command},
    sync::Mutex,
};
use zeroize::Zeroizing;

const SIGNATURE_DOMAIN: &str = "protected-evaluation-v1";
const REQUEST_DOMAIN: &str = "protected-evaluation-request-v1";
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const EVALUATOR_COMMAND_PATH: &str = "/evaluator/command";
#[cfg(target_os = "linux")]
fn process_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationPhase {
    Search,
    HeldOut,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Candidate {
    Artifact {
        value: Value,
    },
    Wasi {
        #[serde(with = "module_bytes")]
        module: Vec<u8>,
        sha256: String,
        #[serde(default)]
        artifact_ref: Option<String>,
    },
    Native {
        language: NativeLanguage,
        source: String,
        sha256: String,
        #[serde(default)]
        artifact_ref: Option<String>,
    },
}
mod module_bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _, ser::Error as _};
    const MAXIMUM: usize = 4 * 1024 * 1024;
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        if bytes.len() > MAXIMUM {
            return Err(S::Error::custom("candidate module exceeds byte ceiling"));
        }
        serializer.serialize_str(&STANDARD.encode(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() > MAXIMUM.div_ceil(3) * 4 {
            return Err(D::Error::custom("candidate module exceeds encoded ceiling"));
        }
        let bytes = STANDARD.decode(encoded).map_err(D::Error::custom)?;
        if bytes.len() > MAXIMUM {
            return Err(D::Error::custom("candidate module exceeds byte ceiling"));
        }
        Ok(bytes)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeLanguage {
    Python,
    JavaScript,
}
impl Candidate {
    /// Programs are immutable content references in the Attempt; executable bytes travel privately.
    pub fn attempt_payload(&self) -> Result<Value> {
        Ok(match self {
            Self::Artifact { value } => value.clone(),
            Self::Wasi {
                module,
                sha256,
                artifact_ref,
            } => {
                ensure!(
                    module.len() <= 4 * 1024 * 1024 && digest_bytes(module) == *sha256,
                    "candidate module digest or size invalid"
                );
                let mut payload = json!({"kind":"wasi","sha256":sha256});
                if let Some(reference) = artifact_ref {
                    payload["artifact_ref"] = json!(reference);
                }
                payload
            }
            Self::Native {
                language,
                source,
                sha256,
                artifact_ref,
            } => {
                ensure!(
                    source.len() <= 8192
                        && !source.contains('\0')
                        && digest_bytes(source.as_bytes()) == *sha256,
                    "candidate source digest or size invalid"
                );
                let mut payload = json!({"kind":"native","language":language,"sha256":sha256});
                if let Some(reference) = artifact_ref {
                    payload["artifact_ref"] = json!(reference);
                }
                payload
            }
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvaluationAction {
    Candidate {
        attempt_id: String,
        candidate_hash: String,
        candidate: Candidate,
        need_ids: Vec<String>,
        phase: EvaluationPhase,
    },
    AcceptNeed {
        spec: NeedSpec,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationRequest {
    pub request_id: String,
    pub goal_id: String,
    pub contract: EvaluationContract,
    pub finding_ids: Vec<String>,
    pub state_revision: u64,
    pub issued_ms: u64,
    pub deadline_ms: u64,
    pub action: EvaluationAction,
}
impl EvaluationRequest {
    pub fn sign(self, broker: &CryptoSigner) -> Result<Signed<Self>> {
        broker.sign(REQUEST_DOMAIN, self)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedCommand {
    pub command: DomainCommand,
    /// Submit through the authenticated evaluator route; its signature binds exact command bytes.
    pub message: Message,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationBatch {
    /// Exact ID of the immutable broker-signed request retained by the control plane.
    /// Candidate bytes remain on the protected evaluation channel.
    pub request_id: String,
    pub result: Signed<EvaluationResult>,
}
impl EvaluationBatch {
    pub fn resolve_request<'a>(
        &self,
        requests: impl IntoIterator<Item = &'a Signed<EvaluationRequest>>,
        broker_public_key: &crate::audit::Hash,
    ) -> Result<&'a Signed<EvaluationRequest>> {
        ensure!(
            !self.request_id.is_empty()
                && self.request_id.len() <= 128
                && self.result.payload.request_id == self.request_id,
            "evaluation result changed durable request identity"
        );
        let mut matched = requests
            .into_iter()
            .filter(|request| request.payload.request_id == self.request_id);
        let request = matched.next().context("evaluation request not retained")?;
        ensure!(
            matched.next().is_none(),
            "evaluation request identity ambiguous"
        );
        request.verify(REQUEST_DOMAIN, broker_public_key)?;
        Ok(request)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationResult {
    pub request_id: String,
    pub goal_id: String,
    pub evaluator_id: String,
    pub contract: EvaluationContract,
    pub state_revision: u64,
    pub attempt_id: Option<String>,
    pub candidate_hash: Option<String>,
    pub finding_ids: Vec<String>,
    pub phase: Option<EvaluationPhase>,
    pub score: Option<Score>,
    pub measurements: BTreeMap<String, f64>,
    pub verdict: Verdict,
    pub commands: Vec<SignedCommand>,
    pub protected_set_digest: String,
    pub evaluation_scope: String,
    pub measured_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinalVerdict {
    pub goal_id: String,
    pub attempt_id: String,
    pub candidate_hash: String,
    pub contract: EvaluationContract,
    pub verdict: Verdict,
    pub evaluator_id: String,
    pub protected_set_digest: String,
    pub evaluation_scope: String,
    pub measured_ms: u64,
    pub signed_result_digest: String,
    pub measurements: BTreeMap<String, f64>,
}
impl EvaluationResult {
    /// Root may attach this only after verify_current; it is not an agent assertion.
    pub fn final_verdict(result: &Signed<Self>) -> Result<FinalVerdict> {
        let value = &result.payload;
        ensure!(
            value.phase == Some(EvaluationPhase::HeldOut) && value.score.is_none(),
            "result is not a separate final verdict"
        );
        ensure!(
            value
                .measurements
                .values()
                .all(|number| number.is_finite() && (0.0..=1.0).contains(number)),
            "final measurement must be finite and normalized"
        );
        Ok(FinalVerdict {
            goal_id: value.goal_id.clone(),
            attempt_id: value.attempt_id.clone().context("final Attempt absent")?,
            candidate_hash: value
                .candidate_hash
                .clone()
                .context("final candidate absent")?,
            contract: value.contract.clone(),
            verdict: value.verdict,
            evaluator_id: value.evaluator_id.clone(),
            protected_set_digest: value.protected_set_digest.clone(),
            evaluation_scope: value.evaluation_scope.clone(),
            measured_ms: value.measured_ms,
            signed_result_digest: hash_json(result)?,
            measurements: value.measurements.clone(),
        })
    }
}

/// Root must recheck this against current committed state before any command admission.
pub fn verify_current(
    result: &Signed<EvaluationResult>,
    request: &Signed<EvaluationRequest>,
    state: &DomainState,
    certificate: &[u8],
    ca: &[u8],
    now_ms: u64,
) -> Result<()> {
    let payload = &result.payload;
    let id = WorkloadId::parse(&payload.evaluator_id)?;
    verify_workload_certificate(certificate, ca, &id, now_ms / 1000)?;
    let key = workload_public_key(certificate)?;
    result.verify(SIGNATURE_DOMAIN, &key)?;
    ensure!(
        payload
            .measurements
            .values()
            .all(|number| number.is_finite() && (0.0..=1.0).contains(number)),
        "evaluator measurements must be finite and normalized"
    );
    let submitted = &request.payload;
    ensure!(
        payload.request_id == submitted.request_id
            && payload.goal_id == submitted.goal_id
            && payload.state_revision == submitted.state_revision
            && payload.contract == submitted.contract
            && payload.finding_ids == submitted.finding_ids
            && payload.measured_ms >= submitted.issued_ms
            && payload.measured_ms <= submitted.deadline_ms,
        "evaluation request binding mismatch"
    );
    let goal = state.goal(&payload.goal_id)?;
    let evaluator_key: Hash =
        serde_json::from_value(goal.spec.security["evaluator_public_key"].clone())
            .context("Goal lacks approved evaluator public key")?;
    ensure!(
        key == evaluator_key,
        "evaluator key differs from approved Goal pin"
    );
    ensure!(
        goal.spec.evaluator_id == payload.evaluator_id
            && goal.spec.expiry_ms > now_ms
            && goal.status != "stopped"
            && payload.contract == state.current_contract(&payload.goal_id)?,
        "evaluation Goal contract is no longer current"
    );
    for finding in &payload.finding_ids {
        ensure!(
            state
                .findings
                .get(finding)
                .is_some_and(|record| record.goal_id == payload.goal_id)
                && state.finding_eligible(finding),
            "evaluation depends on missing or retracted evidence"
        );
    }
    match &submitted.action {
        EvaluationAction::Candidate {
            attempt_id,
            candidate_hash,
            candidate,
            phase,
            need_ids,
        } => {
            let attempt = state.attempt(&payload.goal_id, attempt_id)?;
            ensure!(
                state.attempt_eligible(attempt)
                    && attempt.candidate_hash == *candidate_hash
                    && hash_json(&candidate.attempt_payload()?)? == *candidate_hash
                    && attempt.finding_ids == payload.finding_ids
                    && payload.attempt_id.as_ref() == Some(attempt_id)
                    && payload.candidate_hash.as_ref() == Some(candidate_hash)
                    && payload.phase.as_ref() == Some(phase),
                "evaluation candidate or lineage is no longer current"
            );
            match phase {
                EvaluationPhase::HeldOut => {
                    ensure!(
                        payload.score.is_none() && payload.commands.len() == need_ids.len(),
                        "held-out verdict must not enter scored search history"
                    );
                    for (index, need_id) in need_ids.iter().enumerate() {
                        let need = state.need(&payload.goal_id, need_id)?;
                        ensure!(
                            need.spec.predicate == json!({"kind":"catalog-final"}),
                            "held-out command may satisfy only the protected final Need"
                        );
                        let expected = DomainCommand::EvaluateNeed {
                            goal_id: payload.goal_id.clone(),
                            need_id: need_id.clone(),
                            satisfied: payload.verdict == Verdict::Pass,
                            finding_ids: payload.finding_ids.clone(),
                            contract: payload.contract.clone(),
                            signature: String::new(),
                        };
                        ensure!(
                            hash_json(&payload.commands[index].command)? == hash_json(&expected)?,
                            "final acceptance command changed"
                        );
                    }
                }
                EvaluationPhase::Search => {
                    ensure!(
                        need_ids
                            .iter()
                            .all(|id| state
                                .need(&payload.goal_id, id)
                                .is_ok_and(
                                    |need| need.spec.predicate != json!({"kind":"catalog-final"})
                                )),
                        "search cannot satisfy the protected final Need"
                    );
                    let score = payload.score.as_ref().context("search score missing")?;
                    ensure!(
                        score.candidate_hash == *candidate_hash
                            && score.contract == payload.contract
                            && score.evaluator_id == payload.evaluator_id
                            && finite_score(score),
                        "invalid or unbound evaluator score"
                    );
                    ensure!(
                        payload.commands.len() == 1 + need_ids.len(),
                        "evaluation command selection differs from request"
                    );
                    let mut command_score = score.clone();
                    command_score.signature.clear();
                    let expected = DomainCommand::ScoreAttempt {
                        goal_id: payload.goal_id.clone(),
                        attempt_id: attempt_id.clone(),
                        score: command_score,
                    };
                    ensure!(
                        hash_json(&payload.commands[0].command)? == hash_json(&expected)?
                            && score.signature == payload.commands[0].message.signature,
                        "search score command differs from measured result"
                    );
                    for (index, need_id) in need_ids.iter().enumerate() {
                        let expected = DomainCommand::EvaluateNeed {
                            goal_id: payload.goal_id.clone(),
                            need_id: need_id.clone(),
                            satisfied: payload.verdict == Verdict::Pass,
                            finding_ids: payload.finding_ids.clone(),
                            contract: payload.contract.clone(),
                            signature: String::new(),
                        };
                        ensure!(
                            hash_json(&payload.commands[index + 1].command)?
                                == hash_json(&expected)?,
                            "Need evaluation differs from requested scope"
                        );
                    }
                }
            }
        }
        EvaluationAction::AcceptNeed { spec } => {
            let need = state.need(&payload.goal_id, &spec.id)?;
            ensure!(
                hash_json(&need.spec)? == hash_json(spec)?
                    && payload.score.is_none()
                    && payload.attempt_id.is_none()
                    && payload.commands.len() == 1,
                "Need changed during review"
            );
            let expected = DomainCommand::AcceptNeed {
                goal_id: payload.goal_id.clone(),
                need_id: spec.id.clone(),
                goal_version: payload.contract.goal_version,
                signature: String::new(),
            };
            ensure!(
                hash_json(&payload.commands[0].command)? == hash_json(&expected)?,
                "Need acceptance command changed"
            );
        }
    }
    for command in &payload.commands {
        let observed = command.message.verify(&key, now_ms, MAX_FRAME_BYTES)?;
        ensure!(
            command.message.path == EVALUATOR_COMMAND_PATH
                && observed == serde_json::to_value(&command.command)?,
            "evaluator command bytes changed"
        );
    }
    Ok(())
}
fn finite_score(score: &Score) -> bool {
    score.value.is_finite()
        && (0.0..=1.0).contains(&score.value)
        && !score.dimensions.is_empty()
        && score
            .dimensions
            .values()
            .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
}
fn digest_bytes(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Final acceptance is a separately implemented strict contract check, not a replay of
/// the search grader. It uses only this service's frozen oracle and candidate output.
fn final_contract_check(case: &CatalogCase, artifact: &Value) -> Result<bool> {
    #[derive(Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    struct Row {
        id: String,
        source_a: u64,
        source_b: u64,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Records {
        inconsistencies: Vec<Row>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Component {
        id: String,
        dependencies: BTreeSet<String>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Inventory {
        components: Vec<Component>,
        max_wave_size: usize,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Plan {
        order: Vec<String>,
        waves: Vec<Vec<String>>,
    }
    #[derive(Deserialize, PartialEq, Eq, PartialOrd, Ord)]
    #[serde(deny_unknown_fields)]
    struct Citation {
        document_id: String,
        revision: u64,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        question_id: String,
        value: u64,
        citations: Vec<Citation>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Evidence {
        answers: Vec<Answer>,
    }
    match case.family {
        CatalogFamily::RecordReconciliation => {
            let expected: Records = serde_json::from_value(case.expected.clone())?;
            let actual: Records = serde_json::from_value(artifact.clone())?;
            let normalize = |records: Records| -> Result<BTreeMap<String, (u64, u64)>> {
                let mut rows = BTreeMap::new();
                for row in records.inconsistencies {
                    ensure!(
                        !row.id.is_empty()
                            && row.source_a != row.source_b
                            && rows.insert(row.id, (row.source_a, row.source_b)).is_none(),
                        "final inconsistent row is duplicate or malformed"
                    );
                }
                Ok(rows)
            };
            Ok(normalize(actual)? == normalize(expected)?)
        }
        CatalogFamily::DependencyPlanning => {
            let expected: Inventory = serde_json::from_value(case.expected.clone())?;
            let plan: Plan = serde_json::from_value(artifact.clone())?;
            ensure!(
                expected.max_wave_size > 0,
                "protected wave capacity invalid"
            );
            let mut inventory = BTreeMap::new();
            for component in expected.components {
                ensure!(
                    inventory
                        .insert(component.id, component.dependencies)
                        .is_none(),
                    "protected inventory duplicate"
                );
            }
            let flattened: Vec<_> = plan.waves.iter().flatten().cloned().collect();
            ensure!(flattened == plan.order, "final wave order mismatch");
            let mut completed = BTreeSet::new();
            for wave in plan.waves {
                ensure!(
                    !wave.is_empty() && wave.len() <= expected.max_wave_size,
                    "final wave capacity violated"
                );
                let distinct: BTreeSet<_> = wave.iter().cloned().collect();
                ensure!(
                    distinct.len() == wave.len() && distinct.is_disjoint(&completed),
                    "final component duplicate"
                );
                for id in &distinct {
                    let dependencies = inventory.get(id).context("final unknown component")?;
                    ensure!(
                        dependencies.is_subset(&completed),
                        "final dependency not completed in earlier wave"
                    );
                }
                completed.extend(distinct);
            }
            Ok(completed == inventory.keys().cloned().collect::<BTreeSet<_>>())
        }
        CatalogFamily::EvidenceSynthesis => {
            let expected: Evidence = serde_json::from_value(case.expected.clone())?;
            let actual: Evidence = serde_json::from_value(artifact.clone())?;
            let normalize = |evidence: Evidence| -> Result<BTreeMap<String, (u64, Vec<Citation>)>> {
                let mut answers = BTreeMap::new();
                for mut answer in evidence.answers {
                    answer.citations.sort();
                    ensure!(
                        answer
                            .citations
                            .windows(2)
                            .all(|pair| pair[0].document_id != pair[1].document_id),
                        "final duplicate document citation"
                    );
                    ensure!(
                        answers
                            .insert(answer.question_id, (answer.value, answer.citations))
                            .is_none(),
                        "final duplicate question"
                    );
                }
                Ok(answers)
            };
            Ok(normalize(actual)? == normalize(expected)?)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedCase {
    pub family: CatalogFamily,
    pub seed: u64,
    pub catalog_version: String,
    pub scorer_version: String,
    pub fixture_sha256: String,
    pub expected: Value,
}
impl ProtectedCase {
    pub fn freeze(case: &CatalogCase) -> Result<Self> {
        Ok(serde_json::from_value(case.protected_value())?)
    }
    fn load_case(&self) -> Result<CatalogCase> {
        let mut case = scenarios::prepare_case(self.family, self.seed);
        ensure!(
            self.catalog_version == scenarios::CATALOG_VERSION
                && self.scorer_version == "1"
                && case.fixture_digest() == self.fixture_sha256,
            "protected catalog version or digest mismatch"
        );
        // The frozen oracle is authoritative. It never leaves this process.
        case.expected = self.expected.clone();
        Ok(case)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedGoal {
    pub contract: EvaluationContract,
    pub need_specs: BTreeMap<String, NeedSpec>,
    pub search: Vec<ProtectedCase>,
    pub held_out: Vec<ProtectedCase>,
    pub max_search_evaluations: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "tier", deny_unknown_fields)]
pub enum NativePolicy {
    #[serde(rename = "V")]
    MicroVm { assets: MicroVmAssets },
    #[serde(rename = "G")]
    Gvisor {
        runsc: PinnedAsset,
        rootfs: sandbox::PinnedTree,
        allow_no_kvm_fallback: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedBundle {
    pub schema_version: u32,
    pub broker_public_key: Hash,
    pub evaluator: WorkloadId,
    pub goals: BTreeMap<String, ProtectedGoal>,
    pub wasi_limits: ToolLimits,
    pub native_limits: ProcessLimits,
    pub native_policy: Option<NativePolicy>,
    pub native_work_root: PathBuf,
    #[serde(default)]
    pub delivery: Option<DeliveryTarget>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluatorFiles {
    pub bundle: PathBuf,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub ca: PathBuf,
    pub state: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeliveryTarget {
    pub socket: PathBuf,
    pub endpoint: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationDelivery {
    pub request_id: String,
    pub request: Signed<EvaluationRequest>,
    pub result_digest: String,
    pub issued_ms: u64,
    pub deadline_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluatorCsrRequest {
    pub request_id: String,
    pub nonce: String,
    pub issued_ms: u64,
    pub deadline_ms: u64,
}
impl EvaluatorCsrRequest {
    pub fn sign(self, broker: &CryptoSigner) -> Result<Signed<Self>> {
        broker.sign("protected-evaluator-csr-request-v1", self)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluatorCsrProof {
    pub request_id: String,
    pub nonce: String,
    pub workload: WorkloadId,
    pub public_key: Hash,
    pub csr_pem: String,
    pub current_certificate_sha256: String,
    pub issued_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluatorCertificateInstall {
    pub request_id: String,
    pub nonce: String,
    pub certificate_pem: String,
    pub ca_pem: String,
    pub issued_ms: u64,
    pub deadline_ms: u64,
}
impl EvaluatorCertificateInstall {
    pub fn sign(self, broker: &CryptoSigner) -> Result<Signed<Self>> {
        broker.sign("protected-evaluator-certificate-install-v1", self)
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct RenewalChallenge {
    request_hash: Hash,
    proof: Signed<EvaluatorCsrProof>,
    installed_sha256: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", content = "request", rename_all = "snake_case")]
enum ServiceControl {
    Csr(Signed<EvaluatorCsrRequest>),
    Install(Signed<EvaluatorCertificateInstall>),
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlEnvelope {
    control: ServiceControl,
}
impl EvaluationDelivery {
    pub fn sign(self, broker: &CryptoSigner) -> Result<Signed<Self>> {
        broker.sign("protected-evaluator-delivery-v1", self)
    }
}

#[derive(Serialize, Deserialize)]
struct PrivateState {
    schema_version: u32,
    replies: BTreeMap<String, (Hash, Signed<EvaluationResult>)>,
    scored: BTreeSet<String>,
    held_out: BTreeMap<String, String>,
    accepted: BTreeMap<String, NeedSpec>,
    #[serde(default)]
    pending: BTreeMap<String, Hash>,
    #[serde(default)]
    renewal: BTreeMap<String, RenewalChallenge>,
}
impl Default for PrivateState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            replies: BTreeMap::new(),
            scored: BTreeSet::new(),
            held_out: BTreeMap::new(),
            accepted: BTreeMap::new(),
            pending: BTreeMap::new(),
            renewal: BTreeMap::new(),
        }
    }
}
pub struct ProtectedEvaluator {
    bundle: ProtectedBundle,
    signer: CryptoSigner,
    signing_key: SigningKey,
    state: PrivateState,
    state_path: PathBuf,
    certificate: Vec<u8>,
    ca: Vec<u8>,
    certificate_path: PathBuf,
}
impl ProtectedEvaluator {
    pub fn load(files: &EvaluatorFiles, now_ms: u64) -> Result<Self> {
        let bundle_bytes = protected_read(&files.bundle, MAX_FRAME_BYTES)?;
        let bundle: ProtectedBundle =
            serde_json::from_slice(&bundle_bytes).context("protected evaluator bundle invalid")?;
        ensure!(
            bundle.schema_version == 1 && !bundle.goals.is_empty(),
            "protected evaluator bundle incomplete"
        );
        bundle.evaluator.validate()?;
        bundle.wasi_limits.validate()?;
        let certificate = fs::read(&files.certificate)?;
        let ca = fs::read(&files.ca)?;
        verify_workload_certificate(&certificate, &ca, &bundle.evaluator, now_ms / 1000)?;
        let key_bytes = protected_read(&files.private_key, 64 * 1024)?;
        let signing_key = SigningKey::from_pkcs8_der(&key_bytes)
            .context("evaluator key must be Ed25519 PKCS8")?;
        ensure!(
            workload_public_key(&certificate)? == signing_key.verifying_key().to_bytes(),
            "evaluator SVID/key mismatch"
        );
        let signer = CryptoSigner::from_seed(signing_key.to_bytes());
        for goal in bundle.goals.values() {
            ensure!(
                !goal.search.is_empty()
                    && !goal.held_out.is_empty()
                    && goal.max_search_evaluations > 0,
                "separate nonempty search and held-out sets required"
            );
            for case in goal.search.iter().chain(&goal.held_out) {
                case.load_case()?;
            }
            ensure!(
                goal.need_specs.values().any(|spec| spec.parent_id.is_none()
                    && spec.predicate == json!({"kind":"catalog-goal"})),
                "protected root acceptance predicate missing"
            );
        }
        protected_parent(&files.state)?;
        let state: PrivateState = if files.state.exists() {
            serde_json::from_slice(&protected_read(&files.state, MAX_FRAME_BYTES * 8)?)?
        } else {
            PrivateState::default()
        };
        ensure!(
            state.schema_version == 1,
            "protected evaluator state schema mismatch"
        );
        Ok(Self {
            bundle,
            signer,
            signing_key,
            state,
            state_path: files.state.clone(),
            certificate,
            ca,
            certificate_path: files.certificate.clone(),
        })
    }

    pub async fn evaluate(
        &mut self,
        signed: &Signed<EvaluationRequest>,
        now_ms: u64,
    ) -> Result<Signed<EvaluationResult>> {
        verify_workload_certificate(
            &self.certificate,
            &self.ca,
            &self.bundle.evaluator,
            now_ms / 1000,
        )?;
        signed.verify(REQUEST_DOMAIN, &self.bundle.broker_public_key)?;
        let request = &signed.payload;
        ensure!(
            !request.request_id.is_empty()
                && request.request_id.len() <= 128
                && request
                    .request_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
                && request.issued_ms <= now_ms + 5000
                && request.issued_ms + 60_000 >= now_ms
                && request.deadline_ms > now_ms
                && request.deadline_ms <= request.issued_ms.saturating_add(60_000),
            "evaluation request stale or unbounded"
        );
        let digest = content_hash(request)?;
        if let Some((previous, response)) = self.state.replies.get(&request.request_id) {
            ensure!(*previous == digest, "evaluation request ID conflict");
            return Ok(response.clone());
        }
        ensure!(
            !self.state.pending.contains_key(&request.request_id),
            "evaluation outcome is unknown; final selection cannot be changed"
        );
        let goal = self
            .bundle
            .goals
            .get(&request.goal_id)
            .context("Goal not registered with protected evaluator")?
            .clone();
        self.contract(request, &goal)?;
        ensure!(
            request.finding_ids.len() <= 4096
                && request.finding_ids.iter().collect::<BTreeSet<_>>().len()
                    == request.finding_ids.len(),
            "evaluation evidence selection invalid"
        );
        let mut result = EvaluationResult {
            request_id: request.request_id.clone(),
            goal_id: request.goal_id.clone(),
            evaluator_id: self.bundle.evaluator.uri()?,
            contract: request.contract.clone(),
            state_revision: request.state_revision,
            attempt_id: None,
            candidate_hash: None,
            finding_ids: request.finding_ids.clone(),
            phase: None,
            score: None,
            measurements: BTreeMap::new(),
            verdict: Verdict::Pass,
            commands: vec![],
            protected_set_digest: String::new(),
            evaluation_scope: String::new(),
            measured_ms: now_ms,
        };
        match &request.action {
            EvaluationAction::AcceptNeed { spec } => {
                self.review_need(&request.goal_id, &goal, spec)?;
                let command = DomainCommand::AcceptNeed {
                    goal_id: request.goal_id.clone(),
                    need_id: spec.id.clone(),
                    goal_version: request.contract.goal_version,
                    signature: String::new(),
                };
                result
                    .commands
                    .push(self.sign_command(&request.request_id, 0, command, now_ms)?);
                self.state.accepted.insert(
                    format!(
                        "{}/{}/{}",
                        request.goal_id, request.contract.goal_version, spec.id
                    ),
                    spec.clone(),
                );
                result.protected_set_digest = hash_json(&goal.need_specs)?;
                result.evaluation_scope = "registered-need-contract".into();
            }
            EvaluationAction::Candidate {
                attempt_id,
                candidate_hash,
                candidate,
                need_ids,
                phase,
            } => {
                ensure!(
                    !attempt_id.is_empty()
                        && hash_json(&candidate.attempt_payload()?)? == *candidate_hash,
                    "candidate does not match immutable Attempt hash"
                );
                ensure!(
                    !request.finding_ids.is_empty()
                        && need_ids.iter().collect::<BTreeSet<_>>().len() == need_ids.len()
                        && need_ids
                            .iter()
                            .all(|id| request.contract.need_versions.contains_key(id)),
                    "candidate evidence or Need scope invalid"
                );
                let final_needs = goal
                    .need_specs
                    .iter()
                    .filter(|(_, spec)| spec.predicate == json!({"kind":"catalog-final"}))
                    .map(|(id, _)| id)
                    .collect::<BTreeSet<_>>();
                if *phase == EvaluationPhase::Search {
                    ensure!(
                        need_ids.iter().all(|id| !final_needs.contains(id)),
                        "search cannot satisfy protected final Needs"
                    );
                } else {
                    ensure!(
                        need_ids.iter().all(|id| final_needs.contains(id))
                            && need_ids.len() == final_needs.len(),
                        "final acceptance must cover exactly the protected final Needs"
                    );
                }
                let version_key = format!("{}/{}", request.goal_id, request.contract.goal_version);
                let scoring_key = format!(
                    "{}/{}/{}/{}",
                    request.goal_id,
                    request.contract.goal_version,
                    attempt_id,
                    hash_json(&request.contract)?
                );
                if *phase == EvaluationPhase::Search {
                    ensure!(
                        !self.state.held_out.contains_key(&version_key),
                        "search is closed after held-out evaluation"
                    );
                    ensure!(
                        !self.state.scored.contains(&scoring_key),
                        "immutable score already reserved or issued; use its recorded response"
                    );
                    let prefix = format!("{version_key}/");
                    ensure!(
                        self.state
                            .scored
                            .iter()
                            .filter(|id| id.starts_with(&prefix))
                            .count()
                            < goal.max_search_evaluations,
                        "search stopping rule reached"
                    );
                } else {
                    ensure!(
                        !self.state.held_out.contains_key(&version_key),
                        "final held-out selection already frozen"
                    );
                }
                let cases = if *phase == EvaluationPhase::Search {
                    &goal.search
                } else {
                    &goal.held_out
                };
                if !matches!(candidate, Candidate::Artifact { .. }) {
                    let search_digests: BTreeSet<_> = goal
                        .search
                        .iter()
                        .map(|case| &case.fixture_sha256)
                        .collect();
                    ensure!(
                        goal.held_out
                            .iter()
                            .all(|case| !search_digests.contains(&case.fixture_sha256)),
                        "program generalization requires disjoint held-out inputs"
                    );
                }
                if matches!(candidate, Candidate::Native { .. }) {
                    ensure!(
                        self.bundle.native_policy.is_some(),
                        "native candidate sandbox not admitted"
                    );
                }
                // Freeze selection before running untrusted code. A crash cannot turn final
                // held-out tests into an adaptive feedback oracle or silently retry search.
                self.state
                    .pending
                    .insert(request.request_id.clone(), digest);
                if *phase == EvaluationPhase::Search {
                    self.state.scored.insert(scoring_key.clone());
                } else {
                    self.state
                        .held_out
                        .insert(version_key, candidate_hash.clone());
                }
                self.persist()?;
                let mut passed = 0usize;
                let mut valid = 0usize;
                for (index, case) in cases.iter().enumerate() {
                    ensure!(
                        crate::cluster::now_ms() < request.deadline_ms,
                        "evaluation deadline reached"
                    );
                    let case = case.load_case()?;
                    // Fresh invocation sees only this case's public input, never its corpus or oracle.
                    let output = self
                        .execute_candidate(candidate, &case.public_value(), request, index)
                        .await;
                    if let Ok(artifact) = output {
                        valid += 1;
                        let accepted = if *phase == EvaluationPhase::HeldOut {
                            final_contract_check(&case, &artifact).unwrap_or(false)
                        } else {
                            scenarios::score(&case, &artifact).verdict == Verdict::Pass
                        };
                        if accepted {
                            passed += 1;
                        }
                    }
                }
                let measured_ms = crate::cluster::now_ms();
                ensure!(
                    measured_ms <= request.deadline_ms,
                    "evaluation deadline reached"
                );
                result.measured_ms = measured_ms;
                result.attempt_id = Some(attempt_id.clone());
                result.candidate_hash = Some(candidate_hash.clone());
                result.phase = Some(phase.clone());
                result.verdict = if passed == cases.len() {
                    Verdict::Pass
                } else {
                    Verdict::Fail
                };
                result.measurements = [
                    ("validity".into(), valid as f64 / cases.len() as f64),
                    ("correctness".into(), passed as f64 / cases.len() as f64),
                    (
                        "evidence_quality".into(),
                        passed as f64 / cases.len() as f64,
                    ),
                ]
                .into();
                result.protected_set_digest = hash_json(cases)?;
                result.evaluation_scope = if matches!(candidate, Candidate::Artifact { .. }) {
                    "independent-frozen-artifact-contract"
                } else {
                    "disjoint-input-generalization"
                }
                .into();
                if *phase == EvaluationPhase::Search {
                    let score = Score {
                        candidate_hash: candidate_hash.clone(),
                        contract: request.contract.clone(),
                        value: passed as f64 / cases.len() as f64,
                        dimensions: result.measurements.clone(),
                        evaluator_id: result.evaluator_id.clone(),
                        signature: String::new(),
                        recorded_ms: measured_ms,
                    };
                    ensure!(finite_score(&score), "evaluator emitted invalid score");
                    let command = DomainCommand::ScoreAttempt {
                        goal_id: request.goal_id.clone(),
                        attempt_id: attempt_id.clone(),
                        score: score.clone(),
                    };
                    let signed_command =
                        self.sign_command(&request.request_id, 0, command, measured_ms)?;
                    let mut stored_score = score;
                    stored_score.signature = signed_command.message.signature.clone();
                    result.score = Some(stored_score);
                    result.commands.push(signed_command);
                    for (index, need_id) in need_ids.iter().enumerate() {
                        let command = DomainCommand::EvaluateNeed {
                            goal_id: request.goal_id.clone(),
                            need_id: need_id.clone(),
                            satisfied: result.verdict == Verdict::Pass,
                            finding_ids: request.finding_ids.clone(),
                            contract: request.contract.clone(),
                            signature: String::new(),
                        };
                        result.commands.push(self.sign_command(
                            &request.request_id,
                            index + 1,
                            command,
                            measured_ms,
                        )?);
                    }
                } else {
                    for (index, need_id) in need_ids.iter().enumerate() {
                        let command = DomainCommand::EvaluateNeed {
                            goal_id: request.goal_id.clone(),
                            need_id: need_id.clone(),
                            satisfied: result.verdict == Verdict::Pass,
                            finding_ids: request.finding_ids.clone(),
                            contract: request.contract.clone(),
                            signature: String::new(),
                        };
                        result.commands.push(self.sign_command(
                            &request.request_id,
                            index,
                            command,
                            measured_ms,
                        )?);
                    }
                }
            }
        }
        let signed_result = self.signer.sign(SIGNATURE_DOMAIN, result)?;
        self.state
            .replies
            .insert(request.request_id.clone(), (digest, signed_result.clone()));
        self.state.pending.remove(&request.request_id);
        self.persist()?;
        Ok(signed_result)
    }

    fn contract(&self, request: &EvaluationRequest, goal: &ProtectedGoal) -> Result<()> {
        let expected = &goal.contract;
        ensure!(
            request.contract.goal_version == expected.goal_version
                && request.contract.rubric_version == expected.rubric_version
                && request.contract.evaluation_set_version == expected.evaluation_set_version
                && request.contract.evaluator_version == expected.evaluator_version,
            "evaluation version mismatch"
        );
        ensure!(
            expected.need_versions.iter().all(|(id, version)| request
                .contract
                .need_versions
                .get(id)
                == Some(version)),
            "root Need contract omitted or changed"
        );
        for (id, version) in &request.contract.need_versions {
            let need = goal
                .need_specs
                .get(id)
                .or_else(|| {
                    self.state.accepted.get(&format!(
                        "{}/{}/{}",
                        request.goal_id, request.contract.goal_version, id
                    ))
                })
                .context("unreviewed Need in evaluation contract")?;
            ensure!(need.version == *version, "evaluation Need version mismatch");
        }
        Ok(())
    }
    fn review_need(&self, goal_id: &str, goal: &ProtectedGoal, spec: &NeedSpec) -> Result<()> {
        if let Some(expected) = goal.need_specs.get(&spec.id) {
            ensure!(
                hash_json(spec)? == hash_json(expected)?,
                "registered Need acceptance predicate changed"
            );
            return Ok(());
        }
        let parent_id = spec
            .parent_id
            .as_ref()
            .context("derived Need requires a protected parent")?;
        let parent = goal
            .need_specs
            .get(parent_id)
            .or_else(|| {
                self.state.accepted.get(&format!(
                    "{goal_id}/{}/{parent_id}",
                    goal.contract.goal_version
                ))
            })
            .context("derived Need parent unregistered")?;
        ensure!(
            parent.predicate == json!({"kind":"catalog-goal"})
                && spec.version == 1
                && spec
                    .required_capabilities
                    .is_subset(&parent.required_capabilities)
                && spec
                    .predicate
                    .as_object()
                    .is_some_and(|value| value.len() == 2)
                && spec.predicate["kind"] == "catalog-task",
            "derived Need does not narrow root acceptance contract"
        );
        let task_id = spec.predicate["task_id"]
            .as_str()
            .context("derived Task predicate missing ID")?;
        ensure!(
            goal.search.iter().all(|case| case
                .load_case()
                .is_ok_and(|case| case.tasks.iter().any(|task| task["id"] == task_id))),
            "derived Need refers outside registered public Task scope"
        );
        Ok(())
    }
    fn sign_command(
        &self,
        request_id: &str,
        index: usize,
        command: DomainCommand,
        now_ms: u64,
    ) -> Result<SignedCommand> {
        let mut message = Message {
            request_id: format!(
                "eval-{}-{index}",
                &digest_bytes(request_id.as_bytes())[..24]
            ),
            issued_ms: now_ms,
            path: EVALUATOR_COMMAND_PATH.into(),
            payload: serde_json::to_string(&command)?,
            signature: String::new(),
        };
        message.signature =
            STANDARD.encode(self.signing_key.sign(&message.signing_bytes()?).to_bytes());
        Ok(SignedCommand { command, message })
    }
    async fn execute_candidate(
        &self,
        candidate: &Candidate,
        input: &Value,
        request: &EvaluationRequest,
        index: usize,
    ) -> Result<Value> {
        let encoded = canonical(input)?;
        match candidate {
            Candidate::Artifact { value } => Ok(value.clone()),
            Candidate::Wasi { module, .. } => {
                let mut limits = self.bundle.wasi_limits.clone();
                limits.deadline_ms = limits
                    .deadline_ms
                    .min(request.deadline_ms.saturating_sub(crate::cluster::now_ms()));
                let id = format!(
                    "wasi-eval-{}-{index}",
                    &digest_bytes(request.request_id.as_bytes())[..24]
                );
                let work = self.bundle.native_work_root.join(id);
                let (_cancel, stop) = tokio::sync::watch::channel(false);
                let output = crate::wasi_worker::run_isolated(
                    module,
                    &encoded,
                    &limits,
                    &work,
                    request.deadline_ms,
                    stop,
                )
                .await?;
                Ok(serde_json::from_slice(&output.stdout)
                    .context("candidate output is not JSON")?)
            }
            Candidate::Native {
                language, source, ..
            } => {
                let policy = self
                    .bundle
                    .native_policy
                    .as_ref()
                    .context("native candidate sandbox not admitted")?;
                let id = format!(
                    "eval-{}-{index}",
                    &digest_bytes(request.request_id.as_bytes())[..24]
                );
                let work = self.bundle.native_work_root.join(&id);
                ensure!(
                    self.bundle.native_work_root.is_absolute() && !work.exists(),
                    "native candidate workspace not fresh"
                );
                let argv = match language {
                    NativeLanguage::Python => vec![
                        "/usr/bin/python3".into(),
                        "-I".into(),
                        "-c".into(),
                        source.clone(),
                    ],
                    NativeLanguage::JavaScript => {
                        vec!["/usr/bin/node".into(), "--eval".into(), source.clone()]
                    }
                };
                let spec = NativeToolSpec {
                    run_id: id.clone(),
                    tool_id: id,
                    incarnation: 1,
                    expires_ms: request.deadline_ms,
                    owned_dir: work,
                    limits: self.bundle.native_limits.clone(),
                    argv,
                    input: encoded,
                };
                let plan = match policy {
                    NativePolicy::MicroVm { assets } => sandbox::prepare_microvm(&spec, assets)?,
                    NativePolicy::Gvisor {
                        runsc,
                        rootfs,
                        allow_no_kvm_fallback,
                    } => sandbox::prepare_gvisor(&spec, runsc, rootfs, *allow_no_kvm_fallback)?,
                };
                let output = sandbox::execute_native(&plan).await?;
                Ok(serde_json::from_slice(&output.stdout)
                    .context("candidate output is not JSON")?)
            }
        }
    }
    fn persist(&self) -> Result<()> {
        let bytes = Zeroizing::new(serde_json::to_vec(&self.state)?);
        private_write(&self.state_path, &bytes)
    }
    fn request_csr(
        &mut self,
        request: &Signed<EvaluatorCsrRequest>,
    ) -> Result<Signed<EvaluatorCsrProof>> {
        request.verify(
            "protected-evaluator-csr-request-v1",
            &self.bundle.broker_public_key,
        )?;
        let request = &request.payload;
        let now = crate::cluster::now_ms();
        ensure!(
            !request.request_id.is_empty()
                && request.request_id.len() <= 128
                && (24..=256).contains(&request.nonce.len())
                && request
                    .nonce
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
                && request.issued_ms <= now + 5000
                && request.deadline_ms > now
                && request.deadline_ms <= request.issued_ms.saturating_add(60_000),
            "CSR challenge expired or malformed"
        );
        let hash = content_hash(request)?;
        if let Some(previous) = self.state.renewal.get(&request.request_id) {
            ensure!(
                previous.request_hash == hash,
                "CSR challenge identity conflict"
            );
            return Ok(previous.proof.clone());
        }
        ensure!(
            self.state.renewal.len() < 4096
                && !self
                    .state
                    .renewal
                    .values()
                    .any(|entry| entry.proof.payload.nonce == request.nonce),
            "CSR nonce reused or retained renewal bound reached"
        );
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let document = self.signing_key.to_pkcs8_der()?;
        let der = rustls::pki_types::PrivatePkcs8KeyDer::from(document.as_bytes().to_vec());
        let key = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&der, &rcgen::PKCS_ED25519)?;
        let mut params =
            rcgen::CertificateParams::new(vec![format!("node-{}", self.bundle.evaluator.node)])?;
        params.subject_alt_names.push(rcgen::SanType::URI(
            self.bundle.evaluator.uri()?.try_into()?,
        ));
        let csr = params.serialize_request(&key)?.pem()?;
        let proof = self.signer.sign(
            "protected-evaluator-csr-proof-v1",
            EvaluatorCsrProof {
                request_id: request.request_id.clone(),
                nonce: request.nonce.clone(),
                workload: self.bundle.evaluator.clone(),
                public_key: self.signer.public_key(),
                csr_pem: csr,
                current_certificate_sha256: digest_bytes(&self.certificate),
                issued_ms: now,
            },
        )?;
        self.state.renewal.insert(
            request.request_id.clone(),
            RenewalChallenge {
                request_hash: hash,
                proof: proof.clone(),
                installed_sha256: None,
            },
        );
        self.persist()?;
        Ok(proof)
    }
    fn install_certificate(
        &mut self,
        install: &Signed<EvaluatorCertificateInstall>,
    ) -> Result<Value> {
        install.verify(
            "protected-evaluator-certificate-install-v1",
            &self.bundle.broker_public_key,
        )?;
        let install = &install.payload;
        let now = crate::cluster::now_ms();
        ensure!(
            install.issued_ms <= now + 5000
                && install.deadline_ms > now
                && install.deadline_ms <= install.issued_ms.saturating_add(60_000)
                && install.certificate_pem.len() <= 65536
                && install.ca_pem.len() <= 65536,
            "certificate install expired or oversized"
        );
        let challenge = self
            .state
            .renewal
            .get(&install.request_id)
            .context("certificate install lacks service-owned CSR challenge")?;
        ensure!(
            challenge.proof.payload.nonce == install.nonce,
            "certificate install nonce differs from CSR challenge"
        );
        let parse = |pem: &str| -> Result<Vec<u8>> {
            let mut cursor = std::io::Cursor::new(pem);
            let certs =
                rustls_pemfile::certs(&mut cursor).collect::<std::result::Result<Vec<_>, _>>()?;
            ensure!(
                certs.len() == 1,
                "certificate install requires exactly one certificate"
            );
            Ok(certs[0].to_vec())
        };
        let leaf = parse(&install.certificate_pem)?;
        let ca = parse(&install.ca_pem)?;
        ensure!(
            ca == self.ca,
            "automatic renewal cannot replace the pinned trust CA"
        );
        verify_workload_certificate(&leaf, &ca, &self.bundle.evaluator, now / 1000)?;
        ensure!(
            workload_public_key(&leaf)? == self.signer.public_key(),
            "certificate install rotated independent evaluator key"
        );
        let digest = digest_bytes(&leaf);
        if let Some(previous) = &challenge.installed_sha256 {
            ensure!(
                previous == &digest,
                "immutable renewal challenge installed a different certificate"
            );
            return Ok(json!({"certificate_sha256":digest,"duplicate":true}));
        }
        write_public_certificate(&self.certificate_path, &leaf)?;
        self.certificate = leaf;
        self.state
            .renewal
            .get_mut(&install.request_id)
            .unwrap()
            .installed_sha256 = Some(digest.clone());
        self.persist()?;
        Ok(
            json!({"certificate_sha256":digest,"duplicate":false,"public_key":self.signer.public_key()}),
        )
    }
    async fn deliver(&self, delivery: &Signed<EvaluationDelivery>) -> Result<Value> {
        delivery.verify(
            "protected-evaluator-delivery-v1",
            &self.bundle.broker_public_key,
        )?;
        let delivery = &delivery.payload;
        let now = crate::cluster::now_ms();
        ensure!(
            delivery.issued_ms <= now + 5000
                && delivery.issued_ms + 60_000 >= now
                && delivery.deadline_ms > now,
            "evaluation delivery expired"
        );
        delivery
            .request
            .verify(REQUEST_DOMAIN, &self.bundle.broker_public_key)?;
        let (request_hash, result) = self
            .state
            .replies
            .get(&delivery.request.payload.request_id)
            .context("evaluation has no recorded measured result")?;
        ensure!(
            *request_hash == content_hash(&delivery.request.payload)?
                && hash_json(result)? == delivery.result_digest,
            "delivery changed original request or signed result"
        );
        let target = self
            .bundle
            .delivery
            .as_ref()
            .context("evaluator delivery surface not provisioned")?;
        ensure!(
            target.socket.is_absolute() && target.endpoint.starts_with("https://"),
            "evaluator requires provisioned local mTLS broker endpoint"
        );
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let key = self.signing_key.to_pkcs8_der()?;
        let pem = |label: &str, bytes: &[u8]| {
            format!(
                "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
                STANDARD.encode(bytes)
            )
        };
        let identity = Zeroizing::new(format!(
            "{}{}",
            pem("CERTIFICATE", &self.certificate),
            pem("PRIVATE KEY", key.as_bytes())
        ));
        let client = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .min_tls_version(reqwest::tls::Version::TLS_1_3)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(delivery.deadline_ms - now))
            .unix_socket(target.socket.clone())
            .identity(reqwest::Identity::from_pem(identity.as_bytes())?)
            .add_root_certificate(reqwest::Certificate::from_der(&self.ca)?)
            .build()?;
        let batch = EvaluationBatch {
            request_id: delivery.request.payload.request_id.clone(),
            result: result.clone(),
        };
        let mut message = Message {
            request_id: delivery.request_id.clone(),
            issued_ms: now,
            path: EVALUATOR_COMMAND_PATH.into(),
            payload: serde_json::to_string(&batch)?,
            signature: String::new(),
        };
        message.signature =
            STANDARD.encode(self.signing_key.sign(&message.signing_bytes()?).to_bytes());
        let mut response = client
            .post(format!(
                "{}{}",
                target.endpoint.trim_end_matches('/'),
                EVALUATOR_COMMAND_PATH
            ))
            .json(&message)
            .send()
            .await?;
        let ok = response.status().is_success();
        let mut bytes = vec![];
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= MAX_FRAME_BYTES,
                "evaluator broker response exceeds ceiling"
            );
            bytes.extend_from_slice(&chunk);
        }
        ensure!(ok, "normal evaluator broker rejected measured result");
        Ok(serde_json::from_slice(&bytes)?)
    }
}

fn protected_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("protected evaluator directory missing")?;
    ensure!(
        parent.is_dir() && !fs::symlink_metadata(parent)?.file_type().is_symlink(),
        "protected evaluator directory missing or symlinked"
    );
    #[cfg(unix)]
    {
        #[cfg(target_os = "linux")]
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            fs::metadata(parent)?.permissions().mode() & 0o077 == 0,
            "evaluator directory must be owner-only"
        );
        #[cfg(target_os = "linux")]
        ensure!(
            fs::metadata(parent)?.uid() == process_uid(),
            "evaluator directory belongs to another UID"
        );
    }
    Ok(())
}
fn write_public_certificate(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("public certificate directory missing")?;
    let metadata = fs::symlink_metadata(parent)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "public certificate parent symlink denied"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o022 == 0,
            "public certificate directory must not be writable by group or others"
        );
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == process_uid(),
            "public certificate directory must be evaluator-owned"
        );
    }
    ensure!(
        !path.exists()
            || (fs::symlink_metadata(path)?.is_file()
                && !fs::symlink_metadata(path)?.file_type().is_symlink()),
        "public certificate symlink denied"
    );
    let temporary = path.with_extension(format!("{}-renew", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o640);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn protected_read(path: &Path, maximum: usize) -> Result<Zeroizing<Vec<u8>>> {
    protected_parent(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= maximum as u64,
        "protected evaluator file invalid or oversized"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "evaluator file must be owner-only"
        );
    }
    Ok(Zeroizing::new(fs::read(path)?))
}
fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    protected_parent(path)?;
    ensure!(
        !path.exists() || !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "evaluator state symlink refused"
    );
    let temporary = path.with_extension(format!("{}-tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    fs::File::open(path.parent().context("state parent missing")?)?.sync_all()?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessReply {
    result: Option<Signed<EvaluationResult>>,
    error: Option<String>,
    delivery: Option<Value>,
    #[serde(default)]
    control: Option<Value>,
}
async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    maximum: usize,
) -> Result<Option<Vec<u8>>> {
    let first = match reader.read_u8().await {
        Ok(first) => first,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut prefix = [first, 0, 0, 0];
    reader.read_exact(&mut prefix[1..]).await?;
    let length = u32::from_be_bytes(prefix) as usize;
    ensure!(
        length > 0 && length <= maximum,
        "evaluator frame exceeds configured ceiling"
    );
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(Some(bytes))
}
async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    maximum: usize,
) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= maximum,
        "evaluator frame exceeds configured ceiling"
    );
    writer.write_u32(bytes.len().try_into()?).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}
/// Runs only as the evaluator service CLI. Stdio is inherited private IPC, never an agent endpoint.
pub async fn serve_stdio(files: EvaluatorFiles) -> Result<()> {
    let mut evaluator = ProtectedEvaluator::load(&files, crate::cluster::now_ms())?;
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    serve_channel(&mut evaluator, &mut input, &mut output).await
}
async fn serve_channel<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    evaluator: &mut ProtectedEvaluator,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    while let Some(bytes) = read_frame(input, MAX_FRAME_BYTES).await? {
        if let Ok(envelope) = serde_json::from_slice::<ControlEnvelope>(&bytes) {
            let result = match envelope.control {
                ServiceControl::Csr(request) => evaluator
                    .request_csr(&request)
                    .and_then(|proof| Ok(serde_json::to_value(proof)?)),
                ServiceControl::Install(request) => evaluator.install_certificate(&request),
            };
            let reply = ProcessReply {
                result: None,
                delivery: None,
                error: result
                    .as_ref()
                    .err()
                    .map(|_| "protected service control rejected".into()),
                control: result.ok(),
            };
            write_frame(output, &serde_json::to_vec(&reply)?, MAX_FRAME_BYTES).await?;
            continue;
        }
        if let Ok(envelope) = serde_json::from_slice::<DeliveryEnvelope>(&bytes) {
            let reply = match evaluator.deliver(&envelope.delivery).await {
                Ok(value) => ProcessReply {
                    result: None,
                    error: None,
                    delivery: Some(value),
                    control: None,
                },
                Err(_) => ProcessReply {
                    result: None,
                    error: Some(
                        "normal evaluator delivery rejected or unknown; inspect recorded request"
                            .into(),
                    ),
                    delivery: None,
                    control: None,
                },
            };
            write_frame(output, &serde_json::to_vec(&reply)?, MAX_FRAME_BYTES).await?;
            continue;
        }
        let operation = match serde_json::from_slice::<Signed<EvaluationRequest>>(&bytes) {
            Ok(request) => evaluator.evaluate(&request, crate::cluster::now_ms()).await,
            Err(_) => Err(anyhow::anyhow!("invalid evaluator request")),
        };
        // Do not return parser, expected-data, sandbox stderr or secret file errors to proposers.
        let reply = match operation {
            Ok(result) => ProcessReply {
                result: Some(result),
                error: None,
                delivery: None,
                control: None,
            },
            Err(_) => ProcessReply {
                result: None,
                error: Some(
                    "protected evaluator rejected request; inspect authorized evaluator audit"
                        .into(),
                ),
                delivery: None,
                control: None,
            },
        };
        write_frame(output, &serde_json::to_vec(&reply)?, MAX_FRAME_BYTES).await?;
    }
    Ok(())
}
/// The separately provisioned evaluator service owns this socket and its private files.
#[cfg(target_os = "linux")]
pub async fn serve_socket(files: &EvaluatorFiles, socket: &Path, broker_uid: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    ensure!(
        broker_uid > 0 && socket.is_absolute() && !socket.exists(),
        "evaluator IPC must be a fresh owned socket with nonroot broker UID"
    );
    let parent = socket.parent().context("evaluator IPC parent missing")?;
    let metadata = fs::symlink_metadata(parent)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == process_uid()
            && metadata.permissions().mode() & 0o067 == 0,
        "evaluator IPC directory must be evaluator-owned0710 or0700"
    );
    let listener = tokio::net::UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o660))?;
    let operation = async {
        loop {
            let (stream, _) = listener.accept().await?;
            if stream.peer_cred()?.uid() != broker_uid {
                drop(stream);
                continue;
            }
            let (mut input, mut output) = stream.into_split();
            let mut evaluator = ProtectedEvaluator::load(files, crate::cluster::now_ms())?;
            // One authenticated CP stream at a time avoids concurrent hidden-case dispatch.
            let _ = serve_channel(&mut evaluator, &mut input, &mut output).await;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    #[cfg(unix)]
    let result = {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {result=operation=>result,_=tokio::signal::ctrl_c()=>Ok(()),_=term.recv()=>Ok(())}
    };
    drop(listener);
    fs::remove_file(socket)?;
    result
}
#[cfg(not(target_os = "linux"))]
pub async fn serve_socket(_files: &EvaluatorFiles, _socket: &Path, _broker_uid: u32) -> Result<()> {
    anyhow::bail!(
        "protected evaluator service IPC requires Linux peer credentials and process isolation"
    )
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveryEnvelope {
    delivery: Signed<EvaluationDelivery>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluatorProcessConfig {
    pub executable: PathBuf,
    pub files: EvaluatorFiles,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub max_request_bytes: usize,
    pub timeout_ms: u64,
    #[serde(default)]
    pub ipc_socket: Option<PathBuf>,
}
struct ProcessIo {
    child: Option<Child>,
    input: Box<dyn AsyncWrite + Unpin + Send>,
    output: Box<dyn AsyncRead + Unpin + Send>,
    failed: bool,
}
pub struct EvaluatorProcess {
    io: Mutex<ProcessIo>,
    maximum: usize,
    timeout_ms: u64,
}

#[cfg(target_os = "linux")]
fn checked_user_bus(uid: u32, directory: &Path) -> Result<Option<PathBuf>> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o777 == 0o700,
        "evaluator user runtime directory is not private and owned"
    );
    let bus = directory.join("bus");
    let metadata = match fs::symlink_metadata(&bus) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.file_type().is_socket() && metadata.uid() == uid,
        "evaluator user bus is not an owned Unix socket"
    );
    Ok(Some(bus))
}

impl EvaluatorProcess {
    pub fn spawn(config: EvaluatorProcessConfig) -> Result<Self> {
        #[cfg(unix)]
        if let Some(socket) = &config.ipc_socket {
            ensure!(
                socket.is_absolute()
                    && (1..=MAX_FRAME_BYTES).contains(&config.max_request_bytes)
                    && (1..=65_000).contains(&config.timeout_ms),
                "external evaluator IPC bounds missing"
            );
            let expected_uid = config
                .uid
                .context("external evaluator UID must be pinned")?;
            ensure!(expected_uid > 0, "external evaluator must be nonroot");
            let stream = std::os::unix::net::UnixStream::connect(socket)?;
            stream.set_nonblocking(true)?;
            let stream = tokio::net::UnixStream::from_std(stream)?;
            ensure!(
                stream.peer_cred()?.uid() == expected_uid,
                "external evaluator peer UID mismatch"
            );
            let (output, input) = stream.into_split();
            return Ok(Self {
                io: Mutex::new(ProcessIo {
                    child: None,
                    input: Box::new(input),
                    output: Box::new(output),
                    failed: false,
                }),
                maximum: config.max_request_bytes,
                timeout_ms: config.timeout_ms,
            });
        }
        #[cfg(not(unix))]
        ensure!(
            config.ipc_socket.is_none(),
            "external evaluator Unix IPC unavailable"
        );
        ensure!(
            config.executable.is_absolute()
                && config.executable.is_file()
                && (1..=MAX_FRAME_BYTES).contains(&config.max_request_bytes)
                && (1..=65_000).contains(&config.timeout_ms),
            "evaluator executable or process bounds missing"
        );
        let mut command = Command::new(&config.executable);
        command
            .args(["mvp", "evaluator-serve", "--bundle"])
            .arg(&config.files.bundle)
            .arg("--certificate")
            .arg(&config.files.certificate)
            .arg("--private-key")
            .arg(&config.files.private_key)
            .arg("--ca")
            .arg(&config.files.ca)
            .arg("--state")
            .arg(&config.files.state)
            .env_clear()
            .env("TOKIO_WORKER_THREADS", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(target_os = "linux")]
        {
            // Derive only the intended child UID's local bus; ambient environment stays cleared.
            let uid = config.uid.unwrap_or_else(process_uid);
            let directory = PathBuf::from(format!("/run/user/{uid}"));
            if let Some(bus) = checked_user_bus(uid, &directory)? {
                command.env("XDG_RUNTIME_DIR", directory).env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={}", bus.display()),
                );
            }
        }
        #[cfg(unix)]
        {
            if let Some(gid) = config.gid {
                ensure!(gid > 0, "evaluator GID must be nonroot");
                command.gid(gid);
            }
            if let Some(uid) = config.uid {
                ensure!(uid > 0, "evaluator UID must be nonroot");
                command.uid(uid);
            }
        }
        #[cfg(not(unix))]
        ensure!(
            config.uid.is_none() && config.gid.is_none(),
            "evaluator UID isolation unavailable on this platform"
        );
        let mut child = command.spawn()?;
        let input = child.stdin.take().context("evaluator input missing")?;
        let output = child.stdout.take().context("evaluator output missing")?;
        Ok(Self {
            io: Mutex::new(ProcessIo {
                child: Some(child),
                input: Box::new(input),
                output: Box::new(output),
                failed: false,
            }),
            maximum: config.max_request_bytes,
            timeout_ms: config.timeout_ms,
        })
    }
    pub async fn request(
        &self,
        request: &Signed<EvaluationRequest>,
    ) -> Result<Signed<EvaluationResult>> {
        let bytes = canonical(request)?;
        ensure!(bytes.len() <= self.maximum, "evaluation request too large");
        let mut io = self.io.lock().await;
        ensure!(
            !io.failed && child_running(&mut io)?,
            "evaluator process stopped; reconcile recorded request before restart"
        );
        let operation = async {
            write_frame(&mut io.input, &bytes, self.maximum).await?;
            let reply = read_frame(&mut io.output, self.maximum)
                .await?
                .context("evaluator process closed its channel")?;
            let reply: ProcessReply = serde_json::from_slice(&reply)?;
            reply.result.context("protected evaluator rejected request")
        };
        match tokio::time::timeout(Duration::from_millis(self.timeout_ms), operation).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                io.failed = true;
                if let Some(child) = &mut io.child {
                    let _ = child.kill().await;
                }
                anyhow::bail!(
                    "evaluator deadline reached; request outcome must be reconciled before retry"
                )
            }
        }
    }
    pub async fn deliver(&self, delivery: Signed<EvaluationDelivery>) -> Result<Value> {
        let bytes = canonical(&DeliveryEnvelope { delivery })?;
        let mut io = self.io.lock().await;
        ensure!(
            !io.failed && child_running(&mut io)?,
            "evaluator process stopped"
        );
        let operation = async {
            write_frame(&mut io.input, &bytes, self.maximum).await?;
            let bytes = read_frame(&mut io.output, self.maximum)
                .await?
                .context("evaluator delivery channel closed")?;
            let reply: ProcessReply = serde_json::from_slice(&bytes)?;
            reply
                .delivery
                .context("normal evaluator delivery rejected or unknown")
        };
        match tokio::time::timeout(Duration::from_millis(self.timeout_ms), operation).await {
            Ok(result) => result,
            Err(_) => {
                io.failed = true;
                if let Some(child) = &mut io.child {
                    let _ = child.kill().await;
                }
                anyhow::bail!("evaluator delivery timeout; reconcile broker request before retry")
            }
        }
    }
    async fn control(&self, control: ServiceControl) -> Result<Value> {
        let bytes = canonical(&ControlEnvelope { control })?;
        let mut io = self.io.lock().await;
        ensure!(
            !io.failed && child_running(&mut io)?,
            "evaluator process stopped"
        );
        let operation = async {
            write_frame(&mut io.input, &bytes, self.maximum).await?;
            let bytes = read_frame(&mut io.output, self.maximum)
                .await?
                .context("service control channel closed")?;
            let reply: ProcessReply = serde_json::from_slice(&bytes)?;
            reply.control.context("protected service control rejected")
        };
        match tokio::time::timeout(Duration::from_millis(self.timeout_ms), operation).await {
            Ok(result) => result,
            Err(_) => {
                io.failed = true;
                if let Some(child) = &mut io.child {
                    let _ = child.kill().await;
                }
                anyhow::bail!("service control timeout; reconcile request before reconnect")
            }
        }
    }
    pub async fn request_csr(
        &self,
        request: &Signed<EvaluatorCsrRequest>,
    ) -> Result<Signed<EvaluatorCsrProof>> {
        Ok(serde_json::from_value(
            self.control(ServiceControl::Csr(request.clone())).await?,
        )?)
    }
    pub async fn install_certificate(
        &self,
        request: &Signed<EvaluatorCertificateInstall>,
    ) -> Result<Value> {
        self.control(ServiceControl::Install(request.clone())).await
    }
    pub async fn stop(self) -> Result<()> {
        let ProcessIo {
            child,
            mut input,
            output,
            ..
        } = self.io.into_inner();
        input.shutdown().await?;
        drop(input);
        drop(output);
        let Some(mut child) = child else {
            return Ok(());
        };
        match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
            Ok(status) => {
                ensure!(status?.success(), "evaluator exited unsuccessfully");
                Ok(())
            }
            Err(_) => {
                child.kill().await?;
                child.wait().await?;
                anyhow::bail!("evaluator required forced cleanup")
            }
        }
    }
}
fn child_running(io: &mut ProcessIo) -> Result<bool> {
    Ok(match &mut io.child {
        Some(child) => child.try_wait()?.is_none(),
        None => true,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod user_bus_tests {
    use super::*;
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};

    #[test]
    fn launch_bus_context_requires_private_directory_and_owned_socket() {
        let root = std::env::temp_dir().join(format!("solos-user-bus-{}", uuid::Uuid::new_v4()));
        assert!(checked_user_bus(process_uid(), &root).unwrap().is_none());
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(checked_user_bus(process_uid(), &root).unwrap().is_none());
        fs::write(root.join("bus"), b"regular file is not a bus").unwrap();
        assert!(checked_user_bus(process_uid(), &root).is_err());
        fs::remove_file(root.join("bus")).unwrap();
        let listener = UnixListener::bind(root.join("bus")).unwrap();
        assert_eq!(
            checked_user_bus(process_uid(), &root).unwrap(),
            Some(root.join("bus"))
        );
        assert!(checked_user_bus(process_uid().wrapping_add(1), &root).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(checked_user_bus(process_uid(), &root).is_err());
        drop(listener);
        fs::remove_dir_all(root).unwrap();
    }
}
