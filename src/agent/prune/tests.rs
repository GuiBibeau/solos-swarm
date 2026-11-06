use super::{Kind, Ledger, Options, Standing, apply, human, plan, render};
use anyhow::Result;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

const DAY: u64 = 86_400_000;
const NOW: u64 = 40 * DAY;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agent-prune-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn state(&self) -> PathBuf {
        self.0.join("state")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Fake {
    standing: BTreeMap<String, Standing>,
    /// After this many answers, every Ticket is open again.
    resume_after: Cell<Option<usize>>,
}

impl Fake {
    fn with(mut self, ticket: &str, standing: Standing) -> Self {
        self.standing.insert(ticket.to_owned(), standing);
        self
    }
}

impl Ledger for Fake {
    fn standing(&self, ticket: &str) -> Result<Standing> {
        if let Some(left) = self.resume_after.get() {
            if left == 0 {
                return Ok(Standing::Open);
            }
            self.resume_after.set(Some(left - 1));
        }
        Ok(self
            .standing
            .get(ticket)
            .copied()
            .unwrap_or(Standing::Unknown))
    }
}

fn write(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![7u8; bytes]).unwrap();
}

fn age(path: &Path, days: u64) {
    let when = SystemTime::now() - Duration::from_secs(days * 86_400);
    std::fs::File::open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

/// A Ticket's directory as a worker leaves it.
fn ticket(state: &Path, name: &str) -> PathBuf {
    let dir = state.join("tickets").join(name);
    write(&dir.join("work/src/main.rs"), 20_000);
    write(&dir.join("work/.git/objects/pack/pack-1.pack"), 60_000);
    write(&dir.join("host/repo.json"), 50);
    write(&dir.join("prepared"), 0);
    write(&dir.join("pi-settings.json"), 40);
    write(&dir.join("as_1.rpc.jsonl"), 30_000);
    write(&dir.join("as_1.stderr"), 5_000);
    write(&dir.join("swarm/as_2/ledger.jsonl"), 1_000);
    dir
}

fn old() -> Standing {
    Standing::Ended {
        at_ms: NOW - 20 * DAY,
    }
}

fn recent() -> Standing {
    Standing::Ended {
        at_ms: NOW - 3 * DAY,
    }
}

fn names(plan: &super::Plan, kind: Kind) -> Vec<String> {
    let mut found: Vec<String> = plan
        .items
        .iter()
        .filter(|item| item.kind == kind)
        .map(|item| {
            item.ticket
                .clone()
                .unwrap_or_else(|| item.paths[0].display().to_string())
        })
        .collect();
    found.sort();
    found
}

fn options(scratch: &Scratch) -> Options {
    Options::new(scratch.state(), 14)
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

#[test]
fn only_tickets_that_ended_before_the_cutoff_are_pruned() {
    let scratch = Scratch::new("selection");
    let state = scratch.state();
    for name in ["tk_old", "tk_recent", "tk_open", "tk_unknown"] {
        ticket(&state, name);
    }
    write(&state.join("agent.db"), 4_096);
    let ledger = Fake::default()
        .with("tk_old", old())
        .with("tk_recent", recent())
        .with("tk_open", Standing::Open);

    let found = plan(&options(&scratch), &ledger, NOW).unwrap();
    assert_eq!(names(&found, Kind::TicketWork), ["tk_old"]);
    assert_eq!(names(&found, Kind::TicketTranscript), ["tk_old"]);
    assert_eq!((found.open, found.recent), (1, 1));
    assert_eq!(found.unknown, ["tk_unknown"], "no Ticket row: left alone");
    assert!(names(&found, Kind::Trail).is_empty(), "never unless asked");

    // The exact cutoff: ended 14 days ago is kept, a minute past it is not.
    let edge = Fake::default().with(
        "tk_old",
        Standing::Ended {
            at_ms: NOW - 14 * DAY,
        },
    );
    assert!(
        plan(&options(&scratch), &edge, NOW)
            .unwrap()
            .items
            .is_empty()
    );
    let past = Fake::default().with(
        "tk_old",
        Standing::Ended {
            at_ms: NOW - 14 * DAY - 60_000,
        },
    );
    assert!(
        !plan(&options(&scratch), &past, NOW)
            .unwrap()
            .items
            .is_empty()
    );
}

#[test]
fn a_run_removes_the_clone_its_markers_and_the_transcripts_and_keeps_everything_else() {
    let scratch = Scratch::new("run");
    let state = scratch.state();
    let dir = ticket(&state, "tk_old");
    let open = ticket(&state, "tk_open");
    write(&state.join("agent.db"), 4_096);
    let ledger = Fake::default()
        .with("tk_old", old())
        .with("tk_open", Standing::Open);

    let report = apply(
        &options(&scratch),
        &ledger,
        plan(&options(&scratch), &ledger, NOW).unwrap(),
    );
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(!dir.join("work").exists());
    assert!(
        !dir.join("host/repo.json").exists(),
        "a resumed Ticket clones again"
    );
    assert!(!dir.join("prepared").exists(), "and prepares again");
    assert!(!dir.join("as_1.rpc.jsonl").exists() && !dir.join("as_1.stderr").exists());
    assert!(dir.join("pi-settings.json").exists());
    assert!(dir.join("host").is_dir());
    assert!(
        dir.join("swarm/as_2/ledger.jsonl").exists(),
        "a swarm's books stay"
    );
    assert!(state.join("agent.db").exists());
    assert!(
        open.join("work/src/main.rs").exists(),
        "an open Ticket is never touched"
    );
    assert!(open.join("as_1.rpc.jsonl").exists());
    assert_eq!(report.items.len(), 2);
    assert!(report.freed_bytes >= 115_000, "{}", report.freed_bytes);
    assert_eq!(
        report.freed_bytes,
        report.items.iter().map(|item| item.bytes).sum::<u64>()
    );

    let again = apply(
        &options(&scratch),
        &ledger,
        plan(&options(&scratch), &ledger, NOW).unwrap(),
    );
    assert!(
        again.items.is_empty() && again.freed_bytes == 0,
        "idempotent"
    );
}

#[test]
fn a_dry_run_says_what_a_run_would_do_and_removes_nothing() {
    let scratch = Scratch::new("dry");
    let state = scratch.state();
    let dir = ticket(&state, "tk_old");
    let ledger = Fake::default().with("tk_old", old());
    let mut dry = options(&scratch);
    dry.dry_run = true;

    let report = apply(&dry, &ledger, plan(&dry, &ledger, NOW).unwrap());
    assert!(dir.join("work/src/main.rs").exists() && dir.join("as_1.stderr").exists());
    assert!(dir.join("prepared").exists());
    let text = render(&report);
    assert!(
        text.starts_with("agent prune (dry run): keep 14 days"),
        "{text}"
    );
    assert!(text.contains("would remove tk_old work clone"), "{text}");
    assert!(text.contains("would remove tk_old transcripts"), "{text}");
    assert!(text.contains("would free "), "{text}");

    let real = apply(
        &options(&scratch),
        &ledger,
        plan(&options(&scratch), &ledger, NOW).unwrap(),
    );
    assert_eq!(
        real.freed_bytes, report.freed_bytes,
        "the dry run did not lie"
    );
    assert!(render(&real).contains("removed tk_old work clone"));
}

#[test]
fn the_trail_goes_only_with_its_flag_and_only_for_ended_tickets() {
    let scratch = Scratch::new("trail");
    let state = scratch.state();
    ticket(&state, "tk_old");
    ticket(&state, "tk_open");
    let blobs = state.join("trail");
    write(&blobs.join("tk_old").join("a".repeat(64)), 10_000);
    write(&blobs.join("tk_open").join("b".repeat(64)), 10_000);
    let ledger = Fake::default()
        .with("tk_old", old())
        .with("tk_open", Standing::Open);

    let mut options = options(&scratch);
    assert!(names(&plan(&options, &ledger, NOW).unwrap(), Kind::Trail).is_empty());
    options.include_trail = true;
    let report = apply(&options, &ledger, plan(&options, &ledger, NOW).unwrap());
    assert!(!blobs.join("tk_old").exists());
    assert!(blobs.join("tk_open").join("b".repeat(64)).exists());
    assert!(report.items.iter().any(|item| item.kind == Kind::Trail));

    // A trail the config moved is found there.
    let moved = scratch.0.join("elsewhere");
    write(&moved.join("tk_old").join("c".repeat(64)), 1_000);
    options.trail_root = Some(moved.clone());
    apply(&options, &ledger, plan(&options, &ledger, NOW).unwrap());
    assert!(!moved.join("tk_old").exists());
}

#[test]
fn a_ticket_resumed_after_the_plan_keeps_its_clone() {
    let scratch = Scratch::new("resumed");
    let state = scratch.state();
    let dir = ticket(&state, "tk_old");
    let ledger = Fake::default().with("tk_old", old());
    let found = plan(&options(&scratch), &ledger, NOW).unwrap();
    assert!(!found.items.is_empty());

    ledger.resume_after.set(Some(0));
    let report = apply(&options(&scratch), &ledger, found);
    assert_eq!(report.skipped_resumed.len(), 2);
    assert!(report.items.is_empty());
    assert!(dir.join("work/src/main.rs").exists() && dir.join("as_1.stderr").exists());
}

#[test]
fn mirrors_go_when_nothing_has_fetched_into_them_for_the_window() {
    let scratch = Scratch::new("mirrors");
    let forge = scratch.state().join("forge");
    let stale = forge.join("acme/old.git");
    let fresh = forge.join("acme/new.git");
    for mirror in [&stale, &fresh] {
        write(&mirror.join("objects/pack/p.pack"), 50_000);
        write(&mirror.join("FETCH_HEAD"), 10);
        write(&mirror.join("HEAD"), 10);
        write(&mirror.join("config"), 10);
    }
    let not_a_mirror = forge.join("acme/notes");
    write(&not_a_mirror.join("x"), 10);
    for path in [
        stale.join("objects/pack/p.pack"),
        stale.join("FETCH_HEAD"),
        stale.join("HEAD"),
        stale.join("config"),
        stale.clone(),
        not_a_mirror.join("x"),
        not_a_mirror.clone(),
    ] {
        age(&path, 30);
    }
    // Made long ago and fetched into just now (FETCH_HEAD is fresh): in use.
    age(&fresh.join("objects/pack/p.pack"), 30);
    age(&fresh.join("HEAD"), 30);
    age(&fresh.join("config"), 30);

    let ledger = Fake::default();
    let found = plan(&options(&scratch), &ledger, now_ms()).unwrap();
    let paths: Vec<_> = found
        .items
        .iter()
        .map(|item| item.paths[0].clone())
        .collect();
    assert_eq!(paths, std::slice::from_ref(&stale), "{found:?}");
    let report = apply(&options(&scratch), &ledger, found);
    assert!(!stale.exists() && fresh.exists() && not_a_mirror.exists());
    assert!(report.freed_bytes >= 50_000);
}

#[test]
fn the_routers_old_transcripts_go_but_never_its_newest() {
    let scratch = Scratch::new("router");
    let router = scratch.state().join("router");
    let files = [
        ("router-1-0.rpc.jsonl", 40),
        ("router-2-0.rpc.jsonl", 20),
        ("router-3-0.rpc.jsonl", 18),
        ("router-1-0.stderr", 40),
        ("router-3-0.stderr", 18),
        ("pi-settings.json", 40),
    ];
    for (name, days) in files {
        write(&router.join(name), 2_000);
        age(&router.join(name), days);
    }
    let ledger = Fake::default();
    let report = apply(
        &options(&scratch),
        &ledger,
        plan(&options(&scratch), &ledger, now_ms()).unwrap(),
    );
    assert!(report.errors.is_empty());
    let mut left: Vec<String> = std::fs::read_dir(&router)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            "pi-settings.json",
            "router-3-0.rpc.jsonl",
            "router-3-0.stderr"
        ],
        "the newest of each kind and the settings stay, the rest is past the window"
    );
}

#[test]
fn a_link_in_a_clone_is_removed_not_followed() {
    let scratch = Scratch::new("links");
    let state = scratch.state();
    let dir = ticket(&state, "tk_old");
    let outside = scratch.0.join("outside");
    write(&outside.join("precious"), 100);
    std::os::unix::fs::symlink(&outside, dir.join("work/escape")).unwrap();
    let ledger = Fake::default().with("tk_old", old());
    let report = apply(
        &options(&scratch),
        &ledger,
        plan(&options(&scratch), &ledger, NOW).unwrap(),
    );
    assert!(report.errors.is_empty());
    assert!(!dir.join("work").exists());
    assert!(outside.join("precious").exists());
}

#[test]
fn a_clone_with_read_only_directories_is_removed_all_the_same() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("readonly");
    let state = scratch.state();
    let dir = ticket(&state, "tk_old");
    // As Go's module cache is left: read-only directories with files in them.
    let cache = dir.join("work/pkg/mod/github.com/x");
    write(&cache.join("lib.go"), 100);
    for locked in [&cache, &cache.parent().unwrap().to_owned()] {
        std::fs::set_permissions(locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    }
    let ledger = Fake::default().with("tk_old", old());
    let report = apply(
        &options(&scratch),
        &ledger,
        plan(&options(&scratch), &ledger, NOW).unwrap(),
    );
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(!dir.join("work").exists());
}

#[test]
fn a_ticket_that_was_resumed_and_ended_again_since_the_plan_keeps_its_clone() {
    let scratch = Scratch::new("reended");
    let state = scratch.state();
    let dir = ticket(&state, "tk_old");
    let before = Fake::default().with("tk_old", old());
    let found = plan(&options(&scratch), &before, NOW).unwrap();
    // It ended again, just now: its clone is the second run's.
    let after = Fake::default().with("tk_old", recent());
    let report = apply(&options(&scratch), &after, found);
    assert_eq!(report.skipped_resumed.len(), 2);
    assert!(dir.join("work/src/main.rs").exists());
}

#[test]
fn a_window_under_a_day_is_refused_and_sizes_read_like_df() {
    let scratch = Scratch::new("window");
    let mut zero = options(&scratch);
    zero.keep_days = 0;
    assert!(
        plan(&zero, &Fake::default(), NOW)
            .unwrap_err()
            .to_string()
            .contains("at least 1")
    );
    assert_eq!(human(512), "512 B");
    assert_eq!(human(1_536), "1.5 KB");
    assert_eq!(human(3 * (1 << 30)), "3.0 GB");
}

#[test]
fn run_refuses_a_state_directory_with_no_store() {
    let scratch = Scratch::new("nostore");
    std::fs::create_dir_all(scratch.state().join("tickets/tk_1/work")).unwrap();
    let err = super::run(&options(&scratch), NOW).unwrap_err().to_string();
    assert!(err.contains("agent.db"), "{err}");
    assert!(scratch.state().join("tickets/tk_1/work").exists());
}
