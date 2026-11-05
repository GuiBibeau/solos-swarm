//! pstack, Lauren Tan's MIT-licensed agent skills, vendored at
//! `vendor/pstack/` and pinned by its MANIFEST: the check of the tree, the
//! skill block a worker's first prompt enters, the system prompt section,
//! and the reader behind the `read` tool.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

pub const ADAPTER: &str = include_str!("../../docs/harness/solos-swarm.md");

pub const ADAPTER_CHECKED_AGAINST: &str = "0.15.36";

pub const VENDORED: &str = "vendor/pstack";

pub const MOUNT: &str = "/pstack";

const MANIFEST: &str = "MANIFEST";
const PAGE_LINES: usize = 2_000;
const PAGE_BYTES: usize = 256 * 1024;
const PAGE_ENTRIES: usize = 2_000;
/// `read` is also served `/work`, which a worker writes, so what it scans to
/// answer is bounded: a file's first 32 MiB are counted for its `total`, and
/// a directory's first 20,000 entries are listed from.
const SCAN_BYTES: usize = 32 * 1024 * 1024;
const SCAN_ENTRIES: usize = 10 * PAGE_ENTRIES;
const SNIFF_BYTES: usize = 8 * 1024;
#[cfg(target_os = "linux")]
const OPEN_TRIES: u32 = 64;

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The vendored tree, every file checked against its MANIFEST.
#[derive(Debug)]
pub struct Tree {
    root: PathBuf,
    version: String,
    digest: String,
    roles: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Skill {
    name: String,
    dir: String,
    body: String,
}

impl Tree {
    /// Open `root` and check it: the MANIFEST's file lines hash to its tree
    /// digest, every file has a line with its sha256 and every line a file,
    /// nothing in it is a link or a special file, and `plugin.json` names the
    /// MANIFEST's version, the one the adapter was checked against.
    pub fn open(root: &Path) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot find pstack at {}", root.display()))?;
        let text = std::fs::read_to_string(root.join(MANIFEST))
            .with_context(|| format!("cannot read {}", root.join(MANIFEST).display()))?;
        let manifest = Manifest::parse(&text)?;
        let mut unlisted = manifest.files;
        check_dir(&root, "", &mut unlisted)?;
        if let Some(path) = unlisted.keys().next() {
            bail!("{path} is in MANIFEST but not in the tree");
        }
        let plugin: Value = serde_json::from_slice(&std::fs::read(root.join("plugin.json"))?)
            .context("plugin.json is not JSON")?;
        let version = plugin["version"]
            .as_str()
            .context("plugin.json names no version")?;
        ensure!(
            version == manifest.version,
            "plugin.json says pstack {version} and MANIFEST says {}",
            manifest.version
        );
        ensure!(
            version == ADAPTER_CHECKED_AGAINST,
            "pstack {version} is vendored and the solos-swarm adapter was checked against \
             {ADAPTER_CHECKED_AGAINST}: read pstack's diff against docs/harness/solos-swarm.md, \
             then bump ADAPTER_CHECKED_AGAINST in src/agent/pstack.rs"
        );
        let setup = std::fs::read_to_string(root.join("skills/setup-pstack/SKILL.md"))?;
        let roles = roles(&setup).context("skills/setup-pstack/SKILL.md")?;
        Ok(Self {
            root,
            version: manifest.version,
            digest: manifest.digest,
            roles,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn roles(&self) -> &[String] {
        &self.roles
    }

    /// `skills/<dir>/SKILL.md`, read as Pi 1.1.0 reads a skill: its name
    /// from the frontmatter, else `dir`, and its body as `stripFrontmatter`
    /// leaves it, trimmed.
    pub fn skill(&self, dir: &str) -> Result<Skill> {
        ensure!(
            !dir.is_empty() && dir != "." && dir != ".." && !dir.contains(['/', '\0']),
            "{dir:?} is not a skill: a skill is one directory under skills/"
        );
        let path = self.root.join("skills").join(dir).join("SKILL.md");
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!("pstack has no skill {dir}: cannot read {}", path.display())
        })?;
        let (yaml, body) = strip_frontmatter(&text);
        let name = match yaml {
            Some(yaml) if !yaml.is_empty() => {
                let meta: serde_yaml::Value = serde_yaml::from_str(&yaml)
                    .with_context(|| format!("the frontmatter of {}", path.display()))?;
                meta.get("name")
                    .and_then(serde_yaml::Value::as_str)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
            }
            _ => None,
        };
        Ok(Skill {
            name: name.unwrap_or_else(|| dir.to_owned()),
            dir: dir.to_owned(),
            body: js_trim(&body).to_owned(),
        })
    }

    /// The first prompt of a session that enters `skill`: the block Pi
    /// 1.1.0's `/skill:` expansion builds (agent-session.js:1676-1678) with
    /// the tree at [`MOUNT`], then `text`. The host builds it instead of
    /// sending `/skill:`, so Pi and Claude Code sessions get the same bytes.
    pub fn enter(skill: &Skill, text: &str) -> String {
        let Skill { name, dir, body } = skill;
        let block = format!(
            "<skill name=\"{name}\" location=\"{MOUNT}/skills/{dir}/SKILL.md\">\n\
             References are relative to {MOUNT}/skills/{dir}.\n\n{body}\n</skill>"
        );
        match js_trim(text) {
            "" => block,
            text => format!("{block}\n\n{text}"),
        }
    }
}

struct Manifest {
    version: String,
    digest: String,
    files: BTreeMap<String, String>,
}

impl Manifest {
    fn parse(text: &str) -> Result<Self> {
        let (header, lines) = text
            .split_once("\n\n")
            .context("MANIFEST has no empty line after its header")?;
        let header: Vec<&str> = header.split('\n').collect();
        let [version, source, tree] = header[..] else {
            bail!("MANIFEST's header is not three lines: pstack, source and tree");
        };
        let version = version
            .strip_prefix("pstack ")
            .context("MANIFEST's first line is not `pstack <version>`")?;
        ensure!(
            source.starts_with("source "),
            "MANIFEST's second line is not `source <url>`"
        );
        let digest = tree
            .strip_prefix("tree sha256:")
            .context("MANIFEST's third line is not `tree sha256:<hex>`")?;
        let actual = sha256_hex(lines.as_bytes());
        ensure!(
            actual == digest,
            "MANIFEST's file lines hash to {actual}, not to its tree sha256:{digest}"
        );
        let mut files = BTreeMap::new();
        for line in lines.split_terminator('\n') {
            let (sum, path) = line
                .split_once("  ")
                .filter(|(sum, _)| {
                    sum.len() == 64 && sum.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                })
                .with_context(|| format!("MANIFEST line {line:?} is not `<sha256>  <path>`"))?;
            ensure!(
                files.insert(path.to_owned(), sum.to_owned()).is_none(),
                "MANIFEST names {path} twice"
            );
        }
        Ok(Self {
            version: version.to_owned(),
            digest: digest.to_owned(),
            files,
        })
    }
}

fn check_dir(dir: &Path, prefix: &str, unlisted: &mut BTreeMap<String, String>) -> Result<()> {
    let mut entries = std::fs::read_dir(dir)
        .with_context(|| format!("cannot list {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        let path = format!("{prefix}{}", name.to_string_lossy());
        ensure!(name.to_str().is_some(), "{path} is not a UTF-8 name");
        let kind = entry.file_type()?;
        if kind.is_dir() {
            check_dir(&entry.path(), &format!("{path}/"), unlisted)?;
        } else if kind.is_file() {
            if path == MANIFEST {
                continue;
            }
            let sum = sha256_hex(&std::fs::read(entry.path())?);
            let Some(listed) = unlisted.remove(&path) else {
                bail!("{path} is not in MANIFEST");
            };
            ensure!(
                listed == sum,
                "{path} differs from MANIFEST: its sha256 is {sum}, not {listed}"
            );
        } else if kind.is_symlink() {
            bail!("{path} is a symbolic link");
        } else {
            bail!("{path} is not a regular file or a directory");
        }
    }
    Ok(())
}

fn roles(setup: &str) -> Result<Vec<String>> {
    let mut lines = setup
        .lines()
        .skip_while(|line| !line.starts_with("# budget:"))
        .skip(1);
    let mut roles = Vec::new();
    loop {
        match lines.next() {
            None => bail!("no rule block from a `# budget:` line to a closing fence"),
            Some("```") => break,
            Some(line) => roles.push(
                line.split_once(": ")
                    .map(|(role, _)| role.to_owned())
                    .with_context(|| format!("the rule line {line:?} names no role"))?,
            ),
        }
    }
    ensure!(!roles.is_empty(), "the rule block names no role");
    Ok(roles)
}

/// Pi's `stripFrontmatter`: without a BOM and with `\n` line ends, the YAML
/// between a leading `---` and the next `\n---`, and the text after it.
/// Without both, no YAML and the whole text.
fn strip_frontmatter(text: &str) -> (Option<String>, String) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return (None, normalized);
    }
    let Some(end) = normalized[3..].find("\n---").map(|at| at + 3) else {
        return (None, normalized);
    };
    let yaml = normalized.get(4..end).unwrap_or_default().to_owned();
    (Some(yaml), normalized[end + 4..].to_owned())
}

/// JavaScript's `String.prototype.trim`, which Pi uses: Rust's white space
/// less U+0085, and U+FEFF.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

#[derive(Debug)]
pub struct Pstack {
    tree: Tree,
    entry: Skill,
}

impl Pstack {
    /// The tree at `<pi_root>/vendor/pstack`, and its skill `entry`. Any
    /// difference from the MANIFEST refuses.
    pub fn open(pi_root: &Path, entry: &str) -> Result<Self> {
        let tree = Tree::open(&pi_root.join(VENDORED))?;
        let entry = tree.skill(entry)?;
        Ok(Self { tree, entry })
    }

    /// A worker's first prompt: the entry skill, then `text`.
    pub fn enter(&self, text: &str) -> String {
        Tree::enter(&self.entry, text)
    }

    /// What a worker's system prompt says about pstack: where it is, the
    /// adapter, and the pstack-models rule, every role on `model`.
    pub fn section(&self, model: &str, run_code_has_pstack: bool) -> String {
        let run = if run_code_has_pstack {
            format!("run_code has the same tree at {MOUNT}, so its bash scripts run there.")
        } else {
            "pstack's scripts do not run here, so skip a step that needs one with the line from \
             the adapter."
                .to_owned()
        };
        let mut text = format!(
            "pstack. You work the pstack way. pstack is Lauren Tan's set of agent skills, at \
             {MOUNT} (pstack {}, read-only). Read its files with read. {run} pstack is written \
             in Cursor's names, and the solos-swarm adapter below maps them to this host. \
             Wherever a skill tells you to read the Claude Code adapter, use this one instead. \
             pstack's reply rules are written for a reviewer who reads a report, and the person \
             here reads chat, so the adapter's \"Talking to people\" wins wherever they differ.\
             \n\n{}\n\nYour pstack-models rule:\n",
            self.tree.version(),
            ADAPTER.trim()
        );
        for role in self.tree.roles() {
            let _ = writeln!(text, "{role}: {model}");
        }
        let _ = write!(
            text,
            "Every role runs on your own model, {model}. This host runs one model family."
        );
        text
    }

    pub fn root(&self) -> &Path {
        self.tree.root()
    }

    pub fn version(&self) -> &str {
        self.tree.version()
    }

    pub fn digest(&self) -> &str {
        self.tree.digest()
    }
}

/// Why `read` refused, by what the caller does about it.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not a path `read` serves, or an offset or limit out of range.
    Invalid(String),
    /// The path leaves its root, through `..` or a link.
    Leaves(String),
    Missing(String),
    /// Binary or not UTF-8, or neither a file nor a directory.
    NotText(String),
    /// This host cannot read it: no `openat2`, or an I/O error.
    Host(String),
}

/// Reads beneath fixed roots, each a virtual path and a directory opened
/// once. The kernel resolves every path beneath its root's descriptor
/// (`openat2` with `RESOLVE_BENEATH`), so neither `..` nor a link, even one
/// swapped in during the call, reaches past it.
#[derive(Debug)]
pub struct Reader {
    roots: Vec<(&'static str, OwnedFd)>,
}

impl Reader {
    pub fn new(roots: &[(&'static str, &Path)]) -> Result<Self> {
        let roots = roots
            .iter()
            .map(|(at, dir)| {
                let file =
                    File::open(dir).with_context(|| format!("cannot open {}", dir.display()))?;
                ensure!(
                    file.metadata()?.is_dir(),
                    "{} is not a directory",
                    dir.display()
                );
                Ok((*at, OwnedFd::from(file)))
            })
            .collect::<Result<_>>()?;
        Ok(Self { roots })
    }

    /// A text file's lines from `offset` (from 1), at most `limit` of them,
    /// or a directory's entries.
    pub fn read(
        &self,
        path: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> std::result::Result<Value, Refused> {
        let (from, limit) = (offset.unwrap_or(1), limit.unwrap_or(PAGE_LINES));
        if from == 0 || limit == 0 {
            return Err(Refused::Invalid("offset and limit count from 1".into()));
        }
        let (root, rel) = self.resolve(path)?;
        let file = open_beneath(root, rel, path)?;
        let meta = file
            .metadata()
            .map_err(|err| Refused::Host(format!("cannot stat {path}: {err}")))?;
        if meta.is_dir() {
            list(path, &file)
        } else if meta.is_file() {
            page(path, file, from, limit.min(PAGE_LINES))
        } else {
            Err(Refused::NotText(format!(
                "{path} is not a file or a directory"
            )))
        }
    }

    fn resolve<'a>(
        &self,
        path: &'a str,
    ) -> std::result::Result<(&(&'static str, OwnedFd), &'a str), Refused> {
        if path.contains('\0') {
            return Err(Refused::Invalid("a path holds no NUL".into()));
        }
        for root in &self.roots {
            let Some(rest) = path.strip_prefix(root.0) else {
                continue;
            };
            if rest.is_empty() || rest.starts_with('/') {
                let rel = rest.trim_start_matches('/');
                return Ok((root, if rel.is_empty() { "." } else { rel }));
            }
        }
        let roots: Vec<&str> = self.roots.iter().map(|(at, _)| *at).collect();
        Err(Refused::Invalid(format!(
            "{path} is not under {}: read takes an absolute path there",
            roots.join(" or ")
        )))
    }
}

#[cfg(target_os = "linux")]
fn open_beneath(
    (at, root): &(&'static str, OwnedFd),
    rel: &str,
    path: &str,
) -> std::result::Result<File, Refused> {
    use std::os::fd::FromRawFd;
    let rel =
        std::ffi::CString::new(rel).map_err(|_| Refused::Invalid("a path holds no NUL".into()))?;
    // SAFETY: open_how is a plain C struct of integers, for which all zeroes
    // is a valid value; it is non_exhaustive, so this is how one is built.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    // O_NONBLOCK keeps the open of a FIFO from waiting for a writer.
    how.flags = (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NONBLOCK) as u64;
    how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS;
    let mut tries = 0;
    let fd = loop {
        // SAFETY: root is an open descriptor this Reader owns, rel is a
        // NUL-terminated string and how an initialized open_how, both alive
        // for the call, and the size passed is open_how's.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root.as_raw_fd(),
                rel.as_ptr(),
                &raw const how,
                std::mem::size_of::<libc::open_how>(),
            )
        };
        if fd >= 0 {
            break fd;
        }
        let err = std::io::Error::last_os_error();
        // A rename or mount anywhere on the host during a `..` step makes
        // the kernel refuse rather than risk an escape; openat2(2) says to
        // try again.
        if err.raw_os_error() == Some(libc::EAGAIN) && tries < OPEN_TRIES {
            tries += 1;
            continue;
        }
        return Err(match err.raw_os_error() {
            Some(libc::EXDEV | libc::ELOOP) => Refused::Leaves(format!("{path} leaves {at}")),
            Some(libc::ENOENT | libc::ENOTDIR) => {
                Refused::Missing(format!("{path} does not exist"))
            }
            Some(libc::ENOSYS) => Refused::Host("read needs Linux 5.6 or later".into()),
            _ => Refused::Host(format!("cannot open {path}: {err}")),
        });
    };
    // SAFETY: openat2 returned a new descriptor that nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd as std::os::fd::RawFd) })
}

#[cfg(not(target_os = "linux"))]
fn open_beneath(
    _: &(&'static str, OwnedFd),
    _: &str,
    _: &str,
) -> std::result::Result<File, Refused> {
    Err(Refused::Host("read needs Linux 5.6 or later".into()))
}

fn list(path: &str, dir: &File) -> std::result::Result<Value, Refused> {
    let failed = |err: std::io::Error| Refused::Host(format!("cannot list {path}: {err}"));
    // A directory of millions of entries is read for its first SCAN_ENTRIES
    // alone, and only the PAGE_ENTRIES that are listed are looked at.
    let mut entries = Vec::new();
    let mut unscanned = false;
    for entry in std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd())).map_err(failed)? {
        if entries.len() == SCAN_ENTRIES {
            unscanned = true;
            break;
        }
        let entry = entry.map_err(failed)?;
        let kind = entry.file_type().map_err(failed)?;
        entries.push((
            entry.file_name().to_string_lossy().into_owned(),
            kind,
            entry,
        ));
    }
    entries.sort_by(|(a, ..), (b, ..)| a.cmp(b));
    let truncated = unscanned || entries.len() > PAGE_ENTRIES;
    let mut listed = Vec::new();
    for (name, kind, entry) in entries.into_iter().take(PAGE_ENTRIES) {
        listed.push(if kind.is_file() {
            let bytes = entry.metadata().map_err(failed)?.len();
            json!({"name": name, "kind": "file", "bytes": bytes})
        } else {
            let kind = if kind.is_dir() {
                "dir"
            } else if kind.is_symlink() {
                "link"
            } else {
                "other"
            };
            json!({"name": name, "kind": kind})
        });
    }
    Ok(json!({"path": path, "entries": listed, "truncated": truncated}))
}

fn page(path: &str, file: File, from: usize, limit: usize) -> std::result::Result<Value, Refused> {
    let failed = |err: std::io::Error| Refused::Host(format!("cannot read {path}: {err}"));
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut page = Page::new(from, from.saturating_add(limit - 1));
    let (mut line, mut sniffed, mut scanned, mut partial, mut capped) = (1, 0, 0, false, false);
    loop {
        let chunk = reader.fill_buf().map_err(failed)?;
        if chunk.is_empty() {
            break;
        }
        let look = &chunk[..chunk.len().min(SNIFF_BYTES - sniffed)];
        if look.contains(&0) {
            return Err(Refused::NotText(format!("{path} is binary")));
        }
        sniffed += look.len();
        let mut rest = chunk;
        while !rest.is_empty() {
            let end = rest.iter().position(|&byte| byte == b'\n');
            let (piece, more) = rest.split_at(end.map_or(rest.len(), |at| at + 1));
            page.take(line, piece);
            if end.is_some() {
                line += 1;
            }
            partial = end.is_none();
            rest = more;
        }
        let read = chunk.len();
        reader.consume(read);
        scanned += read;
        if scanned >= SCAN_BYTES {
            capped = !reader.fill_buf().map_err(failed)?.is_empty();
            break;
        }
    }
    page.end();
    let total = line - 1 + usize::from(partial);
    if capped && partial && page.to == line {
        // The scan stopped inside this line, so the page ends short of it.
        page.cut = true;
    }
    if total > 0 && from > total {
        return Err(Refused::Invalid(if capped {
            format!(
                "{path} has at least {total} lines; offset {from} is past the first {} MiB, \
                 which is as far as read goes into a file",
                SCAN_BYTES >> 20
            )
        } else {
            format!("{path} has {total} lines; offset {from} is past its end")
        }));
    }
    let mut text = page.text;
    if page.cut
        && let Err(err) = std::str::from_utf8(&text)
        && err.error_len().is_none()
    {
        text.truncate(err.valid_up_to());
    }
    let lines = String::from_utf8(text)
        .map_err(|_| Refused::NotText(format!("{path} is not UTF-8 text")))?;
    let mut reply = json!({
        "path": path,
        "from": from,
        "to": page.to,
        "cut": page.cut,
        "lines": lines,
    });
    // Past SCAN_BYTES the count is a floor, and it is named as one.
    reply[if capped { "total_at_least" } else { "total" }] = json!(total);
    Ok(reply)
}

struct Page {
    from: usize,
    last: usize,
    text: Vec<u8>,
    open: Option<OpenLine>,
    to: usize,
    full: bool,
    cut: bool,
}

struct OpenLine {
    line: usize,
    start: usize,
}

impl Page {
    fn new(from: usize, last: usize) -> Self {
        Self {
            from,
            last,
            text: Vec::new(),
            open: None,
            to: from - 1,
            full: false,
            cut: false,
        }
    }

    fn take(&mut self, n: usize, piece: &[u8]) {
        if self.full || n < self.from || n > self.last {
            return;
        }
        let start = match &self.open {
            Some(open) if open.line == n => open.start,
            _ => {
                let start = self.text.len();
                self.text.extend_from_slice(format!("{n:>6}\t").as_bytes());
                self.open = Some(OpenLine { line: n, start });
                start
            }
        };
        self.text.extend_from_slice(piece);
        if self.text.len() > PAGE_BYTES {
            if n == self.from {
                self.text.truncate(PAGE_BYTES);
                self.cut = true;
                self.to = n;
            } else {
                self.text.truncate(start);
            }
            self.full = true;
            self.open = None;
        } else if piece.ends_with(b"\n") {
            self.to = n;
            self.open = None;
        }
    }

    fn end(&mut self) {
        if let Some(open) = self.open.take() {
            self.to = open.line;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ADAPTER, Pstack, Tree, VENDORED, sha256_hex};
    use crate::agent::broker::WORKER_TOOLS;
    use serde_json::Value;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    const ROLES: [&str; 17] = [
        "feature, refactoring",
        "bug-fix",
        "perf-issue",
        "hillclimb",
        "judgment and prose",
        "hardest tasks",
        "how explorer",
        "how explainer",
        "why investigators",
        "why synthesizer",
        "reflect tooling",
        "reflect judgment, divergent, synthesizer",
        "arena runners",
        "arena cross-judge pool",
        "swarm workers",
        "architect runners",
        "interrogate reviewers",
    ];

    fn checkout() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    fn vendored() -> Pstack {
        Pstack::open(checkout(), "poteto-mode").unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pstack-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    fn refused_after(change: impl Fn(&Path), named: &str) {
        let dir = scratch("tree");
        let tree = dir.join("pstack");
        copy_tree(&checkout().join(VENDORED), &tree);
        Tree::open(&tree).expect("the unchanged copy opens");
        change(&tree);
        let refused = format!("{:#}", Tree::open(&tree).unwrap_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(refused.contains(named), "{refused}");
    }

    #[test]
    fn the_vendored_tree_opens_at_its_version_with_setup_pstacks_roles() {
        let tree = Tree::open(&checkout().join(VENDORED)).unwrap();
        assert_eq!(tree.version(), "0.15.36");
        assert_eq!(tree.roles(), ROLES);
        let manifest = std::fs::read_to_string(tree.root().join("MANIFEST")).unwrap();
        let lines = manifest.split_once("\n\n").unwrap().1;
        assert_eq!(tree.digest(), sha256_hex(lines.as_bytes()));
    }

    #[test]
    fn a_changed_missing_or_extra_file_is_refused_by_its_path() {
        let skill = "skills/poteto-mode/SKILL.md";
        refused_after(
            |tree| {
                let mut bytes = std::fs::read(tree.join(skill)).unwrap();
                bytes[40] ^= 1;
                std::fs::write(tree.join(skill), bytes).unwrap();
            },
            &format!("{skill} differs from MANIFEST"),
        );
        refused_after(
            |tree| std::fs::remove_file(tree.join("skills/how/SKILL.md")).unwrap(),
            "skills/how/SKILL.md is in MANIFEST but not in the tree",
        );
        refused_after(
            |tree| std::fs::write(tree.join("skills/how/notes.md"), "mine").unwrap(),
            "skills/how/notes.md is not in MANIFEST",
        );
    }

    #[test]
    fn entering_poteto_mode_is_pis_own_expansion_byte_for_byte() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/pstack-entry.json")).unwrap();
        let tree = Tree::open(&checkout().join(VENDORED)).unwrap();
        let skill = tree.skill(fixture["entry"].as_str().unwrap()).unwrap();
        let text = fixture["text"].as_str().unwrap();
        let entered = Tree::enter(&skill, text);
        let digest = sha256_hex(entered.as_bytes());
        assert_eq!(
            digest, fixture["sha256"],
            "tests/fixtures/pstack-entry.json holds Pi's digest; this build's is {digest}"
        );
        assert!(
            entered.starts_with(
                "<skill name=\"poteto-mode\" location=\"/pstack/skills/poteto-mode/SKILL.md\">\n\
                 References are relative to /pstack/skills/poteto-mode.\n\n# Poteto mode\n"
            ),
            "{entered}"
        );
        assert!(
            entered.ends_with(&format!("</skill>\n\n{}", text.trim())),
            "{entered}"
        );
        assert_eq!(
            Tree::enter(&skill, " \n"),
            entered.split_once("\n\nTicket ").unwrap().0
        );
    }

    #[test]
    fn a_skill_names_one_directory_under_skills() {
        let tree = Tree::open(&checkout().join(VENDORED)).unwrap();
        for dir in [
            "",
            ".",
            "..",
            "../skills/how",
            "poteto-mode/playbooks",
            "no-such-skill",
        ] {
            assert!(tree.skill(dir).is_err(), "{dir:?}");
        }
    }

    #[test]
    fn the_section_carries_the_adapter_and_a_one_family_rule() {
        let pstack = vendored();
        let section = pstack.section("deepseek-flash", true);
        assert!(
            section.starts_with(
                "pstack. You work the pstack way. pstack is Lauren Tan's set of agent skills, at \
                 /pstack (pstack 0.15.36, read-only). Read its files with read. run_code has the \
                 same tree at /pstack, so its bash scripts run there. pstack is written in \
                 Cursor's names"
            ),
            "{section}"
        );
        assert!(section.contains(&format!("\n\n{}\n\n", ADAPTER.trim())));
        let (_, rule) = section.split_once("\nYour pstack-models rule:\n").unwrap();
        let lines: Vec<&str> = rule.lines().collect();
        let roles: Vec<String> = ROLES
            .iter()
            .map(|role| format!("{role}: deepseek-flash"))
            .collect();
        assert_eq!(lines[..ROLES.len()], roles);
        assert_eq!(
            lines[ROLES.len()..],
            ["Every role runs on your own model, deepseek-flash. This host runs one model family."]
        );

        let elsewhere = pstack.section("deepseek-flash", false);
        assert!(elsewhere.contains(
            "Read its files with read. pstack's scripts do not run here, so skip a step that \
             needs one with the line from the adapter. pstack is written"
        ));
        assert!(!elsewhere.contains("run_code has the same tree"));
    }

    #[test]
    fn the_adapter_names_every_worker_tool_no_later_one_and_only_paths_the_tree_has() {
        let ticked: BTreeSet<&str> = ADAPTER.split('`').skip(1).step_by(2).collect();
        for tool in WORKER_TOOLS {
            assert!(ticked.contains(tool), "the adapter never names `{tool}`");
        }
        let later = [
            "delegate",
            "delegate_return",
            "check",
            "pr_publish",
            "pr_view",
            "pr_reply",
            "pr_merge",
        ];
        for tool in later {
            if WORKER_TOOLS.contains(&tool) {
                continue;
            }
            assert!(
                !ticked.contains(tool),
                "the adapter names `{tool}`, which no worker has"
            );
        }
        let tree = checkout().join(VENDORED);
        let paths: Vec<&str> = ticked
            .iter()
            .filter_map(|token| token.strip_prefix("/pstack/"))
            .filter(|path| !path.contains('<') && !path.contains("..."))
            .collect();
        assert!(!paths.is_empty());
        for path in paths {
            assert!(
                tree.join(path).exists(),
                "the adapter names /pstack/{path}, which the tree lacks"
            );
        }
    }

    /// The adapter says how pstack's reply rules read in a chat. It quotes each
    /// rule it overrides, so a pstack update that rewords one makes this fail
    /// and sends the maintainer to the adapter, instead of letting the worker
    /// follow a rule the adapter no longer matches.
    #[test]
    fn the_chat_section_overrides_only_reply_rules_pstack_still_has() {
        let skill = std::fs::read_to_string(
            checkout()
                .join(VENDORED)
                .join("skills/poteto-mode/SKILL.md"),
        )
        .unwrap();
        let (_, chat) = ADAPTER.split_once("## Talking to people").unwrap();
        let (chat, _) = chat.split_once("\n## ").unwrap();
        for rule in [
            "Your first message on a task",
            "Open each final report with",
            "your final reply included",
            "name each principle that shaped a decision",
            "every section the playbook's reply names stays",
            "Every claim carries its evidence or its label in the same sentence",
            "Writing the reply",
            "Frame impact for the consumer and the maintainer",
        ] {
            assert!(skill.contains(rule), "pstack no longer says {rule:?}");
            assert!(
                chat.contains(rule),
                "the chat section never quotes {rule:?}"
            );
        }
        for told in [
            "your `ready` text is posted to them as a message in their thread",
            "Open with the answer or the outcome. Never restate the goal or the task.",
            "Name one only when the person asks why you decided something.",
            "no greeting, no recap, no new introduction",
            "A short question gets a short answer",
        ] {
            assert!(ADAPTER.contains(told), "the adapter does not say {told:?}");
        }
        // The `Skipped in solos-swarm` line is operator bookkeeping: it goes in a
        // `progress` report, which no person is told, never in a reply.
        assert!(chat.contains("Send them in a `worker_report` with status `progress`"));
        assert!(
            ADAPTER.contains("put one line in a `progress` report, `Skipped in solos-swarm:"),
            "the Not on this host section sends its line to the reply"
        );
        assert!(
            !ADAPTER.contains("in your next report") && !ADAPTER.contains("in your report"),
            "a bookkeeping line is sent to the person"
        );
    }

    #[cfg(target_os = "linux")]
    mod reader {
        use super::super::{MOUNT, PAGE_ENTRIES, Reader, Refused, SCAN_BYTES, SCAN_ENTRIES};
        use super::{scratch, vendored};
        use serde_json::{Value, json};
        use std::os::unix::fs::symlink;
        use std::path::PathBuf;

        fn vendored_reader() -> Reader {
            Reader::new(&[(MOUNT, vendored().root())]).unwrap()
        }

        fn world() -> (PathBuf, PathBuf, Reader) {
            let dir = scratch("reader");
            let root = dir.join("root");
            std::fs::create_dir_all(root.join("sub")).unwrap();
            std::fs::write(dir.join("outside.txt"), "secret\n").unwrap();
            std::fs::write(root.join("a.txt"), "one\ntwo\nthree").unwrap();
            std::fs::write(root.join("sub/b.md"), "# b\n").unwrap();
            std::fs::write(root.join("bin.dat"), b"text\0more").unwrap();
            symlink("/etc/passwd", root.join("passwd")).unwrap();
            symlink("/proc/self/environ", root.join("environ")).unwrap();
            symlink("hop2", root.join("hop1")).unwrap();
            symlink("sub/../../outside.txt", root.join("hop2")).unwrap();
            symlink("sub/b.md", root.join("inside")).unwrap();
            let made = std::process::Command::new("mkfifo")
                .arg(root.join("pipe"))
                .status()
                .unwrap();
            assert!(made.success());
            let reader = Reader::new(&[(MOUNT, root.as_path())]).unwrap();
            (dir, root, reader)
        }

        fn bounds(page: &Value) -> Value {
            json!([page["from"], page["to"], page["total"], page["cut"]])
        }

        fn stripped(lines: &Value) -> String {
            lines
                .as_str()
                .unwrap()
                .split_inclusive('\n')
                .map(|line| line.split_once('\t').unwrap().1)
                .collect()
        }

        #[test]
        fn nothing_outside_the_root_is_read_however_the_path_gets_there() {
            let (dir, _root, reader) = world();
            let read = |path: &str| reader.read(path, None, None);
            for path in [
                "/pstack/../outside.txt",
                "/pstack/sub/../../outside.txt",
                "/pstack/passwd",
                "/pstack/hop1",
                "/pstack/environ",
            ] {
                assert!(
                    matches!(read(path), Err(Refused::Leaves(_))),
                    "{path}: {:?}",
                    read(path)
                );
            }
            for path in [
                "/proc/self/environ",
                "/etc/passwd",
                "/pstackx/a.txt",
                "pstack/a.txt",
                "/work/x",
            ] {
                assert!(
                    matches!(read(path), Err(Refused::Invalid(_))),
                    "{path}: {:?}",
                    read(path)
                );
            }
            assert!(matches!(read("/pstack/nothing"), Err(Refused::Missing(_))));
            assert!(matches!(read("/pstack/a.txt/x"), Err(Refused::Missing(_))));
            assert_eq!(
                read("/pstack/inside").unwrap()["lines"],
                "     1\t# b\n",
                "a link that stays inside is followed"
            );
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn a_fifo_and_a_binary_file_are_refused_at_once() {
            let (dir, _root, reader) = world();
            for path in ["/pstack/pipe", "/pstack/bin.dat"] {
                let read = reader.read(path, None, None);
                assert!(matches!(read, Err(Refused::NotText(_))), "{path}: {read:?}");
            }
            let image = vendored_reader().read("/pstack/docs/guide/images/router.jpg", None, None);
            assert!(matches!(image, Err(Refused::NotText(_))), "{image:?}");
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn a_directory_lists_its_entries_by_name() {
            let (dir, _root, reader) = world();
            let listed = reader.read("/pstack/", None, None).unwrap();
            let names: Vec<(&str, &str)> = listed["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| (e["name"].as_str().unwrap(), e["kind"].as_str().unwrap()))
                .collect();
            assert_eq!(
                names,
                [
                    ("a.txt", "file"),
                    ("bin.dat", "file"),
                    ("environ", "link"),
                    ("hop1", "link"),
                    ("hop2", "link"),
                    ("inside", "link"),
                    ("passwd", "link"),
                    ("pipe", "other"),
                    ("sub", "dir"),
                ]
            );
            assert_eq!(listed["entries"][0]["bytes"], 13);
            assert_eq!(listed["truncated"], false);
            let skills = vendored_reader()
                .read("/pstack/skills", None, None)
                .unwrap();
            assert!(
                skills["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["name"] == "poteto-mode" && e["kind"] == "dir")
            );
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn a_long_file_comes_in_pages_whose_lines_are_the_file() {
            let (dir, root, reader) = world();
            let text: String = (1..=3_000).map(|n| format!("line {n}\n")).collect();
            std::fs::write(root.join("long.txt"), &text).unwrap();
            let first = reader.read("/pstack/long.txt", None, None).unwrap();
            assert_eq!(bounds(&first), json!([1, 2_000, 3_000, false]));
            let second = reader.read("/pstack/long.txt", Some(2_001), None).unwrap();
            assert_eq!(bounds(&second), json!([2_001, 3_000, 3_000, false]));
            assert_eq!(
                stripped(&first["lines"]) + &stripped(&second["lines"]),
                text
            );
            let some = reader.read("/pstack/a.txt", Some(2), Some(1)).unwrap();
            assert_eq!(some["lines"], "     2\ttwo\n");
            let last = reader.read("/pstack/a.txt", Some(3), None).unwrap();
            assert_eq!(bounds(&last), json!([3, 3, 3, false]));
            assert_eq!(last["lines"], "     3\tthree");
            let past = reader.read("/pstack/a.txt", Some(4), None);
            assert_eq!(
                past,
                Err(Refused::Invalid(
                    "/pstack/a.txt has 3 lines; offset 4 is past its end".into()
                ))
            );
            assert!(matches!(
                reader.read("/pstack/a.txt", Some(0), None),
                Err(Refused::Invalid(_))
            ));
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn a_page_stops_before_256_kib_and_cuts_only_a_first_line_that_alone_passes_it() {
            let (dir, root, reader) = world();
            let row = format!("{}\n", "é".repeat(511));
            let text = row.repeat(300);
            std::fs::write(root.join("wide.txt"), &text).unwrap();
            let page = reader.read("/pstack/wide.txt", None, None).unwrap();
            let to = page["to"].as_u64().unwrap() as usize;
            assert!((1..300).contains(&to), "{page}");
            assert!(page["lines"].as_str().unwrap().len() <= 256 * 1024);
            assert_eq!(bounds(&page), json!([1, to, 300, false]));
            assert_eq!(stripped(&page["lines"]), row.repeat(to));
            let rest = reader.read("/pstack/wide.txt", Some(to + 1), None).unwrap();
            assert_eq!(rest["from"], to + 1);

            let huge = "é".repeat(200_000);
            std::fs::write(root.join("huge.txt"), format!("{huge}\nnext\n")).unwrap();
            let cut = reader.read("/pstack/huge.txt", None, None).unwrap();
            assert_eq!(bounds(&cut), json!([1, 1, 2, true]));
            let lines = cut["lines"].as_str().unwrap();
            assert!(lines.len() <= 256 * 1024 && lines.len() > 256 * 1024 - 2);
            assert!(huge.starts_with(lines.split_once('\t').unwrap().1));
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn a_file_is_counted_only_as_far_as_the_scan_goes_and_the_total_says_at_least() {
            let (dir, root, reader) = world();
            let lines = SCAN_BYTES / 2;
            std::fs::write(root.join("exact.txt"), "x\n".repeat(lines)).unwrap();
            let exact = reader.read("/pstack/exact.txt", None, None).unwrap();
            assert_eq!(
                exact["total"], lines,
                "a file of exactly the bound is counted"
            );
            assert!(exact.get("total_at_least").is_none());

            std::fs::write(root.join("big.txt"), "x\n".repeat(lines + lines / 4)).unwrap();
            let big = reader.read("/pstack/big.txt", None, None).unwrap();
            assert_eq!(big["total_at_least"], lines);
            assert!(big.get("total").is_none(), "{big}");
            assert_eq!(big["to"], 2_000);
            let deep = reader
                .read("/pstack/big.txt", Some(lines - 1), Some(2))
                .unwrap();
            assert_eq!(
                deep["lines"],
                format!("{}\tx\n{lines}\tx\n", lines - 1),
                "a page inside the scanned part is served"
            );
            let past = reader.read("/pstack/big.txt", Some(lines + 1), None);
            assert!(
                matches!(&past, Err(Refused::Invalid(why))
                    if why.contains(&format!("has at least {lines} lines"))),
                "{past:?}"
            );

            let mut straddles = "x\n".repeat(lines - 1);
            straddles.push_str("yyyyyyyy\nz\n");
            std::fs::write(root.join("straddles.txt"), straddles).unwrap();
            let last = reader
                .read("/pstack/straddles.txt", Some(lines), Some(5))
                .unwrap();
            assert_eq!(last["lines"], format!("{lines}\tyy"));
            assert_eq!(
                last["cut"], true,
                "the line the scan stopped in is cut short"
            );
            assert_eq!(last["total_at_least"], lines);
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn a_directory_is_listed_from_its_first_entries_only() {
            let (dir, root, reader) = world();
            let many = root.join("many");
            std::fs::create_dir(&many).unwrap();
            for n in 0..SCAN_ENTRIES + 5 {
                std::fs::File::create(many.join(format!("f{n:06}"))).unwrap();
            }
            let listed = reader.read("/pstack/many", None, None).unwrap();
            let names = listed["entries"].as_array().unwrap();
            assert_eq!(names.len(), PAGE_ENTRIES);
            assert_eq!(listed["truncated"], true);
            assert!(
                names
                    .windows(2)
                    .all(|pair| pair[0]["name"].as_str() < pair[1]["name"].as_str()),
                "what is listed is in order"
            );
            assert!(names.iter().all(|entry| entry["bytes"] == 0));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
