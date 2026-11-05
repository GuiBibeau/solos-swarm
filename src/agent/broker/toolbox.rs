//! The tools a principal holds. Each registry tool is bound, once, to the
//! host resources its handler runs on: a seat's toolbox is built when the seat
//! is admitted, from its environment and this host. A tool whose resource
//! this host lacks is refused with the reason and never built, so a handler
//! is only reached with what it needs.

use super::browser::{Browse, BrowserParts, BrowserTool};
use super::registry::{ROUTER, RouterOp, Tool, ToolSpec, WORKER, WorkerOp};
use super::{AgentBroker, HOST_MOUNT, VaultParts, WORK_MOUNT, WorkerSeat};
use crate::agent::environment::ExecutionEnvironment;
use crate::agent::pstack::{self, Reader};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

pub type Env = Arc<dyn ExecutionEnvironment>;

/// A host resource a tool can need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    RunCode,
    Browser,
    Vault,
    Read,
}

impl Resource {
    /// Why a tool that needs this is refused on a host without it.
    pub const fn unavailable(self) -> &'static str {
        match self {
            Self::RunCode => "run_code is not available on this host",
            Self::Browser => "no browser where this ticket runs",
            Self::Vault => "no vault on this host",
            Self::Read => "this ticket has neither pstack nor a repository to read",
        }
    }
}

/// One tool a principal holds: its spec, and the handler bound to its resources.
struct Bound<C> {
    spec: ToolSpec,
    call: C,
    /// The table's `repeatable`.
    repeatable: bool,
}

/// The tools of one principal's table. Each is either held, or refused with
/// the reason its resource is missing.
pub struct Toolbox<C> {
    held: Vec<Bound<C>>,
    refused: Vec<(&'static str, String)>,
}

/// What a tool name means to one principal.
pub enum Lookup<'a, C> {
    Held(&'a C),
    Refused(&'a str),
    Unknown,
}

impl<C> Toolbox<C> {
    /// Binds every tool of `table` that `build` can serve, in the table's
    /// order. `why` gives the refusal for a tool whose `build` names a
    /// resource this host lacks.
    pub fn build<O: Copy>(
        table: &[Tool<O>],
        build: impl Fn(O) -> Result<C, Resource>,
        why: impl Fn(Resource) -> String,
    ) -> Self {
        let mut toolbox = Self::default();
        for tool in table {
            match build(tool.op) {
                Ok(call) => toolbox.held.push(Bound {
                    spec: tool.spec(),
                    call,
                    repeatable: tool.repeatable,
                }),
                Err(resource) => toolbox.refused.push((tool.name, why(resource))),
            }
        }
        toolbox
    }

    /// The specs of the held tools, in the table's order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.held.iter().map(|bound| bound.spec.clone()).collect()
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.held.iter().map(|bound| bound.spec.name).collect()
    }

    pub fn holds(&self, name: &str) -> bool {
        self.held.iter().any(|bound| bound.spec.name == name)
    }

    /// Whether the same call of tool `name` again is normal, so a worker is
    /// never stopped for it. A tool this toolbox does not hold has no such
    /// leave.
    pub fn repeatable(&self, name: &str) -> bool {
        self.held
            .iter()
            .any(|bound| bound.spec.name == name && bound.repeatable)
    }

    pub fn get(&self, name: &str) -> Lookup<'_, C> {
        if let Some(bound) = self.held.iter().find(|bound| bound.spec.name == name) {
            return Lookup::Held(&bound.call);
        }
        match self.refused.iter().find(|(tool, _)| *tool == name) {
            Some((_, why)) => Lookup::Refused(why),
            None => Lookup::Unknown,
        }
    }
}

impl<C> Default for Toolbox<C> {
    fn default() -> Self {
        Self {
            held: Vec::new(),
            refused: Vec::new(),
        }
    }
}

impl<C> fmt::Debug for Toolbox<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

/// The Router's tools. They need no host resource.
pub(super) fn router() -> Toolbox<RouterOp> {
    Toolbox::build(ROUTER, Ok, |resource| resource.unavailable().to_owned())
}

/// The host resources one seat's environment gives it.
struct Resources {
    code: Option<Env>,
    vault: Option<Arc<VaultParts>>,
    browser: Option<Arc<BrowserParts>>,
    reader: Option<Arc<Reader>>,
}

impl Resources {
    fn run_code(&self) -> Result<Env, Resource> {
        self.code.clone().ok_or(Resource::RunCode)
    }

    fn vault(&self) -> Result<Arc<VaultParts>, Resource> {
        self.vault.clone().ok_or(Resource::Vault)
    }

    fn browse(&self) -> Result<Browse, Resource> {
        Ok(Browse {
            parts: self.browser.clone().ok_or(Resource::Browser)?,
            vault: self.vault.clone(),
        })
    }

    fn reader(&self) -> Result<Arc<Reader>, Resource> {
        self.reader.clone().ok_or(Resource::Read)
    }
}

/// A worker tool's handler, with the resources it runs on.
pub enum WorkerCall {
    Report,
    Handback,
    RunCode(Env),
    /// `job_wait` and `job_stop` need run_code's environment to exist, and
    /// use no part of it.
    JobStart(Env),
    JobWait,
    JobStop,
    /// Without a vault, `fetch_url` serves keyless requests only.
    FetchUrl(Option<Arc<VaultParts>>),
    VaultList(Arc<VaultParts>),
    Browser(BrowserTool, Browse),
    ReadPath(Arc<Reader>),
}

impl WorkerCall {
    fn build(op: WorkerOp, host: &Resources) -> Result<Self, Resource> {
        Ok(match op {
            WorkerOp::Report => Self::Report,
            WorkerOp::Handback => Self::Handback,
            WorkerOp::RunCode => Self::RunCode(host.run_code()?),
            WorkerOp::JobStart => Self::JobStart(host.run_code()?),
            WorkerOp::JobWait => {
                host.run_code()?;
                Self::JobWait
            }
            WorkerOp::JobStop => {
                host.run_code()?;
                Self::JobStop
            }
            WorkerOp::FetchUrl => Self::FetchUrl(host.vault.clone()),
            WorkerOp::VaultList => Self::VaultList(host.vault()?),
            WorkerOp::BrowserOpen => Self::Browser(BrowserTool::Open, host.browse()?),
            WorkerOp::BrowserAct => Self::Browser(BrowserTool::Act, host.browse()?),
            WorkerOp::BrowserLogin => {
                let browse = host.browse()?;
                if browse.vault.is_none() {
                    return Err(Resource::Vault);
                }
                Self::Browser(BrowserTool::Login, browse)
            }
            WorkerOp::BrowserRead => Self::Browser(BrowserTool::Read, host.browse()?),
            WorkerOp::ReadPath => Self::ReadPath(host.reader()?),
        })
    }
}

impl AgentBroker {
    /// What `read` reaches for `seat`: pstack where this host has it, and a
    /// repository Ticket's clone at `/work` and what the host wrote about it
    /// at `/host`. `None` where the seat has neither.
    pub(super) fn seat_reader(&self, seat: &WorkerSeat) -> anyhow::Result<Option<Arc<Reader>>> {
        let mut roots: Vec<(&'static str, &Path)> = Vec::new();
        if let Some(tree) = &self.inner.pstack {
            roots.push((pstack::MOUNT, tree.root()));
        }
        if let Some(host) = &seat.host_dir {
            roots.extend([
                (WORK_MOUNT, seat.work_dir.as_path()),
                (HOST_MOUNT, host.as_path()),
            ]);
        }
        let reader = (!roots.is_empty()).then(|| Reader::new(&roots));
        Ok(reader.transpose()?.map(Arc::new))
    }

    /// The Reader of a toolbox that is listed and never called, for
    /// `worker_tools`: it reaches nothing, and exists where
    /// [`seat_reader`](Self::seat_reader) would give a seat one.
    pub(super) fn unmounted_reader(&self, repository: bool) -> Option<Arc<Reader>> {
        let reads = self.inner.pstack.is_some() || repository;
        reads.then(|| Reader::new(&[]).ok().map(Arc::new)).flatten()
    }

    /// The worker tools a worker whose Plan names `environment` holds on this
    /// host: run_code and the job tools where that environment offers
    /// run_code, the browser where it is this host, `browser_login` with a
    /// vault too, `fetch_url` wherever its own resource is, and `read` where
    /// the seat has a `reader`.
    pub(super) fn worker_toolbox(
        &self,
        environment: Option<&str>,
        reader: Option<Arc<Reader>>,
    ) -> Toolbox<WorkerCall> {
        let environments = &self.inner.environments;
        let unknown = environments.get(environment).err();
        let host = Resources {
            code: environments.get(environment).ok().flatten().cloned(),
            vault: self.inner.vault.clone(),
            browser: self
                .inner
                .browser
                .clone()
                .filter(|_| environments.is_local(environment)),
            reader,
        };
        Toolbox::build(
            WORKER,
            |op| WorkerCall::build(op, &host),
            |resource| {
                unknown
                    .clone()
                    .unwrap_or_else(|| resource.unavailable().to_owned())
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Lookup, Resource, Toolbox};
    use crate::agent::broker::registry::Tool;
    use serde_json::{Value, json};

    #[derive(Clone, Copy)]
    enum Op {
        Plain,
        Browse,
        Pstack,
    }

    fn schema() -> Value {
        json!({"type": "object", "properties": {}})
    }

    const TABLE: &[Tool<Op>] = &[
        Tool {
            name: "plain",
            op: Op::Plain,
            description: "needs nothing",
            input_schema: schema,
            repeatable: false,
        },
        Tool {
            name: "browse",
            op: Op::Browse,
            description: "needs a browser",
            input_schema: schema,
            repeatable: true,
        },
        Tool {
            name: "pstack",
            op: Op::Pstack,
            description: "needs a pstack",
            input_schema: schema,
            repeatable: true,
        },
    ];

    /// A host with no browser: the browser tool is refused with its reason,
    /// and the rest are held in the table's order.
    #[test]
    fn a_tool_whose_resource_is_missing_is_refused_and_the_rest_are_held_in_order() {
        let toolbox: Toolbox<&'static str> = Toolbox::build(
            TABLE,
            |op| match op {
                Op::Plain => Ok("plain"),
                Op::Browse => Err(Resource::Browser),
                Op::Pstack => Ok("pstack"),
            },
            |resource| resource.unavailable().to_owned(),
        );
        assert_eq!(toolbox.names(), ["plain", "pstack"]);
        assert!(toolbox.holds("pstack") && !toolbox.holds("browse"));
        assert!(toolbox.repeatable("pstack"), "as the table declares it");
        assert!(!toolbox.repeatable("plain"), "as the table declares it");
        assert!(
            !toolbox.repeatable("browse"),
            "a tool the toolbox does not hold has no leave to repeat"
        );
        assert!(matches!(toolbox.get("plain"), Lookup::Held(&"plain")));
        assert!(matches!(
            toolbox.get("browse"),
            Lookup::Refused("no browser where this ticket runs")
        ));
        assert!(matches!(toolbox.get("run_code"), Lookup::Unknown));
        assert_eq!(
            toolbox
                .specs()
                .iter()
                .map(|spec| spec.name)
                .collect::<Vec<_>>(),
            ["plain", "pstack"]
        );
    }
}
