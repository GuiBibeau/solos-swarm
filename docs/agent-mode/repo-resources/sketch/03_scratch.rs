//! Lives at: src/fs_usage.rs (new), src/agent/scratch.rs (new).
//!
//! Sketch only: not compiled. It names the walk and the state machine,
//! not working code.
//!
//! One walk for the host, safe against a worker that runs while it walks. It
//! replaces `deps::dir_bytes` (src/agent/forge/deps.rs:851) and prune's `du`
//! (src/agent/prune.rs:514). Then the scratch reservations, the accountant
//! task and the floor guard, all inside `agent serve`.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;

// src/fs_usage.rs

pub struct Limits {
    pub max_entries: u64,
    pub max_depth: usize,
    pub max_time: Duration,
}

pub struct Usage {
    /// `st_blocks * 512`, a multiply-linked file once by (dev, ino).
    pub bytes: u64,
    pub entries: u64,
    /// Set when a bound stopped the walk; the caller treats it as over.
    pub stopped: Option<Stopped>,
}

pub enum Stopped {
    Entries,
    Depth,
    Time,
}

/// Walks `root` by descriptor: the root opened `O_DIRECTORY | O_NOFOLLOW`,
/// each entry `fstatat(AT_SYMLINK_NOFOLLOW)`, each subdirectory
/// `openat(dirfd, name, O_DIRECTORY | O_NOFOLLOW)`. A directory swapped for a
/// link between the stat and the open fails to open and is skipped. Opens
/// nothing but directories, so a FIFO cannot block it. Never leaves the
/// root's `st_dev`. Remembers each directory's (dev, ino), so
/// nothing is walked twice.
pub fn measure(root: &Path, limits: &Limits) -> Result<Usage> {
    let _ = (root, limits);
    todo!("libc::open, fdopendir, fstatat, openat")
}

// src/agent/scratch.rs

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Reserved,
    Over,
    Released,
    Freed,
}

pub struct Reservation {
    pub ticket: String,
    pub repo: String,
    pub quota_bytes: u64,
    pub max_files: u64,
    pub used_bytes: u64,
    pub peak_bytes: u64,
    pub files: u64,
    pub state: State,
}

pub enum Admission {
    Granted { dir: PathBuf, quota_bytes: u64 },
    /// No `/scratch` this Assignment; the worker's prompt gives the numbers.
    Refused { free: u64, floor: u64, reserved: u64, wanted: u64 },
}

/// `headroom = free - floor - margin - sum(open quota - used)`, under one
/// lock in `agent serve`. `free` is `statvfs`'s `f_bavail * f_frsize`
/// (src/agent/health.rs:904).
pub fn admit(
    free: u64,
    floor: u64,
    margin: u64,
    open: &[Reservation],
    wanted: u64,
) -> bool {
    let reserved: u64 = open
        .iter()
        .filter(|r| matches!(r.state, State::Reserved | State::Over))
        .map(|r| r.quota_bytes.saturating_sub(r.used_bytes))
        .sum();
    free.saturating_sub(floor)
        .saturating_sub(margin)
        .saturating_sub(reserved)
        >= wanted
}

/// The accountant: walks a Ticket's `work/` and `scratch/`, every second
/// while one of its sandboxes runs, every minute otherwise. Over the quota,
/// the file cap, or a stopped walk: kill the Ticket's sandboxes
/// (`code::kill_sandbox`, src/code.rs:1353), mark it `Over`, and run its
/// commands with `--fsize=0` until it is back under.
pub struct Accountant;

impl Accountant {
    pub async fn tick(&self) -> Result<()> {
        todo!()
    }
}

/// The floor guard: `statvfs` every second. Below `floor + margin`, write
/// `PAUSE` with its own first line, kill every sandbox and dependency build,
/// refuse new ones and new gate connections. Lift its own `PAUSE` above
/// `floor + margin + margin / 10`.
pub struct FloorGuard {
    pub floor: u64,
    pub margin: u64,
}

pub const PAUSE_MARK: &str = "paused by solos-agent disk guard";

impl FloorGuard {
    pub async fn tick(&self, free: u64) -> Result<()> {
        let _ = free;
        todo!()
    }
}
