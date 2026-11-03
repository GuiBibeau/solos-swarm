use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
};
use zeroize::Zeroizing;

/// What a write's temporary file is named, beside the file it replaces.
pub const TEMPORARY_PREFIX: &str = ".write-";
const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;

#[derive(Serialize, Deserialize)]
struct Envelope {
    schema_version: u32,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

pub struct Key(Zeroizing<[u8; KEY_BYTES]>);

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

impl Key {
    pub fn create(path: &Path) -> Result<()> {
        create_key_file(path)
    }

    pub fn read(path: &Path) -> Result<Self> {
        read_key_file(path).map(Self)
    }

    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_BYTES];
        getrandom::fill(&mut nonce).map_err(|_| anyhow!("OS random source unavailable"))?;
        let ciphertext = self
            .cipher()
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| anyhow!("encryption failed"))?;
        Ok(serde_json::to_vec(&Envelope {
            schema_version: 1,
            nonce: nonce.to_vec(),
            ciphertext,
        })?)
    }

    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let envelope: Envelope = serde_json::from_slice(sealed).context("not a sealed envelope")?;
        ensure!(
            envelope.schema_version == 1 && envelope.nonce.len() == NONCE_BYTES,
            "unsupported sealed envelope format"
        );
        let plaintext = self
            .cipher()
            .decrypt(
                Nonce::from_slice(&envelope.nonce),
                Payload {
                    msg: &envelope.ciphertext,
                    aad,
                },
            )
            .map_err(|_| anyhow!("authentication failed: a different key, or changed bytes"))?;
        Ok(Zeroizing::new(plaintext))
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(aes_gcm::Key::<Aes256Gcm>::from_slice(&*self.0))
    }
}

/// 32 random bytes at `path`, mode 0600. An existing file is never replaced.
pub fn create_key_file(path: &Path) -> Result<()> {
    let mut key = Zeroizing::new([0u8; KEY_BYTES]);
    getrandom::fill(&mut *key).map_err(|_| anyhow!("OS random source unavailable"))?;
    private_file(path, &*key, false)
}

/// Refused: a missing file, a symlink, a file that group or others may read
/// or write, or one that is not 32 bytes.
pub fn read_key_file(path: &Path) -> Result<Zeroizing<[u8; KEY_BYTES]>> {
    let shown = path.display();
    ensure!(path.is_file(), "the key file {shown} is missing");
    ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "the key file {shown} is a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            fs::metadata(path)?.permissions().mode() & 0o077 == 0,
            "the key file {shown} is open to group or others; chmod 600 it"
        );
    }
    let bytes = Zeroizing::new(fs::read(path)?);
    let mut key = Zeroizing::new([0u8; KEY_BYTES]);
    ensure!(
        bytes.len() == KEY_BYTES,
        "the key file {shown} does not hold exactly {KEY_BYTES} bytes"
    );
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// Write `bytes` to `path` with mode 0600, all or nothing: a temporary file
/// in the same directory, synced, then renamed over `path` (`overwrite`) or
/// linked to it, which fails when `path` exists.
pub fn private_file(path: &Path, bytes: &[u8], overwrite: bool) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!("{TEMPORARY_PREFIX}{}", uuid::Uuid::new_v4()));
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

#[cfg(test)]
mod tests {
    use super::*;

    mod legacy {
        use aes_gcm::{
            Aes256Gcm, Nonce,
            aead::{Aead, KeyInit, Payload},
        };
        use anyhow::{Context, Result, ensure};
        use serde::{Deserialize, Serialize};
        use zeroize::Zeroizing;

        #[derive(Serialize, Deserialize)]
        struct Encrypted {
            schema_version: u32,
            nonce: Vec<u8>,
            ciphertext: Vec<u8>,
        }

        pub fn encrypt(key: &[u8], bytes: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
            let cipher = Aes256Gcm::new_from_slice(key)
                .map_err(|_| anyhow::anyhow!("invalid encryption key"))?;
            let mut nonce = [0u8; 12];
            getrandom::fill(&mut nonce)
                .map_err(|_| anyhow::anyhow!("OS random source unavailable"))?;
            let ciphertext = cipher
                .encrypt(Nonce::from_slice(&nonce), Payload { msg: bytes, aad })
                .map_err(|_| anyhow::anyhow!("artifact encryption failed"))?;
            Ok(serde_json::to_vec(&Encrypted {
                schema_version: 1,
                nonce: nonce.to_vec(),
                ciphertext,
            })?)
        }

        pub fn decrypt(key: &[u8], bytes: &[u8], aad: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
            let value: Encrypted =
                serde_json::from_slice(bytes).context("encrypted artifact envelope invalid")?;
            ensure!(
                value.schema_version == 1 && value.nonce.len() == 12,
                "unsupported encrypted artifact format"
            );
            let cipher = Aes256Gcm::new_from_slice(key)
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
    }

    fn key_file() -> (std::path::PathBuf, Key, Vec<u8>) {
        let dir = std::env::temp_dir().join(format!("sealed-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("key");
        Key::create(&path).unwrap();
        let bytes = fs::read(&path).unwrap();
        (dir, Key::read(&path).unwrap(), bytes)
    }

    #[test]
    fn bytes_sealed_the_old_way_open_and_the_reverse() {
        let (dir, key, raw) = key_file();
        let aad = b"solos-swarm/index/v1";
        let old = legacy::encrypt(&raw, b"an index written before sealed.rs", aad).unwrap();
        assert_eq!(
            key.open(&old, aad).unwrap().as_slice(),
            b"an index written before sealed.rs"
        );
        let new = key.seal(b"an index written by sealed.rs", aad).unwrap();
        assert_eq!(
            legacy::decrypt(&raw, &new, aad).unwrap().as_slice(),
            b"an index written by sealed.rs"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_different_key_or_aad_does_not_open() {
        let (dir, key, _) = key_file();
        let (other_dir, other, _) = key_file();
        let sealed = key.seal(b"payload", b"one").unwrap();
        assert!(other.open(&sealed, b"one").is_err());
        assert!(key.open(&sealed, b"two").is_err());
        assert!(Key::create(&dir.join("key")).is_err());
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(other_dir).unwrap();
    }
}
