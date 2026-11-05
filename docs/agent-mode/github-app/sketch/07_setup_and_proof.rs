//! Lives at: src/agent/github/setup.rs (the two subcommands) and
//! tests/support/fake_github.rs (the offline proof's server, grown from the
//! fake that tests/agent_github.rs already holds).
//!
//! Sketch only: not compiled. Candidate B's setup and fake, amended by the
//! synthesis: the App's webhook is inactive and subscribes to no events
//! (intake is polling), the manifest is `docs/agent-mode/github-app/manifest.json`,
//! and the fake serves the REST listings, review states, account ids for
//! the pins, and a suspension switch instead of a delivery log.
//!
//! ---------------------------------------------------------------------------
//! The owner's setup, end to end (decision 7)
//!
//!   From PR 1, by hand (rationale.md, Usage (a)): create the App on
//!   github.com from the list in the README's "GitHub App" section, which is
//!   `manifest.json` in words. Generate a key, install the App on chosen
//!   repositories, store the key with `scripts/agent-deploy token
//!   github-app.pem < <downloaded pem>`, add the `github:` block, deploy, run
//!   `agent github check`.
//!
//!   From PR 6, by the CLI:
//!   1. solos-swarm agent github init --name solos-bot [--org acme]
//!        - binds 127.0.0.1:<port> (loopback only), prints and opens
//!          http://127.0.0.1:<port>/ , a page holding the manifest as a form.
//!          The manifest is BUILT, not hand-written: permissions from
//!          `Access::app_permissions()`, `hook_attributes.active: false`, no
//!          `default_events`, `redirect_url` set to the bound port. A test
//!          compares it to `docs/agent-mode/github-app/manifest.json` with
//!          `redirect_url` masked, so the docs and the code cannot drift.
//!          The form POSTs to github.com/settings/apps/new?state=<nonce>
//!          (or /organizations/<org>/settings/apps/new).
//!        - the owner clicks "Create GitHub App" once on github.com.
//!        - GitHub redirects the browser to /created?code=<c>&state=<nonce>.
//!          `state` is checked against the nonce; one request is served, then
//!          the listener closes.
//!        - POST https://api.github.com/app-manifests/<code>/conversions
//!          (no auth; the code lives one hour, single use) returns
//!          { id, slug, pem, webhook_secret, client_id, .. }.
//!        - writes ./github-app.pem mode 0600 (`private_file`, refuses to
//!          overwrite), DROPS the webhook secret (no receiver exists, so there
//!          is nothing to verify; it is not stored anywhere), and prints:
//!            app id, slug, the install URL
//!            https://github.com/apps/<slug>/installations/new,
//!            the exact github: block for agent.yaml, and the two commands:
//!              scripts/agent-deploy token github-app.pem < github-app.pem
//!              rm -P github-app.pem
//!   2. Install the App: "Only select repositories", pick the throwaway
//!      repository first.
//!   3. Edit ~/agent/agent.yaml (agent.yaml.example), delete any forge-token
//!      and github token files, `scripts/agent-deploy deploy`.
//!   Box:
//!   4. solos-swarm agent github check --config ~/agent/agent.yaml \
//!        --key-file ~/agent/tokens/github-app.pem
//!      (reads the durable copy; never deletes). It is also run by `doctor`.
//!      `serve` runs the same preflight at start but only logs it.
//! ---------------------------------------------------------------------------

use anyhow::Result;
use std::path::PathBuf;

pub struct InitArgs {
    pub name: String,
    pub org: Option<String>,
    /// Test seam: `https://github.com` in production.
    pub github_url: String,
    /// Test seam: `https://api.github.com` in production.
    pub api_url: String,
    pub out: PathBuf,
    /// Open the owner's browser. Off in tests, which play the browser.
    pub open: bool,
}

pub struct Created {
    pub app_id: u64,
    pub slug: String,
    pub pem: PathBuf,
    pub install_url: String,
}

pub async fn init(_args: InitArgs) -> Result<Created> {
    todo!()
}

pub struct CheckArgs {
    pub config: PathBuf,
    pub key_file: Option<PathBuf>,
}

/// One row per configured repository and one verdict line. Nothing is written
/// anywhere except missing identity pins. Checks, in order, each naming what
/// to do on failure; any failure exits nonzero:
///
///   key parses as RSA; JWT accepted (`GET /app`) -> slug and bot login
///   per repository: `GET /repos/<r>/installation` (installed, not suspended,
///            else the install URL); the installation's permissions cover
///            `Access::app_permissions()`, and DO NOT include `workflows` or
///            `administration` (refused if present: "remove workflows; the
///            agent never edits CI")
///   mint a Read and a Write token for one repository, `GET /repos/<r>` with the
///            first; the Write token is never used here
///   the base branch exists; branch protection is not read (no admin permission)
///   the owner's `policy.people` / `approvers` github: logins exist and are
///            pinned (`GET /users/<login>`); prints each pin and the pin file
pub async fn check(_args: CheckArgs) -> Result<()> {
    todo!()
}

// ---------------------------------------------------------------------------
// The fake GitHub (tests/support/fake_github.rs). One axum server, no real
// network. It is the PR 1..6 proof harness and the thing `init`/`check` run
// against. What it must serve, and what it can be told:
// ---------------------------------------------------------------------------

/// REST it serves (everything the design calls):
///   POST /app-manifests/{code}/conversions
///   GET  /app                                   (JWT: RS256, iss, exp - iat <= 10 min, test key)
///   GET  /repos/{r}/installation                (JWT; 404 when uninstalled)
///   POST /app/installations/{id}/access_tokens  (JWT; records repositories + permissions;
///                                                token `ghs_fake_<n>` with expires_at;
///                                                403 "suspended" when suspended)
///   GET  /users/{login}                         (login and numeric id, for the pins; a
///                                                login can be "renamed" to a new id)
///   GET  /repos/{r}, /repos/{r}/issues, /issues/comments, /pulls/comments,
///        /actions/runs, /issues/events          (the listings, with ETags and 304s)
///   GET  /repos/{r}/pulls?head=, /repos/{r}/pulls/{n}, /repos/{r}/pulls/{n}/reviews
///   POST /repos/{r}/pulls                       (Write token for {r} only; else 403/404)
///   PUT  /repos/{r}/pulls/{n}/merge             (sha must equal head; else 409; honours Merge::{Ok,Protected405,Queued})
///   POST /repos/{r}/issues/{n}/comments
///   GET  /repos/{r}/commits/{sha}/check-runs, /status
///                                               (check runs carry `app.slug` and `details_url`)
///   GET  /repos/{r}/actions/jobs/{id}/logs      (302 to /logs/{id}; the redirect target must receive NO Authorization header)
///
/// Every request is recorded with the bearer's *scope* (repository set,
/// access), so a test can assert "this push/POST used a token narrowed to
/// exactly this repository with write", "no Read token ever wrote", and
/// "no token was minted for a repository outside forge.repos".
pub struct FakeGithub {
    _private: (),
}

impl FakeGithub {
    pub async fn start(_app_key: &rsa::RsaPublicKey) -> Self {
        todo!()
    }
    pub fn api_base(&self) -> String {
        todo!()
    }

    /// People act as GitHub would show it in the listings.
    pub fn open_issue(&self, _repo: &str, _by: &str, _title: &str, _body: &str) -> u64 {
        todo!()
    }
    pub fn comment(&self, _repo: &str, _number: u64, _by: &str, _body: &str) -> u64 {
        todo!()
    }
    pub fn label(&self, _repo: &str, _number: u64, _by: &str, _label: &str) {}
    pub fn review(&self, _repo: &str, _pr: u64, _by: &str, _state: &str, _commit: &str, _body: &str) -> u64 {
        todo!()
    }
    /// The login keeps its name and gets another account id, as a rename and
    /// a reclaim look from outside.
    pub fn reclaim_login(&self, _login: &str, _new_id: u64) {}

    pub fn set_checks(&self, _repo: &str, _sha: &str, _runs: Vec<Run>) {}
    pub fn set_merge(&self, _repo: &str, _pr: u64, _how: MergeBehavior) {}
    /// A person closes it / pushes to its branch / merges it by hand.
    pub fn close_pr(&self, _repo: &str, _pr: u64) {}
    pub fn push_as_other(&self, _repo: &str, _branch: &str) {}
    /// The owner's kill switch: suspend, unsuspend, uninstall, reinstall.
    pub fn suspend(&self, _on: bool) {}
    pub fn uninstall(&self, _repo: &str) {}
    pub fn reinstall(&self, _repo: &str) {}
    /// Answer the next matching request with this fault *after* performing it
    /// (a PR created, then 500), which is what a crash looks like from outside.
    pub fn fault_after(&self, _method: &str, _path_prefix: &str, _status: u16) {}

    /// What every request carried, for the scope assertions above.
    pub fn requests(&self) -> Vec<Seen> {
        todo!()
    }
}

pub struct Run {
    pub name: String,
    /// `github-actions` or another app's slug.
    pub app: String,
    pub conclusion: Option<String>,
    /// For an Actions run, `.../actions/runs/<run>/job/<job>`; the job id is
    /// deliberately not the check run's id.
    pub details_url: String,
    pub log: String,
}

pub enum MergeBehavior {
    Ok,
    /// 405 "Required status check ... is expected" or "Base branch policy
    /// prohibits the merge".
    Protected405(String),
    /// 200 but not merged until `finish_queue` (a merge queue).
    Queued,
}

pub struct Seen {
    pub method: String,
    pub path: String,
    /// `None` for a JWT call; `(repositories, Write?)` for an installation token.
    pub scope: Option<(Vec<String>, bool)>,
}

/// The git half is a directory of bare repositories addressed with
/// `github.git_base: file:///<dir>/`, so the real mirror, bundle, lease and
/// push code run with no network and no HTTP git server. The scope of a git
/// push is asserted where it is made: a wrapper `git` first on the host's PATH
/// logs argv (the token must not be in it), environment keys (the extraheader
/// must be, in the mirror's git only), and cwd (never under a ticket's `work/`).
pub struct GitHalf;
