//! Merge queue, forge (src/agent/ports.rs, src/agent/forge/{mirror,github}.rs,
//! src/agent/environment.rs). Documentation, not compiled code.
//!
//! Every git command runs in the host's bare mirror, as today: never in a
//! worker's tree. Every write is an "ensure", so a rerun after a crash
//! converges.

use solos_agent_core::change::{CommitSha, PrRef, RepoName};
use solos_agent_core::model::TicketId;
use std::path::Path;

// ---------------------------------------------------------------------------
// The port (src/agent/ports.rs). `Forge` gains one method; `admit` and
// `execute` widen.
// ---------------------------------------------------------------------------

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

pub trait ForgeAdditions {
    /// GitHub App PR 3's `observe`, keyed by pull request instead of by a
    /// `solos/` branch: `GET pulls/{n}`, the check runs and statuses at its
    /// head (latest attempt of each), `GET compare/{base}...{head}`, and the
    /// reviews whose `commit_id` is the head. Read token only.
    fn observe(&self, repo: &RepoName, pr: u64) -> BoxFuture<'_, anyhow::Result<Look>>;

    /// A resolve round's clone: `work` at `from` on a local branch, `base`
    /// present as `origin/<base>`, `repo.json` with `base_commit: from` and
    /// `merge_from: base`, `/host/conflicts.json`. Same rules as `prepare`:
    /// host git only before any sandbox touched `work`.
    fn prepare_round<'a>(
        &'a self,
        repo: &'a RepoName,
        round: Round<'a>,
        work: &'a Path,
        host: &'a Path,
    ) -> BoxFuture<'a, anyhow::Result<()>>;
}

pub struct Look {
    // PullSeen (01_core.rs) plus the review verdicts at the head, which the
    // Source sends as Command(Order::Merge), as PR 5 planned.
}

#[derive(Clone, Copy, Debug)]
pub struct Round<'a> {
    pub ticket: &'a TicketId,
    pub pr: &'a PrRef,
    pub from: &'a CommitSha,
    pub base: &'a CommitSha,
    pub conflicts: &'a [String],
}

/// `Proposal` gains `range`. `Ticket` is today's rule; `Round` is a resolve
/// round's.
#[derive(Clone, Copy, Debug)]
pub enum Range<'a> {
    /// `head --not base_commit refs/heads/<base>` (mirror.rs:338-353).
    Ticket,
    /// `head --not from base`: only the round's own commits.
    Round {
        from: &'a CommitSha,
        base: &'a CommitSha,
        conflicts: &'a [String],
    },
}

/// `Refusal` gains:
pub enum RefusalAdditions {
    /// A merge commit's own edits (its tree against the host's merge-tree of
    /// its parents) touch a protected path. Q0: every Ticket.
    MergeProtected { commit: CommitSha, paths: Vec<String> },
    /// A round's merge commit edits a file that merged cleanly.
    MergeBeyondConflicts { commit: CommitSha, paths: Vec<String> },
    /// A round's first merge commit's parents are not exactly `[from, base]`.
    MergeParents { commit: CommitSha },
}

/// `Execute` gains `Update(Update)` and `Merge(Merge)`.
pub struct Update<'a> {
    pub ticket: &'a TicketId,
    pub repo: &'a RepoName,
    pub pr: &'a PrRef,
    pub branch: &'a str, // a PrBranch, validated
    pub from: &'a CommitSha,
    pub base: &'a CommitSha,
    /// `None`: the host merges. `Some`: an admitted round's head.
    pub head: Option<&'a CommitSha>,
}

pub struct Merge<'a> {
    pub ticket: &'a TicketId,
    pub repo: &'a RepoName,
    pub pr: &'a PrRef,
    pub head: &'a CommitSha,
    /// The tip the gate saw; the read-back checks the squash's parent.
    pub base: &'a CommitSha,
}

// ---------------------------------------------------------------------------
// The mirror (src/agent/forge/mirror.rs).
// ---------------------------------------------------------------------------

pub enum Merged {
    Clean { commit: CommitSha },
    Conflicted { paths: Vec<String> },
}

pub trait MirrorAdditions {
    /// `git merge-tree --write-tree --name-only from base` in the mirror. On
    /// a clean result, `git commit-tree -p from -p base` with the bot's
    /// identity in the command's own environment (the mirror's git clears
    /// it), kept under `refs/solos/<ticket>/update/<pr>/<from>-<base>` so a
    /// retry reuses the same object, then judged by `judge` with
    /// `Range::Round` and no conflicts. Needs git >= 2.38.
    fn host_merge(
        &self,
        repo: &RepoName,
        ticket: &TicketId,
        pr: &PrRef,
        from: &CommitSha,
        base: &CommitSha,
    ) -> BoxFuture<'_, anyhow::Result<Merged>>;

    /// For each merge commit in the range: its tree against
    /// `merge-tree --write-tree` of its parents. The paths that differ are
    /// the merge's own edits. Q0 landed in today's `judge` with `diff-tree
    /// -c` and the range's net diff, which needs no git >= 2.38; this is for
    /// a round, which also checks the edits against its conflicts.
    fn merge_edits(
        &self,
        mirror: &Path,
        range: &[String],
    ) -> BoxFuture<'_, anyhow::Result<Vec<(CommitSha, Vec<String>)>>>;

    /// `push_head` for a `PrBranch`: `ls-remote` first (equal to the target:
    /// done; equal to `from`: push; anything else: `Moved(found)`, which for
    /// a queue entry only re-observes), then
    /// `git push --force-with-lease=refs/heads/<branch>:<from> -- <url>
    /// <stage-ref>:refs/heads/<branch>`. The target always descends from
    /// `from`: asserted before the push, so the lease never forces.
    fn push_update(&self, update: &Update<'_>) -> BoxFuture<'_, anyhow::Result<PushOutcome>>;
}

pub enum PushOutcome {
    Pushed,
    Present,
    Moved(CommitSha),
    Rejected(String),
}

// ---------------------------------------------------------------------------
// GitHub (src/agent/forge/github.rs).
// ---------------------------------------------------------------------------

pub trait GithubAdditions {
    /// `PUT pulls/{n}/merge {sha: head, merge_method: "squash", commit_title,
    /// commit_message}`. 200: read back `GET pulls/{n}` (`merged`,
    /// `merge_commit_sha`) and `GET commits/{sha}` (its parent). 409: the
    /// head moved, so re-observe. 405: `MergeRefused` with GitHub's words. A
    /// PR already merged at `head` is the same answer as a merge just made.
    fn merge(&self, merge: &Merge<'_>) -> BoxFuture<'_, anyhow::Result<()>>;
}

// ---------------------------------------------------------------------------
// The harvest (src/agent/environment.rs:139-141). For a round, `BASE` is
// `from` and a new `MERGE_FROM` is `base`:
//
//     g bundle create /out/proposal.bundle HEAD --not "$BASE" "$MERGE_FROM"
//
// so the bundle holds only the round's commits, not the base's.
// ---------------------------------------------------------------------------

// Tests (tests/agent_github.rs, the fake grown as design §5.12 says):
//  - host_merge clean: one commit, parents [from, base], by the bot, judged;
//    a retry after a crash pushes the same object; a rebuilt mirror re-makes
//    one with the same tree and ls-remote recognizes it.
//  - host_merge conflicted: the paths, and nothing pushed.
//  - merge_edits: an evil merge's extra path is found; a clean merge has
//    none; a resolution's edits are exactly its conflicted files.
//  - push_update: a moved branch is Moved and nothing is pushed; a target
//    that does not descend from `from` is refused before git runs; a
//    pre-receive hook refusing a workflow file is Rejected with its words.
//  - PrBranch: `main`, the default branch, `refs/heads/x`, `a..b`, `-x`
//    refused; no Update can be built for the base.
//  - merge: the PUT carries the sha; 409, 405 and an already-merged PR each
//    map as above; the parent check.
