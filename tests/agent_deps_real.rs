//! The real thing, by hand: a real package manager installs a real
//! repository's dependencies in the host's build sandbox, and a Ticket's
//! `run_code` sandbox, offline, runs the repository's own command on them.
//! It needs the network and a few minutes, so it is ignored; run it with
//!
//! ```text
//! SOLOS_DEPS_REMOTE=/dir/holding/owner SOLOS_DEPS_REPO=owner/name \
//!   cargo test --test agent_deps_real -- --ignored --nocapture
//! ```
//!
//! where `owner/name.git` is a bare repository under `SOLOS_DEPS_REMOTE`.
//! `SOLOS_DEPS_KIND` is `bun` (the default) or `cargo`. `SOLOS_DEPS_BINARY`
//! names the package manager: bun (by default `$HOME/.bun/bin/bun`, and
//! `SOLOS_DEPS_BUN` is the older name for it) or, for cargo, the `cargo` of a
//! toolchain (no default). `SOLOS_DEPS_COMMAND` is the command: by default
//! `bun run solos dev verify --scope unit --json`, or for cargo
//! `cargo test -p solos-agent-core --offline`.

use serde_json::{Value, json};
use solos_agent_core::change::RepoName;
use solos_swarm::agent::config::{DepsConfig, DepsKind, ForgeConfig, Identity, RepoConfig};
use solos_swarm::agent::forge;
use solos_swarm::agent::ports::{Dependencies, TicketDirs};
use solos_swarm::code::{CodeConfig, CodeExecutor, Ended};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::test]
#[ignore = "needs bun or a Rust toolchain, bwrap, prlimit, the network and a bare repository (see the module docs)"]
async fn a_real_install_of_a_real_repository_runs_its_command_offline_in_a_ticket_sandbox() {
    let remote = PathBuf::from(std::env::var("SOLOS_DEPS_REMOTE").expect("SOLOS_DEPS_REMOTE"));
    let repo =
        RepoName::try_from(std::env::var("SOLOS_DEPS_REPO").expect("SOLOS_DEPS_REPO")).unwrap();
    let kind = match std::env::var("SOLOS_DEPS_KIND").as_deref() {
        Ok("cargo") => DepsKind::Cargo,
        Ok("bun") | Err(_) => DepsKind::Bun,
        Ok(other) => panic!("SOLOS_DEPS_KIND is bun or cargo, not {other}"),
    };
    let command = std::env::var("SOLOS_DEPS_COMMAND").unwrap_or_else(|_| {
        match kind {
            DepsKind::Bun => "bun run solos dev verify --scope unit --json",
            DepsKind::Cargo => "cargo test -p solos-agent-core --offline",
        }
        .into()
    });
    let binary = std::env::var_os("SOLOS_DEPS_BINARY")
        .or_else(|| std::env::var_os("SOLOS_DEPS_BUN"))
        .map(PathBuf::from);
    // On disk, not in /tmp: a repository's node_modules is hundreds of
    // thousands of files, and /tmp is often a tmpfs with an inode limit.
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("agent-deps-real-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let _gone = Cleanup(root.clone());
    let terms = RepoConfig {
        about: "A real repository.".into(),
        base: "main".into(),
        deps: Some(DepsConfig {
            kind,
            binary,
            timeout_secs: 900,
            max_bytes: 16 << 30,
            memory_bytes: 16 << 30,
        }),
        ..RepoConfig::default()
    };
    let config = ForgeConfig::Local {
        remote,
        identity: Identity {
            name: "solos-bot".into(),
            email: "bot@example.com".into(),
        },
        repos: BTreeMap::from([(repo.clone(), terms)]),
    };
    let forge = forge::open(&config, root.join("forge"), None).unwrap();
    let tickets = root.join("tickets");
    let (work, host) = (tickets.join("tk_1/work"), tickets.join("tk_1/host"));
    std::fs::create_dir_all(&host).unwrap();
    let workspace = forge
        .prepare(
            &repo,
            TicketDirs {
                work: &work,
                host: &host,
            },
        )
        .await
        .unwrap();

    let started = Instant::now();
    let found = forge.dependencies(&workspace).await;
    let built = started.elapsed();
    let Dependencies::Ready { key, extras, .. } = found else {
        panic!("the dependencies are not ready: {found:?}")
    };
    let again = Instant::now();
    assert!(matches!(
        forge.dependencies(&workspace).await,
        Dependencies::Ready { .. }
    ));
    eprintln!(
        "first request {built:?} (a build), second {:?} (the cache), key {key}",
        again.elapsed()
    );
    let cache = root
        .join("deps")
        .join(repo.owner())
        .join(repo.name())
        .join(&key);
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(cache.join("manifest.json")).unwrap()).unwrap();
    eprintln!(
        "cache: {} bytes, {} entries, {} directories, {} mounts",
        manifest["bytes"],
        entries(&cache),
        manifest["dirs"].as_array().unwrap().len(),
        extras.binds.len()
    );

    // The Ticket's sandbox, as `agent serve` builds it: the state's tickets
    // directory is the scratch root and nothing else is mounted, with the
    // usual 4 GiB cap, which the cache's extras raise for this Ticket alone.
    let mut code = CodeConfig::new(tickets.clone(), Vec::new());
    code.concurrency = 1;
    let executor = Arc::new(CodeExecutor::from_config(code).unwrap());
    let run = executor
        .start_in_with(
            &work,
            &extras,
            &json!({"code": command, "timeout_seconds": 300}),
        )
        .unwrap();
    let Some(Ended::Ran { receipt, .. }) = run.wait(Duration::from_secs(600)).await else {
        panic!("the command did not end")
    };
    let stdout = receipt["stdout"].as_str().unwrap_or_default();
    eprintln!(
        "exit {} in {} ms\n--- stdout\n{stdout}\n--- stderr (tail)\n{}",
        receipt["exit_code"],
        receipt["duration_ms"],
        receipt["stderr"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .rev()
            .take(1500)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>()
    );
    assert_eq!(receipt["exit_code"], 0, "{receipt}");
    if let Ok(Value::Object(evidence)) = serde_json::from_str::<Value>(stdout) {
        assert_eq!(evidence.get("ok"), Some(&json!(true)), "{evidence:?}");
        assert_eq!(evidence.get("dirty"), Some(&json!(false)), "{evidence:?}");
    }
}

/// Files, links and directories under `path`: what a cache costs in inodes.
fn entries(path: &std::path::Path) -> u64 {
    let Ok(read) = std::fs::read_dir(path) else {
        return 0;
    };
    read.flatten()
        .map(|entry| {
            1 + if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                entries(&entry.path())
            } else {
                0
            }
        })
        .sum()
}

/// Remove the test's directory however it ends.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
