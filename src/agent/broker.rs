//! The agent's broker: the one gate between its model sessions and the model
//! and their tools. It speaks the swarm broker's wire (`POST /model`,
//! `POST /tool/{name}`, one bearer token per session, the same 503 pause
//! reply and `swarm_cost_microusd`), so the Pi extension works against
//! either. Each token names a principal, and every model call is held and
//! settled against the principal's account in the agent Store.

use super::AgentConfig;
use super::ports::{Applied, Fact, FactSink};
use crate::broker::{FETCH_TOTAL_BYTES, fetch, now_ms};
use crate::code::CodeExecutor;
use crate::gateway::{self, PriceTable, Provider, Receipt, Upstream};
use anyhow::{Context, Result, anyhow, ensure};
use axum::{
    Json,
    extract::{DefaultBodyLimit, Path as RoutePath, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde::Deserialize;
use serde_json::{Value, json};
use solos_agent_core::model::{
    Accept, Acknowledge, AssignmentId, EvalId, MicroUsd, Ms, Plan, ReportIn, ReportStatus,
    ThreadKey, Ticket, TicketId, Trigger, TriggerId, Verdict,
};
use solos_agent_store::{
    Charge, HoldId, HoldRequest, Reservation, SqliteStore, StoreError, Tokens,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::net::UnixListener;

/// The broker's socket in the state directory.
pub const SOCKET_FILE: &str = "broker.sock";
/// What the Router's sessions may call. No `run_code`, no `fetch_url`.
pub const ROUTER_TOOLS: [&str; 2] = ["router_decide", "router_read"];
/// What a worker's sessions may call. `run_code` needs the host to offer it.
pub const WORKER_TOOLS: [&str; 4] = ["worker_report", "worker_handback", "run_code", "fetch_url"];
const DAY_MS: Ms = 86_400_000;
/// `sockaddr_un.sun_path` is 108 bytes, with a trailing NUL.
const SUN_PATH_LIMIT: usize = 108;
const READ_TEXT_CHARS: usize = 2_000;
const READ_REPORT_CHARS: usize = 1_000;
const READ_REASON_CHARS: usize = 300;
const READ_RECENT: usize = 20;
const REPORT_TEXT_CHARS: usize = 8_000;
const QUESTION_CHARS: usize = 1_000;

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
}

/// One Assignment's worker as the broker knows it. The executor that
/// started the worker holds it too, to tell whether a turn reported.
#[derive(Debug)]
pub struct WorkerSeat {
    pub ticket: TicketId,
    pub assignment: AssignmentId,
    /// `run_code`'s `/work`.
    pub work_dir: PathBuf,
    /// The Ticket's whole budget, which every Assignment of it shares.
    pub budget_microusd: MicroUsd,
    /// The Ticket account's spend from this time on is this Assignment's.
    pub since_ms: Ms,
    /// The latest status the Engine took in the current turn.
    turn: Mutex<Option<ReportStatus>>,
    fetched_bytes: AtomicU64,
}

impl WorkerSeat {
    pub fn new(
        ticket: TicketId,
        assignment: AssignmentId,
        work_dir: PathBuf,
        budget_microusd: MicroUsd,
    ) -> Self {
        Self {
            ticket,
            assignment,
            work_dir,
            budget_microusd,
            since_ms: now_ms(),
            turn: Mutex::new(None),
            fetched_bytes: AtomicU64::new(0),
        }
    }

    pub fn account(&self) -> Account {
        Account::Ticket {
            id: self.ticket.clone(),
            budget_microusd: self.budget_microusd,
        }
    }

    /// A prompt went out: what the worker reports from now on is this turn's.
    pub fn begin_turn(&self) {
        if let Ok(mut turn) = self.turn.lock() {
            *turn = None;
        }
    }

    /// The turn left the Ticket as if still working: it reported nothing,
    /// or only progress.
    pub fn quiet_turn(&self) -> bool {
        self.turn.lock().map_or(true, |turn| {
            matches!(*turn, None | Some(ReportStatus::Progress))
        })
    }

    fn reported(&self, status: ReportStatus) {
        if let Ok(mut turn) = self.turn.lock() {
            *turn = Some(status);
        }
    }
}

/// Who pays for a model call, and how much it may commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Account {
    /// The Router's judging, capped per UTC day.
    Router { daily_microusd: MicroUsd },
    /// One Ticket's work, capped for its whole life by its Mandate's budget,
    /// which is fixed at accept time.
    Ticket {
        id: TicketId,
        budget_microusd: MicroUsd,
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

    /// Spend from the first time on counts against the cap.
    pub fn allowance(&self, now_ms: Ms) -> (Ms, MicroUsd) {
        match self {
            Self::Router { daily_microusd } => (now_ms - now_ms % DAY_MS, *daily_microusd),
            Self::Ticket {
                budget_microusd, ..
            } => (0, *budget_microusd),
        }
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

/// The budgets a Router's accept may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecideTerms {
    /// An accept that names no budget gets this.
    pub default_budget_microusd: MicroUsd,
    /// The most an accept may name, or no cap. Above the Policy's approval
    /// threshold a Ticket waits for a person's approval, so where nobody can
    /// give one the threshold is the cap, instead of a Ticket that waits
    /// forever.
    pub max_budget_microusd: Option<MicroUsd>,
}

impl DecideTerms {
    /// What `config` lets an accept name: above `policy.auto_approve_microusd`
    /// only when Slack can take the approval its Ticket then waits for.
    pub fn for_config(config: &AgentConfig) -> Self {
        Self {
            default_budget_microusd: config.budgets.session_microusd,
            max_budget_microusd: (!config.takes_approvals())
                .then_some(config.policy.auto_approve_microusd),
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

/// Every worker's model and its `run_code`.
pub struct WorkerParts {
    pub terms: ModelTerms,
    /// `None`: this host offers workers no `run_code`.
    pub code: Option<Arc<CodeExecutor>>,
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
    workers: Option<WorkerParts>,
    /// `fetch_url`'s egress, apart from the provider's transport.
    client: reqwest::Client,
    grants: RwLock<BTreeMap<String, Arc<Grant>>>,
    /// A settlement above its hold means the ledger no longer bounds spend,
    /// so every later model call is refused.
    overrun: AtomicBool,
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
        Ok(Self {
            inner: Arc::new(Inner {
                dir,
                store: Mutex::new(store),
                provider,
                facts,
                decide,
                workers,
                client: crate::swarm::http_client()?,
                grants: RwLock::new(BTreeMap::new()),
                overrun: AtomicBool::new(false),
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
        seat: WorkerSeat,
        system_prompt: String,
    ) -> Result<Arc<WorkerSeat>> {
        let parts = self
            .inner
            .workers
            .as_ref()
            .context("this broker admits no workers")?;
        let seat = Arc::new(seat);
        let grant = Grant {
            principal: Principal::Worker(seat.clone()),
            account: seat.account(),
            terms: parts.terms.clone(),
            system_prompt: Some(system_prompt),
        };
        self.admit(token, grant)?;
        Ok(seat)
    }

    /// Whether this broker offers workers `run_code`.
    pub fn offers_run_code(&self) -> bool {
        self.inner
            .workers
            .as_ref()
            .is_some_and(|parts| parts.code.is_some())
    }

    /// Refuse `token` from now on. A call already admitted finishes.
    pub fn revoke(&self, token: &str) {
        match self.inner.grants.write() {
            Ok(mut grants) => {
                grants.remove(token);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove(token);
            }
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
            .workers
            .as_ref()
            .map(|parts| parts.terms.max_input_bytes);
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
            .route("/session", post(session))
            // Above the admitted input on purpose, as in swarm mode: a
            // session replays its whole conversation, so the wire body may
            // exceed what the model limit admits, and is refused with a reason.
            .layer(DefaultBodyLimit::max(largest.saturating_mul(4).max(65_536)))
            .with_state(self.clone()))
    }

    fn authenticate(&self, headers: &HeaderMap) -> std::result::Result<Arc<Grant>, Refusal> {
        let token = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or_else(|| forbidden("missing identity"))?;
        self.inner
            .grants
            .read()
            .map_err(|_| failed("the grant table lock is poisoned"))?
            .get(token)
            .cloned()
            .ok_or_else(|| forbidden("invalid identity"))
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

async fn model(
    State(broker): State<AgentBroker>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    let grant = broker.authenticate(&headers)?;
    let terms = &grant.terms;
    let input_bytes = serde_json::to_vec(&body).map_err(forbidden)?.len();
    if input_bytes > terms.max_input_bytes {
        return Err(forbidden("request exceeds the model limit"));
    }
    // Before any hold: a paused call costs nothing and leaves no row.
    if let Some(wait) = gateway::effective_control(&broker.inner.dir, gateway::unix_secs()) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(gateway::paused_reply(wait)),
        ));
    }
    gateway::admit_request(&body, &terms.upstream.model, |name| {
        grant.principal.tools().contains(&name)
    })
    .map_err(forbidden)?;
    if broker.inner.overrun.load(Ordering::SeqCst) {
        return Err(forbidden(
            "an earlier call cost more than its hold, so model calls are stopped",
        ));
    }
    let hold = terms
        .prices
        .request_hold(input_bytes, terms.max_output_tokens);
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
            return Err(forbidden(grant.account.cap_reached(committed, cap)));
        }
    };
    let upstream = gateway::upstream_body(&terms.upstream.model, &body, terms.max_output_tokens);
    // Its own task, so a session that hangs up mid-call still gets its hold closed.
    let call = tokio::spawn(dispatch(broker.clone(), grant.clone(), id, hold, upstream));
    call.await.map_err(failed)?
}

/// Send one held call upstream and close its hold: settled at the receipt's
/// cost, released if the provider failed, or charged at the hold if it
/// answered with a receipt that cannot be read.
async fn dispatch(
    broker: AgentBroker,
    grant: Arc<Grant>,
    id: HoldId,
    hold: u64,
    body: Value,
) -> Reply {
    let terms = &grant.terms;
    let mut response = match broker.inner.provider.dispatch(body).await {
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
        broker.inner.overrun.store(true, Ordering::SeqCst);
        return Err(forbidden(
            "provider exceeded reservation; admission stopped",
        ));
    }
    response["swarm_cost_microusd"] = json!(cost);
    Ok(Json(response))
}

async fn tool(
    State(broker): State<AgentBroker>,
    RoutePath(name): RoutePath<String>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Reply {
    let grant = broker.authenticate(&headers)?;
    if !grant.principal.tools().contains(&name.as_str()) {
        return Err(forbidden("tool outside admitted grants"));
    }
    match (&grant.principal, name.as_str()) {
        (Principal::Router, "router_decide") => decide(&broker, args).await,
        (Principal::Router, "router_read") => read(&broker, args).await,
        (Principal::Worker(seat), "worker_report") => report(&broker, seat, args).await,
        (Principal::Worker(seat), "worker_handback") => handback(&broker, seat, args).await,
        (Principal::Worker(seat), "run_code") => run_code(&broker, seat, args).await,
        (Principal::Worker(seat), "fetch_url") => fetch_url(&broker, seat, args).await,
        _ => Err(forbidden("unknown tool")),
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
    budget_microusd: Option<MicroUsd>,
    #[serde(default)]
    acknowledge: Option<String>,
    #[serde(default)]
    rules: Vec<String>,
    #[serde(default)]
    ticket: Option<TicketId>,
}

impl DecideArgs {
    fn verdict(self, terms: DecideTerms) -> std::result::Result<(EvalId, Verdict), String> {
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
                let plan = match (self.plan.unwrap_or(PlanKind::Session), self.agents) {
                    (PlanKind::Session, None) => Plan::Session { harness: None },
                    (PlanKind::Session, Some(_)) => {
                        return Err("agents is for a swarm plan only".into());
                    }
                    (PlanKind::Swarm, Some(agents)) => Plan::Swarm {
                        agents,
                        goal_template: None,
                    },
                    (PlanKind::Swarm, None) => return Err("a swarm plan names its agents".into()),
                };
                let budget = self
                    .budget_microusd
                    .unwrap_or(terms.default_budget_microusd);
                if let Some(max) = terms.max_budget_microusd
                    && budget > max
                {
                    return Err(format!(
                        "budget_microusd {budget} is above {max}, the most an accept may grant \
                         without a person's approval"
                    ));
                }
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
    let (eval, verdict) = args
        .verdict(broker.inner.decide)
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
    let spent = broker.spent(seat).await.map_err(failed)?;
    let fact = Fact::Report {
        ticket: seat.ticket.clone(),
        assignment: seat.assignment.clone(),
        report: ReportIn {
            status,
            text,
            question: question.map(|(question, _)| question),
            spent_microusd: spent,
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
    match broker.inner.facts.ask(fact).await.map_err(failed)? {
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
/// non-zero exit is a result, not an error.
async fn run_code(broker: &AgentBroker, seat: &Arc<WorkerSeat>, args: Value) -> Reply {
    let code = broker
        .inner
        .workers
        .as_ref()
        .and_then(|parts| parts.code.clone())
        .ok_or_else(|| forbidden("run_code is not available on this host"))?;
    code.run_in(&seat.work_dir, &args)
        .await
        .map(Json)
        .map_err(|err| refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("{err:#}")))
}

/// `fetch_url` with swarm mode's limits: each call's bounds in `fetch`, and
/// a read budget across the Assignment's calls.
async fn fetch_url(broker: &AgentBroker, seat: &Arc<WorkerSeat>, args: Value) -> Reply {
    if seat.fetched_bytes.load(Ordering::SeqCst) >= FETCH_TOTAL_BYTES {
        return Err(forbidden(format!(
            "read budget exhausted after {} MiB; stop fetching and report what you have with worker_report",
            FETCH_TOTAL_BYTES / (1024 * 1024)
        )));
    }
    let response = fetch(&broker.inner.client, &args)
        .await
        .map_err(|err| refuse(StatusCode::BAD_GATEWAY, format!("{err:#}")))?;
    let bytes = response["body_bytes"].as_u64().unwrap_or(0);
    seat.fetched_bytes.fetch_add(bytes, Ordering::SeqCst);
    Ok(Json(response))
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

#[cfg(test)]
mod tests {
    use super::{Account, DecideArgs, DecideTerms, clip};
    use serde_json::json;
    use solos_agent_core::model::{Acknowledge, Plan, TicketId, Verdict};

    const TERMS: DecideTerms = DecideTerms {
        default_budget_microusd: 500_000,
        max_budget_microusd: Some(2_000_000),
    };

    fn verdict(args: serde_json::Value) -> Result<Verdict, String> {
        let args: DecideArgs = serde_json::from_value(args).map_err(|e| e.to_string())?;
        args.verdict(TERMS).map(|(_, verdict)| verdict)
    }

    #[test]
    fn accounts_have_stable_keys_and_their_own_windows() {
        let router = Account::Router { daily_microusd: 7 };
        let day = 86_400_000;
        assert_eq!(router.key(), "router");
        assert_eq!(router.allowance(3 * day + 5), (3 * day, 7));
        let ticket = Account::Ticket {
            id: TicketId::new("tk_1"),
            budget_microusd: 9,
        };
        assert_eq!(ticket.key(), "ticket:tk_1");
        assert_eq!(ticket.allowance(3 * day + 5), (0, 9));
    }

    #[test]
    fn decide_arguments_become_verdicts() {
        let Verdict::Accept(accept) = verdict(json!({
            "eval":"ev_1","decision":"accept","reason":"asked","title":"T","scope":"S"
        }))
        .unwrap() else {
            panic!("not an accept")
        };
        assert_eq!(accept.plan, Plan::Session { harness: None });
        assert_eq!(accept.budget_microusd, 500_000);
        assert_eq!(accept.acknowledge, Acknowledge::Standard);
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
        ] {
            assert!(verdict(args).is_err(), "{case}");
        }
    }

    #[test]
    fn clipping_counts_characters_and_says_when_it_cut() {
        assert_eq!(clip("héllo", 5), ("héllo".into(), false));
        assert_eq!(clip("héllo", 2), ("hé...".into(), true));
    }
}
