//! Lives at: src/agent/ports.rs (the `Forge` port and its domain types) and
//! src/agent/forge/{mod,github,local,watch}.rs (new).
//!
//! How a change becomes a pull request while no model and no sandbox ever
//! holds a token. One rule carries it:
//!
//! > The host never runs git in a repository a sandbox can write, and never
//! > writes into a directory a sandbox can write.
//!
//! A worker's `/work` is a git repository whose config, hooks and refs the
//! worker controls. Host git in it would run `core.fsmonitor`, a filter or a
//! hook the worker planted, with the host's privileges. So every git command
//! that touches a worker's repository runs in a `run_code` sandbox, and what
//! crosses to the host is data: a `git bundle` file of the commits to
//! publish. The host fetches that bundle into its own mirror (a host-owned
//! bare repository with `transfer.fsckObjects`), checks it there, and pushes
//! from there with the token.
//!
//! The flow of one `pr_publish`:
//!
//! ```text
//! worker ── pr_publish{branch,title,body} ──▶ broker
//!   broker ─ run_code sandbox (fixed host script, /work rw):
//!              git rev-parse <commit>; git bundle create /host-out/n.bundle <base>..<commit>
//!   broker ─ Fact::PublishAsk ─▶ Engine: PublishAsked on the chain     (write-ahead)
//!   broker ─ Forge::publish(bundle, …):
//!              mirror: git fetch <bundle> <commit>:refs/solos/<ticket>/<n>
//!                      merge-base --is-ancestor <base> <commit>; diff --name-only; size
//!                      refuse protected paths (.github/workflows/** by default)
//!              push:   git push --force-with-lease=<branch>:<last head we pushed, or none>
//!                      <forge> refs/solos/…:refs/heads/<branch>     (token in the env, never argv)
//!              REST:   find the PR by head branch, else create it (ready, never draft);
//!                      edit title, body, base when they changed; the body ends with a
//!                      hidden marker naming the Ticket
//!   broker ─ Fact::PrSeen ─▶ Engine: Change upserted, Ticket subscribed to the PR Thread
//!   ◀── {pr, url, head, base, created, ci: pending}
//! watcher ─ polls the head's CI until terminal ─ Fact::PrSeen ─▶ Engine: Note "CI on #61 …"
//! ```
//!
//! Every step is idempotent. A repeat with the same commit pushes nothing
//! new and finds the same PR. A crash after the push and before `PrSeen`
//! leaves a pushed branch that the next publish, or the watcher's sweep of
//! `PublishAsked` without `PrSeen`, finds by its head branch.

use crate::agent::ports::BoxFuture;
use anyhow::Result;
use solos_agent_core::change::{BranchName, Change, CommitSha, PrRef, RepoName};
use solos_agent_core::model::{MessageId, TicketId};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// The port (src/agent/ports.rs).
// ---------------------------------------------------------------------------

/// A code forge the operator configured. Its adapter holds the token, the
/// mirrors and the forge's API; callers hand it data and get domain types
/// back, never a forge's wire objects.
pub trait Forge: Send + Sync {
    /// Make `work` a clone of `repo` at its base's tip, with full history,
    /// the agent's commit identity in its own config, and `origin` naming
    /// the forge's URL (unreachable from a sandbox, but readable). Fetches
    /// the mirror first. Writes `host/repo.json` (repo, base, base commit,
    /// branch namespace). Runs before any sandbox has touched `work`, and
    /// leaves a marker, so a recovery launch reuses the tree as the worker
    /// left it. A missing marker means no sandbox ran, so a half-made `work`
    /// is removed and made again.
    fn prepare<'a>(&'a self, repo: &'a RepoName, ticket: &'a TicketId, dirs: &'a TicketDirs)
    -> BoxFuture<'a, Result<Prepared>>;

    /// Push the bundle's commit to `branch` and open or update its pull
    /// request. Refusals are the worker's to fix and come back as tool
    /// errors; transport failures are errors.
    fn publish<'a>(&'a self, request: PublishRequest<'a>)
    -> BoxFuture<'a, Result<std::result::Result<Seen, Refusal>>>;

    /// The pull request now, and the base branch's tip written as a bundle
    /// to `base_bundle` for a sandbox to fetch from (a rebase onto a newer
    /// base, Babysit's stale-base fix).
    fn view<'a>(&'a self, pr: &'a PrRef, base_bundle: Option<&'a Path>) -> BoxFuture<'a, Result<View>>;

    /// A reply in a review thread, or a comment. `marker` makes a repeat
    /// post nothing, as `GithubChannel` does with its hidden line.
    fn reply<'a>(&'a self, pr: &'a PrRef, thread: Option<&'a str>, body: &'a str, marker: &'a MessageId)
    -> BoxFuture<'a, Result<()>>;

    /// Merge `pr` only if its head is still `head` (GitHub's merge call
    /// takes the sha and refuses a moved head), then read it back: a merge
    /// a queue only queued is not reported merged.
    fn merge<'a>(&'a self, pr: &'a PrRef, head: &'a CommitSha, method: MergeMethod)
    -> BoxFuture<'a, Result<std::result::Result<Seen, Refusal>>>;
}

/// A Ticket's directories on the host.
pub struct TicketDirs {
    /// `<ticket>/work`: the repository, writable by sandboxes.
    pub work: PathBuf,
    /// `<ticket>/host`: written only by the host, read-only to sandboxes.
    pub host: PathBuf,
}

pub struct Prepared {
    pub base: String,
    pub base_commit: CommitSha,
}

pub struct PublishRequest<'a> {
    pub ticket: &'a TicketId,
    pub repo: &'a RepoName,
    pub branch: &'a BranchName,
    /// The bundle the sandbox wrote, holding `base_commit..commit`. Data:
    /// the adapter fetches it into its own mirror and never runs git in the
    /// worker's tree.
    pub bundle: &'a Path,
    pub commit: &'a CommitSha,
    /// The commit `prepare` started from, or a newer base tip the worker
    /// rebased onto (it must be an ancestor of `commit` and of the base).
    pub base_commit: &'a CommitSha,
    /// The repository's base, or another branch of this Ticket (a stack).
    pub base: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    /// The head this host last pushed to `branch`, for the lease: a push by
    /// anyone else since is refused, never overwritten.
    pub lease: Option<&'a CommitSha>,
}

/// A pull request as the forge answered, parsed into the core's terms.
pub struct Seen {
    pub pr: PrRef,
    pub change: Change,
    pub created: bool,
}

pub struct View {
    pub seen: Seen,
    pub mergeable: Mergeable,
    pub failed_checks: Vec<FailedCheck>,
    /// Unresolved review threads, their text clipped and quoted: data.
    pub threads: Vec<Thread>,
    pub base_tip: CommitSha,
    pub behind: u32,
}

pub enum Mergeable {
    Clean,
    Conflicted,
    Blocked,
    Unknown,
}

pub struct FailedCheck {
    pub name: String,
    pub url: String,
    pub summary: String,
}

pub struct Thread {
    pub id: String,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub author: String,
    pub body: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}

/// Why the forge would not do it, in words the worker can act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Someone else pushed to the branch since our last push.
    Leased { found: CommitSha },
    NotDescendant { base: CommitSha },
    Protected { path: String },
    TooLarge { bytes: u64, limit: u64 },
    /// The head moved after the gate; nothing was merged.
    HeadMoved { now: CommitSha },
    Forge(String),
}

// ---------------------------------------------------------------------------
// The GitHub adapter (src/agent/forge/github.rs).
// ---------------------------------------------------------------------------

/// GitHub over its REST API, with host-side git for the mirror and the
/// push. Its token is its own: a fine-grained token with Contents and Pull
/// requests read and write, and Checks and Commit statuses read, on the
/// configured repositories only, never Workflows. It is not the `github`
/// Source's token, which stays read-mostly, and it is not a vault item, so
/// no worker can name it to `fetch_url`.
pub struct GithubForge {
    api: std::sync::Arc<crate::agent::github::GithubApi>,
    /// `<state>/forge/<owner>/<repo>.git`, bare, host-owned.
    mirrors: PathBuf,
    repos: std::collections::BTreeMap<RepoName, RepoTerms>,
    identity: (String, String),
}

/// One repository's terms, from `forge.repos.<repo>` in agent.yaml.
pub struct RepoTerms {
    pub base: String,
    /// Globs a published commit may not change. Default
    /// `[".github/workflows/**"]`: CI on a branch of the same repository
    /// runs with the repository's secrets, so a workflow a model wrote
    /// would hand them out.
    pub protected: Vec<String>,
    pub max_change_bytes: u64,
}

impl GithubForge {
    /// Run host git with the token in the environment, as
    /// `GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.<base>.extraheader
    /// GIT_CONFIG_VALUE_0=<Authorization: basic …>`: never argv, which any
    /// local user can read in /proc. Only in the mirror, which no sandbox
    /// mounts.
    fn git(&self, mirror: &Path, args: &[&str], with_token: bool) -> BoxFuture<'_, Result<std::process::Output>> {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// The offline adapter (src/agent/forge/local.rs).
// ---------------------------------------------------------------------------

/// A forge on the local disk, for tests and dry runs: a bare repository as
/// the remote, pull requests in a JSON file, and CI results a test sets.
/// The same mirror, bundle, lease and protected-path code as `GithubForge`
/// runs against it; only the REST half is replaced. So every PR's behavior
/// test runs with no network.
pub struct LocalForge {
    pub remote: PathBuf,
    pub prs: PathBuf,
}

impl LocalForge {
    /// Set the CI result a later `view` reports for `head`.
    pub fn set_ci(&self, head: &CommitSha, ci: solos_agent_core::change::Ci) -> Result<()> {
        todo!()
    }
    /// Push to `branch` as someone else would, to test the lease.
    pub fn push_as_other(&self, branch: &str, commit: &CommitSha) -> Result<()> {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// The CI watcher (src/agent/forge/watch.rs).
// ---------------------------------------------------------------------------

/// pstack's Babysit wants a watcher that wakes the lead on CI ("Watcher
/// output drives wakeups. Never add a second sleep loop"). Here it is host
/// code with no model: every `poll` it reads the Store's open Tickets, and
/// for each open Change whose CI at the head is `Pending`, it calls
/// `Forge::view` and sends `Fact::PrSeen`. The Engine records it and, when
/// CI turns terminal at the head, wakes the worker with a Note. The
/// watcher keeps no state of its own: it derives what to watch from the
/// Store on each pass, so a restart loses nothing. It also sweeps
/// `PublishAsked` events with no later `PrSeen` for the same head, the
/// trace of a crash between push and record.
pub struct Watcher {
    pub poll: std::time::Duration,
}

impl Watcher {
    pub fn spawn(self, forge: std::sync::Arc<dyn Forge>, store: PathBuf, facts: crate::agent::ports::FactSink) -> tokio::task::JoinHandle<()> {
        todo!()
    }
}

// Tests (tests/agent_forge.rs, LocalForge, scripted harness):
// - prepare: /work is a clone at the base tip with history; a scripted
//   worker's `git commit` in run_code succeeds with the configured identity
//   and HOME=/work; a recovery launch keeps the worker's commits; a missing
//   marker remakes /work; the host makes no git call in /work after the
//   marker (a wrapper `git` on the host's PATH logs every cwd, and none is
//   under work/).
// - publish: the remote's branch solos/<t>/<slug> holds exactly the
//   worker's commit ids; a second publish of a new commit moves it and
//   keeps PR #1; a repeat of the same commit changes nothing; a push by
//   someone else is refused Leased and is not overwritten; a commit that
//   touches .github/workflows/x.yml is refused Protected and nothing is
//   pushed; a commit not descending from the base is refused; a planted
//   `core.fsmonitor` in /work/.git/config never runs on the host.
// - the token: a canary token value appears in no transcript, no sandbox
//   environment, no argv (/proc/<pid>/cmdline of every git the host ran),
//   and no event.
// - binding: after publish, a person's comment on the PR Thread reaches the
//   Ticket as an Input, and a resumed Ticket's worker can pr_view it.
// - watcher: CI set to passed on the LocalForge wakes the worker once with
//   a Note; a second pass sends nothing; a head the worker moved resets CI
//   to pending; a restart of serve keeps watching.
