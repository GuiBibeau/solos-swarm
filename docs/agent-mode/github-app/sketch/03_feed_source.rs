//! Lives at: src/agent/github/{event.rs, feed.rs, source.rs} (event.rs and
//! feed.rs new; source.rs keeps its loop and loses its listing code to
//! feed.rs).
//!
//! Sketch only: not compiled. This file is where the synthesis departs most
//! from candidate B. B read the App's webhook delivery log
//! (`GET /app/hook/deliveries`) on a webhook pointed at a URL that cannot
//! receive. Here the primary intake is REST polling: the github Source's
//! existing listings, each ETag'd, with today's cursor file, overlap window
//! and dedupe, moved behind B's `EventFeed` trait as `RestFeed`, the one
//! shipped implementation, and extended where the design needs more. A
//! periodic reconcile pass calls `Forge::observe` on every open Change.
//!
//! Why: polling is already built, already survives restarts, needs no
//! public endpoint, no relay and no secret, and its quiet polls are 304s that
//! cost nothing against the rate limit. The delivery log rests on a niche API
//! whose retention, ordering and behaviour on a webhook that always fails
//! are unproven; the cross-judge named it B's main risk. It stays possible as
//! a second `EventFeed` (rationale: Alternatives; prs.md: Later), behind its
//! own live spike, and nothing here depends on it.
//!
//! Events are two kinds of thing, kept apart:
//!   * SPEECH (a person said or gestured something: an issue, a comment, a
//!     review summary, a label): becomes a Trigger, judged by the Policy and
//!     the Router, exactly as today.
//!   * STATE (CI finished, the bot's pull request moved, closed or was
//!     reviewed): becomes a `Nudge`. A nudge carries no facts. It says "look
//!     at this branch again", and the host reads the forge
//!     (`Forge::observe`), which is the only thing that produces state facts.
//!     The reconcile pass calls the same function every five minutes for
//!     every open Change. Events are hints, the forge is the truth, so a
//!     missed, late or duplicated listing entry cannot corrupt state.
//!
//! Approving and change-requesting reviews on the bot's pull requests are a
//! person's control, not speech and not state: `observe` reads every
//! reviewer's latest review state at the head (04_forge.rs `Look`), and this
//! Source sends each as `Command(Order::Merge)`. Because the reconcile pass
//! reads them again, a review the listings never surfaced still reaches the
//! merge gate. (Fix of B: its `Seen` had no approvals, so a lost
//! `pull_request_review` delivery lost a grant.)

use crate::agent::ports::{BoxFuture, Command, FactSink, Source, TriggerSink};
use anyhow::Result;
use solos_agent_core::change::{BranchName, CommitSha, RepoName};
use solos_agent_core::model::{Actor, TicketId, Trigger};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------------------
// The domain events (event.rs). Wire JSON is parsed in feed.rs and nowhere
// else; nothing downstream sees a payload field name. Every user object
// becomes an `Actor` through `Identities::actor` (02_app_auth.rs), so a
// renamed-and-reclaimed listed login is already `<login>#<id>` here.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
}

/// Which open Change a state event is about. The runs listing names a head
/// branch; the issues listing names a pull request number. The Source
/// resolves either through `OpenChanges`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stir {
    Branch(String),
    Pr(u64),
}

/// What the Source understands of one listing entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A new issue or pull request (`issues` listing), as today.
    Opened {
        repo: RepoName,
        number: u64,
        is_pr: bool,
        title: String,
        body: String,
        by: Actor,
    },
    /// A conversation comment on an issue or pull request
    /// (`issues/comments`), as today.
    Commented {
        repo: RepoName,
        number: u64,
        comment: u64,
        body: String,
        on_pr: bool,
        by: Actor,
    },
    /// A line comment (`pulls/comments`), as today.
    ReviewCommented {
        repo: RepoName,
        number: u64,
        comment: u64,
        body: String,
        path: String,
        line: Option<u32>,
        commit: CommitSha,
        in_reply_to: Option<u64>,
        by: Actor,
    },
    /// A completed workflow run (`actions/runs?status=completed`), as today.
    Run {
        repo: RepoName,
        run: u64,
        attempt: u32,
        branch: String,
        head: CommitSha,
        failed: bool,
        /// Open pull requests the run is for (fork runs matched by head sha,
        /// as today).
        prs: Vec<u64>,
    },
    /// PR 4. A review summary on a pull request the bot authored
    /// (`pulls/{n}/reviews`, read when the issues listing shows that pull
    /// request updated, positioned by the cursor's `reviews` feed).
    Reviewed {
        repo: RepoName,
        number: u64,
        review: u64,
        state: ReviewState,
        body: String,
        commit: CommitSha,
        by: Actor,
    },
    /// PR 4, only when `github.trigger_label` is set. A `labeled` entry of the
    /// repository's issue events (`issues/events`), whose `actor` is the
    /// person who added the label.
    Labeled {
        repo: RepoName,
        number: u64,
        title: String,
        body: String,
        label: String,
        by: Actor,
    },
    /// Something about one of the bot's branches or pull requests changed:
    /// a pull request the bot authored appears in the issues listing with a
    /// newer `updated_at` (a push, a review, a close, a merge, a label).
    /// Carries no data the host will believe.
    Stirred { repo: RepoName, at: Stir },
}

// ---------------------------------------------------------------------------
// The feed port and its one adapter (feed.rs).
// ---------------------------------------------------------------------------

pub struct Item {
    /// Stable across re-reads: the Trigger id the entry maps to, or the
    /// entry's own id for state.
    pub id: String,
    pub at: SystemTime,
    pub event: Event,
}

pub struct Batch {
    /// Oldest first.
    pub items: Vec<Item>,
    /// A listing ran past its page limit (today: 10 pages of 100) before it
    /// reached the cursor: entries between were not read. The Source tells
    /// the operator once and moves on; state is healed by the reconcile
    /// pass, speech is not.
    pub gap: Option<Gap>,
}

pub struct Gap {
    pub repo: RepoName,
    pub feed: &'static str,
    pub since_ms: u64,
}

/// The intake port. The feed owns its position: `poll` reads everything new
/// since the saved position and does not move it; `commit` saves the position
/// the last `poll` reached, and the Source calls it only after the batch's
/// Triggers are reserved in the runtime, so a crash cannot leave the position
/// past a Trigger the Engine has not stored (today's discipline).
pub trait EventFeed: Send {
    fn poll<'a>(&'a mut self) -> BoxFuture<'a, Result<Batch>>;
    fn commit<'a>(&'a mut self) -> BoxFuture<'a, Result<()>>;
}

/// The one shipped `EventFeed`. Per configured repository, per poll, with a
/// narrowed `Read` token for that repository:
///
/// | listing                              | yields                                  | since | PR |
/// |---|---|---|---|
/// | `repos/{r}/issues` (state=all, sort=updated, asc, `since`) | `Opened`; `Stirred{Pr}` for the bot's own PRs | today | 3 |
/// | `repos/{r}/issues/comments`          | `Commented`                             | today | 3 |
/// | `repos/{r}/pulls/comments`           | `ReviewCommented`                       | today | 3 |
/// | `repos/{r}/actions/runs?status=completed` | `Run`                              | today | 3 |
/// | `repos/{r}/pulls/{n}/reviews`, for each bot PR the issues listing stirred | `Reviewed` | new | 4 |
/// | `repos/{r}/issues/events`, only with `trigger_label` | `Labeled`               | new | 4 |
///
/// The first four are today's listings, moved here unchanged: the same
/// `since` windows, the 600 s overlap, `seen` dedupe, ETags kept per
/// first-page URL, at most 10 pages, `<state>/github-cursor.json` written
/// whole through temp file + rename + directory fsync. Their Trigger ids are
/// byte for byte today's, so a store written by the PAT-era Source dedupes.
/// The two new listings are new keys in the same cursor file (`reviews` and
/// `events` per repository); the file's shape does not change, so a cursor
/// written by the old Source loads. `issues/events` has no `since`: it is
/// read newest first until an id at or below the saved one, with an ETag on
/// the first page, so a quiet repository costs one 304.
///
/// A quiet repository costs four (five with a label) conditional requests
/// per poll, all 304s, which GitHub does not count against the rate limit.
pub struct RestFeed {
    _private: (),
}

impl RestFeed {
    pub fn new(
        _api: Arc<crate::agent::github::GithubApi>,
        _repos: Vec<RepoName>,
        _cursor: PathBuf,
        _trigger_label: Option<String>,
    ) -> Self {
        todo!()
    }
}

impl EventFeed for RestFeed {
    fn poll<'a>(&'a mut self) -> BoxFuture<'a, Result<Batch>> {
        todo!()
    }
    fn commit<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// Event -> what the runtime understands (source.rs). Pure, table-tested.
// ---------------------------------------------------------------------------

pub enum Mapped {
    /// Speech. See `map` for id, actor, addressing and text.
    Trigger(Trigger),
    /// Read this Change again.
    Nudge { repo: RepoName, at: Stir },
    Ignore,
}

pub struct Me {
    pub bot: crate::agent::github::app::Bot,
    /// `github.trigger_label`, if the owner configured one.
    pub trigger_label: Option<String>,
}

/// The whole mapping, one place. Rules:
///
/// * Anything by the bot itself is `Ignore` (login compare, and the marker
///   prefix as a second belt). No echo loops.
/// * Triggers keep today's ids: `github:<repo>#<n>` for an opened issue/PR,
///   `github:<repo>:comment:<id>`, `github:<repo>:review_comment:<id>`,
///   `github:<repo>:run:<id>:<attempt>`; new: `github:<repo>:review:<id>`
///   and `github:<repo>#<n>:labeled:<event id>`.
/// * Actor: as `Identities::actor` made it; `Person` for a user, `Bot`
///   otherwise.
/// * Addressing: `Mention` when `me.bot.mentioned_in(text)` (the leading
///   mention is stripped before the worker sees it, as today); `Direct` for
///   a `Labeled` event whose label is `trigger_label` (the labeler is the
///   requester; only people with triage or write access can label, and the
///   Policy then judges that person); otherwise `Unaddressed`, which stays
///   `Inferred` and starts nothing unless the owner turned on
///   `accept_mode: auto` and a rule matches (unchanged behaviour).
/// * Text: "Repository <repo>. <Issue|Pull request> #<n>: <title>" then the
///   body, so the Router can name the repository from the Trigger alone.
/// * Threads: `github:<repo>#<n>` first. `meta` holds repo, number, kind,
///   path/line/commit for review comments (as today).
/// * `Reviewed`: `ChangesRequested` with a body is a Trigger whose text
///   starts "Requested changes:"; `Commented` with a body is a Trigger;
///   `Approved` is `Ignore` (its verdict is read by `observe`, and its words
///   do not resume work: "merge this head" needs no model turn). No review
///   is ever a Command here.
/// * `Run` on a branch that parses as a solos branch: `Nudge`. A failed run
///   on any other branch: today's bot Trigger, unchanged (a proactive notice
///   about a person's pull request). Anything else: `Ignore`.
/// * `Stirred`: `Nudge`.
pub fn map(_event: &Event, _me: &Me) -> Mapped {
    todo!()
}

// ---------------------------------------------------------------------------
// The Source (source.rs). Two loops on one task and no state of its own
// beyond the feed's cursor file.
// ---------------------------------------------------------------------------

/// An open Change as the Store has it: Tickets whose Change is `Open`, ended
/// Tickets included (repairs and merges come after `ready`), not stopped by
/// a person.
pub struct OpenChange {
    pub ticket: TicketId,
    pub repo: RepoName,
    pub branch: BranchName,
    pub pr: Option<u64>,
}

/// What the Source may read of the Store to know what to reconcile. A
/// read-only port, so the Source never holds the Engine and never writes
/// state.
pub trait OpenChanges: Send + Sync {
    fn open(&self) -> Vec<OpenChange>;
}

pub struct GithubSource {
    _private: (),
}

pub struct GithubSourceConfig {
    pub repos: Vec<RepoName>,
    pub poll: Duration,
    /// How often every open Change is read again whatever the listings said.
    pub reconcile: Duration,
    pub trigger_label: Option<String>,
    /// `false`: read once and end (tests, dry runs), as `follow` does today.
    pub follow: bool,
}

impl GithubSource {
    pub fn new(
        _config: GithubSourceConfig,
        _api: Arc<crate::agent::github::GithubApi>,
        _feed: Box<dyn EventFeed>,
        _forge: Arc<dyn crate::agent::ports::Forge>,
        _changes: Arc<dyn OpenChanges>,
    ) -> Self {
        todo!()
    }
}

impl Source for GithubSource {
    /// 1. `begin`: `minter.preflight` (one log line per repository; a
    ///    suspended or missing installation is a wait, never a stop; only a
    ///    key GitHub rejects is `Fatal`), then `identities.pin_missing`.
    /// 2. Every `poll`: `feed.poll()`; `map` each item; `sink.reserve_many`
    ///    for the Triggers; send them; collect Nudges (de-duplicated per
    ///    Change within the pass); THEN `feed.commit()`. A `Gap` is one
    ///    operator alert.
    /// 3. For each nudge, and for every open Change every `reconcile`:
    ///    `forge.observe(repo, branch)` -> `Some(Look { seen, verdicts })`:
    ///      `facts.send(Fact::Observed(Observed::Seen(seen)))`, then for each
    ///      verdict `facts.send(Fact::Command(Command {
    ///          id: "gh-review-<review id>",
    ///          order: Order::Merge { head, allow },
    ///          source: "github", by: verdict.by, reply_to: the PR }))`.
    ///    `observe` is the only producer of state facts and of GitHub merge
    ///    verdicts, so the periodic pass and the event-driven pass are the
    ///    same function called at different times, not two mechanisms.
    /// 4. Backoff and `RetryAfter` exactly as the old loop: doubling to 300 s,
    ///    `RetryAfter` wins (a suspended installation waits here), `Fatal`
    ///    stops.
    ///
    /// Restart: the cursor resumes the listings; Triggers dedupe by id in the
    /// Engine; Observations are idempotent; a verdict already recorded at
    /// that head appends nothing. First start reads from "now" (never
    /// replays a repository's history), as today.
    fn run(self: Box<Self>, _sink: TriggerSink, _facts: FactSink) -> BoxFuture<'static, Result<()>> {
        todo!()
    }
}

pub fn _unused(_: Command) {}

// ---------------------------------------------------------------------------
// Pinned by PR 3's and PR 4's offline tests (the fake GitHub serving the
// listings; tests/agent_github.rs):
//  - PR 3: every existing `agent_github` intake test passes with only its
//    setup changed (an App instead of a token); PAT-era Trigger ids
//    (`github:<repo>#<n>`, `:comment:<id>`, `:run:<id>:<attempt>`) are
//    reproduced byte for byte; a cursor file written by the old Source loads
//    and resumes without a repeat.
//  - PR 3: a completed run on `solos/<t>/<slug>` is one Nudge and no Trigger;
//    a failed run on `feature/x` is today's Trigger; the bot's PR appearing
//    with a newer `updated_at` is one Nudge; a Nudge and the reconcile pass
//    for the same Change in one pass call `observe` once.
//  - PR 3: a check run the listings never show (another CI app) reaches the
//    Change within one reconcile interval (manual clock).
//  - PR 4: a `labeled` entry by a listed person for `trigger_label` is one
//    Trigger, `Direct`, actor `github:alice`, the Router sees the repository
//    in its text; the same label by an unlisted person is stored and
//    rejected by the Policy (`not_listed`); another label is no Trigger; a
//    restart re-reads the events page and sends nothing twice.
//  - PR 4: a `changes_requested` review with a body by a listed reviewer on
//    the bot's PR is one Trigger on its Thread; an `approved` review is no
//    Trigger; a review on a person's PR is not read.
//  - PR 5: an approving review by a listed approver at the delivered head is
//    a Command (Order::Merge, allow) naming that Ticket, sent again by every
//    reconcile pass and recorded once; the same by a stranger or by
//    `alice#99` is a Command the Engine refuses as Forbidden; an approver's
//    later "request changes" at that head is a Command with allow false; a
//    review comment with text "merge it" is a Trigger and nothing else.
//  - `@solos-bot fix it` and `@solos-bot[bot] fix it` in an issue comment are
//    `Mention`, mention stripped; the bot's own comment (login match) and any
//    comment with the marker prefix are dropped.
// ---------------------------------------------------------------------------
