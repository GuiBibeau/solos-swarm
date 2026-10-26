//! Encrypted immutable payloads and scoped, offline prepared artifact versions.
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use zeroize::{Zeroize, Zeroizing};

const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|c| c.is_ascii_hexdigit())
}
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        && name != "."
        && name != ".."
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactMetadata {
    pub goal_id: String,
    pub logical_name: String,
    pub version: String,
    pub classifications: BTreeSet<String>,
    pub trust: u8,
    pub producer_id: String,
    pub broker_id: String,
    pub provenance: serde_json::Value,
    pub parents: Vec<String>,
    pub signature: String,
    pub created_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactInfo {
    pub id: String,
    pub body_sha256: String,
    pub size_bytes: usize,
    pub metadata: ArtifactMetadata,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadPermit {
    pub holder_id: String,
    pub goal_ids: BTreeSet<String>,
    pub classifications: BTreeSet<String>,
    pub expiry_ms: u64,
    pub allow_retracted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportPermit {
    pub read: ReadPermit,
    pub destination_root: PathBuf,
    pub recipient_id: String,
    pub allowed_recipients: BTreeSet<String>,
    pub max_bytes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedVersion {
    pub namespace: String,
    pub version: String,
    pub artifact_ids: Vec<String>,
    pub digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactRetraction {
    pub id: String,
    pub actor_id: String,
    pub reason: String,
    pub signature: String,
    pub recorded_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Index {
    revision: u64,
    records: BTreeMap<String, ArtifactInfo>,
    children: BTreeMap<String, BTreeSet<String>>,
    retracted: BTreeSet<String>,
    retractions: Vec<ArtifactRetraction>,
    versions: BTreeMap<String, PreparedVersion>,
}

#[derive(Serialize, Deserialize)]
struct Encrypted {
    schema_version: u32,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}
#[derive(Serialize, Deserialize)]
struct Blob {
    info: ArtifactInfo,
    bytes: Vec<u8>,
}
impl Drop for Blob {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

pub struct ArtifactStore {
    root: PathBuf,
    key: Zeroizing<Vec<u8>>,
    index: Index,
}
impl std::fmt::Debug for ArtifactStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactStore")
            .field("root", &self.root)
            .field("revision", &self.index.revision)
            .field("records", &self.index.records.len())
            .finish_non_exhaustive()
    }
}

fn private_file(path: &Path, bytes: &[u8], overwrite: bool) -> Result<()> {
    let parent = path.parent().context("artifact parent missing")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".write-{}", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if overwrite {
            fs::rename(&temporary, path)?;
        } else {
            fs::hard_link(&temporary, path)?;
            fs::remove_file(&temporary)?;
        }
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

impl ArtifactStore {
    /// Provisioning only: no key is invented or downloaded during normal dispatch.
    pub fn create_key(path: &Path) -> Result<()> {
        let mut key = Zeroizing::new(vec![0u8; 32]);
        getrandom::fill(&mut key).map_err(|_| anyhow::anyhow!("OS random source unavailable"))?;
        private_file(path, &key, false)
    }
    pub fn open(root: &Path, key_file: &Path) -> Result<Self> {
        ensure!(
            key_file.is_file(),
            "operator-provisioned encryption key missing"
        );
        ensure!(
            !fs::symlink_metadata(key_file)?.file_type().is_symlink(),
            "key symlink is not admitted"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                fs::metadata(key_file)?.permissions().mode() & 0o077 == 0,
                "encryption key permissions are not private"
            );
        }
        let key_file = key_file.canonicalize()?;
        fs::create_dir_all(root)?;
        ensure!(
            !fs::symlink_metadata(root)?.file_type().is_symlink(),
            "artifact root symlink is not admitted"
        );
        let root = root.canonicalize()?;
        ensure!(
            !key_file.starts_with(&root),
            "encryption key must be outside artifact storage"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        let key = Zeroizing::new(fs::read(key_file)?);
        ensure!(key.len() == 32, "AES-256 key must have exactly 32 bytes");
        let mut store = Self {
            root,
            key,
            index: Index::default(),
        };
        let index = store.root.join("index.enc");
        if index.exists() {
            let plaintext = store.decrypt(&fs::read(index)?, b"solos-swarm/index/v1")?;
            store.index =
                serde_json::from_slice(&plaintext).context("encrypted artifact index invalid")?;
        }
        Ok(store)
    }
    pub fn revision(&self) -> u64 {
        self.index.revision
    }
    fn encrypt(&self, bytes: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|_| anyhow::anyhow!("invalid encryption key"))?;
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(|_| anyhow::anyhow!("OS random source unavailable"))?;
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: bytes, aad })
            .map_err(|_| anyhow::anyhow!("artifact encryption failed"))?;
        Ok(serde_json::to_vec(&Encrypted {
            schema_version: 1,
            nonce: nonce.to_vec(),
            ciphertext,
        })?)
    }
    fn decrypt(&self, bytes: &[u8], aad: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let value: Encrypted =
            serde_json::from_slice(bytes).context("encrypted artifact envelope invalid")?;
        ensure!(
            value.schema_version == 1 && value.nonce.len() == 12,
            "unsupported encrypted artifact format"
        );
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|_| anyhow::anyhow!("invalid encryption key"))?;
        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(&value.nonce),
                Payload {
                    msg: &value.ciphertext,
                    aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("artifact authentication failed"))?;
        Ok(Zeroizing::new(plaintext))
    }
    fn save(&self, index: &Index) -> Result<()> {
        let plain = Zeroizing::new(serde_json::to_vec(index)?);
        private_file(
            &self.root.join("index.enc"),
            &self.encrypt(&plain, b"solos-swarm/index/v1")?,
            true,
        )
    }
    pub fn put(&mut self, bytes: &[u8], mut metadata: ArtifactMetadata) -> Result<ArtifactInfo> {
        ensure!(
            bytes.len() <= MAX_BLOB_BYTES,
            "large payload must use bounded content-addressed chunks"
        );
        ensure!(
            !metadata.goal_id.is_empty()
                && safe_name(&metadata.logical_name)
                && safe_name(&metadata.version)
                && !metadata.classifications.is_empty()
                && metadata.trust <= 2,
            "artifact declaration incomplete"
        );
        ensure!(
            !metadata.producer_id.is_empty()
                && !metadata.broker_id.is_empty()
                && metadata.producer_id != metadata.broker_id
                && !metadata.signature.is_empty()
                && metadata.provenance.is_object(),
            "verified broker provenance required"
        );
        let mut parents = BTreeSet::new();
        for id in &metadata.parents {
            ensure!(
                parents.insert(id) && self.eligible(id),
                "duplicate, unknown or retracted artifact parent"
            );
            let source = &self.index.records[id];
            ensure!(
                source.metadata.goal_id == metadata.goal_id,
                "cross-Goal artifact derivation denied"
            );
            metadata
                .classifications
                .extend(source.metadata.classifications.iter().cloned());
            metadata.trust = metadata.trust.min(source.metadata.trust);
        }
        let body_sha256 = hash(bytes);
        let id = crate::domain::hash_json(
            &serde_json::json!({"body_sha256":body_sha256,"metadata":metadata}),
        )?;
        if let Some(existing) = self.index.records.get(&id) {
            return Ok(existing.clone());
        }
        let info = ArtifactInfo {
            id: id.clone(),
            body_sha256,
            size_bytes: bytes.len(),
            metadata,
        };
        let blob = Blob {
            info: info.clone(),
            bytes: bytes.to_vec(),
        };
        let plain = Zeroizing::new(serde_json::to_vec(&blob)?);
        let encrypted = self.encrypt(&plain, id.as_bytes())?;
        let path = self.root.join(format!("{id}.enc"));
        if path.exists() {
            let previous = self.decrypt(&fs::read(&path)?, id.as_bytes())?;
            let old: Blob = serde_json::from_slice(&previous)?;
            ensure!(
                crate::domain::hash_json(&old.info)? == crate::domain::hash_json(&info)?
                    && hash(&old.bytes) == info.body_sha256,
                "orphan artifact does not match immutable retry"
            );
        } else {
            private_file(&path, &encrypted, false)?;
        }
        let mut index = self.index.clone();
        index.revision = index
            .revision
            .checked_add(1)
            .context("artifact revision exhausted")?;
        for parent in &info.metadata.parents {
            index
                .children
                .entry(parent.clone())
                .or_default()
                .insert(id.clone());
        }
        index.records.insert(id, info.clone());
        self.save(&index)?;
        self.index = index;
        Ok(info)
    }
    pub fn eligible(&self, id: &str) -> bool {
        let mut pending = vec![id];
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            if self.index.retracted.contains(id) {
                return false;
            }
            let Some(info) = self.index.records.get(id) else {
                return false;
            };
            pending.extend(info.metadata.parents.iter().map(String::as_str));
        }
        true
    }
    fn permit(&self, id: &str, permit: &ReadPermit, now_ms: u64) -> Result<&ArtifactInfo> {
        ensure!(
            valid_id(id) && !permit.holder_id.is_empty() && permit.expiry_ms > now_ms,
            "expired or invalid artifact read permit"
        );
        let info = self.index.records.get(id).context("unknown artifact")?;
        ensure!(
            permit.goal_ids.contains(&info.metadata.goal_id)
                && info
                    .metadata
                    .classifications
                    .is_subset(&permit.classifications),
            "artifact outside scope or clearance"
        );
        ensure!(
            permit.allow_retracted || self.eligible(id),
            "artifact lineage is retracted"
        );
        Ok(info)
    }
    pub fn inspect(&self, id: &str, permit: &ReadPermit, now_ms: u64) -> Result<ArtifactInfo> {
        Ok(self.permit(id, permit, now_ms)?.clone())
    }
    pub fn get(&self, id: &str, permit: &ReadPermit, now_ms: u64) -> Result<Zeroizing<Vec<u8>>> {
        let expected = self.permit(id, permit, now_ms)?;
        let path = self.root.join(format!("{id}.enc"));
        ensure!(
            fs::metadata(&path)?.len() <= MAX_BLOB_BYTES as u64 * 10,
            "encrypted artifact exceeds storage bound"
        );
        let plain = self.decrypt(&fs::read(path)?, id.as_bytes())?;
        let mut blob: Blob =
            serde_json::from_slice(&plain).context("decrypted artifact invalid")?;
        ensure!(
            blob.info.id == id
                && crate::domain::hash_json(&blob.info)? == crate::domain::hash_json(expected)?
                && blob.bytes.len() == expected.size_bytes
                && hash(&blob.bytes) == expected.body_sha256,
            "artifact content or provenance digest mismatch"
        );
        Ok(Zeroizing::new(std::mem::take(&mut blob.bytes)))
    }
    pub fn retract(
        &mut self,
        id: &str,
        actor_id: &str,
        reason: &str,
        verified_signature: &str,
        now_ms: u64,
    ) -> Result<BTreeSet<String>> {
        ensure!(
            self.index.records.contains_key(id)
                && !actor_id.is_empty()
                && !reason.is_empty()
                && !verified_signature.is_empty(),
            "verified scoped retraction required"
        );
        let mut index = self.index.clone();
        let mut queue = VecDeque::from([id.to_owned()]);
        let mut affected = BTreeSet::new();
        while let Some(current) = queue.pop_front() {
            if !affected.insert(current.clone()) {
                continue;
            }
            index.retracted.insert(current.clone());
            if let Some(children) = index.children.get(&current) {
                queue.extend(children.iter().cloned());
            }
        }
        if !self.index.retracted.contains(id) {
            index.retractions.push(ArtifactRetraction {
                id: id.into(),
                actor_id: actor_id.into(),
                reason: reason.into(),
                signature: verified_signature.into(),
                recorded_ms: now_ms,
            });
        }
        index.revision = index
            .revision
            .checked_add(1)
            .context("artifact revision exhausted")?;
        self.save(&index)?;
        self.index = index;
        Ok(affected)
    }
    pub fn export(
        &self,
        id: &str,
        file_name: &str,
        permit: &ExportPermit,
        now_ms: u64,
    ) -> Result<PathBuf> {
        ensure!(
            safe_name(file_name)
                && permit.allowed_recipients.contains(&permit.recipient_id)
                && !permit.recipient_id.is_empty()
                && permit.max_bytes > 0
                && !permit.read.allow_retracted,
            "export recipient or ownership permit invalid"
        );
        let info = self.permit(id, &permit.read, now_ms)?;
        ensure!(
            info.size_bytes <= permit.max_bytes,
            "export byte ceiling exceeded"
        );
        ensure!(
            permit.destination_root.is_dir()
                && !fs::symlink_metadata(&permit.destination_root)?
                    .file_type()
                    .is_symlink(),
            "export root not prepared"
        );
        let destination = permit.destination_root.canonicalize()?.join(file_name);
        private_file(&destination, &self.get(id, &permit.read, now_ms)?, false)?;
        Ok(destination)
    }
    pub fn prepare_version(
        &mut self,
        namespace: &str,
        version: &str,
        ids: Vec<String>,
        permit: &ReadPermit,
        now_ms: u64,
    ) -> Result<PreparedVersion> {
        ensure!(
            safe_name(namespace) && safe_name(version) && !ids.is_empty(),
            "prepared version incomplete"
        );
        let mut seen = BTreeSet::new();
        for id in &ids {
            ensure!(seen.insert(id), "duplicate prepared artifact");
            self.permit(id, permit, now_ms)?;
        }
        let digest = crate::domain::hash_json(
            &serde_json::json!({"namespace":namespace,"version":version,"artifact_ids":ids}),
        )?;
        let record = PreparedVersion {
            namespace: namespace.into(),
            version: version.into(),
            artifact_ids: ids,
            digest,
        };
        let key = format!("{namespace}/{version}");
        if let Some(existing) = self.index.versions.get(&key) {
            ensure!(
                existing.digest == record.digest,
                "immutable prepared version conflict"
            );
            return Ok(existing.clone());
        }
        let mut index = self.index.clone();
        index.versions.insert(key, record.clone());
        index.revision = index
            .revision
            .checked_add(1)
            .context("artifact revision exhausted")?;
        self.save(&index)?;
        self.index = index;
        Ok(record)
    }
    pub fn prepared_version(
        &self,
        namespace: &str,
        version: &str,
        permit: &ReadPermit,
        now_ms: u64,
    ) -> Result<PreparedVersion> {
        let record = self
            .index
            .versions
            .get(&format!("{namespace}/{version}"))
            .context("prepared version not imported; ambient downloads are disabled")?;
        for id in &record.artifact_ids {
            self.permit(id, permit, now_ms)?;
        }
        Ok(record.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn metadata(name: &str, parents: Vec<String>) -> ArtifactMetadata {
        ArtifactMetadata {
            goal_id: "goal".into(),
            logical_name: name.into(),
            version: "1".into(),
            classifications: BTreeSet::from(["private".into()]),
            trust: 0,
            producer_id: "agent".into(),
            broker_id: "broker".into(),
            provenance: serde_json::json!({"observed":true}),
            parents,
            signature: "verified".into(),
            created_ms: 1,
        }
    }
    fn permit() -> ReadPermit {
        ReadPermit {
            holder_id: "operator".into(),
            goal_ids: BTreeSet::from(["goal".into()]),
            classifications: BTreeSet::from(["public".into(), "private".into()]),
            expiry_ms: 1000,
            allow_retracted: false,
        }
    }
    #[test]
    fn encrypted_classified_versioned_payloads_retract_without_erasing_history() {
        let root =
            std::env::temp_dir().join(format!("swarm-artifact-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let key = root.join("keys/key.bin");
        ArtifactStore::create_key(&key).unwrap();
        let blobs = root.join("blobs");
        let mut store = ArtifactStore::open(&blobs, &key).unwrap();
        let source = store
            .put(b"sensitive unique fixture text", metadata("source", vec![]))
            .unwrap();
        let mut derived_metadata = metadata("derived", vec![source.id.clone()]);
        derived_metadata.classifications = BTreeSet::from(["public".into()]);
        derived_metadata.trust = 2;
        let derived = store.put(b"derived answer", derived_metadata).unwrap();
        assert_eq!(derived.metadata.trust, 0);
        assert!(derived.metadata.classifications.contains("private"));
        assert!(
            !String::from_utf8(fs::read(blobs.join(format!("{}.enc", source.id))).unwrap())
                .unwrap()
                .contains("sensitive unique fixture text")
        );
        assert!(
            !String::from_utf8(fs::read(blobs.join("index.enc")).unwrap())
                .unwrap()
                .contains("private")
        );
        let mut denied = permit();
        denied.classifications = BTreeSet::from(["public".into()]);
        assert!(store.get(&source.id, &denied, 2).is_err());
        assert_eq!(
            store.get(&source.id, &permit(), 2).unwrap().as_slice(),
            b"sensitive unique fixture text"
        );
        let prepared = store
            .prepare_version("model", "1", vec![source.id.clone()], &permit(), 2)
            .unwrap();
        assert_eq!(
            prepared.digest,
            store
                .prepared_version("model", "1", &permit(), 2)
                .unwrap()
                .digest
        );
        assert!(
            store
                .prepare_version("model", "1", vec![derived.id.clone()], &permit(), 2)
                .is_err()
        );
        let affected = store
            .retract(&source.id, "operator", "bad source", "verified", 3)
            .unwrap();
        assert_eq!(affected.len(), 2);
        assert!(store.get(&derived.id, &permit(), 4).is_err());
        drop(store);
        let store = ArtifactStore::open(&blobs, &key).unwrap();
        assert!(!store.eligible(&derived.id));
        let mut auditor = permit();
        auditor.allow_retracted = true;
        assert!(store.get(&derived.id, &auditor, 4).is_ok());
        assert!(ArtifactStore::open(&blobs, &blobs.join("key.bin")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn authenticated_storage_and_export_scope_fail_closed() {
        let root =
            std::env::temp_dir().join(format!("swarm-artifact-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let key = root.join("key.bin");
        ArtifactStore::create_key(&key).unwrap();
        let blobs = root.join("blobs");
        let mut store = ArtifactStore::open(&blobs, &key).unwrap();
        let info = store.put(b"data", metadata("source", vec![])).unwrap();
        let destination = root.join("approved-export");
        fs::create_dir(&destination).unwrap();
        let export = ExportPermit {
            read: permit(),
            destination_root: destination,
            recipient_id: "recipient".into(),
            allowed_recipients: BTreeSet::from(["recipient".into()]),
            max_bytes: 10,
        };
        assert!(store.export(&info.id, "../escape", &export, 2).is_err());
        let path = store.export(&info.id, "result.bin", &export, 2).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"data");
        assert!(store.export(&info.id, "result.bin", &export, 2).is_err());
        let path = blobs.join(format!("{}.enc", info.id));
        let mut encrypted: Encrypted = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        encrypted.ciphertext[0] ^= 1;
        fs::write(path, serde_json::to_vec(&encrypted).unwrap()).unwrap();
        assert!(store.get(&info.id, &permit(), 2).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
