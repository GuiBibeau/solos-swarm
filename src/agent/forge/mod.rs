//! The host never runs git in a directory a sandbox has written: once
//! `host/repo.json` exists, `work/` is the worker's. What a worker commits
//! reaches the forge as a bundle made in a fresh sandbox, judged and pushed
//! from the host's own mirror (see [`mirror`]).

mod deps;
mod evidence;
mod github;
mod local;
mod mirror;
mod paths;

pub use deps::{MANIFEST as DEPS_MANIFEST, SCRATCH_PREFIX as DEPS_SCRATCH_PREFIX};
pub use evidence::check as check_evidence;
pub use github::{GithubForge, body, closes, ticket_marker, title};
pub use local::LocalForge;
pub(crate) use mirror::GIT_WAIT;
pub use mirror::{MAX_CHANGE_FILES, REPO_JSON};
pub use paths::compiles as valid_glob;

use super::config::ForgeConfig;
use super::github::GithubApp;
use super::ports::Forge;
use anyhow::{Context, Result};
use mirror::Mirrors;
use std::path::PathBuf;
use std::sync::Arc;

/// `<state>/forge`, the mirrors.
pub const FORGE_DIR: &str = "forge";
/// `<state>/forge-stage`, beside the mirrors, not in them: a mirror that is
/// made again must not lose the bundle a queued Publish will push.
pub const STAGE_DIR: &str = "forge-stage";
/// `<state>/deps`, the repositories' installed dependencies, which every
/// Ticket's `run_code` mounts read-only (see [`deps`]).
pub const DEPS_DIR: &str = "deps";

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
                api: github.api.clone(),
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

#[cfg(test)]
mod tests {
    use super::github::github_remote;
    use super::mirror::{Mirrors, REPO_JSON, Remote, fetch};
    use super::{LocalForge, open};
    use crate::agent::config::{
        DepsConfig, DepsKind, EvidenceConfig, ForgeConfig, Identity, RepoConfig,
    };
    use crate::agent::github::{Access, InstallToken};
    use crate::agent::ports::{Dependencies, Forge, TicketDirs, Workspace};
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
                ..RepoConfig::default()
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
            Self::with(name, |_, _| {})
        }

        /// A world whose repositories `edit` has changed; it is given the
        /// world's directory, for paths in the terms.
        fn with(name: &str, edit: impl FnOnce(&Path, &mut BTreeMap<RepoName, RepoConfig>)) -> Self {
            let root = std::env::temp_dir().join(format!("forge-{name}-{}", uuid::Uuid::new_v4()));
            let mut terms = repos();
            edit(&root, &mut terms);
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
                mirrors: Mirrors::new(root.join("forge"), &terms),
                identity: identity(),
                remote,
            };
            Self {
                root,
                forge,
                upstream,
            }
        }

        /// Commit `files` to the upstream's main and push them to the remote.
        fn push_files(&self, files: &[(&str, &str)]) -> String {
            for (path, text) in files {
                let at = self.upstream.join(path);
                std::fs::create_dir_all(at.parent().unwrap()).unwrap();
                std::fs::write(at, text).unwrap();
            }
            git(&self.upstream, &["add", "-A"]);
            git(&self.upstream, &["commit", "--quiet", "-m", "files"]);
            let bare = self.root.join("remote/acme/widgets.git");
            git(
                &self.upstream,
                &["push", "--quiet", bare.to_str().unwrap(), "main"],
            );
            git(&self.upstream, &["rev-parse", "HEAD"])
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
                evidence: None,
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
        let guard = w.forge.mirrors.lock_of(&widgets()).lock().await;
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
            evidence: None,
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

    /// A package manager stand-in: it leaves bun's layout and writes down
    /// the lockfile it was given, which only the base commit's files can be.
    const FAKE_BUN: &str = r#"#!/bin/sh
set -eu
mkdir -p node_modules/.bun/x@1/node_modules/x node_modules/.facts
echo 1 > node_modules/.bun/x@1/node_modules/x/index.js
ln -s .bun/x@1/node_modules/x node_modules/x
echo "lock: $(cat bun.lock)" > node_modules/.facts/seen.txt
"#;

    const PACKAGE: &str = r#"{"name":"acme","workspaces":["apps/*"]}"#;

    fn with_deps(root: &Path, terms: &mut BTreeMap<RepoName, RepoConfig>) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(root).unwrap();
        let bun = root.join("bun");
        std::fs::write(&bun, FAKE_BUN).unwrap();
        std::fs::set_permissions(&bun, std::fs::Permissions::from_mode(0o755)).unwrap();
        terms.get_mut(&widgets()).unwrap().deps = Some(DepsConfig {
            kind: DepsKind::Bun,
            binary: Some(bun),
            timeout_secs: 60,
            max_bytes: 1 << 30,
            memory_bytes: 16 << 30,
        });
    }

    fn sandbox() -> bool {
        let found = cfg!(target_os = "linux")
            && Path::new("/usr/bin/bwrap").is_file()
            && Path::new("/usr/bin/prlimit").is_file();
        if !found {
            eprintln!("skipped: a build needs Linux, /usr/bin/bwrap and /usr/bin/prlimit");
        }
        found
    }

    fn seen_in(w: &World, key: &str) -> String {
        let path = w
            .root
            .join("deps/acme/widgets")
            .join(key)
            .join("node_modules/.facts/seen.txt");
        std::fs::read_to_string(path).unwrap().trim().to_owned()
    }

    fn ready(found: &Dependencies) -> &str {
        match found {
            Dependencies::Ready { key, .. } => key,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn dependencies_are_built_from_the_base_commit_in_the_mirror_and_never_from_the_workers_tree()
     {
        if !sandbox() {
            return;
        }
        let w = World::with("deps", with_deps);
        w.push_files(&[
            ("package.json", PACKAGE),
            ("bun.lock", "lock one"),
            ("apps/web/package.json", r#"{"name":"web"}"#),
        ]);
        let first = w.prepare(&widgets(), "tk_1").await.unwrap();

        let found = w.forge.dependencies(&first).await;

        let key = ready(&found).to_owned();
        assert_eq!(seen_in(&w, &key), "lock: lock one");
        let work = w.root.join("tk_1/work");
        let exclude = std::fs::read_to_string(work.join(".git/info/exclude")).unwrap();
        assert!(
            exclude.lines().any(|line| line == "node_modules/")
                && exclude.lines().any(|line| line == "/.bun/"),
            "the clone keeps what the cache puts there out of its commits: {exclude}"
        );

        // The worker changes the lockfile in its clone and commits it on a branch.
        std::fs::write(work.join("bun.lock"), "lock BY THE WORKER").unwrap();
        std::fs::write(work.join("package.json"), r#"{"name":"mine"}"#).unwrap();
        git(&work, &["checkout", "--quiet", "-b", "worker"]);
        git(&work, &["commit", "--quiet", "-am", "the worker's"]);
        assert_eq!(
            w.forge.dependencies(&first).await,
            found,
            "the same base commit is the same cache"
        );
        // Whatever the clone says, a build reads the mirror: take the cache away.
        std::fs::remove_dir_all(w.root.join("deps")).unwrap();
        let again = w.forge.dependencies(&first).await;
        assert_eq!(ready(&again), key);
        assert_eq!(
            seen_in(&w, &key),
            "lock: lock one",
            "not the worker's lockfile"
        );

        // A new lockfile on the base is a new cache for the next Ticket.
        let later = w.push_files(&[("bun.lock", "lock two")]);
        let second = w.prepare(&widgets(), "tk_2").await.unwrap();
        assert_eq!(second.base_commit, later);
        let newer = w.forge.dependencies(&second).await;
        assert_ne!(ready(&newer), key, "the key changes when the lockfile does");
        assert_eq!(seen_in(&w, ready(&newer)), "lock: lock two");
        assert_eq!(
            seen_in(&w, &key),
            "lock: lock one",
            "the first Ticket's cache is not touched"
        );
    }

    #[tokio::test]
    async fn a_repository_that_cannot_be_installed_is_unavailable_with_the_reason_and_still_clones()
    {
        let w = World::with("nolock", with_deps);
        w.push_files(&[("package.json", PACKAGE)]);

        let workspace = w.prepare(&widgets(), "tk_1").await.unwrap();
        let found = w.forge.dependencies(&workspace).await;

        let Dependencies::Unavailable { why } = found else {
            panic!("{found:?}")
        };
        assert!(why.contains("no bun.lock at its root"), "{why}");
        assert!(
            w.root.join("tk_1/work/package.json").is_file(),
            "the Ticket has its clone"
        );
        assert!(!w.root.join("deps").exists(), "nothing was built");
    }

    /// A toolchain stand-in: its cargo vendors one crate and writes down the
    /// lockfile it was given.
    const FAKE_CARGO: &str = r#"#!/bin/sh
set -eu
mkdir -p /s/vendor/x-1.0.0
echo "lock: $(cat Cargo.lock)" > /s/vendor/x-1.0.0/seen.txt
"#;

    fn with_cargo(root: &Path, terms: &mut BTreeMap<RepoName, RepoConfig>) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(root.join("toolchain/bin")).unwrap();
        std::fs::create_dir_all(root.join("toolchain/lib/rustlib")).unwrap();
        std::fs::write(root.join("toolchain/bin/rustc"), "").unwrap();
        let cargo = root.join("toolchain/bin/cargo");
        std::fs::write(&cargo, FAKE_CARGO).unwrap();
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        terms.get_mut(&widgets()).unwrap().deps = Some(DepsConfig {
            kind: DepsKind::Cargo,
            binary: Some(cargo),
            timeout_secs: 60,
            max_bytes: 1 << 30,
            memory_bytes: 16 << 30,
        });
    }

    #[tokio::test]
    async fn crates_are_vendored_from_the_base_commits_lockfile_and_the_clone_ignores_target() {
        if !sandbox() {
            return;
        }
        let w = World::with("cargo", with_cargo);
        w.push_files(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"core\"]\n"),
            ("Cargo.lock", "lock one"),
            ("core/Cargo.toml", "[package]\nname = \"core\"\n"),
        ]);
        let first = w.prepare(&widgets(), "tk_1").await.unwrap();

        let found = w.forge.dependencies(&first).await;

        let Dependencies::Ready { key, kind, extras } = &found else {
            panic!("{found:?}")
        };
        assert_eq!(*kind, DepsKind::Cargo);
        let cache = w.root.join("deps/acme/widgets").join(key);
        let seen = std::fs::read_to_string(cache.join("vendor/x-1.0.0/seen.txt")).unwrap();
        assert_eq!(seen.trim(), "lock: lock one");
        assert!(
            extras.binds.iter().any(|bind| matches!(
                bind,
                crate::code::Bind::ReadOnly { source, target }
                    if *source == cache.join("vendor") && target == Path::new("/runtime/cargo/vendor")
            )),
            "{extras:?}"
        );
        let work = w.root.join("tk_1/work");
        let exclude = std::fs::read_to_string(work.join(".git/info/exclude")).unwrap();
        assert!(
            exclude.lines().any(|line| line == "/target/")
                && !exclude.lines().any(|line| line == "node_modules/"),
            "the build output stays out of the clone's commits, and a cargo repository is not \
             given bun's exclusions: {exclude}"
        );

        // The worker changes the lockfile in its clone and commits it on a branch.
        std::fs::write(work.join("Cargo.lock"), "lock BY THE WORKER").unwrap();
        git(&work, &["checkout", "--quiet", "-b", "worker"]);
        git(&work, &["commit", "--quiet", "-am", "the worker's"]);
        std::fs::remove_dir_all(w.root.join("deps")).unwrap();
        let again = w.forge.dependencies(&first).await;
        assert_eq!(again, found, "the same base commit, the same key");
        assert_eq!(
            std::fs::read_to_string(cache.join("vendor/x-1.0.0/seen.txt"))
                .unwrap()
                .trim(),
            "lock: lock one",
            "a build reads the mirror, not the worker's lockfile"
        );
    }

    #[tokio::test]
    async fn a_cargo_repository_with_a_git_dependency_is_unavailable_in_words_and_still_clones() {
        let w = World::with("cargo-git", with_cargo);
        w.push_files(&[
            ("Cargo.toml", "[package]\nname = \"x\"\n"),
            (
                "Cargo.lock",
                "[[package]]\nname = \"y\"\nsource = \"git+https://github.com/a/y?rev=abc#def\"\n",
            ),
        ]);

        let workspace = w.prepare(&widgets(), "tk_1").await.unwrap();
        let found = w.forge.dependencies(&workspace).await;

        let Dependencies::Unavailable { why } = found else {
            panic!("{found:?}")
        };
        assert!(
            why.contains("github.com/a/y?rev=abc") && why.contains("crates.io only"),
            "{why}"
        );
        assert!(
            w.root.join("tk_1/work/Cargo.toml").is_file(),
            "the Ticket has its clone"
        );
        assert!(!w.root.join("deps").exists(), "nothing was built");
    }

    #[tokio::test]
    async fn a_repository_that_asks_for_no_dependencies_gets_none_and_a_clone_as_before() {
        let w = World::new("nodeps");
        let workspace = w.prepare(&widgets(), "tk_1").await.unwrap();

        assert_eq!(w.forge.dependencies(&workspace).await, Dependencies::None);

        assert!(!w.root.join("tk_1/work/.git/info/exclude").exists());
        let written = std::fs::read_to_string(w.root.join("tk_1/host").join(REPO_JSON)).unwrap();
        assert!(!written.contains("evidence"), "{written}");
    }

    #[tokio::test]
    async fn what_a_repositorys_pull_requests_must_carry_is_fixed_in_the_clone_with_repo_json() {
        let evidence = EvidenceConfig {
            command: "./verify --json".into(),
            heading: "## Evidence".into(),
        };
        let w = World::with("evidence", |_, terms| {
            terms.get_mut(&widgets()).unwrap().evidence = Some(evidence.clone());
        });

        let workspace = w.prepare(&widgets(), "tk_1").await.unwrap();

        assert_eq!(workspace.evidence, Some(evidence.clone()));
        let written = std::fs::read(w.root.join("tk_1/host").join(REPO_JSON)).unwrap();
        let read: Workspace = serde_json::from_slice(&written).unwrap();
        assert_eq!(
            read.evidence,
            Some(evidence),
            "a recovery gets the terms of the clone"
        );
        assert_eq!(
            w.prepare(&widgets(), "tk_1").await.unwrap(),
            workspace,
            "and so does a second launch"
        );
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
        let remote = Remote {
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
