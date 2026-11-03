use super::{ItemInfo, SecretValue};
use crate::sealed::{Key, private_file};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

const AAD: &[u8] = b"solos-swarm/vault/v1";

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Contents {
    pub(super) items: Vec<Stored>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Stored {
    pub(super) info: ItemInfo,
    #[serde(serialize_with = "write_value", deserialize_with = "read_value")]
    pub(super) value: SecretValue,
}

fn write_value<S: Serializer>(value: &SecretValue, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(value.reveal())
}

fn read_value<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SecretValue, D::Error> {
    String::deserialize(deserializer).map(|text| SecretValue(Zeroizing::new(text)))
}

pub(super) fn read(path: &Path, key: &Key) -> Result<Contents> {
    let shown = path.display();
    let sealed = fs::read(path).with_context(|| {
        format!("cannot read the vault {shown}; `solos-swarm vault init` makes it")
    })?;
    let plain = key
        .open(&sealed, AAD)
        .with_context(|| format!("the vault {shown} does not open under its key"))?;
    serde_json::from_slice(&plain).map_err(|_| anyhow!("the vault {shown} holds no item list"))
}

pub(super) fn create(path: &Path, key: &Key) -> Result<()> {
    write(path, key, &Contents::default(), false)
}

pub(super) fn update<T>(
    path: &Path,
    key: &Key,
    change: impl FnOnce(&mut Contents) -> Result<T>,
) -> Result<T> {
    let _lock = lock(path)?;
    let mut contents = read(path, key)?;
    let answer = change(&mut contents)?;
    write(path, key, &contents, true)?;
    Ok(answer)
}

/// The lock file stays, and the kernel releases the lock when its holder
/// exits, however it exits.
pub(super) fn lock(path: &Path) -> Result<File> {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let lock = PathBuf::from(name);
    if let Some(parent) = lock.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock)
        .with_context(|| format!("cannot open {}", lock.display()))?;
    file.lock()
        .with_context(|| format!("cannot lock {}", lock.display()))?;
    Ok(file)
}

fn write(path: &Path, key: &Key, contents: &Contents, overwrite: bool) -> Result<()> {
    let plain = Zeroizing::new(serde_json::to_vec(contents)?);
    private_file(path, &key.seal(&plain, AAD)?, overwrite)
}
