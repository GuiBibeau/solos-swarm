use super::secrets::{ask_first_use, use_context};
use super::{AgentBroker, Refusal, WorkerSeat, arguments, failed, forbidden, refuse};
use crate::agent::environment::LOCAL;
use crate::agent::ports::{Applied, BoxFuture, Fact};
use crate::agent::trail::BlobStore;
use crate::broker::FETCH_TOTAL_BYTES;
use crate::browse::{Blob, BlobKind, BrowseCall, Browsed, Browsers, NO_SESSION, VIEW_BYTES};
use crate::vault::{GrantState, SecretName};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::Value;
use solos_agent_core::model::TicketId;
use solos_agent_core::trail::{
    ActText, CloseReason, Digest, PageAction, SignedStep, StepOutcome, TrailAction, TrailKey,
    TrailOutcome,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::task::{JoinHandle, JoinSet};

const REAP_EVERY: Duration = Duration::from_secs(60);
const TYPED_CHARS: usize = 4_000;

/// Where a Ticket's browser runs.
pub trait BrowserPlace: Send + Sync {
    /// What a signed step's `environment` must name.
    fn name(&self) -> &str;
    /// The key its steps must verify under, pinned as the agent starts.
    fn trusted(&self) -> &TrailKey;
    fn act(&self, call: BrowseCall) -> BoxFuture<'_, Browsed>;
    /// A signed refusal of `call` that acts on nothing.
    fn refuse(&self, call: &BrowseCall, why: String) -> Browsed;
}

pub struct LocalBrowser {
    browsers: Arc<Browsers>,
    trusted: TrailKey,
}

impl LocalBrowser {
    pub fn new(browsers: Arc<Browsers>) -> Self {
        let trusted = browsers.key();
        Self { browsers, trusted }
    }
}

impl BrowserPlace for LocalBrowser {
    fn name(&self) -> &str {
        LOCAL
    }
    fn trusted(&self) -> &TrailKey {
        &self.trusted
    }
    fn act(&self, call: BrowseCall) -> BoxFuture<'_, Browsed> {
        Box::pin(self.browsers.act(call))
    }
    fn refuse(&self, call: &BrowseCall, why: String) -> Browsed {
        self.browsers.refuse(call, why)
    }
}

pub struct BrowserParts {
    place: Arc<dyn BrowserPlace>,
    blobs: Arc<BlobStore>,
    idle: Duration,
    closing: AtomicBool,
}

impl BrowserParts {
    /// A browser with no step for `idle` is closed.
    pub fn new(place: Arc<dyn BrowserPlace>, blobs: BlobStore, idle: Duration) -> Self {
        Self {
            place,
            blobs: Arc::new(blobs),
            idle,
            closing: AtomicBool::new(false),
        }
    }
}

/// Held for a whole step, so a seat's steps run one at a time when an MCP
/// host calls its tools at once.
#[derive(Debug, Default)]
pub struct SeatBrowser {
    open_since_last_step: tokio::sync::Mutex<Option<Instant>>,
}

pub(super) async fn call(
    broker: &AgentBroker,
    seat: &Arc<WorkerSeat>,
    tool: &str,
    args: Value,
) -> Result<Response, Refusal> {
    let (action, typed) = action_of(tool, args)?;
    if seat.fetched_bytes.load(Ordering::SeqCst) >= FETCH_TOTAL_BYTES {
        return Err(forbidden(format!(
            "read budget exhausted after {} MiB; stop browsing and report what you have with \
             worker_report",
            FETCH_TOTAL_BYTES / (1024 * 1024)
        )));
    }
    let (broker, seat) = (broker.clone(), seat.clone());
    detached(async move {
        let mut open = seat.browser.open_since_last_step.lock().await;
        let parts = browser(&broker)?;
        if parts.closing.load(Ordering::SeqCst) {
            return Err(refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "the agent is stopping, so no browser step starts",
            ));
        }
        let answered = step(&broker, &seat, &mut open, action, typed).await?;
        show(&broker, &seat, answered).await
    })
    .await
    .map_err(failed)?
}

enum Answered {
    Signed(Box<Verified>),
    Lost(String),
}

struct Verified {
    step: SignedStep,
    blobs: Vec<Blob>,
}

struct Asked {
    call: BrowseCall,
    room: Result<(), String>,
    place: Arc<dyn BrowserPlace>,
    blobs: Arc<BlobStore>,
}

async fn step(
    broker: &AgentBroker,
    seat: &WorkerSeat,
    open: &mut Option<Instant>,
    action: TrailAction,
    typed: Option<String>,
) -> Result<Answered, Refusal> {
    let asked = ask_step(broker, seat, action, typed).await?;
    let answered = answer_step(broker, asked).await?;
    *open = open_since(*open, &answered);
    Ok(answered)
}

async fn ask_step(
    broker: &AgentBroker,
    seat: &WorkerSeat,
    action: TrailAction,
    typed: Option<String>,
) -> Result<Asked, Refusal> {
    let parts = browser(broker)?;
    let context = use_context(broker, seat).await?;
    let room = if keeps_blobs(&action) {
        parts.blobs.room(&seat.ticket)
    } else {
        Ok(())
    };
    if room.is_ok()
        && let Some(text) = &typed
    {
        keep(&parts.blobs, &seat.ticket, vec![text.as_bytes().to_vec()])
            .await
            .map_err(failed)?;
    }
    let asking = Fact::TrailAsk {
        ticket: seat.ticket.clone(),
        assignment: seat.assignment.clone(),
        action: action.clone(),
    };
    let n = match ask(broker, asking).await? {
        Applied::TrailAsked { n } => n,
        other => return Err(failed(format!("a trail ask was answered with {other:?}"))),
    };
    let call = BrowseCall {
        ticket: seat.ticket.clone(),
        assignment: seat.assignment.clone(),
        n,
        action,
        typed,
        context,
    };
    Ok(Asked {
        call,
        room,
        place: parts.place.clone(),
        blobs: parts.blobs.clone(),
    })
}

async fn answer_step(broker: &AgentBroker, asked: Asked) -> Result<Answered, Refusal> {
    let Asked {
        call,
        room,
        place,
        blobs,
    } = asked;
    let answered = match detached(answer(place, blobs, call.clone(), room)).await {
        Ok(Ok(verified)) => Answered::Signed(Box::new(verified)),
        Ok(Err(why)) => Answered::Lost(why),
        Err(lost) => Answered::Lost(format!("the step's answer was lost: {lost}")),
    };
    done(broker, &call, answered).await
}

fn keeps_blobs(action: &TrailAction) -> bool {
    !matches!(action, TrailAction::Close { .. })
}

async fn answer(
    place: Arc<dyn BrowserPlace>,
    blobs: Arc<BlobStore>,
    call: BrowseCall,
    room: Result<(), String>,
) -> Result<Verified, String> {
    let browsed = match room {
        Ok(()) => place.act(call.clone()).await,
        Err(why) => place.refuse(&call, why),
    };
    let verified = verify(place.as_ref(), &call, browsed)?;
    let bytes = verified
        .blobs
        .iter()
        .map(|blob| blob.bytes.clone())
        .collect();
    if let Err(err) = keep(&blobs, &call.ticket, bytes).await {
        eprintln!(
            "agent: the blobs of step {} of {} were not kept: {err:#}",
            call.n, call.assignment
        );
    }
    Ok(verified)
}

fn verify(
    place: &dyn BrowserPlace,
    call: &BrowseCall,
    browsed: Browsed,
) -> Result<Verified, String> {
    crate::browse::verify_step(call, &browsed, place.trusted())?;
    let environment = &browsed.step.body.environment;
    if environment != place.name() {
        return Err(format!(
            "the step says it ran on {environment}, not on {}",
            place.name()
        ));
    }
    if let Some(view) = view(&browsed.blobs)
        && view.len() > VIEW_BYTES
    {
        return Err(format!(
            "the view is {} bytes, past the {VIEW_BYTES} a view may be",
            view.len()
        ));
    }
    Ok(Verified {
        step: browsed.step,
        blobs: browsed.blobs,
    })
}

fn view(blobs: &[Blob]) -> Option<&[u8]> {
    blobs
        .iter()
        .find(|blob| blob.kind == BlobKind::View)
        .map(|blob| blob.bytes.as_slice())
}

async fn keep(blobs: &Arc<BlobStore>, ticket: &TicketId, each: Vec<Vec<u8>>) -> anyhow::Result<()> {
    let (blobs, ticket) = (blobs.clone(), ticket.clone());
    tokio::task::spawn_blocking(move || {
        each.iter()
            .try_for_each(|bytes| blobs.put(&ticket, bytes).map(drop))
    })
    .await?
}

async fn done(
    broker: &AgentBroker,
    call: &BrowseCall,
    answered: Answered,
) -> Result<Answered, Refusal> {
    let outcome = match &answered {
        Answered::Signed(verified) => TrailOutcome::signed(verified.step.clone()),
        Answered::Lost(why) => TrailOutcome::Lost { why: why.clone() },
    };
    let fact = |outcome| Fact::TrailDone {
        ticket: call.ticket.clone(),
        assignment: call.assignment.clone(),
        n: call.n,
        outcome,
    };
    match (ask(broker, fact(outcome)).await, answered) {
        (Ok(_), answered) => Ok(answered),
        (Err(why), Answered::Signed(_)) => {
            let why = format!("the core refused the signed step: {}", error(&why));
            ask(broker, fact(TrailOutcome::Lost { why: why.clone() })).await?;
            Ok(Answered::Lost(why))
        }
        (Err(refused), Answered::Lost(_)) => Err(refused),
    }
}

fn open_since(open: Option<Instant>, answered: &Answered) -> Option<Instant> {
    let still_open = match answered {
        Answered::Lost(_) => true,
        Answered::Signed(verified) => {
            let body = &verified.step.body;
            !matches!(body.outcome, StepOutcome::Closed { .. })
                && (open.is_some() || body.session != NO_SESSION)
        }
    };
    still_open.then(Instant::now)
}

async fn show(
    broker: &AgentBroker,
    seat: &WorkerSeat,
    answered: Answered,
) -> Result<Response, Refusal> {
    let verified = match answered {
        Answered::Signed(verified) => verified,
        Answered::Lost(why) => {
            return Err(refuse(
                StatusCode::BAD_GATEWAY,
                format!("the browser's answer did not verify, so it is recorded as lost: {why}"),
            ));
        }
    };
    let body = &verified.step.body;
    match &body.outcome {
        StepOutcome::Page(_) => {
            let view = view(&verified.blobs)
                .ok_or_else(|| failed("a page step came with no view"))?
                .to_vec();
            seat.fetched_bytes
                .fetch_add(view.len() as u64, Ordering::SeqCst);
            Ok(([(header::CONTENT_TYPE, "application/json")], view).into_response())
        }
        StepOutcome::NeedsApproval { secret, label } => {
            Err(approval(broker, seat, &body.action, secret, label).await)
        }
        StepOutcome::Refused { why } => Err(refuse(StatusCode::CONFLICT, why)),
        StepOutcome::Closed { .. } => Err(refuse(
            StatusCode::BAD_GATEWAY,
            "the browser stopped and its session is closed; the next browser call starts a new \
             one, signed out",
        )),
    }
}

async fn approval(
    broker: &AgentBroker,
    seat: &WorkerSeat,
    action: &TrailAction,
    secret: &str,
    label: &str,
) -> Refusal {
    let TrailAction::Login { login, .. } = action else {
        return failed("a step that fills no login asked for an approval");
    };
    let Some(vault) = broker.vault() else {
        return failed("a login asked for an approval on a host with no vault");
    };
    let context = match use_context(broker, seat).await {
        Ok(context) => context,
        Err(refusal) => return refusal,
    };
    let name = login.parse::<SecretName>().ok();
    let item = vault.vault.catalog(&context).ok().and_then(|items| {
        items
            .into_iter()
            .find(|item| Some(&item.name) == name.as_ref())
    });
    let Some(item) = item else {
        return forbidden(format!("there is no login {login} for this ticket here"));
    };
    let asked_before = context.grants.get(&item.name) == Some(&GrantState::Waiting);
    let answerable_by = item.users.into_iter().collect();
    ask_first_use(
        broker,
        seat,
        secret.to_owned(),
        label.to_owned(),
        answerable_by,
        asked_before,
    )
    .await
}

/// Close `seat`'s browser with a signed step, if one may be open.
pub async fn end(broker: &AgentBroker, seat: &Arc<WorkerSeat>, reason: CloseReason) {
    close(broker.clone(), seat.clone(), reason, None).await;
}

/// Close every admitted worker's browser with a signed step, and start no
/// step after.
pub async fn close_all(broker: AgentBroker) {
    let Ok(parts) = browser(&broker) else {
        return;
    };
    parts.closing.store(true, Ordering::SeqCst);
    let mut closing = JoinSet::new();
    for seat in broker.worker_seats() {
        closing.spawn(close(broker.clone(), seat, CloseReason::Shutdown, None));
    }
    while closing.join_next().await.is_some() {}
}

/// Close each admitted worker's browser that has had no step for the
/// browser's idle time. `None` where this host has no browser.
pub fn spawn_reaper(broker: &AgentBroker) -> Option<JoinHandle<()>> {
    let idle = browser(broker).ok()?.idle;
    let broker = broker.clone();
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(idle.min(REAP_EVERY));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            for seat in broker.worker_seats() {
                let due = seat
                    .browser
                    .open_since_last_step
                    .try_lock()
                    .is_ok_and(|open| open.is_some_and(|at| at.elapsed() >= idle));
                if due {
                    tokio::spawn(close(broker.clone(), seat, CloseReason::Idle, Some(idle)));
                }
            }
        }
    }))
}

async fn close(
    broker: AgentBroker,
    seat: Arc<WorkerSeat>,
    reason: CloseReason,
    idle: Option<Duration>,
) {
    let (who, ticket) = (seat.assignment.clone(), seat.ticket.clone());
    let closed = detached(async move {
        let mut open = seat.browser.open_since_last_step.lock().await;
        let due = match (*open, idle) {
            (None, _) => false,
            (Some(at), Some(idle)) => at.elapsed() >= idle,
            (Some(_), None) => true,
        };
        if !due {
            return Ok(());
        }
        let close = TrailAction::Close { reason };
        step(&broker, &seat, &mut open, close, None).await.map(drop)
    })
    .await;
    let why = match closed {
        Ok(Ok(())) => return,
        Ok(Err((_, why))) => why.0.to_string(),
        Err(lost) => lost.to_string(),
    };
    eprintln!("agent: the browser of {who} on {ticket} was not closed: {why}");
}

/// Run `work` on its own task. A caller that is dropped or aborted, such as
/// a request whose session hung up or a stop past its timeout, does not cut
/// it short, so a step asked on the chain is still answered there.
fn detached<T: Send + 'static>(work: impl Future<Output = T> + Send + 'static) -> JoinHandle<T> {
    tokio::spawn(work)
}

fn browser(broker: &AgentBroker) -> Result<&BrowserParts, Refusal> {
    broker
        .browser()
        .ok_or_else(|| failed("a browser tool was admitted without a browser"))
}

async fn ask(broker: &AgentBroker, fact: Fact) -> Result<Applied, Refusal> {
    broker
        .inner
        .facts
        .ask(fact)
        .await
        .map_err(failed)?
        .map_err(|why| refuse(StatusCode::CONFLICT, why))
}

fn error(refusal: &Refusal) -> String {
    refusal.1.0["error"].as_str().unwrap_or_default().to_owned()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenArgs {
    url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActArgs {
    action: PageAction,
    element: Option<u32>,
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginArgs {
    login: String,
    username_element: Option<u32>,
    password_element: Option<u32>,
    submit: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    part: Option<u32>,
    element: Option<u32>,
}

fn action_of(tool: &str, args: Value) -> Result<(TrailAction, Option<String>), Refusal> {
    let action = match tool {
        "browser_open" => {
            let OpenArgs { url } = arguments(args)?;
            TrailAction::Open { url, key: None }
        }
        "browser_act" => {
            let ActArgs {
                action,
                element,
                text,
            } = arguments(args)?;
            if action != PageAction::Type {
                let text = text.map(|text| ActText::Named { text });
                return Ok((
                    TrailAction::Act {
                        action,
                        element,
                        text,
                    },
                    None,
                ));
            }
            let text = text
                .ok_or_else(|| refuse(StatusCode::UNPROCESSABLE_ENTITY, "type needs its text"))?;
            let chars = text.chars().count();
            if chars > TYPED_CHARS {
                return Err(refuse(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("type takes at most {TYPED_CHARS} characters"),
                ));
            }
            let typed = ActText::Typed {
                digest: Digest::of(text.as_bytes()),
                chars: u32::try_from(chars).map_err(failed)?,
            };
            let action = TrailAction::Act {
                action,
                element,
                text: Some(typed),
            };
            return Ok((action, Some(text)));
        }
        "browser_login" => {
            let LoginArgs {
                login,
                username_element,
                password_element,
                submit,
            } = arguments(args)?;
            TrailAction::Login {
                login,
                username_element,
                password_element,
                submit: submit.unwrap_or(true),
            }
        }
        "browser_read" => {
            let ReadArgs { part, element } = arguments(args)?;
            TrailAction::Read { part, element }
        }
        _ => return Err(forbidden("unknown tool")),
    };
    Ok((action, None))
}
