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
            let state: KernelState = serde_json::from_slice(&fs::read(snapshot)?)?;
            state.validate()?;
            let value = match kind.as_str() {
                "goal" => serde_json::to_value(state.domain.goal(&id)?)?,
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
                json!({"domain_revision":state.domain.revision,"authoritative_revision":null,"object":value,"source":"saved snapshot; authoritative readback revision must be retained separately; no effects replayed"})
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
            effect.permit.verify(
                "dispatch-permit",
                &state.security.policy.payload.permit_signer,
            )?;
            ensure!(
                effect.permit.payload.intent_id == *id
                    && effect.payload == effect.request.action.arguments,
                "effect identity/payload mismatch"
            );
            let event = state
                .audit
                .events()
                .iter()
                .find(|event| event.id == format!("authorize/{id}"))
                .context("effect lacks authorization event")?;
            ensure!(
                event.payload_hash == solos_swarm::audit::content_hash(&effect.permit.payload)?,
                "effect authorization audit mismatch"
            );
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
