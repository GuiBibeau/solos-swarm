use super::*;
use crate::agent::config::DepsKind;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

fn repo() -> RepoName {
    RepoName::try_from("acme/widgets".to_owned()).unwrap()
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agent-deps-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// An executable file named `name` holding `script`.
    fn program(&self, name: &str, script: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

fn tree(files: &[(&str, &str)]) -> Tree {
    files
        .iter()
        .enumerate()
        .map(|(n, (path, text))| {
            let blob = Blob {
                sha: format!("{n:040x}"),
                size: text.len() as u64,
            };
            ((*path).to_owned(), blob)
        })
        .collect()
}

fn inputs(files: &[(&str, &str)]) -> Inputs {
    Inputs {
        files: files
            .iter()
            .map(|(path, text)| ((*path).to_owned(), text.as_bytes().to_vec()))
            .collect(),
    }
}

const ROOT: &str = r#"{"name":"acme","workspaces":["apps/*","packages/*"]}"#;

fn config(binary: &Path) -> DepsConfig {
    DepsConfig {
        kind: DepsKind::Bun,
        binary: Some(binary.to_owned()),
        timeout_secs: 60,
        max_bytes: 1 << 30,
        memory_bytes: 16 << 30,
    }
}

fn selected(listed: &[&str], manifest: &str) -> Result<Vec<String>> {
    let files: Vec<(&str, &str)> = listed.iter().map(|path| (*path, "x")).collect();
    let manifest: Value = serde_json::from_str(manifest).unwrap();
    select_bun(&tree(&files), &manifest).map(|paths| paths.into_iter().collect())
}

#[test]
fn an_install_reads_the_lockfile_and_the_workspaces_manifests_and_nothing_else() {
    let listed = [
        "package.json",
        "bun.lock",
        "bunfig.toml",
        ".npmrc",
        "apps/web/package.json",
        "apps/web/.npmrc",
        "apps/cli/package.json",
        "packages/core/package.json",
        "packages/core/node_modules/dep/package.json",
        "tools/fixture/package.json",
        "patches/dep@1.0.0.patch",
        "src/index.js",
    ];
    let root = r#"{"workspaces":["apps/*","./packages/*/","!apps/cli"],
                   "patchedDependencies":{"dep@1.0.0":"patches/dep@1.0.0.patch"}}"#;

    let paths = selected(&listed, root).unwrap();

    assert_eq!(
        paths,
        [
            "apps/web/package.json",
            "bun.lock",
            "package.json",
            "packages/core/package.json",
            "patches/dep@1.0.0.patch",
        ],
        "not bunfig.toml or .npmrc (no registry setting of the repository's is read), not a \
         manifest outside the workspaces, not one in node_modules, not an excluded workspace"
    );
    let object = r#"{"workspaces":{"packages":["apps/*"]}}"#;
    let paths = selected(&listed, object).unwrap();
    assert!(paths.contains(&"apps/cli/package.json".to_owned()));
    assert!(!paths.contains(&"packages/core/package.json".to_owned()));
    let none = selected(&listed, r#"{"name":"single"}"#).unwrap();
    assert_eq!(none, ["bun.lock", "package.json"]);
}

#[test]
fn a_repository_without_a_lockfile_is_refused_in_words() {
    let why = selected(&["package.json", "apps/web/package.json"], ROOT).unwrap_err();
    assert!(
        format!("{why:#}").contains("no bun.lock at its root"),
        "{why:#}"
    );
    let binary = selected(&["package.json", "bun.lockb"], ROOT).unwrap();
    assert_eq!(binary, ["bun.lockb", "package.json"]);
}

#[test]
fn a_lockfile_that_names_any_host_but_the_default_registry_is_refused() {
    let ok = r#"{"packages":{"left-pad":["left-pad@1.3.0","","sha512-x"]},
                 "note":"https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz"}"#;
    assert!(registry_only(ok).is_ok());
    assert!(
        registry_only(r#"{"workspaces":{}}"#).is_ok(),
        "no URL at all"
    );
    for (lock, host) in [
        (
            r#""x": ["x@https://evil.example.com/x.tgz", "", {}]"#,
            "evil.example.com",
        ),
        (
            r#""x": ["x@git+https://github.com/a/b.git#abc"]"#,
            "github.com",
        ),
        (
            r#""x": ["x@git+ssh://git@github.com/a/b.git"]"#,
            "git@github.com",
        ),
        (
            r#""x": ["x@1.0.0", "http://10.0.0.5:4873/", {}]"#,
            "10.0.0.5",
        ),
        (
            r#""x": ["x@1.0.0", "https://registry.npmjs.org.evil.io/", {}]"#,
            "registry.npmjs.org.evil.io",
        ),
    ] {
        let why = registry_only(lock).unwrap_err().to_string();
        assert!(
            why.contains(host) && why.contains("registry.npmjs.org"),
            "{lock}: {why}"
        );
    }
}

#[test]
fn the_key_follows_the_lockfile_and_the_manifests_and_nothing_unrelated() {
    let files = [
        ("package.json", ROOT),
        ("bun.lock", "lock v1"),
        ("apps/web/package.json", r#"{"name":"web"}"#),
    ];
    let key = |files: &[(&str, &str)]| super::key(DepsKind::Bun, &inputs(files));
    let base = key(&files);
    assert_eq!(base.len(), 64, "a SHA-256");
    assert_eq!(base, key(&files), "the same files, the same key");
    let mut reversed = files;
    reversed.reverse();
    assert_eq!(
        base,
        key(&reversed),
        "the order they were read in is no part of it"
    );

    let mut lock = files;
    lock[1].1 = "lock v2";
    assert_ne!(base, key(&lock), "a new lockfile is a new cache");
    let mut manifest = files;
    manifest[2].1 = r#"{"name":"web","dependencies":{"x":"1"}}"#;
    assert_ne!(base, key(&manifest), "a workspace's manifest is part of it");
    let mut moved = files;
    moved[2].0 = "apps/site/package.json";
    assert_ne!(base, key(&moved), "and where it is");
    assert_ne!(
        key(&[("a", "bc")]),
        key(&[("ab", "c")]),
        "a path and a body cannot be told apart by where one ends"
    );
}

#[test]
fn the_tree_listing_keeps_regular_files_only() {
    let listed = b"100644 blob aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa      12\tpackage.json\0\
                   100755 blob bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb       4\tscripts/run\0\
                   120000 blob cccccccccccccccccccccccccccccccccccccccc       9\tbun.lock\0\
                   160000 commit dddddddddddddddddddddddddddddddddddddddd       -\tvendor/sub\0\
                   100644 blob eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee       7\twith space/a b.txt\0";
    let tree = parse_tree(listed).unwrap();
    let names: Vec<&str> = tree.keys().map(String::as_str).collect();
    assert_eq!(names, ["package.json", "scripts/run", "with space/a b.txt"]);
    assert_eq!(tree["package.json"].size, 12);
}

#[test]
fn the_build_sandbox_has_the_network_and_only_what_an_install_needs() {
    let argv = Build {
        prlimit: Path::new("/usr/bin/prlimit"),
        bwrap: Path::new("/usr/bin/bwrap"),
        binary: Path::new("/home/me/.bun/bin/bun"),
        scratch: Path::new("/state/deps/acme/widgets/tmp-1/s"),
    }
    .argv();
    let words: Vec<String> = argv
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let text = words.join("\n");

    assert_eq!(words[0], "/usr/bin/prlimit");
    let bwrap = words.iter().position(|w| w == "/usr/bin/bwrap").unwrap();
    let flags = &words[bwrap + 1..];
    assert_eq!(
        &flags[..2],
        ["--unshare-all", "--share-net"],
        "the one thing shared"
    );
    for must in ["--die-with-parent", "--new-session", "--clearenv"] {
        assert!(flags.iter().any(|w| w == must), "{must}");
    }
    let binds: Vec<&[String]> = flags
        .windows(3)
        .filter(|w| w[0] == "--bind" || w[0] == "--ro-bind")
        .collect();
    assert_eq!(
        binds.iter().filter(|w| w[0] == "--bind").count(),
        1,
        "one writable mount: {text}"
    );
    assert!(text.contains("--bind\n/state/deps/acme/widgets/tmp-1/s\n/s\n"));
    assert!(text.contains("--ro-bind\n/home/me/.bun/bin/bun\n/runtime/bun/bun\n"));
    let sources: Vec<&str> = binds.iter().map(|w| w[1].as_str()).collect();
    assert_eq!(
        sources,
        [
            "/usr",
            "/home/me/.bun/bin/bun",
            "/state/deps/acme/widgets/tmp-1/s"
        ],
        "no home, no state, no vault, no broker socket (the optional /etc files are -try binds)"
    );
    let set: Vec<&str> = flags
        .windows(3)
        .filter(|w| w[0] == "--setenv")
        .map(|w| w[1].as_str())
        .collect();
    assert_eq!(
        set,
        [
            "PATH",
            "HOME",
            "TMPDIR",
            "BUN_TMPDIR",
            "BUN_INSTALL_CACHE_DIR",
            "CI",
            "NO_COLOR",
            "DO_NOT_TRACK"
        ],
        "nothing of the host's environment, and no token"
    );
    assert!(
        text.contains("--setenv\nHOME\n/s/home\n"),
        "a home that is the scratch's"
    );
    let install = &flags[flags.iter().rposition(|w| w == "/runtime/bun/bun").unwrap() + 1..];
    assert_eq!(
        install,
        [
            "install",
            "--frozen-lockfile",
            "--ignore-scripts",
            "--no-progress",
            "--registry",
            "https://registry.npmjs.org/"
        ],
        "no lifecycle script, the lockfile as it is, the default registry named on the line"
    );
    assert!(text.contains("--chdir\n/s/src\n"));
    for limit in ["--as=", "--fsize=", "--nofile="] {
        assert!(words.iter().any(|w| w.starts_with(limit)), "{limit}");
    }
}

#[test]
fn what_a_ticket_mounts_is_the_trees_read_only_the_links_made_again_and_bun_as_node() {
    let manifest = Manifest {
        version: VERSION,
        kind: "bun".into(),
        key: "k".repeat(64),
        built_ms: 1,
        bytes: 10,
        dirs: vec![
            Dir::Tree {
                path: "node_modules".into(),
                bytes: 9,
            },
            Dir::Links {
                path: "apps/web/node_modules".into(),
                links: vec![
                    Entry {
                        path: "left-pad".into(),
                        target: "../../../node_modules/.bun/left-pad/node_modules/left-pad".into(),
                    },
                    Entry {
                        path: "@acme/lib".into(),
                        target: "../../../../packages/lib".into(),
                    },
                ],
            },
        ],
    };
    manifest.check(&"k".repeat(64)).unwrap();

    let found = extras(
        Path::new("/state/deps/acme/widgets/kkkk"),
        &manifest,
        Path::new("/opt/bun"),
        16 << 30,
    );

    let ro = |source: &str, target: &str| Bind::ReadOnly {
        source: source.into(),
        target: target.into(),
    };
    assert_eq!(
        found.binds,
        [
            ro("/opt/bun", "/runtime/bun/bun"),
            ro("/opt/bun", "/runtime/bun/node"),
            Bind::Links {
                dir: "/tmp/home".into(),
                links: Vec::new()
            },
            Bind::Links {
                dir: "/work/apps/web/node_modules".into(),
                links: vec![
                    Link {
                        path: "left-pad".into(),
                        target: "../../../node_modules/.bun/left-pad/node_modules/left-pad".into()
                    },
                    Link {
                        path: "@acme/lib".into(),
                        target: "../../../../packages/lib".into()
                    },
                ]
            },
            ro(
                "/state/deps/acme/widgets/kkkk/node_modules",
                "/work/node_modules"
            ),
        ]
    );
    assert_eq!(found.path, [PathBuf::from("/runtime/bun")]);
    assert_eq!(found.env, [("HOME".to_owned(), "/tmp/home".to_owned())]);
    assert_eq!(
        found.memory_bytes,
        Some(16 << 30),
        "bun reserves more address space than a sandbox's usual cap"
    );
}

#[test]
fn a_manifest_that_is_not_what_a_build_writes_is_refused_before_a_mount_is_made_from_it() {
    let key = "k".repeat(64);
    let good = Manifest {
        version: VERSION,
        kind: "bun".into(),
        key: key.clone(),
        built_ms: 1,
        bytes: 1,
        dirs: vec![Dir::Tree {
            path: "node_modules".into(),
            bytes: 1,
        }],
    };
    assert!(good.check(&key).is_ok());
    assert!(good.check(&"j".repeat(64)).is_err(), "another key");
    let with = |dir: Dir| Manifest {
        dirs: vec![dir],
        ..good.clone()
    };
    for path in [
        "../node_modules",
        "/node_modules",
        "a/../node_modules",
        "apps/web",
        "",
        "a//node_modules",
    ] {
        let bad = with(Dir::Tree {
            path: path.into(),
            bytes: 1,
        });
        assert!(bad.check(&key).is_err(), "accepted {path:?}");
    }
    for link in ["../x", "/abs", "", "a/../b"] {
        let bad = with(Dir::Links {
            path: "a/node_modules".into(),
            links: vec![Entry {
                path: link.into(),
                target: "../y".into(),
            }],
        });
        assert!(bad.check(&key).is_err(), "accepted a link at {link:?}");
    }
    let old = Manifest {
        version: VERSION + 1,
        ..good
    };
    assert!(old.check(&key).is_err());
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn link(target: &str, at: &Path) {
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, at).unwrap();
}

/// What `bun install` leaves for a workspace repository, in bun's own layout.
fn installed(src: &Path) {
    write(
        &src.join("node_modules/.bun/left-pad@1.3.0/node_modules/left-pad/index.js"),
        "module.exports = 1",
    );
    link(
        ".bun/left-pad@1.3.0/node_modules/left-pad",
        &src.join("node_modules/left-pad"),
    );
    link(
        "../../../node_modules/.bun/left-pad@1.3.0/node_modules/left-pad",
        &src.join("apps/web/node_modules/left-pad"),
    );
    link(
        "../../../../packages/lib",
        &src.join("apps/web/node_modules/@acme/lib"),
    );
    write(&src.join("packages/lib/package.json"), "{}");
    std::fs::create_dir_all(src.join("packages/lib/node_modules")).unwrap();
}

#[test]
fn an_install_is_kept_as_trees_to_mount_and_links_to_make_again_and_what_it_leaves_over_is_not() {
    let scratch = Scratch::new("harvest");
    let (src, out) = (scratch.0.join("src"), scratch.0.join("out"));
    std::fs::create_dir_all(&out).unwrap();
    installed(&src);

    let (dirs, bytes) = harvest(&src, &out).unwrap();

    assert_eq!(
        dirs,
        [
            Dir::Links {
                path: "apps/web/node_modules".into(),
                links: {
                    let mut links = vec![
                        Entry {
                            path: "@acme/lib".into(),
                            target: "../../../../packages/lib".into(),
                        },
                        Entry {
                            path: "left-pad".into(),
                            target:
                                "../../../node_modules/.bun/left-pad@1.3.0/node_modules/left-pad"
                                    .into(),
                        },
                    ];
                    links.sort_by(|a, b| a.path.cmp(&b.path));
                    links
                }
            },
            Dir::Tree {
                path: "node_modules".into(),
                bytes: dirs_bytes(&out.join("node_modules")),
            },
        ],
        "an empty node_modules is nothing to mount"
    );
    assert!(bytes > 0);
    assert!(
        out.join("node_modules/.bun/left-pad@1.3.0/node_modules/left-pad/index.js")
            .is_file()
    );
    assert!(
        !src.join("node_modules").exists(),
        "moved, not copied: one rename, no second tree"
    );
    assert!(
        !out.join("apps").exists() && !out.join("packages").exists(),
        "a links-only directory leaves nothing in the cache"
    );
}

fn dirs_bytes(path: &Path) -> u64 {
    dir_bytes(path, u64::MAX)
}

#[test]
fn a_link_the_sandbox_could_not_follow_refuses_the_cache() {
    let absolute = Scratch::new("absolute");
    let (src, out) = (absolute.0.join("src"), absolute.0.join("out"));
    std::fs::create_dir_all(&out).unwrap();
    link(
        "/s/src/node_modules/x",
        &src.join("apps/web/node_modules/x"),
    );
    let why = harvest(&src, &out).unwrap_err().to_string();
    assert!(why.contains("absolute path"), "{why}");

    let escaping = Scratch::new("escaping");
    let (src, out) = (escaping.0.join("src"), escaping.0.join("out"));
    std::fs::create_dir_all(&out).unwrap();
    link("../../../../etc", &src.join("apps/web/node_modules/x"));
    let why = harvest(&src, &out).unwrap_err().to_string();
    assert!(why.contains("leaves the repository"), "{why}");

    let inside = Scratch::new("inside");
    let (src, out) = (inside.0.join("src"), inside.0.join("out"));
    std::fs::create_dir_all(&out).unwrap();
    link("../../../packages/x", &src.join("apps/web/node_modules/x"));
    assert!(harvest(&src, &out).is_ok());
}

#[test]
fn a_directory_with_real_files_among_its_links_is_mounted_whole() {
    let scratch = Scratch::new("mixed");
    let (src, out) = (scratch.0.join("src"), scratch.0.join("out"));
    std::fs::create_dir_all(&out).unwrap();
    link("../../../packages/x", &src.join("apps/web/node_modules/x"));
    write(&src.join("apps/web/node_modules/conflict/index.js"), "1");

    let (dirs, _) = harvest(&src, &out).unwrap();

    assert!(matches!(&dirs[..], [Dir::Tree { path, .. }] if path == "apps/web/node_modules"));
    assert!(
        out.join("apps/web/node_modules/conflict/index.js")
            .is_file()
    );
}

/// A stand-in for the package manager, run in the build sandbox, that leaves
/// bun's layout and writes down what it saw.
const FAKE_BUN: &str = r#"set -eu
SRC="$(ls -A /s/src | tr '\n' ' ')"
mkdir -p node_modules/.bun/left-pad@1.3.0/node_modules/left-pad apps/web/node_modules node_modules/.facts
echo 'module.exports = 1' > node_modules/.bun/left-pad@1.3.0/node_modules/left-pad/index.js
ln -s .bun/left-pad@1.3.0/node_modules/left-pad node_modules/left-pad
ln -s ../../../node_modules/.bun/left-pad@1.3.0/node_modules/left-pad apps/web/node_modules/left-pad
{
  echo "args: $*"
  echo "pwd: $(pwd)"
  echo "home: $HOME"
  echo "root: $(ls / | tr '\n' ' ')"
  echo "src: $SRC"
  echo "lock: $(cat /s/src/bun.lock)"
  echo "env: $(env | cut -d= -f1 | sort | tr '\n' ' ')"
  echo "built: $(date +%s%N)"
} > node_modules/.facts/seen.txt
"#;

fn facts(cache: &Path) -> String {
    std::fs::read_to_string(cache.join("node_modules/.facts/seen.txt")).unwrap()
}

fn put(of: &Dependencies) -> (&str, &Extras) {
    match of {
        Dependencies::Ready { key, extras } => (key, extras),
        other => panic!("{other:?}"),
    }
}

fn base() -> Inputs {
    inputs(&[
        ("package.json", ROOT),
        ("bun.lock", "lock one"),
        ("apps/web/package.json", r#"{"name":"web"}"#),
    ])
}

#[tokio::test]
async fn a_build_runs_in_its_own_sandbox_and_is_kept_whole_under_its_key() {
    if !sandbox() {
        return;
    }
    let scratch = Scratch::new("build");
    let bun = scratch.program("bun", FAKE_BUN);
    let cache = Cache::new(scratch.0.join("deps"));
    let wanted = base();
    let key = key(DepsKind::Bun, &wanted);

    let built = cache.ensure(&repo(), &wanted, &config(&bun)).await;

    let (found, extras) = put(&built);
    assert_eq!(found, key);
    let at = scratch.0.join("deps/acme/widgets").join(&key);
    assert!(at.join(MANIFEST).is_file());
    assert!(
        std::fs::read_dir(at.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("tmp-")),
        "the scratch directory is gone"
    );
    let seen = facts(&at);
    assert!(
        seen.contains("args: install --frozen-lockfile --ignore-scripts --no-progress --registry https://registry.npmjs.org/"),
        "{seen}"
    );
    assert!(
        seen.contains("pwd: /s/src") && seen.contains("home: /s/home"),
        "{seen}"
    );
    assert!(
        seen.contains("src: apps bun.lock package.json"),
        "the base commit's files and no bunfig.toml or .npmrc: {seen}"
    );
    assert!(seen.contains("lock: lock one"), "{seen}");
    let root = seen.lines().find(|line| line.starts_with("root:")).unwrap();
    for host in ["home", "root", "srv", "mnt", "var", "opt"] {
        assert!(!root.split_whitespace().any(|name| name == host), "{root}");
    }
    let env = seen.lines().find(|line| line.starts_with("env:")).unwrap();
    assert_eq!(
        env.split_whitespace().skip(1).collect::<Vec<_>>(),
        [
            "BUN_INSTALL_CACHE_DIR",
            "BUN_TMPDIR",
            "CI",
            "DO_NOT_TRACK",
            "HOME",
            "NO_COLOR",
            "PATH",
            "PWD",
            "TMPDIR"
        ],
        "a token or a home of the host's does not reach it"
    );
    assert!(extras.binds.iter().any(|bind| matches!(
        bind,
        Bind::ReadOnly { source, target }
            if *source == at.join("node_modules") && target == Path::new("/work/node_modules")
    )));
    assert!(
        !at.join("apps").exists(),
        "a directory of links is made again in each sandbox, not kept"
    );
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(at.join(MANIFEST)).unwrap()).unwrap();
    assert!(manifest.dirs.iter().any(
        |dir| matches!(dir, Dir::Links { path, links } if path == "apps/web/node_modules" && links.len() == 1)
    ));
}

#[tokio::test]
async fn the_same_lockfile_is_built_once_and_a_new_one_has_a_cache_of_its_own() {
    if !sandbox() {
        return;
    }
    let scratch = Scratch::new("reuse");
    let bun = scratch.program("bun", FAKE_BUN);
    let cache = Cache::new(scratch.0.join("deps"));
    let wanted = base();
    let first = cache.ensure(&repo(), &wanted, &config(&bun)).await;
    let (key_one, _) = put(&first);
    let at = scratch.0.join("deps/acme/widgets").join(key_one);
    let built = facts(&at);

    let again = cache.ensure(&repo(), &wanted, &config(&bun)).await;

    assert_eq!(first, again);
    assert_eq!(
        facts(&at),
        built,
        "a cache that is there is not built again"
    );

    let newer = inputs(&[
        ("package.json", ROOT),
        ("bun.lock", "lock two"),
        ("apps/web/package.json", r#"{"name":"web"}"#),
    ]);
    let second = cache.ensure(&repo(), &newer, &config(&bun)).await;
    let (key_two, _) = put(&second);
    assert_ne!(key_one, key_two, "the key changes when the lockfile does");
    let other = scratch.0.join("deps/acme/widgets").join(key_two);
    assert!(facts(&other).contains("lock: lock two"));
    assert!(
        facts(&at).contains("lock: lock one"),
        "the first stays as it was"
    );
}

#[tokio::test]
async fn a_cache_whose_manifest_is_gone_is_built_again_not_trusted() {
    if !sandbox() {
        return;
    }
    let scratch = Scratch::new("damaged");
    let bun = scratch.program("bun", FAKE_BUN);
    let cache = Cache::new(scratch.0.join("deps"));
    let wanted = base();
    let first = cache.ensure(&repo(), &wanted, &config(&bun)).await;
    let (key, _) = put(&first);
    let at = scratch.0.join("deps/acme/widgets").join(key);
    let built = facts(&at);
    std::fs::remove_file(at.join(MANIFEST)).unwrap();

    let again = cache.ensure(&repo(), &wanted, &config(&bun)).await;

    put(&again);
    assert_ne!(facts(&at), built, "it was built again");
    assert!(at.join(MANIFEST).is_file());
}

#[tokio::test]
async fn a_failed_build_is_unavailable_with_the_reason_and_leaves_nothing_behind() {
    if !sandbox() {
        return;
    }
    let scratch = Scratch::new("failed");
    let bun = scratch.program(
        "bun",
        "echo 'error: lockfile had changes, but lockfile is frozen' >&2\nexit 1",
    );
    let cache = Cache::new(scratch.0.join("deps"));
    let wanted = base();

    let failed = cache.ensure(&repo(), &wanted, &config(&bun)).await;

    let Dependencies::Unavailable { why } = &failed else {
        panic!("{failed:?}")
    };
    assert!(
        why.contains("the install failed") && why.contains("lockfile is frozen"),
        "{why}"
    );
    assert!(!why.contains('\n'), "one line, for a prompt");
    let repo_dir = scratch.0.join("deps/acme/widgets");
    assert_eq!(
        std::fs::read_dir(&repo_dir).unwrap().count(),
        0,
        "no cache and no scratch directory"
    );
    let again = cache.ensure(&repo(), &wanted, &config(&bun)).await;
    let Dependencies::Unavailable { why: soon } = &again else {
        panic!("{again:?}")
    };
    assert!(soon.contains("not tried again yet"), "{soon}");
}

#[tokio::test]
async fn a_missing_package_manager_is_unavailable_and_says_where_it_looked() {
    let scratch = Scratch::new("nobun");
    let cache = Cache::new(scratch.0.join("deps"));
    let gone = scratch.0.join("no-such-bun");

    let found = cache.ensure(&repo(), &base(), &config(&gone)).await;

    let Dependencies::Unavailable { why } = &found else {
        panic!("{found:?}")
    };
    assert!(why.contains("no-such-bun"), "{why}");
}

#[tokio::test]
async fn a_build_past_its_time_is_killed() {
    if !sandbox() {
        return;
    }
    let scratch = Scratch::new("slow");
    let bun = scratch.program("bun", "sleep 30");
    let cache = Cache::new(scratch.0.join("deps"));
    let mut slow = config(&bun);
    slow.timeout_secs = 1;
    let started = Instant::now();

    let found = cache.ensure(&repo(), &base(), &slow).await;

    let Dependencies::Unavailable { why } = &found else {
        panic!("{found:?}")
    };
    assert!(why.contains("took longer than 1 seconds"), "{why}");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "it did not wait for the sleep"
    );
}

#[tokio::test]
async fn a_build_past_its_disk_cap_is_killed() {
    if !sandbox() {
        return;
    }
    let scratch = Scratch::new("big");
    let bun = scratch.program("bun", "head -c 3000000 /dev/zero > /s/src/blob\nsleep 30");
    let cache = Cache::new(scratch.0.join("deps"));
    let mut small = config(&bun);
    small.max_bytes = 1_000_000;

    let found = cache.ensure(&repo(), &base(), &small).await;

    let Dependencies::Unavailable { why } = &found else {
        panic!("{found:?}")
    };
    assert!(why.contains("more than 1000000 bytes of disk"), "{why}");
}

#[tokio::test]
async fn an_install_that_made_no_node_modules_is_unavailable_not_ready() {
    if !sandbox() {
        return;
    }
    let scratch = Scratch::new("empty");
    let bun = scratch.program("bun", "true");
    let cache = Cache::new(scratch.0.join("deps"));

    let found = cache.ensure(&repo(), &base(), &config(&bun)).await;

    let Dependencies::Unavailable { why } = &found else {
        panic!("{found:?}")
    };
    assert!(why.contains("no node_modules directory"), "{why}");
}

#[test]
fn the_manifest_of_a_build_survives_a_round_trip_through_json() {
    let manifest = Manifest {
        version: VERSION,
        kind: "bun".into(),
        key: "a".repeat(64),
        built_ms: 7,
        bytes: 9,
        dirs: vec![
            Dir::Tree {
                path: "node_modules".into(),
                bytes: 5,
            },
            Dir::Links {
                path: "apps/web/node_modules".into(),
                links: vec![Entry {
                    path: "x".into(),
                    target: "../y".into(),
                }],
            },
        ],
    };
    let text = serde_json::to_string(&manifest).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["dirs"][1],
        json!({"mount":"links","path":"apps/web/node_modules","links":[{"path":"x","target":"../y"}]})
    );
    assert_eq!(serde_json::from_str::<Manifest>(&text).unwrap(), manifest);
}
