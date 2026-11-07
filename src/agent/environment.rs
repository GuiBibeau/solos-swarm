//! Where a worker's `run_code` and background jobs run: this host's
//! sandbox ([`CodeExecutor`], unchanged) or a named remote node. A Ticket's
//! Plan may name its environment; one that names none gets the executor's
//! default, and `local` is always this host. The broker resolves a worker's
//! environment on each call, so a node that went offline is a clear tool
//! error, and the executor holds a Launch until the node is back.

use crate::code::{CodeExecutor, Extras, OutputBounds, Started};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use solos_agent_core::change::CommitSha;
use solos_agent_core::model::{Plan, TicketId};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// This host's sandbox, by name.
pub const LOCAL: &str = "local";

/// The Ticket a command runs for, and its `/work` on this host.
#[derive(Clone, Copy, Debug)]
pub struct Work<'a> {
    pub ticket: &'a TicketId,
    pub dir: &'a Path,
    /// What this host adds to the Ticket's sandbox (a repository's installed
    /// dependencies). A node's own sandbox ignores it: a repository Ticket
    /// never runs on one.
    pub extras: &'a Extras,
}

/// The environment cannot take a command now, but may later: a node that
/// is not connected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offline(pub String);

impl std::fmt::Display for Offline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Offline {}

/// Runs a worker's commands. Each starts on its own task and returns at
/// once; its [`Started`] reads, waits and stops the same wherever it runs.
/// An [`Offline`] error means the environment may take it later.
pub trait ExecutionEnvironment: Send + Sync {
    fn ready(&self) -> Result<(), Offline>;
    /// `run_code`'s request.
    fn run_code(&self, work: Work<'_>, args: &Value) -> Result<Started>;
    /// A `job_start` command.
    fn job(
        &self,
        work: Work<'_>,
        code: &str,
        timeout: Duration,
        bounds: OutputBounds,
    ) -> Result<Started>;
}

/// This host's sandbox, in the Ticket's directory under the state dir.
pub struct Local(pub Arc<CodeExecutor>);

impl ExecutionEnvironment for Local {
    fn ready(&self) -> Result<(), Offline> {
        Ok(())
    }
    fn run_code(&self, work: Work<'_>, args: &Value) -> Result<Started> {
        self.0.start_in_with(work.dir, work.extras, args)
    }
    fn job(
        &self,
        work: Work<'_>,
        code: &str,
        timeout: Duration,
        bounds: OutputBounds,
    ) -> Result<Started> {
        self.0
            .start_job_in_with(work.dir, work.extras, code, timeout, bounds)
    }
}

/// The environment `plan` names, if any.
pub fn named(plan: &Plan) -> Option<&str> {
    match plan {
        Plan::Session { environment, .. } => environment.as_deref(),
        Plan::Swarm { .. } => None,
    }
}

/// The exit code the harvest script ends with when the tree holds no commit
/// beyond the base.
const NO_COMMITS: i64 = 3;

/// What a fresh sandbox made of a worker's tree. The bundle is a file the
/// sandbox wrote, so it is untrusted data until the forge has judged it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle {
    pub path: PathBuf,
    /// The head the sandbox says the bundle holds. Advisory.
    pub head: CommitSha,
    /// `git status --porcelain | wc -l`, advisory, told to the worker so it
    /// is not surprised that uncommitted work is not delivered.
    pub uncommitted: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Harvested {
    Bundle(Bundle),
    /// The tree holds nothing beyond the commit the clone started from.
    NoCommits,
    /// The sandbox could not read the tree, in these words.
    Failed(String),
}

/// What a sandbox wrote in a file of at most a few bytes, trimmed: nothing for a
/// link (which could point at any file the host can read), a directory or a
/// file that is large.
fn small_file(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 64 {
        return None;
    }
    Some(std::fs::read_to_string(path).ok()?.trim().to_owned())
}

/// The host's script for `Environments::harvest`, run in a sandbox with no
/// network and an empty environment but the one below. It uses nothing the
/// worker could have set: git is told not to run a file system monitor or a
/// hook, to ignore replace refs and to read no global or system config. Its
/// own output is advisory, since a hostile `.git` can make the sandbox's git
/// lie but cannot make the host's believe it.
const HARVEST_SCRIPT: &str = r#"set -eu
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_NO_REPLACE_OBJECTS=1 \
       GIT_TERMINAL_PROMPT=0 LC_ALL=C
g() { git -c core.fsmonitor=false -c core.hooksPath=/dev/null -C /work "$@"; }
head=$(g rev-parse --verify 'HEAD^{commit}')
count=$(g rev-list --count "$BASE..HEAD")
[ "$count" -gt 0 ] || { echo "no commits since $BASE" >&2; exit 3; }
g bundle create /out/proposal.bundle "$BASE..HEAD" 2>/dev/null
printf '%s\n' "$head" > /out/head
n=$(g --no-optional-locks status --porcelain 2>/dev/null | wc -l || true)
printf '%s\n' "${n:-0}" > /out/uncommitted
"#;

/// The remote nodes a host knows, and which one a Plan that names none
/// runs on.
#[derive(Clone, Default)]
pub struct Nodes {
    /// `None` or [`LOCAL`]: this host.
    pub default: Option<String>,
    pub nodes: BTreeMap<String, Arc<dyn ExecutionEnvironment>>,
}

/// Whether a Ticket's worker can start now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launchable {
    Now,
    /// Its node is offline: the Launch waits for it.
    Later(String),
    /// Its Plan names an environment this host does not have.
    Never(String),
}

/// Every environment a worker may run on.
#[derive(Clone, Default)]
pub struct Environments {
    /// `None`: this host offers no `run_code`.
    local: Option<Arc<dyn ExecutionEnvironment>>,
    /// The same sandbox, for the host's own scripts on a worker's tree.
    code: Option<Arc<CodeExecutor>>,
    remote: Nodes,
}

impl Environments {
    pub fn new(local: Option<Arc<CodeExecutor>>, remote: Nodes) -> Self {
        Self {
            local: local
                .clone()
                .map(|code| Arc::new(Local(code)) as Arc<dyn ExecutionEnvironment>),
            code: local,
            remote,
        }
    }

    /// Whether a Plan may name `name`.
    pub fn knows(&self, name: &str) -> bool {
        name == LOCAL || self.remote.nodes.contains_key(name)
    }

    /// Every name a Plan may give.
    pub fn names(&self) -> Vec<&str> {
        std::iter::once(LOCAL)
            .chain(self.remote.nodes.keys().map(String::as_str))
            .collect()
    }

    fn name<'a>(&'a self, named: Option<&'a str>) -> &'a str {
        named.or(self.remote.default.as_deref()).unwrap_or(LOCAL)
    }

    /// Whether a worker whose Plan names `named` runs on this host.
    pub fn is_local(&self, named: Option<&str>) -> bool {
        self.name(named) == LOCAL
    }

    /// Where a worker whose Plan names `named` runs its commands: `None`
    /// when that is this host and it offers no `run_code`.
    pub fn get(
        &self,
        named: Option<&str>,
    ) -> Result<Option<&Arc<dyn ExecutionEnvironment>>, String> {
        let name = self.name(named);
        if name == LOCAL {
            return Ok(self.local.as_ref());
        }
        self.remote.nodes.get(name).map(Some).ok_or_else(|| {
            format!(
                "there is no environment {name} here; the environments are {}",
                self.names().join(", ")
            )
        })
    }

    /// Whether such a worker has `run_code`.
    pub fn offers_run_code(&self, named: Option<&str>) -> bool {
        matches!(self.get(named), Ok(Some(_)))
    }

    /// Bundle the commits a worker made in `work` since `base`, in a fresh
    /// sandbox this host builds (`work` read-only, `out` a fresh writable
    /// directory), never in the worker's own. What the script printed is
    /// advisory: the bundle is data that `Forge::admit` judges again in the
    /// host's mirror.
    pub async fn harvest(
        &self,
        named: Option<&str>,
        work: &Path,
        out: &Path,
        base: &CommitSha,
    ) -> Result<Harvested> {
        if !self.is_local(named) {
            bail!("a ticket on a repository runs on this host only");
        }
        let code = self
            .code
            .as_ref()
            .context("this host offers no run_code to harvest a change in")?;
        let receipt = code
            .run_on_tree(work, out, &[("BASE", base.as_str())], HARVEST_SCRIPT)
            .await?;
        if receipt["timed_out"] == true {
            return Ok(Harvested::Failed(
                "reading your commits took longer than a minute".into(),
            ));
        }
        match receipt["exit_code"].as_i64() {
            Some(0) => {}
            Some(NO_COMMITS) => return Ok(Harvested::NoCommits),
            _ => {
                let said = receipt["stderr"].as_str().unwrap_or_default().trim();
                let last = said.lines().last().unwrap_or("git failed in your tree");
                return Ok(Harvested::Failed(last.chars().take(300).collect()));
            }
        }
        let read = |name: &str| small_file(&out.join(name));
        let Some(head) = read("head") else {
            return Ok(Harvested::Failed("the sandbox wrote no head".into()));
        };
        let Ok(head) = CommitSha::try_from(head) else {
            return Ok(Harvested::Failed("the sandbox wrote no commit id".into()));
        };
        let uncommitted = read("uncommitted")
            .and_then(|text| text.parse().ok())
            .unwrap_or(0);
        Ok(Harvested::Bundle(Bundle {
            path: out.join("proposal.bundle"),
            head,
            uncommitted,
        }))
    }

    pub fn launchable(&self, named: Option<&str>) -> Launchable {
        match self.get(named) {
            Err(why) => Launchable::Never(why),
            Ok(None) => Launchable::Now,
            Ok(Some(environment)) => match environment.ready() {
                Ok(()) => Launchable::Now,
                Err(Offline(why)) => Launchable::Later(why),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Environments, ExecutionEnvironment, LOCAL, Launchable, Nodes, Offline, Work};
    use crate::code::{OutputBounds, Started};
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[derive(Default)]
    struct Switch(AtomicBool);

    impl ExecutionEnvironment for Switch {
        fn ready(&self) -> Result<(), Offline> {
            if self.0.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(Offline("node laptop is offline".into()))
            }
        }
        fn run_code(&self, _: Work<'_>, _: &Value) -> anyhow::Result<Started> {
            anyhow::bail!("not in this test")
        }
        fn job(
            &self,
            _: Work<'_>,
            _: &str,
            _: Duration,
            _: OutputBounds,
        ) -> anyhow::Result<Started> {
            anyhow::bail!("not in this test")
        }
    }

    #[test]
    fn a_plan_runs_where_it_names_or_on_the_default_and_waits_for_an_offline_node() {
        let laptop = Arc::new(Switch::default());
        let nodes = Nodes {
            default: Some("laptop".into()),
            nodes: BTreeMap::from([(
                "laptop".to_owned(),
                laptop.clone() as Arc<dyn ExecutionEnvironment>,
            )]),
        };
        let environments = Environments::new(None, nodes);
        assert_eq!(environments.names(), [LOCAL, "laptop"]);
        assert!(environments.knows("local") && environments.knows("laptop"));
        assert!(!environments.knows("desktop"));

        assert!(!environments.is_local(None) && environments.is_local(Some(LOCAL)));
        assert!(environments.offers_run_code(None));
        assert!(
            !environments.offers_run_code(Some(LOCAL)),
            "this host has no run_code"
        );
        assert_eq!(
            environments.launchable(None),
            Launchable::Later("node laptop is offline".into())
        );
        assert_eq!(environments.launchable(Some(LOCAL)), Launchable::Now);
        laptop.0.store(true, Ordering::SeqCst);
        assert_eq!(environments.launchable(Some("laptop")), Launchable::Now);
        let Launchable::Never(why) = environments.launchable(Some("desktop")) else {
            panic!("an unknown node launched")
        };
        assert!(why.contains("local, laptop"), "{why}");

        let here = Environments::default();
        assert!(here.is_local(None) && !here.offers_run_code(None));
        assert_eq!(here.launchable(None), Launchable::Now);
    }
}
