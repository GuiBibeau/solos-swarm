use crate::broker::{now_ms, write_json};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde_json::{Value, json};
use solos_swarm::{
    audit::{Hash, Signed},
    kernel::{KernelState, hex},
};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Subcommand)]
pub enum MvpCommand {
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    WitnessServe {
        #[arg(long)]
        config: PathBuf,
    },
    OfflineStartupRequest {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    OfflineStartupAssemble {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        requests: PathBuf,
        /// JSON map of exact node IDs to operator-issued fresh DER certificate paths.
        #[arg(long)]
        certificates: PathBuf,
        #[arg(long)]
        epoch: u64,
        #[arg(long)]
        expires_ms: u64,
        #[arg(long)]
        output: PathBuf,
    },
    Campaign {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    Qa {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = "quick")]
        profile: String,
        #[arg(long)]
        selection: Option<PathBuf>,
    },
    EvaluatorServe {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        certificate: PathBuf,
        #[arg(long)]
        private_key: PathBuf,
        #[arg(long)]
        ca: PathBuf,
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        ipc_socket: Option<PathBuf>,
        #[arg(long)]
        broker_uid: Option<u32>,
    },
    VerifySignature {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        trusted_key: PathBuf,
        #[arg(long)]
        domain: String,
    },
    Inspect {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        id: String,
    },
    Verify {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        head: PathBuf,
        #[arg(long)]
        trusted_key: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    Catalog {
        #[arg(long)]
        seed: u64,
        #[arg(long)]
        output: PathBuf,
    },
}
fn key(path: &Path) -> Result<Hash> {
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    if let Some(encoded) = value.as_str() {
        ensure!(
            encoded.len() == 64 && encoded.bytes().all(|c| c.is_ascii_hexdigit()),
            "trusted key must be 32-byte hex"
        );
        let mut bytes = [0; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&encoded[i * 2..i * 2 + 2], 16)?;
        }
        Ok(bytes)
    } else {
        Ok(serde_json::from_value(value)?)
    }
}
fn public_bytes(path: &Path, max: u64) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= max,
        "public recovery input exceeds file bounds"
    );
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= max,
        "public recovery input grew beyond its bound"
    );
    Ok(bytes)
}
pub async fn execute(command: MvpCommand) -> Result<()> {
    match command {
        MvpCommand::Serve { config } => solos_swarm::runtime::serve(&config).await?,
        MvpCommand::WitnessServe { config } => solos_swarm::runtime::serve_witness(&config).await?,
        MvpCommand::OfflineStartupRequest { config, output } => {
            let request = solos_swarm::runtime::offline_startup_request(&config).await?;
            write_json(&output, &request)?;
            println!(
                "{}",
                json!({"output":output,"node_id":request.node_id,"prior_epoch":request.prior_epoch})
            );
        }
        MvpCommand::OfflineStartupAssemble {
            config,
            requests,
            certificates,
            epoch,
            expires_ms,
            output,
        } => {
            let requests: Vec<solos_swarm::cluster::OfflineStartupRequest> =
                serde_json::from_slice(&public_bytes(&requests, 32 * 1024 * 1024)?)?;
            let paths: BTreeMap<u64, PathBuf> =
                serde_json::from_slice(&public_bytes(&certificates, 1024 * 1024)?)?;
            ensure!(
                !paths.is_empty() && paths.len() <= 1024 && requests.len() <= 1024,
                "offline inventory exceeds operator input bounds"
            );
            let leaves = paths
                .into_iter()
                .map(|(node, path)| Ok((node, public_bytes(&path, 32768)?)))
                .collect::<Result<BTreeMap<_, _>>>()?;
            let admission = solos_swarm::runtime::offline_startup_admission(
                &config, &requests, leaves, epoch, expires_ms,
            )?;
            write_json(&output, &admission)?;
            println!(
                "{}",
                json!({"output":output,"epoch":epoch,"admission_sha256":hex(&solos_swarm::audit::content_hash(&admission)?)})
            );
        }
        MvpCommand::Campaign { config, output } => {
            let config = serde_json::from_slice(&fs::read(config)?)?;
            let report = solos_swarm::qa::execute_campaign(config, &output).await?;
            println!("{}", serde_json::to_string(&report)?);
            ensure!(
                matches!(report.verdict, solos_swarm::scenarios::Verdict::Pass),
                "campaign did not pass"
            );
        }
        MvpCommand::Qa {
            config,
            output,
            profile,
            selection,
        } => {
            let config = serde_json::from_slice(&fs::read(config)?)?;
            let profile = match profile.as_str() {
                "quick" => solos_swarm::qa::QaProfile::Quick,
                "release-small" => solos_swarm::qa::QaProfile::ReleaseSmall,
                "integration" => solos_swarm::qa::QaProfile::Integration,
                "acceptance" => solos_swarm::qa::QaProfile::Acceptance,
                "eval" => solos_swarm::qa::QaProfile::Eval,
                _ => anyhow::bail!("unknown QA profile"),
            };
            let selection = match selection {
                Some(path) => serde_json::from_slice(&fs::read(path)?)?,
                None => solos_swarm::qa::QaSelection {
                    profile,
                    scenarios: vec![],
                    changed_components: vec!["unknown-impact".into()],
                },
            };
            let report = solos_swarm::qa::run(config, selection, &output).await?;
            println!("{}", serde_json::to_string(&report)?);
            ensure!(
                matches!(report.verdict, solos_swarm::scenarios::Verdict::Pass),
                "QA did not pass"
            );
        }
        MvpCommand::EvaluatorServe {
            bundle,
            certificate,
            private_key,
            ca,
            state,
            ipc_socket,
            broker_uid,
        } => {
            let files = solos_swarm::evaluator::EvaluatorFiles {
                bundle,
                certificate,
                private_key,
                ca,
                state,
            };
            if let Some(socket) = ipc_socket {
                solos_swarm::evaluator::serve_socket(
                    &files,
                    &socket,
                    broker_uid.context("external evaluator requires admitted broker UID")?,
                )
                .await?;
            } else {
                ensure!(
                    broker_uid.is_none(),
                    "broker UID requires evaluator IPC socket"
                );
                solos_swarm::evaluator::serve_stdio(files).await?;
            }
        }
        MvpCommand::VerifySignature {
            input,
            trusted_key,
            domain,
        } => {
            let signed: Signed<Value> = serde_json::from_slice(&fs::read(input)?)?;
            signed.verify(&domain, &key(&trusted_key)?)?;
            println!("{}", json!({"verified":true,"domain":domain}));
        }
        MvpCommand::Inspect { snapshot, kind, id } => {
            let packet: Value = serde_json::from_slice(&fs::read(snapshot)?)?;
            let saved_revision = packet
                .get("snapshot")
                .and_then(|_| packet.get("revision"))
                .and_then(Value::as_u64);
            let state: KernelState =
                serde_json::from_value(packet.get("snapshot").unwrap_or(&packet).clone())?;
            state.validate()?;
            let value = match kind.as_str() {
                "goal" => serde_json::to_value(state.domain.goal(&id)?)?,
                "explain" => {
                    let (goal, task) = id
                        .split_once('/')
                        .map_or((id.as_str(), None), |(goal, task)| (goal, Some(task)));
                    let current = state.domain.goal(goal)?;
                    let fence = task
                        .map(|id| state.domain.task(goal, id).map(|task| task.fencing_token))
                        .transpose()?;
                    let key = format!("{goal}/{}", task.unwrap_or("@goal"));
                    let record = state
                        .diagnostic_reasons
                        .get(&key)
                        .or_else(|| state.diagnostic_reasons.get(&format!("{goal}/@goal")))
                        .filter(|record| {
                            record.goal_version == current.spec.version
                                && record
                                    .task_fence
                                    .is_none_or(|recorded| Some(recorded) == fence)
                        });
                    json!({"goal_id":goal,"task_id":task,"reason":record.map(|record|record.reason.clone()).unwrap_or(solos_swarm::telemetry::DiagnosticReason::Unknown),"record":record,"recorded_authoritative_revision":saved_revision})
                }
                "board" => serde_json::to_value(
                    state
                        .domain
                        .boards
                        .get(&id)
                        .context("Board not found; use Goal ID")?,
                )?,
                "need" => serde_json::to_value(
                    state
                        .domain
                        .needs
                        .get(&id)
                        .context("Need not found; use Goal/Need ID")?,
                )?,
                "task" => serde_json::to_value(
                    state
                        .domain
                        .tasks
                        .get(&id)
                        .context("Task not found; use Goal/Task ID")?,
                )?,
                "intent" => {
                    serde_json::to_value(state.effects.get(&id).context("intent not found")?)?
                }
                "finding" => serde_json::to_value(
                    state
                        .domain
                        .findings
                        .get(&id)
                        .context("Finding not found")?,
                )?,
                "attempt" => serde_json::to_value(
                    state
                        .domain
                        .attempts
                        .get(&id)
                        .context("Attempt not found")?,
                )?,
                "request" => {
                    serde_json::to_value(state.replies.get(&id).context("request not found")?)?
                }
                "review" => serde_json::to_value(
                    state
                        .pending_actions
                        .get(&id)
                        .context("pending review not found")?,
                )?,
                _ => anyhow::bail!("unsupported inspected object"),
            };
            println!(
                "{}",
                json!({"domain_revision":state.domain.revision,"authoritative_revision":saved_revision,"object":value,"source":"saved snapshot/readback, not a fresh query; supplied revision metadata requires separately verified signed evidence; no effects replayed"})
            );
        }
        MvpCommand::Verify {
            snapshot,
            head,
            trusted_key,
            output,
        } => {
            let state: KernelState = serde_json::from_slice(&fs::read(snapshot)?)?;
            let head: solos_swarm::audit::SignedTreeHead =
                serde_json::from_slice(&fs::read(head)?)?;
            let report = verify_snapshot(&state, &head, &key(&trusted_key)?);
            write_json(&output, &report)?;
            println!("{report}");
            ensure!(
                report["evidence_verdict"] == "PASS",
                "evidence verification failed"
            );
        }
        MvpCommand::Catalog { seed, output } => {
            let family = solos_swarm::scenarios::select_family(seed);
            let case = solos_swarm::scenarios::prepare_case(family, seed);
            ensure!(!output.exists(), "fixture destination already exists");
            fs::create_dir_all(&output)?;
            write_json(&output.join("public-case.json"), &case.public_value())?;
            let protected = output.join("protected");
            fs::create_dir(&protected)?;
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&protected, fs::Permissions::from_mode(0o700))?;
            write_json(&protected.join("case.json"), &case.protected_value())?;
            println!(
                "{}",
                json!({"case_directory":output,"fixture_digest":case.fixture_digest(),"catalog_version":case.version})
            );
        }
    }
    Ok(())
}
fn verify_permit_authorization(
    intent: &str,
    current: &Signed<solos_swarm::security::DispatchPermit>,
    events: &[solos_swarm::audit::AuditEvent],
    replies: &BTreeMap<String, solos_swarm::kernel::RecordedReply>,
    signer: &Hash,
) -> Result<()> {
    use solos_swarm::audit::content_hash;
    current.verify("dispatch-permit", signer)?;
    let permit = &current.payload;
    ensure!(
        permit.intent_id == intent,
        "permit intent differs from effect"
    );
    let original_index = events
        .iter()
        .position(|event| event.id == format!("authorize/{intent}"))
        .context("effect lacks original authorization event")?;
    let authorization = &events[original_index];
    ensure!(
        authorization.kind == "dispatch_authorized"
            && authorization.actor == permit.holder
            && authorization.goal.as_deref() == Some(permit.goal_id.as_str()),
        "original authorization identity or Goal mismatch"
    );
    if authorization.payload_hash == content_hash(permit)? {
        ensure!(
            authorization.decision == format!("{:?}", permit.confirmation),
            "original authorization confirmation mismatch"
        );
        return Ok(());
    }
    let refresh_id = format!("permit-refresh/{intent}/{}", hex(&content_hash(current)?));
    let refresh_index = events
        .iter()
        .position(|event| event.id == refresh_id)
        .context("effect lacks the exact signed-permit refresh event")?;
    let refresh = &events[refresh_index];
    ensure!(
        original_index < refresh_index
            && refresh.kind == "dispatch_permit_refreshed"
            && refresh.actor == permit.holder
            && refresh.goal.as_deref() == Some(permit.goal_id.as_str())
            && refresh.payload_hash == content_hash(permit)?
            && refresh.decision == format!("{:?}", permit.confirmation),
        "permit refresh audit binding mismatch"
    );
    let mut original: Option<Signed<solos_swarm::security::DispatchPermit>> = None;
    for (id, reply) in replies {
        if !reply.ok
            || reply.actor != permit.holder
            || reply.value["intent"] != intent
            || reply.value["state"] != "pending"
        {
            continue;
        }
        let candidate: Signed<solos_swarm::security::DispatchPermit> =
            serde_json::from_value(reply.value["permit"].clone())?;
        if content_hash(&candidate.payload)? != authorization.payload_hash {
            continue;
        }
        candidate.verify("dispatch-permit", signer)?;
        ensure!(
            candidate.payload.intent_id == intent
                && candidate.payload.holder == reply.actor
                && authorization.decision == format!("{:?}", candidate.payload.confirmation),
            "original admission permit identity mismatch"
        );
        let request_index = events
            .iter()
            .position(|event| event.id == format!("request/{id}"))
            .context("original admission lacks recorded request evidence")?;
        let request = &events[request_index];
        ensure!(
            original_index < request_index
                && request_index < refresh_index
                && request.kind == "kernel_request"
                && request.actor == reply.actor
                && request.decision == "allow"
                && request.payload_hash == reply.request_hash
                && content_hash(&(&reply.actor, &reply.request))? == reply.request_hash,
            "original admission request/audit binding mismatch"
        );
        if let Some(previous) = &original {
            ensure!(
                content_hash(previous)? == content_hash(&candidate)?,
                "original admission contains conflicting signed permits"
            );
        }
        original = Some(candidate);
    }
    let original =
        original.context("refreshed effect lost its original signed admission permit")?;
    let old = &original.payload;
    ensure!(
        !old.billable
            && !permit.billable
            && old.price_version.is_none()
            && permit.price_version.is_none()
            && permit.intent_id == old.intent_id
            && permit.holder == old.holder
            && permit.certificate_hash == old.certificate_hash
            && permit.goal_id == old.goal_id
            && permit.grant_id == old.grant_id
            && permit.grant_version == old.grant_version
            && permit.fencing_token == old.fencing_token
            && permit.action_hash == old.action_hash
            && permit.evidence_hash == old.evidence_hash
            && permit.observed_hash == old.observed_hash
            && permit.confirmation >= old.confirmation
            && permit.grant_epoch >= old.grant_epoch
            && permit.stop_epoch >= old.stop_epoch
            && permit.retraction_epoch >= old.retraction_epoch
            && permit.issued >= old.issued
            && permit.issued < old.expiry
            && permit.expiry > permit.issued
            && permit.expiry <= old.expiry,
        "permit refresh widened or changed the original application authority"
    );
    Ok(())
}

pub fn verify_snapshot(
    state: &KernelState,
    head: &solos_swarm::audit::SignedTreeHead,
    trusted: &Hash,
) -> Value {
    let check = || -> Result<()> {
        state.validate()?;
        head.verify("audit-tree-head", trusted)?;
        let checkpoint = state
            .audit
            .events()
            .last()
            .context("state checkpoint missing")?;
        ensure!(
            checkpoint.kind == "state_checkpoint"
                && checkpoint.payload_hash == state.content_hash()?,
            "state differs from its audited checkpoint"
        );
        ensure!(
            head.payload.log_id == state.audit.log_id
                && head.payload.size == state.audit.events().len() as u64
                && head.payload.size > 0,
            "audit head missing events or wrong deployment"
        );
        let recomputed = state.audit.signed_head(
            &solos_swarm::audit::CryptoSigner::from_seed([0; 32]),
            head.payload.timestamp,
        )?;
        ensure!(
            recomputed.payload.root == head.payload.root,
            "audit tree root differs from witnessed head"
        );
        for (id, reply) in &state.replies {
            let event = state
                .audit
                .events()
                .iter()
                .find(|event| event.id == format!("request/{id}"))
                .context("recorded request lacks audit event")?;
            ensure!(
                solos_swarm::audit::content_hash(&(&reply.actor, &reply.request))?
                    == reply.request_hash
                    && event.payload_hash == reply.request_hash
                    && event.actor == reply.actor
                    && event.decision == if reply.ok { "allow" } else { "deny" },
                "request evidence differs from recorded decision"
            );
        }
        for (id, effect) in &state.effects {
            ensure!(
                effect.permit.payload.intent_id == *id
                    && effect.payload == effect.request.action.arguments,
                "effect identity/payload mismatch"
            );
            verify_permit_authorization(
                id,
                &effect.permit,
                state.audit.events(),
                &state.replies,
                &state.security.policy.payload.permit_signer,
            )?;
            if effect.permit.payload.billable {
                let reservation = state
                    .ledger
                    .intents
                    .get(id)
                    .context("billable effect lacks funded intent")?;
                ensure!(
                    reservation.request.request_hash == effect.permit.payload.action_hash,
                    "receipt bound to wrong request"
                );
                if let Some(receipt) = &reservation.receipt {
                    ensure!(
                        receipt.intent_id == *id
                            && receipt.provider == reservation.price.provider
                            && receipt.request_hash == reservation.request.request_hash
                            && reservation.price.cost(&receipt.usage)? == reservation.settled,
                        "wrong or conflicting external receipt"
                    );
                }
                ensure!(
                    effect.state != "done" || reservation.receipt.is_some(),
                    "completed billable effect has no receipt"
                );
            }
        }
        for (id, finding) in &state.domain.findings {
            let source = state
                .finding_sources
                .get(id)
                .context("Finding lost broker lineage")?;
            let provenance = state
                .security
                .findings
                .get(source)
                .context("Finding signature missing")?;
            provenance.verify(
                "finding-provenance",
                &state.security.policy.payload.provenance_signer,
            )?;
            ensure!(
                provenance.payload.result_hash == solos_swarm::audit::content_hash(&finding.body)?
                    && provenance.payload.producer == finding.provenance.producer_id
                    && provenance.payload.goal_id == finding.goal_id
                    && provenance.payload.markings == finding.markings,
                "Finding body/producer/markings altered"
            );
        }
        Ok(())
    };
    let (verdict, reason) = match check() {
        Ok(()) => (
            "PASS",
            "recorded authorization, accounting, lineage and Merkle evidence agree".to_owned(),
        ),
        Err(error) => ("FAIL", error.to_string()),
    };
    json!({"schema_version":1,"evidence_verdict":verdict,"runtime_verdict":"INCONCLUSIVE","goal_verdict":"INCONCLUSIVE","cleanup_verdict":"INCONCLUSIVE",
        "checks":[{"id":"recorded-evidence","verdict":verdict,"reason":reason}],"domain_revision":state.domain.revision,"audit_root":hex(&head.payload.root),"verified_ms":now_ms()})
}

#[cfg(test)]
mod recovery_input_tests {
    use super::*;
    #[test]
    fn recovery_inputs_require_bounded_regular_files() {
        let root =
            std::env::temp_dir().join(format!("solos-recovery-cli-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let path = root.join("public.json");
        fs::write(&path, b"[]").unwrap();
        assert_eq!(public_bytes(&path, 2).unwrap(), b"[]");
        assert!(public_bytes(&path, 1).is_err());
        assert!(public_bytes(&root, 100).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod permit_history_tests {
    use super::*;
    use solos_swarm::{
        audit::{AuditEvent, CryptoSigner, content_hash},
        kernel::RecordedReply,
        security::{ConfirmationTier, DispatchPermit},
    };

    struct History {
        signer: CryptoSigner,
        original: Signed<DispatchPermit>,
        current: Signed<DispatchPermit>,
        events: Vec<AuditEvent>,
        replies: BTreeMap<String, RecordedReply>,
    }
    impl History {
        fn new() -> Self {
            let signer = CryptoSigner::from_seed([73; 32]);
            let original = signer
                .sign(
                    "dispatch-permit",
                    DispatchPermit {
                        intent_id: "local-intent".into(),
                        holder: "spiffe://test.local/t/g/n/a/1".into(),
                        certificate_hash: Some([1; 32]),
                        goal_id: "g".into(),
                        grant_id: "a-1".into(),
                        grant_version: 1,
                        fencing_token: 4,
                        action_hash: [2; 32],
                        stop_epoch: 0,
                        grant_epoch: 1,
                        retraction_epoch: 0,
                        evidence_hash: [3; 32],
                        observed_hash: [4; 32],
                        price_version: None,
                        confirmation: ConfirmationTier::C1,
                        issued: 100,
                        expiry: 130,
                        billable: false,
                    },
                )
                .unwrap();
            let mut refreshed = original.payload.clone();
            refreshed.grant_epoch = 2;
            refreshed.issued = 101;
            let current = signer.sign("dispatch-permit", refreshed).unwrap();
            let reply = RecordedReply {
                correlation: None,
                actor: original.payload.holder.clone(),
                request: json!({"immutable_request_hash":"fixture-only"}),
                request_hash: [0; 32],
                ok: true,
                value: json!({"intent":"local-intent","state":"pending","permit":original}),
            };
            let mut reply = reply;
            reply.request_hash = content_hash(&(&reply.actor, &reply.request)).unwrap();
            let events = vec![
                AuditEvent {
                    correlation: None,
                    id: "authorize/local-intent".into(),
                    kind: "dispatch_authorized".into(),
                    actor: original.payload.holder.clone(),
                    goal: Some("g".into()),
                    timestamp: 100,
                    decision: "C1".into(),
                    payload_hash: content_hash(&original.payload).unwrap(),
                },
                AuditEvent {
                    correlation: None,
                    id: "request/admit-local/phase".into(),
                    kind: "kernel_request".into(),
                    actor: reply.actor.clone(),
                    goal: None,
                    timestamp: 100,
                    decision: "allow".into(),
                    payload_hash: reply.request_hash,
                },
                AuditEvent {
                    correlation: None,
                    id: format!(
                        "permit-refresh/local-intent/{}",
                        hex(&content_hash(&current).unwrap())
                    ),
                    kind: "dispatch_permit_refreshed".into(),
                    actor: current.payload.holder.clone(),
                    goal: Some("g".into()),
                    timestamp: 101,
                    decision: "C1".into(),
                    payload_hash: content_hash(&current.payload).unwrap(),
                },
            ];
            Self {
                signer,
                original,
                current,
                events,
                replies: [("admit-local/phase".into(), reply)].into(),
            }
        }
        fn verify(&self) -> Result<()> {
            verify_permit_authorization(
                "local-intent",
                &self.current,
                &self.events,
                &self.replies,
                &self.signer.public_key(),
            )
        }
        fn resign_current(&mut self) {
            self.current = self
                .signer
                .sign("dispatch-permit", self.current.payload.clone())
                .unwrap();
            self.events[2].id = format!(
                "permit-refresh/local-intent/{}",
                hex(&content_hash(&self.current).unwrap())
            );
            self.events[2].payload_hash = content_hash(&self.current.payload).unwrap();
            self.events[2].decision = format!("{:?}", self.current.payload.confirmation);
            self.events[2].actor = self.current.payload.holder.clone();
            self.events[2].goal = Some(self.current.payload.goal_id.clone());
        }
    }
    #[test]
    fn verifier_accepts_exact_refreshed_history_and_preserves_original_admission() {
        let history = History::new();
        let before = content_hash(&history.replies).unwrap();
        history.verify().unwrap();
        verify_permit_authorization(
            "local-intent",
            &history.original,
            &history.events,
            &history.replies,
            &history.signer.public_key(),
        )
        .unwrap();
        assert_eq!(content_hash(&history.replies).unwrap(), before);
    }
    #[test]
    fn verifier_rejects_missing_or_tampered_original_and_exact_refresh_evidence() {
        for case in [
            "original-event",
            "refresh-event",
            "refresh-id",
            "refresh-hash",
            "refresh-actor",
            "refresh-goal",
            "refresh-kind",
            "refresh-order",
            "original-ack",
            "original-signature",
            "original-request",
            "original-holder",
            "original-hash",
        ] {
            let mut history = History::new();
            match case {
                "original-event" => {
                    history.events.remove(0);
                }
                "refresh-event" => {
                    history.events.pop();
                }
                "refresh-id" => {
                    history.events[2].id = "permit-refresh/unrelated/same-payload-hash".into()
                }
                "refresh-hash" => history.events[2].payload_hash = [9; 32],
                "refresh-actor" => history.events[2].actor = "other-holder".into(),
                "refresh-goal" => history.events[2].goal = Some("other-goal".into()),
                "refresh-kind" => history.events[2].kind = "dispatch_authorized".into(),
                "refresh-order" => history.events.swap(0, 2),
                "original-ack" => history.replies.clear(),
                "original-signature" => {
                    history.original.signature[0] ^= 1;
                    history.replies.get_mut("admit-local/phase").unwrap().value["permit"] =
                        serde_json::to_value(&history.original).unwrap();
                }
                "original-request" => {
                    history
                        .replies
                        .get_mut("admit-local/phase")
                        .unwrap()
                        .request["extra"] = json!(true)
                }
                "original-holder" => {
                    history.replies.get_mut("admit-local/phase").unwrap().actor =
                        "other-holder".into()
                }
                "original-hash" => history.events[0].payload_hash = [9; 32],
                _ => unreachable!(),
            }
            assert!(
                history.verify().is_err(),
                "tampered history accepted: {case}"
            );
        }
    }
    #[test]
    fn verifier_rejects_validly_signed_refreshes_that_change_authority_or_extend_deadlines() {
        for case in [
            "intent",
            "holder",
            "certificate",
            "goal",
            "grant",
            "version",
            "fence",
            "action",
            "evidence",
            "observed",
            "billable",
            "price",
            "confirmation",
            "grant-epoch",
            "stop-epoch",
            "retraction-epoch",
            "expiry",
            "issued",
            "expired-before-refresh",
        ] {
            let mut history = History::new();
            let current = &mut history.current.payload;
            match case {
                "intent" => current.intent_id = "other-intent".into(),
                "holder" => current.holder = "other-holder".into(),
                "certificate" => current.certificate_hash = Some([9; 32]),
                "goal" => current.goal_id = "other-goal".into(),
                "grant" => current.grant_id = "other-grant".into(),
                "version" => current.grant_version += 1,
                "fence" => current.fencing_token += 1,
                "action" => current.action_hash = [9; 32],
                "evidence" => current.evidence_hash = [9; 32],
                "observed" => current.observed_hash = [9; 32],
                "billable" => current.billable = true,
                "price" => current.price_version = Some("v2".into()),
                "confirmation" => current.confirmation = ConfirmationTier::C0,
                "grant-epoch" => current.grant_epoch = 0,
                "stop-epoch" => {
                    history.original.payload.stop_epoch = 1;
                    current.stop_epoch = 0;
                }
                "retraction-epoch" => {
                    history.original.payload.retraction_epoch = 1;
                    current.retraction_epoch = 0;
                }
                "expiry" => current.expiry += 1,
                "issued" => current.issued = 99,
                "expired-before-refresh" => {
                    current.issued = 130;
                    current.expiry = 131;
                }
                _ => unreachable!(),
            }
            if matches!(case, "stop-epoch" | "retraction-epoch") {
                history.original = history
                    .signer
                    .sign("dispatch-permit", history.original.payload.clone())
                    .unwrap();
                history.events[0].payload_hash = content_hash(&history.original.payload).unwrap();
                history.replies.get_mut("admit-local/phase").unwrap().value["permit"] =
                    serde_json::to_value(&history.original).unwrap();
            }
            history.resign_current();
            history
                .current
                .verify("dispatch-permit", &history.signer.public_key())
                .unwrap();
            assert!(
                history.verify().is_err(),
                "changed authority accepted: {case}"
            );
        }
    }
}
