//! Where a worker's `run_code` and background jobs run: this host's
//! sandbox ([`CodeExecutor`], unchanged) or a named remote node. A Ticket's
//! Plan may name its environment; one that names none gets the executor's
//! default, and `local` is always this host. The broker resolves a worker's
//! environment on each call, so a node that went offline is a clear tool
//! error, and the executor holds a Launch until the node is back.

use crate::code::{CodeExecutor, OutputBounds, Started};
use anyhow::Result;
use serde_json::Value;
use solos_agent_core::model::{Plan, TicketId};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// This host's sandbox, by name.
pub const LOCAL: &str = "local";

/// The Ticket a command runs for, and its `/work` on this host.
#[derive(Clone, Copy, Debug)]
pub struct Work<'a> {
    pub ticket: &'a TicketId,
    pub dir: &'a Path,
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
        self.0.start_in(work.dir, args)
    }
    fn job(
        &self,
        work: Work<'_>,
        code: &str,
        timeout: Duration,
        bounds: OutputBounds,
    ) -> Result<Started> {
        self.0.start_job_in(work.dir, code, timeout, bounds)
    }
}

/// The environment `plan` names, if any.
pub fn named(plan: &Plan) -> Option<&str> {
    match plan {
        Plan::Session { environment, .. } => environment.as_deref(),
        Plan::Swarm { .. } => None,
    }
}

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
    remote: Nodes,
}

impl Environments {
    pub fn new(local: Option<Arc<CodeExecutor>>, remote: Nodes) -> Self {
        Self {
            local: local.map(|code| Arc::new(Local(code)) as Arc<dyn ExecutionEnvironment>),
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
