//! RFC 6962 hashing and public verification; secret signers are never state DTOs.
use anyhow::{Result, ensure};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use zeroize::Zeroize;

pub type Hash = [u8; 32];
pub fn hash(bytes: &[u8]) -> Hash {
    Sha256::digest(bytes).into()
}
pub fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    // serde_json's ordered maps recursively canonicalize arbitrary object payloads.
    Ok(serde_json::to_vec(&serde_json::to_value(value)?)?)
}
pub fn content_hash<T: Serialize>(value: &T) -> Result<Hash> {
    Ok(hash(&canonical(value)?))
}

pub struct CryptoSigner {
    key: SigningKey,
}
impl std::fmt::Debug for CryptoSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CryptoSigner([REDACTED])")
    }
}
impl CryptoSigner {
    pub fn generate() -> Result<Self> {
        let mut seed = [0; 32];
        getrandom::fill(&mut seed).map_err(|_| anyhow::anyhow!("OS entropy unavailable"))?;
        let key = SigningKey::from_bytes(&seed);
        seed.zeroize();
        Ok(Self { key })
    }
    pub fn from_seed(mut seed: [u8; 32]) -> Self {
        let key = SigningKey::from_bytes(&seed);
        seed.zeroize();
        Self { key }
    }
    pub fn public_key(&self) -> Hash {
        self.key.verifying_key().to_bytes()
    }
    pub fn sign<T: Serialize>(&self, domain: &str, payload: T) -> Result<Signed<T>> {
        let signature = self
            .key
            .sign(&signing_bytes(domain, &payload)?)
            .to_bytes()
            .to_vec();
        Ok(Signed {
            payload,
            signer: self.public_key(),
            signature,
        })
    }
}
fn signing_bytes<T: Serialize>(domain: &str, payload: &T) -> Result<Vec<u8>> {
    ensure!(
        !domain.is_empty() && domain.len() <= 128,
        "invalid signature domain"
    );
    let mut bytes = b"solos-swarm-signature-v1\0".to_vec();
    bytes.extend_from_slice(&(domain.len() as u32).to_be_bytes());
    bytes.extend_from_slice(domain.as_bytes());
    bytes.extend_from_slice(&canonical(payload)?);
    Ok(bytes)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signed<T> {
    pub payload: T,
    pub signer: Hash,
    pub signature: Vec<u8>,
}
impl<T: Serialize> Signed<T> {
    pub fn verify(&self, domain: &str, trusted_key: &Hash) -> Result<()> {
        ensure!(&self.signer == trusted_key, "untrusted signing authority");
        let key = VerifyingKey::from_bytes(trusted_key)?;
        let signature = Signature::from_slice(&self.signature)?;
        key.verify_strict(&signing_bytes(domain, &self.payload)?, &signature)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEvent {
    pub id: String,
    pub kind: String,
    pub actor: String,
    pub goal: Option<String>,
    pub timestamp: u64,
    pub decision: String,
    pub payload_hash: Hash,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditLog {
    pub log_id: String,
    events: Vec<AuditEvent>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TreeHead {
    pub log_id: String,
    pub size: u64,
    pub root: Hash,
    pub timestamp: u64,
}
pub type SignedTreeHead = Signed<TreeHead>;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InclusionProof {
    pub index: u64,
    pub size: u64,
    pub path: Vec<Hash>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsistencyProof {
    pub old_size: u64,
    pub new_size: u64,
    pub path: Vec<Hash>,
}

fn leaf_hash(event: &AuditEvent) -> Result<Hash> {
    let mut bytes = vec![0];
    bytes.extend_from_slice(&canonical(event)?);
    Ok(hash(&bytes))
}
fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut bytes = [0; 65];
    bytes[0] = 1;
    bytes[1..33].copy_from_slice(left);
    bytes[33..].copy_from_slice(right);
    hash(&bytes)
}
fn split(n: usize) -> usize {
    1usize << (usize::BITS - (n - 1).leading_zeros() - 1)
}
fn root(leaves: &[Hash]) -> Hash {
    match leaves.len() {
        0 => hash(b""),
        1 => leaves[0],
        n => {
            let k = split(n);
            node_hash(&root(&leaves[..k]), &root(&leaves[k..]))
        }
    }
}
fn inclusion(index: usize, leaves: &[Hash], out: &mut Vec<Hash>) {
    if leaves.len() <= 1 {
        return;
    }
    let k = split(leaves.len());
    if index < k {
        inclusion(index, &leaves[..k], out);
        out.push(root(&leaves[k..]));
    } else {
        inclusion(index - k, &leaves[k..], out);
        out.push(root(&leaves[..k]));
    }
}
fn consistency(m: usize, leaves: &[Hash], complete: bool, out: &mut Vec<Hash>) {
    if m == leaves.len() {
        if !complete {
            out.push(root(leaves));
        }
        return;
    }
    let k = split(leaves.len());
    if m <= k {
        consistency(m, &leaves[..k], complete, out);
        out.push(root(&leaves[k..]));
    } else {
        consistency(m - k, &leaves[k..], false, out);
        out.push(root(&leaves[..k]));
    }
}
impl AuditLog {
    pub fn new(log_id: String) -> Result<Self> {
        ensure!(!log_id.is_empty(), "missing audit deployment identity");
        Ok(Self {
            log_id,
            events: vec![],
        })
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.log_id.is_empty(), "missing log identity");
        let mut ids = BTreeSet::new();
        let mut last = 0;
        for event in &self.events {
            ensure!(
                ids.insert(&event.id) && !event.id.is_empty() && event.timestamp >= last,
                "invalid audit ordering or duplicate identity"
            );
            last = event.timestamp;
        }
        Ok(())
    }
    pub fn append(&mut self, event: AuditEvent) -> Result<u64> {
        ensure!(
            !event.id.is_empty() && !event.kind.is_empty() && !event.actor.is_empty(),
            "incomplete audit event"
        );
        ensure!(
            !self.events.iter().any(|e| e.id == event.id),
            "duplicate audit event"
        );
        ensure!(
            self.events
                .last()
                .is_none_or(|e| e.timestamp <= event.timestamp),
            "audit time moved backwards"
        );
        let index = self.events.len() as u64;
        self.events.push(event);
        Ok(index)
    }
    /// Broker-generated decisions retain their original signed time in the payload; the log uses commit order.
    pub(crate) fn append_committed(&mut self, mut event: AuditEvent) -> Result<u64> {
        event.timestamp = event
            .timestamp
            .max(crate::cluster::now_ms() / 1000)
            .max(self.events.last().map_or(0, |previous| previous.timestamp));
        self.append(event)
    }
    pub fn events(&self) -> &[AuditEvent] {
        &self.events
    }
    fn leaves(&self) -> Result<Vec<Hash>> {
        self.events.iter().map(leaf_hash).collect()
    }
    pub fn signed_head(&self, signer: &CryptoSigner, timestamp: u64) -> Result<SignedTreeHead> {
        self.validate()?;
        ensure!(
            self.events.last().is_none_or(|e| e.timestamp <= timestamp),
            "head predates audit event"
        );
        signer.sign(
            "audit-tree-head",
            TreeHead {
                log_id: self.log_id.clone(),
                size: self.events.len() as u64,
                root: root(&self.leaves()?),
                timestamp,
            },
        )
    }
    pub fn inclusion(&self, index: u64, size: u64) -> Result<InclusionProof> {
        ensure!(
            index < size && size <= self.events.len() as u64,
            "invalid inclusion range"
        );
        let leaves = self.leaves()?;
        let mut path = vec![];
        inclusion(index as usize, &leaves[..size as usize], &mut path);
        Ok(InclusionProof { index, size, path })
    }
    pub fn consistency(&self, old_size: u64, new_size: u64) -> Result<ConsistencyProof> {
        ensure!(
            old_size <= new_size && new_size <= self.events.len() as u64,
            "invalid consistency range"
        );
        let mut path = vec![];
        if old_size != 0 && old_size != new_size {
            consistency(
                old_size as usize,
                &self.leaves()?[..new_size as usize],
                true,
                &mut path,
            );
        }
        Ok(ConsistencyProof {
            old_size,
            new_size,
            path,
        })
    }
}
pub fn verify_inclusion(event: &AuditEvent, proof: &InclusionProof, head: &TreeHead) -> Result<()> {
    ensure!(
        proof.size == head.size && proof.index < proof.size && proof.path.len() <= 64,
        "invalid inclusion bounds"
    );
    fn rebuild(
        index: usize,
        n: usize,
        leaf: Hash,
        path: &mut std::slice::Iter<'_, Hash>,
    ) -> Result<Hash> {
        if n == 1 {
            return Ok(leaf);
        }
        let k = split(n);
        let own = rebuild(
            if index < k { index } else { index - k },
            if index < k { k } else { n - k },
            leaf,
            path,
        )?;
        let other = path
            .next()
            .ok_or_else(|| anyhow::anyhow!("incomplete inclusion proof"))?;
        Ok(if index < k {
            node_hash(&own, other)
        } else {
            node_hash(other, &own)
        })
    }
    let mut path = proof.path.iter();
    let observed = rebuild(
        proof.index as usize,
        proof.size as usize,
        leaf_hash(event)?,
        &mut path,
    )?;
    ensure!(
        path.next().is_none() && observed == head.root,
        "invalid inclusion proof"
    );
    Ok(())
}

/// Goal-scoped metadata only; the shared signed root proves inclusion without exposing other events.
pub fn goal_view(
    log: &AuditLog,
    goal: &str,
    after_index: u64,
    limit: usize,
    signer: &CryptoSigner,
    now: u64,
) -> Result<serde_json::Value> {
    ensure!(
        !goal.is_empty() && (1..=32).contains(&limit),
        "invalid audit view bounds"
    );
    let size = log.events().len() as u64;
    ensure!(after_index <= size, "audit cursor exceeds retained log");
    let timestamp = now.max(log.events().last().map_or(0, |event| event.timestamp));
    let head = log.signed_head(signer, timestamp)?;
    let mut entries = Vec::new();
    let mut scanned_through = after_index;
    for (index, event) in log.events().iter().enumerate().skip(after_index as usize) {
        scanned_through = index as u64 + 1;
        if event.goal.as_deref() == Some(goal) {
            entries
                .push(serde_json::json!({"event":event,"proof":log.inclusion(index as u64,size)?}));
            if entries.len() == limit {
                break;
            }
        }
    }
    Ok(
        serde_json::json!({"goal_id":goal,"entries":entries,"scanned_through":scanned_through,"head":head}),
    )
}
pub fn verify_consistency(old: &TreeHead, new: &TreeHead, proof: &ConsistencyProof) -> Result<()> {
    ensure!(
        old.log_id == new.log_id && old.timestamp <= new.timestamp && old.size <= new.size,
        "log fork or stale head"
    );
    ensure!(
        proof.old_size == old.size && proof.new_size == new.size && proof.path.len() <= 64,
        "invalid consistency bounds"
    );
    if old.size == 0 {
        ensure!(
            old.root == hash(b"") && proof.path.is_empty(),
            "invalid empty-tree proof"
        );
        return Ok(());
    }
    if old.size == new.size {
        ensure!(
            old.root == new.root && proof.path.is_empty(),
            "same-size log fork"
        );
        return Ok(());
    }
    let mut first = old.size - 1;
    let mut last = new.size - 1;
    while first & 1 != 0 {
        first >>= 1;
        last >>= 1;
    }
    let mut path = proof.path.iter();
    let mut old_root = if first == 0 {
        old.root
    } else {
        *path
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing consistency seed"))?
    };
    let mut new_root = old_root;
    for sibling in path {
        ensure!(last != 0, "excess consistency proof");
        if first & 1 != 0 || first == last {
            old_root = node_hash(sibling, &old_root);
            new_root = node_hash(sibling, &new_root);
            while first & 1 == 0 && first != 0 {
                first >>= 1;
                last >>= 1;
            }
        } else {
            new_root = node_hash(&new_root, sibling);
        }
        first >>= 1;
        last >>= 1;
    }
    ensure!(
        last == 0 && old_root == old.root && new_root == new.root,
        "invalid consistency proof"
    );
    Ok(())
}

/// Read-only adapter. The operator must place this store outside control-plane write authority.
pub struct WitnessStore {
    path: PathBuf,
    pub trusted_key: Hash,
    pub log_id: String,
}
impl WitnessStore {
    pub fn read_only(path: &Path, trusted_key: Hash, log_id: String) -> Self {
        Self {
            path: path.into(),
            trusted_key,
            log_id,
        }
    }
    pub fn latest(&self) -> Result<SignedTreeHead> {
        let head: SignedTreeHead = serde_json::from_slice(&fs::read(&self.path)?)?;
        head.verify("audit-tree-head", &self.trusted_key)?;
        ensure!(
            head.payload.log_id == self.log_id,
            "witness deployment mismatch"
        );
        Ok(head)
    }
    pub fn verify_extension(&self, new: &SignedTreeHead, proof: &ConsistencyProof) -> Result<()> {
        new.verify("audit-tree-head", &self.trusted_key)?;
        verify_consistency(&self.latest()?.payload, &new.payload, proof)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn committed_stamps_order_delayed_decisions_while_raw_history_remains_strict() {
        let mut log = AuditLog::new("concurrent-decisions".into()).unwrap();
        let now = crate::cluster::now_ms() / 1000;
        let event = |id: &str, time| AuditEvent {
            id: id.into(),
            kind: "decision".into(),
            actor: "broker".into(),
            goal: Some("g".into()),
            timestamp: time,
            decision: "admitted".into(),
            payload_hash: hash(id.as_bytes()),
        };
        log.append(event("later", now)).unwrap();
        assert!(log.append(event("delayed-raw", now - 1)).is_err());
        log.append_committed(event("delayed-commit", now - 1))
            .unwrap();
        log.validate().unwrap();
        assert!(log.events()[1].timestamp >= log.events()[0].timestamp);
        let signer = CryptoSigner::from_seed([10; 32]);
        let head = log.signed_head(&signer, log.events()[1].timestamp).unwrap();
        verify_inclusion(
            &log.events()[1],
            &log.inclusion(1, 2).unwrap(),
            &head.payload,
        )
        .unwrap();
        log.events[1].timestamp = now - 1;
        assert!(log.validate().is_err());
        assert!(
            verify_inclusion(&log.events[1], &log.inclusion(1, 2).unwrap(), &head.payload).is_err()
        );
    }
    #[test]
    fn goal_views_paginate_only_authorized_metadata_and_prove_each_entry() {
        let signer = CryptoSigner::from_seed([9; 32]);
        let mut log = AuditLog::new("scoped-log".into()).unwrap();
        for (index, goal) in [Some("g"), Some("other"), None, Some("g"), Some("other")]
            .into_iter()
            .enumerate()
        {
            log.append(AuditEvent {
                id: index.to_string(),
                kind: "decision".into(),
                actor: "actor".into(),
                goal: goal.map(String::from),
                timestamp: index as u64,
                decision: "accept".into(),
                payload_hash: hash(&[index as u8]),
            })
            .unwrap();
        }
        let first = goal_view(&log, "g", 0, 1, &signer, 0).unwrap();
        assert_eq!(first["scanned_through"], 1);
        let second = goal_view(&log, "g", 1, 32, &signer, 0).unwrap();
        assert_eq!(second["scanned_through"], 5);
        assert_eq!(second["entries"].as_array().unwrap().len(), 1);
        for view in [first, second] {
            let head: SignedTreeHead = serde_json::from_value(view["head"].clone()).unwrap();
            head.verify("audit-tree-head", &signer.public_key())
                .unwrap();
            for entry in view["entries"].as_array().unwrap() {
                let event: AuditEvent = serde_json::from_value(entry["event"].clone()).unwrap();
                let proof: InclusionProof = serde_json::from_value(entry["proof"].clone()).unwrap();
                assert_eq!(event.goal.as_deref(), Some("g"));
                verify_inclusion(&event, &proof, &head.payload).unwrap();
            }
        }
        assert!(goal_view(&log, "g", 6, 32, &signer, 0).is_err());
        assert!(goal_view(&log, "g", 0, 33, &signer, 0).is_err());
    }
    #[test]
    fn merkle_proofs_cover_uneven_trees_and_detect_tampering() {
        let signer = CryptoSigner::from_seed([7; 32]);
        let mut log = AuditLog::new("deployment-1".into()).unwrap();
        for n in 0..64 {
            log.append(AuditEvent {
                id: n.to_string(),
                kind: "decision".into(),
                actor: "operator".into(),
                goal: None,
                timestamp: n,
                decision: "admitted".into(),
                payload_hash: hash(&n.to_be_bytes()),
            })
            .unwrap();
        }
        for size in 1..=64 {
            let head = TreeHead {
                log_id: log.log_id.clone(),
                size,
                root: root(&log.leaves().unwrap()[..size as usize]),
                timestamp: 64,
            };
            for index in 0..size {
                let proof = log.inclusion(index, size).unwrap();
                verify_inclusion(&log.events[index as usize], &proof, &head).unwrap();
                let mut bad = log.events[index as usize].clone();
                bad.actor.push('x');
                assert!(verify_inclusion(&bad, &proof, &head).is_err());
            }
            for old_size in 0..=size {
                let old = TreeHead {
                    log_id: head.log_id.clone(),
                    size: old_size,
                    root: root(&log.leaves().unwrap()[..old_size as usize]),
                    timestamp: 64,
                };
                let proof = log.consistency(old_size, size).unwrap();
                verify_consistency(&old, &head, &proof).unwrap();
                if !proof.path.is_empty() {
                    let mut bad = proof.clone();
                    bad.path[0][0] ^= 1;
                    assert!(verify_consistency(&old, &head, &bad).is_err());
                }
            }
        }
        let head = log.signed_head(&signer, 64).unwrap();
        head.verify("audit-tree-head", &signer.public_key())
            .unwrap();
        let mut bad = head.clone();
        bad.payload.size += 1;
        assert!(bad.verify("audit-tree-head", &signer.public_key()).is_err());
        assert!(head.verify("price-table", &signer.public_key()).is_err());
        let roundtrip: AuditLog =
            serde_json::from_slice(&serde_json::to_vec(&log).unwrap()).unwrap();
        roundtrip.validate().unwrap();
        assert_eq!(
            roundtrip.signed_head(&signer, 64).unwrap().payload,
            head.payload
        );
    }
}
