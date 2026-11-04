//! The host never runs git in a directory a sandbox has written: once
//! `host/repo.json` exists, `work/` is the worker's.

use super::config::{ForgeConfig, Identity, RepoConfig};
use super::github::Token;
use super::pi_executor::private_dir;
use super::ports::{BoxFuture, Forge, TicketDirs, Workspace};
use crate::broker::write_json;
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use solos_agent_core::change::RepoName;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::PathBuf;
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
const GIT_WAIT: Duration = Duration::from_secs(600);
const STDERR_CHARS: usize = 400;

/// The forge `config` names, with its mirrors under `dir`. A GitHub token
/// file is read and deleted here.
pub fn open(config: &ForgeConfig, dir: PathBuf) -> Result<Arc<dyn Forge>> {
    Ok(match config {
        ForgeConfig::Github {
            token_file,
            identity,
            repos,
        } => Arc::new(GithubForge {
            mirrors: Mirrors::new(dir, identity, repos),
            token: Token::take(token_file).context("forge.token_file")?,
        }),
        ForgeConfig::Local {
            remote,
            identity,
            repos,
        } => Arc::new(LocalForge {
            mirrors: Mirrors::new(dir, identity, repos),
            remote: remote.clone(),
        }),
    })
}

/// GitHub over https. The token goes in git's environment for the mirror's
/// fetch alone, never in its argv, which any local user can read.
pub struct GithubForge {
    mirrors: Mirrors,
    token: Token,
}

impl GithubForge {
    fn remote(&self, repo: &RepoName) -> (String, Zeroizing<String>) {
        let basic = Zeroizing::new(format!("x-access-token:{}", self.token.secret()));
        let header = format!("Authorization: Basic {}", STANDARD.encode(basic.as_bytes()));
        (
            format!("https://github.com/{repo}.git"),
            Zeroizing::new(header),
        )
    }
}

impl Forge for GithubForge {
    fn prepare<'a>(
        &'a self,
        repo: &'a RepoName,
        dirs: TicketDirs<'a>,
    ) -> BoxFuture<'a, Result<Workspace>> {
        Box::pin(async move {
            let (url, header) = self.remote(repo);
            let remote = Remote {
                url: &url,
                auth: Some(("http.https://github.com/.extraheader", &header)),
            };
            self.mirrors.prepare(repo, dirs, &remote).await
        })
    }
}

/// Bare repositories on this disk, `<remote>/<owner>/<name>.git`, so the
/// whole path runs offline.
pub struct LocalForge {
    mirrors: Mirrors,
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
            self.mirrors.prepare(repo, dirs, &remote).await
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
    identity: Identity,
    repos: BTreeMap<RepoName, (RepoConfig, Mutex<()>)>,
}

impl Mirrors {
    fn new(dir: PathBuf, identity: &Identity, repos: &BTreeMap<RepoName, RepoConfig>) -> Self {
        Self {
            dir,
            identity: identity.clone(),
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
    ) -> Result<Workspace> {
        let marker = dirs.host.join(REPO_JSON);
        match std::fs::read(&marker) {
            Ok(bytes) => {
                return serde_json::from_slice(&bytes)
                    .with_context(|| format!("{} is not what the forge wrote", marker.display()));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", marker.display()));
            }
        }
        let (terms, lock) = self
            .repos
            .get(repo)
            .with_context(|| format!("the forge has no repository {repo}"))?;
        let _one_ticket_at_a_time = lock.lock().await;
        let owner = self.dir.join(repo.owner());
        private_dir(&owner)?;
        let mirror = owner.join(format!("{}.git", repo.name()));
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
        let name = format!("user.name={}", self.identity.name);
        let email = format!("user.email={}", self.identity.email);
        run(
            git([
                "clone".as_ref(),
                "--quiet".as_ref(),
                "--no-local".as_ref(),
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
        write_json(&marker, &workspace)?;
        Ok(workspace)
    }
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
    use super::{GithubForge, LocalForge, Mirrors, REPO_JSON, fetch};
    use crate::agent::config::{Identity, RepoConfig};
    use crate::agent::github::Token;
    use crate::agent::ports::{Forge, TicketDirs, Workspace};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use solos_agent_core::change::{Deliverable, RepoName};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

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
                mirrors: Mirrors::new(root.join("forge"), &identity(), &repos()),
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
    async fn a_repository_the_forge_lacks_or_a_missing_base_is_refused() {
        let w = World::new("refused");
        let gadgets = RepoName::try_from("acme/gadgets".to_owned()).unwrap();
        let err = w.prepare(&gadgets, "tk_1").await.unwrap_err();
        assert_eq!(err.to_string(), "the forge has no repository acme/gadgets");

        let mut terms = repos();
        terms.get_mut(&widgets()).unwrap().base = "trunk".into();
        let forge = LocalForge {
            mirrors: Mirrors::new(w.root.join("forge"), &identity(), &terms),
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
        let canary = "github_pat_canary0123456789";
        let forge = GithubForge {
            mirrors: Mirrors::new(PathBuf::from("/forge"), &identity(), &repos()),
            token: Token::new(canary),
        };
        let (url, header) = forge.remote(&widgets());
        let remote = super::Remote {
            url: &url,
            auth: Some(("http.https://github.com/.extraheader", &header)),
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
    }
}
