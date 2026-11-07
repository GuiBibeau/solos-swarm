//! The host's mirrors: one bare repository per configured repository, which
//! only the host writes. Every git command the host runs against a Ticket's
//! code runs here, in a mirror whose config the host alone wrote: a worker
//! writes the clone's `.git/config` (`core.hooksPath`, `core.fsmonitor`,
//! `core.sshCommand`, `credential.helper` and `url.*.insteadOf` each make git
//! run a program or reach a host the worker chose), so the host never runs
//! git in `work/` once `repo.json` exists, and never writes into it.
//!
//! What crosses from a worker's tree is one `git bundle` file made in a
//! sandbox. It is untrusted data: opened without following links, size
//! limited, copied beside the mirrors (`forge-stage`, which a mirror rebuild
//! cannot lose), fetched with `fsckObjects`, and judged by git in the mirror.
//! The push to the forge runs from the mirror, with a token that exists only
//! in that git's environment.

use super::deps::{self, Cache};
use super::paths;
use crate::agent::config::{DepsKind, Identity, RepoConfig};
use crate::agent::pi_executor::private_dir;
use crate::agent::ports::{Dependencies, Proposal, Refusal, TicketDirs, Workspace};
use crate::broker::write_json;
use anyhow::{Context, Result, anyhow, bail, ensure};
use solos_agent_core::change::{BranchName, CommitSha, Harvest, RepoName};
use solos_agent_core::model::TicketId;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;

/// `<ticket>/host/repo.json`. Written last, so it also marks the clone done.
pub const REPO_JSON: &str = "repo.json";
/// The longest a git command that reaches the network may run. A GitHub token
/// is refreshed with more than this left, so a command that starts with one
/// finishes before it expires.
pub(crate) const GIT_WAIT: Duration = Duration::from_secs(600);
/// The longest a git command on the mirror alone may run.
const LOCAL_WAIT: Duration = Duration::from_secs(120);
const STDERR_CHARS: usize = 400;
/// Staging refs one Ticket keeps in the mirror; older ones are a cache that
/// the staged bundles can make again.
const KEPT_REFS: usize = 8;
/// A change of more files than this is refused, whatever its bytes.
pub const MAX_CHANGE_FILES: u32 = 2_000;
/// A range of more commits than this is refused: the worker squashes it.
const MAX_COMMITS: usize = 500;

/// Where a mirror fetches from. `url` is also the clone's `origin`, so it
/// never holds a credential.
pub(super) struct Remote<'a> {
    pub(super) url: &'a str,
    /// A git config key and value for the command alone.
    pub(super) auth: Option<(&'a str, &'a str)>,
}

pub(super) struct Mirrors {
    dir: PathBuf,
    /// `<state>/forge-stage`: admitted bundles, outside the mirrors.
    stage: PathBuf,
    repos: BTreeMap<RepoName, (RepoConfig, Mutex<()>)>,
    /// The repositories' dependency builds, one at a time each. A build can
    /// take minutes, so it has a lock apart from the one that serializes git
    /// on the mirror, which a Ticket's clone and a publish wait on.
    builds: BTreeMap<RepoName, Mutex<()>>,
    /// `<state>/deps`.
    deps: Cache,
}

/// What pushing a head to the Ticket's branch came to.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Branch {
    /// The branch now holds the head.
    Pushed,
    /// It already did: a crash after the push, or a repeat.
    Present,
    /// Someone else's commit is on the branch, which is never overwritten.
    Moved(CommitSha),
    /// The branch was there when the head was last delivered, and is not now.
    Gone,
    /// The remote refused the push, in these words.
    Rejected(String),
}

impl Mirrors {
    pub(super) fn new(dir: PathBuf, repos: &BTreeMap<RepoName, RepoConfig>) -> Self {
        let stage = dir.with_file_name(super::STAGE_DIR);
        let deps = Cache::new(dir.with_file_name(super::DEPS_DIR));
        Self {
            dir,
            stage,
            repos: repos
                .iter()
                .map(|(repo, terms)| (repo.clone(), (terms.clone(), Mutex::new(()))))
                .collect(),
            builds: repos
                .keys()
                .map(|repo| (repo.clone(), Mutex::new(())))
                .collect(),
            deps,
        }
    }

    /// The repository's dependencies at the commit `workspace`'s clone
    /// started from (see [`Forge::dependencies`]). The files an install reads
    /// are taken from the mirror under its lock, which is brief; the build
    /// runs after it, under the repository's build lock alone.
    ///
    /// [`Forge::dependencies`]: crate::agent::ports::Forge::dependencies
    pub(super) async fn dependencies(&self, workspace: &Workspace) -> Dependencies {
        let repo = &workspace.repo;
        let (Some((terms, lock)), Some(build)) = (self.repos.get(repo), self.builds.get(repo))
        else {
            return Dependencies::None;
        };
        let Some(config) = &terms.deps else {
            return Dependencies::None;
        };
        let unavailable = |why: String| Dependencies::Unavailable { why };
        let read = {
            let _one_command_at_a_time = lock.lock().await;
            let mirror = self.mirror_of(repo);
            if mirror.is_dir() {
                deps::read_inputs(&mirror, &workspace.base_commit, config.kind).await
            } else {
                Err(anyhow!("the host's mirror of {repo} is not there"))
            }
        };
        let inputs = match read {
            Ok(inputs) => inputs,
            Err(err) => {
                let why = format!("{err:#}");
                eprintln!(
                    "agent: deps: {repo} {} are unavailable: {why}",
                    config.kind.as_str()
                );
                return unavailable(why);
            }
        };
        let _one_build_at_a_time = build.lock().await;
        self.deps.ensure(repo, &inputs, config).await
    }

    #[cfg(test)]
    pub(super) fn lock_of(&self, repo: &RepoName) -> &Mutex<()> {
        &self.repos[repo].1
    }

    fn mirror_of(&self, repo: &RepoName) -> PathBuf {
        self.dir
            .join(repo.owner())
            .join(format!("{}.git", repo.name()))
    }

    pub(super) async fn prepare(
        &self,
        repo: &RepoName,
        dirs: TicketDirs<'_>,
        remote: &Remote<'_>,
        identity: &Identity,
    ) -> Result<Workspace> {
        let marker = dirs.host.join(REPO_JSON);
        if let Some(done) = prepared(&marker)? {
            return Ok(done);
        }
        let (terms, lock) = self
            .repos
            .get(repo)
            .with_context(|| format!("the forge has no repository {repo}"))?;
        let _one_ticket_at_a_time = lock.lock().await;
        // A launch of this Ticket that overlapped this one may have cloned
        // while it waited, and then `work/` is the worker's.
        if let Some(done) = prepared(&marker)? {
            return Ok(done);
        }
        let owner = self.dir.join(repo.owner());
        private_dir(&owner)?;
        let mirror = owner.join(format!("{}.git", repo.name()));
        refresh(&mirror, repo, remote).await?;
        let mirror = mirror.as_os_str();
        let tip = format!("refs/heads/{}^{{commit}}", terms.base);
        let base_commit = run(
            git([
                "-C".as_ref(),
                mirror,
                "rev-parse".as_ref(),
                "--verify".as_ref(),
                "--quiet".as_ref(),
                tip.as_ref(),
            ]),
            "rev-parse",
        )
        .await
        .with_context(|| format!("{repo} has no branch {}", terms.base))?;

        if dirs.work.exists() {
            std::fs::remove_dir_all(dirs.work)
                .with_context(|| format!("cannot clear {}", dirs.work.display()))?;
        }
        private_dir(dirs.work)?;
        let name = format!("user.name={}", identity.name);
        let email = format!("user.email={}", identity.email);
        run(
            git([
                "-c".as_ref(),
                "core.hooksPath=/dev/null".as_ref(),
                "clone".as_ref(),
                "--quiet".as_ref(),
                "--no-local".as_ref(),
                "--template=".as_ref(),
                "--branch".as_ref(),
                terms.base.as_ref(),
                "--config".as_ref(),
                name.as_ref(),
                "--config".as_ref(),
                email.as_ref(),
                "--".as_ref(),
                mirror,
                dirs.work.as_os_str(),
            ]),
            "clone",
        )
        .await?;
        run(
            git([
                "-C".as_ref(),
                dirs.work.as_os_str(),
                "remote".as_ref(),
                "set-url".as_ref(),
                "origin".as_ref(),
                remote.url.as_ref(),
            ]),
            "remote set-url",
        )
        .await?;
        if let Some(deps) = &terms.deps {
            exclude_installed(dirs.work, deps.kind)?;
        }
        let workspace = Workspace {
            repo: repo.clone(),
            base: terms.base.clone(),
            base_commit,
            notes: terms.notes.clone(),
            evidence: terms.evidence.clone(),
        };
        // The marker says the clone is whole, so it must not reach the disk
        // before the clone does.
        sync_to_disk(dirs.work).await?;
        write_json(&marker, &workspace)?;
        Ok(workspace)
    }

    /// Judge a proposal, in the mirror (see [`Forge::admit`]). A refusal is
    /// `Ok(Err(..))`; `Err` is the host's own trouble.
    ///
    /// [`Forge::admit`]: crate::agent::ports::Forge::admit
    pub(super) async fn admit(
        &self,
        repo: &RepoName,
        proposal: &Proposal<'_>,
        bot: &Identity,
    ) -> Result<std::result::Result<Harvest, Refusal>> {
        let (terms, lock) = self
            .repos
            .get(repo)
            .with_context(|| format!("the forge has no repository {repo}"))?;
        let _one_command_at_a_time = lock.lock().await;
        let mirror = self.mirror_of(repo);
        ensure_mirror(&mirror, repo)?;
        let bytes = match self.stage(proposal, terms.max_change_bytes)? {
            Ok(bytes) => bytes,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if let Err(why) = self
            .fetch_staged(&mirror, proposal.ticket, proposal.head)
            .await?
        {
            return Ok(Err(Refusal::BadBundle(why)));
        }
        let ours = stage_ref(proposal.ticket, proposal.head);
        let kept = self.judge(&mirror, proposal, bot, terms, bytes).await;
        if !matches!(kept, Ok(Ok(_))) {
            // A refused or failed proposal leaves no ref a Publish could use.
            let _ = run(
                git_in(&mirror, ["update-ref", "-d", ours.as_str()]),
                "update-ref",
            )
            .await;
        } else {
            prune_refs(&mirror, proposal.ticket).await;
        }
        kept
    }

    async fn judge(
        &self,
        mirror: &Path,
        proposal: &Proposal<'_>,
        bot: &Identity,
        terms: &RepoConfig,
        bytes: u64,
    ) -> Result<std::result::Result<Harvest, Refusal>> {
        let (head, base) = (proposal.head.as_str(), proposal.base_commit);
        let ours = format!("{}^{{commit}}", stage_ref(proposal.ticket, proposal.head));
        // The bundle's head is what git found in it, not what the sandbox said.
        let found = run(
            git_in(mirror, ["rev-parse", "--verify", "--quiet", ours.as_str()]),
            "rev-parse",
        )
        .await;
        if found.as_deref().ok() != Some(head) {
            return Ok(Err(Refusal::BadBundle(
                "the bundle does not hold the head the sandbox reported".into(),
            )));
        }
        let base_tip = format!("{base}^{{commit}}");
        if run(
            git_in(mirror, ["cat-file", "-e", base_tip.as_str()]),
            "cat-file",
        )
        .await
        .is_err()
        {
            return Ok(Err(Refusal::BadBundle(
                "the clone's base commit is not in the host's mirror".into(),
            )));
        }
        let ancestor = git_in(mirror, ["merge-base", "--is-ancestor", base.as_str(), head]);
        match status_of(ancestor, "merge-base").await? {
            0 => {}
            1 => {
                return Ok(Err(Refusal::NotDescendant { base: base.clone() }));
            }
            other => bail!("git merge-base exited {other}"),
        }
        // The agent's own range: reachable from the head and from nothing on
        // the base, so a newer base merged in is not judged.
        let branch_tip = format!("refs/heads/{}^{{commit}}", proposal.base);
        let on_base = run(
            git_in(
                mirror,
                ["rev-parse", "--verify", "--quiet", branch_tip.as_str()],
            ),
            "rev-parse",
        )
        .await
        .is_ok();
        let mut range = vec![head.to_owned(), "--not".into(), base.to_string()];
        if on_base {
            range.push(format!("refs/heads/{}", proposal.base));
        }
        let listed = run_bytes(git_range(mirror, ["rev-list"], &range), "rev-list", None).await?;
        let commits: Vec<&str> = std::str::from_utf8(&listed)
            .context("git rev-list is not UTF-8")?
            .lines()
            .collect();
        if commits.is_empty() {
            return Ok(Err(Refusal::NoCommits));
        }
        if commits.len() > MAX_COMMITS {
            return Ok(Err(Refusal::BadBundle(format!(
                "{} commits are more than the {MAX_COMMITS} the host takes at once: squash them",
                commits.len()
            ))));
        }
        // Every commit's author and committer is the bot, exactly.
        let idents = run_bytes(
            git_range(
                mirror,
                [
                    "log",
                    "--no-mailmap",
                    "-z",
                    "--format=%H%x00%an%x00%ae%x00%cn%x00%ce",
                ],
                &range,
            ),
            "log",
            None,
        )
        .await?;
        let fields: Vec<&[u8]> = idents
            .split(|b| *b == 0)
            .filter(|f| !f.is_empty())
            .collect();
        for chunk in fields.chunks(5) {
            let [id, an, ae, cn, ce] = chunk else {
                return Ok(Err(Refusal::BadBundle(
                    "a commit's identity is unreadable".into(),
                )));
            };
            let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
            let (id, author, committer) = (text(id), (text(an), text(ae)), (text(cn), text(ce)));
            let wrong = [("author", &author), ("committer", &committer)]
                .into_iter()
                .find(|(_, who)| who.0 != bot.name || who.1 != bot.email);
            if let Some((role, who)) = wrong {
                let Ok(commit) = CommitSha::try_from(id) else {
                    return Ok(Err(Refusal::BadBundle("a commit id is unreadable".into())));
                };
                return Ok(Err(Refusal::Identity {
                    commit,
                    found: format!("{} <{}> as its {role}", who.0, who.1),
                    expected: format!("{} <{}>", bot.name, bot.email),
                    base: base.clone(),
                }));
            }
        }
        // Per-commit changed paths, any commit in the range: GitHub judges
        // the push, and a reviewer reads commit by commit. Merge commits show
        // no paths here, as in `git log`; GitHub refuses a workflow file
        // whatever commit carries it, since the App holds no such permission.
        let plain = run_bytes(
            git_range(mirror, ["rev-list", "--no-merges"], &range),
            "rev-list",
            None,
        )
        .await?;
        let touched = run_bytes(
            git_in(
                mirror,
                [
                    "diff-tree",
                    "--stdin",
                    "-r",
                    "--name-only",
                    "-z",
                    "--no-renames",
                    "--root",
                    "--no-commit-id",
                ],
            ),
            "diff-tree",
            Some(plain),
        )
        .await?;
        let touched: BTreeSet<String> = touched
            .split(|b| *b == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect();
        let protected: Vec<String> = touched
            .iter()
            .filter(|path| paths::any(&terms.protected, path))
            .cloned()
            .collect();
        if !protected.is_empty() {
            return Ok(Err(Refusal::Protected {
                paths: protected,
                globs: terms.protected.clone(),
                base: base.clone(),
            }));
        }
        // The size of the change, and how many files it touches net.
        let net = run_bytes(
            git_in(
                mirror,
                [
                    "diff",
                    "--name-only",
                    "-z",
                    "--no-renames",
                    "--no-ext-diff",
                    base.as_str(),
                    head,
                    "--",
                ],
            ),
            "diff",
            None,
        )
        .await?;
        let files = net
            .split(|b| *b == 0)
            .filter(|path| !path.is_empty())
            .count();
        let files = u32::try_from(files).unwrap_or(u32::MAX);
        if files > MAX_CHANGE_FILES {
            return Ok(Err(Refusal::TooManyFiles {
                files,
                limit: MAX_CHANGE_FILES,
            }));
        }
        Ok(Ok(Harvest {
            head: proposal.head.clone(),
            base: proposal.base.to_owned(),
            commits: u32::try_from(commits.len()).unwrap_or(u32::MAX),
            files,
            bytes,
        }))
    }

    /// Copy the bundle beside the mirrors: opened without following a link,
    /// size limited, written whole and renamed into place. Its size.
    fn stage(
        &self,
        proposal: &Proposal<'_>,
        limit: u64,
    ) -> Result<std::result::Result<u64, Refusal>> {
        use std::io::Read as _;
        let bad = |why: &str| Ok(Err(Refusal::BadBundle(why.to_owned())));
        let mut file = match open_without_links(proposal.bundle) {
            Ok(file) => file,
            Err(_) => return bad("the sandbox left no readable bundle"),
        };
        let meta = file.metadata().context("cannot stat the bundle")?;
        if !meta.is_file() {
            return bad("the bundle is not a file");
        }
        if meta.len() > limit {
            return Ok(Err(Refusal::TooLarge {
                bytes: meta.len(),
                limit,
            }));
        }
        let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
        // One more byte than the limit: a file that grew while it was read.
        (&mut file)
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .context("cannot read the bundle")?;
        let size = bytes.len() as u64;
        if size > limit {
            return Ok(Err(Refusal::TooLarge { bytes: size, limit }));
        }
        let dir = self.stage.join(proposal.ticket.as_str());
        private_dir(&self.stage)?;
        private_dir(&dir)?;
        let target = dir.join(format!("{}.bundle", proposal.head));
        crate::sealed::private_file(&target, &bytes, true)
            .with_context(|| format!("cannot stage {}", target.display()))?;
        Ok(Ok(size))
    }

    /// Fetch the staged bundle's `HEAD` into the Ticket's staging ref, with
    /// every object checked. `Ok(Err(why))`: git refused the bundle.
    async fn fetch_staged(
        &self,
        mirror: &Path,
        ticket: &TicketId,
        head: &CommitSha,
    ) -> Result<std::result::Result<(), String>> {
        let bundle = self
            .stage
            .join(ticket.as_str())
            .join(format!("{head}.bundle"));
        let spec = format!("+HEAD:{}", stage_ref(ticket, head));
        let mut fetch = git_in(
            mirror,
            [
                "-c",
                "transfer.fsckObjects=true",
                "-c",
                "fetch.fsckObjects=true",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                "--",
            ],
        );
        fetch.arg(&bundle).arg(&spec);
        match run(fetch, "fetch").await {
            Ok(_) => Ok(Ok(())),
            Err(err) => {
                let said = format!("{err:#}");
                // The bundle's own faults are the worker's to mend; a missing
                // directory or a full disk are the host's.
                if said.contains("fatal:") || said.contains("error:") {
                    Ok(Err(clip_message(&said)))
                } else {
                    Err(err)
                }
            }
        }
    }

    /// Make the forge's branch hold `head`, from the mirror, with `lease`
    /// as the head this host last confirmed there. Never overwrites a
    /// commit that is not the host's own.
    pub(super) async fn push_head(
        &self,
        repo: &RepoName,
        remote: &Remote<'_>,
        ticket: &TicketId,
        head: &CommitSha,
        branch: &BranchName,
        lease: Option<&CommitSha>,
    ) -> Result<Branch> {
        let (_, lock) = self
            .repos
            .get(repo)
            .with_context(|| format!("the forge has no repository {repo}"))?;
        let _one_command_at_a_time = lock.lock().await;
        let mirror = self.mirror_of(repo);
        ensure_mirror(&mirror, repo)?;
        self.ensure_staged(&mirror, ticket, head).await?;
        let on = format!("refs/heads/{branch}");
        let tip = ls_remote(&mirror, remote, &on).await?;
        let expect = match (&tip, lease) {
            (Some(found), _) if found == head => return Ok(Branch::Present),
            (None, None) => None,
            (None, Some(_)) => return Ok(Branch::Gone),
            (Some(found), Some(lease)) if found == lease => Some(lease),
            (Some(found), _) => return Ok(Branch::Moved(found.clone())),
        };
        let spec = format!("{}:{on}", stage_ref(ticket, head));
        let mut push = git_in(&mirror, ["push", "--quiet", "--no-verify"]);
        if let Some(lease) = expect {
            push.arg(format!("--force-with-lease={on}:{lease}"));
        }
        push.args(["--", remote.url]).arg(&spec);
        with_auth(&mut push, remote);
        match run_for(push, "push", GIT_WAIT).await {
            Ok(_) => Ok(Branch::Pushed),
            Err(err) => {
                let said = format!("{err:#}");
                // A remote that refused in words (a ruleset, a workflow file
                // without permission) is a decided no; anything else is the
                // network, and the next attempt looks at the branch again.
                if said.contains("[remote rejected]") {
                    Ok(Branch::Rejected(clip_message(&said)))
                } else {
                    Err(err)
                }
            }
        }
    }

    /// The Ticket's staging ref holds `head`: when a rebuilt mirror lost it,
    /// it is made again from the staged bundle, which is the one owner of the
    /// bytes.
    async fn ensure_staged(
        &self,
        mirror: &Path,
        ticket: &TicketId,
        head: &CommitSha,
    ) -> Result<()> {
        let ours = format!("{}^{{commit}}", stage_ref(ticket, head));
        let held = run(
            git_in(mirror, ["rev-parse", "--verify", "--quiet", ours.as_str()]),
            "rev-parse",
        )
        .await;
        if held.as_deref().ok() == Some(head.as_str()) {
            return Ok(());
        }
        let bundle = self
            .stage
            .join(ticket.as_str())
            .join(format!("{head}.bundle"));
        ensure!(
            bundle.is_file(),
            "the staged bundle of {head} is gone from {}",
            bundle.display()
        );
        match self.fetch_staged(mirror, ticket, head).await? {
            Ok(()) => Ok(()),
            Err(why) => bail!("the staged bundle of {head} no longer fetches: {why}"),
        }
    }
}

/// `refs/solos/<ticket>/<head>`: the cache of one staged bundle in the mirror.
fn stage_ref(ticket: &TicketId, head: &CommitSha) -> String {
    format!("refs/solos/{ticket}/{head}")
}

fn ensure_mirror(mirror: &Path, repo: &RepoName) -> Result<()> {
    ensure!(
        mirror.is_dir(),
        "the host's mirror of {repo} is not there; a clone makes it"
    );
    Ok(())
}

/// Keep the newest [`KEPT_REFS`] staging refs of a Ticket.
async fn prune_refs(mirror: &Path, ticket: &TicketId) {
    let prefix = format!("refs/solos/{ticket}/");
    let Ok(listed) = run(
        git_in(
            mirror,
            [
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(refname)",
                prefix.as_str(),
            ],
        ),
        "for-each-ref",
    )
    .await
    else {
        return;
    };
    for old in listed.lines().skip(KEPT_REFS) {
        let _ = run(git_in(mirror, ["update-ref", "-d", old]), "update-ref").await;
    }
}

/// The tip of `on` on the remote, from `ls-remote`, or `None` when absent.
async fn ls_remote(mirror: &Path, remote: &Remote<'_>, on: &str) -> Result<Option<CommitSha>> {
    let mut list = git_in(mirror, ["ls-remote", "--"]);
    list.arg(remote.url).arg(on);
    with_auth(&mut list, remote);
    let listed = run_for(list, "ls-remote", GIT_WAIT).await?;
    for line in listed.lines() {
        let Some((sha, name)) = line.split_once('\t') else {
            continue;
        };
        if name.trim() == on {
            let sha = CommitSha::try_from(sha.trim().to_owned())
                .map_err(|why| anyhow!("ls-remote answered {why}"))?;
            return Ok(Some(sha));
        }
    }
    Ok(None)
}

fn with_auth(command: &mut Command, remote: &Remote<'_>) {
    if let Some((key, value)) = remote.auth {
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", key)
            .env("GIT_CONFIG_VALUE_0", value);
    }
}

/// Open `path` without following a link, where the platform can say so.
#[cfg(target_os = "linux")]
fn open_without_links(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(target_os = "linux"))]
fn open_without_links(path: &Path) -> std::io::Result<std::fs::File> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::other("a link"));
    }
    std::fs::File::open(path)
}

/// Keep what the dependency cache puts in the clone out of its commits and
/// its `git status`: `node_modules` is mounted over the clone, and a tool the
/// package manager runs may leave its own directory in `/work` (a dirty tree
/// fails a repository's evidence check, and `git add -A` would commit it). The
/// host writes this before any sandbox has touched the clone.
fn exclude_installed(work: &Path, kind: DepsKind) -> Result<()> {
    let info = work.join(".git/info");
    std::fs::create_dir_all(&info).with_context(|| format!("cannot make {}", info.display()))?;
    let own = match kind {
        DepsKind::Bun => "/.bun/\n",
    };
    let text = format!("# solos-swarm: the host's dependency cache\nnode_modules/\n{own}");
    std::fs::write(info.join("exclude"), text)
        .with_context(|| format!("cannot write {}", info.join("exclude").display()))
}

/// What `repo.json` says, or `None` before the clone is done.
pub(super) fn prepared(marker: &Path) -> Result<Option<Workspace>> {
    match std::fs::read(marker) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("{} is not what the forge wrote", marker.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", marker.display())),
    }
}

/// Bring `mirror` up to the remote's branches and tags. A fetch that was
/// killed can leave lock files or temporary packs that fail every fetch
/// after it, so a failed fetch clears them and tries again, and a mirror
/// that still fails is made again from nothing.
async fn refresh(mirror: &Path, repo: &RepoName, remote: &Remote<'_>) -> Result<()> {
    let Err(first) = fetch_into(mirror, remote).await else {
        return Ok(());
    };
    eprintln!("agent: forge: the mirror of {repo} failed ({first:#}); clearing its stale files");
    clear_stale(mirror, false);
    let Err(second) = fetch_into(mirror, remote).await else {
        return Ok(());
    };
    eprintln!("agent: forge: the mirror of {repo} failed again ({second:#}); making it again");
    match std::fs::remove_dir_all(mirror) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            return Err(err).with_context(|| format!("cannot remove {}", mirror.display()));
        }
        _ => {}
    }
    fetch_into(mirror, remote)
        .await
        .with_context(|| format!("the mirror of {repo} failed, was made again, and failed"))
}

async fn fetch_into(mirror: &Path, remote: &Remote<'_>) -> Result<()> {
    let mirror = mirror.as_os_str();
    run(
        git([
            OsStr::new("init"),
            "--bare".as_ref(),
            "--quiet".as_ref(),
            mirror,
        ]),
        "init",
    )
    .await?;
    run(fetch(mirror, remote), "fetch").await?;
    Ok(())
}

/// Remove what a killed git leaves in a repository: its lock files, and the
/// temporary packs and objects it was writing. Links are not followed.
fn clear_stale(dir: &Path, in_objects: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            clear_stale(&entry.path(), in_objects || name == "objects");
        } else if kind.is_file()
            && (name.ends_with(".lock") || (in_objects && name.starts_with("tmp_")))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Write what is pending on `dir`'s filesystem to disk.
async fn sync_to_disk(dir: &Path) -> Result<()> {
    let owned = dir.to_owned();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&owned)?;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: the descriptor is open for the whole call, and syncfs
            // reads nothing from this process's memory.
            if unsafe { libc::syncfs(file.as_raw_fd()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        #[cfg(not(target_os = "linux"))]
        file.sync_all()?;
        Ok::<(), std::io::Error>(())
    })
    .await?
    .with_context(|| format!("cannot sync {}", dir.display()))
}

/// A git with nothing of the host's: no system or global config, no prompts,
/// the locale fixed, and replace refs off, so what the host's git believes
/// about an object is what the object says.
pub(super) fn git<'a>(args: impl IntoIterator<Item = &'a OsStr>) -> Command {
    let mut git = Command::new("git");
    git.args(args)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = std::env::var_os("PATH") {
        git.env("PATH", path);
    }
    git
}

/// [`git_in`] with `args`, then the revisions in `range`, then `--`.
fn git_range<'a>(dir: &Path, args: impl IntoIterator<Item = &'a str>, range: &[String]) -> Command {
    let mut command = git_in(dir, args);
    command.args(range).arg("--");
    command
}

/// [`git`] on the mirror at `dir`, which only the host writes.
pub(super) fn git_in<'a>(dir: &Path, args: impl IntoIterator<Item = &'a str>) -> Command {
    let mut command = git(["-C".as_ref(), dir.as_os_str()]);
    command.args(args);
    command
}

pub(super) fn fetch(mirror: &OsStr, remote: &Remote<'_>) -> Command {
    let mut fetch = git([
        "-C".as_ref(),
        mirror,
        "-c".as_ref(),
        "transfer.fsckObjects=true".as_ref(),
        "fetch".as_ref(),
        "--quiet".as_ref(),
        "--prune".as_ref(),
        "--".as_ref(),
        remote.url.as_ref(),
        "+refs/heads/*:refs/heads/*".as_ref(),
        "+refs/tags/*:refs/tags/*".as_ref(),
    ]);
    with_auth(&mut fetch, remote);
    fetch
}

pub(super) async fn run(git: Command, what: &str) -> Result<String> {
    let bytes = run_for_bytes(git, what, GIT_WAIT, None).await?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
}

async fn run_for(git: Command, what: &str, wait: Duration) -> Result<String> {
    let bytes = run_for_bytes(git, what, wait, None).await?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
}

/// A git command on the mirror alone: its raw output, with `stdin` written to
/// it.
pub(super) async fn run_bytes(git: Command, what: &str, stdin: Option<Vec<u8>>) -> Result<Vec<u8>> {
    run_for_bytes(git, what, LOCAL_WAIT, stdin).await
}

async fn run_for_bytes(
    mut git: Command,
    what: &str,
    wait: Duration,
    stdin: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    git.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() {
        git.stdin(Stdio::piped());
    }
    let mut child = git
        .spawn()
        .with_context(|| format!("git {what} did not start"))?;
    let feed = child.stdin.take().zip(stdin).map(|(mut pipe, input)| {
        tokio::spawn(async move {
            pipe.write_all(&input).await?;
            pipe.shutdown().await
        })
    });
    let output = tokio::time::timeout(wait, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("git {what} took longer than {wait:?}"))?
        .with_context(|| format!("git {what} did not finish"))?;
    if let Some(feed) = feed {
        // A command that exited early closes the pipe, which is not ours to report.
        let _ = feed.await;
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git {what} failed ({}): {}",
            output.status,
            clip_message(stderr.trim())
        );
    }
    Ok(output.stdout)
}

/// A command's exit code, for the ones where a non-zero is an answer.
async fn status_of(mut git: Command, what: &str) -> Result<i32> {
    git.stdout(Stdio::null()).stderr(Stdio::null());
    let status = tokio::time::timeout(LOCAL_WAIT, git.status())
        .await
        .map_err(|_| anyhow!("git {what} took longer than {LOCAL_WAIT:?}"))?
        .with_context(|| format!("git {what} did not start"))?;
    status
        .code()
        .ok_or_else(|| anyhow!("git {what} was killed"))
}

/// The last [`STDERR_CHARS`] characters of git's complaint.
fn clip_message(text: &str) -> String {
    let start = text
        .char_indices()
        .rev()
        .nth(STDERR_CHARS)
        .map_or(0, |(at, _)| at);
    text[start..].to_owned()
}
