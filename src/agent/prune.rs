//! `solos-swarm agent prune`: take back the disk an agent's finished work
//! leaves behind, and nothing else.
//!
//! It removes, for Tickets that ended more than `keep_days` days ago, the
//! worker's clone (`tickets/<id>/work`, with the markers that say the clone is
//! done, so a Ticket a person later resumes clones again instead of finding an
//! empty directory) and the worker's transcripts; the forge mirrors nobody has
//! fetched into for that long (the next Ticket on the repository makes one
//! again); and the Router's old transcripts. It never touches an open Ticket,
//! `agent.db`, a directory it cannot match to a Ticket, or, unless asked
//! (`include_trail`), the trail's blobs, which `agent trail` needs to check a
//! browser's steps.
//!
//! [`plan`] only looks. [`apply`] removes what a plan names, after asking the
//! Store again whether each Ticket is still ended, so a Ticket resumed since
//! the plan keeps its clone. Both are testable without a Store: they ask a
//! [`Ledger`].

use super::forge::{FORGE_DIR, REPO_JSON};
use super::pi_executor::{HOST_DIR, PREPARED, WORK_DIR, dir_name};
use super::{ROUTER_DIR, STORE_FILE, TICKETS_DIR, TRAIL_DIR};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use solos_agent_core::model::{Ms, TicketId};
use solos_agent_store::SqliteStore;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::Metadata;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const DAY_MS: Ms = 86_400_000;
/// What a worker session writes beside its Ticket's directory entries.
const TRANSCRIPT_SUFFIXES: [&str; 2] = [".rpc.jsonl", ".stderr"];
/// Files inside a bare mirror that git touches whenever the mirror is used.
const MIRROR_USE_MARKS: [&str; 4] = ["FETCH_HEAD", "HEAD", "config", "packed-refs"];

/// What the Store says of a Ticket directory's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    Open,
    Ended {
        at_ms: Ms,
    },
    /// No such Ticket: a directory something else made, or a Store that was
    /// restored from before it. It is left alone.
    Unknown,
}

/// Where the Tickets' standing comes from.
pub trait Ledger {
    fn standing(&self, ticket: &str) -> Result<Standing>;
}

impl Ledger for SqliteStore {
    fn standing(&self, ticket: &str) -> Result<Standing> {
        Ok(match self.read_ticket(&TicketId::new(ticket))? {
            None => Standing::Unknown,
            Some(found) => match found.ended {
                None => Standing::Open,
                Some(ended) => Standing::Ended { at_ms: ended.at_ms },
            },
        })
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub state_dir: PathBuf,
    /// Tickets that ended, and mirrors that were used, longer ago than this
    /// are pruned. At least 1.
    pub keep_days: u64,
    pub dry_run: bool,
    /// Also remove the trail blobs of those Tickets. `agent trail` then
    /// reports their steps' blobs as missing.
    pub include_trail: bool,
    /// Where the trail's blobs are, when agent.yaml's `trail.path` moved them.
    pub trail_root: Option<PathBuf>,
}

impl Options {
    pub fn new(state_dir: impl Into<PathBuf>, keep_days: u64) -> Self {
        Self {
            state_dir: state_dir.into(),
            keep_days,
            dry_run: false,
            include_trail: false,
            trail_root: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A Ticket's `work/` clone and the markers that say it is done.
    TicketWork,
    /// A Ticket's worker transcripts and stderr.
    TicketTranscript,
    /// A Ticket's trail blobs, only with `include_trail`.
    Trail,
    /// The Router's transcripts past `keep_days`, but never its newest.
    RouterTranscript,
    /// A forge mirror nobody has used for `keep_days`.
    ForgeMirror,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::TicketWork => "work clone",
            Self::TicketTranscript => "transcripts",
            Self::Trail => "trail blobs",
            Self::RouterTranscript => "router transcripts",
            Self::ForgeMirror => "forge mirror",
        }
    }
}

/// One thing to remove: some paths, what they hold and why.
#[derive(Clone, Debug, Serialize)]
pub struct Item {
    pub kind: Kind,
    /// The Ticket it belongs to, for the kinds that have one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    /// Everything removed together. Directories go whole; the first is the
    /// one the report names.
    pub paths: Vec<PathBuf>,
    /// Disk it holds, as allocated (what removing it frees).
    pub bytes: u64,
    pub why: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Plan {
    pub items: Vec<Item>,
    /// Tickets that ended before this (Unix ms) are the ones pruned; `apply`
    /// asks again that each still did.
    pub cutoff_ms: Ms,
    /// Ticket directories left alone, by reason.
    pub open: usize,
    pub recent: usize,
    /// Ticket directories the Store has no Ticket for.
    pub unknown: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub dry_run: bool,
    pub keep_days: u64,
    pub state_dir: PathBuf,
    /// What was removed, or with `dry_run` what would be.
    pub items: Vec<Item>,
    /// Bytes those items hold: freed, or with `dry_run` to be freed.
    pub freed_bytes: u64,
    pub kept_open: usize,
    pub kept_recent: usize,
    pub unknown: Vec<String>,
    /// Items skipped because their Ticket was resumed after the plan.
    pub skipped_resumed: Vec<String>,
    /// What could not be removed. The rest was.
    pub errors: Vec<String>,
}

/// The whole job: open the Store read-only, plan, and unless `dry_run` apply.
pub fn run(options: &Options, now_ms: Ms) -> Result<Report> {
    let path = options.state_dir.join(STORE_FILE);
    ensure!(
        path.is_file(),
        "no {} to read: refusing to prune a state directory that has no Store, where no Ticket \
         could be told from another",
        path.display()
    );
    let store = SqliteStore::open_read_only(&path)
        .with_context(|| format!("cannot open {} to read", path.display()))?;
    let plan = plan(options, &store, now_ms)?;
    Ok(apply(options, &store, plan))
}

/// What `options` would remove at `now_ms`. Reads the disk and `ledger` and
/// changes nothing.
pub fn plan(options: &Options, ledger: &dyn Ledger, now_ms: Ms) -> Result<Plan> {
    ensure!(
        options.keep_days >= 1,
        "--keep-days must be at least 1: a Ticket that ended today may still be resumed"
    );
    let cutoff = now_ms.saturating_sub(options.keep_days.saturating_mul(DAY_MS));
    let state = &options.state_dir;
    let mut plan = Plan {
        cutoff_ms: cutoff,
        ..Plan::default()
    };
    let trail_root = options
        .trail_root
        .clone()
        .unwrap_or_else(|| state.join(TRAIL_DIR));
    for (name, dir) in subdirectories(&state.join(TICKETS_DIR))? {
        if dir_name(&name).is_err() {
            continue;
        }
        match ledger.standing(&name)? {
            Standing::Unknown => plan.unknown.push(name),
            Standing::Open => plan.open += 1,
            Standing::Ended { at_ms } if at_ms >= cutoff => plan.recent += 1,
            Standing::Ended { at_ms } => {
                let why = format!("ended {} days ago", now_ms.saturating_sub(at_ms) / DAY_MS);
                ticket_items(&mut plan, &name, &dir, &why);
                if options.include_trail {
                    trail_item(&mut plan, &name, &trail_root.join(&name), &why);
                }
            }
        }
    }
    router_item(
        &mut plan,
        &state.join(ROUTER_DIR),
        cutoff,
        options.keep_days,
    );
    mirror_items(&mut plan, &state.join(FORGE_DIR), cutoff, options.keep_days)?;
    Ok(plan)
}

/// Remove what `plan` names, unless `options.dry_run`.
pub fn apply(options: &Options, ledger: &dyn Ledger, plan: Plan) -> Report {
    let mut report = Report {
        dry_run: options.dry_run,
        keep_days: options.keep_days,
        state_dir: options.state_dir.clone(),
        items: Vec::new(),
        freed_bytes: 0,
        kept_open: plan.open,
        kept_recent: plan.recent,
        unknown: plan.unknown,
        skipped_resumed: Vec::new(),
        errors: Vec::new(),
    };
    for item in plan.items {
        if options.dry_run {
            report.freed_bytes += item.bytes;
            report.items.push(item);
            continue;
        }
        if let Some(ticket) = &item.ticket {
            match ledger.standing(ticket) {
                Ok(Standing::Ended { at_ms }) if at_ms < plan.cutoff_ms => {}
                Ok(_) => {
                    report.skipped_resumed.push(ticket.clone());
                    continue;
                }
                Err(err) => {
                    report
                        .errors
                        .push(format!("{ticket}: cannot read its standing, kept: {err:#}"));
                    continue;
                }
            }
        }
        let mut failed = false;
        for path in &item.paths {
            if let Err(err) = remove(path) {
                failed = true;
                report
                    .errors
                    .push(format!("cannot remove {}: {err:#}", path.display()));
            }
        }
        if !failed {
            report.freed_bytes += item.bytes;
            report.items.push(item);
        }
    }
    report
}

/// A Ticket's clone and markers, and its transcripts.
fn ticket_items(plan: &mut Plan, name: &str, dir: &Path, why: &str) {
    let work = dir.join(WORK_DIR);
    let markers: Vec<PathBuf> = [dir.join(HOST_DIR).join(REPO_JSON), dir.join(PREPARED)]
        .into_iter()
        .filter(|path| exists(path))
        .collect();
    if exists(&work) || !markers.is_empty() {
        let bytes = [&work].into_iter().chain(&markers).map(|p| du(p)).sum();
        let paths = [work]
            .into_iter()
            .filter(|path| exists(path))
            .chain(markers)
            .collect();
        plan.items.push(Item {
            kind: Kind::TicketWork,
            ticket: Some(name.to_owned()),
            paths,
            bytes,
            why: why.to_owned(),
        });
    }
    let transcripts: Vec<PathBuf> = files(dir)
        .into_iter()
        .filter(|(file, _)| TRANSCRIPT_SUFFIXES.iter().any(|end| file.ends_with(end)))
        .map(|(_, path)| path)
        .collect();
    if !transcripts.is_empty() {
        plan.items.push(Item {
            kind: Kind::TicketTranscript,
            ticket: Some(name.to_owned()),
            bytes: transcripts.iter().map(|p| du(p)).sum(),
            paths: transcripts,
            why: why.to_owned(),
        });
    }
}

fn trail_item(plan: &mut Plan, name: &str, dir: &Path, why: &str) {
    if is_real_dir(dir) {
        plan.items.push(Item {
            kind: Kind::Trail,
            ticket: Some(name.to_owned()),
            bytes: du(dir),
            paths: vec![dir.to_owned()],
            why: why.to_owned(),
        });
    }
}

/// The Router's transcripts last modified before `cutoff`, except the newest
/// file of each kind, which a live session may still be writing.
fn router_item(plan: &mut Plan, dir: &Path, cutoff: Ms, keep_days: u64) {
    let mut old = Vec::new();
    for suffix in TRANSCRIPT_SUFFIXES {
        let mut mine: Vec<(Ms, PathBuf)> = files(dir)
            .into_iter()
            .filter(|(file, _)| file.ends_with(suffix))
            .filter_map(|(_, path)| Some((modified(&path)?, path)))
            .collect();
        mine.sort();
        mine.pop();
        old.extend(mine.into_iter().filter(|(at, _)| *at < cutoff));
    }
    if old.is_empty() {
        return;
    }
    let paths: Vec<PathBuf> = old.into_iter().map(|(_, path)| path).collect();
    plan.items.push(Item {
        kind: Kind::RouterTranscript,
        ticket: None,
        bytes: paths.iter().map(|p| du(p)).sum(),
        paths,
        why: format!("not written for {keep_days} days"),
    });
}

/// `forge/<owner>/<name>.git`, last used before `cutoff`.
fn mirror_items(plan: &mut Plan, forge: &Path, cutoff: Ms, keep_days: u64) -> Result<()> {
    for (_, owner) in subdirectories(forge)? {
        for (name, mirror) in subdirectories(&owner)? {
            if !name.ends_with(".git") {
                continue;
            }
            let used = MIRROR_USE_MARKS
                .iter()
                .filter_map(|mark| modified(&mirror.join(mark)))
                .chain(modified(&mirror))
                .max()
                .unwrap_or(0);
            if used < cutoff {
                plan.items.push(Item {
                    kind: Kind::ForgeMirror,
                    ticket: None,
                    bytes: du(&mirror),
                    paths: vec![mirror],
                    why: format!("not fetched into for {keep_days} days"),
                });
            }
        }
    }
    Ok(())
}

/// Remove a file, a link (never what it points at) or a directory tree.
fn remove(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if meta.is_dir() {
        // A worker's build can leave read-only directories (Go's module cache is one), which
        // `remove_dir_all` cannot empty until they are writable again.
        if let Err(err) = std::fs::remove_dir_all(path) {
            if err.kind() != std::io::ErrorKind::PermissionDenied {
                return Err(err.into());
            }
            make_writable(path);
            std::fs::remove_dir_all(path)?;
        }
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Give the owner write and search access to every directory under `dir` (links are not followed).
fn make_writable(dir: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mut permissions = meta.permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    let _ = std::fs::set_permissions(dir, permissions);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            make_writable(&entry.path());
        }
    }
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// The directories directly under `dir` (not links), by name. A missing
/// `dir` has none.
fn subdirectories(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            found.push((name, entry.path()));
        }
    }
    found.sort();
    Ok(found)
}

/// The regular files directly under `dir`, by name.
fn files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
        .collect();
    found.sort();
    found
}

fn modified(path: &Path) -> Option<Ms> {
    let at = std::fs::symlink_metadata(path).ok()?.modified().ok()?;
    u64::try_from(at.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

/// What `path` holds on disk: allocated blocks, links not followed, a file
/// with several links counted once.
fn du(path: &Path) -> u64 {
    fn walk(path: &Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return 0;
        };
        let mut bytes = allocated(&meta, seen);
        if meta.is_dir()
            && let Ok(entries) = std::fs::read_dir(path)
        {
            for entry in entries.flatten() {
                bytes += walk(&entry.path(), seen);
            }
        }
        bytes
    }
    fn allocated(meta: &Metadata, seen: &mut HashSet<(u64, u64)>) -> u64 {
        if meta.nlink() > 1 && !meta.is_dir() && !seen.insert((meta.dev(), meta.ino())) {
            return 0;
        }
        meta.blocks() * 512
    }
    walk(path, &mut HashSet::new())
}

/// `1.2 GB`, `340 KB`: powers of 1024, like `df -h`.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The report as text, one line per item and a total.
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    let (verb, total) = if report.dry_run {
        ("would remove", "would free")
    } else {
        ("removed", "freed")
    };
    let _ = writeln!(
        out,
        "agent prune{}: keep {} days, state {}",
        if report.dry_run { " (dry run)" } else { "" },
        report.keep_days,
        report.state_dir.display()
    );
    for item in &report.items {
        let what = item.ticket.as_ref().map_or_else(
            || item.paths[0].display().to_string(),
            |ticket| format!("{ticket} {}", item.kind.label()),
        );
        let more = match item.paths.len() {
            0 | 1 => String::new(),
            n => format!(" (+{} more)", n - 1),
        };
        let _ = writeln!(
            out,
            "  {verb} {what}{more}: {} ({})",
            human(item.bytes),
            item.why
        );
    }
    for ticket in &report.skipped_resumed {
        let _ = writeln!(out, "  kept {ticket}: it was resumed while this ran");
    }
    for error in &report.errors {
        let _ = writeln!(out, "  ERROR {error}");
    }
    let _ = writeln!(
        out,
        "{total} {} across {} item(s); kept {} open ticket(s) and {} ended within {} days{}",
        human(report.freed_bytes),
        report.items.len(),
        report.kept_open,
        report.kept_recent,
        report.keep_days,
        if report.unknown.is_empty() {
            String::new()
        } else {
            format!(
                "; left alone {} ticket directories with no ticket in the Store ({})",
                report.unknown.len(),
                report.unknown.join(", ")
            )
        }
    );
    out
}

/// `agent prune`'s exit: an error in the report is a failure.
pub fn check(report: &Report) -> Result<()> {
    if report.errors.is_empty() {
        return Ok(());
    }
    bail!("{} item(s) could not be removed", report.errors.len())
}

#[cfg(test)]
mod tests;
