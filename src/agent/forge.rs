//! The host never runs git in a directory a sandbox has written: once
//! `host/repo.json` exists, `work/` is the worker's.

use super::config::{ForgeConfig, Identity, RepoConfig};
use super::github::{Access, GithubApp, InstallToken, Minter};
use super::pi_executor::private_dir;
use super::ports::{BoxFuture, Forge, TicketDirs, Workspace};
use crate::broker::write_json;
use anyhow::{Context, Result, anyhow, bail};
use solos_agent_core::change::RepoName;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;
use zeroize::Zeroizing;

/// `<state>/forge`, the mirrors.
pub const FORGE_DIR: &str = "forge";
/// `<ticket>/host/repo.json`. Written last, so it also marks the clone done.
pub const REPO_JSON: &str = "repo.json";
/// The longest a git command may run. A GitHub token is refreshed with more
/// than this left, so a fetch that starts with one finishes before it expires.
pub(crate) const GIT_WAIT: Duration = Duration::from_secs(600);
const STDERR_CHARS: usize = 400;

/// The forge `config` names, with its mirrors under `dir`. GitHub's goes
/// through the App `github` opened, whose key was read once.
pub fn open(
    config: &ForgeConfig,
    dir: PathBuf,
    github: Option<&GithubApp>,
) -> Result<Arc<dyn Forge>> {
    Ok(match config {
        ForgeConfig::Github { repos } => {
            let github = github.context("forge: github needs the github: section's App")?;
            Arc::new(GithubForge {
                mirrors: Mirrors::new(dir, repos),
                minter: github.minter.clone(),
                git_base: github.git_base.clone(),
            })
        }
        ForgeConfig::Local {
            remote,
            identity,
            repos,
        } => Arc::new(LocalForge {
            mirrors: Mirrors::new(dir, repos),
            identity: identity.clone(),
            // Git runs in the mirror, so it would read a relative path from
            // there, not from where the agent was started.
            remote: remote
                .canonicalize()
                .with_context(|| format!("forge.remote {} cannot be found", remote.display()))?,
        }),
    })
}

/// GitHub, as the App. The mirror fetches with a token narrowed to the one
/// repository and to read access, which goes in git's environment for that
/// fetch alone, never in its argv, which any local user can read. The clone's
/// commits are by the App's bot.
pub struct GithubForge {
    mirrors: Mirrors,
    minter: Arc<Minter>,
    /// Ends in `/`; a mirror fetches `<git_base><owner>/<name>.git`.
    git_base: String,
}

/// Where a mirror fetches `repo` from, and the git config key and value that
/// authenticate that fetch: the token as an `extraheader` for that base alone.
fn github_remote(
    git_base: &str,
    repo: &RepoName,
    token: &InstallToken,
) -> (String, String, Zeroizing<String>) {
    (
        format!("{git_base}{repo}.git"),
        format!("http.{git_base}.extraheader"),
        token.git_header(),
    )
}

impl Forge for GithubForge {
    fn prepare<'a>(
        &'a self,
        repo: &'a RepoName,
        dirs: TicketDirs<'a>,
    ) -> BoxFuture<'a, Result<Workspace>> {
        Box::pin(async move {
            // A finished clone needs nothing from GitHub, so a recovery does
            // not wait on a suspended installation.
            if let Some(done) = prepared(&dirs.host.join(REPO_JSON))? {
                return Ok(done);
            }
            let token = self.minter.token(repo, Access::Read).await?;
            let (name, email) = self.minter.bot().await?.commit_identity();
            let (url, key, header) = github_remote(&self.git_base, repo, &token);
            let remote = Remote {
                url: &url,
                auth: Some((&key, &header)),
            };
            let identity = Identity { name, email };
            self.mirrors.prepare(repo, dirs, &remote, &identity).await
        })
    }
}

/// Bare repositories on this disk, `<remote>/<owner>/<name>.git`, so the
/// whole path runs offline.
pub struct LocalForge {
    mirrors: Mirrors,
    identity: Identity,
    remote: PathBuf,
}

impl Forge for LocalForge {
    fn prepare<'a>(
        &'a self,
        repo: &'a RepoName,
        dirs: TicketDirs<'a>,
    ) -> BoxFuture<'a, Result<Workspace>> {
        Box::pin(async move {
            let bare = self
                .remote
                .join(repo.owner())
                .join(format!("{}.git", repo.name()));
            let url = bare
                .to_str()
                .with_context(|| format!("{} is not UTF-8", bare.display()))?;
            let remote = Remote { url, auth: None };
            self.mirrors
                .prepare(repo, dirs, &remote, &self.identity)
                .await
        })
    }
}

/// Where a mirror fetches from. `url` is also the clone's `origin`, so it
/// never holds a credential.
struct Remote<'a> {
    url: &'a str,
    /// A git config key and value for the fetch alone.
    auth: Option<(&'a str, &'a str)>,
}

struct Mirrors {
    dir: PathBuf,
    repos: BTreeMap<RepoName, (RepoConfig, Mutex<()>)>,
}

impl Mirrors {
    fn new(dir: PathBuf, repos: &BTreeMap<RepoName, RepoConfig>) -> Self {
        Self {
            dir,
            repos: repos
                .iter()
                .map(|(repo, terms)| (repo.clone(), (terms.clone(), Mutex::new(()))))
                .collect(),
        }
    }

    async fn prepare(
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
        let workspace = Workspace {
            repo: repo.clone(),
            base: terms.base.clone(),
            base_commit,
            notes: terms.notes.clone(),
        };
        // The marker says the clone is whole, so it must not reach the disk
        // before the clone does.
        sync_to_disk(dirs.work).await?;
        write_json(&marker, &workspace)?;
        Ok(workspace)
    }
}

/// What `repo.json` says, or `None` before the clone is done.
fn prepared(marker: &Path) -> Result<Option<Workspace>> {
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

fn git<'a>(args: impl IntoIterator<Item = &'a OsStr>) -> Command {
    let mut git = Command::new("git");
    git.args(args)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = std::env::var_os("PATH") {
        git.env("PATH", path);
    }
    git
}

fn fetch(mirror: &OsStr, remote: &Remote<'_>) -> Command {
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
    if let Some((key, value)) = remote.auth {
        fetch
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", key)
            .env("GIT_CONFIG_VALUE_0", value);
    }
    fetch
}

async fn run(mut git: Command, what: &str) -> Result<String> {
    let output = tokio::time::timeout(GIT_WAIT, git.output())
        .await
        .map_err(|_| anyhow!("git {what} took longer than {GIT_WAIT:?}"))?
        .with_context(|| format!("git {what} did not start"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let start = stderr
            .char_indices()
            .rev()
            .nth(STDERR_CHARS)
            .map_or(0, |(at, _)| at);
        bail!(
            "git {what} failed ({}): {}",
            output.status,
            &stderr[start..]
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::{LocalForge, Mirrors, REPO_JSON, fetch, github_remote, open};
    use crate::agent::config::{ForgeConfig, Identity, RepoConfig};
    use crate::agent::github::{Access, InstallToken};
    use crate::agent::ports::{Forge, TicketDirs, Workspace};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use solos_agent_core::change::{Deliverable, RepoName};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    fn widgets() -> RepoName {
        RepoName::try_from("acme/widgets".to_owned()).unwrap()
    }

    fn identity() -> Identity {
        Identity {
            name: "solos-bot".into(),
            email: "bot@example.com".into(),
        }
    }

    fn repos() -> BTreeMap<RepoName, RepoConfig> {
        BTreeMap::from([(
            widgets(),
            RepoConfig {
                about: "Widgets.".into(),
                base: "main".into(),
                budget_microusd: None,
                notes: Some("Tests run with ./check.".into()),
                owes: Deliverable::PullRequest,
            },
        )])
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Alice",
                "-c",
                "user.email=alice@example.com",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    struct World {
        root: PathBuf,
        forge: LocalForge,
        upstream: PathBuf,
    }

    impl World {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("forge-{name}-{}", uuid::Uuid::new_v4()));
            let upstream = root.join("upstream");
            std::fs::create_dir_all(&upstream).unwrap();
            git(&upstream, &["init", "--quiet", "--initial-branch=main"]);
            std::fs::write(upstream.join("README.md"), "Widgets\n").unwrap();
            git(&upstream, &["add", "README.md"]);
            git(&upstream, &["commit", "--quiet", "-m", "first"]);
            std::fs::write(upstream.join("README.md"), "Widgets, tested\n").unwrap();
            git(&upstream, &["commit", "--quiet", "-am", "second"]);
            git(&upstream, &["branch", "feature", "HEAD~1"]);
            let remote = root.join("remote");
            std::fs::create_dir_all(remote.join("acme")).unwrap();
            git(
                &root,
                &[
                    "clone",
                    "--quiet",
                    "--bare",
                    "upstream",
                    "remote/acme/widgets.git",
                ],
            );
            let forge = LocalForge {
                mirrors: Mirrors::new(root.join("forge"), &repos()),
                identity: identity(),
                remote,
            };
            Self {
                root,
                forge,
                upstream,
            }
        }

        fn ticket(&self, id: &str) -> (PathBuf, PathBuf) {
            let (work, host) = (
                self.root.join(id).join("work"),
                self.root.join(id).join("host"),
            );
            std::fs::create_dir_all(&work).unwrap();
            std::fs::create_dir_all(&host).unwrap();
            (work, host)
        }

        async fn prepare(&self, repo: &RepoName, id: &str) -> anyhow::Result<Workspace> {
            let (work, host) = self.ticket(id);
            let dirs = TicketDirs {
                work: &work,
                host: &host,
            };
            self.forge.prepare(repo, dirs).await
        }

        fn push(&self, file: &str) -> String {
            std::fs::write(self.upstream.join(file), file).unwrap();
            git(&self.upstream, &["add", file]);
            git(&self.upstream, &["commit", "--quiet", "-m", file]);
            let bare = self.root.join("remote/acme/widgets.git");
            git(
                &self.upstream,
                &["push", "--quiet", bare.to_str().unwrap(), "main"],
            );
            git(&self.upstream, &["rev-parse", "HEAD"])
        }
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn a_clone_holds_the_bases_history_and_the_agents_identity_on_its_own() {
        let w = World::new("clone");
        let tip = git(&w.upstream, &["rev-parse", "main"]);

        let workspace = w.prepare(&widgets(), "tk_1").await.unwrap();

        let work = w.root.join("tk_1/work");
        assert_eq!(
            workspace,
            Workspace {
                repo: widgets(),
                base: "main".into(),
                base_commit: tip.clone(),
                notes: Some("Tests run with ./check.".into()),
            }
        );
        let written = std::fs::read(w.root.join("tk_1/host").join(REPO_JSON)).unwrap();
        assert_eq!(
            serde_json::from_slice::<Workspace>(&written).unwrap(),
            workspace
        );
        assert_eq!(
            std::fs::read_to_string(work.join("README.md")).unwrap(),
            "Widgets, tested\n"
        );
        assert_eq!(
            git(&work, &["log", "--format=%s"]),
            "second\nfirst",
            "the clone keeps its history"
        );
        assert_eq!(git(&work, &["rev-parse", "HEAD"]), tip);
        assert_eq!(git(&work, &["config", "--local", "user.name"]), "solos-bot");
        assert_eq!(
            git(&work, &["config", "--local", "user.email"]),
            "bot@example.com"
        );
        assert_eq!(
            git(&work, &["remote", "get-url", "origin"]),
            w.root.join("remote/acme/widgets.git").to_str().unwrap()
        );
        assert!(
            git(&work, &["branch", "--remotes"]).contains("origin/feature"),
            "the forge's other branches come too"
        );
        assert!(work.join(".git").is_dir(), "a repository, not a worktree");
        assert!(
            !work.join(".git/objects/info/alternates").exists(),
            "its objects are its own"
        );
        assert!(
            std::fs::read_dir(work.join(".git/hooks"))
                .map_or(true, |mut hooks| hooks.next().is_none()),
            "no template, so no hooks"
        );
        assert!(
            git(&work, &["config", "--local", "--list"])
                .lines()
                .all(|line| !line.starts_with("core.hookspath")),
            "the hooks path is not left in the clone's config"
        );
    }

    #[tokio::test]
    async fn a_prepared_clone_is_left_as_the_worker_left_it_and_the_next_ticket_sees_the_new_tip() {
        let w = World::new("recover");
        let first = w.prepare(&widgets(), "tk_1").await.unwrap();
        let work = w.root.join("tk_1/work");
        std::fs::write(work.join("notes.txt"), "mine").unwrap();
        git(
            &work,
            &["commit", "--quiet", "--allow-empty", "-m", "the worker's"],
        );
        let worker_head = git(&work, &["rev-parse", "HEAD"]);
        let pushed = w.push("later.txt");

        let again = w.prepare(&widgets(), "tk_1").await.unwrap();

        assert_eq!(again, first, "repo.json answers a recovery");
        assert_eq!(git(&work, &["rev-parse", "HEAD"]), worker_head);
        assert_eq!(
            std::fs::read_to_string(work.join("notes.txt")).unwrap(),
            "mine"
        );
        let next = w.prepare(&widgets(), "tk_2").await.unwrap();
        assert_eq!(next.base_commit, pushed, "the mirror fetched the new tip");
        assert_eq!(
            git(&w.root.join("tk_2/work"), &["rev-parse", "HEAD"]),
            pushed
        );
    }

    #[tokio::test]
    async fn a_work_dir_without_its_marker_is_made_again() {
        let w = World::new("remake");
        let (work, _) = w.ticket("tk_1");
        std::fs::write(work.join("half-made"), "").unwrap();

        w.prepare(&widgets(), "tk_1").await.unwrap();

        assert!(!work.join("half-made").exists());
        assert_eq!(git(&work, &["log", "--format=%s"]), "second\nfirst");
    }

    #[tokio::test]
    async fn a_launch_that_waited_for_the_mirror_leaves_the_clone_another_launch_made() {
        let w = World::new("overlap");
        let (work, host) = w.ticket("tk_1");
        let guard = w.forge.mirrors.repos[&widgets()].1.lock().await;
        let repo = widgets();
        let mut late = Box::pin(w.prepare(&repo, "tk_1"));
        let waiting = tokio::time::timeout(Duration::from_millis(200), &mut late).await;
        assert!(waiting.is_err(), "the second launch waits for the first");

        // Meanwhile the first launch cloned, and the worker began.
        let made = Workspace {
            repo: widgets(),
            base: "main".into(),
            base_commit: "0123456789abcdef".into(),
            notes: None,
        };
        std::fs::write(host.join(REPO_JSON), serde_json::to_vec(&made).unwrap()).unwrap();
        std::fs::write(work.join("mine"), "the worker's").unwrap();
        drop(guard);

        assert_eq!(late.await.unwrap(), made);
        assert_eq!(
            std::fs::read_to_string(work.join("mine")).unwrap(),
            "the worker's"
        );
        assert!(!work.join(".git").exists(), "nothing cleared or cloned it");
    }

    #[tokio::test]
    async fn a_killed_fetchs_stale_files_do_not_stop_the_next_ticket() {
        let w = World::new("stale");
        w.prepare(&widgets(), "tk_1").await.unwrap();
        let pushed = w.push("later.txt");
        let mirror = w.root.join("forge/acme/widgets.git");
        let lock = mirror.join("refs/heads/main.lock");
        let pack = mirror.join("objects/pack/tmp_pack_Xa1b2c");
        std::fs::write(&lock, "").unwrap();
        std::fs::write(&pack, "half a pack").unwrap();
        let fetch_would_fail = std::process::Command::new("git")
            .arg("-C")
            .arg(&mirror)
            .args(["fetch", "--quiet", "--"])
            .arg(w.root.join("remote/acme/widgets.git"))
            .arg("+refs/heads/*:refs/heads/*")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(!fetch_would_fail.status.success(), "the lock is in the way");

        let next = w.prepare(&widgets(), "tk_2").await.unwrap();

        assert_eq!(next.base_commit, pushed);
        assert_eq!(
            git(&w.root.join("tk_2/work"), &["rev-parse", "HEAD"]),
            pushed
        );
        assert!(!lock.exists() && !pack.exists());
    }

    #[tokio::test]
    async fn a_mirror_that_clearing_does_not_mend_is_made_again() {
        let w = World::new("broken");
        w.prepare(&widgets(), "tk_1").await.unwrap();
        let pushed = w.push("later.txt");
        std::fs::write(w.root.join("forge/acme/widgets.git/config"), "[[[ not git").unwrap();

        let next = w.prepare(&widgets(), "tk_2").await.unwrap();

        assert_eq!(next.base_commit, pushed);
        assert_eq!(
            git(&w.root.join("tk_2/work"), &["log", "--format=%s", "-1"]),
            "later.txt"
        );
    }

    #[tokio::test]
    async fn a_mirror_that_cannot_be_made_again_is_an_error_not_a_loop() {
        let w = World::new("gone");
        std::fs::remove_dir_all(w.root.join("remote/acme/widgets.git")).unwrap();

        let err = w.prepare(&widgets(), "tk_1").await.unwrap_err();

        assert!(
            err.to_string().contains("was made again, and failed"),
            "{err:#}"
        );
        assert!(!w.root.join("tk_1/host").join(REPO_JSON).exists());
    }

    #[tokio::test]
    async fn a_relative_remote_is_read_from_where_the_agent_runs_not_from_the_mirror() {
        let w = World::new("relative");
        let tip = git(&w.upstream, &["rev-parse", "main"]);
        let here = std::env::current_dir().unwrap().canonicalize().unwrap();
        let mut relative = PathBuf::new();
        relative.extend(here.components().skip(1).map(|_| ".."));
        relative.push(w.root.join("remote").strip_prefix("/").unwrap());
        assert!(relative.is_relative());
        let config = |remote: PathBuf| ForgeConfig::Local {
            remote,
            identity: identity(),
            repos: repos(),
        };

        let forge = open(&config(relative), w.root.join("mirrors"), None).unwrap();

        let (work, host) = w.ticket("tk_1");
        let dirs = TicketDirs {
            work: &work,
            host: &host,
        };
        let made = forge.prepare(&widgets(), dirs).await.unwrap();
        assert_eq!(made.base_commit, tip);
        let origin = git(&work, &["remote", "get-url", "origin"]);
        assert_eq!(
            origin,
            w.root
                .canonicalize()
                .unwrap()
                .join("remote/acme/widgets.git")
                .to_str()
                .unwrap()
        );
        let missing = open(
            &config(PathBuf::from("no/such/remote")),
            w.root.join("x"),
            None,
        );
        let err = missing.err().unwrap();
        assert!(
            err.to_string()
                .starts_with("forge.remote no/such/remote cannot be found"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_repository_the_forge_lacks_or_a_missing_base_is_refused() {
        let w = World::new("refused");
        let gadgets = RepoName::try_from("acme/gadgets".to_owned()).unwrap();
        let err = w.prepare(&gadgets, "tk_1").await.unwrap_err();
        assert_eq!(err.to_string(), "the forge has no repository acme/gadgets");

        let mut terms = repos();
        terms.get_mut(&widgets()).unwrap().base = "trunk".into();
        let forge = LocalForge {
            mirrors: Mirrors::new(w.root.join("forge"), &terms),
            identity: identity(),
            remote: w.root.join("remote"),
        };
        let (work, host) = w.ticket("tk_2");
        let dirs = TicketDirs {
            work: &work,
            host: &host,
        };
        let err = forge.prepare(&widgets(), dirs).await.unwrap_err();
        assert_eq!(err.to_string(), "acme/widgets has no branch trunk");
        assert!(!host.join(REPO_JSON).exists());
    }

    #[test]
    fn githubs_token_reaches_git_in_its_environment_and_never_in_its_argv() {
        let canary = "ghs_canary0123456789";
        let token = InstallToken::new(
            canary,
            widgets(),
            Access::Read,
            SystemTime::now() + Duration::from_secs(3_600),
        );
        let (url, key, header) = github_remote("https://github.com/", &widgets(), &token);
        let remote = super::Remote {
            url: &url,
            auth: Some((&key, &header)),
        };

        let command = fetch("/forge/acme/widgets.git".as_ref(), &remote);

        let command = command.as_std();
        let argv: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(argv.contains(&"https://github.com/acme/widgets.git".to_owned()));
        assert!(!argv.iter().any(|arg| arg.contains("canary")), "{argv:?}");
        let env: BTreeMap<String, String> = command
            .get_envs()
            .filter_map(|(key, value)| {
                Some((key.to_str()?.to_owned(), value?.to_str()?.to_owned()))
            })
            .collect();
        let value = env["GIT_CONFIG_VALUE_0"].clone();
        let encoded = value.strip_prefix("Authorization: Basic ").unwrap();
        assert_eq!(
            STANDARD.decode(encoded).unwrap(),
            format!("x-access-token:{canary}").into_bytes()
        );
        assert_eq!(
            env["GIT_CONFIG_KEY_0"],
            "http.https://github.com/.extraheader"
        );
        assert_eq!(
            format!("{token:?}"),
            "InstallToken(acme/widgets Read, ..)",
            "a token's Debug never shows it"
        );
    }
}
