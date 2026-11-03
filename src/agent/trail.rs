//! The trail's blob store: the page each browser step captured, the exact
//! View its model got, and the text it typed, kept by digest so the chain
//! holds only digests.

use super::pi_executor::{dir_name, private_dir};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use solos_agent_core::model::TicketId;
use solos_agent_core::trail::Digest;
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// `trail:` in agent.yaml.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrailStoreConfig {
    /// Default: `<state>/trail`. Refused where a sandbox can read it.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Past it, no Ticket's browser starts a step.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    /// Past it, this Ticket's browser starts no step.
    #[serde(default = "default_ticket_max_bytes")]
    pub ticket_max_bytes: u64,
}

impl Default for TrailStoreConfig {
    fn default() -> Self {
        Self {
            path: None,
            max_bytes: default_max_bytes(),
            ticket_max_bytes: default_ticket_max_bytes(),
        }
    }
}

impl TrailStoreConfig {
    /// `path`, or `<state>/trail`.
    pub fn root(&self, state_dir: &Path) -> PathBuf {
        self.path
            .clone()
            .unwrap_or_else(|| state_dir.join(super::TRAIL_DIR))
    }
}

fn default_max_bytes() -> u64 {
    8 << 30
}

fn default_ticket_max_bytes() -> u64 {
    256 << 20
}

/// `<root>/<ticket>/<64 hex>`, each file written whole or not at all, 0600
/// in 0700 directories.
pub struct BlobStore {
    root: PathBuf,
    max_bytes: u64,
    ticket_max_bytes: u64,
    used: Mutex<Used>,
}

struct Used {
    total: u64,
    tickets: BTreeMap<TicketId, u64>,
}

impl BlobStore {
    /// Count what the store holds, and remove what a killed write left.
    pub fn open(root: &Path, config: &TrailStoreConfig) -> Result<Self> {
        ensure!(
            (1..=config.max_bytes).contains(&config.ticket_max_bytes),
            "trail.ticket_max_bytes must be above zero and at most trail.max_bytes"
        );
        private_dir(root)?;
        let mut total = 0;
        for ticket in std::fs::read_dir(root)? {
            let ticket = ticket?.path();
            if ticket.is_dir() {
                remove_killed_writes(&ticket)?;
                total += size(&ticket)?;
            }
        }
        Ok(Self {
            root: root.to_owned(),
            max_bytes: config.max_bytes,
            ticket_max_bytes: config.ticket_max_bytes,
            used: Mutex::new(Used {
                total,
                tickets: BTreeMap::new(),
            }),
        })
    }

    /// Whether `ticket`'s browser may start a step. The step that crosses a
    /// cap keeps every blob it names; the next one is refused.
    pub fn room(&self, ticket: &TicketId) -> Result<(), String> {
        let mut used = self.lock();
        let held = self
            .held(&mut used, ticket)
            .map_err(|err| format!("{err:#}"))?;
        if used.total >= self.max_bytes {
            return Err(format!(
                "the trail store is full ({} of {} bytes), so no browser step starts until the \
                 operator makes room",
                used.total, self.max_bytes
            ));
        }
        if held >= self.ticket_max_bytes {
            return Err(format!(
                "this ticket's browser steps have stored their most ({} bytes); report what you \
                 have",
                self.ticket_max_bytes
            ));
        }
        Ok(())
    }

    /// Keep `bytes` for `ticket`, once.
    pub fn put(&self, ticket: &TicketId, bytes: &[u8]) -> Result<Digest> {
        let digest = Digest::of(bytes);
        let dir = ticket_dir(&self.root, ticket)?;
        let path = dir.join(digest.digits());
        let mut used = self.lock();
        self.held(&mut used, ticket)?;
        if path.exists() {
            return Ok(digest);
        }
        private_dir(&dir)?;
        let temp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .with_context(|| format!("cannot create {}", temp.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("cannot write {}", temp.display()))?;
        std::fs::rename(&temp, &path).with_context(|| format!("cannot keep {}", path.display()))?;
        std::fs::File::open(&dir)?.sync_all()?;
        let added = bytes.len() as u64;
        used.total += added;
        *used.tickets.entry(ticket.clone()).or_default() += added;
        Ok(digest)
    }

    pub fn get(&self, ticket: &TicketId, digest: &Digest) -> Result<Option<Vec<u8>>> {
        read_blob(&self.root, ticket, digest)
    }

    fn lock(&self) -> MutexGuard<'_, Used> {
        self.used.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn held(&self, used: &mut Used, ticket: &TicketId) -> Result<u64> {
        if let Some(held) = used.tickets.get(ticket) {
            return Ok(*held);
        }
        let dir = ticket_dir(&self.root, ticket)?;
        let held = if dir.is_dir() { size(&dir)? } else { 0 };
        used.tickets.insert(ticket.clone(), held);
        Ok(held)
    }
}

fn ticket_dir(root: &Path, ticket: &TicketId) -> Result<PathBuf> {
    Ok(root.join(dir_name(ticket.as_str())?))
}

fn read_blob(root: &Path, ticket: &TicketId, digest: &Digest) -> Result<Option<Vec<u8>>> {
    let path = ticket_dir(root, ticket)?.join(digest.digits());
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn size(dir: &Path) -> Result<u64> {
    let mut bytes = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !temporary(&entry) {
            bytes += entry.metadata()?.len();
        }
    }
    Ok(bytes)
}

fn remove_killed_writes(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if temporary(&entry) {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn temporary(entry: &std::fs::DirEntry) -> bool {
    entry.file_name().to_string_lossy().ends_with(".tmp")
}

#[cfg(test)]
mod tests {
    use super::{BlobStore, TrailStoreConfig};
    use solos_agent_core::model::TicketId;
    use solos_agent_core::trail::Digest;
    use std::path::PathBuf;

    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "blob-store-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn limits(max_bytes: u64, ticket_max_bytes: u64) -> TrailStoreConfig {
        TrailStoreConfig {
            path: None,
            max_bytes,
            ticket_max_bytes,
        }
    }

    #[test]
    fn a_blob_is_kept_once_by_its_digest_and_read_back() {
        let root = root("put");
        let store = BlobStore::open(&root, &limits(1_000, 100)).unwrap();
        let ticket = TicketId::new("tk_a");

        let digest = store.put(&ticket, b"the page").unwrap();
        assert_eq!(digest, Digest::of(b"the page"));
        assert_eq!(store.put(&ticket, b"the page").unwrap(), digest);
        assert_eq!(store.get(&ticket, &digest).unwrap().unwrap(), b"the page");
        assert_eq!(
            store.get(&TicketId::new("tk_b"), &digest).unwrap(),
            None,
            "a blob is its ticket's"
        );
        let files = std::fs::read_dir(root.join("tk_a")).unwrap().count();
        assert_eq!(files, 1);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn past_either_cap_no_step_starts_and_the_count_survives_a_reopen() {
        let root = root("caps");
        let (a, b) = (TicketId::new("tk_a"), TicketId::new("tk_b"));
        let store = BlobStore::open(&root, &limits(30, 10)).unwrap();
        assert_eq!(store.room(&a), Ok(()));
        store.put(&a, b"0123456789").unwrap();
        let full = store.room(&a).unwrap_err();
        assert!(full.contains("this ticket's browser steps"), "{full}");
        assert_eq!(store.room(&b), Ok(()), "another ticket has its own share");
        store.put(&b, b"0123456789abcdefghij").unwrap();
        let full = store.room(&TicketId::new("tk_c")).unwrap_err();
        assert!(
            full.contains("the trail store is full (30 of 30 bytes)"),
            "{full}"
        );

        std::fs::write(root.join("tk_a").join(".killed.tmp"), b"half").unwrap();
        let reopened = BlobStore::open(&root, &limits(1_000, 10)).unwrap();
        assert!(
            reopened.room(&a).is_err(),
            "the ticket's share counts what is on disk"
        );
        assert_eq!(reopened.room(&TicketId::new("tk_c")), Ok(()));
        assert!(
            !root.join("tk_a").join(".killed.tmp").exists(),
            "a write a kill cut short is removed"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_ticket_cap_above_the_store_is_refused() {
        let root = root("limits");
        assert!(BlobStore::open(&root, &limits(10, 11)).is_err());
        assert!(BlobStore::open(&root, &limits(10, 0)).is_err());
    }
}
