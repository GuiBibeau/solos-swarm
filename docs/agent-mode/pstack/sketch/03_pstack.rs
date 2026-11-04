//! Lives at: src/agent/pstack.rs (new), with `vendor/pstack/` (the tree),
//! `docs/harness/solos-swarm.md` (the adapter) and `scripts/pstack-sync`.
//!
//! What pstack is to the host: a pinned, read-only tree of text, an entry
//! skill pasted into the first prompt, an adapter and a model rule in the
//! system prompt, and a reader. Nothing here interprets a playbook. The one
//! place the host parses pstack is metadata it would otherwise have to copy:
//! skill frontmatter (names and descriptions for the Router), agent
//! definitions, and the role labels in setup-pstack's rule block. Copying
//! them would be a hand-synced list; deriving them means a pstack update
//! that adds a role or an agent needs no code change.
//!
//! The tree is vendored verbatim at `vendor/pstack/` in the solos-swarm
//! checkout, the same checkout `--pi-root` already names for `node_modules`
//! and `extensions/broker.ts`. So the shipped checkout pins both the host and
//! the method, and `ship.sh` on the verify box ships both. It holds pstack's
//! `LICENSE` (MIT, Lauren Tan) unchanged, its `plugin.json` (the version),
//! and `MANIFEST`, written by `scripts/pstack-sync`: one `sha256  path` line
//! per file, the upstream ref it came from, and the tree digest (the sha256
//! of the MANIFEST's file lines). `agent serve` refuses to start when a file
//! differs from MANIFEST, so the text a worker reads is the text a reviewer
//! approved in the sync PR.

use crate::agent::broker::registry::Seat;
use anyhow::Result;
use solos_agent_core::change::{Entered, Entry, RepoName};
use solos_agent_core::trail::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The adapter, compiled in. It is the host half of the method, versioned
/// with the tools it names. Rendered verbatim into every worker's and
/// delegate's system prompt.
pub const ADAPTER: &str = include_str!("../../docs/harness/solos-swarm.md");

/// The pstack version the adapter was last checked against. The sync script
/// refuses to finish, and a test fails, while `vendor/pstack/plugin.json`
/// names another: a pstack update is not done until a person has read its
/// diff against the adapter and bumped this.
pub const ADAPTER_CHECKED_AGAINST: &str = "0.15.36";

/// The vendored tree, verified and indexed once at start.
pub struct Tree {
    /// `<pi_root>/vendor/pstack`, canonical.
    root: PathBuf,
    version: String,
    digest: Digest,
    /// Every `skills/<name>/SKILL.md`, by name.
    skills: BTreeMap<String, Skill>,
    /// `agents/<name>.md`, by name.
    agents: BTreeMap<String, Agent>,
    /// The role labels of setup-pstack's rule block, such as `how explorer`
    /// and `arena cross-judge pool`. A label with a comma (`feature,
    /// refactoring`) is one role.
    roles: BTreeSet<String>,
}

pub struct Skill {
    pub name: String,
    pub description: String,
    pub disable_model_invocation: bool,
    /// The body without frontmatter, trimmed, as Pi's expansion uses it.
    body: String,
}

pub struct Agent {
    pub name: String,
    /// The definition's body without frontmatter.
    body: String,
}

impl Tree {
    /// Open `root`, check every file against MANIFEST (a missing, extra or
    /// changed file is refused with its path), and index skills, agents and
    /// roles. Fails when `plugin.json`'s version is not
    /// [`ADAPTER_CHECKED_AGAINST`].
    pub fn open(root: &Path) -> Result<Self> {
        todo!()
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn digest(&self) -> &Digest {
        &self.digest
    }

    /// The skill `entry` names, if the tree has one.
    pub fn entry(&self, entry: &Entry) -> Option<&Skill> {
        self.skills.get(entry.as_str())
    }

    pub fn agent(&self, name: &str) -> Option<&Agent> {
        self.agents.get(name)
    }

    pub fn is_role(&self, label: &str) -> bool {
        self.roles.contains(label)
    }

    /// The host path of a `/pstack/...` path, for the reader and for
    /// `run_code`'s mount.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The first prompt of a session that enters `entry`: the skill block in
    /// exactly the shape Pi's `/skill:` expansion builds
    /// (`agent-session.js:1676-1677`), then `text`.
    ///
    /// ```text
    /// <skill name="poteto-mode" location="/pstack/skills/poteto-mode/SKILL.md">
    /// References are relative to /pstack/skills/poteto-mode.
    ///
    /// # Poteto mode
    /// …
    /// </skill>
    ///
    /// Ticket tk_5f1e9a0c2b7d4e11: Fix the flaky retry test
    /// …
    /// ```
    ///
    /// The host builds it instead of sending `/skill:poteto-mode`, because
    /// Pi expands only at the very start of a prompt and only skills it
    /// loaded, and Claude Code has no such expansion. One builder gives both
    /// harnesses the same bytes, needs no `--skill` in either argv, and
    /// leaves the Pi launch's golden test unedited.
    pub fn enter(&self, entry: &Skill, text: &str) -> String {
        todo!()
    }

    /// What `pstack.entries` offers the Router: each entry's name and its
    /// frontmatter description, pstack's own words.
    pub fn entry_cards(&self, entries: &[Entry]) -> Vec<(String, String)> {
        todo!()
    }

    pub fn entered(&self, entry: &Entry) -> Entered {
        todo!("version, tree digest, Digest::of(ADAPTER)")
    }
}

// ---------------------------------------------------------------------------
// Models and the rule.
// ---------------------------------------------------------------------------

/// A model family, derived from the admitted model id, never configured:
/// `deepseek*` and `deepseek-v4-1-flash-uncensored*` are `deepseek`,
/// `zai-org-glm-5*` is `glm`. The admitted catalog is closed in code
/// (`gateway::upstream::venice_model`), so this match is total over it, and
/// a new admitted model fails this function's test until it names a family.
pub fn family(model: &str) -> &'static str {
    todo!()
}

/// One model a session may run on, by the alias pstack's rule uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alias {
    pub name: String,
    pub terms: crate::agent::broker::ModelTerms,
    pub family: &'static str,
}

/// The host's models: the workers' own (`executor.model`, under its own
/// name) and `pstack.models`. Every alias's provider has a credential, or
/// config load fails.
pub struct Models {
    pub lead: Alias,
    aliases: BTreeMap<String, Alias>,
}

impl Models {
    /// An alias, or the refusal pstack's fallback rule reads: "model x is
    /// not valid here; the valid models are a, b".
    pub fn get(&self, name: &str) -> Result<&Alias, String> {
        todo!()
    }

    /// Whether more than one family is configured, which decides
    /// `MergeTerms::other_than`.
    pub fn families(&self) -> BTreeSet<&'static str> {
        todo!()
    }
}

/// The worker's `pstack-models` rule: one line per role, each a list of
/// aliases, in pstack's grammar (setup-pstack step 5). Built from
/// `pstack.roles`; a role the operator left out takes pstack's default slug
/// mapped by the adapter's Models table (a `claude-*` default becomes the
/// `judgment and prose` line's alias, a `grok-*` default the `feature,
/// refactoring` line's), and with no roles at all every line is the lead's
/// alias: one family, which pstack handles as its own fallback.
pub struct Rule {
    lines: Vec<(String, Vec<String>)>,
}

impl Rule {
    /// Refuses a label that is not a role of the vendored setup-pstack, and
    /// an alias that is not in `models`.
    pub fn new(tree: &Tree, models: &Models, roles: &BTreeMap<String, Vec<String>>) -> Result<Self> {
        todo!()
    }

    /// The rule as pstack writes it, after a header line:
    ///
    /// ```text
    /// # budget: large (xhigh)
    /// feature, refactoring: deepseek-flash
    /// judgment and prose: glm-5
    /// how explorer: deepseek-flash
    /// how explainer: glm-5
    /// arena runners: deepseek-flash, glm-5
    /// arena cross-judge pool: glm-5, deepseek-flash
    /// swarm workers: deepseek-flash
    /// …
    /// ```
    pub fn render(&self) -> String {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// The system prompt section.
// ---------------------------------------------------------------------------

/// What the seat's prompt says about where it works.
pub struct Place<'a> {
    pub seat: Seat,
    /// The Ticket's repository, its base and the operator's notes on it
    /// (toolchain paths, how its tests run).
    pub repo: Option<(&'a RepoName, &'a str, &'a str)>,
    pub checks: &'a [String],
    pub me: &'a Alias,
}

/// The pstack section appended to the existing `system_prompt()` output
/// (pi_executor.rs:82-114) for a Ticket whose Plan has a method, and to every
/// delegate's prompt:
///
/// ```text
/// pstack. You work the pstack way. Its skills are at /pstack (pstack 0.15.36, read-only): read
/// them with read, run their bash scripts with run_code. pstack is written in Cursor's names;
/// the solos-swarm adapter below maps them. Wherever a skill tells you to read the Claude Code
/// adapter, use this one instead.
///
/// <the adapter, verbatim>
///
/// Your pstack-models rule:
/// <Rule::render()>
/// You run on deepseek-flash (family deepseek). The families here are deepseek and glm.
///
/// You are the lead of this ticket. | You are a delegate: …
/// Repository: acme/widgets, base main. /work is a clone at the base's tip, with its history;
/// commit there. <operator notes>. Checks: unit, ci.
/// ```
///
/// The system prompt is the same for every turn of a session, so the
/// provider's prefix cache holds it (DeepSeek bills a cached input token at
/// 2% of an uncached one).
pub fn section(tree: &Tree, rule: &Rule, models: &Models, place: &Place<'_>) -> String {
    todo!()
}

// ---------------------------------------------------------------------------
// The reader: the `read` tool.
// ---------------------------------------------------------------------------

/// The roots a seat may read, each a virtual path and the host directory
/// behind it. The same virtual paths are the seat's `run_code` mounts, so a
/// path `read` prints works in a command, and the reverse.
///
/// | Root | Lead | Delegate | Host directory | In run_code |
/// |---|---|---|---|---|
/// | `/pstack` | read | read | `vendor/pstack` | read-only |
/// | `/work` | read | read | `<ticket>/work`, or a fork `<ticket>/children/<n>/work` | the lead and a fork write; a `read` delegate does not |
/// | `/method` | read | read | `<ticket>/method` | the lead writes; read-only to delegates |
/// | `/children` | read | none | `<ticket>/children` | read-only |
/// | `/host` | read | read | `<ticket>/host` | read-only |
///
/// `/host` is the one place the host writes for a sandbox: check logs, the
/// base bundle, `repo.json` and this Ticket's transcripts (bound per file,
/// never the Ticket directory, which holds Claude Code's token files). The
/// host never writes into a directory a sandbox can write.
pub struct Reader {
    roots: Vec<(&'static str, PathBuf)>,
}

/// One answer of `read`.
pub enum Read {
    Lines { from: usize, to: usize, total: usize, text: String, truncated: bool },
    Entries(Vec<(String, EntryKind, Option<u64>)>),
}

pub enum EntryKind {
    File,
    Dir,
    Link,
}

impl Reader {
    pub fn for_seat(roots: Vec<(&'static str, PathBuf)>) -> Self {
        Self { roots }
    }

    /// Resolve `path` under its root and read it. The root is opened once,
    /// and the rest of the path is resolved with `openat2(RESOLVE_BENEATH |
    /// RESOLVE_NO_MAGICLINKS)`, so a symbolic link a sandbox planted in
    /// /work, or one swapped in between a check and the open, cannot reach
    /// a host file: the kernel refuses the escape, not a path comparison
    /// that a rename can race. Text only (a NUL in the first 8 KiB is
    /// binary), at most 2,000 lines and 256 KiB per call.
    pub fn read(&self, path: &str, offset: usize, limit: usize) -> Result<Read, String> {
        todo!()
    }
}

// Tests (src/agent/pstack.rs and tests/agent_pstack.rs):
// - `Tree::open` on the vendored tree succeeds; a changed byte, a missing
//   file and an extra file are each refused with the path; a plugin.json
//   version other than ADAPTER_CHECKED_AGAINST fails.
// - `enter` equals Pi's own expansion of the same SKILL.md, byte for byte,
//   checked by running Pi's `_expandSkillCommand` on the vendored file under
//   node in tests/broker.test.ts.
// - roles parsed from the vendored setup-pstack are exactly its seventeen
//   lines; `Rule::new` refuses `how critics` (a retired role) and an
//   unknown alias, and names the valid ones.
// - `family` covers every model the gateway admits.
// - the adapter drift test: every backticked host tool in the adapter's
//   tables is a registry tool, and every worker tool appears in the
//   adapter; every `/pstack/...` path the adapter names exists in the tree.
// - Reader: `..` past a root, an absolute symlink in /work to /etc/passwd,
//   a relative symlink chain out of /work, `/proc/self/environ`, a FIFO and
//   a binary file are refused; a 3,000-line file comes back in two calls; a
//   directory lists its entries; a delegate cannot read /children.
