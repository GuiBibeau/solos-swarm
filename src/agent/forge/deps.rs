//! The dependency cache of a repository (`forge.repos.<repo>.deps`).
//!
//! A worker's sandbox has no network and its clone has no `node_modules`, so
//! the repository's own scripts cannot run. The host installs the dependencies
//! once per lockfile and every Ticket's `run_code` mounts them read-only:
//!
//! - The inputs are the files of the **base commit in the host's mirror**: the
//!   lockfile, the root `package.json`, the workspaces' `package.json` files and
//!   the files `patchedDependencies` names. Never a worker's branch, never `/work`.
//!   `bunfig.toml` and `.npmrc` are not inputs: the install reads no registry,
//!   scope or token setting the repository chose, and fetches from the default
//!   npm registry only. A text lockfile that names any other host is refused.
//! - The key is the SHA-256 of those files, so a lockfile or manifest change at
//!   the base is a new cache and an unrelated commit is the same one.
//! - The build is `bun install --frozen-lockfile --ignore-scripts` in a bwrap
//!   sandbox of its own: the host's network, and nothing else of the host's (no
//!   home, no token, no vault, no agent state: a scratch directory, the package
//!   manager read-only and the system's `/usr`). No lifecycle script runs, so no
//!   code of the repository runs. It has a time limit and a disk cap.
//! - The result is written whole in a scratch directory and renamed to
//!   `<state>/deps/<owner>/<name>/<key>`, so a directory with a `manifest.json`
//!   is a complete one. A crash leaves a `tmp-*` directory the next build and
//!   `agent prune` remove.
//!
//! What a Ticket mounts (see [`extras`]): bun lays out workspaces with
//! links, `node_modules/.bun` holding the packages and each workspace's own
//! `node_modules` holding only relative links into it. The big directories are
//! mounted read-only. A directory of links only is made again in each sandbox
//! as a fresh tmpfs, because tools write into it (Vite makes
//! `node_modules/.vite-temp`), and what they write is gone with the command. The
//! shared cache is never writable, so no Ticket can plant a file for another.

use super::mirror::{git_in, run_bytes};
use super::paths;
use crate::agent::config::{DepsConfig, DepsKind};
use crate::agent::pi_executor::private_dir;
use crate::agent::ports::Dependencies;
use crate::bwrap::Bwrap;
use crate::code::{Bind, Extras, Link};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use solos_agent_core::change::RepoName;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

/// Complete caches have this file; it is written last, inside the scratch
/// directory that is then renamed into place.
pub const MANIFEST: &str = "manifest.json";
/// Scratch directories of builds, beside the caches they become.
pub const SCRATCH_PREFIX: &str = "tmp-";
/// Bumped when what a key or a layout means changes, so old caches are not
/// taken for new ones.
const VERSION: u32 = 1;

/// The most files an install reads, and the most bytes of one and of all.
const MAX_FILES: usize = 4_096;
const MAX_FILE_BYTES: u64 = 16 << 20;
const MAX_INPUT_BYTES: u64 = 64 << 20;
/// The most links one directory may be made again from, and how deep they
/// sit (`@scope/name` is one deep).
const MAX_LINKS: usize = 4_096;
const MAX_LINK_DEPTH: usize = 3;
/// The most `node_modules` directories one repository may have.
const MAX_DIRS: usize = 256;
/// How deep below the repository's root a `node_modules` may sit.
const MAX_FIND_DEPTH: usize = 8;

/// The only registry a build fetches from.
const REGISTRY: &str = "https://registry.npmjs.org/";
const REGISTRY_HOST: &str = "registry.npmjs.org";

/// How often a running build is looked at, for its clock and its disk use.
const POLL: Duration = Duration::from_secs(2);
/// A build that failed is not tried again for the same key this soon, so a
/// network that is down does not hold up every Ticket for the build's limit.
const FAILED_FOR: Duration = Duration::from_secs(300);
/// Characters of a one-line reason.
const WHY_CHARS: usize = 400;
const LOG_TAIL_CHARS: usize = 500;

/// Where the build sandbox keeps its scratch directory.
const SCRATCH: &str = "/s";
const TMPFS_BYTES: u64 = 512 << 20;
/// The address space a build may use: bun reserves a good deal of it, and
/// this stops only a runaway.
const BUILD_ADDRESS_SPACE: u64 = 16 << 30;
const BUILD_FILE_BYTES: u64 = 2 << 30;
const BUILD_OPEN_FILES: u64 = 8_192;
const ETC_BINDS: [&str; 6] = [
    "/etc/ssl",
    "/etc/ca-certificates",
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/localtime",
];

/// Where a Ticket's `run_code` finds the package manager (`bun`, and as
/// `node` too: bun runs what a `#!/usr/bin/env node` script is, which the
/// host's own node may be too old for) and its throwaway home. A tool that
/// writes a cache under `$HOME` would otherwise dirty the clone (`/work`),
/// which the repository's own evidence check calls a dirty tree.
const TOOLS: &str = "/runtime/bun";
const HOME: &str = "/tmp/home";

/// The files of the base commit an install reads, by path.
#[derive(Debug, Default)]
pub(super) struct Inputs {
    files: BTreeMap<String, Vec<u8>>,
}

/// What `git ls-tree` says of one regular file.
struct Blob {
    sha: String,
    size: u64,
}

/// The repository's files at a commit, regular files only: a link or a
/// submodule is never read.
type Tree = BTreeMap<String, Blob>;

fn parse_tree(listed: &[u8]) -> Result<Tree> {
    let mut tree = Tree::new();
    for record in listed.split(|byte| *byte == 0).filter(|r| !r.is_empty()) {
        let record = std::str::from_utf8(record).unwrap_or_default();
        let Some((meta, path)) = record.split_once('\t') else {
            continue;
        };
        let mut fields = meta.split_whitespace();
        let (Some(mode), Some(kind), Some(sha), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if kind != "blob" || !matches!(mode, "100644" | "100755") {
            continue;
        }
        let size = size
            .parse()
            .context("git ls-tree gave an unreadable size")?;
        tree.insert(
            path.to_owned(),
            Blob {
                sha: sha.to_owned(),
                size,
            },
        );
    }
    Ok(tree)
}

/// Read the files an install of `kind` needs from `mirror` at `commit`.
pub(super) async fn read_inputs(mirror: &Path, commit: &str, kind: DepsKind) -> Result<Inputs> {
    ensure!(
        (7..=64).contains(&commit.len()) && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "the base commit {commit:?} is not a commit id"
    );
    let listed = run_bytes(
        git_in(mirror, ["ls-tree", "-r", "-l", "-z", "--full-tree", commit]),
        "ls-tree",
        None,
    )
    .await?;
    let tree = parse_tree(&listed)?;
    let root = tree
        .get("package.json")
        .with_context(|| format!("the repository has no package.json at {commit}"))?;
    let root = fetch(mirror, &[("package.json", root)]).await?;
    let manifest: Value =
        serde_json::from_slice(&root[0]).context("the root package.json is not JSON")?;
    let paths = select(kind, &tree, &manifest)?;
    let mut total = 0;
    for path in &paths {
        let size = tree[path].size;
        ensure!(
            size <= MAX_FILE_BYTES,
            "{path} is {size} bytes, over the {MAX_FILE_BYTES} an install reads"
        );
        total += size;
    }
    ensure!(
        total <= MAX_INPUT_BYTES,
        "an install would read {total} bytes of manifests and lockfiles, over {MAX_INPUT_BYTES}"
    );
    let wanted: Vec<(&str, &Blob)> = paths
        .iter()
        .map(|path| (path.as_str(), &tree[path]))
        .collect();
    let contents = fetch(mirror, &wanted).await?;
    let files: BTreeMap<String, Vec<u8>> = paths.into_iter().zip(contents).collect();
    for (path, bytes) in &files {
        if path == "bun.lock" {
            registry_only(std::str::from_utf8(bytes).context("bun.lock is not UTF-8")?)?;
        }
    }
    Ok(Inputs { files })
}

/// The blobs, in order, from one `git cat-file --batch`.
async fn fetch(mirror: &Path, blobs: &[(&str, &Blob)]) -> Result<Vec<Vec<u8>>> {
    let mut stdin = Vec::new();
    for (_, blob) in blobs {
        stdin.extend_from_slice(blob.sha.as_bytes());
        stdin.push(b'\n');
    }
    let out = run_bytes(
        git_in(mirror, ["cat-file", "--batch"]),
        "cat-file",
        Some(stdin),
    )
    .await?;
    let mut at = 0;
    let mut found = Vec::with_capacity(blobs.len());
    for (path, blob) in blobs {
        let end = out[at..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|n| at + n)
            .with_context(|| format!("git cat-file gave no header for {path}"))?;
        let header = std::str::from_utf8(&out[at..end]).unwrap_or_default();
        let size: usize = match header.split(' ').collect::<Vec<_>>()[..] {
            [sha, "blob", size] if sha == blob.sha => size.parse().unwrap_or(usize::MAX),
            _ => bail!("git cat-file did not find {path}: {header}"),
        };
        let body = end + 1;
        ensure!(
            body.checked_add(size).is_some_and(|stop| stop < out.len()),
            "git cat-file cut {path} short"
        );
        found.push(out[body..body + size].to_vec());
        at = body + size + 1;
    }
    Ok(found)
}

/// Which files of `tree` an install of `kind` reads.
fn select(kind: DepsKind, tree: &Tree, manifest: &Value) -> Result<BTreeSet<String>> {
    match kind {
        DepsKind::Bun => select_bun(tree, manifest),
    }
}

fn select_bun(tree: &Tree, manifest: &Value) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::from(["package.json".to_owned()]);
    let locks: Vec<&str> = ["bun.lock", "bun.lockb"]
        .into_iter()
        .filter(|lock| tree.contains_key(*lock))
        .collect();
    ensure!(
        !locks.is_empty(),
        "the repository has no bun.lock at its root, which `bun install --frozen-lockfile` needs"
    );
    paths.extend(locks.into_iter().map(str::to_owned));
    let (include, exclude) = workspaces(manifest);
    for path in tree.keys() {
        let Some(dir) = path.strip_suffix("/package.json") else {
            continue;
        };
        let below_modules = dir.split('/').any(|part| part == "node_modules");
        if !below_modules && paths::any(&include, path) && !paths::any(&exclude, path) {
            paths.insert(path.clone());
        }
    }
    for patch in patched(manifest) {
        if tree.contains_key(&patch) {
            paths.insert(patch);
        }
    }
    ensure!(
        paths.len() <= MAX_FILES,
        "an install would read {} files, over the {MAX_FILES} a build takes",
        paths.len()
    );
    Ok(paths)
}

/// The globs of the files a root manifest's `workspaces` name, and those a
/// `!` excludes. `"packages/*"` is `packages/*/package.json`.
fn workspaces(manifest: &Value) -> (Vec<String>, Vec<String>) {
    let listed = match &manifest["workspaces"] {
        Value::Array(list) => list.as_slice(),
        Value::Object(object) => object
            .get("packages")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice),
        _ => &[],
    };
    let (mut include, mut exclude) = (Vec::new(), Vec::new());
    for pattern in listed.iter().filter_map(Value::as_str) {
        let (negated, pattern) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern),
        };
        let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
        let glob = format!("{pattern}/package.json");
        if paths::compiles(&glob).is_err() {
            continue;
        }
        if negated {
            exclude.push(glob);
        } else {
            include.push(glob);
        }
    }
    (include, exclude)
}

/// The patch files `patchedDependencies` names.
fn patched(manifest: &Value) -> Vec<String> {
    manifest["patchedDependencies"]
        .as_object()
        .into_iter()
        .flat_map(|object| object.values())
        .filter_map(Value::as_str)
        .map(|path| path.trim_start_matches("./").to_owned())
        .collect()
}

/// Refused: the text lockfile names a host other than the default registry.
/// A tarball, a git dependency or a private registry is a URL there, and the
/// build would fetch it on the host's network.
fn registry_only(lock: &str) -> Result<()> {
    let mut rest = lock;
    while let Some(at) = rest.find("://") {
        let after = &rest[at + 3..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '@' | '_')))
            .unwrap_or(after.len());
        let host = &after[..end];
        ensure!(
            host == REGISTRY_HOST,
            "the lockfile names {host:?}, and a build fetches from {REGISTRY_HOST} only"
        );
        rest = &after[end..];
    }
    Ok(())
}

/// The cache key of `inputs`: a SHA-256 of what the install reads.
pub(super) fn key(kind: DepsKind, inputs: &Inputs) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("solos-deps/{VERSION}\nkind:{}\n", kind.as_str()));
    for (path, bytes) in &inputs.files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    format!("{:x}", hasher.finalize())
}

/// What a finished build left, and what a Ticket needs of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    kind: String,
    key: String,
    built_ms: u64,
    /// Bytes on disk, each file once.
    bytes: u64,
    dirs: Vec<Dir>,
}

/// One `node_modules` directory, by its path from the repository's root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mount", rename_all = "snake_case")]
enum Dir {
    /// The directory itself, mounted read-only.
    Tree { path: String, bytes: u64 },
    /// Only links: made again in every sandbox.
    Links { path: String, links: Vec<Entry> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    path: String,
    target: String,
}

impl Dir {
    fn path(&self) -> &str {
        match self {
            Self::Tree { path, .. } | Self::Links { path, .. } => path,
        }
    }
}

/// A path that stays below its root: `a/b`, no `..`, no root.
fn below(path: &str) -> bool {
    !path.contains('\0')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

impl Manifest {
    /// Refused: a manifest that is not what a build writes. The host wrote
    /// it, but a mount is made from it, so it is read strictly.
    fn check(&self, key: &str) -> Result<()> {
        ensure!(
            self.version == VERSION && self.key == key,
            "the manifest is of another version or key"
        );
        ensure!(
            self.dirs.len() <= MAX_DIRS,
            "the manifest has too many directories"
        );
        for dir in &self.dirs {
            ensure!(
                below(dir.path()) && dir.path().rsplit('/').next() == Some("node_modules"),
                "the manifest names {:?}, not a node_modules directory",
                dir.path()
            );
            if let Dir::Links { links, .. } = dir {
                ensure!(links.len() <= MAX_LINKS, "the manifest has too many links");
                for link in links {
                    ensure!(
                        below(&link.path) && !link.target.is_empty() && !link.target.contains('\0'),
                        "the manifest has a link {:?} that is not one",
                        link.path
                    );
                }
            }
        }
        Ok(())
    }
}

/// What a Ticket's `run_code` adds to have the cache at `dir`: each tree
/// read-only and each links directory made again, under `/work`; the package
/// manager read-only as `bun` and `node`, first on `PATH`; a throwaway home;
/// and the address space the package manager needs.
fn extras(dir: &Path, manifest: &Manifest, binary: &Path, memory_bytes: u64) -> Extras {
    let mut binds = vec![
        Bind::ReadOnly {
            source: binary.to_owned(),
            target: Path::new(TOOLS).join("bun"),
        },
        Bind::ReadOnly {
            source: binary.to_owned(),
            target: Path::new(TOOLS).join("node"),
        },
        Bind::Links {
            dir: PathBuf::from(HOME),
            links: Vec::new(),
        },
    ];
    let mut dirs: Vec<&Dir> = manifest.dirs.iter().collect();
    dirs.sort_by(|a, b| a.path().cmp(b.path()));
    for each in dirs {
        let target = Path::new("/work").join(each.path());
        binds.push(match each {
            Dir::Tree { path, .. } => Bind::ReadOnly {
                source: dir.join(path),
                target,
            },
            Dir::Links { links, .. } => Bind::Links {
                dir: target,
                links: links
                    .iter()
                    .map(|link| Link {
                        path: PathBuf::from(&link.path),
                        target: PathBuf::from(&link.target),
                    })
                    .collect(),
            },
        });
    }
    Extras {
        binds,
        path: vec![PathBuf::from(TOOLS)],
        env: vec![("HOME".to_owned(), HOME.to_owned())],
        memory_bytes: Some(memory_bytes),
    }
}

/// The bwrap sandbox of one build, as the argument vector that starts it.
/// Every input is a distinct mount or variable: the host's network (the one
/// thing this sandbox shares), `/usr` and the TLS and DNS files, the package
/// manager read-only, and `scratch`, a directory made for it, writable at `/s`.
pub(super) struct Build<'a> {
    pub prlimit: &'a Path,
    pub bwrap: &'a Path,
    pub binary: &'a Path,
    pub scratch: &'a Path,
}

impl Build<'_> {
    pub(super) fn argv(&self) -> Vec<OsString> {
        let sandbox = Bwrap::new()
            .unshare_all()
            .share_net()
            .die_with_parent()
            .new_session()
            .ro_bind("/usr", "/usr")
            .ro_bind_try("/lib", "/lib")
            .ro_bind_try("/lib64", "/lib64");
        let sandbox = ETC_BINDS
            .into_iter()
            .fold(sandbox, |sandbox, path| sandbox.ro_bind_try(path, path))
            .symlink("usr/bin", "/bin")
            .symlink("usr/sbin", "/sbin")
            .proc("/proc")
            .dev("/dev")
            .tmpfs_sized(TMPFS_BYTES, "/tmp")
            .ro_bind(self.binary, Path::new(TOOLS).join("bun"))
            .bind(self.scratch, SCRATCH)
            .chdir(format!("{SCRATCH}/src"))
            .clearenv()
            .setenv("PATH", "/usr/bin:/bin")
            .setenv("HOME", format!("{SCRATCH}/home"))
            .setenv("TMPDIR", format!("{SCRATCH}/tmp"))
            .setenv("BUN_TMPDIR", format!("{SCRATCH}/tmp"))
            .setenv("BUN_INSTALL_CACHE_DIR", format!("{SCRATCH}/cache"))
            .setenv("CI", "1")
            .setenv("NO_COLOR", "1")
            .setenv("DO_NOT_TRACK", "1");
        let mut line: Vec<OsString> = vec![
            self.prlimit.as_os_str().to_owned(),
            format!("--as={BUILD_ADDRESS_SPACE}").into(),
            format!("--fsize={BUILD_FILE_BYTES}").into(),
            format!("--nofile={BUILD_OPEN_FILES}").into(),
            "--core=0".into(),
            "--".into(),
            self.bwrap.as_os_str().to_owned(),
        ];
        line.extend(sandbox.exec(
            Path::new(TOOLS).join("bun"),
            [
                "install",
                "--frozen-lockfile",
                "--ignore-scripts",
                "--no-progress",
                "--registry",
                REGISTRY,
            ],
        ));
        line
    }
}

/// The caches under `<state>/deps`, and the builds that make them.
pub(super) struct Cache {
    dir: PathBuf,
    /// A build that failed, by repository: its key, when and why.
    failed: Mutex<HashMap<RepoName, (String, Instant, String)>>,
    prlimit: PathBuf,
    bwrap: PathBuf,
}

impl Cache {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            failed: Mutex::new(HashMap::new()),
            prlimit: PathBuf::from("/usr/bin/prlimit"),
            bwrap: PathBuf::from("/usr/bin/bwrap"),
        }
    }

    /// The cache for `inputs`, built now if it is not there. One build per
    /// repository at a time: the caller holds that lock.
    pub(super) async fn ensure(
        &self,
        repo: &RepoName,
        inputs: &Inputs,
        config: &DepsConfig,
    ) -> Dependencies {
        let key = key(config.kind, inputs);
        match self.ready(repo, inputs, config, &key).await {
            Ok(ready) => ready,
            Err(err) => {
                let why = one_line(&format!("{err:#}"));
                eprintln!(
                    "agent: deps: {repo} {} are unavailable: {why}",
                    config.kind.as_str()
                );
                Dependencies::Unavailable { why }
            }
        }
    }

    async fn ready(
        &self,
        repo: &RepoName,
        inputs: &Inputs,
        config: &DepsConfig,
        key: &str,
    ) -> Result<Dependencies> {
        let binary = config
            .binary_path()
            .context("there is no package manager path: set forge.repos.<repo>.deps.binary")?;
        let binary = binary
            .canonicalize()
            .with_context(|| format!("{} is not there", binary.display()))?;
        ensure!(binary.is_file(), "{} is not a file", binary.display());
        let repo_dir = self.dir.join(repo.owner()).join(repo.name());
        let at = repo_dir.join(key);
        match load(&at, key) {
            Ok(Some(manifest)) => {
                touch(&at);
                return Ok(ready(key, &at, &manifest, &binary, config.memory_bytes));
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!(
                    "agent: deps: the cache of {repo} at {} is damaged ({err:#}); building it again",
                    at.display()
                );
                remove(&at).await;
            }
        }
        if let Some((failed, when, why)) = self
            .failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(repo)
            && failed == key
            && when.elapsed() < FAILED_FOR
        {
            bail!(
                "{why} (the build of this lockfile failed a moment ago and is not tried again yet)"
            );
        }
        private_dir(&self.dir)?;
        private_dir(&self.dir.join(repo.owner()))?;
        private_dir(&repo_dir)?;
        clear_scratch(&repo_dir).await;
        let scratch = repo_dir.join(format!("{SCRATCH_PREFIX}{}", uuid::Uuid::new_v4().simple()));
        let built = self
            .build(&scratch, inputs, config, &binary, key, &at)
            .await;
        remove(&scratch).await;
        match built {
            Ok(manifest) => {
                eprintln!(
                    "agent: deps: built {repo} {} dependencies, {} bytes, key {}",
                    config.kind.as_str(),
                    manifest.bytes,
                    &key[..12]
                );
                Ok(ready(key, &at, &manifest, &binary, config.memory_bytes))
            }
            Err(err) => {
                self.failed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(
                        repo.clone(),
                        (
                            key.to_owned(),
                            Instant::now(),
                            one_line(&format!("{err:#}")),
                        ),
                    );
                Err(err)
            }
        }
    }

    /// Install into `scratch`, then publish what it made at `at`.
    async fn build(
        &self,
        scratch: &Path,
        inputs: &Inputs,
        config: &DepsConfig,
        binary: &Path,
        key: &str,
        at: &Path,
    ) -> Result<Manifest> {
        // The sandbox writes in `s/` alone; `out/` and the log are the host's.
        let sandboxed = scratch.join("s");
        for dir in ["src", "cache", "tmp", "home"] {
            private_dir(&sandboxed.join(dir))?;
        }
        private_dir(&scratch.join("out"))?;
        write_inputs(&sandboxed.join("src"), inputs)?;
        let sandbox = Build {
            prlimit: &self.prlimit,
            bwrap: &self.bwrap,
            binary,
            scratch: &sandboxed,
        };
        run_install(&sandbox.argv(), scratch, config).await?;
        let src = sandboxed.join("src");
        let out = scratch.join("out");
        let (dirs, bytes) = tokio::task::spawn_blocking(move || harvest(&src, &out))
            .await
            .context("the harvest of the install did not finish")??;
        ensure!(
            !dirs.is_empty(),
            "the install made no node_modules directory (does the repository have dependencies?)"
        );
        let manifest = Manifest {
            version: VERSION,
            kind: config.kind.as_str().to_owned(),
            key: key.to_owned(),
            built_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(0)),
            bytes,
            dirs,
        };
        let out = scratch.join("out");
        let text = serde_json::to_vec_pretty(&manifest)?;
        let mut file = tokio::fs::File::create(out.join(MANIFEST)).await?;
        file.write_all(&text).await?;
        file.sync_all().await?;
        drop(file);
        match std::fs::rename(&out, at) {
            Ok(()) => {}
            // Another process published this key first: its cache is as good.
            Err(_) if load(at, key).ok().flatten().is_some() => {}
            Err(err) => {
                return Err(err).with_context(|| format!("cannot publish {}", at.display()));
            }
        }
        Ok(manifest)
    }
}

fn ready(key: &str, dir: &Path, manifest: &Manifest, binary: &Path, memory: u64) -> Dependencies {
    Dependencies::Ready {
        key: key.to_owned(),
        extras: extras(dir, manifest, binary, memory),
    }
}

/// A complete cache at `dir`, `None` if there is none, an error if what is
/// there is not one.
fn load(dir: &Path, key: &str) -> Result<Option<Manifest>> {
    let text = match std::fs::read(dir.join(MANIFEST)) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !dir.exists() => {
            return Ok(None);
        }
        Err(err) => return Err(err).context("cannot read the manifest"),
    };
    let manifest: Manifest = serde_json::from_slice(&text).context("the manifest is not JSON")?;
    manifest.check(key)?;
    for each in &manifest.dirs {
        if let Dir::Tree { path, .. } = each {
            let meta = std::fs::symlink_metadata(dir.join(path))
                .with_context(|| format!("{path} is gone"))?;
            ensure!(meta.is_dir(), "{path} is not a directory");
        }
    }
    Ok(Some(manifest))
}

/// Mark the cache used now, for `agent prune`.
fn touch(dir: &Path) {
    if let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join(MANIFEST))
    {
        let _ = file.set_modified(SystemTime::now());
    }
}

fn write_inputs(src: &Path, inputs: &Inputs) -> Result<()> {
    for (path, bytes) in &inputs.files {
        ensure!(below(path), "{path:?} is not a path inside the repository");
        let at = src.join(path);
        if let Some(parent) = at.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&at, bytes).with_context(|| format!("cannot write {}", at.display()))?;
    }
    Ok(())
}

/// Run the build sandbox `argv` over `scratch` until it ends, its time is up
/// or the disk it uses passes the cap.
async fn run_install(argv: &[OsString], scratch: &Path, config: &DepsConfig) -> Result<()> {
    let log = scratch.join("build.log");
    let out = std::fs::File::create(&log)?;
    let err = out.try_clone()?;
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .kill_on_drop(true)
        .spawn()
        .context("the build sandbox did not start")?;
    let limit = Duration::from_secs(config.timeout_secs);
    let started = Instant::now();
    let status = loop {
        tokio::select! {
            status = child.wait() => break status.context("the build did not finish")?,
            () = tokio::time::sleep(POLL) => {}
        }
        if started.elapsed() > limit {
            let _ = child.kill().await;
            bail!(
                "the install took longer than {} seconds",
                config.timeout_secs
            );
        }
        let (root, cap) = (scratch.to_owned(), config.max_bytes);
        let used = tokio::task::spawn_blocking(move || dir_bytes(&root, cap))
            .await
            .unwrap_or(0);
        if used > config.max_bytes {
            let _ = child.kill().await;
            bail!(
                "the install used more than {} bytes of disk, its cap",
                config.max_bytes
            );
        }
    };
    if !status.success() {
        bail!(
            "the install failed ({status}): {}",
            complaint(&std::fs::read_to_string(&log).unwrap_or_default())
        );
    }
    Ok(())
}

/// What a failed install said: its error lines if it had any, since its
/// last lines are the packages it did install, else the end of its output.
fn complaint(output: &str) -> String {
    let errors: Vec<&str> = output
        .lines()
        .filter(|line| line.trim_start().to_ascii_lowercase().starts_with("error"))
        .take(3)
        .collect();
    let text = if errors.is_empty() {
        output.trim().to_owned()
    } else {
        errors.join(" ")
    };
    let tail: String = text.chars().rev().take(LOG_TAIL_CHARS).collect();
    tail.chars().rev().collect()
}

/// Bytes on disk under `root`, each file once and no link followed, counting
/// stops once it passes `limit`.
fn dir_bytes(root: &Path, limit: u64) -> u64 {
    let mut seen = HashSet::new();
    let mut total = 0u64;
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                pending.push(entry.path());
            } else if meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
                continue;
            }
            total = total.saturating_add(meta.blocks() * 512);
            if total > limit {
                return total;
            }
        }
    }
    total
}

/// What an install left under `src`: the `node_modules` directories that hold
/// real files are moved to `out` at the same paths, and a directory of links
/// only is recorded by its links. Returns what to record and the bytes kept.
fn harvest(src: &Path, out: &Path) -> Result<(Vec<Dir>, u64)> {
    let mut found = Vec::new();
    find_modules(src, Path::new(""), 0, &mut found)?;
    ensure!(
        found.len() <= MAX_DIRS,
        "the install made {} node_modules directories",
        found.len()
    );
    found.sort();
    let mut dirs = Vec::new();
    for rel in found {
        let path = rel
            .to_str()
            .with_context(|| format!("{} is not UTF-8", rel.display()))?
            .to_owned();
        match classify(&src.join(&rel))? {
            Class::Empty => {}
            Class::Links(links) => {
                for link in &links {
                    ensure!(
                        inside(&rel, link),
                        "the install left a link, {path}/{} -> {}, that leaves the repository",
                        link.path,
                        link.target
                    );
                }
                dirs.push(Dir::Links { path, links });
            }
            Class::Tree => {
                let to = out.join(&rel);
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(src.join(&rel), &to)
                    .with_context(|| format!("cannot keep {path}"))?;
                dirs.push(Dir::Tree { path, bytes: 0 });
            }
        }
    }
    for dir in &mut dirs {
        if let Dir::Tree { path, bytes } = dir {
            *bytes = dir_bytes(&out.join(&*path), u64::MAX);
        }
    }
    let kept = dir_bytes(out, u64::MAX);
    Ok((dirs, kept))
}

/// Every `node_modules` below `dir` (relative to the root) that is not inside
/// another, links not followed.
fn find_modules(root: &Path, rel: &Path, depth: usize, found: &mut Vec<PathBuf>) -> Result<()> {
    if depth > MAX_FIND_DEPTH {
        return Ok(());
    }
    for entry in std::fs::read_dir(root.join(rel))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let at = rel.join(entry.file_name());
        if entry.file_name() == "node_modules" {
            found.push(at);
        } else {
            find_modules(root, &at, depth + 1, found)?;
        }
    }
    Ok(())
}

enum Class {
    Empty,
    /// Real files: mount the directory.
    Tree,
    /// Links and the directories between them, nothing else.
    Links(Vec<Entry>),
}

fn classify(dir: &Path) -> Result<Class> {
    let mut links = Vec::new();
    if !only_links(dir, dir, 0, &mut links)? {
        return Ok(Class::Tree);
    }
    // The order a directory lists in is the file system's, so what is kept
    // does not depend on it.
    links.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(if links.is_empty() {
        Class::Empty
    } else {
        Class::Links(links)
    })
}

/// Whether everything below `dir` is a link or a directory of them, with the
/// links added to `links`.
fn only_links(root: &Path, dir: &Path, depth: usize, links: &mut Vec<Entry>) -> Result<bool> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let at = entry.path();
        if kind.is_symlink() {
            let target = std::fs::read_link(&at)?;
            let (Some(target), Some(path)) = (
                target.to_str(),
                at.strip_prefix(root).ok().and_then(Path::to_str),
            ) else {
                bail!(
                    "{} is a link whose name or target is not UTF-8",
                    at.display()
                );
            };
            ensure!(
                !target.starts_with('/'),
                "{} is a link to the absolute path {target}, which is the build's, not a sandbox's",
                at.display()
            );
            links.push(Entry {
                path: path.to_owned(),
                target: target.to_owned(),
            });
            if links.len() > MAX_LINKS {
                return Ok(false);
            }
        } else if kind.is_dir() {
            if depth >= MAX_LINK_DEPTH || !only_links(root, &at, depth + 1, links)? {
                return Ok(false);
            }
        } else {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether `link`, in the `node_modules` at `dir` (from the repository's
/// root), points no higher than the root.
fn inside(dir: &Path, link: &Entry) -> bool {
    let mut depth = dir.components().count() + Path::new(&link.path).components().count() - 1;
    for part in Path::new(&link.target).components() {
        match part {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => match depth.checked_sub(1) {
                Some(up) => depth = up,
                None => return false,
            },
            Component::CurDir => {}
            _ => return false,
        }
    }
    true
}

async fn remove(path: &Path) {
    let owned = path.to_owned();
    let _ = tokio::task::spawn_blocking(move || {
        if let Err(err) = std::fs::remove_dir_all(&owned)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("agent: deps: cannot remove {}: {err}", owned.display());
        }
    })
    .await;
}

/// The scratch directories an earlier build left, which no build running now
/// owns: the caller holds the repository's build lock.
async fn clear_scratch(repo_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(repo_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(SCRATCH_PREFIX)
        {
            remove(&entry.path()).await;
        }
    }
}

/// `text` on one line, cut at a bound.
fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= WHY_CHARS {
        return flat;
    }
    let kept: String = flat.chars().take(WHY_CHARS).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests;
