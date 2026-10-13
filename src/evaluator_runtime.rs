//! Durable independent evaluator supervision. Results return through the normal mTLS broker.
use crate::{
    artifacts::{ArtifactStore, ReadPermit},
    audit::{Signed, content_hash},
    domain::{Attempt, hash_json},
    evaluator::{
        Candidate, DeliveryTarget, EvaluationAction, EvaluationDelivery, EvaluationPhase,
        EvaluationRequest, EvaluationResult, EvaluatorProcess, EvaluatorProcessConfig,
        NativeLanguage, verify_current,
    },
    kernel::{Kernel, KernelState, Reply},
    node::Session,
    runtime::IdentityFiles,
    security::{AuthenticatedSubject, CapabilityKind, Scope},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::watch;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateFormat {
    Artifact,
    PreparedProgram,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateStore {
    pub directory: PathBuf,
    pub key_file: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluatorSupervisorConfig {
    pub process: EvaluatorProcessConfig,
    pub delivery: DeliveryTarget,
    /// This session and its allocation lease must already be admitted by the operator.
    pub session: Session,
    /// Existing trusted runtime identity; it records dispatch observations, never scores.
    pub runtime_identity: IdentityFiles,
    pub interval_ms: u64,
    pub request_timeout_ms: u64,
    pub candidate_format: CandidateFormat,
    pub candidate_store: Option<CandidateStore>,
    pub max_jobs_per_goal: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationJob {
    pub key: String,
    pub request: Signed<EvaluationRequest>,
    pub status: String,
    pub result: Option<Signed<EvaluationResult>>,
    pub observation: Option<String>,
    pub updated_ms: u64,
}

impl Kernel {
    async fn queue_evaluation(
        &self,
        subject: &AuthenticatedSubject,
        job: EvaluationJob,
        max_jobs: usize,
    ) -> Result<Reply> {
        let id = format!("queue-{}", job.request.payload.request_id);
        self.runtime_commit(&id, subject, &job, |state| {
            job.request
                .verify("protected-evaluation-request-v1", &self.signer.public_key())?;
            let request = &job.request.payload;
            ensure!(
                job.status == "queued" && job.result.is_none() && job.observation.is_none(),
                "new evaluation job must be unexecuted"
            );
            ensure!(
                state.domain.current_contract(&request.goal_id)? == request.contract,
                "evaluation job has stale contract"
            );
            let goal = state.domain.goal(&request.goal_id)?;
            ensure!(
                matches!(goal.status.as_str(), "active" | "awaiting_evaluation")
                    && goal.spec.expiry_ms > crate::cluster::now_ms(),
                "evaluation Goal stopped or expired"
            );
            ensure!(
                state
                    .evaluation_jobs
                    .values()
                    .filter(|j| j.request.payload.goal_id == request.goal_id
                        && j.request.payload.contract.goal_version == request.contract.goal_version)
                    .count()
                    < max_jobs,
                "evaluation job bound reached"
            );
            match &request.action {
                EvaluationAction::Candidate {
                    attempt_id,
                    candidate_hash,
                    candidate,
                    ..
                } => {
                    let attempt = state.domain.attempt(&request.goal_id, attempt_id)?;
                    ensure!(
                        state.domain.attempt_eligible(attempt)
                            && attempt.candidate_hash == *candidate_hash
                            && attempt.finding_ids == request.finding_ids
                            && hash_json(&candidate.attempt_payload()?)? == *candidate_hash,
                        "evaluation job changed candidate or provenance"
                    );
                }
                EvaluationAction::AcceptNeed { spec } => ensure!(
                    hash_json(&state.domain.need(&request.goal_id, &spec.id)?.spec)?
                        == hash_json(spec)?,
                    "evaluation job changed proposed Need"
                ),
            }
            if let Some(previous) = state.evaluation_jobs.get(&job.key) {
                ensure!(
                    content_hash(&previous.request)? == content_hash(&job.request)?,
                    "evaluation job immutable selection conflict"
                );
                return Ok(serde_json::to_value(previous)?);
            }
            state.evaluation_jobs.insert(job.key.clone(), job.clone());
            Ok(json!({"job":job.key,"status":"queued"}))
        })
        .await
    }
    async fn evaluation_observation(
        &self,
        subject: &AuthenticatedSubject,
        key: &str,
        status: &str,
        result: Option<Signed<EvaluationResult>>,
        certificate: &[u8],
        ca: &[u8],
    ) -> Result<Reply> {
        let id = format!("eval-observation-{}-{}", &hash_json(&key)?[..24], status);
        self.runtime_commit(&id, subject, &(key, status, &result), |state| {
            let job = state
                .evaluation_jobs
                .get(key)
                .context("evaluation job absent")?
                .clone();
            let permitted = match status {
                "dispatching" => job.status == "queued",
                "measured" => job.status == "dispatching" && result.is_some(),
                "applied" => job.status == "measured",
                "unknown" => matches!(job.status.as_str(), "dispatching" | "measured"),
                "stale" => matches!(job.status.as_str(), "queued" | "dispatching" | "measured"),
                _ => false,
            };
            ensure!(permitted, "evaluation observation transition invalid");
            if let Some(measured) = &result {
                verify_current(
                    measured,
                    &job.request,
                    &state.domain,
                    certificate,
                    ca,
                    crate::cluster::now_ms(),
                )?;
                ensure!(
                    job.result.as_ref().is_none_or(
                        |previous| content_hash(previous).ok() == content_hash(measured).ok()
                    ),
                    "immutable evaluation result changed"
                );
            }
            if status == "applied" {
                let measured = job
                    .result
                    .as_ref()
                    .context("applied observation lacks measured result")?;
                ensure!(
                    result_applied(state, &job.request, measured)?,
                    "normal broker has not committed evaluator result"
                );
            }
            let stored = state.evaluation_jobs.get_mut(key).unwrap();
            stored.status = status.into();
            stored.updated_ms = crate::cluster::now_ms();
            if let Some(result) = &result {
                stored.result = Some(result.clone());
            }
            stored.observation = Some(
                match status {
                    "unknown" => "dispatch-or-delivery-outcome-requires-reconciliation",
                    "stale" => "current-contract-or-lineage-changed",
                    _ => "normal-runtime-observation",
                }
                .into(),
            );
            Ok(json!({"job":key,"status":status}))
        })
        .await
    }
}

fn result_applied(
    state: &KernelState,
    request: &Signed<EvaluationRequest>,
    result: &Signed<EvaluationResult>,
) -> Result<bool> {
    let payload = &request.payload;
    Ok(match &payload.action {
        EvaluationAction::AcceptNeed { spec } => {
            state.domain.need(&payload.goal_id, &spec.id)?.accepted
        }
        EvaluationAction::Candidate {
            attempt_id, phase, ..
        } => match phase {
            EvaluationPhase::Search => state
                .domain
                .attempt(&payload.goal_id, attempt_id)?
                .scores
                .iter()
                .any(|score| {
                    result.payload.score.as_ref().is_some_and(|expected| {
                        score.candidate_hash == expected.candidate_hash
                            && score.contract == expected.contract
                            && score.value == expected.value
                    })
                }),
            EvaluationPhase::HeldOut => state
                .final_evaluations
                .get(&format!(
                    "{}/{}",
                    payload.goal_id, payload.contract.goal_version
                ))
                .is_some_and(|record| content_hash(record).ok() == content_hash(result).ok()),
        },
    })
}

fn runtime_subject(config: &EvaluatorSupervisorConfig) -> Result<AuthenticatedSubject> {
    let identity = &config.runtime_identity;
    AuthenticatedSubject::workload(
        &fs::read(&identity.certificate_der)?,
        &fs::read(&identity.ca_der)?,
        identity.workload.clone(),
        crate::cluster::now_ms() / 1000,
    )
}

/// Expected authority withdrawal ends the service without inventing an execution failure.
pub(crate) fn evaluator_scope_active(state: &KernelState, session: &Session, now_ms: u64) -> bool {
    let Some(goal) = state.domain.goals.get(&session.workload.goal) else {
        return false;
    };
    let Ok(grant) = state.security.live_grant(&session.grant_id, now_ms / 1000) else {
        return false;
    };
    matches!(goal.status.as_str(), "active" | "awaiting_evaluation")
        && goal.spec.expiry_ms > now_ms
        && goal.spec.evaluator_id == session.workload.uri().unwrap_or_default()
        && grant.holder == session.workload
        && grant.version == goal.spec.version
        && !grant.revoked
        && grant.lease.not_before.saturating_mul(1000) <= now_ms
        && grant.lease.expiry.saturating_mul(1000) > now_ms
        && grant.lease.fencing_token == session.allocation_fence
        && state
            .security
            .policy
            .payload
            .trusted_components
            .contains(&session.workload.uri().unwrap_or_default())
        && grant.capabilities.iter().any(|cap| {
            cap.kind == CapabilityKind::ToolCall
                && cap
                    .resource
                    .matches(&format!("evaluate/{}", session.workload.goal))
                && cap.constraints.operations.contains("evaluate")
                && cap.not_before.saturating_mul(1000) <= now_ms
                && cap.expiry.saturating_mul(1000) > now_ms
        })
        && !state.security.stopped(&Scope::Agent {
            tenant: session.workload.tenant.clone(),
            goal: session.workload.goal.clone(),
            holder: session.workload.uri().unwrap_or_default(),
        })
}

fn evaluator_clearance(
    state: &KernelState,
    config: &EvaluatorSupervisorConfig,
) -> Result<BTreeSet<String>> {
    let session = &config.session;
    let grant = state
        .security
        .live_grant(&session.grant_id, crate::cluster::now_ms() / 1000)?;
    ensure!(
        grant.holder == session.workload
            && !grant.revoked
            && grant.lease.expiry > crate::cluster::now_ms() / 1000
            && grant.lease.fencing_token == session.allocation_fence,
        "evaluator service grant expired or revoked"
    );
    ensure!(
        state
            .security
            .policy
            .payload
            .trusted_components
            .contains(&session.workload.uri()?),
        "evaluator service is not a registered trusted component"
    );
    ensure!(
        !state.security.stopped(&Scope::Agent {
            tenant: session.workload.tenant.clone(),
            goal: session.workload.goal.clone(),
            holder: session.workload.uri()?
        }),
        "evaluator service stopped"
    );
    let capability = grant
        .capabilities
        .iter()
        .find(|cap| {
            cap.kind == CapabilityKind::ToolCall
                && cap
                    .resource
                    .matches(&format!("evaluate/{}", session.workload.goal))
                && cap.constraints.operations.contains("evaluate")
                && cap.not_before <= crate::cluster::now_ms() / 1000
                && cap.expiry > crate::cluster::now_ms() / 1000
        })
        .context("evaluator service lacks compiled evaluate capability")?;
    Ok(capability.constraints.read_markings.clone())
}

/// Bound protected reads by current admitted authority, never an initial Session cache.
fn evaluator_read_horizon(
    state: &KernelState,
    session: &Session,
    live_expiry_ms: u64,
) -> Result<u64> {
    let grant = state
        .security
        .live_grant(&session.grant_id, crate::cluster::now_ms() / 1000)?;
    ensure!(
        grant.holder == session.workload
            && !grant.revoked
            && grant.lease.fencing_token == session.allocation_fence,
        "evaluator read authority differs from current grant"
    );
    let goal = state.domain.goal(&session.workload.goal)?;
    ensure!(
        grant.goal_id == goal.spec.goal_id && grant.version == goal.spec.version,
        "evaluator read Goal version changed"
    );
    let cap = grant
        .capabilities
        .iter()
        .find(|cap| {
            cap.kind == CapabilityKind::ToolCall
                && cap
                    .resource
                    .matches(&format!("evaluate/{}", session.workload.goal))
                && cap.constraints.operations.contains("evaluate")
                && cap.not_before <= crate::cluster::now_ms() / 1000
                && cap.expiry > crate::cluster::now_ms() / 1000
        })
        .context("evaluator read capability expired or absent")?;
    Ok(goal
        .spec
        .expiry_ms
        .min(grant.lease.expiry.saturating_mul(1000))
        .min(cap.expiry.saturating_mul(1000))
        .min(live_expiry_ms))
}

fn candidate(
    config: &EvaluatorSupervisorConfig,
    attempt: &Attempt,
    clearance: &BTreeSet<String>,
    authority_expiry_ms: u64,
) -> Result<Candidate> {
    ensure!(
        attempt.markings.is_subset(clearance),
        "candidate outside evaluator read clearance"
    );
    let result = match config.candidate_format {
        CandidateFormat::Artifact => Candidate::Artifact {
            value: attempt.payload.clone(),
        },
        CandidateFormat::PreparedProgram => {
            let spec = config
                .candidate_store
                .as_ref()
                .context("prepared program store not configured")?;
            let reference = attempt.payload["artifact_ref"]
                .as_str()
                .context("program requires immutable encrypted artifact reference")?;
            let digest = attempt.payload["sha256"]
                .as_str()
                .context("program content digest missing")?
                .to_owned();
            let permit = ReadPermit {
                holder_id: config.session.workload.uri()?,
                goal_ids: [attempt.goal_id.clone()].into(),
                classifications: clearance.clone(),
                expiry_ms: authority_expiry_ms,
                allow_retracted: false,
            };
            let store = ArtifactStore::open(&spec.directory, &spec.key_file)?;
            let bytes = store.get(reference, &permit, crate::cluster::now_ms())?;
            match attempt.payload["kind"].as_str() {
                Some("wasi") => Candidate::Wasi {
                    module: bytes.to_vec(),
                    sha256: digest,
                    artifact_ref: Some(reference.into()),
                },
                Some("native") => {
                    let language: NativeLanguage =
                        serde_json::from_value(attempt.payload["language"].clone())?;
                    Candidate::Native {
                        language,
                        source: String::from_utf8(bytes.to_vec())?,
                        sha256: digest,
                        artifact_ref: Some(reference.into()),
                    }
                }
                _ => anyhow::bail!("unsupported prepared program kind"),
            }
        }
    };
    ensure!(
        hash_json(&result.attempt_payload()?)? == attempt.candidate_hash,
        "prepared candidate differs from immutable Attempt"
    );
    Ok(result)
}

fn selection(
    config: &EvaluatorSupervisorConfig,
    state: &KernelState,
    clearance: &BTreeSet<String>,
    now: u64,
    authority_expiry_ms: u64,
) -> Result<Option<(String, EvaluationRequest)>> {
    let goal_id = &config.session.workload.goal;
    let goal = state.domain.goal(goal_id)?;
    if !matches!(goal.status.as_str(), "active" | "awaiting_evaluation")
        || goal.spec.expiry_ms <= now
    {
        return Ok(None);
    }
    ensure!(
        goal.spec.evaluator_id == config.session.workload.uri()?,
        "supervisor differs from Goal registered evaluator"
    );
    let contract = state.domain.current_contract(goal_id)?;
    let prefix = format!("{goal_id}/{}", goal.spec.version);
    let request =
        |action, finding_ids, key: String| -> Result<Option<(String, EvaluationRequest)>> {
            if state.evaluation_jobs.contains_key(&key) {
                return Ok(None);
            }
            let request_id = format!("evaluate-{}", &hash_json(&key)?[..32]);
            Ok(Some((
                key,
                EvaluationRequest {
                    request_id,
                    goal_id: goal_id.clone(),
                    contract: contract.clone(),
                    finding_ids,
                    state_revision: state.domain.revision,
                    issued_ms: now,
                    deadline_ms: now
                        .saturating_add(config.request_timeout_ms)
                        .min(goal.spec.expiry_ms),
                    action,
                },
            )))
        };
    for need in state
        .domain
        .needs
        .values()
        .filter(|n| n.goal_id == *goal_id && !n.accepted)
    {
        if let Some(selected) = request(
            EvaluationAction::AcceptNeed {
                spec: need.spec.clone(),
            },
            vec![],
            format!("{prefix}/need/{}/{}", need.spec.id, need.spec.version),
        )? {
            return Ok(Some(selected));
        }
    }
    let final_needs: Vec<_> = state
        .domain
        .needs
        .values()
        .filter(|n| {
            n.goal_id == *goal_id
                && n.accepted
                && n.spec.predicate == json!({"kind":"catalog-final"})
        })
        .map(|n| n.spec.id.clone())
        .collect();
    let final_key = format!("{prefix}/held-out");
    if state.evaluation_jobs.contains_key(&final_key) {
        return Ok(None);
    }
    let search_needs: Vec<_> = state
        .domain
        .needs
        .values()
        .filter(|n| {
            n.goal_id == *goal_id
                && n.accepted
                && n.spec.predicate != json!({"kind":"catalog-final"})
        })
        .map(|n| n.spec.id.clone())
        .collect();
    for attempt in state.domain.attempts.values().filter(|a| {
        a.goal_id == *goal_id && state.domain.attempt_eligible(a) && a.markings.is_subset(clearance)
    }) {
        if attempt.scores.iter().any(|s| s.contract == contract) {
            continue;
        }
        let key = format!("{prefix}/search/{}/{}", attempt.id, hash_json(&contract)?);
        if state.evaluation_jobs.contains_key(&key) {
            continue;
        }
        let value = candidate(config, attempt, clearance, authority_expiry_ms)?;
        return request(
            EvaluationAction::Candidate {
                attempt_id: attempt.id.clone(),
                candidate_hash: attempt.candidate_hash.clone(),
                candidate: value,
                need_ids: search_needs.clone(),
                phase: EvaluationPhase::Search,
            },
            attempt.finding_ids.clone(),
            key,
        );
    }
    let search_complete = state
        .domain
        .needs
        .values()
        .filter(|n| {
            n.goal_id == *goal_id
                && n.spec.required
                && n.spec.predicate != json!({"kind":"catalog-final"})
        })
        .all(|n| n.accepted && n.satisfied);
    if search_complete && !final_needs.is_empty() {
        let selected = state
            .domain
            .attempts
            .values()
            .filter(|a| {
                a.goal_id == *goal_id
                    && state.domain.attempt_eligible(a)
                    && a.markings.is_subset(clearance)
            })
            .filter_map(|attempt| {
                attempt
                    .scores
                    .iter()
                    .find(|s| s.contract == contract)
                    .map(|score| (attempt, score.value))
            })
            .max_by(|(a, x), (b, y)| {
                x.total_cmp(y)
                    .then_with(|| b.recorded_ms.cmp(&a.recorded_ms))
                    .then_with(|| b.id.cmp(&a.id))
            });
        if let Some((attempt, _)) = selected {
            let value = candidate(config, attempt, clearance, authority_expiry_ms)?;
            return request(
                EvaluationAction::Candidate {
                    attempt_id: attempt.id.clone(),
                    candidate_hash: attempt.candidate_hash.clone(),
                    candidate: value,
                    need_ids: final_needs,
                    phase: EvaluationPhase::HeldOut,
                },
                attempt.finding_ids.clone(),
                final_key,
            );
        }
    }
    Ok(None)
}

/// A restarted dispatching job is explicitly unknown. This supervisor never reruns a secret
/// held-out case, changes final selection, or silently mints a fresh adaptive evaluation ID.
pub async fn serve(
    config: EvaluatorSupervisorConfig,
    kernel: Arc<Kernel>,
    stop: watch::Receiver<bool>,
) -> Result<()> {
    serve_inner(config, kernel, stop, None).await
}
pub async fn serve_with_authority(
    config: EvaluatorSupervisorConfig,
    kernel: Arc<Kernel>,
    stop: watch::Receiver<bool>,
    authority: Arc<crate::identity::IdentityAuthority>,
) -> Result<()> {
    serve_inner(config, kernel, stop, Some(authority)).await
}
async fn serve_inner(
    config: EvaluatorSupervisorConfig,
    kernel: Arc<Kernel>,
    mut stop: watch::Receiver<bool>,
    authority: Option<Arc<crate::identity::IdentityAuthority>>,
) -> Result<()> {
    ensure!(
        (10..=60_000).contains(&config.interval_ms)
            && (1..=60_000).contains(&config.request_timeout_ms)
            && config.max_jobs_per_goal > 0
            && config.max_jobs_per_goal <= 65_536,
        "evaluator supervisor bounds invalid"
    );
    let certificate = fs::read(&config.process.files.certificate)?;
    let ca = fs::read(&config.process.files.ca)?;
    crate::identity::verify_workload_certificate(
        &certificate,
        &ca,
        &config.session.workload,
        crate::cluster::now_ms() / 1000,
    )?;
    let process = EvaluatorProcess::spawn(config.process.clone())?;
    let run = async {
        let initial = kernel.state().await?;
        let subject = runtime_subject(&config)?;
        for job in initial.evaluation_jobs.values().filter(|job| {
            job.request.payload.goal_id == config.session.workload.goal
                && job.status == "dispatching"
        }) {
            kernel
                .evaluation_observation(&subject, &job.key, "unknown", None, &certificate, &ca)
                .await?
                .require()?;
        }
        loop {
            if *stop.borrow() {
                break;
            }
            let state = kernel.state().await?;
            if !evaluator_scope_active(&state, &config.session, crate::cluster::now_ms()) {
                break;
            }
            let clearance = evaluator_clearance(&state, &config)?;
            let view = kernel.cluster.view().await?;
            let Some(lease) = view
                .leases
                .get(&format!("allocation/{}", config.session.workload.agent))
            else {
                break;
            };
            if !(!lease.revoked
                && lease.holder == config.session.workload.node
                && lease.fence == config.session.allocation_fence
                && lease.expires_ms > crate::cluster::now_ms() + 1000)
            {
                break;
            }
            let authority_expiry_ms =
                evaluator_read_horizon(&state, &config.session, lease.expires_ms)?;
            let mut certificate = fs::read(&config.process.files.certificate)?;
            let ca = fs::read(&config.process.files.ca)?;
            if let Some(authority) = &authority {
                let (_, parsed) = x509_parser::parse_x509_certificate(&certificate)
                    .map_err(|_| anyhow::anyhow!("evaluator public leaf invalid"))?;
                let now = crate::cluster::now_ms() / 1000;
                let expiry: u64 = parsed.validity().not_after.timestamp().try_into()?;
                let lifetime = (parsed.validity().not_after.timestamp()
                    - parsed.validity().not_before.timestamp())
                .max(1) as u64;
                let goal = state.domain.goal(&config.session.workload.goal)?;
                let grant = &state.security.grants[&config.session.grant_id];
                let authority_end = (goal.spec.expiry_ms / 1000)
                    .min(grant.lease.expiry)
                    .min(lease.expires_ms / 1000);
                if expiry.saturating_sub(now) <= lifetime / 3
                    && authority_end > expiry.saturating_add(5)
                {
                    let request_id = format!("renew-{}", uuid::Uuid::new_v4());
                    let nonce = format!(
                        "{}{}",
                        uuid::Uuid::new_v4().simple(),
                        uuid::Uuid::new_v4().simple()
                    );
                    let now_ms = crate::cluster::now_ms();
                    let request = crate::evaluator::EvaluatorCsrRequest {
                        request_id: request_id.clone(),
                        nonce: nonce.clone(),
                        issued_ms: now_ms,
                        deadline_ms: now_ms + 60_000,
                    }
                    .sign(&kernel.signer)?;
                    let proof = process.request_csr(&request).await?;
                    let pin =
                        state.security.goals[&config.session.workload.goal].evaluator_public_key;
                    proof.verify("protected-evaluator-csr-proof-v1", &pin)?;
                    ensure!(
                        proof.payload.request_id == request_id
                            && proof.payload.nonce == nonce
                            && proof.payload.workload == config.session.workload
                            && proof.payload.public_key == pin
                            && proof.payload.current_certificate_sha256
                                == crate::kernel::hex(&crate::audit::hash(&certificate)),
                        "evaluator CSR proof changed challenge, key or identity"
                    );
                    let public = authority.issue_csr(
                        &proof.payload.csr_pem,
                        config.session.workload.clone(),
                        &pin,
                        now,
                        (authority_end - now).min(3600),
                    )?;
                    let now_ms = crate::cluster::now_ms();
                    let install = crate::evaluator::EvaluatorCertificateInstall {
                        request_id,
                        nonce,
                        certificate_pem: public.certificate_pem,
                        ca_pem: public.ca_pem,
                        issued_ms: now_ms,
                        deadline_ms: now_ms + 60_000,
                    }
                    .sign(&kernel.signer)?;
                    process.install_certificate(&install).await?;
                    certificate = fs::read(&config.process.files.certificate)?;
                    ensure!(
                        crate::identity::workload_public_key(&certificate)? == pin,
                        "renewed evaluator leaf lost approved key"
                    );
                    let subject = runtime_subject(&config)?;
                    kernel
                        .register_workload_certificate(
                            &format!(
                                "evaluator-renewed-{}",
                                crate::kernel::hex(&crate::audit::hash(&certificate))
                            ),
                            &subject,
                            &config.session.grant_id,
                            &certificate,
                            &ca,
                            now,
                        )
                        .await?
                        .require()?;
                }
            }
            crate::identity::verify_workload_certificate(
                &certificate,
                &ca,
                &config.session.workload,
                crate::cluster::now_ms() / 1000,
            )?;
            let pending = state
                .evaluation_jobs
                .values()
                .find(|job| {
                    job.request.payload.goal_id == config.session.workload.goal
                        && matches!(job.status.as_str(), "queued" | "measured")
                })
                .cloned();
            if let Some(job) = pending {
                process_job(&config, &kernel, &process, job, &certificate, &ca).await?;
                continue;
            }
            if let Some((key, request)) = selection(
                &config,
                &state,
                &clearance,
                crate::cluster::now_ms(),
                authority_expiry_ms,
            )? {
                let subject = runtime_subject(&config)?;
                let request = request.sign(&kernel.signer)?;
                kernel
                    .queue_evaluation(
                        &subject,
                        EvaluationJob {
                            key,
                            request,
                            status: "queued".into(),
                            result: None,
                            observation: None,
                            updated_ms: crate::cluster::now_ms(),
                        },
                        config.max_jobs_per_goal,
                    )
                    .await?
                    .require()?;
                continue;
            }
            tokio::select! {
                _=tokio::time::sleep(Duration::from_millis(config.interval_ms))=>{},
                changed=stop.changed()=>{if changed.is_err() || *stop.borrow() {break;}}
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let cleanup = process.stop().await;
    run.and(cleanup)
}

async fn process_job(
    config: &EvaluatorSupervisorConfig,
    kernel: &Kernel,
    process: &EvaluatorProcess,
    mut job: EvaluationJob,
    certificate: &[u8],
    ca: &[u8],
) -> Result<()> {
    let now = crate::cluster::now_ms();
    let state = kernel.state().await?;
    if state
        .domain
        .current_contract(&job.request.payload.goal_id)?
        != job.request.payload.contract
        || job.request.payload.deadline_ms <= now
    {
        kernel
            .evaluation_observation(
                &runtime_subject(config)?,
                &job.key,
                "stale",
                None,
                certificate,
                ca,
            )
            .await?
            .require()?;
        return Ok(());
    }
    if job.status == "queued" {
        kernel
            .evaluation_observation(
                &runtime_subject(config)?,
                &job.key,
                "dispatching",
                None,
                certificate,
                ca,
            )
            .await?
            .require()?;
        match process.request(&job.request).await {
            Ok(result) => {
                let recorded = kernel
                    .evaluation_observation(
                        &runtime_subject(config)?,
                        &job.key,
                        "measured",
                        Some(result.clone()),
                        certificate,
                        ca,
                    )
                    .await?;
                if !recorded.ok {
                    kernel
                        .evaluation_observation(
                            &runtime_subject(config)?,
                            &job.key,
                            "stale",
                            None,
                            certificate,
                            ca,
                        )
                        .await?
                        .require()?;
                    return Ok(());
                }
                job.result = Some(result);
            }
            Err(_) => {
                kernel
                    .evaluation_observation(
                        &runtime_subject(config)?,
                        &job.key,
                        "unknown",
                        None,
                        certificate,
                        ca,
                    )
                    .await?
                    .require()?;
                return Ok(());
            }
        }
    }
    let measured = job.result.context("measured job lost evaluator result")?;
    let state = kernel.state().await?;
    if result_applied(&state, &job.request, &measured)? {
        kernel
            .evaluation_observation(
                &runtime_subject(config)?,
                &job.key,
                "applied",
                None,
                certificate,
                ca,
            )
            .await?
            .require()?;
        return Ok(());
    }
    if verify_current(
        &measured,
        &job.request,
        &state.domain,
        certificate,
        ca,
        crate::cluster::now_ms(),
    )
    .is_err()
    {
        kernel
            .evaluation_observation(
                &runtime_subject(config)?,
                &job.key,
                "stale",
                None,
                certificate,
                ca,
            )
            .await?
            .require()?;
        return Ok(());
    }
    let now = crate::cluster::now_ms();
    let delivery = EvaluationDelivery {
        request_id: format!("deliver-{}", job.request.payload.request_id),
        request: job.request.clone(),
        result_digest: hash_json(&measured)?,
        issued_ms: now,
        deadline_ms: now
            .saturating_add(config.request_timeout_ms)
            .min(job.request.payload.deadline_ms),
    }
    .sign(&kernel.signer)?;
    match process.deliver(delivery).await {
        Ok(_) => {
            kernel
                .evaluation_observation(
                    &runtime_subject(config)?,
                    &job.key,
                    "applied",
                    None,
                    certificate,
                    ca,
                )
                .await?
                .require()?;
        }
        Err(_) => {
            kernel
                .evaluation_observation(
                    &runtime_subject(config)?,
                    &job.key,
                    "unknown",
                    None,
                    certificate,
                    ca,
                )
                .await?
                .require()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_renewed_lease_bounds_protected_reads_and_security_stop_ends_scope() {
        let mut fixture = crate::security::test_support::fixture();
        let resource = "evaluate/g".to_owned();
        fixture.posture.resources.insert(resource.clone());
        fixture.policy.payload.inventory.insert(resource.clone());
        fixture
            .policy
            .payload
            .trusted_components
            .insert(fixture.subject.id().into());
        for capability in [
            &mut fixture.posture.capability_ceiling[0],
            &mut fixture.definition.capabilities[0],
        ] {
            capability.constraints.resources.insert(resource.clone());
            capability.constraints.operations.insert("evaluate".into());
        }
        fixture.policy = fixture
            .signer
            .sign("deployment-policy", fixture.policy.payload)
            .unwrap();
        fixture.posture.evaluator_public_key =
            crate::identity::workload_public_key(&fixture.identity.certificate_der).unwrap();
        let mut spec = crate::security::test_support::goal_spec(&fixture);
        spec.evaluator_id = fixture.subject.id().into();
        crate::security::bind_goal_contract(&mut fixture.posture, &spec).unwrap();
        spec.security = serde_json::to_value(&fixture.posture).unwrap();
        let mut state = KernelState::new(
            fixture.policy.clone(),
            &fixture.signer.public_key(),
            fixture.now,
            "evaluator-lease-test".into(),
        )
        .unwrap();
        let approval = state
            .security
            .issue_approval(
                &fixture.operators[0],
                fixture.posture.registration_hash().unwrap(),
                [0; 32],
                0,
                Scope::Tenant { tenant: "t".into() },
                fixture.now + 600,
                &fixture.signer,
                fixture.now,
            )
            .unwrap();
        state
            .security
            .register_goal(
                &AuthenticatedSubject::operator(&fixture.operators[0]),
                fixture.posture.clone(),
                &[approval],
                &mut state.ledger,
                fixture.now,
            )
            .unwrap();
        state
            .domain
            .execute(
                &crate::domain::Actor {
                    id: fixture.operators[0].id(),
                    role: crate::domain::Role::Operator,
                },
                crate::domain::DomainCommand::RegisterGoal { spec },
                fixture.now * 1000,
            )
            .unwrap();
        state
            .security
            .compile_definition(
                fixture.definition.clone(),
                None,
                &mut state.ledger,
                fixture.now,
            )
            .unwrap();
        let session = Session {
            workload: fixture.definition.holder.clone(),
            grant_id: fixture.definition.id.clone(),
            envelope_id: "fixture-service-envelope".into(),
            task_id: "fixture-service-task".into(),
            allocation_fence: fixture.definition.lease.fencing_token,
            task_fence: 1,
            lease_expiry_ms: fixture.now * 1000 - 1,
            isolation: crate::security::IsolationTier::P,
            provider: "local".into(),
            model: "m1".into(),
            history: crate::planner::HistoryConfig::default(),
            tools: vec![],
        };
        let now = crate::cluster::now_ms();
        let renewed = now + 120_000;
        assert!(evaluator_scope_active(&state, &session, now));
        let horizon = evaluator_read_horizon(&state, &session, renewed).unwrap();
        assert_eq!(horizon, renewed);
        assert!(horizon > session.lease_expiry_ms);
        let directory =
            std::env::temp_dir().join(format!("ss-eval-lease-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let key = directory.join("key");
        ArtifactStore::create_key(&key).unwrap();
        let mut store = ArtifactStore::open(&directory.join("blobs"), &key).unwrap();
        let bytes = b"\0asm\x01\0\0\0";
        let info = store
            .put(
                bytes,
                crate::artifacts::ArtifactMetadata {
                    goal_id: "g".into(),
                    logical_name: "candidate".into(),
                    version: "v1".into(),
                    classifications: ["internal".into()].into(),
                    trust: 0,
                    producer_id: fixture.subject.id().into(),
                    broker_id: "trusted-fixture-broker".into(),
                    provenance: json!({"source":"fixture"}),
                    parents: vec![],
                    signature: serde_json::to_string(
                        &fixture
                            .signer
                            .sign("fixture-artifact", crate::audit::hash(bytes))
                            .unwrap(),
                    )
                    .unwrap(),
                    created_ms: now,
                },
            )
            .unwrap();
        let mut permit = ReadPermit {
            holder_id: fixture.subject.id().into(),
            goal_ids: ["g".into()].into(),
            classifications: ["internal".into()].into(),
            expiry_ms: session.lease_expiry_ms,
            allow_retracted: false,
        };
        assert!(store.get(&info.id, &permit, now).is_err());
        permit.expiry_ms = horizon;
        assert_eq!(store.get(&info.id, &permit, now).unwrap().as_slice(), bytes);
        let mut fenced = session.clone();
        fenced.allocation_fence += 1;
        assert!(!evaluator_scope_active(&state, &fenced, now));
        assert!(evaluator_read_horizon(&state, &fenced, renewed).is_err());
        state
            .security
            .engage_ledger_stop("t".into(), "g".into(), [9; 32], now / 1000)
            .unwrap();
        assert_eq!(state.domain.goal("g").unwrap().status, "active");
        assert!(!evaluator_scope_active(&state, &session, now));
        assert!(evaluator_read_horizon(&state, &session, renewed).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
