//! The agent's broker: the one gate between its model sessions and the model
//! and their tools. It speaks the swarm broker's wire (`POST /model`,
//! `POST /tool/{name}`, one bearer token per session, the same 503 pause
//! reply and `swarm_cost_microusd`), so the Pi extension works against
//! either. Each token names a principal, and every model call is held and
//! settled against the principal's account in the agent Store.

pub mod browser;
pub mod registry;
mod repeat;
mod secrets;
mod toolbox;

use super::AgentConfig;
use super::config::{ExecutorConfig, RepoConfig};
use super::environment::{Environments, ExecutionEnvironment, Harvested, Nodes};
use super::forge::REPO_JSON;
use super::jobs::{Host, JobLimits, Jobs, Refused};
use super::pi_executor::private_dir;
use super::ports::{Applied, Fact, FactSink, Forge, Proposal, Refusal as Unadmitted, Workspace};
use super::pstack::{self, Pstack, Reader};
use super::watchdog::usd;
use crate::broker::{FETCH_TOTAL_BYTES, FetchGuard, FetchRefused, fetch_guarded, now_ms};
use crate::code::CodeExecutor;
use crate::gateway::{self, PriceTable, Provider, Receipt, Temperature, Upstream};
use crate::vault::Vault;
use anyhow::{Context, Result, anyhow, ensure};
use axum::{
    Json,
    extract::{DefaultBodyLimit, Path as RoutePath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use browser::Browse;
use registry::{PrincipalKind, RouterOp};
pub use registry::{ROUTER_TOOLS, WORKER_TOOLS};
use repeat::{RepeatGuard, Stopped};
use serde::Deserialize;
use serde_json::{Value, json};
use solos_agent_core::change::{CommitSha, Deliverable, Harvest, RepoName};
use solos_agent_core::delivery::{Admission, NotReady, ready_gate};
use solos_agent_core::model::{
    Accept, Acknowledge, AssignmentId, EvalId, Method, MicroUsd, Ms, Plan, ReportIn, ReportStatus,
    ThreadKey, Ticket, TicketId, Trigger, TriggerId, Verdict,
};
use solos_agent_store::{
    Charge, HoldId, HoldRequest, Reservation, SqliteStore, StoreError, Tokens,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};
use std::time::Duration;
use tokio::net::UnixListener;
use toolbox::{Env, Lookup, Toolbox, WorkerCall};

/// The broker's socket in the state directory.
pub const SOCKET_FILE: &str = "broker.sock";
const DAY_MS: Ms = 86_400_000;
/// `sockaddr_un.sun_path` is 108 bytes, with a trailing NUL.
const SUN_PATH_LIMIT: usize = 108;
const READ_TEXT_CHARS: usize = 2_000;
const READ_REPORT_CHARS: usize = 1_000;
const READ_REASON_CHARS: usize = 300;
const READ_RECENT: usize = 20;
/// The most a Report's text keeps, which the host's own Reports keep too.
pub(crate) const REPORT_TEXT_CHARS: usize = 8_000;
const QUESTION_CHARS: usize = 1_000;
const FAILURE_CHARS: usize = 400;
/// What a worker whose turn the broker stopped for looping or for making no
/// progress reads, in the refusal of every tool call it makes after.
fn turn_stopped(stopped: Stopped) -> String {
    format!(
        "Your turn is stopped because you {}, and the person is told you got stuck. Do not \
         call any more tools.",
        stopped.note()
    )
}

/// Why the model call after that is refused. A harness that has no turn to
/// end, such as Claude Code, shows this as the session's error.
fn stuck_reason(stopped: Stopped) -> String {
    format!("the turn was stopped because the worker {}", stopped.note())
}

/// Who a session is.
#[derive(Clone, Debug)]
pub enum Principal {
    Router,
    /// The worker of one Assignment.
    Worker(Arc<WorkerSeat>),
}

impl Principal {
    pub fn tools(&self) -> &'static [&'static str] {
        match self {
            Self::Router => &ROUTER_TOOLS,
            Self::Worker(_) => &WORKER_TOOLS,
        }
    }

    pub fn kind(&self) -> PrincipalKind {
        match self {
            Self::Router => PrincipalKind::Router,
            Self::Worker(_) => PrincipalKind::Worker,
        }
    }
}

/// Where `read` serves a repository Ticket's clone, as `run_code` does.
pub const WORK_MOUNT: &str = "/work";
/// Where `read` serves what the host wrote about a repository Ticket.
pub const HOST_MOUNT: &str = "/host";

/// One Assignment's worker as the broker knows it. The executor that
/// started the worker holds it too, to tell whether a turn reported.
#[derive(Debug)]
pub struct WorkerSeat {
    pub ticket: TicketId,
    pub assignment: AssignmentId,
    /// `run_code`'s `/work`.
    pub work_dir: PathBuf,
    /// Where its commands run, as its Plan names it.
    environment: Option<String>,
    /// A repository Ticket's `<ticket>/host`.
    host_dir: Option<PathBuf>,
    /// The Ticket's whole budget, which every Assignment of it shares, or
    /// `None` where it has no cap.
    pub budget_microusd: Option<MicroUsd>,
    /// The Ticket account's spend from this time on is this Assignment's.
    pub since_ms: Ms,
    /// The latest status the Engine took in the current turn.
    turn: Mutex<Option<ReportStatus>>,
    /// Why the turn's latest model call failed, if it did.
    model_failure: Mutex<Option<ModelFailure>>,
    /// The worker handed an Input back in the current turn.
    handed_back: AtomicBool,
    /// The turn's latest tool calls, so a loop is refused.
    repeats: Mutex<RepeatGuard>,
    fetched_bytes: AtomicU64,
    jobs: Arc<Jobs>,
    browser: browser::SeatBrowser,
    /// The worker tools this seat holds. `admit_worker` builds it from the
    /// seat's environment and the host.
    held: Toolbox<WorkerCall>,
}

impl WorkerSeat {
    pub fn new(
        ticket: TicketId,
        assignment: AssignmentId,
        work_dir: PathBuf,
        budget_microusd: impl Into<Option<MicroUsd>>,
    ) -> Self {
        Self {
            ticket,
            assignment,
            work_dir,
            environment: None,
            host_dir: None,
            budget_microusd: budget_microusd.into(),
            since_ms: now_ms(),
            turn: Mutex::new(None),
            model_failure: Mutex::new(None),
            handed_back: AtomicBool::new(false),
            repeats: Mutex::new(RepeatGuard::default()),
            fetched_bytes: AtomicU64::new(0),
            jobs: Arc::default(),
            browser: browser::SeatBrowser::default(),
            held: Toolbox::default(),
        }
    }

    /// Its commands run where its Plan's `environment` names.
    pub fn in_environment(mut self, environment: Option<String>) -> Self {
        self.environment = environment;
        self
    }

    /// Its Ticket works on a repository: `work_dir` is the clone and
    /// `host_dir` what the host wrote about it.
    pub fn on_repository(mut self, host_dir: PathBuf) -> Self {
        self.host_dir = Some(host_dir);
        self
    }

    fn has_repository(&self) -> bool {
        self.host_dir.is_some()
    }

    pub fn account(&self) -> Account {
        Account::Ticket {
            id: self.ticket.clone(),
            budget_microusd: self.budget_microusd,
        }
    }

    /// A prompt went out: what the worker reports from now on is this turn's,
    /// and only its own job calls take a job's end until the turn is over.
    pub fn begin_turn(&self) {
        if let Ok(mut turn) = self.turn.lock() {
            *turn = None;
        }
        if let Ok(mut failure) = self.model_failure.lock() {
            *failure = None;
        }
        self.handed_back.store(false, Ordering::SeqCst);
        self.guard().begin_turn();
        self.jobs.begin_turn();
    }

    /// The turn ended: the ends of the jobs it did not take, as Notes that
    /// wake the worker. A job that ends later is told as it ends.
    pub fn end_turn(&self) -> Vec<Fact> {
        self.jobs
            .end_turn()
            .into_iter()
            .map(|text| Fact::Note {
                ticket: self.ticket.clone(),
                text,
            })
            .collect()
    }

    /// New input waits for the next turn: a `job_wait` returns early to say so.
    pub fn input_waiting(&self) {
        self.jobs.input_waiting();
    }

    /// A background job still runs, so the worker is still working.
    pub fn jobs_running(&self) -> bool {
        self.jobs.running()
    }

    /// The Assignment's worker has ended: kill its jobs, and tell none of
    /// their ends.
    pub async fn stop_jobs(&self) {
        self.jobs.stop_all().await;
    }

    /// The turn left the Ticket as if still working: it reported nothing,
    /// or only progress.
    pub fn quiet_turn(&self) -> bool {
        self.turn.lock().map_or(true, |turn| {
            matches!(*turn, None | Some(ReportStatus::Progress))
        })
    }

    /// The turn ended on a failed model call and left no report to say so:
    /// why the call failed, once. A model call that fails ends a Pi turn
    /// with no tool call, so without this the Ticket would look as if the
    /// worker had nothing more to say.
    pub fn take_model_failure(&self) -> Option<ModelFailure> {
        let failure = self.model_failure.lock().ok()?.take();
        failure.filter(|_| self.quiet_turn())
    }

    /// The worker handed an Input back in the current turn, so what it says
    /// after that is not an answer to the person.
    pub fn handed_back(&self) -> bool {
        self.handed_back.load(Ordering::SeqCst)
    }

    fn reported(&self, status: ReportStatus) {
        if let Ok(mut turn) = self.turn.lock() {
            *turn = Some(status);
        }
    }

    /// A model call ended as `reply`: the turn's latest call failed, or did
    /// not any more. `named` is the failure the call itself named.
    fn model_called(&self, reply: &Reply, named: Option<ModelFailure>) {
        let Ok(mut failure) = self.model_failure.lock() else {
            return;
        };
        match reply {
            Ok(_) => *failure = None,
            // A pause is waited out by the session, and nothing failed.
            Err((StatusCode::SERVICE_UNAVAILABLE, _)) => {}
            Err((_, Json(body))) => {
                *failure = named.or_else(|| {
                    let why = body["error"].as_str().unwrap_or("refused");
                    Some(ModelFailure::Failed(why.to_owned()))
                });
            }
        }
    }

    fn guard(&self) -> MutexGuard<'_, RepeatGuard> {
        self.repeats.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Refuse a tool call that repeats the seat's latest calls too often,
    /// and say what the call answered before. Once the refusals reach the
    /// limit, the calls stop bringing anything new, or the turn passes its
    /// ceiling of tool calls, the turn is stopped: that call and each one
    /// after it is told so, and runs nothing.
    fn repeat_refused(&self, tool: &str, args: &Value, repeatable: bool) -> Option<Refusal> {
        let mut repeats = self.guard();
        if let Some(stopped) = repeats.stopped() {
            return Some(refuse(StatusCode::TOO_MANY_REQUESTS, turn_stopped(stopped)));
        }
        let repeated = repeats.call(tool, args, repeatable);
        let stopped = repeats.stopped();
        let mut why = match repeated {
            Some(repeated) => repeated.why(tool, repeats.earlier(tool, args)),
            None if stopped.is_none() => return None,
            None => String::new(),
        };
        if let Some(stopped) = stopped {
            eprintln!(
                "agent: the worker of {} on {} {}, last {tool}; its turn is stopped",
                self.assignment,
                self.ticket,
                stopped.note()
            );
            if !why.is_empty() {
                why.push(' ');
            }
            why.push_str(&turn_stopped(stopped));
        }
        Some(refuse(StatusCode::TOO_MANY_REQUESTS, why))
    }

    /// Keep what a call of `tool` with `args` answered, so a refusal of the
    /// same call can say so.
    fn answered(&self, tool: &str, args: &Value, answer: Result<&Value, &str>) {
        self.guard().answered(tool, args, answer);
    }

    /// A model call is about to be made: note it, and say whether the turn is
    /// stopped, by what the guard saw of the tool calls or by this call being
    /// past the turn's ceiling. Then the seat's model calls are refused,
    /// which ends a Pi turn, until the next turn begins; this is why, and
    /// what the host tells the person.
    fn stopped_before_model_call(&self) -> Option<(String, ModelFailure)> {
        let mut repeats = self.guard();
        let was = repeats.stopped();
        repeats.model_call();
        let stopped = repeats.stopped()?;
        if was.is_none() {
            eprintln!(
                "agent: the worker of {} on {} {}; its turn is stopped",
                self.assignment,
                self.ticket,
                stopped.note()
            );
        }
        let said = repeats.sentence()?;
        Some((stuck_reason(stopped), ModelFailure::Stuck(said)))
    }
}

/// Why a worker's model call did not go through, which its seat keeps until
/// its turn ends, so the host can tell the person when the turn ends
/// with nothing reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelFailure {
    /// The Ticket's account has no room for another call.
    BudgetReached {
        committed: MicroUsd,
        budget: MicroUsd,
    },
    /// Anything else: the provider's error or its call limit, the model
    /// limit, or the stop after an overrun. The text is the broker's reason.
    Failed(String),
    /// The broker stopped the turn: the worker kept sending the same call
    /// after it was refused, or made calls that brought nothing new, or
    /// passed the turn's ceiling of calls, so the call after the stop was
    /// refused too, with no model call spent. The text is what the host
    /// tells the person in the worker's place, in the worker's words: what it
    /// kept doing, and that it stopped.
    Stuck(String),
}

impl ModelFailure {
    /// The Report the host makes for the person in the worker's place: a
    /// budget that has no room is blocked on a person, any other failure is
    /// failed. The text goes to the person as it is, so it says in plain
    /// words what stopped the work and how to go on.
    pub fn report(&self) -> (ReportStatus, String) {
        match self {
            Self::Stuck(what) => (
                ReportStatus::Failed,
                format!(
                    "{} Reply here to have me try again with a different approach.",
                    clip(what, FAILURE_CHARS).0
                ),
            ),
            Self::BudgetReached { committed, budget } => (
                ReportStatus::Blocked,
                format!(
                    "I stopped before finishing: this request's budget ({}) cannot cover \
                     another model call ({} of it is already committed). A request's budget is \
                     set when it is accepted, so to go on, make a new request (not a reply to \
                     this one) and name a bigger budget.",
                    usd(*budget),
                    usd(*committed)
                ),
            ),
            Self::Failed(why) => (
                ReportStatus::Failed,
                format!(
                    "I stopped before finishing: the model call failed ({}). Reply here and I \
                     will try again.",
                    clip(why, FAILURE_CHARS).0
                ),
            ),
        }
    }
}

/// Who pays for a model call, and how much it may commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Account {
    /// The Router's judging, capped per UTC day, or not capped where
    /// `budgets.router_daily_microusd` is absent.
    Router { daily_microusd: Option<MicroUsd> },
    /// One Ticket's work, capped for its whole life by its Mandate's budget,
    /// which is fixed at accept time. A Ticket whose Mandate has none is not
    /// capped: its calls are still held and settled, so its spend is recorded.
    Ticket {
        id: TicketId,
        budget_microusd: Option<MicroUsd>,
    },
}

impl Account {
    /// The ledger's key. The cap is not part of it, so a changed cap still
    /// counts what the account already spent.
    pub fn key(&self) -> String {
        match self {
            Self::Router { .. } => "router".into(),
            Self::Ticket { id, .. } => Self::ticket_key(id),
        }
    }

    /// The key of `ticket`'s account.
    pub fn ticket_key(ticket: &TicketId) -> String {
        format!("ticket:{ticket}")
    }

    /// Spend from the first time on counts against the cap, if there is one.
    pub fn allowance(&self, now_ms: Ms) -> (Ms, Option<MicroUsd>) {
        match self {
            Self::Router { daily_microusd } => (now_ms - now_ms % DAY_MS, *daily_microusd),
            Self::Ticket {
                budget_microusd, ..
            } => (0, *budget_microusd),
        }
    }

    /// Whether a call on this account can be refused for spend.
    fn capped(&self) -> bool {
        self.allowance(0).1.is_some()
    }

    fn cap_reached(&self, committed: MicroUsd, cap: MicroUsd) -> String {
        match self {
            Self::Router { .. } => format!(
                "the Router's daily spend cap is reached: {committed} of {cap} microusd committed today"
            ),
            Self::Ticket { id, .. } => {
                format!("{id}'s budget is reached: {committed} of {cap} microusd committed")
            }
        }
    }
}

/// The model a principal's sessions call, and its limits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTerms {
    pub upstream: Upstream,
    pub prices: PriceTable,
    pub max_input_bytes: usize,
    pub max_output_tokens: u64,
    /// The longest one call may take before it is dropped and its hold
    /// released, or no limit beyond the transport's own.
    pub call_limit: Option<Duration>,
    /// What the calls are sampled at. `None` leaves the shared upstream body's
    /// 0, as the Router judges and as swarm mode runs; workers have one, since
    /// a model decoded greedily is the more likely to go round in circles.
    pub temperature: Option<Temperature>,
}

impl ModelTerms {
    /// The upstream body `upstream_body` made, sampled as these terms say.
    pub(crate) fn sample(&self, upstream: Value) -> Value {
        match self.temperature {
            Some(temperature) => gateway::sampled(upstream, temperature),
            None => upstream,
        }
    }
}

/// What one token admits.
#[derive(Clone, Debug)]
pub struct Grant {
    pub principal: Principal,
    pub account: Account,
    pub terms: ModelTerms,
    /// Served at `/session`; the Pi extension makes it the session's system prompt.
    pub system_prompt: Option<String>,
}

/// The budgets and plans a Router's accept may name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DecideTerms {
    /// What a session accept that names no budget gets
    /// (`budgets.session_microusd`). `None`: a session has no budget, so it
    /// needs no approval and its account has no cap, and a budget the
    /// Router names for one is not used. A swarm always has a budget.
    pub default_budget_microusd: Option<MicroUsd>,
    /// The most an accept may name, or no cap. Above the Policy's approval
    /// threshold a Ticket waits for a person's approval, so where nobody can
    /// give one the threshold is the cap, instead of a Ticket that waits
    /// forever.
    pub max_budget_microusd: Option<MicroUsd>,
    /// Where `max_budget_microusd` is no cap because a person can approve,
    /// the Sources whose Tickets can ask one: their Channel shows buttons
    /// and an approver is on that Source. An accept from any other Source
    /// (GitHub, MCP, a schedule) is capped at `auto_approve_microusd`, since
    /// its Ticket would wait for a button that Source cannot show. Empty: no
    /// per-source cap.
    pub approval_sources: BTreeSet<String>,
    /// The Policy's approval threshold: the cap on an accept from a Source
    /// outside `approval_sources`.
    pub auto_approve_microusd: MicroUsd,
    /// What a swarm plan may name, or `None` where no swarm runs.
    pub swarm: Option<SwarmTerms>,
    /// The repositories an accept may name, from `forge.repos`.
    pub repos: BTreeMap<RepoName, RepoConfig>,
}

/// The Goal templates a swarm plan may name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwarmTerms {
    pub templates: Vec<String>,
    /// The one a swarm plan that names none gets.
    pub default_template: Option<String>,
    /// What a swarm plan that names no budget gets
    /// (`executor.swarm.default_budget_microusd`), before the session
    /// default.
    pub default_budget_microusd: Option<MicroUsd>,
}

impl SwarmTerms {
    /// Refuse a template that is not configured, and a plan that names none
    /// where there is no default.
    fn admit(&self, template: Option<&str>) -> std::result::Result<(), String> {
        match template.or(self.default_template.as_deref()) {
            Some(name) if self.templates.iter().any(|t| t == name) => Ok(()),
            Some(name) => Err(format!(
                "there is no goal template {name}; the templates are {}",
                self.templates.join(", ")
            )),
            None => Err(format!(
                "a swarm plan here names its goal_template, one of {}",
                self.templates.join(", ")
            )),
        }
    }
}

impl DecideTerms {
    /// The most an accept whose Ticket came from `origin` may name, or no
    /// cap.
    pub(crate) fn ceiling(&self, origin: Option<&str>) -> Option<MicroUsd> {
        match self.max_budget_microusd {
            Some(max) => Some(max),
            None if self.approval_sources.is_empty() => None,
            None if origin.is_some_and(|source| self.approval_sources.contains(source)) => None,
            None => Some(self.auto_approve_microusd),
        }
    }

    fn repo(&self, name: &str) -> std::result::Result<(RepoName, &RepoConfig), String> {
        let found = RepoName::try_from(name.to_owned())
            .ok()
            .and_then(|repo| self.repos.get_key_value(&repo));
        match found {
            Some((repo, terms)) => Ok((repo.clone(), terms)),
            None if self.repos.is_empty() => {
                Err("this agent works on no repository; accept the work without a repo".into())
            }
            None => {
                let names: Vec<&str> = self.repos.keys().map(RepoName::as_str).collect();
                Err(format!(
                    "there is no repository {name}; the repositories are {}",
                    names.join(", ")
                ))
            }
        }
    }

    /// What `config` lets an accept name: above `policy.auto_approve_microusd`
    /// only when Slack or Discord can take the approval its Ticket then waits
    /// for, and a swarm only when `executor.swarm` runs one.
    pub fn for_config(config: &AgentConfig) -> Self {
        let swarm = match &config.executor {
            ExecutorConfig::Pi {
                swarm: Some(swarm), ..
            } => Some(SwarmTerms {
                templates: swarm.templates.keys().cloned().collect(),
                default_template: swarm.default_template.clone(),
                default_budget_microusd: swarm.default_budget_microusd,
            }),
            _ => None,
        };
        Self {
            default_budget_microusd: config.budgets.session_microusd,
            max_budget_microusd: (!config.takes_approvals())
                .then_some(config.policy.auto_approve_microusd),
            approval_sources: config.approval_sources(),
            auto_approve_microusd: config.policy.auto_approve_microusd,
            swarm,
            repos: config
                .forge
                .as_ref()
                .map(|forge| forge.repos().clone())
                .unwrap_or_default(),
        }
    }
}

pub struct BrokerParts {
    /// The state directory. Its pause gate refuses model calls.
    pub dir: PathBuf,
    /// The broker's own connection to the agent Store, for the ledger and
    /// for reads. The runtime keeps the Engine's.
    pub store: SqliteStore,
    pub provider: Arc<dyn Provider>,
    /// Where the Router's Verdicts and the workers' Reports and handbacks go.
    pub facts: FactSink,
    pub decide: DecideTerms,
    /// What workers are admitted on; `None` admits none.
    pub workers: Option<WorkerParts>,
}

pub struct WorkerParts {
    pub terms: ModelTerms,
    /// `None`: this host offers workers no `run_code`.
    pub code: Option<Arc<CodeExecutor>>,
    /// `None`: no `vault_list`, and `fetch_url` takes no key.
    pub vault: Option<VaultParts>,
    /// `None`: no browser tools.
    pub browser: Option<browser::BrowserParts>,
    /// Off, `fetch_url` refuses an address that is loopback, private,
    /// link-local, shared (carrier-grade NAT, Tailscale), unspecified or
    /// multicast, whatever name the worker used for it and on every redirect
    /// hop, because this host also runs services on those addresses. On, it
    /// reaches them (`executor.fetch.allow_private_networks`).
    pub allow_private_networks: bool,
}

pub struct VaultParts {
    pub vault: Arc<Vault>,
    /// The Sources whose Channel here can ask a person to approve an
    /// item's first use. A Ticket takes approvals when one of its Channels
    /// is on one of them.
    pub asks_on: BTreeSet<String>,
}

#[derive(Clone)]
pub struct AgentBroker {
    inner: Arc<Inner>,
}

struct Inner {
    dir: PathBuf,
    store: Mutex<SqliteStore>,
    provider: Arc<dyn Provider>,
    facts: FactSink,
    decide: DecideTerms,
    /// The model terms every admitted worker gets; `None` admits none.
    worker_terms: Option<ModelTerms>,
    /// Whether `fetch_url`, keyed or not, may reach a private address.
    allow_private_networks: bool,
    grants: RwLock<BTreeMap<String, Arc<Grant>>>,
    /// A settlement above its hold means the ledger no longer bounds spend,
    /// so every later model call is refused.
    overrun: AtomicBool,
    jobs: JobLimits,
    environments: Environments,
    pstack: Option<Arc<Pstack>>,
    vault: Option<Arc<VaultParts>>,
    browser: Option<Arc<browser::BrowserParts>>,
    /// Judges what a worker proposes on `ready`; set once, before workers run.
    forge: OnceLock<Arc<dyn Forge>>,
    /// The Router's tools, which need no host resource.
    router: Toolbox<RouterOp>,
}

/// What `ready` came to before the Engine records it: what the Ticket owes
/// decides whether the worker's commits were harvested and admitted.
#[derive(Clone, Debug)]
pub struct ReadyAdmission {
    pub admission: Admission,
    /// The commits the host admitted, for a Ticket that owes a change or a
    /// pull request. Goes into the Report as `ReportIn.harvest`.
    pub harvest: Option<Harvest>,
    /// Files left uncommitted in the worker's tree, which are not delivered.
    pub uncommitted: u32,
    /// The head's number among the heads admitted for the Ticket.
    pub round: u32,
}

impl ReadyAdmission {
    /// Add to the worker's reply what the host did with its commits, and
    /// what to do next.
    fn describe(&self, reply: &mut Value) {
        let Some(harvest) = &self.harvest else {
            return;
        };
        let head = harvest.head.short();
        let (kind, next) = match self.admission {
            Admission::Propose(_) => (
                "proposal",
                format!(
                    "Accepted. The host is publishing {head} on its own branch and opening the \
                     pull request as the bot; the ticket ends when it exists. Stop here."
                ),
            ),
            Admission::Committed(_) => (
                "proposal",
                format!(
                    "Accepted. Your commits ({head}) stay in this ticket's clone, which is not \
                     published; the ticket has ended. Stop here."
                ),
            ),
            Admission::Reply | Admission::Answer => {
                reply["next"] = json!(
                    "Sent to whoever asked; the head is as it was, and the ticket has ended. \
                     Stop here."
                );
                return;
            }
        };
        reply[kind] = json!({
            "head": head,
            "commits": harvest.commits,
            "files": harvest.files,
            "bytes": harvest.bytes,
            "round": self.round,
            "uncommitted_files_not_included": self.uncommitted,
        });
        reply["next"] = json!(next);
    }
}

/// Why `ready` is not admitted. Nothing was committed.
#[derive(Debug)]
pub struct NotAdmitted {
    /// The worker's to mend (a refusal it acts on), not the host's trouble.
    pub conflict: bool,
    /// What the worker reads: what to do next, or what went wrong here.
    pub why: String,
}

impl NotAdmitted {
    fn refused(why: String) -> Self {
        Self {
            conflict: true,
            why,
        }
    }

    fn host(err: impl Into<anyhow::Error>) -> Self {
        Self {
            conflict: false,
            why: format!(
                "the host could not check your commits: {:#}. Report ready again in a moment.",
                err.into()
            ),
        }
    }
}

type Refusal = (StatusCode, Json<Value>);
type Reply = std::result::Result<Json<Value>, Refusal>;

fn refuse(status: StatusCode, why: impl std::fmt::Display) -> Refusal {
    (status, Json(json!({"error": why.to_string()})))
}
fn forbidden(why: impl std::fmt::Display) -> Refusal {
    refuse(StatusCode::FORBIDDEN, why)
}
fn failed(why: impl std::fmt::Display) -> Refusal {
    refuse(StatusCode::INTERNAL_SERVER_ERROR, why)
}

/// A bearer token for one principal's sessions.
pub fn new_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Bind the broker's socket in the state directory, removing one a dead
/// process left behind. The directory is private, so the socket is too.
pub fn bind(state_dir: &Path) -> Result<(UnixListener, PathBuf)> {
    let path = state_dir.join(SOCKET_FILE);
    ensure!(
        path.as_os_str().len() < SUN_PATH_LIMIT,
        "{} is too long for a Unix socket; use a shorter state directory",
        path.display()
    );
    if path.exists() {
        ensure!(
            std::os::unix::net::UnixStream::connect(&path).is_err(),
            "another agent serves {}",
            state_dir.display()
        );
        std::fs::remove_file(&path)
            .with_context(|| format!("cannot remove the stale {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(&path).with_context(|| format!("cannot bind {}", path.display()))?;
    Ok((listener, path))
}

impl AgentBroker {
    /// A broker over the agent Store. Holds a dead process left open are
    /// charged at their hold first, since they may have been billed.
    pub fn new(parts: BrokerParts) -> Result<Self> {
        Self::with_jobs(parts, JobLimits::default())
    }

    /// [`new`](Self::new), with `jobs` bounding the workers' background jobs.
    pub fn with_jobs(parts: BrokerParts, jobs: JobLimits) -> Result<Self> {
        Self::with_nodes(parts, jobs, Nodes::default())
    }

    /// [`with_jobs`](Self::with_jobs), with remote nodes a worker's
    /// commands may run on.
    pub fn with_nodes(parts: BrokerParts, jobs: JobLimits, nodes: Nodes) -> Result<Self> {
        Self::with_pstack(parts, jobs, nodes, None)
    }

    /// [`with_nodes`](Self::with_nodes), with the pstack every worker may
    /// `read`.
    pub fn with_pstack(
        parts: BrokerParts,
        jobs: JobLimits,
        nodes: Nodes,
        pstack: Option<Arc<Pstack>>,
    ) -> Result<Self> {
        let BrokerParts {
            dir,
            mut store,
            provider,
            facts,
            decide,
            workers,
        } = parts;
        let orphans = store.close_open_holds(now_ms())?;
        if orphans > 0 {
            eprintln!(
                "agent: charged {orphans} model calls a previous run left open at their hold"
            );
        }
        let (worker_terms, local, vault, browser, allow_private_networks) = match workers {
            Some(WorkerParts {
                terms,
                code,
                vault,
                browser,
                allow_private_networks,
            }) => (
                Some(terms),
                code,
                vault.map(Arc::new),
                browser.map(Arc::new),
                allow_private_networks,
            ),
            None => (None, None, None, None, false),
        };
        Ok(Self {
            inner: Arc::new(Inner {
                dir,
                store: Mutex::new(store),
                provider,
                facts,
                decide,
                environments: Environments::new(local, nodes),
                worker_terms,
                allow_private_networks,
                grants: RwLock::new(BTreeMap::new()),
                overrun: AtomicBool::new(false),
                jobs,
                pstack,
                vault,
                browser,
                forge: OnceLock::new(),
                router: toolbox::router(),
            }),
        })
    }

    pub fn admit(&self, token: String, grant: Grant) -> Result<()> {
        self.inner
            .grants
            .write()
            .map_err(|_| anyhow!("the grant table lock is poisoned"))?
            .insert(token, Arc::new(grant));
        Ok(())
    }

    /// Admit one Assignment's worker on its Ticket's account, with the
    /// workers' model terms and `system_prompt` at `/session`.
    pub fn admit_worker(
        &self,
        token: String,
        mut seat: WorkerSeat,
        system_prompt: String,
    ) -> Result<Arc<WorkerSeat>> {
        let terms = self
            .inner
            .worker_terms
            .clone()
            .context("this broker admits no workers")?;
        let reader = self.seat_reader(&seat)?;
        seat.held = self.worker_toolbox(seat.environment.as_deref(), reader);
        let seat = Arc::new(seat);
        let grant = Grant {
            principal: Principal::Worker(seat.clone()),
            account: seat.account(),
            terms,
            system_prompt: Some(system_prompt),
        };
        self.admit(token, grant)?;
        Ok(seat)
    }

    /// Where workers' commands may run.
    pub fn environments(&self) -> &Environments {
        &self.inner.environments
    }

    pub fn pstack(&self) -> Option<&Arc<Pstack>> {
        self.inner.pstack.as_ref()
    }

    /// The forge that judges the commits a worker proposes when it reports
    /// `ready` (see [`admit_ready`](Self::admit_ready)). Set once, before any
    /// worker runs; a second call changes nothing.
    pub fn set_forge(&self, forge: Arc<dyn Forge>) {
        let _ = self.inner.forge.set(forge);
    }

    /// What `ready` means for `seat`'s Ticket, before the Engine records it:
    /// for one that owes only an answer, nothing to check; for one that owes
    /// a change or a pull request, the host harvests the worker's commits in
    /// a fresh sandbox of its own and has the forge admit them in its mirror.
    /// Every path that files a `ready` for a worker goes through here, the
    /// `worker_report` tool and the host's own report of a turn that ended in
    /// plain text, so no `ready` ends such a Ticket without its commits being
    /// judged. A refusal commits nothing and says what to do next.
    pub async fn admit_ready(
        &self,
        seat: &WorkerSeat,
    ) -> std::result::Result<ReadyAdmission, NotAdmitted> {
        let id = seat.ticket.clone();
        let found = self
            .store(move |store| store.read_ticket(&id))
            .await
            .map_err(NotAdmitted::host)?;
        // A Ticket the Store does not hold owes nothing here: the Engine refuses
        // a Report for it, with its own words.
        let Some(ticket) = found else {
            return Ok(ReadyAdmission {
                admission: Admission::Answer,
                harvest: None,
                uncommitted: 0,
                round: 0,
            });
        };
        let terms = ticket.mandate.forge.as_ref();
        // Everything that does not need the commits is answered first.
        match ready_gate(&ticket, terms, None) {
            Ok(admission) => {
                return Ok(ReadyAdmission {
                    admission,
                    harvest: None,
                    uncommitted: 0,
                    round: 0,
                });
            }
            Err(NotReady::NothingToDeliver) => {}
            Err(refused) => return Err(NotAdmitted::refused(refused.tell())),
        }
        let forge = self.inner.forge.get().ok_or_else(|| {
            NotAdmitted::host(anyhow!("this agent has no forge to take a change"))
        })?;
        let repo = ticket
            .mandate
            .repo
            .clone()
            .ok_or_else(|| NotAdmitted::host(anyhow!("{} names no repository", ticket.id)))?;
        let host = seat
            .host_dir
            .as_ref()
            .ok_or_else(|| NotAdmitted::host(anyhow!("{} has no clone", ticket.id)))?;
        let marker = host.join(REPO_JSON);
        let workspace: Workspace = std::fs::read(&marker)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .with_context(|| format!("cannot read {}", marker.display()))
            .map_err(NotAdmitted::host)?;
        let base_commit = CommitSha::try_from(workspace.base_commit.clone())
            .map_err(|why| NotAdmitted::host(anyhow!("{} says {why}", marker.display())))?;
        let ticket_dir = host
            .parent()
            .ok_or_else(|| NotAdmitted::host(anyhow!("{} has no directory", ticket.id)))?;
        let out = ticket_dir
            .join("out")
            .join(uuid::Uuid::new_v4().simple().to_string());
        private_dir(&out).map_err(NotAdmitted::host)?;
        let admitted = async {
            let named = seat.environment.as_deref();
            let bundle = match self
                .inner
                .environments
                .harvest(named, &seat.work_dir, &out, &base_commit)
                .await
                .map_err(NotAdmitted::host)?
            {
                Harvested::Bundle(bundle) => bundle,
                Harvested::NoCommits => {
                    return Err(NotAdmitted::refused(NotReady::NothingToDeliver.tell()));
                }
                Harvested::Failed(why) => {
                    return Err(NotAdmitted::refused(Unadmitted::BadBundle(why).tell()));
                }
            };
            let proposal = Proposal {
                ticket: &seat.ticket,
                repo: &repo,
                head: &bundle.head,
                bundle: &bundle.path,
                base: &workspace.base,
                base_commit: &base_commit,
            };
            match forge.admit(proposal).await.map_err(NotAdmitted::host)? {
                Ok(harvest) => Ok((harvest, bundle.uncommitted)),
                Err(refusal) => Err(NotAdmitted::refused(refusal.tell())),
            }
        }
        .await;
        let _ = std::fs::remove_dir_all(&out);
        let (harvest, uncommitted) = admitted?;
        // What the Engine will make of it, for the words the worker reads.
        let admission = ready_gate(&ticket, terms, Some(&harvest))
            .map_err(|refused| NotAdmitted::refused(refused.tell()))?;
        Ok(ReadyAdmission {
            admission,
            round: ticket.change.as_ref().map_or(1, |change| change.pushes + 1),
            harvest: Some(harvest),
            uncommitted,
        })
    }

    /// What a worker whose Plan names `environment` holds on this host, on a
    /// Ticket with or without a repository: its grants, and what `GET /tools`
    /// lists. It is what [`admit_worker`](Self::admit_worker) binds for such a
    /// seat.
    pub fn worker_tools(&self, environment: Option<&str>, repository: bool) -> Vec<&'static str> {
        self.worker_toolbox(environment, self.unmounted_reader(repository))
            .names()
    }

    /// What `seat` holds once admitted.
    pub fn seat_tools(&self, seat: &WorkerSeat) -> Vec<&'static str> {
        self.worker_tools(seat.environment.as_deref(), seat.has_repository())
    }

    /// This host's browser, with the vault its logins come from when it has one.
    fn browse(&self) -> Option<Browse> {
        Some(Browse {
            parts: self.inner.browser.clone()?,
            vault: self.inner.vault.clone(),
        })
    }

    fn worker_seats(&self) -> Vec<Arc<WorkerSeat>> {
        let grants = match self.inner.grants.read() {
            Ok(grants) => grants,
            Err(poisoned) => poisoned.into_inner(),
        };
        grants
            .values()
            .filter_map(|grant| match &grant.principal {
                Principal::Worker(seat) => Some(seat.clone()),
                Principal::Router => None,
            })
            .collect()
    }

    /// Refuse `token` from now on. A call already admitted finishes; a
    /// worker's background jobs are killed.
    pub fn revoke(&self, token: &str) {
        let revoked = match self.inner.grants.write() {
            Ok(mut grants) => grants.remove(token),
            Err(poisoned) => poisoned.into_inner().remove(token),
        };
        if let Some(grant) = revoked
            && let Principal::Worker(seat) = &grant.principal
        {
            seat.jobs.abandon();
        }
    }

    /// What `seat`'s Assignment has committed so far on its Ticket's account.
    pub async fn spent(&self, seat: &WorkerSeat) -> Result<MicroUsd> {
        let (account, since) = (seat.account().key(), seat.since_ms);
        self.store(move |store| store.committed(&account, since))
            .await
    }

    /// What every Assignment of `ticket` has committed on its account.
    pub async fn ticket_committed(&self, ticket: &TicketId) -> Result<MicroUsd> {
        let account = Account::ticket_key(ticket);
        self.store(move |store| store.committed(&account, 0)).await
    }

    /// Charge `ticket`'s account what a run its own broker settled cost,
    /// once per `request_id`. Returns whether this call charged it.
    pub async fn charge_ticket(
        &self,
        ticket: &TicketId,
        provider: &str,
        cost_microusd: MicroUsd,
        request_id: String,
    ) -> Result<bool> {
        let (account, provider) = (Account::ticket_key(ticket), provider.to_owned());
        self.store(move |store| {
            store.charge(&Charge {
                account: &account,
                provider: &provider,
                cost_microusd,
                request_id: &request_id,
                at_ms: now_ms(),
            })
        })
        .await
    }

    /// The routes, for the grants admitted so far and any worker.
    pub fn router(&self) -> Result<axum::Router> {
        let workers = self
            .inner
            .worker_terms
            .as_ref()
            .map(|terms| terms.max_input_bytes);
        let largest = self
            .inner
            .grants
            .read()
            .map_err(|_| anyhow!("the grant table lock is poisoned"))?
            .values()
            .map(|grant| grant.terms.max_input_bytes)
            .chain(workers)
            .max()
            .unwrap_or(0);
        Ok(axum::Router::new()
            .route("/model", post(model))
            .route("/tool/{name}", post(tool))
            .route("/tools", get(tools))
            .route("/session", post(session))
            .route("/v1/messages", post(messages::messages))
            .route("/api/hello", get(messages::hello))
            .fallback(messages::unrouted)
            // Above the admitted input on purpose, as in swarm mode: a
            // session replays its whole conversation, so the wire body may
            // exceed what the model limit admits, and is refused with a reason.
            .layer(DefaultBodyLimit::max(largest.saturating_mul(4).max(65_536)))
            .with_state(self.clone()))
    }

    fn authenticate(&self, headers: &HeaderMap) -> std::result::Result<Arc<Grant>, Refusal> {
        let header = |name| headers.get(name).and_then(|h| h.to_str().ok());
        let token = header("authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
            .or_else(|| header("x-api-key"))
            .ok_or_else(|| forbidden("missing identity"))?;
        self.inner
            .grants
            .read()
            .map_err(|_| failed("the grant table lock is poisoned"))?
            .get(token)
            .cloned()
            .ok_or_else(|| forbidden("invalid identity"))
    }

    /// The Source of the Trigger that gave `eval`'s batch its strength, which
    /// decides how much an accept may ask for. `None` when the Eval is not
    /// running, and then the tightest cap applies.
    async fn origin_source(&self, eval: &EvalId) -> Option<String> {
        use solos_agent_core::ports::Repositories as _;
        let eval = eval.clone();
        self.store(move |store| {
            let Some(key) = store.thread_of_eval(&eval) else {
                return Ok(None);
            };
            let Some(running) = store.read_thread(&key)?.and_then(|thread| thread.eval) else {
                return Ok(None);
            };
            Ok(store
                .read_trigger(&running.primary)?
                .map(|trigger| trigger.source))
        })
        .await
        .ok()
        .flatten()
    }

    /// Run `work` on the broker's Store connection off the async threads.
    async fn store<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut SqliteStore) -> std::result::Result<T, StoreError> + Send + 'static,
    ) -> Result<T> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = inner
                .store
                .lock()
                .map_err(|_| anyhow!("the store lock is poisoned"))?;
            Ok(work(&mut store)?)
        })
        .await?
    }
}

async fn session(State(broker): State<AgentBroker>, headers: HeaderMap) -> Reply {
    let grant = broker.authenticate(&headers)?;
    match &grant.system_prompt {
        Some(prompt) => Ok(Json(json!({"system_prompt": prompt}))),
        None => Err(refuse(StatusCode::NOT_FOUND, "no system prompt")),
    }
}

/// The calling principal's tools from the registry: a worker's without
/// `run_code` and the job tools where its environment offers no
/// `run_code`, so an MCP host lists only what it can call.
async fn tools(State(broker): State<AgentBroker>, headers: HeaderMap) -> Reply {
    let grant = broker.authenticate(&headers)?;
    let (kind, offered) = match &grant.principal {
        Principal::Router => (PrincipalKind::Router, broker.inner.router.specs()),
        Principal::Worker(seat) => (PrincipalKind::Worker, seat.held.specs()),
    };
    Ok(Json(json!({"principal": kind, "tools": offered})))
}

async fn model(
    State(broker): State<AgentBroker>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    let grant = broker.authenticate(&headers)?;
    let terms = &grant.terms;
    let call = Call {
        input_bytes: serde_json::to_vec(&body).map_err(forbidden)?.len(),
        admitted: gateway::admit_request(&body, &terms.upstream.model, |name| {
            grant.principal.tools().contains(&name)
        }),
        max_output_tokens: terms.max_output_tokens,
        upstream: terms.sample(gateway::upstream_body(
            &terms.upstream.model,
            &body,
            terms.max_output_tokens,
        )),
    };
    call.send(&broker, &grant).await
}

/// One model call a session asked for, before the broker's checks.
struct Call {
    /// What the hold prices as input and the model limit bounds.
    input_bytes: usize,
    /// Whether the request is inside the model contract.
    admitted: Result<()>,
    /// What the hold prices as output.
    max_output_tokens: u64,
    upstream: Value,
}

impl Call {
    /// [`attempt`](Self::attempt) the call, and tell a worker's seat how it
    /// went, so a turn that ends on a failed call can say why.
    async fn send(self, broker: &AgentBroker, grant: &Arc<Grant>) -> Reply {
        let mut named = None;
        let reply = self.attempt(broker, grant, &mut named).await;
        if let Principal::Worker(seat) = &grant.principal {
            seat.model_called(&reply, named);
        }
        reply
    }

    /// Refuse the call past the model limit, while paused, outside the
    /// contract, after an overrun or past the account's cap (which it names
    /// in `named`); else hold it, send it upstream and settle it.
    async fn attempt(
        self,
        broker: &AgentBroker,
        grant: &Arc<Grant>,
        named: &mut Option<ModelFailure>,
    ) -> Reply {
        let terms = &grant.terms;
        // First of all: the turn of a worker that loops, makes no progress or
        // has made its ceiling of calls ends here, with no hold, no upstream
        // call and no cost.
        if let Principal::Worker(seat) = &grant.principal
            && let Some((reason, failure)) = seat.stopped_before_model_call()
        {
            *named = Some(failure);
            return Err(forbidden(reason));
        }
        if self.input_bytes > terms.max_input_bytes {
            return Err(forbidden("request exceeds the model limit"));
        }
        // Before any hold: a paused call costs nothing and leaves no row.
        if let Some(wait) = gateway::effective_control(&broker.inner.dir, gateway::unix_secs()) {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(gateway::paused_reply(wait)),
            ));
        }
        self.admitted.map_err(forbidden)?;
        // An account with no cap relies on no hold to bound its spend, so
        // an overrun stops only the accounts that have one.
        if grant.account.capped() && broker.inner.overrun.load(Ordering::SeqCst) {
            return Err(forbidden(
                "an earlier call cost more than its hold, so model calls are stopped",
            ));
        }
        let hold = terms
            .prices
            .request_hold(self.input_bytes, self.max_output_tokens);
        let now = now_ms();
        let (since_ms, cap) = grant.account.allowance(now);
        let (account, provider) = (grant.account.key(), terms.upstream.provider.clone());
        let reservation = broker
            .store(move |store| {
                store.reserve(&HoldRequest {
                    account: &account,
                    provider: &provider,
                    since_ms,
                    cap_microusd: cap,
                    hold_microusd: hold,
                    at_ms: now,
                })
            })
            .await
            .map_err(failed)?;
        let id = match reservation {
            Reservation::Held(id) => id,
            Reservation::Refused { committed } => {
                // Only an account with a cap is refused for spend.
                let cap = cap.unwrap_or_default();
                *named = Some(ModelFailure::BudgetReached {
                    committed,
                    budget: cap,
                });
                return Err(forbidden(grant.account.cap_reached(committed, cap)));
            }
        };
        // Its own task, so a session that hangs up mid-call still gets its hold closed.
        let call = tokio::spawn(dispatch(
            broker.clone(),
            grant.clone(),
            id,
            hold,
            self.upstream,
        ));
        call.await.map_err(failed)?
    }
}

/// Send one held call upstream and close its hold: settled at the receipt's
/// cost, released if the provider failed, or charged at the hold if it
/// answered with a receipt that cannot be read or did not answer within the
/// grant's call limit, since the provider may still bill a request it got.
async fn dispatch(
    broker: AgentBroker,
    grant: Arc<Grant>,
    id: HoldId,
    hold: u64,
    body: Value,
) -> Reply {
    let terms = &grant.terms;
    let sent = broker.inner.provider.dispatch(body);
    let sent = match terms.call_limit {
        None => sent.await,
        Some(limit) => match tokio::time::timeout(limit, sent).await {
            Ok(sent) => sent,
            Err(_) => {
                broker
                    .store(move |store| store.charge_hold(id, now_ms()))
                    .await
                    .map_err(failed)?;
                return Err(forbidden(format!(
                    "the model call took longer than {limit:?}"
                )));
            }
        },
    };
    let mut response = match sent {
        Ok(response) => response,
        Err(error) => {
            broker
                .store(move |store| store.release(id, now_ms()))
                .await
                .map_err(failed)?;
            return Err(forbidden(error));
        }
    };
    let receipt = match Receipt::read(&response, terms.max_output_tokens) {
        Ok(receipt) => receipt,
        Err(error) => {
            broker
                .store(move |store| store.charge_hold(id, now_ms()))
                .await
                .map_err(failed)?;
            return Err(forbidden(error));
        }
    };
    let cost = receipt.cost(&terms.prices);
    let tokens = Tokens {
        input: receipt.input_tokens,
        cached_input: receipt.cached_input_tokens,
        output: receipt.output_tokens,
    };
    broker
        .store(move |store| store.settle(id, cost, tokens, &receipt.request_id, now_ms()))
        .await
        .map_err(failed)?;
    if cost > hold {
        if grant.account.capped() {
            broker.inner.overrun.store(true, Ordering::SeqCst);
            return Err(forbidden(
                "provider exceeded reservation; admission stopped",
            ));
        }
        // No cap means nothing here is bounded by the hold: the settlement
        // above is the record, and the call stands.
        eprintln!(
            "agent: a call on {} cost {cost} microusd, above its {hold} hold; it has no cap, so \
             the cost is recorded and the call stands",
            grant.account.key()
        );
    }
    response["swarm_cost_microusd"] = json!(cost);
    Ok(Json(response))
}

async fn tool(
    State(broker): State<AgentBroker>,
    RoutePath(name): RoutePath<String>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> std::result::Result<Response, Refusal> {
    let grant = broker.authenticate(&headers)?;
    match &grant.principal {
        Principal::Router => {
            let op = held(&broker.inner.router, &name)?;
            router_call(&broker, *op, args)
                .await
                .map(IntoResponse::into_response)
        }
        Principal::Worker(seat) => {
            let call = held(&seat.held, &name)?;
            let repeatable = seat.held.repeatable(&name);
            if let Some(refusal) = seat.repeat_refused(&name, &args, repeatable) {
                return Err(refusal);
            }
            call.run(&broker, seat, &name, args).await
        }
    }
}

/// The tool `name` as this principal's toolbox holds it. A tool the host
/// cannot serve is refused with the reason; a name no toolbox has is outside
/// the grants.
fn held<'a, C>(toolbox: &'a Toolbox<C>, name: &str) -> std::result::Result<&'a C, Refusal> {
    match toolbox.get(name) {
        Lookup::Held(call) => Ok(call),
        Lookup::Refused(why) => Err(forbidden(why)),
        Lookup::Unknown => Err(forbidden("tool outside admitted grants")),
    }
}

async fn router_call(broker: &AgentBroker, op: RouterOp, args: Value) -> Reply {
    match op {
        RouterOp::Decide => decide(broker, args).await,
        RouterOp::Read => read(broker, args).await,
    }
}

impl WorkerCall {
    /// Run the tool `name`. What a tool that is not repeatable answers is
    /// kept on the seat, so a refusal of the same call can say so.
    async fn run(
        &self,
        broker: &AgentBroker,
        seat: &Arc<WorkerSeat>,
        name: &str,
        args: Value,
    ) -> std::result::Result<Response, Refusal> {
        let asked = (!seat.held.repeatable(name)).then(|| args.clone());
        let reply = match self {
            Self::Browser(tool, browse) => {
                return browser::call(broker, seat, *tool, browse.clone(), args).await;
            }
            Self::Report => report(broker, seat, args).await,
            Self::Handback => handback(broker, seat, args).await,
            Self::RunCode(environment) => run_code(broker, seat, environment, args).await,
            Self::JobStart(environment) => {
                let host = job_host(broker, seat, environment.as_ref());
                seat.jobs.start(&host, args).map(Json).map_err(refused)
            }
            Self::JobWait => seat.jobs.wait(args).await.map(Json).map_err(refused),
            Self::JobStop => seat.jobs.stop(args).await.map(Json).map_err(refused),
            Self::FetchUrl(vault) => fetch_url(broker, seat, vault.as_deref(), args).await,
            Self::VaultList(vault) => secrets::vault_list(broker, seat, vault).await,
            Self::ReadPath(reader) => read_path(reader, args).await,
        };
        if let Some(asked) = asked {
            let answer = match &reply {
                Ok(Json(result)) => Ok(result),
                Err((_, Json(body))) => Err(body["error"].as_str().unwrap_or("refused")),
            };
            seat.answered(name, &asked, answer);
        }
        reply.map(IntoResponse::into_response)
    }
}

fn arguments<T: serde::de::DeserializeOwned>(args: Value) -> std::result::Result<T, Refusal> {
    serde_json::from_value(args).map_err(|err| {
        refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("bad arguments: {err}"),
        )
    })
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Choice {
    Accept,
    Ignore,
    Stop,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PlanKind {
    Session,
    Swarm,
}

/// `router_decide`'s arguments: one flat object, so every model's tool
/// calling can fill it. Fields that do not belong to the chosen decision
/// are ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecideArgs {
    eval: EvalId,
    decision: Choice,
    reason: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    plan: Option<PlanKind>,
    #[serde(default)]
    agents: Option<u16>,
    #[serde(default)]
    goal_template: Option<String>,
    #[serde(default)]
    environment: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    deliverable: Option<Deliverable>,
    #[serde(default)]
    budget_microusd: Option<MicroUsd>,
    #[serde(default)]
    acknowledge: Option<String>,
    #[serde(default)]
    rules: Vec<String>,
    #[serde(default)]
    ticket: Option<TicketId>,
}

impl DecideArgs {
    /// `origin`: the Source of the Trigger that gave the Eval its strength.
    fn verdict(
        self,
        terms: &DecideTerms,
        origin: Option<&str>,
    ) -> std::result::Result<(EvalId, Verdict), String> {
        let reason = self.reason.trim().to_owned();
        if reason.is_empty() {
            return Err("a decision needs a reason".into());
        }
        let verdict = match self.decision {
            Choice::Ignore => Verdict::Ignore { reason },
            Choice::Stop => Verdict::Stop {
                ticket: self.ticket.ok_or("a stop names the ticket to stop")?,
                reason,
            },
            Choice::Accept => {
                let repo = self
                    .repo
                    .as_deref()
                    .map(|name| terms.repo(name))
                    .transpose()?;
                let plan = match (self.plan.unwrap_or(PlanKind::Session), self.agents) {
                    (PlanKind::Session, None) if self.goal_template.is_none() => {
                        let method = match (&repo, self.deliverable) {
                            (None, None) => None,
                            (None, Some(_)) => {
                                return Err("deliverable is for an accept that names a repo".into());
                            }
                            (Some((name, terms)), owed) => {
                                let deliverable = owed.unwrap_or(terms.owes);
                                // What a repository owes is also the most the agent does
                                // there: a model may lower it for one request, never raise it.
                                if deliverable > terms.owes {
                                    return Err(format!(
                                        "deliverable {} is above what {name} owes: its config makes \
                                         {} the most the agent does there",
                                        deliverable.as_str(),
                                        terms.owes.as_str()
                                    ));
                                }
                                Some(Method { deliverable })
                            }
                        };
                        Plan::Session {
                            harness: None,
                            environment: self.environment,
                            method,
                        }
                    }
                    (PlanKind::Session, _) => {
                        return Err("agents and goal_template are for a swarm plan only".into());
                    }
                    (PlanKind::Swarm, Some(_)) if self.environment.is_some() => {
                        return Err("environment is for a session plan only".into());
                    }
                    (PlanKind::Swarm, Some(_)) if repo.is_some() || self.deliverable.is_some() => {
                        return Err("repo and deliverable are for a session plan only".into());
                    }
                    (PlanKind::Swarm, Some(agents)) => {
                        let swarm = terms.swarm.as_ref().ok_or(
                            "this agent runs no swarms; accept the work as a session instead",
                        )?;
                        swarm.admit(self.goal_template.as_deref())?;
                        Plan::Swarm {
                            agents,
                            goal_template: self.goal_template,
                        }
                    }
                    (PlanKind::Swarm, None) => return Err("a swarm plan names its agents".into()),
                };
                let repo_budget = repo.as_ref().and_then(|(_, terms)| terms.budget_microusd);
                let budget = match (&plan, terms.default_budget_microusd) {
                    (Plan::Swarm { .. }, session_default) => Some(
                        self.budget_microusd
                            .or(terms
                                .swarm
                                .as_ref()
                                .and_then(|swarm| swarm.default_budget_microusd))
                            .or(session_default)
                            .ok_or(
                                "a swarm plan needs budget_microusd, and this agent sets no \
                                 default budget for one",
                            )?,
                    ),
                    (Plan::Session { .. }, Some(default)) => {
                        Some(self.budget_microusd.or(repo_budget).unwrap_or(default))
                    }
                    // No session budget is configured, so none is named: a
                    // repository's own budget, set by the operator, still holds.
                    (Plan::Session { .. }, None) => repo_budget,
                };
                if let Some(budget) = budget
                    && let Some(max) = terms.ceiling(origin)
                    && budget > max
                {
                    return Err(match terms.max_budget_microusd {
                        Some(_) => format!(
                            "budget_microusd {budget} is above {max}, the most an accept may \
                             grant without a person's approval"
                        ),
                        None => format!(
                            "budget_microusd {budget} is above {max}, the most an accept may \
                             grant for a request from {}: its ticket could not ask a person to \
                             approve more. Name a budget of {max} or less",
                            origin.unwrap_or("an unknown source")
                        ),
                    });
                }
                // What the work may do on the forge comes from the repository's
                // config, for any deliverable above an answer: the Router names a
                // repository and a deliverable, never a merge mode or a limit.
                let forge = match (&repo, &plan) {
                    (
                        Some((_, terms)),
                        Plan::Session {
                            method: Some(method),
                            ..
                        },
                    ) if method.deliverable > Deliverable::Answer => Some(terms.terms()),
                    _ => None,
                };
                let acknowledge = match self.acknowledge {
                    None => Acknowledge::Standard,
                    Some(text) if text.trim().is_empty() => Acknowledge::Silent,
                    Some(text) => Acknowledge::Text(text),
                };
                Verdict::Accept(Accept {
                    title: self.title.unwrap_or_default(),
                    scope: self.scope.unwrap_or_default(),
                    reason,
                    plan,
                    budget_microusd: budget,
                    acknowledge,
                    rules: self.rules,
                    repo: repo.map(|(name, _)| name),
                    forge,
                })
            }
        };
        Ok((self.eval, verdict))
    }
}

/// `router_decide`: the Verdict goes to the runtime, and the Engine's answer
/// comes back, so a refusal reaches the model as a tool error.
async fn decide(broker: &AgentBroker, args: Value) -> Reply {
    let args: DecideArgs = arguments(args)?;
    let environments = &broker.inner.environments;
    if let Some(name) = &args.environment
        && !environments.knows(name)
    {
        return Err(refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "there is no environment {name}; the environments are {}",
                environments.names().join(", ")
            ),
        ));
    }
    if args.repo.is_some() && !environments.is_local(args.environment.as_deref()) {
        return Err(refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a ticket on a repository runs on this host only; name environment local",
        ));
    }
    let origin = broker.origin_source(&args.eval).await;
    let (eval, verdict) = args
        .verdict(&broker.inner.decide, origin.as_deref())
        .map_err(|why| refuse(StatusCode::UNPROCESSABLE_ENTITY, why))?;
    let answer = broker
        .inner
        .facts
        .ask(Fact::Verdict { eval, verdict })
        .await
        .map_err(failed)?;
    match answer {
        Ok(Applied::Decision(decision)) => Ok(Json(json!({
            "decision": decision.id,
            "kind": decision.kind,
            "ticket": decision.ticket,
            "would_accept": decision.would_accept,
            "reason": decision.reason,
        }))),
        Ok(_) => Ok(Json(json!({"applied": true}))),
        Err(why) => Err(refuse(StatusCode::CONFLICT, why)),
    }
}

/// `worker_report`'s arguments. The spend comes from the ledger, never
/// from the model.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportArgs {
    status: ReportStatus,
    text: String,
    #[serde(default)]
    question: Option<String>,
}

/// What the worker does after a Report the Engine took.
fn after(status: ReportStatus) -> &'static str {
    match status {
        ReportStatus::Progress => "Recorded; nobody was told. Keep working.",
        ReportStatus::Waiting => "Recorded. End your turn; new input continues the ticket.",
        ReportStatus::Ready => "Sent to whoever asked; the ticket has ended. Stop here.",
        ReportStatus::Dropped => "The ticket has ended. Stop here.",
        ReportStatus::Blocked | ReportStatus::Failed => {
            "A person has been asked. End your turn; their answer continues the ticket."
        }
    }
}

/// `worker_report`: the Report goes to the Engine with the Assignment's
/// spend so far. Long text is shortened rather than refused, so the model
/// is not left retrying a rejected call.
async fn report(broker: &AgentBroker, seat: &Arc<WorkerSeat>, args: Value) -> Reply {
    let ReportArgs {
        status,
        text,
        question,
    } = arguments(args)?;
    let (text, text_cut) = clip(text.trim(), REPORT_TEXT_CHARS);
    if text.is_empty() {
        return Err(refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a report needs a text",
        ));
    }
    let question = question
        .as_deref()
        .map(str::trim)
        .filter(|question| !question.is_empty())
        .map(|question| clip(question, QUESTION_CHARS));
    let question_cut = question.as_ref().is_some_and(|(_, cut)| *cut);
    // A `ready` on a Ticket that owes a change or a pull request is admitted by
    // the host before it is recorded, or refused with what to do next.
    let admitted = if status == ReportStatus::Ready {
        let admitted = broker.admit_ready(seat).await.map_err(|refused| {
            let status = if refused.conflict {
                StatusCode::CONFLICT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            refuse(status, refused.why)
        })?;
        Some(admitted)
    } else {
        None
    };
    let harvest = admitted
        .as_ref()
        .and_then(|admitted| admitted.harvest.clone());
    let spent = broker.spent(seat).await.map_err(failed)?;
    let fact = Fact::Report {
        ticket: seat.ticket.clone(),
        assignment: seat.assignment.clone(),
        report: ReportIn {
            status,
            text,
            question: question.map(|(question, _)| question),
            spent_microusd: spent,
            harvest,
        },
    };
    match broker.inner.facts.ask(fact).await.map_err(failed)? {
        Ok(_) => {
            seat.reported(status);
            let mut reply = json!({
                "recorded": true,
                "status": status,
                "spent_microusd": spent,
                "budget_microusd": seat.budget_microusd,
                "next": after(status),
            });
            if let Some(admitted) = &admitted {
                admitted.describe(&mut reply);
            }
            if text_cut || question_cut {
                reply["shortened"] = json!(format!(
                    "The text is kept to {REPORT_TEXT_CHARS} characters and a question to \
                     {QUESTION_CHARS}; the rest was cut."
                ));
            }
            Ok(Json(reply))
        }
        Err(why) => Err(refuse(StatusCode::CONFLICT, why)),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HandbackArgs {
    trigger: TriggerId,
    reason: String,
}

/// `worker_handback`: a follow-up that is not this Ticket's goes to the
/// Router, once.
async fn handback(broker: &AgentBroker, seat: &Arc<WorkerSeat>, args: Value) -> Reply {
    let HandbackArgs { trigger, reason } = arguments(args)?;
    let reason = reason.trim().to_owned();
    if reason.is_empty() {
        return Err(refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a handback needs a reason",
        ));
    }
    let fact = Fact::Handback {
        ticket: seat.ticket.clone(),
        trigger,
        reason,
    };
    let handed = broker.inner.facts.ask(fact).await.map_err(failed)?;
    if handed.is_ok() {
        seat.handed_back.store(true, Ordering::SeqCst);
    }
    match handed {
        Ok(Applied::HandedBack { queued }) => Ok(Json(json!({
            "handed_back": true,
            "queued": queued,
            "next": if queued {
                "The Router will judge it. Do not act on it yourself."
            } else {
                "It was handed back before or was already judged on its own. Do not act on it yourself."
            },
        }))),
        Ok(_) => Ok(Json(json!({"handed_back": true}))),
        Err(why) => Err(refuse(StatusCode::CONFLICT, why)),
    }
}

/// `run_code` in the Ticket's directory. Its receipt goes back as is; a
/// non-zero exit is a result, not an error. One still running after the
/// background threshold becomes a job.
async fn run_code(
    broker: &AgentBroker,
    seat: &Arc<WorkerSeat>,
    environment: &Env,
    args: Value,
) -> Reply {
    let host = job_host(broker, seat, environment.as_ref());
    seat.jobs
        .run_code(&host, &args)
        .await
        .map(Json)
        .map_err(refused)
}

/// Where `seat`'s jobs run: the environment its toolbox bound run_code to.
fn job_host<'a>(
    broker: &'a AgentBroker,
    seat: &'a WorkerSeat,
    environment: &'a dyn ExecutionEnvironment,
) -> Host<'a> {
    Host {
        environment,
        limits: broker.inner.jobs,
        facts: &broker.inner.facts,
        ticket: &seat.ticket,
        assignment: &seat.assignment,
        work_dir: &seat.work_dir,
    }
}

fn refused(refusal: Refused) -> Refusal {
    match refusal {
        Refused::BadArguments(why) => refuse(StatusCode::UNPROCESSABLE_ENTITY, why),
        Refused::Unknown(why) => refuse(StatusCode::NOT_FOUND, why),
        Refused::Full(why) => refuse(StatusCode::CONFLICT, why),
        Refused::Offline(why) => refuse(StatusCode::SERVICE_UNAVAILABLE, why),
    }
}

/// `fetch_url` with swarm mode's limits: each call's bounds in `fetch`, and
/// a read budget across the Assignment's calls. Every request, the first and
/// each redirect hop, keyed or not, goes through one [`FetchGuard`], which
/// refuses a private address after the name is resolved and sends only to
/// the address it vetted.
async fn fetch_url(
    broker: &AgentBroker,
    seat: &Arc<WorkerSeat>,
    vault: Option<&VaultParts>,
    mut args: Value,
) -> Reply {
    if seat.fetched_bytes.load(Ordering::SeqCst) >= FETCH_TOTAL_BYTES {
        return Err(forbidden(format!(
            "read budget exhausted after {} MiB; stop fetching and report what you have with worker_report",
            FETCH_TOTAL_BYTES / (1024 * 1024)
        )));
    }
    let key = args
        .as_object_mut()
        .and_then(|args| args.remove("key"))
        .filter(|key| !key.is_null());
    let guard = FetchGuard::open(broker.inner.allow_private_networks);
    let response = match key {
        None => fetch_guarded(&args, &guard)
            .await
            .map_err(|err| fetch_failed(seat, &err, str::to_owned))?,
        Some(key) => secrets::fetch(broker, seat, vault, &guard, key, &args).await?,
    };
    let bytes = response["body_bytes"].as_u64().unwrap_or(0);
    seat.fetched_bytes.fetch_add(bytes, Ordering::SeqCst);
    Ok(Json(response))
}

/// What a fetch that did not return a page comes to for the worker. The
/// guard's refusal of a private address is this host's rule doing its work:
/// the person running the host reads it in the log, with the address, and the
/// model reads it as a refusal, without the address. `clean` scrubs the text
/// the model gets.
fn fetch_failed(seat: &WorkerSeat, err: &anyhow::Error, clean: impl Fn(&str) -> String) -> Refusal {
    let Some(refused) = err.downcast_ref::<FetchRefused>() else {
        return refuse(StatusCode::BAD_GATEWAY, clean(&format!("{err:#}")));
    };
    let host = reqwest::Url::parse(&refused.url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default();
    let address = refused
        .address
        .map_or_else(String::new, |address| format!(" at {address}"));
    eprintln!(
        "agent: fetch_url of the worker of {} on {} was refused ({}): {host}{address}, hop {}",
        seat.assignment, seat.ticket, refused.reason, refused.hop
    );
    let reply = json!({
        "refused": true,
        "reason": refused.reason,
        "error": clean(&refused.message),
        "url": clean(&refused.url),
    });
    (StatusCode::FORBIDDEN, Json(reply))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadPathArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn read_path(reader: &Arc<Reader>, args: Value) -> Reply {
    let ReadPathArgs {
        path,
        offset,
        limit,
    } = arguments(args)?;
    let reader = reader.clone();
    tokio::task::spawn_blocking(move || reader.read(&path, offset, limit))
        .await
        .map_err(failed)?
        .map(Json)
        .map_err(|refusal| match refusal {
            pstack::Refused::Invalid(why) | pstack::Refused::NotText(why) => {
                refuse(StatusCode::UNPROCESSABLE_ENTITY, why)
            }
            pstack::Refused::Leaves(why) => forbidden(why),
            pstack::Refused::Missing(why) => refuse(StatusCode::NOT_FOUND, why),
            pstack::Refused::Host(why) => failed(why),
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Ticket,
    Trigger,
    Thread,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    id: String,
    /// Which kind of id it is, when a Trigger, a Thread or a Ticket could
    /// share it. Without it, a Ticket is found first, then a Trigger.
    #[serde(default)]
    kind: Option<Kind>,
}

/// `router_read`: one Trigger, Thread or Ticket as a short summary that
/// names its neighbours by id, so the model can follow them one hop at a
/// time.
async fn read(broker: &AgentBroker, args: Value) -> Reply {
    let ReadArgs { id, kind } = arguments(args)?;
    let shown = id.clone();
    let found = broker
        .store(move |store| recall(store, &id, kind))
        .await
        .map_err(failed)?;
    found.map(Json).ok_or_else(|| {
        refuse(
            StatusCode::NOT_FOUND,
            format!("no trigger, thread or ticket {shown}"),
        )
    })
}

fn recall(
    store: &SqliteStore,
    id: &str,
    kind: Option<Kind>,
) -> std::result::Result<Option<Value>, StoreError> {
    let wants = |k: Kind| kind.is_none_or(|kind| kind == k);
    if wants(Kind::Ticket)
        && let Some(ticket) = store.read_ticket(&TicketId::new(id))?
    {
        return Ok(Some(ticket_summary(&ticket)));
    }
    if wants(Kind::Trigger)
        && let Some(trigger) = store.read_trigger(&TriggerId::new(id))?
    {
        return trigger_summary(store, &trigger).map(Some);
    }
    if wants(Kind::Thread) {
        return thread_summary(store, &ThreadKey::new(id));
    }
    Ok(None)
}

/// At most `chars` characters of `text`, and whether any were cut.
pub(crate) fn clip(text: &str, chars: usize) -> (String, bool) {
    let mut kept: String = text.chars().take(chars).collect();
    let cut = kept.len() < text.len();
    if cut {
        kept.push_str("...");
    }
    (kept, cut)
}

/// The last `n` of `items`, and how many came before them.
fn recent<T: Clone>(items: &[T], n: usize) -> (Vec<T>, usize) {
    let earlier = items.len().saturating_sub(n);
    (items[earlier..].to_vec(), earlier)
}

fn ticket_summary(ticket: &Ticket) -> Value {
    let (scope, _) = clip(&ticket.mandate.scope, READ_TEXT_CHARS);
    let inputs: Vec<&TriggerId> = ticket
        .inputs
        .iter()
        .filter_map(|input| match &input.body {
            solos_agent_core::model::InputBody::Trigger { trigger } => Some(trigger),
            _ => None,
        })
        .collect();
    let (inputs, earlier_inputs) = recent(&inputs, READ_RECENT);
    let report = ticket.latest_report().map(|report| {
        json!({
            "status": report.status,
            "text": clip(&report.text, READ_REPORT_CHARS).0,
            "question": report.question,
        })
    });
    json!({
        "kind": "ticket",
        "id": ticket.id,
        "title": ticket.title,
        "stage": ticket.stage(),
        "scope": scope,
        "repo": ticket.mandate.repo,
        "plan": ticket.plan,
        "budget_microusd": ticket.mandate.budget_microusd,
        "requester": ticket.mandate.requester.id,
        "origin": ticket.origin,
        "threads": ticket.threads,
        "input_triggers": inputs,
        "earlier_input_triggers": earlier_inputs,
        "assignments": ticket.assignments.len(),
        "latest_report": report,
        "waiting_question": ticket.attention,
        "ended": ticket.ended.as_ref().map(|e| json!({"outcome": e.outcome, "by": e.by, "reason": clip(&e.reason, READ_REASON_CHARS).0})),
    })
}

fn trigger_summary(
    store: &SqliteStore,
    trigger: &Trigger,
) -> std::result::Result<Value, StoreError> {
    let (text, cut) = clip(&trigger.text, READ_TEXT_CHARS);
    let mut tickets = Vec::new();
    for key in &trigger.threads {
        let Some(thread) = store.read_thread(key)? else {
            continue;
        };
        for id in &thread.subscriptions {
            if tickets.contains(id) {
                continue;
            }
            let Some(ticket) = store.read_ticket(id)? else {
                continue;
            };
            let carries = ticket.origin == trigger.id
                || ticket.inputs.iter().any(|input| {
                    matches!(&input.body, solos_agent_core::model::InputBody::Trigger { trigger: t } if *t == trigger.id)
                });
            if carries {
                tickets.push(id.clone());
            }
        }
    }
    Ok(json!({
        "kind": "trigger",
        "id": trigger.id,
        "source": trigger.source,
        "at_ms": trigger.at_ms,
        "actor": trigger.actor,
        "addressed": trigger.addressed,
        "text": text,
        "text_clipped": cut,
        "threads": trigger.threads,
        "tickets": tickets,
    }))
}

fn thread_summary(
    store: &SqliteStore,
    key: &ThreadKey,
) -> std::result::Result<Option<Value>, StoreError> {
    let Some(thread) = store.read_thread(key)? else {
        return Ok(None);
    };
    let (triggers, earlier_triggers) = recent(&thread.triggers, READ_RECENT);
    let (decision_ids, earlier_decisions) = recent(&thread.decisions, READ_RECENT / 2);
    let mut decisions = Vec::new();
    for id in &decision_ids {
        if let Some(decision) = store.read_decision(id)? {
            decisions.push(json!({
                "id": decision.id,
                "kind": decision.kind,
                "reason": clip(&decision.reason, READ_REASON_CHARS).0,
                "ticket": decision.ticket,
                "triggers": decision.triggers,
            }));
        }
    }
    Ok(Some(json!({
        "kind": "thread",
        "id": thread.key,
        "triggers": triggers,
        "earlier_triggers": earlier_triggers,
        "queued": thread.queued,
        "eval": thread.eval.as_ref().map(|eval| &eval.id),
        "tickets": thread.subscriptions,
        "decisions": decisions,
        "earlier_decisions": earlier_decisions,
    })))
}

mod messages;

#[cfg(test)]
mod tests {
    use super::{
        Account, DecideArgs, DecideTerms, ModelFailure, Reply, SwarmTerms, WorkerSeat, clip, refuse,
    };
    use crate::agent::config::RepoConfig;
    use axum::{Json, http::StatusCode};
    use serde_json::json;
    use solos_agent_core::change::{Deliverable, RepoName};
    use solos_agent_core::model::{
        Acknowledge, AssignmentId, Method, Plan, ReportStatus, TicketId, Verdict,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn terms() -> DecideTerms {
        DecideTerms {
            default_budget_microusd: Some(500_000),
            max_budget_microusd: Some(2_000_000),
            swarm: Some(SwarmTerms {
                templates: vec!["audit".into(), "research".into()],
                default_template: Some("research".into()),
                default_budget_microusd: None,
            }),
            repos: BTreeMap::new(),
            ..DecideTerms::default()
        }
    }

    fn verdict(args: serde_json::Value) -> Result<Verdict, String> {
        verdict_under(&terms(), args)
    }

    fn verdict_under(terms: &DecideTerms, args: serde_json::Value) -> Result<Verdict, String> {
        let args: DecideArgs = serde_json::from_value(args).map_err(|e| e.to_string())?;
        args.verdict(terms, None).map(|(_, verdict)| verdict)
    }

    #[test]
    fn accounts_have_stable_keys_and_their_own_windows() {
        let router = Account::Router {
            daily_microusd: Some(7),
        };
        let day = 86_400_000;
        assert_eq!(router.key(), "router");
        assert_eq!(router.allowance(3 * day + 5), (3 * day, Some(7)));
        let ticket = Account::Ticket {
            id: TicketId::new("tk_1"),
            budget_microusd: Some(9),
        };
        assert_eq!(ticket.key(), "ticket:tk_1");
        assert_eq!(ticket.allowance(3 * day + 5), (0, Some(9)));
        assert!(router.capped() && ticket.capped());
    }

    #[test]
    fn an_account_with_no_cap_keeps_its_key_and_window_and_is_not_capped() {
        let day = 86_400_000;
        let router = Account::Router {
            daily_microusd: None,
        };
        assert_eq!(router.key(), "router");
        assert_eq!(router.allowance(3 * day + 5), (3 * day, None));
        let ticket = Account::Ticket {
            id: TicketId::new("tk_1"),
            budget_microusd: None,
        };
        assert_eq!(ticket.key(), "ticket:tk_1");
        assert_eq!(ticket.allowance(3 * day + 5), (0, None));
        assert!(!router.capped() && !ticket.capped());
    }

    #[test]
    fn decide_arguments_become_verdicts() {
        let Verdict::Accept(accept) = verdict(json!({
            "eval":"ev_1","decision":"accept","reason":"asked","title":"T","scope":"S"
        }))
        .unwrap() else {
            panic!("not an accept")
        };
        assert_eq!(
            accept.plan,
            Plan::Session {
                harness: None,
                environment: None,
                method: None,
            }
        );
        assert_eq!(accept.budget_microusd, Some(500_000));
        assert_eq!(accept.acknowledge, Acknowledge::Standard);
        let Verdict::Accept(remote) = verdict(json!({
            "eval":"ev_1","decision":"accept","reason":"asked","title":"T","scope":"S",
            "environment":"laptop"
        }))
        .unwrap() else {
            panic!("not an accept")
        };
        assert_eq!(
            remote.plan,
            Plan::Session {
                harness: None,
                environment: Some("laptop".into()),
                method: None,
            }
        );
        let Verdict::Accept(swarm) = verdict(json!({
            "eval":"ev_1","decision":"accept","reason":"wide","title":"T","scope":"S",
            "plan":"swarm","agents":4,"budget_microusd":2_000_000,"acknowledge":""
        }))
        .unwrap() else {
            panic!("not an accept")
        };
        assert_eq!(
            swarm.plan,
            Plan::Swarm {
                agents: 4,
                goal_template: None
            }
        );
        assert_eq!(swarm.acknowledge, Acknowledge::Silent);
        let Verdict::Accept(audit) = verdict(json!({
            "eval":"ev_1","decision":"accept","reason":"many checks","title":"T","scope":"S",
            "plan":"swarm","agents":3,"goal_template":"audit"
        }))
        .unwrap() else {
            panic!("not an accept")
        };
        assert_eq!(
            audit.plan,
            Plan::Swarm {
                agents: 3,
                goal_template: Some("audit".into())
            }
        );
        assert_eq!(
            verdict(json!({"eval":"ev_1","decision":"ignore","reason":"chatter","title":"x"})),
            Ok(Verdict::Ignore {
                reason: "chatter".into()
            })
        );
        assert_eq!(
            verdict(json!({"eval":"ev_1","decision":"stop","reason":"asked","ticket":"tk_1"})),
            Ok(Verdict::Stop {
                ticket: TicketId::new("tk_1"),
                reason: "asked".into()
            })
        );
    }

    #[test]
    fn decide_arguments_that_cannot_be_a_verdict_are_refused() {
        for (case, args) in [
            (
                "no reason",
                json!({"eval":"ev_1","decision":"ignore","reason":" "}),
            ),
            (
                "stop without ticket",
                json!({"eval":"ev_1","decision":"stop","reason":"r"}),
            ),
            (
                "swarm without agents",
                json!({"eval":"ev_1","decision":"accept","reason":"r","title":"T","scope":"S","plan":"swarm"}),
            ),
            (
                "session with agents",
                json!({"eval":"ev_1","decision":"accept","reason":"r","title":"T","scope":"S","agents":2}),
            ),
            (
                "over the approval threshold",
                json!({"eval":"ev_1","decision":"accept","reason":"r","title":"T","scope":"S","budget_microusd":2_000_001}),
            ),
            (
                "unknown field",
                json!({"eval":"ev_1","decision":"ignore","reason":"r","budget":1}),
            ),
            (
                "unknown decision",
                json!({"eval":"ev_1","decision":"defer","reason":"r"}),
            ),
            (
                "a goal template for a session",
                json!({"eval":"ev_1","decision":"accept","reason":"r","title":"T","scope":"S","goal_template":"audit"}),
            ),
            (
                "an unknown goal template",
                json!({"eval":"ev_1","decision":"accept","reason":"r","title":"T","scope":"S","plan":"swarm","agents":2,"goal_template":"tournament"}),
            ),
            (
                "an environment for a swarm",
                json!({"eval":"ev_1","decision":"accept","reason":"r","title":"T","scope":"S","plan":"swarm","agents":2,"environment":"laptop"}),
            ),
        ] {
            assert!(verdict(args).is_err(), "{case}");
        }
        let swarm = json!({"eval":"ev_1","decision":"accept","reason":"r","title":"T","scope":"S","plan":"swarm","agents":2});
        let none = DecideTerms {
            swarm: None,
            ..terms()
        };
        let refused = verdict_under(&none, swarm.clone()).unwrap_err();
        assert!(refused.contains("runs no swarms"), "{refused}");
        let undefaulted = DecideTerms {
            swarm: Some(SwarmTerms {
                templates: vec!["audit".into()],
                default_template: None,
                default_budget_microusd: None,
            }),
            ..terms()
        };
        let refused = verdict_under(&undefaulted, swarm).unwrap_err();
        assert!(
            refused.contains("names its goal_template, one of audit"),
            "{refused}"
        );
    }

    #[test]
    fn an_accept_names_a_configured_repo_and_owes_its_default_unless_it_names_a_deliverable() {
        let widgets = RepoName::try_from("acme/widgets".to_owned()).unwrap();
        let with_repos = DecideTerms {
            repos: BTreeMap::from([(
                widgets.clone(),
                RepoConfig {
                    about: "Widgets.".into(),
                    base: "main".into(),
                    budget_microusd: Some(1_500_000),
                    notes: None,
                    owes: Deliverable::PullRequest,
                    ..RepoConfig::default()
                },
            )]),
            ..terms()
        };
        let accept = |extra: serde_json::Value| {
            let mut args = json!({"eval":"ev_1","decision":"accept","reason":"asked","title":"T",
                                  "scope":"S","repo":"acme/widgets"});
            for (key, value) in extra.as_object().unwrap() {
                args[key] = value.clone();
            }
            verdict_under(&with_repos, args)
        };
        let owes = |deliverable| Plan::Session {
            harness: None,
            environment: None,
            method: Some(Method { deliverable }),
        };

        let Ok(Verdict::Accept(defaulted)) = accept(json!({})) else {
            panic!("refused")
        };
        assert_eq!(defaulted.repo, Some(widgets.clone()));
        assert_eq!(defaulted.plan, owes(Deliverable::PullRequest));
        assert_eq!(
            defaulted.budget_microusd,
            Some(1_500_000),
            "the repo's budget"
        );
        let Ok(Verdict::Accept(named)) =
            accept(json!({"deliverable":"answer","budget_microusd":700_000}))
        else {
            panic!("refused")
        };
        assert_eq!(named.plan, owes(Deliverable::Answer));
        assert_eq!(named.budget_microusd, Some(700_000));
        assert_eq!(named.forge, None, "an answer takes no forge terms");

        // What a repository owes also takes its terms from config, never from
        // the Router's words, and is the most the Router may name.
        assert_eq!(
            defaulted.forge,
            Some(with_repos.repos[&widgets].terms()),
            "a pull request carries the repository's terms"
        );
        let Ok(Verdict::Accept(lowered)) = accept(json!({"deliverable":"change"})) else {
            panic!("refused")
        };
        assert_eq!(lowered.plan, owes(Deliverable::Change));
        assert_eq!(lowered.forge, defaulted.forge);
        let read_only = DecideTerms {
            repos: BTreeMap::from([(
                widgets.clone(),
                RepoConfig {
                    about: "Widgets.".into(),
                    owes: Deliverable::Answer,
                    ..RepoConfig::default()
                },
            )]),
            ..terms()
        };
        let ask = |extra: serde_json::Value| {
            let mut args = json!({"eval":"ev_1","decision":"accept","reason":"asked","title":"T",
                                  "scope":"S","repo":"acme/widgets"});
            for (key, value) in extra.as_object().unwrap() {
                args[key] = value.clone();
            }
            verdict_under(&read_only, args)
        };
        let Ok(Verdict::Accept(read)) = ask(json!({})) else {
            panic!("refused")
        };
        assert_eq!((read.plan, read.forge), (owes(Deliverable::Answer), None));
        for raised in ["change", "pull_request"] {
            let refused = ask(json!({"deliverable": raised})).unwrap_err();
            assert!(
                refused.contains(&format!(
                    "deliverable {raised} is above what acme/widgets owes"
                )) && refused.contains("answer the most the agent does there"),
                "{refused}"
            );
        }

        for (extra, why) in [
            (
                json!({"repo":"acme/gadgets"}),
                "there is no repository acme/gadgets; the repositories are acme/widgets",
            ),
            (
                json!({"repo":"acme/../etc"}),
                "there is no repository acme/../etc; the repositories are acme/widgets",
            ),
            (
                json!({"plan":"swarm","agents":2}),
                "repo and deliverable are for a session plan only",
            ),
        ] {
            assert_eq!(accept(extra), Err(why.to_owned()));
        }
        let unscoped = json!({"eval":"ev_1","decision":"accept","reason":"asked","title":"T",
                              "scope":"S","deliverable":"change"});
        assert_eq!(
            verdict_under(&with_repos, unscoped),
            Err("deliverable is for an accept that names a repo".to_owned())
        );
        let elsewhere = json!({"eval":"ev_1","decision":"accept","reason":"asked","title":"T",
                               "scope":"S","repo":"acme/widgets"});
        assert_eq!(
            verdict(elsewhere),
            Err("this agent works on no repository; accept the work without a repo".to_owned())
        );
    }

    /// What an operator who sets no `budgets.session_microusd` gets.
    fn uncapped_sessions() -> DecideTerms {
        DecideTerms {
            default_budget_microusd: None,
            ..terms()
        }
    }

    fn accepted(terms: &DecideTerms, extra: serde_json::Value) -> Result<Option<u64>, String> {
        let mut args = json!({"eval":"ev_1","decision":"accept","reason":"asked","title":"T",
                              "scope":"S"});
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        match verdict_under(terms, args)? {
            Verdict::Accept(accept) => Ok(accept.budget_microusd),
            other => panic!("not an accept: {other:?}"),
        }
    }

    #[test]
    fn a_session_has_no_budget_where_the_operator_sets_none_and_a_named_one_is_not_used() {
        let terms = uncapped_sessions();
        assert_eq!(accepted(&terms, json!({})), Ok(None));
        assert_eq!(
            accepted(&terms, json!({"budget_microusd": 700_000})),
            Ok(None),
            "a budget is for a swarm here"
        );
        assert_eq!(
            accepted(&terms, json!({"budget_microusd": 9_000_000})),
            Ok(None),
            "so even one above the ceiling is not a refusal"
        );
        let capped = terms_with_default(Some(500_000));
        assert_eq!(accepted(&capped, json!({})), Ok(Some(500_000)));
        assert_eq!(
            accepted(&capped, json!({"budget_microusd": 700_000})),
            Ok(Some(700_000)),
            "where one is set, a session still takes the Router's"
        );
    }

    fn terms_with_default(default_budget_microusd: Option<u64>) -> DecideTerms {
        DecideTerms {
            default_budget_microusd,
            ..terms()
        }
    }

    #[test]
    fn a_repository_budget_the_operator_sets_still_holds_without_a_session_budget() {
        let widgets = RepoName::try_from("acme/widgets".to_owned()).unwrap();
        let repo = |budget_microusd| {
            BTreeMap::from([(
                widgets.clone(),
                RepoConfig {
                    about: "Widgets.".into(),
                    base: "main".into(),
                    budget_microusd,
                    notes: None,
                    owes: Deliverable::Answer,
                    ..RepoConfig::default()
                },
            )])
        };
        let with = |budget| DecideTerms {
            repos: repo(budget),
            ..uncapped_sessions()
        };
        let on_repo = json!({"repo": "acme/widgets", "budget_microusd": 700_000});
        assert_eq!(
            accepted(&with(Some(1_500_000)), on_repo.clone()),
            Ok(Some(1_500_000))
        );
        assert_eq!(accepted(&with(None), on_repo), Ok(None));
    }

    #[test]
    fn a_swarm_always_has_a_budget_from_the_router_the_swarm_or_the_session_default() {
        let swarm = |extra: serde_json::Value| {
            let mut args = json!({"plan": "swarm", "agents": 3});
            for (key, value) in extra.as_object().unwrap() {
                args[key] = value.clone();
            }
            args
        };
        let none = uncapped_sessions();
        let refused = accepted(&none, swarm(json!({}))).unwrap_err();
        assert!(
            refused.contains("a swarm plan needs budget_microusd"),
            "{refused}"
        );
        assert_eq!(
            accepted(&none, swarm(json!({"budget_microusd": 1_500_000}))),
            Ok(Some(1_500_000)),
            "the Router names it"
        );
        let defaulted = DecideTerms {
            swarm: Some(SwarmTerms {
                templates: vec!["research".into()],
                default_template: Some("research".into()),
                default_budget_microusd: Some(1_000_000),
            }),
            ..uncapped_sessions()
        };
        assert_eq!(
            accepted(&defaulted, swarm(json!({}))),
            Ok(Some(1_000_000)),
            "executor.swarm names it"
        );
        assert_eq!(
            accepted(&defaulted, swarm(json!({"budget_microusd": 1_500_000}))),
            Ok(Some(1_500_000)),
            "and the Router's wins"
        );
        assert_eq!(
            accepted(&terms(), swarm(json!({}))),
            Ok(Some(500_000)),
            "without one of its own, a swarm gets the session default as before"
        );
        let refused =
            accepted(&defaulted, swarm(json!({"budget_microusd": 2_000_001}))).unwrap_err();
        assert!(
            refused.contains("is above 2000000"),
            "a swarm's budget keeps its ceiling: {refused}"
        );
    }

    #[test]
    fn a_request_from_a_source_that_cannot_show_a_button_may_not_name_a_budget_that_needs_one() {
        let accept = |origin: Option<&str>, budget: u64| {
            let args = json!({"eval":"ev_1","decision":"accept","reason":"asked","title":"T",
                              "scope":"S","budget_microusd":budget});
            let args: DecideArgs = serde_json::from_value(args).unwrap();
            let terms = DecideTerms {
                max_budget_microusd: None,
                approval_sources: ["discord".to_owned()].into(),
                auto_approve_microusd: 500_000,
                ..terms()
            };
            args.verdict(&terms, origin).map(|_| ())
        };

        assert_eq!(
            accept(Some("discord"), 9_000_000),
            Ok(()),
            "Discord has buttons"
        );
        assert_eq!(accept(Some("github"), 500_000), Ok(()), "at the threshold");
        for origin in [Some("github"), Some("mcp"), Some("schedule"), None] {
            let refused = accept(origin, 500_001).unwrap_err();
            assert!(
                refused.contains("above 500000")
                    && refused.contains("its ticket could not ask a person"),
                "{origin:?}: {refused}"
            );
        }
        let everywhere = DecideTerms {
            max_budget_microusd: None,
            ..terms()
        };
        assert_eq!(
            everywhere.ceiling(Some("github")),
            None,
            "no per-source cap without a set"
        );
        let nowhere = terms();
        assert_eq!(nowhere.ceiling(Some("discord")), Some(2_000_000));
    }

    #[test]
    fn clipping_counts_characters_and_says_when_it_cut() {
        assert_eq!(clip("héllo", 5), ("héllo".into(), false));
        assert_eq!(clip("héllo", 2), ("hé...".into(), true));
    }

    fn seat() -> WorkerSeat {
        WorkerSeat::new(
            TicketId::new("tk_1"),
            AssignmentId::new("as_1"),
            PathBuf::from("/work"),
            500_000,
        )
    }

    fn refused(why: &str) -> Reply {
        Err(refuse(StatusCode::FORBIDDEN, why))
    }

    #[test]
    fn a_handback_belongs_to_the_turn_it_was_made_in() {
        let seat = seat();
        assert!(!seat.handed_back());
        seat.handed_back
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(seat.handed_back());
        seat.begin_turn();
        assert!(!seat.handed_back(), "a new turn starts clean");
    }

    #[test]
    fn a_turn_that_said_nothing_tells_why_its_model_call_failed_and_one_that_reported_does_not() {
        let seat = seat();
        seat.model_called(&refused("the provider is down"), None);
        assert_eq!(
            seat.take_model_failure(),
            Some(ModelFailure::Failed("the provider is down".into()))
        );
        assert_eq!(seat.take_model_failure(), None, "once");

        seat.model_called(&refused("the provider is down"), None);
        seat.reported(ReportStatus::Progress);
        assert!(
            seat.take_model_failure().is_some(),
            "progress tells nobody, so the turn still said nothing"
        );

        for status in [
            ReportStatus::Waiting,
            ReportStatus::Ready,
            ReportStatus::Blocked,
            ReportStatus::Failed,
            ReportStatus::Dropped,
        ] {
            seat.begin_turn();
            seat.model_called(&refused("the provider is down"), None);
            seat.reported(status);
            assert_eq!(seat.take_model_failure(), None, "{status:?} said it");
            assert_eq!(seat.take_model_failure(), None, "and it is not kept");
        }

        seat.begin_turn();
        let paused = Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"paused": true})),
        ));
        seat.model_called(&paused, None);
        assert_eq!(seat.take_model_failure(), None, "a pause is not a failure");
        seat.model_called(&refused("no"), None);
        seat.model_called(&Ok(Json(json!({}))), None);
        assert_eq!(
            seat.take_model_failure(),
            None,
            "the latest call went through"
        );
    }

    #[test]
    fn a_failed_model_call_is_reported_in_plain_words_for_the_person() {
        let (status, text) = ModelFailure::BudgetReached {
            committed: 344_865,
            budget: 500_000,
        }
        .report();
        assert_eq!(status, ReportStatus::Blocked, "a person must decide");
        assert_eq!(
            text,
            "I stopped before finishing: this request's budget ($0.50) cannot cover another \
             model call ($0.344865 of it is already committed). A request's budget is set when \
             it is accepted, so to go on, make a new request (not a reply to this one) and \
             name a bigger budget."
        );

        let (status, text) = ModelFailure::Failed("provider returned 503".into()).report();
        assert_eq!(status, ReportStatus::Failed);
        assert_eq!(
            text,
            "I stopped before finishing: the model call failed (provider returned 503). Reply \
             here and I will try again."
        );
        let (_, long) = ModelFailure::Failed("x".repeat(5_000)).report();
        assert!(long.chars().count() < 600, "a long reason is cut: {long}");

        let sentence = "I kept re-reading the same parts of encode.js without making progress, so \
                        I stopped.";
        let (status, text) = ModelFailure::Stuck(sentence.into()).report();
        assert_eq!(status, ReportStatus::Failed);
        assert_eq!(
            text,
            format!("{sentence} Reply here to have me try again with a different approach.")
        );
    }
}
