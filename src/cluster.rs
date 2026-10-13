//! Quorum-backed revisioned state and mutually authenticated, bounded QUIC transport.

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use anyhow::{Context, Result, ensure};
use openraft::error::{InstallSnapshotError, NetworkError, RPCError, RaftError, RemoteError};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::storage::{LogFlushed, RaftLogStorage, RaftStateMachine};
use openraft::{
    BasicNode, Entry, EntryPayload, LogId, Snapshot, SnapshotMeta, StorageError, StorageIOError,
    StoredMembership, Vote,
};
use quinn::{ClientConfig, Connection, Endpoint, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Write},
    net::SocketAddr,
    ops::RangeBounds,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Mutex, Notify, RwLock, Semaphore},
    task::JoinHandle,
};
use zeroize::Zeroize;

pub type NodeId = u64;
pub const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_STORAGE_RECORD_BYTES: usize = 1024 * 1024 * 1024;
openraft::declare_raft_types!(pub TypeConfig: D = Command, R = TxnResult);
type Raft = openraft::Raft<TypeConfig>;
type RpcError<E = openraft::error::Infallible> = RPCError<NodeId, BasicNode, RaftError<NodeId, E>>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Compare {
    pub key: String,
    pub expected: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LeaseCheck {
    pub id: String,
    pub holder: String,
    pub fence: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Mutation {
    Put {
        key: String,
        value: Value,
    },
    Delete {
        key: String,
    },
    GrantLease {
        id: String,
        holder: String,
        expires_ms: u64,
    },
    RenewLease {
        id: String,
        holder: String,
        fence: u64,
        expires_ms: u64,
    },
    RevokeLease {
        id: String,
    },
    ExpireLeases,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Transaction {
    pub request_id: String,
    pub expected_revision: Option<u64>,
    pub compares: Vec<Compare>,
    #[serde(default)]
    pub leases: Vec<LeaseCheck>,
    pub ops: Vec<Mutation>,
}
impl Transaction {
    pub fn new(request_id: impl Into<String>, ops: Vec<Mutation>) -> Self {
        Self {
            request_id: request_id.into(),
            expected_revision: None,
            compares: vec![],
            leases: vec![],
            ops,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Command {
    transaction: Transaction,
    now_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TxnResult {
    pub applied: bool,
    pub revision: u64,
    pub values: BTreeMap<String, Option<Value>>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Lease {
    pub holder: String,
    pub fence: u64,
    pub expires_ms: u64,
    pub revoked: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Change {
    pub revision: u64,
    pub keys: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StateView {
    pub revision: u64,
    pub kv: BTreeMap<String, Value>,
    pub leases: BTreeMap<String, Lease>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WatchResult {
    Events { revision: u64, events: Vec<Change> },
    SnapshotResync(StateView),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevisionedValue {
    pub revision: u64,
    pub value: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeAddress {
    pub addr: SocketAddr,
    pub server_name: String,
    pub certificate_sha256: String,
}
#[derive(Clone)]
pub struct ClusterConfig {
    pub node_id: NodeId,
    pub bind_addr: SocketAddr,
    pub nodes: BTreeMap<NodeId, NodeAddress>,
    pub certificate_der: Vec<u8>,
    pub private_key_der: Vec<u8>,
    pub ca_der: Vec<u8>,
    pub directory: PathBuf,
    /// Host-only envelope key; never serialize or mount into workers.
    pub storage_key: [u8; 32],
    pub rotation_trust_key: Option<[u8; 32]>,
    pub max_frame_bytes: usize,
    pub max_queue_items: usize,
    pub history_limit: usize,
}

impl Drop for ClusterConfig {
    fn drop(&mut self) {
        self.storage_key.zeroize();
        self.private_key_der.zeroize();
    }
}

/// Authority-signed admission of a replacement certificate; private material is host-only.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertificateRotation {
    pub node_id: NodeId,
    pub previous_sha256: String,
    pub certificate_der: Vec<u8>,
    pub admitted_ms: u64,
    pub overlap_until_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CertificateCheckpoint {
    pub node_id: NodeId,
    pub bootstrap_sha256: String,
    pub previous_history_sha256: String,
    pub sequence: u64,
    pub certificate_der: Vec<u8>,
    pub previous_sha256: String,
    pub admitted_ms: u64,
    pub overlap_until_ms: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CertificateHistory {
    checkpoint: Option<crate::audit::Signed<CertificateCheckpoint>>,
    rotations: Vec<crate::audit::Signed<CertificateRotation>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineStartupRequest {
    pub node_id: NodeId,
    pub inventory_sha256: String,
    pub membership_sha256: String,
    pub local_state_sha256: String,
    pub prior_epoch: u64,
    pub last_applied_index: u64,
    pub inventory: BTreeMap<NodeId, NodeAddress>,
    pub histories: BTreeMap<NodeId, Option<Value>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineStartupAdmission {
    pub inventory_sha256: String,
    pub membership_sha256: String,
    pub local_state_sha256: BTreeMap<NodeId, String>,
    pub prior_epoch: u64,
    pub epoch: u64,
    pub issued_ms: u64,
    pub expires_ms: u64,
    pub checkpoints: BTreeMap<NodeId, crate::audit::Signed<CertificateCheckpoint>>,
}
fn digest_json<T: Serialize>(value: &T) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}
/// The operator supplies fresh CA-issued leaves and the explicitly configured admission signer.
/// Public requests contain hashes/admissions only; no Board bodies, private keys or storage keys.
pub fn assemble_offline_startup(
    requests: &[OfflineStartupRequest],
    leaves: BTreeMap<NodeId, Vec<u8>>,
    epoch: u64,
    expires_ms: u64,
    signer: &crate::audit::CryptoSigner,
) -> Result<crate::audit::Signed<OfflineStartupAdmission>> {
    let reference = requests
        .iter()
        .max_by_key(|request| request.last_applied_index)
        .context("offline requests absent")?;
    let now = now_ms();
    ensure!(
        expires_ms > now && expires_ms - now <= 300_000 && epoch > reference.prior_epoch,
        "offline admission bounds/epoch invalid"
    );
    let expected: BTreeSet<_> = reference.inventory.keys().copied().collect();
    ensure!(
        requests.len() == expected.len()
            && leaves.keys().copied().collect::<BTreeSet<_>>() == expected,
        "offline admission requires exact complete inventory"
    );
    let mut state_hashes = BTreeMap::new();
    for request in requests {
        ensure!(
            request.inventory_sha256 == reference.inventory_sha256
                && request.membership_sha256 == reference.membership_sha256
                && request.prior_epoch <= reference.prior_epoch
                && expected.contains(&request.node_id)
                && state_hashes
                    .insert(request.node_id, request.local_state_sha256.clone())
                    .is_none(),
            "offline requests differ/duplicate or membership is stale"
        );
    }
    let mut checkpoints = BTreeMap::new();
    for (id, leaf) in leaves {
        let before = reference
            .histories
            .get(&id)
            .context("offline source history absent")?;
        let history = before
            .as_ref()
            .map(CertificateHistory::parse)
            .transpose()?
            .unwrap_or_default();
        let previous = history
            .leaf()
            .map(|leaf| format!("{:x}", Sha256::digest(leaf)))
            .unwrap_or_else(|| reference.inventory[&id].certificate_sha256.clone());
        let sequence = history
            .checkpoint
            .as_ref()
            .map_or(0, |anchor| anchor.payload.sequence)
            .checked_add(history.rotations.len() as u64)
            .and_then(|number| number.checked_add(1))
            .context("offline certificate epoch overflow")?;
        checkpoints.insert(
            id,
            signer.sign(
                "node-certificate-checkpoint",
                CertificateCheckpoint {
                    node_id: id,
                    bootstrap_sha256: reference.inventory[&id].certificate_sha256.clone(),
                    previous_history_sha256: digest_json(before)?,
                    sequence,
                    certificate_der: leaf,
                    previous_sha256: previous,
                    admitted_ms: now,
                    overlap_until_ms: now,
                },
            )?,
        );
    }
    signer.sign(
        "cluster-offline-startup",
        OfflineStartupAdmission {
            inventory_sha256: reference.inventory_sha256.clone(),
            membership_sha256: reference.membership_sha256.clone(),
            local_state_sha256: state_hashes,
            prior_epoch: reference.prior_epoch,
            epoch,
            issued_ms: now,
            expires_ms,
            checkpoints,
        },
    )
}
fn validate_offline_leaf(
    bytes: &[u8],
    ca: &[u8],
    node: &NodeAddress,
    expires_ms: u64,
) -> Result<()> {
    use x509_parser::extensions::GeneralName;
    let (_, leaf) = x509_parser::parse_x509_certificate(bytes)
        .map_err(|_| anyhow::anyhow!("invalid offline leaf"))?;
    let sans = leaf
        .subject_alternative_name()?
        .context("offline SVID absent")?;
    let uris = sans
        .value
        .general_names
        .iter()
        .filter_map(|name| {
            if let GeneralName::URI(uri) = name {
                Some(*uri)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    ensure!(
        uris.len() == 1,
        "offline certificate requires one SPIFFE identity"
    );
    let workload = crate::identity::WorkloadId::parse(uris[0])?;
    ensure!(
        workload.agent == "cluster" && format!("node-{}", workload.node) == node.server_name,
        "offline SVID outside exact cluster node"
    );
    crate::identity::verify_workload_certificate(bytes, ca, &workload, now_ms() / 1000)?;
    ensure!(
        (leaf.validity().not_after.timestamp() as u64) * 1000 >= expires_ms,
        "offline leaf does not cover recovery window"
    );
    Ok(())
}
fn offline_validate(
    config: &ClusterConfig,
    data: &StateData,
    admission: &crate::audit::Signed<OfflineStartupAdmission>,
) -> Result<()> {
    let trust = config
        .rotation_trust_key
        .as_ref()
        .context("offline authority undeclared")?;
    admission.verify("cluster-offline-startup", trust)?;
    let value = &admission.payload;
    let expected = config.nodes.keys().copied().collect::<BTreeSet<_>>();
    let now = now_ms();
    ensure!(
        value.issued_ms <= now
            && value.expires_ms > now
            && value.expires_ms - value.issued_ms <= 300_000
            && value.inventory_sha256 == digest_json(&config.nodes)?
            && value.membership_sha256 == digest_json(data.membership.membership())?
            && value
                .local_state_sha256
                .keys()
                .copied()
                .collect::<BTreeSet<_>>()
                == expected
            && value.checkpoints.keys().copied().collect::<BTreeSet<_>>() == expected
            && value.local_state_sha256[&config.node_id] == digest_json(data)?
            && value.epoch
                > data
                    .view
                    .kv
                    .get("cluster/offline_epoch")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
        "offline startup artifact stale or outside exact durable inventory/state"
    );
    ensure!(
        value.epoch > value.prior_epoch
            && value.prior_epoch
                >= data
                    .view
                    .kv
                    .get("cluster/offline_epoch")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
        "offline recovery epoch rollback"
    );
    ensure!(
        data.membership.membership().nodes().next().is_some(),
        "offline recovery cannot initialize a new membership"
    );
    for (id, checkpoint) in &value.checkpoints {
        checkpoint.verify("node-certificate-checkpoint", trust)?;
        ensure!(
            checkpoint.payload.node_id == *id
                && checkpoint.payload.bootstrap_sha256 == config.nodes[id].certificate_sha256,
            "offline checkpoint node mismatch"
        );
        let history = serde_json::to_value(CertificateHistory {
            checkpoint: Some(checkpoint.clone()),
            rotations: vec![],
        })?;
        admitted_pins(&config.nodes[id], *id, Some(&history), Some(trust))?;
        validate_offline_leaf(
            &checkpoint.payload.certificate_der,
            &config.ca_der,
            &config.nodes[id],
            value.expires_ms,
        )?;
    }
    ensure!(
        value.checkpoints[&config.node_id].payload.certificate_der == config.certificate_der,
        "own offline leaf not installed"
    );
    Ok(())
}
impl CertificateHistory {
    fn parse(value: &Value) -> Result<Self> {
        if value.is_array() {
            Ok(Self {
                checkpoint: None,
                rotations: serde_json::from_value(value.clone())?,
            })
        } else {
            Ok(serde_json::from_value(value.clone())?)
        }
    }
    fn leaf(&self) -> Option<&[u8]> {
        self.rotations
            .last()
            .map(|last| last.payload.certificate_der.as_slice())
            .or_else(|| {
                self.checkpoint
                    .as_ref()
                    .map(|anchor| anchor.payload.certificate_der.as_slice())
            })
    }
}
fn certificate_valid(bytes: &[u8]) -> Result<()> {
    use x509_parser::prelude::FromDer;
    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(bytes)
        .map_err(|_| anyhow::anyhow!("invalid peer certificate"))?;
    ensure!(
        certificate.validity().is_valid(),
        "peer certificate expired or not yet valid"
    );
    Ok(())
}
fn admitted_pins(
    node: &NodeAddress,
    node_id: NodeId,
    value: Option<&Value>,
    trust: Option<&[u8; 32]>,
) -> Result<BTreeSet<String>> {
    let mut current = node.certificate_sha256.clone();
    let mut accepted = BTreeSet::new();
    if let Some(value) = value {
        let history = CertificateHistory::parse(value)?;
        ensure!(
            history.rotations.len() <= 64,
            "certificate admission history exceeds bound"
        );
        let trust = trust.context("certificate rotation authority not configured")?;
        let mut previous_time = 0;
        if let Some(checkpoint) = history.checkpoint {
            checkpoint.verify("node-certificate-checkpoint", trust)?;
            let anchor = checkpoint.payload;
            ensure!(
                anchor.node_id == node_id
                    && anchor.bootstrap_sha256 == current
                    && anchor.sequence > 0
                    && anchor.admitted_ms <= now_ms()
                    && anchor.overlap_until_ms >= anchor.admitted_ms
                    && anchor.overlap_until_ms - anchor.admitted_ms <= 300_000
                    && !anchor.certificate_der.is_empty()
                    && anchor.certificate_der.len() <= 16384,
                "invalid certificate checkpoint"
            );
            if now_ms() < anchor.overlap_until_ms {
                accepted.insert(anchor.previous_sha256);
            }
            current = format!("{:x}", Sha256::digest(anchor.certificate_der));
            previous_time = anchor.admitted_ms;
        }
        for rotation in history.rotations {
            rotation.verify("node-certificate-rotation", trust)?;
            let next = rotation.payload;
            ensure!(
                next.node_id == node_id
                    && next.previous_sha256 == current
                    && next.admitted_ms >= previous_time
                    && next.admitted_ms <= now_ms()
                    && next.overlap_until_ms >= next.admitted_ms
                    && next.overlap_until_ms - next.admitted_ms <= 300_000,
                "invalid certificate rotation chain"
            );
            ensure!(
                !next.certificate_der.is_empty() && next.certificate_der.len() <= 16384,
                "certificate ingress bound"
            );
            accepted.clear();
            if now_ms() < next.overlap_until_ms {
                accepted.insert(current.clone());
            }
            current = format!("{:x}", Sha256::digest(&next.certificate_der));
            previous_time = next.admitted_ms;
        }
    }
    accepted.insert(current);
    Ok(accepted)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct LogData {
    vote: Option<Vote<NodeId>>,
    committed: Option<LogId<NodeId>>,
    purged: Option<LogId<NodeId>>,
    entries: BTreeMap<u64, Entry<TypeConfig>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
enum LogRecord {
    Vote(Vote<NodeId>),
    Committed(Option<LogId<NodeId>>),
    Append(Vec<Entry<TypeConfig>>),
    Truncate(LogId<NodeId>),
    Purge(LogId<NodeId>),
    Checkpoint(LogData),
    IndexCheckpoint(LogIndex),
}
/// Payloads live in independently authenticated encrypted files, never in the retained log index.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct LogIndex {
    vote: Option<Vote<NodeId>>,
    committed: Option<LogId<NodeId>>,
    purged: Option<LogId<NodeId>>,
    entries: BTreeMap<u64, LogId<NodeId>>,
}
impl LogIndex {
    fn apply(&mut self, record: LogRecord) {
        match record {
            LogRecord::Vote(value) => self.vote = Some(value),
            LogRecord::Committed(value) => self.committed = value,
            LogRecord::Append(entries) => {
                for entry in entries {
                    self.entries.insert(entry.log_id.index, entry.log_id);
                }
            }
            LogRecord::Truncate(id) => {
                self.entries.split_off(&id.index);
            }
            LogRecord::Purge(id) => {
                self.entries = self.entries.split_off(&(id.index + 1));
                self.purged = Some(id);
            }
            LogRecord::Checkpoint(value) => {
                self.vote = value.vote;
                self.committed = value.committed;
                self.purged = value.purged;
                self.entries = value
                    .entries
                    .into_iter()
                    .map(|(index, entry)| (index, entry.log_id))
                    .collect();
            }
            LogRecord::IndexCheckpoint(value) => *self = value,
        }
    }
}
fn storage_error(error: impl std::fmt::Display) -> StorageError<NodeId> {
    StorageIOError::write_state_machine(&std::io::Error::other(error.to_string())).into()
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    File::open(path.parent().context("missing storage parent")?)?.sync_all()?;
    Ok(())
}
const SEALED_MAGIC: &[u8] = b"solos-sealed-v1\0";
fn seal(key: &[u8; 32], domain: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).map_err(|_| anyhow::anyhow!("storage nonce generation failed"))?;
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("invalid storage key"))?;
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: domain,
            },
        )
        .map_err(|_| anyhow::anyhow!("storage encryption failed"))?;
    Ok([SEALED_MAGIC, &nonce, &ciphertext].concat())
}
fn unseal(key: &[u8; 32], domain: &[u8], bytes: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        bytes.starts_with(SEALED_MAGIC) && bytes.len() >= SEALED_MAGIC.len() + 12 + 16,
        "sealed storage header invalid"
    );
    let offset = SEALED_MAGIC.len();
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("invalid storage key"))?;
    cipher
        .decrypt(
            Nonce::from_slice(&bytes[offset..offset + 12]),
            Payload {
                msg: &bytes[offset + 12..],
                aad: domain,
            },
        )
        .map_err(|_| anyhow::anyhow!("sealed storage authentication failed"))
}
fn sealed_write(path: &Path, key: &[u8; 32], domain: &[u8], bytes: &[u8]) -> Result<()> {
    atomic_write(path, &seal(key, domain, bytes)?)
}
fn record_bytes(key: &[u8; 32], record: &LogRecord) -> Result<Vec<u8>> {
    let body = seal(key, b"raft-wal", &serde_json::to_vec(record)?)?;
    ensure!(
        body.len() <= MAX_STORAGE_RECORD_BYTES,
        "WAL record exceeds bound"
    );
    let mut bytes = Vec::with_capacity(body.len() + 36);
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(&Sha256::digest(&body));
    Ok(bytes)
}
#[derive(Clone)]
struct DurableLog {
    path: PathBuf,
    storage_key: [u8; 32],
    data: Arc<Mutex<LogIndex>>,
}
impl Drop for DurableLog {
    fn drop(&mut self) {
        self.storage_key.zeroize();
    }
}

impl DurableLog {
    fn open(directory: &Path, storage_key: [u8; 32]) -> Result<Self> {
        let path = directory.join("raft.wal");
        let mut data = LogIndex::default();
        if path.exists() {
            let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
            let mut valid_length = 0;
            loop {
                let mut length = [0; 4];
                match file.read_exact(&mut length) {
                    Ok(()) => (),
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(error) => return Err(error.into()),
                }
                let length = u32::from_be_bytes(length) as usize;
                ensure!(
                    length <= MAX_STORAGE_RECORD_BYTES,
                    "WAL input exceeds bound"
                );
                let mut body = vec![0; length];
                let mut hash = [0; 32];
                if file.read_exact(&mut body).is_err() || file.read_exact(&mut hash).is_err() {
                    break;
                }
                ensure!(
                    Sha256::digest(&body).as_slice() == hash,
                    "WAL checksum corruption"
                );
                let record: LogRecord =
                    serde_json::from_slice(&unseal(&storage_key, b"raft-wal", &body)?)?;
                Self::store_record_entries(&path, &storage_key, &record)?;
                data.apply(record);
                valid_length += 4 + length as u64 + 32;
            }
            // An incomplete final write was never acknowledged; discard only that tail.
            file.set_len(valid_length)?;
            file.sync_all()?;
        }
        let mut previous = data.purged.map(|id| id.index);
        for index in data.entries.keys() {
            if let Some(last) = previous {
                ensure!(*index == last + 1, "Raft WAL has a log hole");
            }
            previous = Some(*index);
        }
        for id in data.entries.values() {
            Self::read_entry(&path, &storage_key, *id)?;
        }
        let log = Self {
            path,
            storage_key,
            data: Arc::new(Mutex::new(data)),
        };
        Ok(log)
    }
    fn entry_path(path: &Path, id: LogId<NodeId>) -> Result<PathBuf> {
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&id)?));
        Ok(path
            .with_extension("entries")
            .join(format!("{}-{digest}.sealed", id.index)))
    }
    fn entry_domain(id: LogId<NodeId>) -> Result<Vec<u8>> {
        Ok([b"raft-entry:".as_slice(), &serde_json::to_vec(&id)?].concat())
    }
    fn read_entry(path: &Path, key: &[u8; 32], id: LogId<NodeId>) -> Result<Entry<TypeConfig>> {
        let file = Self::entry_path(path, id)?;
        ensure!(
            fs::metadata(&file)?.len() <= MAX_STORAGE_RECORD_BYTES as u64,
            "encrypted log entry exceeds bound"
        );
        let entry: Entry<TypeConfig> =
            serde_json::from_slice(&unseal(key, &Self::entry_domain(id)?, &fs::read(file)?)?)?;
        ensure!(entry.log_id == id, "encrypted log entry identity mismatch");
        Ok(entry)
    }
    fn store_record_entries(path: &Path, key: &[u8; 32], record: &LogRecord) -> Result<()> {
        let entries: Vec<&Entry<TypeConfig>> = match record {
            LogRecord::Append(entries) => entries.iter().collect(),
            LogRecord::Checkpoint(data) => data.entries.values().collect(),
            _ => return Ok(()),
        };
        let directory = path.with_extension("entries");
        fs::create_dir_all(&directory)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        for entry in entries {
            let file = Self::entry_path(path, entry.log_id)?;
            let bytes = serde_json::to_vec(entry)?;
            ensure!(
                bytes.len() <= MAX_STORAGE_RECORD_BYTES,
                "log entry exceeds storage bound"
            );
            if file.exists() {
                ensure!(
                    unseal(key, &Self::entry_domain(entry.log_id)?, &fs::read(&file)?)? == bytes,
                    "log ID reused for differing payload"
                );
            } else {
                sealed_write(&file, key, &Self::entry_domain(entry.log_id)?, &bytes)?;
            }
        }
        Ok(())
    }
    async fn persist(&self, record: LogRecord) -> Result<()> {
        let mut data = self.data.lock().await;
        Self::store_record_entries(&self.path, &self.storage_key, &record)?;
        let bytes = record_bytes(&self.storage_key, &record)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        File::open(self.path.parent().context("missing WAL parent")?)?.sync_all()?;
        data.apply(record);
        Ok(())
    }
}
impl openraft::RaftLogReader<TypeConfig> for DurableLog {
    async fn try_get_log_entries<R: RangeBounds<u64> + Clone + Send>(
        &mut self,
        range: R,
    ) -> std::result::Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
        let data = self.data.lock().await;
        let mut entries = Vec::new();
        for (_, id) in data.entries.range(range) {
            entries
                .push(Self::read_entry(&self.path, &self.storage_key, *id).map_err(storage_error)?);
        }
        Ok(entries)
    }
}
impl RaftLogStorage<TypeConfig> for DurableLog {
    type LogReader = Self;
    async fn get_log_state(
        &mut self,
    ) -> std::result::Result<openraft::LogState<TypeConfig>, StorageError<NodeId>> {
        let data = self.data.lock().await;
        Ok(openraft::LogState {
            last_purged_log_id: data.purged,
            last_log_id: data
                .entries
                .last_key_value()
                .map(|(_, id)| *id)
                .or(data.purged),
        })
    }
    async fn get_log_reader(&mut self) -> Self {
        self.clone()
    }
    async fn save_vote(
        &mut self,
        vote: &Vote<NodeId>,
    ) -> std::result::Result<(), StorageError<NodeId>> {
        self.persist(LogRecord::Vote(*vote))
            .await
            .map_err(storage_error)
    }
    async fn read_vote(
        &mut self,
    ) -> std::result::Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        Ok(self.data.lock().await.vote)
    }
    async fn save_committed(
        &mut self,
        id: Option<LogId<NodeId>>,
    ) -> std::result::Result<(), StorageError<NodeId>> {
        self.persist(LogRecord::Committed(id))
            .await
            .map_err(storage_error)
    }
    async fn read_committed(
        &mut self,
    ) -> std::result::Result<Option<LogId<NodeId>>, StorageError<NodeId>> {
        Ok(self.data.lock().await.committed)
    }
    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> std::result::Result<(), StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let result = self
            .persist(LogRecord::Append(entries.into_iter().collect()))
            .await;
        match result {
            Ok(()) => {
                callback.log_io_completed(Ok(()));
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                callback.log_io_completed(Err(std::io::Error::other(message.clone())));
                Err(storage_error(message))
            }
        }
    }
    async fn truncate(
        &mut self,
        id: LogId<NodeId>,
    ) -> std::result::Result<(), StorageError<NodeId>> {
        self.persist(LogRecord::Truncate(id))
            .await
            .map_err(storage_error)
    }
    async fn purge(&mut self, id: LogId<NodeId>) -> std::result::Result<(), StorageError<NodeId>> {
        self.persist(LogRecord::Purge(id))
            .await
            .map_err(storage_error)?;
        let data = self.data.lock().await;
        atomic_write(
            &self.path,
            &record_bytes(&self.storage_key, &LogRecord::IndexCheckpoint(data.clone()))
                .map_err(storage_error)?,
        )
        .map_err(storage_error)?;
        let retained = data
            .entries
            .values()
            .map(|id| Self::entry_path(&self.path, *id))
            .collect::<Result<BTreeSet<_>>>()
            .map_err(storage_error)?;
        let directory = self.path.with_extension("entries");
        if directory.exists() {
            for entry in fs::read_dir(&directory).map_err(storage_error)? {
                let path = entry.map_err(storage_error)?.path();
                if !retained.contains(&path) {
                    fs::remove_file(path).map_err(storage_error)?;
                }
            }
            File::open(directory)
                .map_err(storage_error)?
                .sync_all()
                .map_err(storage_error)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StateData {
    last_applied: Option<LogId<NodeId>>,
    membership: StoredMembership<NodeId, BasicNode>,
    view: StateView,
    history: VecDeque<Change>,
    results: BTreeMap<String, (String, TxnResult)>,
}
impl StateData {
    fn apply_transaction(&mut self, command: &Command, history_limit: usize) -> TxnResult {
        let txn = &command.transaction;
        let hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(txn).expect("serializable transaction"))
        );
        let fail = |reason: &str, revision| TxnResult {
            applied: false,
            revision,
            values: BTreeMap::new(),
            error: Some(reason.into()),
        };
        if let Some((previous, result)) = self.results.get(&txn.request_id) {
            return if previous == &hash {
                result.clone()
            } else {
                fail(
                    "request ID reused for different transaction",
                    self.view.revision,
                )
            };
        }
        if txn
            .expected_revision
            .is_some_and(|revision| revision != self.view.revision)
            || txn
                .compares
                .iter()
                .any(|compare| self.view.kv.get(&compare.key) != compare.expected.as_ref())
        {
            return fail("compare-and-swap conflict", self.view.revision);
        }
        if txn.leases.iter().any(|check| {
            !self.view.leases.get(&check.id).is_some_and(|lease| {
                !lease.revoked
                    && lease.holder == check.holder
                    && lease.fence == check.fence
                    && lease.expires_ms > command.now_ms
            })
        }) {
            return fail("lease expired, revoked or fenced", self.view.revision);
        }
        let mut next = self.view.clone();
        // Compact durable receipts avoid retaining every historical Board/Kernel payload twice.
        // Values are retrieved separately at an authoritative revision through read/view.
        let values = BTreeMap::new();
        let mut keys = BTreeSet::new();
        let revision = next.revision + 1;
        for op in &txn.ops {
            match op {
                Mutation::Put { key, value } => {
                    next.kv.insert(key.clone(), value.clone());
                    keys.insert(key.clone());
                }
                Mutation::Delete { key } => {
                    next.kv.remove(key);
                    keys.insert(key.clone());
                }
                Mutation::GrantLease {
                    id,
                    holder,
                    expires_ms,
                } => {
                    if *expires_ms <= command.now_ms
                        || next.leases.get(id).is_some_and(|lease| {
                            !lease.revoked && lease.expires_ms > command.now_ms
                        })
                    {
                        return fail("lease already held or expiry invalid", self.view.revision);
                    }
                    next.leases.insert(
                        id.clone(),
                        Lease {
                            holder: holder.clone(),
                            fence: revision,
                            expires_ms: *expires_ms,
                            revoked: false,
                        },
                    );
                    keys.insert(format!("lease/{id}"));
                }
                Mutation::RenewLease {
                    id,
                    holder,
                    fence,
                    expires_ms,
                } => {
                    let Some(lease) = next.leases.get_mut(id) else {
                        return fail("unknown lease", self.view.revision);
                    };
                    if lease.revoked
                        || lease.holder != *holder
                        || lease.fence != *fence
                        || lease.expires_ms <= command.now_ms
                        || *expires_ms <= command.now_ms
                    {
                        return fail("lease renewal fenced or expired", self.view.revision);
                    }
                    lease.expires_ms = *expires_ms;
                    keys.insert(format!("lease/{id}"));
                }
                Mutation::RevokeLease { id } => {
                    if let Some(lease) = next.leases.get_mut(id) {
                        lease.revoked = true;
                        lease.fence = revision;
                        keys.insert(format!("lease/{id}"));
                    }
                }
                Mutation::ExpireLeases => {
                    for (id, lease) in &mut next.leases {
                        if !lease.revoked && lease.expires_ms <= command.now_ms {
                            lease.revoked = true;
                            lease.fence = revision;
                            keys.insert(format!("lease/{id}"));
                        }
                    }
                }
            }
        }
        next.revision = revision;
        self.view = next;
        self.history.push_back(Change {
            revision,
            keys: keys.into_iter().collect(),
        });
        while self.history.len() > history_limit {
            self.history.pop_front();
        }
        let result = TxnResult {
            applied: true,
            revision,
            values,
            error: None,
        };
        self.results
            .insert(txn.request_id.clone(), (hash, result.clone()));
        result
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedSnapshot {
    meta: SnapshotMeta<NodeId, BasicNode>,
    data: Vec<u8>,
}
struct Machine {
    directory: PathBuf,
    storage_key: [u8; 32],
    data: RwLock<StateData>,
    snapshot: RwLock<Option<SavedSnapshot>>,
    notify: Notify,
    history_limit: usize,
}
impl Drop for Machine {
    fn drop(&mut self) {
        self.storage_key.zeroize();
    }
}

impl Machine {
    fn open(directory: &Path, history_limit: usize, storage_key: [u8; 32]) -> Result<Arc<Self>> {
        let read = |name: &str| -> Result<Option<Vec<u8>>> {
            let path = directory.join(name);
            if path.exists() {
                Ok(Some(unseal(
                    &storage_key,
                    name.as_bytes(),
                    &fs::read(path)?,
                )?))
            } else {
                Ok(None)
            }
        };
        let mut data: StateData = read("state.json")?
            .map(|bytes| serde_json::from_slice(&bytes))
            .transpose()?
            .unwrap_or_default();
        let snapshot: Option<SavedSnapshot> = read("snapshot.json")?
            .map(|bytes| serde_json::from_slice(&bytes))
            .transpose()?;
        if let Some(saved) = &snapshot
            && saved.meta.last_log_id.map(|id| id.index) > data.last_applied.map(|id| id.index)
        {
            data = serde_json::from_slice(&saved.data)?;
        }
        Ok(Arc::new(Self {
            directory: directory.into(),
            storage_key,
            data: RwLock::new(data),
            snapshot: RwLock::new(snapshot),
            notify: Notify::new(),
            history_limit,
        }))
    }
}
impl openraft::RaftSnapshotBuilder<TypeConfig> for Arc<Machine> {
    async fn build_snapshot(
        &mut self,
    ) -> std::result::Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        let data = self.data.read().await;
        let bytes = serde_json::to_vec(&*data).map_err(storage_error)?;
        if bytes.len() > MAX_STORAGE_RECORD_BYTES {
            return Err(storage_error(
                "snapshot exceeds bounded in-memory snapshot capacity",
            ));
        }
        let meta = SnapshotMeta {
            last_log_id: data.last_applied,
            last_membership: data.membership.clone(),
            snapshot_id: format!("{}-{}", data.view.revision, now_ms()),
        };
        let snapshot = SavedSnapshot {
            meta: meta.clone(),
            data: bytes.clone(),
        };
        sealed_write(
            &self.directory.join("snapshot.json"),
            &self.storage_key,
            b"snapshot.json",
            &serde_json::to_vec(&snapshot).map_err(storage_error)?,
        )
        .map_err(storage_error)?;
        *self.snapshot.write().await = Some(snapshot);
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(bytes)),
        })
    }
}
impl RaftStateMachine<TypeConfig> for Arc<Machine> {
    type SnapshotBuilder = Self;
    async fn applied_state(
        &mut self,
    ) -> std::result::Result<
        (Option<LogId<NodeId>>, StoredMembership<NodeId, BasicNode>),
        StorageError<NodeId>,
    > {
        let data = self.data.read().await;
        Ok((data.last_applied, data.membership.clone()))
    }
    async fn apply<I>(
        &mut self,
        entries: I,
    ) -> std::result::Result<Vec<TxnResult>, StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let mut data = self.data.write().await;
        let mut next = data.clone();
        let mut results = vec![];
        for entry in entries {
            next.last_applied = Some(entry.log_id);
            results.push(match entry.payload {
                EntryPayload::Normal(command) => {
                    next.apply_transaction(&command, self.history_limit)
                }
                EntryPayload::Membership(membership) => {
                    next.membership = StoredMembership::new(Some(entry.log_id), membership);
                    TxnResult {
                        applied: true,
                        revision: next.view.revision,
                        values: BTreeMap::new(),
                        error: None,
                    }
                }
                EntryPayload::Blank => TxnResult {
                    applied: true,
                    revision: next.view.revision,
                    values: BTreeMap::new(),
                    error: None,
                },
            });
        }
        sealed_write(
            &self.directory.join("state.json"),
            &self.storage_key,
            b"state.json",
            &serde_json::to_vec(&next).map_err(storage_error)?,
        )
        .map_err(storage_error)?;
        *data = next;
        self.notify.notify_waiters();
        Ok(results)
    }
    async fn get_snapshot_builder(&mut self) -> Self {
        self.clone()
    }
    async fn begin_receiving_snapshot(
        &mut self,
    ) -> std::result::Result<Box<Cursor<Vec<u8>>>, StorageError<NodeId>> {
        Ok(Box::new(Cursor::new(vec![])))
    }
    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> std::result::Result<(), StorageError<NodeId>> {
        if snapshot.get_ref().len() > MAX_STORAGE_RECORD_BYTES {
            return Err(storage_error("snapshot exceeds bound"));
        }
        let mut next: StateData =
            serde_json::from_slice(snapshot.get_ref()).map_err(storage_error)?;
        next.last_applied = meta.last_log_id;
        next.membership = meta.last_membership.clone();
        let mut data = self.data.write().await;
        if next.view.revision < data.view.revision {
            return Err(storage_error(
                "snapshot would regress authoritative revision",
            ));
        }
        let saved = SavedSnapshot {
            meta: meta.clone(),
            data: serde_json::to_vec(&next).map_err(storage_error)?,
        };
        sealed_write(
            &self.directory.join("snapshot.json"),
            &self.storage_key,
            b"snapshot.json",
            &serde_json::to_vec(&saved).map_err(storage_error)?,
        )
        .map_err(storage_error)?;
        sealed_write(
            &self.directory.join("state.json"),
            &self.storage_key,
            b"state.json",
            &saved.data,
        )
        .map_err(storage_error)?;
        *data = next;
        *self.snapshot.write().await = Some(saved);
        self.notify.notify_waiters();
        Ok(())
    }
    async fn get_current_snapshot(
        &mut self,
    ) -> std::result::Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
        Ok(self.snapshot.read().await.as_ref().map(|saved| Snapshot {
            meta: saved.meta.clone(),
            snapshot: Box::new(Cursor::new(saved.data.clone())),
        }))
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum Rpc {
    Append(AppendEntriesRequest<TypeConfig>),
    Vote(VoteRequest<NodeId>),
    Snapshot(InstallSnapshotRequest<TypeConfig>),
    Transaction(Command),
    ControlTransaction(Command),
    TransactionResult(String),
    LeaseCurrent {
        id: String,
        holder: String,
        fence: u64,
    },
    Read(String),
    ControlRead(String),
    View,
    ControlView,
    Watch(u64),
    Ping(Vec<u8>),
    RotationReady {
        key: String,
        digest: String,
    },
}
impl Rpc {
    // Distinct QUIC connections keep bulk stream saturation away from Raft and stop/lease traffic.
    fn channel(&self) -> u8 {
        match self {
            Self::Append(_) | Self::Vote(_) | Self::Snapshot(_) => 0,
            Self::ControlTransaction(_)
            | Self::ControlRead(_)
            | Self::ControlView
            | Self::LeaseCurrent { .. }
            | Self::RotationReady { .. } => 1,
            _ => 2,
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
struct Wire {
    sender: NodeId,
    rpc: Rpc,
}
#[derive(Clone)]
struct Transport {
    endpoint: Endpoint,
    client_config: Arc<RwLock<ClientConfig>>,
    node_id: NodeId,
    nodes: Arc<RwLock<BTreeMap<NodeId, NodeAddress>>>,
    connections: Arc<Mutex<BTreeMap<(NodeId, u8), Connection>>>,
    connect_locks: BTreeMap<(NodeId, u8), Arc<Mutex<()>>>,
    max_frame_bytes: usize,
    machine: Arc<Machine>,
    rotation_trust_key: Option<[u8; 32]>,
    startup: Option<crate::audit::Signed<OfflineStartupAdmission>>,
}
fn peer_hash(connection: &Connection) -> Result<String> {
    let identity = connection
        .peer_identity()
        .context("QUIC peer has no certificate identity")?;
    let certificates = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| anyhow::anyhow!("unsupported QUIC peer identity"))?;
    certificate_valid(
        certificates
            .first()
            .context("empty peer certificate chain")?
            .as_ref(),
    )?;
    Ok(format!(
        "{:x}",
        Sha256::digest(
            certificates
                .first()
                .context("empty peer certificate chain")?
                .as_ref()
        )
    ))
}
async fn write_frame(send: &mut quinn::SendStream, bytes: &[u8], max: usize) -> Result<()> {
    ensure!(bytes.len() <= max, "serialized QUIC frame exceeds limit");
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(bytes).await?;
    send.finish()?;
    Ok(())
}
async fn read_frame(recv: &mut quinn::RecvStream, max: usize) -> Result<Vec<u8>> {
    let mut size = [0; 4];
    recv.read_exact(&mut size).await?;
    let size = u32::from_be_bytes(size) as usize;
    ensure!(size <= max, "QUIC ingress frame exceeds limit");
    let mut data = vec![0; size];
    recv.read_exact(&mut data).await?;
    let mut extra = [0];
    ensure!(
        recv.read(&mut extra).await?.is_none(),
        "trailing QUIC frame bytes"
    );
    Ok(data)
}
impl Transport {
    async fn peer_allowed(&self, node_id: NodeId, connection: &Connection) -> Result<()> {
        let nodes = self.nodes.read().await;
        let node = nodes.get(&node_id).context("peer outside inventory")?;
        let data = self.machine.data.read().await;
        let membership = data.membership.membership();
        ensure!(
            membership.nodes().next().is_none() || membership.get_node(&node_id).is_some(),
            "peer removed from authoritative membership"
        );
        let mut accepted = admitted_pins(
            node,
            node_id,
            data.view.kv.get(&format!("cluster/certificates/{node_id}")),
            self.rotation_trust_key.as_ref(),
        )?;
        if let Some(startup) = &self.startup
            && now_ms() < startup.payload.expires_ms
        {
            accepted.insert(format!(
                "{:x}",
                Sha256::digest(
                    &startup.payload.checkpoints[&node_id]
                        .payload
                        .certificate_der
                )
            ));
        }
        ensure!(
            accepted.contains(&peer_hash(connection)?),
            "peer certificate outside authoritative admission"
        );
        Ok(())
    }
    async fn call(&self, node_id: NodeId, rpc: Rpc) -> Result<Value> {
        let cache_key = (node_id, rpc.channel());
        let connect_lock = self
            .connect_locks
            .get(&cache_key)
            .context("connection outside inventory")?;
        let connecting = connect_lock.lock().await;
        let node = self
            .nodes
            .read()
            .await
            .get(&node_id)
            .cloned()
            .context("node absent from admitted inventory")?;
        let existing = self
            .connections
            .lock()
            .await
            .get(&cache_key)
            .filter(|connection| connection.close_reason().is_none())
            .cloned();
        let existing = if let Some(connection) = existing {
            if self.peer_allowed(node_id, &connection).await.is_ok() {
                Some(connection)
            } else {
                connection.close(0_u32.into(), b"certificate admission expired");
                self.connections.lock().await.remove(&cache_key);
                None
            }
        } else {
            None
        };
        let connection = if let Some(connection) = existing {
            connection
        } else {
            let connection = tokio::time::timeout(
                Duration::from_secs(3),
                self.endpoint.connect_with(
                    self.client_config.read().await.clone(),
                    node.addr,
                    &node.server_name,
                )?,
            )
            .await
            .context("QUIC connection deadline")??;
            self.peer_allowed(node_id, &connection).await?;
            self.connections
                .lock()
                .await
                .insert(cache_key, connection.clone());
            connection
        };
        drop(connecting);
        self.peer_allowed(node_id, &connection).await?;
        let body = serde_json::to_vec(&Wire {
            sender: self.node_id,
            rpc,
        })?;
        let exchange = async {
            let (mut send, mut recv) = connection.open_bi().await?;
            write_frame(&mut send, &body, self.max_frame_bytes).await?;
            let result: std::result::Result<Value, String> =
                serde_json::from_slice(&read_frame(&mut recv, self.max_frame_bytes).await?)?;
            result.map_err(anyhow::Error::msg)
        };
        tokio::time::timeout(Duration::from_secs(5), exchange)
            .await
            .context("QUIC operation deadline")?
    }
}
#[derive(Clone)]
struct Network(Transport);
struct NetworkPeer {
    transport: Transport,
    id: NodeId,
}
impl RaftNetworkFactory<TypeConfig> for Network {
    type Network = NetworkPeer;
    async fn new_client(&mut self, target: NodeId, _node: &BasicNode) -> NetworkPeer {
        NetworkPeer {
            transport: self.0.clone(),
            id: target,
        }
    }
}
impl NetworkPeer {
    async fn rpc<Resp: DeserializeOwned, E: std::error::Error + DeserializeOwned>(
        &self,
        rpc: Rpc,
    ) -> std::result::Result<Resp, RPCError<NodeId, BasicNode, E>> {
        let value = self.transport.call(self.id, rpc).await.map_err(|error| {
            RPCError::Network(NetworkError::new(&std::io::Error::other(error.to_string())))
        })?;
        let result: std::result::Result<Resp, E> = serde_json::from_value(value)
            .map_err(|error| RPCError::Network(NetworkError::new(&error)))?;
        result.map_err(|error| RPCError::RemoteError(RemoteError::new(self.id, error)))
    }
}
impl RaftNetwork<TypeConfig> for NetworkPeer {
    async fn append_entries(
        &mut self,
        req: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> std::result::Result<AppendEntriesResponse<NodeId>, RpcError> {
        self.rpc(Rpc::Append(req)).await
    }
    async fn vote(
        &mut self,
        req: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> std::result::Result<VoteResponse<NodeId>, RpcError> {
        self.rpc(Rpc::Vote(req)).await
    }
    async fn install_snapshot(
        &mut self,
        req: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> std::result::Result<InstallSnapshotResponse<NodeId>, RpcError<InstallSnapshotError>> {
        self.rpc(Rpc::Snapshot(req)).await
    }
}
fn tls(config: &ClusterConfig) -> Result<(ServerConfig, ClientConfig)> {
    let cert = CertificateDer::from(config.certificate_der.clone());
    let key = || PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(config.private_key_der.clone()));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(config.ca_der.clone()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots.clone()),
        provider.clone(),
    )
    .build()?;
    let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![cert.clone()], key())?;
    server.alpn_protocols = vec![b"solos-swarm-v1".to_vec()];
    server.max_early_data_size = 0;
    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_client_auth_cert(vec![cert], key())?;
    client.alpn_protocols = vec![b"solos-swarm-v1".to_vec()];
    client.enable_early_data = false;
    let mut server = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server)?,
    ));
    server.max_incoming(config.max_queue_items);
    server.incoming_buffer_size(config.max_frame_bytes as u64);
    server.incoming_buffer_size_total(
        config
            .max_frame_bytes
            .saturating_mul(config.max_queue_items) as u64,
    );
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams((config.max_queue_items as u32).into());
    transport.max_concurrent_uni_streams(0_u32.into());
    transport.datagram_receive_buffer_size(None);
    transport.stream_receive_window((config.max_frame_bytes as u32).into());
    transport.receive_window(
        (config
            .max_frame_bytes
            .saturating_mul(config.max_queue_items)
            .min(u32::MAX as usize) as u32)
            .into(),
    );
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into()?));
    server.transport_config(Arc::new(transport));
    Ok((
        server,
        ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(
            client,
        )?)),
    ))
}
/// Local state writers retain quorum CAS checks while avoiding repeated snapshot races.
/// Controls bypass the normal queue; at most one queued normal writer can precede them.
pub(crate) struct KernelWriteGuard<'a> {
    _writer: tokio::sync::MutexGuard<'a, ()>,
    _normal: Option<tokio::sync::MutexGuard<'a, ()>>,
}
pub struct Cluster {
    pub node_id: NodeId,
    raft: Raft,
    machine: Arc<Machine>,
    transport: Transport,
    limits: ClusterConfig,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    bulk_slots: Arc<Semaphore>,
    control_slots: Arc<Semaphore>,
    kernel_normal_writers: Mutex<()>,
    kernel_writer: Mutex<()>,
    ready: AtomicBool,
}
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
impl Cluster {
    pub(crate) async fn kernel_write(&self, control: bool) -> KernelWriteGuard<'_> {
        let normal = if control {
            None
        } else {
            Some(self.kernel_normal_writers.lock().await)
        };
        let writer = self.kernel_writer.lock().await;
        KernelWriteGuard {
            _writer: writer,
            _normal: normal,
        }
    }
    pub async fn open(config: ClusterConfig) -> Result<Arc<Self>> {
        Self::open_with_startup_admission(config, None).await
    }
    pub async fn offline_startup_request(config: &ClusterConfig) -> Result<OfflineStartupRequest> {
        let machine = Machine::open(&config.directory, config.history_limit, config.storage_key)?;
        let data = machine.data.read().await;
        ensure!(
            data.membership.membership().nodes().next().is_some(),
            "offline request requires existing membership"
        );
        Ok(OfflineStartupRequest {
            node_id: config.node_id,
            inventory_sha256: digest_json(&config.nodes)?,
            membership_sha256: digest_json(data.membership.membership())?,
            local_state_sha256: digest_json(&*data)?,
            prior_epoch: data
                .view
                .kv
                .get("cluster/offline_epoch")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            last_applied_index: data.last_applied.map_or(0, |id| id.index),
            inventory: config.nodes.clone(),
            histories: config
                .nodes
                .keys()
                .map(|id| {
                    (
                        *id,
                        data.view
                            .kv
                            .get(&format!("cluster/certificates/{id}"))
                            .cloned(),
                    )
                })
                .collect(),
        })
    }
    pub async fn open_with_startup_admission(
        config: ClusterConfig,
        startup: Option<crate::audit::Signed<OfflineStartupAdmission>>,
    ) -> Result<Arc<Self>> {
        ensure!(
            config.nodes.contains_key(&config.node_id),
            "own node absent from admitted inventory"
        );
        ensure!(
            (1024..=MAX_FRAME_BYTES).contains(&config.max_frame_bytes)
                && (1..=1024).contains(&config.max_queue_items)
                && config.history_limit > 0,
            "invalid cluster resource limits"
        );
        fs::create_dir_all(&config.directory)?;
        let machine = Machine::open(&config.directory, config.history_limit, config.storage_key)?;
        certificate_valid(&config.certificate_der)?;
        if let Some(admission) = &startup {
            offline_validate(&config, &*machine.data.read().await, admission)?;
        }
        ensure!(
            startup.is_some()
                || admitted_pins(
                    &config.nodes[&config.node_id],
                    config.node_id,
                    machine
                        .data
                        .read()
                        .await
                        .view
                        .kv
                        .get(&format!("cluster/certificates/{}", config.node_id)),
                    config.rotation_trust_key.as_ref()
                )?
                .contains(&format!("{:x}", Sha256::digest(&config.certificate_der))),
            "own certificate outside admission"
        );
        let (server, client) = tls(&config)?;
        let mut endpoint = Endpoint::server(server, config.bind_addr)?;
        endpoint.set_default_client_config(client.clone());
        let transport = Transport {
            endpoint,
            client_config: Arc::new(RwLock::new(client)),
            node_id: config.node_id,
            nodes: Arc::new(RwLock::new(config.nodes.clone())),
            connections: Arc::new(Mutex::new(BTreeMap::new())),
            connect_locks: config
                .nodes
                .keys()
                .flat_map(|id| {
                    (0..3).map(move |channel| ((*id, channel), Arc::new(Mutex::new(()))))
                })
                .collect(),
            max_frame_bytes: config.max_frame_bytes,
            machine: machine.clone(),
            rotation_trust_key: config.rotation_trust_key,
            startup: startup.clone(),
        };
        let raft_config = Arc::new(
            openraft::Config {
                cluster_name: "solos-swarm".into(),
                heartbeat_interval: 200,
                election_timeout_min: 800,
                election_timeout_max: 1400,
                snapshot_policy: openraft::SnapshotPolicy::LogsSinceLast(8),
                max_in_snapshot_log_to_keep: 0,
                max_payload_entries: 1,
                ..Default::default()
            }
            .validate()?,
        );
        let raft = Raft::new(
            config.node_id,
            raft_config,
            Network(transport.clone()),
            DurableLog::open(&config.directory, config.storage_key)?,
            machine.clone(),
        )
        .await?;
        let bulk_slots = Arc::new(Semaphore::new(config.max_queue_items.min(16)));
        let cluster = Arc::new(Self {
            node_id: config.node_id,
            raft,
            machine,
            transport,
            limits: config,
            tasks: Mutex::new(vec![]),
            bulk_slots,
            control_slots: Arc::new(Semaphore::new(4)),
            kernel_normal_writers: Mutex::new(()),
            kernel_writer: Mutex::new(()),
            ready: AtomicBool::new(startup.is_none()),
        });
        let weak = Arc::downgrade(&cluster);
        let endpoint = cluster.transport.endpoint.clone();
        let connection_slots = Arc::new(Semaphore::new(
            cluster
                .limits
                .max_queue_items
                .max(cluster.limits.nodes.len().saturating_mul(3)),
        ));
        let handler = tokio::spawn(async move {
            let mut owned_connections = tokio::task::JoinSet::new();
            loop {
                let incoming = tokio::select! { incoming=endpoint.accept()=>incoming, _=owned_connections.join_next(),if !owned_connections.is_empty()=>continue };
                let Some(incoming) = incoming else { break };
                let Ok(permit) = connection_slots.clone().acquire_owned().await else {
                    break;
                };
                let weak = weak.clone();
                owned_connections.spawn(async move {
                    let _permit = permit; let Ok(Ok(connection)) = tokio::time::timeout(Duration::from_secs(3),incoming).await else { return };
                    let slots = Arc::new(Semaphore::new(32));
                    let mut owned_streams=tokio::task::JoinSet::new();
                    loop {
                        let stream=tokio::select! { stream=connection.accept_bi()=>stream, _=owned_streams.join_next(),if !owned_streams.is_empty()=>continue };
                        let Ok((mut send,mut recv))=stream else {break};
                        let Ok(permit) = slots.clone().acquire_owned().await else { break };
                        let weak = weak.clone(); let connection = connection.clone();
                        owned_streams.spawn(async move {
                            let _permit = permit; let Some(cluster) = weak.upgrade() else { return };
                            let result = tokio::time::timeout(Duration::from_secs(6), cluster.handle(&connection, &mut recv)).await;
                            let result = match result { Ok(value) => value.map_err(|error| error.to_string()), Err(_) => Err("ingress deadline".into()) };
                            if let Ok(bytes) = serde_json::to_vec(&result) { let _ = write_frame(&mut send, &bytes, cluster.limits.max_frame_bytes).await; }
                        });
                    }
                });
            }
        });
        let weak = Arc::downgrade(&cluster);
        let expiry = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                interval.tick().await;
                let Some(cluster) = weak.upgrade() else { break };
                if cluster.raft.metrics().borrow().current_leader == Some(cluster.node_id) {
                    if !cluster.ready.load(Ordering::Acquire) {
                        continue;
                    }
                    let expired = cluster
                        .machine
                        .data
                        .read()
                        .await
                        .view
                        .leases
                        .values()
                        .any(|lease| !lease.revoked && lease.expires_ms <= now_ms());
                    if expired && cluster.raft.ensure_linearizable().await.is_ok() {
                        let transaction = Transaction::new(
                            format!("lease-expiry-{}-{}", cluster.node_id, now_ms()),
                            vec![Mutation::ExpireLeases],
                        );
                        let _ = cluster
                            .raft
                            .client_write(Command {
                                transaction,
                                now_ms: now_ms(),
                            })
                            .await;
                    }
                }
            }
        });
        let mut tasks = vec![handler, expiry];
        if let Some(startup) = startup {
            let weak = Arc::downgrade(&cluster);
            tasks.push(tokio::spawn(async move {
                loop {
                    let Some(cluster) = weak.upgrade() else { break };
                    if cluster.recover_offline(&startup).await.is_ok()
                        && cluster.ready.load(Ordering::Acquire)
                    {
                        break;
                    }
                    if now_ms() >= startup.payload.expires_ms {
                        break;
                    }
                    drop(cluster);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }));
        }
        *cluster.tasks.lock().await = tasks;
        Ok(cluster)
    }
    pub async fn wait_ready(&self, deadline: Duration) -> Result<()> {
        tokio::time::timeout(deadline, async {
            while !self.ready.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("offline startup did not obtain quorum admission")?;
        Ok(())
    }
    async fn recover_offline(
        &self,
        startup: &crate::audit::Signed<OfflineStartupAdmission>,
    ) -> Result<()> {
        let payload = &startup.payload;
        let admission_hash = digest_json(startup)?;
        let data = self.machine.data.read().await;
        if data
            .view
            .kv
            .get("cluster/offline_epoch")
            .and_then(Value::as_u64)
            == Some(payload.epoch)
            && data
                .view
                .kv
                .get("cluster/offline_admission_sha256")
                .and_then(Value::as_str)
                == Some(admission_hash.as_str())
        {
            self.ready.store(true, Ordering::Release);
            return Ok(());
        }
        ensure!(
            now_ms() < payload.expires_ms && self.raft.current_leader().await == Some(self.node_id),
            "offline recovery waiting for current quorum leader"
        );
        ensure!(
            data.view
                .kv
                .get("cluster/offline_epoch")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                == payload.prior_epoch,
            "offline recovery epoch changed"
        );
        let mut compares = vec![Compare {
            key: "cluster/offline_epoch".into(),
            expected: data.view.kv.get("cluster/offline_epoch").cloned(),
        }];
        let mut ops = vec![
            Mutation::Put {
                key: "cluster/offline_epoch".into(),
                value: Value::from(payload.epoch),
            },
            Mutation::Put {
                key: "cluster/offline_admission_sha256".into(),
                value: Value::String(admission_hash),
            },
        ];
        for (id, checkpoint) in &payload.checkpoints {
            let key = format!("cluster/certificates/{id}");
            let before = data.view.kv.get(&key).cloned();
            ensure!(
                digest_json(&before)? == checkpoint.payload.previous_history_sha256,
                "offline source history differs from recovered leader"
            );
            compares.push(Compare {
                key: key.clone(),
                expected: before,
            });
            ops.push(Mutation::Put {
                key,
                value: serde_json::to_value(CertificateHistory {
                    checkpoint: Some(checkpoint.clone()),
                    rotations: vec![],
                })?,
            });
        }
        drop(data);
        self.raft.ensure_linearizable().await?;
        let mut transaction = Transaction::new(format!("offline-startup-{}", payload.epoch), ops);
        transaction.compares = compares;
        ensure!(
            self.raft
                .client_write(Command {
                    transaction,
                    now_ms: now_ms()
                })
                .await?
                .data
                .applied,
            "offline certificate admission CAS raced"
        );
        Ok(())
    }
    /// Latest quorum-admitted leaf. Remote bootstrap inventories contain only its fingerprint.
    pub async fn current_certificate(&self, node_id: NodeId) -> Result<Vec<u8>> {
        let node = self
            .limits
            .nodes
            .get(&node_id)
            .context("node outside admitted inventory")?;
        let entry = self
            .read_control(&format!("cluster/certificates/{node_id}"))
            .await?;
        let _ = admitted_pins(
            node,
            node_id,
            entry.value.as_ref(),
            self.limits.rotation_trust_key.as_ref(),
        )?;
        if let Some(value) = entry.value {
            return Ok(CertificateHistory::parse(&value)?
                .leaf()
                .context("empty certificate rotation history")?
                .to_vec());
        }
        ensure!(
            node_id == self.node_id,
            "remote initial leaf not persisted; inventory contains its fingerprint only"
        );
        Ok(self.limits.certificate_der.clone())
    }
    /// A signed checkpoint compacts the exact durable chain; it cannot reset an inventory pin.
    pub async fn certificate_checkpoint(&self) -> Result<Option<CertificateCheckpoint>> {
        let key = format!("cluster/certificates/{}", self.node_id);
        let before = self.read_control(&key).await?;
        let Some(value) = before.value else {
            return Ok(None);
        };
        let history = CertificateHistory::parse(&value)?;
        admitted_pins(
            &self.limits.nodes[&self.node_id],
            self.node_id,
            Some(&value),
            self.limits.rotation_trust_key.as_ref(),
        )?;
        if history.rotations.len() < 32 {
            return Ok(None);
        }
        let last = &history
            .rotations
            .last()
            .context("no rotations to checkpoint")?
            .payload;
        Ok(Some(CertificateCheckpoint {
            node_id: self.node_id,
            bootstrap_sha256: self.limits.nodes[&self.node_id].certificate_sha256.clone(),
            previous_history_sha256: format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?)),
            sequence: history
                .checkpoint
                .as_ref()
                .map_or(0, |anchor| anchor.payload.sequence)
                .checked_add(history.rotations.len() as u64)
                .context("certificate sequence overflow")?,
            certificate_der: last.certificate_der.clone(),
            previous_sha256: last.previous_sha256.clone(),
            admitted_ms: last.admitted_ms,
            overlap_until_ms: last.overlap_until_ms,
        }))
    }
    pub async fn checkpoint_certificates(
        &self,
        checkpoint: crate::audit::Signed<CertificateCheckpoint>,
    ) -> Result<()> {
        checkpoint.verify(
            "node-certificate-checkpoint",
            self.limits
                .rotation_trust_key
                .as_ref()
                .context("rotation authority absent")?,
        )?;
        ensure!(
            self.certificate_checkpoint().await?.as_ref() == Some(&checkpoint.payload),
            "checkpoint differs from current durable admission"
        );
        let key = format!("cluster/certificates/{}", self.node_id);
        let before = self.read_control(&key).await?;
        ensure!(
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(
                    &before.value.as_ref().context("history absent")?
                )?)
            ) == checkpoint.payload.previous_history_sha256,
            "certificate checkpoint raced"
        );
        let value = serde_json::to_value(CertificateHistory {
            checkpoint: Some(checkpoint),
            rotations: vec![],
        })?;
        admitted_pins(
            &self.limits.nodes[&self.node_id],
            self.node_id,
            Some(&value),
            self.limits.rotation_trust_key.as_ref(),
        )?;
        let mut tx = Transaction::new(
            uuid::Uuid::new_v4().to_string(),
            vec![Mutation::Put {
                key: key.clone(),
                value,
            }],
        );
        tx.compares.push(Compare {
            key,
            expected: before.value,
        });
        ensure!(
            self.transact_control(tx).await?.applied,
            "certificate checkpoint CAS raced"
        );
        Ok(())
    }
    pub async fn rotate_certificate(
        &self,
        admission: crate::audit::Signed<CertificateRotation>,
        private_key_der: Vec<u8>,
    ) -> Result<()> {
        admission.verify(
            "node-certificate-rotation",
            self.limits
                .rotation_trust_key
                .as_ref()
                .context("rotation authority not configured")?,
        )?;
        ensure!(
            admission.payload.node_id == self.node_id,
            "cannot install another node certificate"
        );
        let key = format!("cluster/certificates/{}", self.node_id);
        let before = self.read_control(&key).await?;
        let mut history = before
            .value
            .clone()
            .as_ref()
            .map(CertificateHistory::parse)
            .transpose()?
            .unwrap_or_default();
        let new_hash = format!("{:x}", Sha256::digest(&admission.payload.certificate_der));
        let already = history
            .leaf()
            .is_some_and(|leaf| format!("{:x}", Sha256::digest(leaf)) == new_hash);
        if !already {
            history.rotations.push(admission.clone());
        }
        let value = serde_json::to_value(&history)?;
        admitted_pins(
            &self.limits.nodes[&self.node_id],
            self.node_id,
            Some(&value),
            self.limits.rotation_trust_key.as_ref(),
        )?;
        let mut replacement = self.limits.clone();
        replacement.certificate_der = admission.payload.certificate_der.clone();
        replacement.private_key_der = private_key_der;
        let (server, client) = tls(&replacement)?; // Validate key pair before committing admission.
        if !already {
            let mut transaction = Transaction::new(
                format!("certificate-rotation-{}-{new_hash}", self.node_id),
                vec![Mutation::Put {
                    key: key.clone(),
                    value: value.clone(),
                }],
            );
            transaction.compares.push(Compare {
                key: key.clone(),
                expected: before.value,
            });
            ensure!(
                self.transact_control(transaction).await?.applied,
                "certificate rotation admission raced"
            );
        }
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?));
        let members = self
            .machine
            .data
            .read()
            .await
            .membership
            .membership()
            .nodes()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        // Wait for every currently admitted participant before presenting the new leaf.
        tokio::time::timeout(Duration::from_secs(10), async {
            for peer in members {
                if peer == self.node_id {
                    continue;
                }
                loop {
                    if self
                        .transport
                        .call(
                            peer,
                            Rpc::RotationReady {
                                key: key.clone(),
                                digest: digest.clone(),
                            },
                        )
                        .await
                        .is_ok()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        })
        .await
        .context("rotation admission propagation incomplete; old endpoint retained")?;
        self.transport.endpoint.set_server_config(Some(server));
        *self.transport.client_config.write().await = client;
        // Existing streams retain their transport; every new request checks current pins and expiry.
        self.transport.connections.lock().await.clear();
        Ok(())
    }
    pub async fn initialize(&self, voters: BTreeSet<NodeId>) -> Result<()> {
        ensure!(
            self.ready.load(Ordering::Acquire),
            "offline quorum admission must precede initialization"
        );
        ensure!(
            !voters.is_empty() && voters.iter().all(|id| self.limits.nodes.contains_key(id)),
            "voter outside admitted inventory"
        );
        {
            let data = self.machine.data.read().await;
            let membership = data.membership.membership();
            if membership.nodes().next().is_some() {
                ensure!(
                    membership.get_joint_config().len() == 1
                        && membership.voter_ids().collect::<BTreeSet<_>>() == voters
                        && membership
                            .nodes()
                            .map(|(id, _)| *id)
                            .collect::<BTreeSet<_>>()
                            == voters
                        && membership.nodes().all(|(id, node)| self
                            .limits
                            .nodes
                            .get(id)
                            .is_some_and(|admitted| node.addr == admitted.addr.to_string())),
                    "existing durable membership differs from initialization; use admitted membership change"
                );
                return Ok(());
            }
        }
        let members = voters
            .into_iter()
            .map(|id| {
                (
                    id,
                    BasicNode {
                        addr: self.limits.nodes[&id].addr.to_string(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        self.raft.initialize(members).await?;
        Ok(())
    }
    pub async fn add_learner(&self, id: NodeId) -> Result<()> {
        let node = self
            .limits
            .nodes
            .get(&id)
            .context("learner outside admitted inventory")?;
        self.raft
            .add_learner(
                id,
                BasicNode {
                    addr: node.addr.to_string(),
                },
                true,
            )
            .await?;
        Ok(())
    }
    pub async fn change_membership(&self, voters: BTreeSet<NodeId>) -> Result<()> {
        ensure!(
            !voters.is_empty() && voters.iter().all(|id| self.limits.nodes.contains_key(id)),
            "voter outside admitted inventory"
        );
        self.raft.change_membership(voters, false).await?;
        Ok(())
    }
    pub async fn leader(&self) -> Option<NodeId> {
        self.raft.current_leader().await
    }
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.transport.endpoint.local_addr()?)
    }
    pub async fn cached_view(&self) -> StateView {
        self.machine.data.read().await.view.clone()
    }
    async fn authoritative(&self, rpc: Rpc) -> Result<Value> {
        ensure!(
            self.ready.load(Ordering::Acquire),
            "offline startup admission not quorum committed"
        );
        let leader = self
            .raft
            .current_leader()
            .await
            .context("quorum leader unavailable; admission refused")?;
        if leader == self.node_id {
            self.process(rpc).await
        } else {
            self.transport.call(leader, rpc).await
        }
    }
    pub async fn transact(&self, transaction: Transaction) -> Result<TxnResult> {
        let lease_only = transaction.ops.iter().all(|op| {
            matches!(
                op,
                Mutation::GrantLease { .. }
                    | Mutation::RenewLease { .. }
                    | Mutation::RevokeLease { .. }
                    | Mutation::ExpireLeases
            )
        });
        self.transact_class(transaction, lease_only).await
    }
    /// Only broker code can select control admission; workload requests cannot set this class.
    pub(crate) async fn transact_control(&self, transaction: Transaction) -> Result<TxnResult> {
        self.transact_class(transaction, true).await
    }
    async fn transact_class(&self, transaction: Transaction, control: bool) -> Result<TxnResult> {
        ensure!(
            !transaction.request_id.is_empty()
                && transaction.request_id.len() <= 256
                && !transaction.ops.is_empty()
                && transaction.ops.len() <= 4096,
            "invalid or empty transaction"
        );
        let command = Command {
            transaction,
            now_ms: now_ms(),
        };
        let value = self
            .authoritative(if control {
                Rpc::ControlTransaction(command)
            } else {
                Rpc::Transaction(command)
            })
            .await?;
        Ok(serde_json::from_value(value)?)
    }
    pub async fn read(&self, key: &str) -> Result<RevisionedValue> {
        Ok(serde_json::from_value(
            self.authoritative(Rpc::Read(key.into())).await?,
        )?)
    }
    pub(crate) async fn read_control(&self, key: &str) -> Result<RevisionedValue> {
        Ok(serde_json::from_value(
            self.authoritative(Rpc::ControlRead(key.into())).await?,
        )?)
    }
    pub async fn transaction_result(&self, id: &str) -> Result<Option<TxnResult>> {
        Ok(serde_json::from_value(
            self.authoritative(Rpc::TransactionResult(id.into()))
                .await?,
        )?)
    }
    pub async fn view(&self) -> Result<StateView> {
        Ok(serde_json::from_value(
            self.authoritative(Rpc::View).await?,
        )?)
    }
    pub(crate) async fn view_control(&self) -> Result<StateView> {
        Ok(serde_json::from_value(
            self.authoritative(Rpc::ControlView).await?,
        )?)
    }
    pub async fn watch(&self, after: u64) -> Result<WatchResult> {
        Ok(serde_json::from_value(
            self.authoritative(Rpc::Watch(after)).await?,
        )?)
    }
    pub async fn wait_watch(&self, after: u64, deadline: Duration) -> Result<WatchResult> {
        let wait = async {
            loop {
                let notified = self.machine.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let result = self.watch(after).await?;
                if !matches!(&result, WatchResult::Events { events, .. } if events.is_empty()) {
                    return Ok(result);
                }
                notified.await;
            }
        };
        tokio::time::timeout(deadline, wait)
            .await
            .context("watch wait expired")?
    }
    pub async fn lease_current(&self, id: &str, holder: &str, fence: u64) -> Result<bool> {
        Ok(serde_json::from_value(
            self.authoritative(Rpc::LeaseCurrent {
                id: id.into(),
                holder: holder.into(),
                fence,
            })
            .await?,
        )?)
    }
    pub async fn ping(&self, id: NodeId, payload: Vec<u8>) -> Result<Vec<u8>> {
        Ok(serde_json::from_value(
            self.transport.call(id, Rpc::Ping(payload)).await?,
        )?)
    }
    pub async fn trigger_snapshot(&self) -> Result<()> {
        self.raft.trigger().snapshot().await?;
        Ok(())
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.transport.endpoint.close(0_u32.into(), b"run stopped");
        for task in self.tasks.lock().await.drain(..) {
            task.abort();
            let _ = task.await;
        }
        self.raft.shutdown().await?;
        self.transport.connections.lock().await.clear();
        self.transport.endpoint.wait_idle().await;
        Ok(())
    }
    async fn handle(&self, connection: &Connection, recv: &mut quinn::RecvStream) -> Result<Value> {
        let wire: Wire =
            serde_json::from_slice(&read_frame(recv, self.limits.max_frame_bytes).await?)?;
        self.transport.peer_allowed(wire.sender, connection).await?;
        self.process(wire.rpc).await
    }
    async fn process(&self, rpc: Rpc) -> Result<Value> {
        ensure!(
            rpc.channel() == 0 || self.ready.load(Ordering::Acquire),
            "offline recovery blocks application traffic before quorum admission"
        );
        let _admission = match rpc.channel() {
            0 => None, // Raft protocol never waits behind application admission.
            1 => Some(
                tokio::time::timeout(
                    Duration::from_millis(500),
                    self.control_slots.clone().acquire_owned(),
                )
                .await
                .context("control admission saturated")??,
            ),
            _ => Some(
                self.bulk_slots
                    .clone()
                    .try_acquire_owned()
                    .context("bulk admission saturated; retry with the same intent")?,
            ),
        };
        match rpc {
            Rpc::RotationReady { key, digest } => {
                let data = self.machine.data.read().await;
                let value = data
                    .view
                    .kv
                    .get(&key)
                    .context("rotation not committed locally")?;
                ensure!(
                    format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)) == digest,
                    "rotation not yet applied"
                );
                Ok(Value::Bool(true))
            }
            Rpc::Append(request) => Ok(serde_json::to_value(
                self.raft.append_entries(request).await,
            )?),
            Rpc::Vote(request) => Ok(serde_json::to_value(self.raft.vote(request).await)?),
            Rpc::Snapshot(request) => Ok(serde_json::to_value(
                self.raft.install_snapshot(request).await,
            )?),
            Rpc::Transaction(mut command) | Rpc::ControlTransaction(mut command) => {
                self.raft.ensure_linearizable().await?;
                command.now_ms = now_ms();
                ensure!(
                    serde_json::to_vec(&command)?.len() <= self.limits.max_frame_bytes,
                    "transaction exceeds ingress bound"
                );
                Ok(serde_json::to_value(
                    self.raft.client_write(command).await?.data,
                )?)
            }
            Rpc::TransactionResult(id) => {
                self.raft.ensure_linearizable().await?;
                Ok(serde_json::to_value(
                    self.machine
                        .data
                        .read()
                        .await
                        .results
                        .get(&id)
                        .map(|(_, result)| result),
                )?)
            }
            Rpc::LeaseCurrent { id, holder, fence } => {
                self.raft.ensure_linearizable().await?;
                let data = self.machine.data.read().await;
                Ok(serde_json::to_value(
                    data.view.leases.get(&id).is_some_and(|lease| {
                        !lease.revoked
                            && lease.holder == holder
                            && lease.fence == fence
                            && lease.expires_ms > now_ms()
                    }),
                )?)
            }
            Rpc::Read(key) | Rpc::ControlRead(key) => {
                self.raft.ensure_linearizable().await?;
                let data = self.machine.data.read().await;
                Ok(serde_json::to_value(RevisionedValue {
                    revision: data.view.revision,
                    value: data.view.kv.get(&key).cloned(),
                })?)
            }
            Rpc::View | Rpc::ControlView => {
                self.raft.ensure_linearizable().await?;
                Ok(serde_json::to_value(&self.machine.data.read().await.view)?)
            }
            Rpc::Watch(after) => {
                self.raft.ensure_linearizable().await?;
                let data = self.machine.data.read().await;
                ensure!(
                    after <= data.view.revision,
                    "watch cursor is ahead of authoritative revision"
                );
                let result = if data
                    .history
                    .front()
                    .is_some_and(|change| after + 1 < change.revision)
                {
                    WatchResult::SnapshotResync(data.view.clone())
                } else {
                    WatchResult::Events {
                        revision: data.view.revision,
                        events: data
                            .history
                            .iter()
                            .filter(|change| change.revision > after)
                            .cloned()
                            .collect(),
                    }
                };
                Ok(serde_json::to_value(result)?)
            }
            Rpc::Ping(payload) => Ok(serde_json::to_value(payload)?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct OwnedLogTestDir(PathBuf);
    impl OwnedLogTestDir {
        fn create() -> Self {
            let path = std::env::temp_dir().join(format!("solos-log-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for OwnedLogTestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[tokio::test]
    async fn retained_raft_payloads_are_off_heap_authenticated_and_purged_with_restart() {
        let dir = OwnedLogTestDir::create();
        let log = DurableLog::open(dir.path(), [7; 32]).unwrap();
        let payload = "PRIVATE_RETAINED_BODY".repeat(4096);
        for index in 0..128 {
            let entry = Entry {
                log_id: LogId::new(openraft::CommittedLeaderId::new(1, 1), index),
                payload: EntryPayload::Normal(Command {
                    transaction: Transaction::new(
                        format!("large-{index}"),
                        vec![Mutation::Put {
                            key: "kernel/state".into(),
                            value: Value::String(payload.clone()),
                        }],
                    ),
                    now_ms: index,
                }),
            };
            log.persist(LogRecord::Append(vec![entry])).await.unwrap();
        }
        let index_bytes = serde_json::to_vec(&*log.data.lock().await).unwrap();
        assert!(
            index_bytes.len() < 16 * 1024,
            "historical body entered memory index"
        );
        assert!(
            !String::from_utf8_lossy(&fs::read(&log.path).unwrap())
                .contains("PRIVATE_RETAINED_BODY")
        );
        let mut reader = log.clone();
        let entries = openraft::RaftLogReader::try_get_log_entries(&mut reader, 127..128)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        let EntryPayload::Normal(command) = &entries[0].payload else {
            panic!("normal entry missing")
        };
        assert_eq!(
            command.transaction.ops[0],
            Mutation::Put {
                key: "kernel/state".into(),
                value: Value::String(payload)
            }
        );
        let purge_id = LogId::new(openraft::CommittedLeaderId::new(1, 1), 126);
        openraft::storage::RaftLogStorage::purge(&mut reader, purge_id)
            .await
            .unwrap();
        assert_eq!(
            fs::read_dir(log.path.with_extension("entries"))
                .unwrap()
                .count(),
            1
        );
        let reopened = DurableLog::open(dir.path(), [7; 32]).unwrap();
        assert_eq!(reopened.data.lock().await.entries.len(), 1);
        assert_eq!(reopened.data.lock().await.purged, Some(purge_id));
        let id = LogId::new(openraft::CommittedLeaderId::new(1, 1), 127);
        let file = DurableLog::entry_path(&log.path, id).unwrap();
        let mut corrupt = fs::read(&file).unwrap();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 1;
        fs::write(file, corrupt).unwrap();
        assert!(DurableLog::open(dir.path(), [7; 32]).is_err());
    }

    #[tokio::test]
    async fn retained_float_entry_restarts_without_decode_drift() {
        let dir = OwnedLogTestDir::create();
        let log = DurableLog::open(dir.path(), [7; 32]).unwrap();
        let id = LogId::new(openraft::CommittedLeaderId::new(1, 1), 0);
        let entry = Entry {
            log_id: id,
            payload: EntryPayload::Normal(Command {
                transaction: Transaction::new(
                    "float",
                    vec![Mutation::Put {
                        key: "float".into(),
                        value: Value::from(51.248178375505404),
                    }],
                ),
                now_ms: 1,
            }),
        };
        log.persist(LogRecord::Append(vec![entry])).await.unwrap();
        DurableLog::open(dir.path(), [7; 32]).expect("floating log body changed during decoding");
    }

    #[test]
    fn old_payload_checkpoint_replays_into_small_authenticated_index() {
        let dir = OwnedLogTestDir::create();
        let id = LogId::new(openraft::CommittedLeaderId::new(1, 1), 0);
        let entry = Entry {
            log_id: id,
            payload: EntryPayload::Normal(Command {
                transaction: Transaction::new(
                    "legacy",
                    vec![Mutation::Put {
                        key: "legacy".into(),
                        value: serde_json::json!({"kept":true}),
                    }],
                ),
                now_ms: 1,
            }),
        };
        let mut old = LogData::default();
        old.entries.insert(0, entry);
        fs::write(
            dir.path().join("raft.wal"),
            record_bytes(&[7; 32], &LogRecord::Checkpoint(old)).unwrap(),
        )
        .unwrap();
        let log = DurableLog::open(dir.path(), [7; 32]).unwrap();
        assert!(DurableLog::read_entry(&log.path, &[7; 32], id).is_ok());
        assert!(
            !serde_json::to_string(&*log.data.blocking_lock())
                .unwrap()
                .contains("kept")
        );
    }

    #[test]
    fn transactions_are_atomic_fenced_idempotent_and_watch_history_compacts() {
        let mut state = StateData::default();
        let command = |id: &str, ops| Command {
            transaction: Transaction::new(id, ops),
            now_ms: 100,
        };
        let granted = command(
            "grant",
            vec![
                Mutation::Put {
                    key: "allocation".into(),
                    value: Value::from("worker"),
                },
                Mutation::GrantLease {
                    id: "allocation".into(),
                    holder: "worker".into(),
                    expires_ms: 200,
                },
            ],
        );
        assert!(state.apply_transaction(&granted, 2).applied);
        assert_eq!(state.apply_transaction(&granted, 2).revision, 1);
        let failed = command(
            "failed",
            vec![
                Mutation::Put {
                    key: "partial".into(),
                    value: Value::from(1),
                },
                Mutation::GrantLease {
                    id: "allocation".into(),
                    holder: "other".into(),
                    expires_ms: 200,
                },
            ],
        );
        assert!(!state.apply_transaction(&failed, 2).applied);
        assert!(!state.view.kv.contains_key("partial"));
        state.apply_transaction(
            &command(
                "revoke",
                vec![Mutation::RevokeLease {
                    id: "allocation".into(),
                }],
            ),
            2,
        );
        let mut fenced = command(
            "effect",
            vec![Mutation::Put {
                key: "effect".into(),
                value: Value::from(1),
            }],
        );
        fenced.transaction.leases.push(LeaseCheck {
            id: "allocation".into(),
            holder: "worker".into(),
            fence: 1,
        });
        assert!(!state.apply_transaction(&fenced, 2).applied);
        for n in 0..5 {
            state.apply_transaction(
                &command(
                    &format!("change-{n}"),
                    vec![Mutation::Put {
                        key: "n".into(),
                        value: Value::from(n),
                    }],
                ),
                2,
            );
        }
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.view.revision, 7);
    }
    #[test]
    fn sealed_storage_never_contains_payload_and_authenticates_domain_key_and_bytes() {
        let plaintext = b"PRIVATE prompt board finding canary";
        let bytes = seal(&[7; 32], b"state.json", plaintext).unwrap();
        assert!(
            !bytes
                .windows(plaintext.len())
                .any(|window| window == plaintext)
        );
        assert_eq!(unseal(&[7; 32], b"state.json", &bytes).unwrap(), plaintext);
        assert!(unseal(&[8; 32], b"state.json", &bytes).is_err());
        assert!(unseal(&[7; 32], b"snapshot.json", &bytes).is_err());
        let mut bad = bytes;
        *bad.last_mut().unwrap() ^= 1;
        assert!(unseal(&[7; 32], b"state.json", &bad).is_err());
    }
    #[test]
    fn rotation_pins_require_authority_and_bounded_overlap() {
        let signer = crate::audit::CryptoSigner::from_seed([3; 32]);
        let node = NodeAddress {
            addr: "127.0.0.1:1234".parse().unwrap(),
            server_name: "node-1".into(),
            certificate_sha256: "old".into(),
        };
        let now = now_ms();
        let record = signer
            .sign(
                "node-certificate-rotation",
                CertificateRotation {
                    node_id: 1,
                    previous_sha256: "old".into(),
                    certificate_der: vec![1, 2, 3],
                    admitted_ms: now,
                    overlap_until_ms: now + 50,
                },
            )
            .unwrap();
        let value = serde_json::to_value(vec![record.clone()]).unwrap();
        assert!(
            admitted_pins(&node, 1, Some(&value), Some(&signer.public_key()))
                .unwrap()
                .contains("old")
        );
        assert!(admitted_pins(&node, 1, Some(&value), Some(&[9; 32])).is_err());
        let mut invalid = record.payload;
        invalid.overlap_until_ms = now + 300001;
        let value = serde_json::to_value(vec![
            signer.sign("node-certificate-rotation", invalid).unwrap(),
        ])
        .unwrap();
        assert!(admitted_pins(&node, 1, Some(&value), Some(&signer.public_key())).is_err());
    }
    #[test]
    fn signed_checkpoint_supports_repeated_bounded_rotation_without_bootstrap_reset() {
        let signer = crate::audit::CryptoSigner::from_seed([3; 32]);
        let node = NodeAddress {
            addr: "127.0.0.1:1".parse().unwrap(),
            server_name: "node-1".into(),
            certificate_sha256: "bootstrap".into(),
        };
        let now = now_ms();
        let mut history = CertificateHistory::default();
        let mut current = node.certificate_sha256.clone();
        for sequence in 1..=160u64 {
            let leaf = sequence.to_be_bytes().to_vec();
            history.rotations.push(
                signer
                    .sign(
                        "node-certificate-rotation",
                        CertificateRotation {
                            node_id: 1,
                            previous_sha256: current.clone(),
                            certificate_der: leaf.clone(),
                            admitted_ms: now,
                            overlap_until_ms: now,
                        },
                    )
                    .unwrap(),
            );
            current = format!("{:x}", Sha256::digest(&leaf));
            if sequence % 32 == 0 {
                let before = serde_json::to_value(&history).unwrap();
                assert!(
                    admitted_pins(&node, 1, Some(&before), Some(&signer.public_key()))
                        .unwrap()
                        .contains(&current)
                );
                history = CertificateHistory {
                    checkpoint: Some(
                        signer
                            .sign(
                                "node-certificate-checkpoint",
                                CertificateCheckpoint {
                                    node_id: 1,
                                    bootstrap_sha256: node.certificate_sha256.clone(),
                                    previous_history_sha256: digest_json(&before).unwrap(),
                                    sequence,
                                    certificate_der: leaf,
                                    previous_sha256: history
                                        .rotations
                                        .last()
                                        .unwrap()
                                        .payload
                                        .previous_sha256
                                        .clone(),
                                    admitted_ms: now,
                                    overlap_until_ms: now,
                                },
                            )
                            .unwrap(),
                    ),
                    rotations: vec![],
                };
            }
            let value = serde_json::to_value(&history).unwrap();
            assert!(
                admitted_pins(&node, 1, Some(&value), Some(&signer.public_key()))
                    .unwrap()
                    .contains(&current)
            );
        }
        let mut altered = history.checkpoint.unwrap().payload;
        altered.bootstrap_sha256 = "another-inventory".into();
        let value = serde_json::to_value(CertificateHistory {
            checkpoint: Some(signer.sign("node-certificate-checkpoint", altered).unwrap()),
            rotations: vec![],
        })
        .unwrap();
        assert!(admitted_pins(&node, 1, Some(&value), Some(&signer.public_key())).is_err());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn expired_three_voter_cluster_requires_exact_offline_authority_and_commits_fresh_pins() {
        let root = std::env::temp_dir().join(format!("solos-offline-{}", uuid::Uuid::new_v4()));
        let authority = crate::identity::IdentityAuthority::generate("test.local").unwrap();
        let signer = crate::audit::CryptoSigner::from_seed([3; 32]);
        let mut sockets = (1..=3)
            .map(|_| Some(std::net::UdpSocket::bind("127.0.0.1:0").unwrap()))
            .collect::<Vec<_>>();
        let mut nodes = BTreeMap::new();
        let mut identities = BTreeMap::new();
        for id in 1..=3u64 {
            let workload = crate::identity::WorkloadId {
                trust_domain: "test.local".into(),
                tenant: "system".into(),
                goal: "cluster".into(),
                node: id.to_string(),
                agent: "cluster".into(),
                incarnation: "1".into(),
            };
            let identity = authority.issue(workload, now_ms() / 1000, 4).unwrap();
            nodes.insert(
                id,
                NodeAddress {
                    addr: sockets[id as usize - 1]
                        .as_ref()
                        .unwrap()
                        .local_addr()
                        .unwrap(),
                    server_name: identity.server_name.clone(),
                    certificate_sha256: format!("{:x}", Sha256::digest(&identity.certificate_der)),
                },
            );
            identities.insert(id, identity);
        }
        let mut configs = (1..=3u64)
            .map(|id| ClusterConfig {
                node_id: id,
                bind_addr: nodes[&id].addr,
                nodes: nodes.clone(),
                certificate_der: identities[&id].certificate_der.clone(),
                private_key_der: identities[&id].private_key_der.clone(),
                ca_der: authority.ca_der().to_vec(),
                directory: root.join(id.to_string()),
                storage_key: [7; 32],
                rotation_trust_key: Some(signer.public_key()),
                max_frame_bytes: 1024 * 1024,
                max_queue_items: 32,
                history_limit: 16,
            })
            .collect::<Vec<_>>();
        let mut cluster = vec![];
        for (index, config) in configs.iter().enumerate() {
            drop(sockets[index].take());
            cluster.push(Cluster::open(config.clone()).await.unwrap());
        }
        cluster[0].initialize([1, 2, 3].into()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if cluster[0].leader().await.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            cluster[0]
                .transact(Transaction::new(
                    "offline-preserved",
                    vec![Mutation::Put {
                        key: "preserved".into(),
                        value: Value::from(42)
                    }]
                ))
                .await
                .unwrap()
                .applied
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if cluster.iter().all(|_| true) {
                    let mut complete = true;
                    for node in &cluster {
                        complete &=
                            node.cached_view().await.kv.get("preserved") == Some(&Value::from(42));
                    }
                    if complete {
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for node in &cluster {
            node.shutdown().await.unwrap();
        }
        cluster.clear();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            Cluster::open(configs[0].clone()).await.is_err(),
            "expired ordinary pins cannot restart"
        );
        let mut requests = vec![];
        for config in &configs {
            requests.push(Cluster::offline_startup_request(config).await.unwrap());
        }
        let mut leaves = BTreeMap::new();
        for config in &mut configs {
            let fresh = authority
                .issue(
                    identities[&config.node_id].identity.clone(),
                    now_ms() / 1000,
                    60,
                )
                .unwrap();
            config.certificate_der = fresh.certificate_der.clone();
            config.private_key_der = fresh.private_key_der.clone();
            leaves.insert(config.node_id, fresh.certificate_der.clone());
        }
        let admission =
            assemble_offline_startup(&requests, leaves.clone(), 1, now_ms() + 30_000, &signer)
                .unwrap();
        let mut incomplete = admission.payload.clone();
        incomplete.local_state_sha256.remove(&3);
        assert!(
            Cluster::open_with_startup_admission(
                configs[0].clone(),
                Some(signer.sign("cluster-offline-startup", incomplete).unwrap())
            )
            .await
            .is_err()
        );
        for config in &configs {
            cluster.push(
                Cluster::open_with_startup_admission(config.clone(), Some(admission.clone()))
                    .await
                    .unwrap(),
            );
        }
        assert!(
            cluster[0]
                .transact(Transaction::new(
                    "before-admission",
                    vec![Mutation::Put {
                        key: "forbidden".into(),
                        value: Value::Bool(true)
                    }]
                ))
                .await
                .is_err()
        );
        for node in &cluster {
            node.wait_ready(Duration::from_secs(10)).await.unwrap();
        }
        assert_eq!(
            cluster[1].read("preserved").await.unwrap().value,
            Some(Value::from(42))
        );
        assert_eq!(
            cluster[2]
                .read("cluster/offline_epoch")
                .await
                .unwrap()
                .value,
            Some(Value::from(1))
        );
        assert!(
            cluster[1]
                .transact(Transaction::new(
                    "after-admission",
                    vec![Mutation::Put {
                        key: "fresh".into(),
                        value: Value::Bool(true)
                    }]
                ))
                .await
                .unwrap()
                .applied
        );
        for node in &cluster {
            node.shutdown().await.unwrap();
        }
        cluster.clear();
        assert!(
            Cluster::open_with_startup_admission(configs[0].clone(), Some(admission))
                .await
                .is_err(),
            "recovery admission replay rejected against advanced state"
        );
        let restored = Cluster::open(configs[0].clone()).await.unwrap();
        restored.shutdown().await.unwrap();
        drop(restored);
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn wal_restart_preserves_vote_logs_and_rejects_corruption() {
        let dir = std::env::temp_dir().join(format!("solos-wal-{}", now_ms()));
        fs::create_dir_all(&dir).unwrap();
        let log = DurableLog::open(&dir, [7; 32]).unwrap();
        log.persist(LogRecord::Vote(Vote::new(3, 2))).await.unwrap();
        assert_eq!(
            DurableLog::open(&dir, [7; 32])
                .unwrap()
                .data
                .lock()
                .await
                .vote,
            Some(Vote::new(3, 2))
        );
        let mut bytes = fs::read(dir.join("raft.wal")).unwrap();
        bytes[5] ^= 1;
        fs::write(dir.join("raft.wal"), bytes).unwrap();
        assert!(DurableLog::open(&dir, [7; 32]).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn actual_raft_over_mtls_quic_survives_leader_loss_and_restart() {
        let root = std::env::temp_dir().join(format!("solos-raft-test-{}", now_ms()));
        fs::create_dir_all(&root).unwrap();
        let mut ca = rcgen::CertificateParams::default();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca, ca_key);
        let mut assets = BTreeMap::new();
        let mut nodes = BTreeMap::new();
        for id in 1..=3 {
            let name = format!("node-{id}");
            let mut params = rcgen::CertificateParams::new(vec![name.clone()]).unwrap();
            params.extended_key_usages = vec![
                rcgen::ExtendedKeyUsagePurpose::ServerAuth,
                rcgen::ExtendedKeyUsagePurpose::ClientAuth,
            ];
            let key = rcgen::KeyPair::generate().unwrap();
            let cert = params.signed_by(&key, &issuer).unwrap();
            let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let addr = socket.local_addr().unwrap();
            nodes.insert(
                id,
                NodeAddress {
                    addr,
                    server_name: name,
                    certificate_sha256: format!("{:x}", Sha256::digest(cert.der())),
                },
            );
            assets.insert(id, (cert.der().to_vec(), key.serialize_der()));
        }
        let rotation_signer = crate::audit::CryptoSigner::from_seed([3; 32]);
        let mut configs = (1..=3)
            .map(|id| ClusterConfig {
                node_id: id,
                bind_addr: nodes[&id].addr,
                nodes: nodes.clone(),
                certificate_der: assets[&id].0.clone(),
                private_key_der: assets[&id].1.clone(),
                ca_der: ca_cert.der().to_vec(),
                directory: root.join(id.to_string()),
                storage_key: [7; 32],
                rotation_trust_key: Some(rotation_signer.public_key()),
                max_frame_bytes: 1_048_576,
                max_queue_items: 32,
                history_limit: 2,
            })
            .collect::<Vec<_>>();
        let mut cluster = Vec::new();
        for config in &configs {
            cluster.push(Cluster::open(config.clone()).await.unwrap());
        }
        cluster[0]
            .initialize(BTreeSet::from([1, 2, 3]))
            .await
            .unwrap();
        let wait = async {
            loop {
                if cluster[1].leader().await.is_some() && cluster[2].leader().await.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(8), wait)
            .await
            .unwrap();
        // Three real replicas on one host: saturate normal application ingress while renewing
        // and revoking an authoritative lease over the separate control connection.
        let leader = cluster[0].leader().await.unwrap();
        let leader_node = cluster.iter().find(|node| node.node_id == leader).unwrap();
        let follower = cluster
            .iter()
            .find(|node| node.node_id != leader)
            .unwrap()
            .clone();
        let granted = follower
            .transact(Transaction::new(
                "saturation-lease",
                vec![Mutation::GrantLease {
                    id: "saturation-worker".into(),
                    holder: "worker".into(),
                    expires_ms: now_ms() + 30_000,
                }],
            ))
            .await
            .unwrap();
        let mut flood = tokio::task::JoinSet::new();
        for index in 0..64 {
            let follower = follower.clone();
            flood.spawn(async move {
                follower
                    .transact(Transaction::new(
                        format!("board-flood-{index}"),
                        vec![Mutation::Put {
                            key: "board/flood".into(),
                            value: Value::String("b".repeat(32 * 1024)),
                        }],
                    ))
                    .await
            });
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while leader_node.bulk_slots.available_permits() == 16 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let control_started = std::time::Instant::now();
        assert!(
            follower
                .transact(Transaction::new(
                    "flood-renew",
                    vec![Mutation::RenewLease {
                        id: "saturation-worker".into(),
                        holder: "worker".into(),
                        fence: granted.revision,
                        expires_ms: now_ms() + 60_000,
                    }]
                ))
                .await
                .unwrap()
                .applied
        );
        assert!(
            follower
                .lease_current("saturation-worker", "worker", granted.revision)
                .await
                .unwrap()
        );
        assert!(
            follower
                .transact_control(Transaction::new(
                    "flood-stop",
                    vec![
                        Mutation::Put {
                            key: "stops/flood-worker".into(),
                            value: Value::Bool(true)
                        },
                        Mutation::RevokeLease {
                            id: "saturation-worker".into()
                        },
                    ]
                ))
                .await
                .unwrap()
                .applied
        );
        assert!(
            !follower
                .lease_current("saturation-worker", "worker", granted.revision)
                .await
                .unwrap()
        );
        let control_elapsed = control_started.elapsed();
        assert!(control_elapsed < Duration::from_secs(5));
        let mut rejected = 0;
        while let Some(result) = flood.join_next().await {
            if result.unwrap().is_err() {
                rejected += 1;
            }
        }
        assert!(
            rejected > 0,
            "normal ingress must reject saturation instead of growing unbounded"
        );
        assert!(
            follower
                .transport
                .connections
                .lock()
                .await
                .keys()
                .any(|(_, channel)| *channel == 1)
        );
        assert!(
            follower
                .transport
                .connections
                .lock()
                .await
                .keys()
                .any(|(_, channel)| *channel == 2)
        );
        eprintln!(
            "same-host three-replica saturation: 64 writes, {rejected} rejected; renewal+stop+fence checks {} ms",
            control_elapsed.as_millis()
        );
        drop(follower);
        let transaction = Transaction::new(
            "original",
            vec![Mutation::Put {
                key: "answer".into(),
                value: Value::from(42),
            }],
        );
        let result = cluster[1].transact(transaction.clone()).await.unwrap();
        assert!(result.applied);
        assert_eq!(
            cluster[2].read("answer").await.unwrap().value,
            Some(Value::from(42))
        );
        assert_eq!(
            cluster[0].transact(transaction).await.unwrap().revision,
            result.revision
        );
        let mut replacement = rcgen::CertificateParams::new(vec!["node-2".into()]).unwrap();
        replacement.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let replacement_key = rcgen::KeyPair::generate().unwrap();
        let replacement_cert = replacement.signed_by(&replacement_key, &issuer).unwrap();
        let rotation = rotation_signer
            .sign(
                "node-certificate-rotation",
                CertificateRotation {
                    node_id: 2,
                    previous_sha256: nodes[&2].certificate_sha256.clone(),
                    certificate_der: replacement_cert.der().to_vec(),
                    admitted_ms: now_ms(),
                    overlap_until_ms: now_ms() + 2000,
                },
            )
            .unwrap();
        cluster[1]
            .rotate_certificate(rotation, replacement_key.serialize_der())
            .await
            .unwrap();
        configs[1].certificate_der = replacement_cert.der().to_vec();
        configs[1].private_key_der = replacement_key.serialize_der();
        assert_eq!(
            cluster[1]
                .ping(3, b"rotated-client".to_vec())
                .await
                .unwrap(),
            b"rotated-client"
        );
        tokio::time::sleep(Duration::from_millis(2100)).await;
        assert_eq!(
            cluster[0]
                .ping(2, b"rotated-server".to_vec())
                .await
                .unwrap(),
            b"rotated-server"
        );
        // A stale ledger plugin must not overwrite a newer kernel blob under a fresh placement revision.
        struct KernelPlugin {
            expected: Value,
            next: Value,
        }
        impl crate::scheduler::Reserve for KernelPlugin {
            fn compares(&self) -> Result<Vec<Compare>> {
                Ok(vec![Compare {
                    key: "kernel/state".into(),
                    expected: Some(self.expected.clone()),
                }])
            }
            fn mutations(&self, _: &crate::scheduler::Plan) -> Result<Vec<Mutation>> {
                Ok(vec![Mutation::Put {
                    key: "kernel/state".into(),
                    value: self.next.clone(),
                }])
            }
        }
        impl crate::scheduler::Unreserve for KernelPlugin {
            fn compares(&self) -> Result<Vec<Compare>> {
                Ok(vec![Compare {
                    key: "kernel/state".into(),
                    expected: Some(self.expected.clone()),
                }])
            }
            fn mutations(&self, _: &[crate::scheduler::Allocation]) -> Result<Vec<Mutation>> {
                Ok(vec![Mutation::Put {
                    key: "kernel/state".into(),
                    value: self.next.clone(),
                }])
            }
        }
        let node = crate::scheduler::Node {
            id: "1".into(),
            capacity: crate::scheduler::Resources {
                cpu_millicores: 1000,
                memory_bytes: 10000,
                provider_slots: 10,
                rate_tokens: 10,
            },
            used: Default::default(),
            labels: BTreeMap::new(),
            capabilities: BTreeSet::new(),
            healthy: true,
            provider_rate_remaining: 10,
        };
        cluster[0]
            .transact(Transaction::new(
                "cas-fixture",
                vec![
                    Mutation::GrantLease {
                        id: "node/1".into(),
                        holder: "1".into(),
                        expires_ms: now_ms() + 120000,
                    },
                    Mutation::Put {
                        key: "nodes/1".into(),
                        value: serde_json::to_value(&node).unwrap(),
                    },
                    Mutation::Put {
                        key: "kernel/state".into(),
                        value: Value::from(2),
                    },
                ],
            ))
            .await
            .unwrap();
        let scheduler = crate::scheduler::Scheduler::new(10).unwrap();
        let gang = crate::scheduler::Gang {
            id: "cas-gang".into(),
            goal_id: "cas-goal".into(),
            strategy: crate::scheduler::Strategy::Pack,
            bundles: vec![crate::scheduler::Bundle {
                agent_id: "cas-agent".into(),
                placement_intent_id: "cas-intent".into(),
                incarnation: 1,
                resources: crate::scheduler::Resources {
                    cpu_millicores: 1,
                    memory_bytes: 1,
                    provider_slots: 1,
                    rate_tokens: 1,
                },
                labels: BTreeMap::new(),
                required_capabilities: BTreeSet::new(),
                budget_microusd: 1,
            }],
            priority: 0,
            created_revision: 0,
        };
        let view = cluster[0].view().await.unwrap();
        let plan = scheduler
            .plan(
                &gang,
                &crate::scheduler::PlacementSnapshot {
                    revision: view.revision,
                    nodes: BTreeMap::from([("1".into(), node.clone())]),
                    allocations: BTreeMap::new(),
                },
            )
            .unwrap();
        let stale = KernelPlugin {
            expected: Value::from(1),
            next: Value::from(999),
        };
        assert!(
            !scheduler
                .bind(&cluster[0], &plan, &stale, 10000)
                .await
                .unwrap()
                .applied
        );
        assert_eq!(
            cluster[0].read("kernel/state").await.unwrap().value,
            Some(Value::from(2))
        );
        assert_eq!(
            cluster[0]
                .read("allocations/cas-agent")
                .await
                .unwrap()
                .value,
            None
        );
        let fresh = KernelPlugin {
            expected: Value::from(2),
            next: Value::from(3),
        };
        assert!(
            scheduler
                .bind(&cluster[0], &plan, &fresh, 10000)
                .await
                .unwrap()
                .applied
        );
        let before = cluster[0]
            .read("allocations/cas-agent")
            .await
            .unwrap()
            .value;
        assert!(
            !scheduler
                .unreserve(&cluster[0], &plan.allocations, &stale)
                .await
                .unwrap()
                .applied
        );
        assert_eq!(
            cluster[0].read("kernel/state").await.unwrap().value,
            Some(Value::from(3))
        );
        assert_eq!(
            cluster[0]
                .read("allocations/cas-agent")
                .await
                .unwrap()
                .value,
            before
        );
        for index in 0..4 {
            assert!(
                cluster[1]
                    .transact(Transaction::new(
                        format!("history-{index}"),
                        vec![Mutation::Put {
                            key: "n".into(),
                            value: Value::from(index)
                        }]
                    ))
                    .await
                    .unwrap()
                    .applied
            );
        }
        assert!(matches!(
            cluster[2].watch(0).await.unwrap(),
            WatchResult::SnapshotResync(_)
        ));
        let grant = cluster[1]
            .transact(Transaction::new(
                "lease",
                vec![Mutation::GrantLease {
                    id: "task".into(),
                    holder: "worker".into(),
                    expires_ms: now_ms() + 150,
                }],
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !cluster[2]
                .lease_current("task", "worker", grant.revision)
                .await
                .unwrap()
        );
        let leader = cluster[1].leader().await.unwrap();
        cluster[(leader - 1) as usize].shutdown().await.unwrap();
        let survivor = (1..=3).find(|id| *id != leader).unwrap();
        let index = (survivor - 1) as usize;
        let wait = async {
            loop {
                if cluster[index].leader().await.is_some_and(|id| id != leader)
                    && cluster[index].view().await.is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .unwrap();
        let next = cluster[index]
            .transact(Transaction::new(
                "after-loss",
                vec![Mutation::Put {
                    key: "survived".into(),
                    value: Value::Bool(true),
                }],
            ))
            .await
            .unwrap();
        assert!(next.revision > result.revision);
        for (index, node) in cluster.iter().enumerate() {
            if index != (leader - 1) as usize {
                node.shutdown().await.unwrap();
            }
        }
        drop(cluster);
        let mut restarted = Vec::new();
        for config in configs {
            restarted.push(Cluster::open(config).await.unwrap());
        }
        let wait = async {
            loop {
                if restarted[0].view().await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .unwrap();
        let restored = restarted[0].view().await.unwrap();
        assert!(restored.revision >= next.revision);
        assert_eq!(restored.kv.get("survived"), Some(&Value::Bool(true)));
        assert!(restored.leases["task"].revoked);
        for node in restarted {
            node.shutdown().await.unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }
}
