//! Independently owned append-only witness: the control plane can extend, never rewrite.
use crate::audit::{
    ConsistencyProof, CryptoSigner, Hash, Signed, SignedTreeHead, TreeHead, hash,
    verify_consistency,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WitnessRequest {
    pub head: SignedTreeHead,
    pub consistency: ConsistencyProof,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WitnessReceipt {
    pub log_id: String,
    pub size: u64,
    pub root: Hash,
    pub witnessed_unix: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WitnessCheckpoint {
    pub head: SignedTreeHead,
    pub receipt: Signed<WitnessReceipt>,
}
impl WitnessCheckpoint {
    pub fn verify(
        &self,
        controlplane_key: &Hash,
        witness_key: &Hash,
        log_id: &str,
        minimum_size: u64,
        now: u64,
        max_age: u64,
    ) -> Result<()> {
        self.head.verify("audit-tree-head", controlplane_key)?;
        self.receipt.verify("audit-witness-receipt", witness_key)?;
        let h = &self.head.payload;
        let r = &self.receipt.payload;
        ensure!(
            max_age > 0
                && h.log_id == log_id
                && r.log_id == log_id
                && h.size == r.size
                && h.root == r.root
                && h.size >= minimum_size
                && h.timestamp <= now.saturating_add(300)
                && r.witnessed_unix <= now.saturating_add(300)
                && now.saturating_sub(r.witnessed_unix) <= max_age,
            "witness checkpoint stale, mismatched or rolled back"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalRecord {
    request: WitnessRequest,
    receipt: Signed<WitnessReceipt>,
}
pub struct OwnedWitness {
    directory: PathBuf,
    journal: File,
    log_id: String,
    controlplane_key: Hash,
    witness_key: Hash,
    latest: Option<JournalRecord>,
    poisoned: bool,
}
unsafe extern "C" {
    fn geteuid() -> u32;
}
fn uid() -> u32 {
    unsafe { geteuid() }
}
impl OwnedWitness {
    pub fn open(
        directory: &Path,
        log_id: String,
        controlplane_key: Hash,
        witness_key: Hash,
        controlplane_uid: u32,
    ) -> Result<Self> {
        ensure!(
            controlplane_uid != 0 && controlplane_uid != uid(),
            "witness requires independent ownership from a non-root control plane"
        );
        ensure!(
            !log_id.is_empty() && controlplane_key != witness_key,
            "independent witness signing authority required"
        );
        if !directory.exists() {
            fs::create_dir(directory)?;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        let metadata = fs::symlink_metadata(directory)?;
        ensure!(
            metadata.is_dir() && metadata.uid() == uid() && metadata.mode() & 0o077 == 0,
            "witness directory owner/permissions invalid"
        );
        let path = directory.join("heads.jsonl");
        if path.exists() {
            let metadata = fs::symlink_metadata(&path)?;
            ensure!(
                metadata.is_file() && metadata.uid() == uid() && metadata.mode() & 0o077 == 0,
                "witness journal owner/permissions invalid"
            );
        }
        let journal = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&path)?;
        journal
            .try_lock()
            .context("witness store already has a writer")?;
        let mut latest: Option<JournalRecord> = None;
        let mut reader = BufReader::new(File::open(&path)?);
        loop {
            let mut line = vec![];
            let count = reader.by_ref().take(65537).read_until(b'\n', &mut line)?;
            if count == 0 {
                break;
            }
            ensure!(
                count <= 65536 && line.ends_with(b"\n"),
                "oversized/partial witness journal record"
            );
            let record: JournalRecord =
                serde_json::from_slice(&line).context("damaged witness journal")?;
            Self::validate_record(
                &record,
                &log_id,
                &controlplane_key,
                &witness_key,
                latest.as_ref(),
            )?;
            latest = Some(record);
        }
        let store = Self {
            directory: directory.into(),
            journal,
            log_id,
            controlplane_key,
            witness_key,
            latest,
            poisoned: false,
        };
        if store.latest.is_some() {
            store.persist_latest()?;
        }
        Ok(store)
    }
    fn validate_record(
        record: &JournalRecord,
        log_id: &str,
        cp: &Hash,
        witness: &Hash,
        previous: Option<&JournalRecord>,
    ) -> Result<()> {
        record.request.head.verify("audit-tree-head", cp)?;
        record.receipt.verify("audit-witness-receipt", witness)?;
        let head = &record.request.head.payload;
        let receipt = &record.receipt.payload;
        ensure!(
            head.log_id == log_id
                && receipt.log_id == log_id
                && receipt.size == head.size
                && receipt.root == head.root
                && head.timestamp <= receipt.witnessed_unix.saturating_add(300)
                && receipt.witnessed_unix.saturating_sub(head.timestamp) <= 300,
            "witness identity/head mismatch"
        );
        let genesis = TreeHead {
            log_id: log_id.into(),
            size: 0,
            root: hash(b""),
            timestamp: 0,
        };
        verify_consistency(
            previous
                .map(|p| &p.request.head.payload)
                .unwrap_or(&genesis),
            head,
            &record.request.consistency,
        )?;
        Ok(())
    }
    pub fn accept(
        &mut self,
        request: WitnessRequest,
        signer: &CryptoSigner,
        now: u64,
    ) -> Result<Signed<WitnessReceipt>> {
        ensure!(
            !self.poisoned && signer.public_key() == self.witness_key,
            "witness unavailable or wrong signing authority"
        );
        request
            .head
            .verify("audit-tree-head", &self.controlplane_key)?;
        ensure!(
            request.head.payload.log_id == self.log_id
                && request.head.payload.timestamp <= now.saturating_add(300)
                && now.saturating_sub(request.head.payload.timestamp) <= 300,
            "wrong log or future tree head"
        );
        if let Some(previous) = &self.latest
            && previous.request.head.payload == request.head.payload
        {
            let receipt = previous.receipt.clone();
            self.persist_latest()?;
            return Ok(receipt);
        }
        let receipt = signer.sign(
            "audit-witness-receipt",
            WitnessReceipt {
                log_id: self.log_id.clone(),
                size: request.head.payload.size,
                root: request.head.payload.root,
                witnessed_unix: now,
            },
        )?;
        let record = JournalRecord {
            request,
            receipt: receipt.clone(),
        };
        Self::validate_record(
            &record,
            &self.log_id,
            &self.controlplane_key,
            &self.witness_key,
            self.latest.as_ref(),
        )?;
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        if let Err(error) = self
            .journal
            .write_all(&bytes)
            .and_then(|_| self.journal.sync_all())
        {
            self.poisoned = true;
            return Err(error.into());
        }
        self.latest = Some(record);
        self.persist_latest()?;
        Ok(receipt)
    }
    fn persist_latest(&self) -> Result<()> {
        // The journal is authoritative if a crash interrupts this convenience readback.
        let temp = self.directory.join("latest.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec(
            &self
                .latest
                .as_ref()
                .context("missing witness head")?
                .request
                .head,
        )?)?;
        file.sync_all()?;
        fs::rename(temp, self.directory.join("latest.json"))?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
    pub fn latest(&self) -> Option<SignedTreeHead> {
        self.latest.as_ref().map(|r| r.request.head.clone())
    }
    pub fn checkpoint(&self) -> Option<WitnessCheckpoint> {
        self.latest.as_ref().map(|r| WitnessCheckpoint {
            head: r.request.head.clone(),
            receipt: r.receipt.clone(),
        })
    }
}
#[derive(Clone)]
pub struct WitnessApi {
    pub store: Arc<Mutex<OwnedWitness>>,
    pub signer: Arc<CryptoSigner>,
}
async fn append(
    State(api): State<WitnessApi>,
    Json(request): Json<WitnessRequest>,
) -> std::result::Result<Json<Signed<WitnessReceipt>>, (StatusCode, Json<serde_json::Value>)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    api.store.lock().await.accept(request,&api.signer,now).map(Json).map_err(|_|(StatusCode::CONFLICT,Json(serde_json::json!({"error":"witness refused unverifiable, forked or stale history"}))))
}
async fn read_checkpoint(State(api): State<WitnessApi>) -> Json<Option<WitnessCheckpoint>> {
    Json(api.store.lock().await.checkpoint())
}
/// Serve with mTLS and a separately administered service account; no mutation/cleanup route exists.
pub fn router(api: WitnessApi) -> Router {
    Router::new()
        .route("/witness/append", post(append))
        .route("/witness/head", get(read_checkpoint))
        .layer(DefaultBodyLimit::max(65536))
        .with_state(api)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditEvent, AuditLog};
    #[test]
    fn witnessed_history_survives_restart_and_rejects_forks() {
        let directory = std::env::temp_dir().join(format!("witness-{}", uuid::Uuid::new_v4()));
        let cp = CryptoSigner::from_seed([31; 32]);
        let witness = CryptoSigner::from_seed([32; 32]);
        let other_uid = if uid() == 1 { 2 } else { 1 };
        assert!(
            OwnedWitness::open(
                &directory,
                "log".into(),
                cp.public_key(),
                witness.public_key(),
                uid()
            )
            .is_err()
        );
        let mut store = OwnedWitness::open(
            &directory,
            "log".into(),
            cp.public_key(),
            witness.public_key(),
            other_uid,
        )
        .unwrap();
        let mut log = AuditLog::new("log".into()).unwrap();
        for n in 1..=4 {
            log.append(AuditEvent {
                correlation: None,
                id: n.to_string(),
                kind: "decision".into(),
                actor: "operator".into(),
                goal: None,
                timestamp: n,
                decision: "allow".into(),
                payload_hash: hash(&[n as u8]),
            })
            .unwrap();
            let head = log.signed_head(&cp, n).unwrap();
            let receipt = store
                .accept(
                    WitnessRequest {
                        head,
                        consistency: log.consistency(n - 1, n).unwrap(),
                    },
                    &witness,
                    n,
                )
                .unwrap();
            receipt
                .verify("audit-witness-receipt", &witness.public_key())
                .unwrap();
        }
        drop(store);
        let mut store = OwnedWitness::open(
            &directory,
            "log".into(),
            cp.public_key(),
            witness.public_key(),
            other_uid,
        )
        .unwrap();
        assert_eq!(store.latest().unwrap().payload.size, 4);
        let mut fork = log.signed_head(&cp, 5).unwrap().payload;
        fork.root[0] ^= 1;
        let fork = cp.sign("audit-tree-head", fork).unwrap();
        assert!(
            store
                .accept(
                    WitnessRequest {
                        head: fork,
                        consistency: log.consistency(4, 4).unwrap()
                    },
                    &witness,
                    5
                )
                .is_err()
        );
        let (mut old, proof) = (
            log.signed_head(&cp, 5).unwrap(),
            log.consistency(3, 4).unwrap(),
        );
        old.payload.size = 3;
        assert!(
            store
                .accept(
                    WitnessRequest {
                        head: old,
                        consistency: proof
                    },
                    &witness,
                    5
                )
                .is_err()
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
