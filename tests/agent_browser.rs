use anyhow::Result;
use axum::response::Html;
use axum::routing::{get, post};
use serde_json::{Value, json};
use solos_agent_core::model::{
    Actor, Addressed, Ask, AssignmentId, ChangeSet, ChannelRef, DomainEvent, Effect, Event,
    InputBody, SecretGrant, SecretId, ThreadKey, Ticket, TicketId, TicketOutcome, Trigger,
    TriggerId,
};
use solos_agent_core::trail::{
    ActText, CloseReason, Digest, Pending, Signature, SignedStep, StepOutcome, TrailAction,
    TrailKey, TrailOutcome, answers,
};
use solos_agent_core::{AllowlistPolicy, Repositories};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::broker::browser::{self, BrowserParts, BrowserPlace, LocalBrowser};
use solos_swarm::agent::broker::{
    self, AgentBroker, BrokerParts, DecideTerms, ModelTerms, VaultParts, WorkerParts, WorkerSeat,
};
use solos_swarm::agent::jsonl::{JsonlChannel, JsonlSource};
use solos_swarm::agent::ports::{
    BoxFuture, Channel, Command, Delivery, Executor, Fact, FactSink, Launch, Order,
};
use solos_swarm::agent::scripted::ScriptedRouter;
use solos_swarm::agent::trail::{
    BlobStore, Found, Problem, StepAt, TrailReport, TrailStoreConfig, Trust, Trusted, render,
    verify,
};
use solos_swarm::agent::{
    Adapters, AgentConfig, BROWSER_DIR, HOST_RESTARTED, PiHost, RunOptions, Runtime, STORE_FILE,
    Summary, serve_with,
};
use solos_swarm::audit::CryptoSigner;
use solos_swarm::browse::fake::{Element, FakeCdp, Form, Page};
use solos_swarm::browse::{
    Blob, BlobKind, BrowseCall, Browsed, BrowserConfig, Browsers, Launcher, TRAIL_DOMAIN,
    TrailSigner, verify_step,
};
use solos_swarm::gateway::{Dispatch, PriceTable, Provider, Upstream};
use solos_swarm::harness::scripted::{Call, ScriptedHarness};
use solos_swarm::vault::{
    Approval, ItemKind, NewItem, SandboxView, SecretValue, UseContext, Vault, VaultConfig,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const LIMIT: Duration = Duration::from_secs(60);
const SITE: &str = "https://site.example";
const PASSWORD: &str = "pw-Probe-7f3a9c-gui";
const LOGIN: &str = "site-login";
const BUDGET: u64 = 500_000;

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "agent-browser-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("state")).unwrap();
    std::fs::create_dir_all(root.join("keys")).unwrap();
    root
}

fn page(url: &str, title: &str) -> Page {
    Page {
        url: url.into(),
        title: title.into(),
        status: 200,
        text: format!("{title} page"),
        ..Page::default()
    }
}

fn login_site_pages() -> Vec<Page> {
    vec![
        Page {
            url: format!("{SITE}/login"),
            title: "Sign in".into(),
            status: 200,
            text: "Sign in to the site".into(),
            elements: vec![
                Element::input("text", "Username"),
                Element::input("password", "Password"),
                Element::button("Sign in"),
                Element::textarea("Comment"),
                Element::link("Next", &format!("{SITE}/next")),
            ],
            forms: vec![Form {
                action: Some("/session".into()),
                fields: vec![0, 1, 3],
                submit: Some(2),
            }],
            echo: false,
        },
        page(&format!("{SITE}/session"), "Signed in"),
        page(&format!("{SITE}/next"), "Next"),
    ]
}

fn vault_with_login(root: &Path, site: &str, users: &[&str], approval: Approval) -> VaultConfig {
    let config = VaultConfig {
        path: root.join("keys").join("vault.enc"),
        key_file: root.join("keys").join("vault.key"),
    };
    let view = SandboxView::new([]);
    Vault::init(&config, &view).unwrap();
    let vault = Vault::open(&config, &view).unwrap();
    let value = root.join("keys").join("password");
    std::fs::write(&value, PASSWORD).unwrap();
    let login = NewItem {
        name: LOGIN.parse().unwrap(),
        kind: ItemKind::Login {
            sites: vec![site.parse().unwrap()],
            username: "gui".into(),
            hint: "the test site".into(),
        },
        users: users.iter().map(|user| (*user).to_owned()).collect(),
        approval,
        value: SecretValue::read_once(&value).unwrap(),
    };
    vault.add(login, false).unwrap();
    config
}

#[derive(Clone, Debug)]
enum Logged {
    Asked(u32, TrailAction),
    Done(u32, TrailOutcome),
}

fn trail(store: &Path) -> Vec<Logged> {
    let Ok(store) = SqliteStore::open_read_only(store) else {
        return Vec::new();
    };
    store
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            DomainEvent::TrailAsked { n, action, .. } => Some(Logged::Asked(n, action)),
            DomainEvent::TrailDone { n, outcome, .. } => Some(Logged::Done(n, outcome)),
            _ => None,
        })
        .collect()
}

fn kind<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value).unwrap()["kind"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn lines(trail: &[Logged]) -> Vec<String> {
    trail
        .iter()
        .map(|logged| match logged {
            Logged::Asked(n, action) => format!("asked {n} {}", kind(action)),
            Logged::Done(n, TrailOutcome::Signed(step)) => {
                format!("done {n} {}", kind(&step.body.outcome))
            }
            Logged::Done(n, TrailOutcome::Lost { .. }) => format!("done {n} lost"),
        })
        .collect()
}

fn signed(trail: &[Logged]) -> Vec<SignedStep> {
    trail
        .iter()
        .filter_map(|logged| match logged {
            Logged::Done(_, TrailOutcome::Signed(step)) => Some((**step).clone()),
            _ => None,
        })
        .collect()
}

fn page_seen(step: &SignedStep) -> &solos_agent_core::trail::PageSeen {
    match &step.body.outcome {
        StepOutcome::Page(page) => page,
        other => panic!("step {} is not a page: {other:?}", step.body.n),
    }
}

fn assert_verifies(step: &SignedStep, asked: &TrailAction, blobs: &BlobStore, key: &TrailKey) {
    let body = &step.body;
    let kept = match &body.outcome {
        StepOutcome::Page(page) => [
            (BlobKind::Snapshot, &page.snapshot),
            (BlobKind::View, &page.view),
        ]
        .into_iter()
        .map(|(kind, digest)| Blob {
            digest: digest.clone(),
            kind,
            bytes: blobs
                .get(&body.ticket, digest)
                .unwrap()
                .unwrap_or_else(|| panic!("step {}'s {kind:?} blob is missing", body.n)),
        })
        .collect(),
        _ => Vec::new(),
    };
    let call = BrowseCall {
        ticket: body.ticket.clone(),
        assignment: body.assignment.clone(),
        n: body.n,
        action: asked.clone(),
        typed: None,
        context: UseContext {
            requester: String::new(),
            environment: "local".into(),
            grants: BTreeMap::new(),
            takes_approvals: false,
        },
    };
    let browsed = Browsed {
        step: step.clone(),
        blobs: kept,
    };
    verify_step(&call, &browsed, key).unwrap_or_else(|why| panic!("step {}: {why}", body.n));
}

fn asked(trail: &[Logged], n: u32) -> TrailAction {
    trail
        .iter()
        .find_map(|logged| match logged {
            Logged::Asked(at, action) if *at == n => Some(action.clone()),
            _ => None,
        })
        .unwrap()
}

async fn gone(fake: &FakeCdp, what: &str) {
    until(&format!("{what} to hang up"), || {
        (fake.connections() == 0).then_some(())
    })
    .await;
}

async fn until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Some(found) = check() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct Silent;

impl Provider for Silent {
    fn dispatch(&self, _: Value) -> Dispatch {
        Box::pin(async { Err(anyhow::anyhow!("no model in this test")) })
    }
}

#[derive(Default)]
struct Starting {
    launched: Mutex<Vec<(TicketId, AssignmentId)>>,
    delivered: Mutex<Vec<InputBody>>,
}

impl Executor for Starting {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let started = Fact::Started {
                ticket: launch.ticket.clone(),
                assignment: launch.assignment.clone(),
            };
            facts.send(started).await?;
            self.launched
                .lock()
                .unwrap()
                .push((launch.ticket, launch.assignment));
            Ok(())
        })
    }
    fn deliver(&self, delivery: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        let bodies = delivery.inputs.into_iter().map(|d| d.input.body);
        self.delivered.lock().unwrap().extend(bodies);
        Box::pin(async { Ok(()) })
    }
    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone, Copy, Debug)]
enum Tamper {
    FlipViewByte,
    WrongPinnedKey,
}

struct Tampering {
    local: LocalBrowser,
    how: Tamper,
    other: TrailKey,
}

impl BrowserPlace for Tampering {
    fn name(&self) -> &str {
        self.local.name()
    }
    fn trusted(&self) -> &TrailKey {
        match self.how {
            Tamper::WrongPinnedKey => &self.other,
            Tamper::FlipViewByte => self.local.trusted(),
        }
    }
    fn act(&self, call: BrowseCall) -> BoxFuture<'_, Browsed> {
        Box::pin(async move {
            let mut browsed = self.local.act(call).await;
            if let Tamper::FlipViewByte = self.how
                && let Some(view) = browsed.blobs.iter_mut().find(|b| b.kind == BlobKind::View)
            {
                view.bytes[1] ^= 1;
            }
            browsed
        })
    }
    fn refuse(&self, call: &BrowseCall, why: String) -> Browsed {
        self.local.refuse(call, why)
    }
}

struct Setup {
    approval: Approval,
    users: &'static [&'static str],
    asks_on: &'static [&'static str],
    browser: bool,
    tamper: Option<Tamper>,
    idle: Duration,
    limits: TrailStoreConfig,
    lightpanda: Option<RealBrowser>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            approval: Approval::None,
            users: &["slack:alice"],
            asks_on: &["slack"],
            browser: true,
            tamper: None,
            idle: Duration::from_secs(1_800),
            limits: TrailStoreConfig::default(),
            lightpanda: None,
        }
    }
}

struct RealBrowser {
    binary: PathBuf,
    site: String,
}

struct World {
    root: PathBuf,
    runtime: Option<Runtime>,
    facts: FactSink,
    broker: AgentBroker,
    client: reqwest::Client,
    server: JoinHandle<()>,
    started: Arc<Starting>,
    fake: FakeCdp,
    browsers: Option<Arc<Browsers>>,
    key: TrailKey,
}

impl World {
    fn new(name: &str, setup: Setup) -> Self {
        let root = scratch(name);
        let state = root.join("state");
        let store = || SqliteStore::open(state.join(STORE_FILE)).unwrap();
        let channel: Arc<dyn Channel> =
            Arc::new(JsonlChannel::open(root.join("outbox.jsonl")).unwrap());
        let started = Arc::new(Starting::default());
        let adapters = Adapters {
            sources: vec![Box::new(JsonlSource::new(
                root.join("triggers.jsonl"),
                false,
            ))],
            channels: BTreeMap::from([("slack".to_owned(), channel)]),
            router: Arc::new(ScriptedRouter::new(BUDGET)),
            executor: started.clone(),
        };
        let policy = AllowlistPolicy {
            people: ["slack:alice".to_owned(), "github:alice".to_owned()].into(),
            ..AllowlistPolicy::default()
        };
        let options = RunOptions {
            tick: Duration::from_millis(10),
            ..RunOptions::default()
        };
        let runtime = Runtime::new(store(), policy, adapters, options);
        let site = setup
            .lightpanda
            .as_ref()
            .map_or(SITE, |real| real.site.as_str());
        let config = vault_with_login(&root, site, setup.users, setup.approval);
        let vault = Arc::new(Vault::open(&config, &SandboxView::new([])).unwrap());
        let view = SandboxView::new([]);
        let key_file = root.join("keys").join("trail.key");
        let key = TrailSigner::init(&key_file, &view).unwrap();
        let fake = FakeCdp::start(&root.join("cdp.sock"), login_site_pages()).unwrap();
        let (launcher, binary) = match &setup.lightpanda {
            Some(real) => (Launcher::Lightpanda, real.binary.clone()),
            None => (
                Launcher::Socket(fake.socket().to_owned()),
                PathBuf::from("/nowhere/lightpanda"),
            ),
        };
        let browser_config: BrowserConfig = serde_json::from_value(json!({
            "lightpanda": binary,
            "audit_key": key_file,
            "allow_private_networks": setup.lightpanda.is_some(),
        }))
        .unwrap();
        let browsers = setup.browser.then(|| {
            Arc::new(
                Browsers::new(
                    &browser_config,
                    "local",
                    &root.join("run"),
                    Some(vault.clone()),
                    TrailSigner::load(&key_file, &view).unwrap(),
                    launcher,
                )
                .unwrap(),
            )
        });
        let browser = browsers.clone().map(|browsers| {
            let local = LocalBrowser::new(browsers);
            let place: Arc<dyn BrowserPlace> = match setup.tamper {
                None => Arc::new(local),
                Some(how) => {
                    let other_file = root.join("keys").join("other.key");
                    let other = TrailSigner::init(&other_file, &view).unwrap();
                    Arc::new(Tampering { local, how, other })
                }
            };
            let blobs = BlobStore::open(&state.join("trail"), &setup.limits).unwrap();
            BrowserParts::new(place, blobs, setup.idle)
        });
        let parts = BrokerParts {
            dir: state.clone(),
            store: store(),
            provider: Arc::new(Silent),
            facts: runtime.facts(),
            decide: DecideTerms {
                default_budget_microusd: Some(BUDGET),
                max_budget_microusd: None,
                swarm: None,
                repos: BTreeMap::new(),
                ..DecideTerms::default()
            },
            workers: Some(WorkerParts {
                terms: ModelTerms {
                    upstream: Upstream::deepseek("deepseek-flash"),
                    prices: PriceTable {
                        input_microusd_per_million: 300_000,
                        output_microusd_per_million: 1_200_000,
                        cache_hit_microusd_per_million: None,
                    },
                    max_input_bytes: 65_536,
                    max_output_tokens: 1_024,
                    call_limit: None,
                    temperature: None,
                },
                code: None,
                vault: Some(VaultParts {
                    vault,
                    asks_on: setup.asks_on.iter().map(|s| (*s).to_owned()).collect(),
                }),
                browser,
                allow_private_networks: false,
            }),
        };
        let broker = AgentBroker::new(parts).unwrap();
        let (listener, socket) = broker::bind(&state).unwrap();
        let app = broker.router().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .unix_socket(socket)
            .build()
            .unwrap();
        Self {
            root,
            facts: runtime.facts(),
            runtime: Some(runtime),
            broker,
            client,
            server,
            started,
            fake,
            browsers,
            key,
        }
    }

    fn store_path(&self) -> PathBuf {
        self.root.join("state").join(STORE_FILE)
    }

    fn trail(&self) -> Vec<Logged> {
        trail(&self.store_path())
    }

    fn blobs(&self) -> BlobStore {
        BlobStore::open(
            &self.root.join("state").join("trail"),
            &TrailStoreConfig::default(),
        )
        .unwrap()
    }

    fn ticket(&self) -> Ticket {
        let store = SqliteStore::open_read_only(self.store_path()).unwrap();
        store.tickets().unwrap().remove(0)
    }

    fn ask(&self, source: &str) {
        let trigger = Trigger {
            id: TriggerId::new(format!("{source}:t1")),
            source: source.into(),
            at_ms: 1_000,
            actor: Actor::person("alice"),
            addressed: Addressed::Mention,
            text: "Read the site".into(),
            threads: vec![ThreadKey::new(format!("{source}:T1"))],
            reply_to: vec![ChannelRef {
                source: source.into(),
                address: BTreeMap::from([("thread_ts".to_owned(), "T1".to_owned())]),
            }],
            meta: json!({}),
        };
        let line = format!("{}\n", serde_json::to_string(&trigger).unwrap());
        std::fs::write(self.root.join("triggers.jsonl"), line).unwrap();
    }

    async fn admit_worker(&self) -> (String, Arc<WorkerSeat>) {
        let (ticket, assignment) = until("the ticket's launch", || {
            self.started.launched.lock().unwrap().first().cloned()
        })
        .await;
        let token = format!("worker-{ticket}");
        let seat = WorkerSeat::new(ticket, assignment, self.root.join("work"), BUDGET);
        let seat = self
            .broker
            .admit_worker(token.clone(), seat, "You are a worker.".into())
            .unwrap();
        (token, seat)
    }

    async fn post(&self, tool: &str, token: &str, body: Value) -> (u16, Vec<u8>) {
        call_tool(&self.client, tool, token, body).await
    }

    async fn call(&self, tool: &str, token: &str, body: Value) -> (u16, Value) {
        let (status, bytes) = self.post(tool, token, body).await;
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn listed(&self, token: &str) -> Vec<String> {
        let listed: Value = self
            .client
            .get("http://broker/tools")
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn call_tool(
    client: &reqwest::Client,
    tool: &str,
    token: &str,
    body: Value,
) -> (u16, Vec<u8>) {
    let response = client
        .post(format!("http://broker/tool/{tool}"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.bytes().await.unwrap().to_vec())
}

fn error(reply: &Value) -> &str {
    reply["error"].as_str().unwrap_or_default()
}

fn id(view: &Value, name: &str) -> u64 {
    view["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|element| element["name"] == name)
        .unwrap_or_else(|| panic!("no element {name} in {view}"))["id"]
        .as_u64()
        .unwrap()
}

async fn with_runtime<F: Future<Output = ()>>(runtime: Runtime, body: F) {
    let (stop, stopped) = oneshot::channel::<()>();
    let run = runtime.run(async {
        let _ = stopped.await;
    });
    let test = async {
        body.await;
        let _ = stop.send(());
    };
    let (summary, ()) = tokio::time::timeout(LIMIT, async { tokio::join!(run, test) })
        .await
        .expect("the test did not finish");
    summary.unwrap();
}

#[tokio::test]
async fn each_step_is_asked_then_answered_signed_and_the_model_gets_its_view() {
    let mut w = World::new("steps", Setup::default());
    w.ask("slack");
    let mut shown = BTreeMap::new();
    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let (token, seat) = w.admit_worker().await;
        let login = json!({"url": format!("{SITE}/login")});
        let (status, bytes) = w.post("browser_open", &token, login).await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        let view: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            (&view["step"], &view["title"]),
            (&json!(1), &json!("Sign in"))
        );
        assert_eq!(view["logins"], json!([LOGIN]));
        shown.insert(1, bytes);

        let typing =
            json!({"action": "type", "element": id(&view, "Comment"), "text": "a comment"});
        let (status, bytes) = w.post("browser_act", &token, typing).await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        shown.insert(2, bytes);
        let (status, bytes) = w.post("browser_read", &token, json!({})).await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        shown.insert(3, bytes);
        let (status, bytes) = w
            .post("browser_login", &token, json!({"login": LOGIN}))
            .await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        let landed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(landed["url"], format!("{SITE}/session"));
        shown.insert(4, bytes);

        browser::end(&w.broker, &seat, CloseReason::AssignmentEnded).await;
    })
    .await;

    let trail = w.trail();
    assert_eq!(
        lines(&trail),
        [
            "asked 1 open",
            "done 1 page",
            "asked 2 act",
            "done 2 page",
            "asked 3 read",
            "done 3 page",
            "asked 4 login",
            "done 4 page",
            "asked 5 close",
            "done 5 closed"
        ],
        "each step is asked before it is answered"
    );
    let blobs = w.blobs();
    let steps = signed(&trail);
    for step in &steps {
        assert_verifies(step, &asked(&trail, step.body.n), &blobs, &w.key);
        if let StepOutcome::Page(page) = &step.body.outcome {
            let view = blobs.get(&step.body.ticket, &page.view).unwrap().unwrap();
            assert_eq!(
                Some(&view),
                shown.get(&step.body.n),
                "step {}: the model got the View blob's exact bytes",
                step.body.n
            );
        }
    }
    let sessions: BTreeSet<_> = steps.iter().map(|step| step.body.session.clone()).collect();
    assert_eq!(sessions.len(), 1, "one browser for the assignment");
    let session_steps: Vec<u32> = steps.iter().map(|step| step.body.session_step).collect();
    assert_eq!(session_steps, [0, 1, 2, 3, 4]);
    assert!(
        steps[0].body.provenance.is_some(),
        "the first step names its build"
    );
    assert_eq!(page_seen(&steps[3]).filled, ["username", "password"]);
    let TrailAction::Act {
        text: Some(ActText::Typed { digest, chars }),
        ..
    } = asked(&trail, 2)
    else {
        panic!("step 2 typed nothing")
    };
    assert_eq!(chars, 9);
    assert_eq!(
        blobs.get(&steps[1].body.ticket, &digest).unwrap().unwrap(),
        b"a comment",
        "what was typed is a blob, and the chain holds its digest"
    );
    let StepOutcome::Closed {
        steps: count,
        reason,
    } = &steps[4].body.outcome
    else {
        panic!("the last step did not close")
    };
    assert_eq!((*count, *reason), (5, CloseReason::AssignmentEnded));
    let fills: Vec<_> = w.fake.fills().into_iter().map(|fill| fill.text).collect();
    assert!(
        fills.contains(&PASSWORD.to_owned()),
        "the login filled its password"
    );
    for bytes in shown.values() {
        assert!(!String::from_utf8_lossy(bytes).contains(PASSWORD));
    }
    gone(&w.fake, "the closed browser").await;
}

#[tokio::test]
async fn a_step_cut_off_by_a_hang_up_and_a_stop_is_still_recorded_then_closed() {
    let mut w = World::new("stop", Setup::default());
    w.ask("slack");
    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let (token, seat) = w.admit_worker().await;
        let held = w.fake.hold_on("Page.navigate");
        let (client, worker) = (w.client.clone(), token.clone());
        let opening = tokio::spawn(async move {
            let open = json!({"url": format!("{SITE}/login")});
            call_tool(&client, "browser_open", &worker, open).await
        });
        until("the open to be asked", || {
            (lines(&w.trail()) == ["asked 1 open"]).then_some(())
        })
        .await;
        opening.abort();
        let ready = json!({"status": "ready", "text": "Done."});
        let (status, reply) = w.call("worker_report", &token, ready).await;
        assert_eq!(status, 200, "{reply}");
        let (broker, stopped) = (w.broker.clone(), seat.clone());
        let ending = tokio::spawn(async move {
            browser::end(&broker, &stopped, CloseReason::AssignmentEnded).await;
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            lines(&w.trail()),
            ["asked 1 open"],
            "the close waits for the step in flight"
        );
        held.notify_one();
        ending.await.unwrap();

        let (status, reply) = w.call("browser_read", &token, json!({})).await;
        assert_eq!(status, 409, "{reply}");
        assert!(error(&reply).ends_with("is not running"), "{reply}");
    })
    .await;

    let trail = w.trail();
    assert_eq!(
        lines(&trail),
        [
            "asked 1 open",
            "done 1 page",
            "asked 2 close",
            "done 2 closed"
        ],
        "the step the hang-up cut off is answered, then the browser closes"
    );
    let steps = signed(&trail);
    assert_eq!(page_seen(&steps[0]).url, format!("{SITE}/login"));
    gone(&w.fake, "the closed browser").await;
}

#[tokio::test]
async fn an_answer_that_does_not_verify_is_recorded_lost_and_never_shown() {
    for (how, why) in [
        (Tamper::FlipViewByte, "no View blob matches"),
        (Tamper::WrongPinnedKey, "not by ed25519:"),
    ] {
        let setup = Setup {
            tamper: Some(how),
            ..Setup::default()
        };
        let mut w = World::new("lost", setup);
        w.ask("slack");
        let runtime = w.runtime.take().unwrap();
        with_runtime(runtime, async {
            let (token, seat) = w.admit_worker().await;
            let open = json!({"url": format!("{SITE}/login")});
            let (status, reply) = w.call("browser_open", &token, open).await;
            assert_eq!(status, 502, "{how:?}: {reply}");
            assert!(
                error(&reply).starts_with("the browser's answer did not verify"),
                "{how:?}: {reply}"
            );
            assert!(!reply.to_string().contains("Sign in to the site"));
            browser::end(&w.broker, &seat, CloseReason::AssignmentEnded).await;
        })
        .await;

        let trail = w.trail();
        assert_eq!(
            lines(&trail)[..2],
            ["asked 1 open", "done 1 lost"],
            "{how:?}"
        );
        let Logged::Done(1, TrailOutcome::Lost { why: lost }) = &trail[1] else {
            panic!("{how:?}: {trail:?}")
        };
        assert!(lost.contains(why), "{how:?}: {lost}");
        assert_eq!(
            lines(&trail)[2],
            "asked 2 close",
            "{how:?}: a browser that may be open is still closed"
        );

        let state = w.root.join("state");
        let events = SqliteStore::open_read_only(w.store_path())
            .unwrap()
            .events()
            .unwrap();
        let lost: Vec<Found> = events
            .into_iter()
            .filter_map(|event| match event.event {
                DomainEvent::TrailDone {
                    ticket,
                    assignment,
                    n,
                    outcome: TrailOutcome::Lost { why },
                } => Some(warning(Problem::Lost {
                    at: StepAt {
                        ticket,
                        assignment,
                        n,
                    },
                    why,
                })),
                _ => None,
            })
            .collect();
        let trust = Trust::new(
            None,
            vec![Trusted {
                environment: "local".to_owned(),
                key: w.key.clone(),
            }],
        )
        .unwrap();
        let report = verify(&state, &state.join("trail"), &trust, None, false).unwrap();
        assert!(!lost.is_empty(), "{how:?}");
        assert_found(&report, lost, true);
        assert!(
            !verify(&state, &state.join("trail"), &trust, None, true)
                .unwrap()
                .ok,
            "{how:?}: strict counts a lost answer"
        );
    }
}

#[tokio::test]
async fn concurrent_calls_on_one_seat_run_one_after_the_other() {
    let mut w = World::new("concurrent", Setup::default());
    w.ask("slack");
    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let (token, _seat) = w.admit_worker().await;
        let held = w.fake.hold_on("Page.navigate");
        let call = |tool: &'static str, body: Value| {
            let (client, token) = (w.client.clone(), token.clone());
            tokio::spawn(async move { call_tool(&client, tool, &token, body).await })
        };
        let open = call("browser_open", json!({"url": format!("{SITE}/login")}));
        until("the open to be asked", || {
            (!w.trail().is_empty()).then_some(())
        })
        .await;
        let read = call("browser_read", json!({}));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            lines(&w.trail()).len(),
            1,
            "the second call waits for the first"
        );
        held.notify_one();
        for call in [open, read] {
            let (status, bytes) = call.await.unwrap();
            assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        }
    })
    .await;

    assert_eq!(
        lines(&w.trail()),
        ["asked 1 open", "done 1 page", "asked 2 read", "done 2 page"],
        "the read is asked only once the open is answered"
    );
}

#[tokio::test]
async fn a_login_waits_for_an_allow_click_and_the_retry_fills_it() {
    let setup = Setup {
        approval: Approval::FirstUse,
        ..Setup::default()
    };
    let mut w = World::new("approve", setup);
    w.ask("slack");
    let secret = SecretId::try_from(format!("local/{LOGIN}")).unwrap();
    let label = format!("sign in to {SITE} as gui (login {LOGIN})");
    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let (token, seat) = w.admit_worker().await;
        let open = json!({"url": format!("{SITE}/login")});
        let (status, reply) = w.call("browser_open", &token, open).await;
        assert_eq!(status, 200, "{reply}");
        let login = json!({"login": LOGIN});
        let (status, reply) = w.call("browser_login", &token, login.clone()).await;
        assert_eq!(status, 409, "{reply}");
        assert_eq!(
            error(&reply),
            format!(
                "A person must allow this ticket to {label} first; they were asked in the \
                 thread. End your turn: their answer comes back as new input."
            )
        );
        let (status, reply) = w.call("browser_login", &token, login.clone()).await;
        assert_eq!(status, 409, "{reply}");
        assert!(
            error(&reply).starts_with(&format!(
                "A person has not yet allowed this ticket to {label}"
            )),
            "{reply}"
        );
        assert!(
            w.fake.fills().is_empty(),
            "nothing is filled before the click"
        );

        let click = Command {
            id: "click-1".into(),
            order: Order::Secret {
                secret: secret.clone(),
                allow: true,
            },
            ticket: w.ticket().id,
            source: "slack".into(),
            by: Actor::person("alice"),
            reply_to: w.ticket().channels[0].clone(),
        };
        w.facts.send(Fact::Command(click)).await.unwrap();
        until("the allow to reach the worker", || {
            let delivered = w.started.delivered.lock().unwrap();
            delivered
                .iter()
                .any(|body| matches!(body, InputBody::Secret { granted: true, .. }))
                .then_some(())
        })
        .await;
        // The worker ended its turn to wait, and the answer starts its next.
        seat.begin_turn();
        let (status, reply) = w.call("browser_login", &token, login).await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["url"], format!("{SITE}/session"));
    })
    .await;

    assert_eq!(
        lines(&w.trail()),
        [
            "asked 1 open",
            "done 1 page",
            "asked 2 login",
            "done 2 needs_approval",
            "asked 3 login",
            "done 3 needs_approval",
            "asked 4 login",
            "done 4 page"
        ]
    );
    assert!(matches!(
        w.ticket().secrets.get(&secret),
        Some(SecretGrant::Granted { .. })
    ));
    let store = SqliteStore::open_read_only(w.store_path()).unwrap();
    let asks: Vec<Ask> = store
        .effects()
        .unwrap()
        .into_iter()
        .filter_map(|queued| match queued.effect {
            Effect::Send(message) => message.ask,
            _ => None,
        })
        .collect();
    assert_eq!(
        asks,
        [Ask::Secret { secret }],
        "one ask for two refused tries"
    );
    let fills: Vec<_> = w.fake.fills().into_iter().map(|fill| fill.text).collect();
    assert!(
        fills.contains(&PASSWORD.to_owned()),
        "the retry filled the password"
    );

    let state = w.root.join("state");
    let key = Trusted {
        environment: "local".to_owned(),
        key: w.key.clone(),
    };
    let trust = Trust::new(None, vec![key]).unwrap();
    let report = verify(&state, &state.join("trail"), &trust, None, false).unwrap();
    assert_found(&report, Vec::new(), true);
    let replay = render(&report, Some(&w.ticket().id));
    println!("{replay}");
    let named = format!("local/{LOGIN}");
    for line in [
        format!("secrets: asked {named} "),
        format!(" · allowed {named} by alice "),
        format!("login {LOGIN}  "),
        format!("needs approval of {named}"),
    ] {
        assert!(replay.contains(&line), "no {line:?} in\n{replay}");
    }
}

#[tokio::test]
async fn a_login_on_a_ticket_with_no_channel_that_asks_is_refused_not_left_waiting() {
    let setup = Setup {
        approval: Approval::FirstUse,
        users: &[],
        ..Setup::default()
    };
    let mut w = World::new("no-buttons", setup);
    w.ask("github");
    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let (token, _seat) = w.admit_worker().await;
        let open = json!({"url": format!("{SITE}/login")});
        let (status, reply) = w.call("browser_open", &token, open).await;
        assert_eq!(status, 200, "{reply}");
        let (status, reply) = w
            .call("browser_login", &token, json!({"login": LOGIN}))
            .await;
        assert_eq!(
            (status, error(&reply)),
            (
                409,
                "site-login needs a person's approval, and none of this ticket's channels can \
                 ask for one: only a Slack or Discord thread with an approver shows the buttons"
            )
        );
        let (_, listed) = w.call("vault_list", &token, json!({})).await;
        assert_eq!(listed["logins"][0]["approval"], "no_approver");
        assert_eq!(listed["logins"][0]["use"], json!(["browser_login"]));
    })
    .await;

    assert!(w.ticket().secrets.is_empty(), "nobody was asked");
    assert!(w.fake.fills().is_empty());
    assert_eq!(
        lines(&w.trail())[2..],
        ["asked 2 login", "done 2 refused"],
        "the refusal is a signed step"
    );
}

#[tokio::test]
async fn without_a_browser_nothing_is_offered_and_a_direct_call_is_refused() {
    let setup = Setup {
        browser: false,
        ..Setup::default()
    };
    let mut w = World::new("none", setup);
    w.ask("slack");
    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let (token, _seat) = w.admit_worker().await;
        let listed = w.listed(&token).await;
        assert_eq!(
            listed,
            [
                "worker_report",
                "worker_handback",
                "fetch_url",
                "vault_list"
            ]
        );
        for tool in [
            "browser_open",
            "browser_act",
            "browser_login",
            "browser_read",
        ] {
            let (status, reply) = w.call(tool, &token, json!({"url": SITE})).await;
            assert_eq!(
                (status, error(&reply)),
                (403, "no browser where this ticket runs"),
                "{tool}"
            );
        }
        let (_, listed) = w.call("vault_list", &token, json!({})).await;
        assert_eq!(
            listed["logins"][0]["use"],
            json!([]),
            "no tool takes a login here"
        );
    })
    .await;
    assert!(w.trail().is_empty(), "nothing was asked");
}

#[tokio::test]
async fn an_idle_browser_is_closed_with_a_signed_step() {
    let setup = Setup {
        idle: Duration::from_millis(300),
        ..Setup::default()
    };
    let mut w = World::new("idle", setup);
    w.ask("slack");
    let reaper = browser::spawn_reaper(&w.broker).unwrap();
    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let (token, _seat) = w.admit_worker().await;
        let open = json!({"url": format!("{SITE}/login")});
        let (status, reply) = w.call("browser_open", &token, open.clone()).await;
        assert_eq!(status, 200, "{reply}");
        until("the idle close", || {
            (lines(&w.trail()).len() == 4).then_some(())
        })
        .await;
        gone(&w.fake, "the idle browser").await;
        let (status, reply) = w.call("browser_open", &token, open).await;
        assert_eq!(status, 200, "{reply}");
    })
    .await;
    reaper.abort();

    let trail = w.trail();
    assert_eq!(
        lines(&trail)[..5],
        [
            "asked 1 open",
            "done 1 page",
            "asked 2 close",
            "done 2 closed",
            "asked 3 open"
        ]
    );
    assert_eq!(
        asked(&trail, 2),
        TrailAction::Close {
            reason: CloseReason::Idle
        }
    );
    let steps = signed(&trail);
    assert_ne!(
        steps[0].body.session, steps[2].body.session,
        "the next call starts a new browser"
    );
    assert_eq!(steps[2].body.session_step, 0);
}

#[tokio::test]
async fn a_full_trail_store_refuses_the_next_step_and_records_it() {
    let setup = Setup {
        limits: TrailStoreConfig {
            path: None,
            max_bytes: 1 << 30,
            ticket_max_bytes: 1,
        },
        ..Setup::default()
    };
    let mut w = World::new("full", setup);
    w.ask("slack");
    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let (token, seat) = w.admit_worker().await;
        let open = json!({"url": format!("{SITE}/login")});
        let (status, reply) = w.call("browser_open", &token, open).await;
        assert_eq!(
            status, 200,
            "the step that crosses the cap keeps its blobs: {reply}"
        );
        let (status, reply) = w.call("browser_read", &token, json!({})).await;
        assert_eq!(status, 409, "{reply}");
        assert!(
            error(&reply).starts_with("this ticket's browser steps have stored their most"),
            "{reply}"
        );
        browser::end(&w.broker, &seat, CloseReason::AssignmentEnded).await;
    })
    .await;

    let trail = w.trail();
    assert_eq!(
        lines(&trail),
        [
            "asked 1 open",
            "done 1 page",
            "asked 2 read",
            "done 2 refused",
            "asked 3 close",
            "done 3 closed"
        ],
        "a full store refuses with a signed step and still closes the browser"
    );
    let blobs = w.blobs();
    for step in signed(&trail) {
        assert_verifies(&step, &asked(&trail, step.body.n), &blobs, &w.key);
    }
}

type Worker = Box<dyn Fn(&str) -> Vec<(&'static str, Value)> + Send + Sync>;

struct Model(Worker);

impl Provider for Model {
    fn dispatch(&self, body: Value) -> Dispatch {
        let last = body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .cloned()
            .unwrap_or_default();
        let prompt: String = match &last["content"] {
            Value::String(text) => text.clone(),
            parts => parts
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect(),
        };
        let calls: Vec<Value> = (self.0)(&prompt)
            .into_iter()
            .enumerate()
            .map(|(at, (name, arguments))| {
                json!({"id": format!("call-{at}"), "type": "function",
                       "function": {"name": name, "arguments": arguments.to_string()}})
            })
            .collect();
        let (message, finish) = if calls.is_empty() {
            (json!({"role": "assistant", "content": "Done."}), "stop")
        } else {
            (
                json!({"role": "assistant", "content": null, "tool_calls": calls}),
                "tool_calls",
            )
        };
        Box::pin(async move {
            Ok(
                json!({"id": "model-1", "created": 1, "model": "deepseek-flash",
                "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                "usage": {"prompt_tokens": 100, "prompt_cache_hit_tokens": 0, "completion_tokens": 50}}),
            )
        })
    }
}

struct Agent {
    root: PathBuf,
    fake: FakeCdp,
}

impl Agent {
    fn new(name: &str) -> Self {
        let root = scratch(name);
        std::fs::create_dir_all(root.join("pi")).unwrap();
        std::fs::create_dir_all(root.join("node").join("bin")).unwrap();
        std::fs::write(root.join("node").join("bin").join("node"), "").unwrap();
        vault_with_login(&root, SITE, &[], Approval::None);
        TrailSigner::init(&root.join("keys").join("trail.key"), &SandboxView::new([])).unwrap();
        let fake = FakeCdp::start(&root.join("cdp.sock"), login_site_pages()).unwrap();
        Self { root, fake }
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    fn store(&self) -> PathBuf {
        self.state().join(STORE_FILE)
    }

    fn ask(&self) {
        let line = json!({"id": "t-ask", "source": "slack", "at_ms": 1_793_613_600_000u64,
            "actor": {"id": "alice", "kind": "person"}, "addressed": "mention",
            "text": "Sign in to the site", "threads": ["T1"],
            "reply_to": [{"source": "slack", "address": {"thread_ts": "T1"}}]});
        std::fs::write(self.root.join("triggers.jsonl"), format!("{line}\n")).unwrap();
    }

    fn config(&self) -> AgentConfig {
        let yaml = "policy:\n  people: [slack:alice]\n\
            sources:\n  - {kind: jsonl, path: triggers.jsonl}\n\
            channels:\n  - {kind: jsonl, source: slack, path: outbox.jsonl}\n\
            router: {kind: scripted}\n\
            executor:\n  kind: pi\n  provider: deepseek\n  model: deepseek-flash\n  \
            prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}\n  \
            max_output_tokens: 2048\n\
            budgets: {session_microusd: 500000}\n\
            vault: {path: keys/vault.enc, key_file: keys/vault.key}\n\
            browser: {lightpanda: /nowhere/lightpanda, audit_key: keys/trail.key}\n";
        let path = self.root.join("agent.yaml");
        std::fs::write(&path, yaml).unwrap();
        AgentConfig::load(&path).unwrap()
    }

    fn host(&self, worker: Worker) -> PiHost<ScriptedHarness> {
        PiHost {
            harness: ScriptedHarness::new(BTreeMap::new()).every_turn(vec![Call::Replied]),
            provider: Arc::new(Model(worker)),
            pi_root: self.root.join("pi"),
            node: self.root.join("node").join("bin").join("node"),
            browser: Launcher::Socket(self.fake.socket().to_owned()),
        }
    }

    async fn serve_until_idle(&self, worker: Worker) -> Summary {
        let state = self.state();
        let serving = serve_with(self.config(), &state, true, Some(self.host(worker)));
        tokio::time::timeout(LIMIT, serving)
            .await
            .expect("serve did not go idle")
            .unwrap()
    }

    fn tickets(&self) -> Vec<Ticket> {
        SqliteStore::open_read_only(self.store())
            .unwrap()
            .tickets()
            .unwrap()
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn first_prompt(prompt: &str) -> bool {
    prompt.starts_with("Ticket ")
}

fn sign_in_script() -> Worker {
    Box::new(|prompt| {
        if !first_prompt(prompt) {
            return Vec::new();
        }
        vec![
            ("vault_list", json!({})),
            ("browser_open", json!({"url": format!("{SITE}/login")})),
            (
                "browser_act",
                json!({"action": "type", "element": 4, "text": "hello"}),
            ),
            ("browser_login", json!({"login": LOGIN})),
            ("browser_act", json!({"action": "back"})),
            (
                "worker_report",
                json!({"status": "ready", "text": "Signed in."}),
            ),
        ]
    })
}

#[tokio::test]
async fn a_worker_signs_in_through_its_tools_and_its_browser_closes_when_the_ticket_ends() {
    let agent = Agent::new("serve-login");
    agent.ask();
    let summary = agent.serve_until_idle(sign_in_script()).await;

    assert_eq!(summary.effects_pending, 0);
    let ticket = &agent.tickets()[0];
    assert_eq!(
        ticket.ended.as_ref().map(|ended| ended.outcome),
        Some(TicketOutcome::Completed)
    );
    let trail = trail(&agent.store());
    assert_eq!(
        lines(&trail),
        [
            "asked 1 open",
            "done 1 page",
            "asked 2 act",
            "done 2 page",
            "asked 3 login",
            "done 3 page",
            "asked 4 act",
            "done 4 page",
            "asked 5 close",
            "done 5 closed"
        ]
    );
    assert_eq!(
        asked(&trail, 5),
        TrailAction::Close {
            reason: CloseReason::AssignmentEnded
        },
        "the executor closes the browser as the assignment's session ends"
    );
    let steps = signed(&trail);
    assert_eq!(page_seen(&steps[2]).filled, ["username", "password"]);
    let key = TrailSigner::load(
        &agent.root.join("keys").join("trail.key"),
        &SandboxView::new([]),
    )
    .unwrap()
    .public();
    let blobs =
        BlobStore::open(&agent.state().join("trail"), &TrailStoreConfig::default()).unwrap();
    for step in &steps {
        assert_verifies(step, &asked(&trail, step.body.n), &blobs, &key);
    }
    gone(&agent.fake, "every browser").await;
    SqliteStore::open_read_only(agent.store())
        .unwrap()
        .verify_chain()
        .unwrap();
}

#[tokio::test]
async fn stopping_the_agent_closes_every_browser_with_a_signed_step() {
    let agent = Agent::new("serve-stop");
    agent.ask();
    agent
        .serve_until_idle(Box::new(|prompt| {
            if !first_prompt(prompt) {
                return Vec::new();
            }
            vec![
                ("browser_open", json!({"url": format!("{SITE}/login")})),
                (
                    "worker_report",
                    json!({"status": "waiting", "text": "Waiting."}),
                ),
            ]
        }))
        .await;

    let trail = trail(&agent.store());
    assert_eq!(
        lines(&trail),
        [
            "asked 1 open",
            "done 1 page",
            "asked 2 close",
            "done 2 closed"
        ],
        "the ticket is still open when the agent stops"
    );
    assert_eq!(
        asked(&trail, 2),
        TrailAction::Close {
            reason: CloseReason::Shutdown
        }
    );
    gone(&agent.fake, "every browser").await;
}

#[tokio::test]
async fn a_crash_leaves_no_browser_running_and_the_restart_opens_a_new_one() {
    let agent = Agent::new("serve-crash");
    agent.ask();
    let _never = agent.fake.hold_on("Page.navigate");
    let crashed = {
        let (config, host, state, store) = (
            agent.config(),
            agent.host(Box::new(|prompt| {
                if first_prompt(prompt) {
                    vec![
                        ("browser_read", json!({})),
                        ("browser_open", json!({"url": format!("{SITE}/login")})),
                    ]
                } else {
                    Vec::new()
                }
            })),
            agent.state(),
            agent.store(),
        );
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let browsed = async {
                    until("the second step to hang", || {
                        (lines(&trail(&store)).len() == 3).then_some(())
                    })
                    .await;
                };
                tokio::select! {
                    ended = serve_with(config, &state, false, Some(host)) => {
                        panic!("serve ended: {ended:?}")
                    }
                    () = browsed => {}
                }
            });
            runtime.shutdown_timeout(Duration::from_secs(5));
        })
    };
    tokio::task::spawn_blocking(move || crashed.join().unwrap())
        .await
        .unwrap();
    gone(&agent.fake, "the crashed browser").await;
    let stale = agent.state().join(BROWSER_DIR).join("00112233aabbccdd");
    std::fs::create_dir_all(&stale).unwrap();

    agent
        .serve_until_idle(Box::new(|prompt| {
            if !prompt.contains("This ticket was worked on before") {
                return Vec::new();
            }
            vec![
                ("browser_open", json!({"url": format!("{SITE}/login")})),
                (
                    "worker_report",
                    json!({"status": "ready", "text": "Opened again."}),
                ),
            ]
        }))
        .await;

    assert!(
        !stale.exists(),
        "the restart removes what a killed browser left"
    );
    let trail = trail(&agent.store());
    assert_eq!(
        lines(&trail),
        [
            "asked 1 read",
            "done 1 page",
            "asked 2 open",
            "asked 1 open",
            "done 1 page",
            "asked 2 close",
            "done 2 closed"
        ],
        "the step the crash cut off stays asked, and only the new browser is closed"
    );
    let steps = signed(&trail);
    assert_ne!(steps[0].body.assignment, steps[1].body.assignment);
    assert_ne!(steps[0].body.session, steps[1].body.session);
    assert_eq!(steps[1].body.session_step, 0);
    gone(&agent.fake, "the restarted browser").await;
}

#[tokio::test]
async fn serve_refuses_a_trail_key_blob_or_browser_socket_a_sandbox_can_read() {
    let agent = Agent::new("serve-view");
    let tickets = agent.state().join("tickets");
    let pi = agent.root.join("pi");
    let key = agent.root.join("keys").join("trail.key");
    for (place, edit) in [
        (
            "browser.audit_key",
            Box::new(|config: &mut AgentConfig| {
                config.browser.as_mut().unwrap().audit_key = tickets.join("trail.key");
            }) as Box<dyn Fn(&mut AgentConfig)>,
        ),
        (
            "trail.path",
            Box::new(|config: &mut AgentConfig| config.trail.path = Some(pi.join("trail"))),
        ),
        (
            "browser.runtime_dir",
            Box::new(|config: &mut AgentConfig| {
                config.browser.as_mut().unwrap().runtime_dir = Some(tickets.join("browser"));
            }),
        ),
    ] {
        let mut config = agent.config();
        edit(&mut config);
        let state = agent.state();
        let served = serve_with(
            config,
            &state,
            true,
            Some(agent.host(Box::new(|_| Vec::new()))),
        );
        let refused = tokio::time::timeout(LIMIT, served)
            .await
            .expect("serve did not end")
            .expect_err(place);
        let why = format!("{refused:#}");
        assert!(
            why.starts_with(place) && why.contains("which a sandbox can read"),
            "{place}: {why}"
        );
    }
    assert!(key.exists(), "the real key stays where it was");
}

const REAL_LOGIN: &str = r#"<!doctype html><html><head><title>Probe login</title></head><body>
<form action="/session" method="post">
<label for="u">Username</label><input id="u" name="username" type="text">
<label for="p">Password</label><input id="p" name="password" type="password">
<button type="submit">Sign in</button>
</form></body></html>"#;

#[tokio::test]
#[ignore = "needs a Lightpanda binary in SOLOS_LIGHTPANDA, bwrap and prlimit"]
async fn a_real_lightpanda_signs_in_through_the_broker_tools() {
    let binary = PathBuf::from(std::env::var("SOLOS_LIGHTPANDA").expect("SOLOS_LIGHTPANDA"));
    let posted: Arc<Mutex<Vec<String>>> = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let site = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let kept = posted.clone();
    let app = axum::Router::new()
        .route("/login", get(|| async { Html(REAL_LOGIN) }))
        .route(
            "/session",
            post(move |body: String| {
                kept.lock().unwrap().push(body);
                async { Html("<html><head><title>Signed in</title></head><body>ok</body></html>") }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let setup = Setup {
        lightpanda: Some(RealBrowser {
            binary,
            site: site.clone(),
        }),
        ..Setup::default()
    };
    let mut w = World::new("real", setup);
    w.ask("slack");
    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let (token, seat) = w.admit_worker().await;
        let (status, reply) = w
            .call(
                "browser_open",
                &token,
                json!({"url": format!("{site}/login")}),
            )
            .await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["title"], "Probe login");
        let (status, reply) = w
            .call("browser_login", &token, json!({"login": LOGIN}))
            .await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["url"], format!("{site}/session"));
        assert!(!reply.to_string().contains(PASSWORD));
        browser::end(&w.broker, &seat, CloseReason::AssignmentEnded).await;
    })
    .await;
    if let Some(browsers) = &w.browsers {
        browsers.shutdown().await;
    }
    server.abort();

    let trail = w.trail();
    assert_eq!(
        lines(&trail),
        [
            "asked 1 open",
            "done 1 page",
            "asked 2 login",
            "done 2 page",
            "asked 3 close",
            "done 3 closed"
        ]
    );
    let blobs = w.blobs();
    for step in signed(&trail) {
        assert_verifies(&step, &asked(&trail, step.body.n), &blobs, &w.key);
    }
    let posted = posted.lock().unwrap();
    assert!(
        posted.iter().any(|body| body.contains("username=gui")),
        "the site got the sign-in: {posted:?}"
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_none_or(|extension| extension != "tmp"))
        .collect()
}

fn fatal(problem: Problem) -> Found {
    Found {
        fatal: true,
        problem,
    }
}

fn warning(problem: Problem) -> Found {
    Found {
        fatal: false,
        problem,
    }
}

fn position(events: &[Event], wanted: impl Fn(&DomainEvent) -> bool) -> usize {
    events
        .iter()
        .position(|event| wanted(&event.event))
        .expect("the chain has no such event")
}

fn is_asked(n: u32) -> impl Fn(&DomainEvent) -> bool {
    move |event| matches!(event, DomainEvent::TrailAsked { n: at, .. } if *at == n)
}

fn is_done(n: u32) -> impl Fn(&DomainEvent) -> bool {
    move |event| matches!(event, DomainEvent::TrailDone { n: at, .. } if *at == n)
}

fn is_ended(event: &DomainEvent) -> bool {
    matches!(event, DomainEvent::AssignmentEnded { .. })
}

fn without_step(events: &mut Vec<Event>, n: u32) {
    events.retain(|event| !is_asked(n)(&event.event) && !is_done(n)(&event.event));
}

fn signed_mut(events: &mut [Event], n: u32) -> &mut SignedStep {
    let at = position(events, is_done(n));
    match &mut events[at].event {
        DomainEvent::TrailDone {
            outcome: TrailOutcome::Signed(step),
            ..
        } => step,
        other => panic!("step {n} was not answered with a signed step: {other:?}"),
    }
}

fn answer_mut(events: &mut [Event], n: u32) -> &mut TrailOutcome {
    let at = position(events, is_done(n));
    match &mut events[at].event {
        DomainEvent::TrailDone { outcome, .. } => outcome,
        other => panic!("{other:?} is no answer"),
    }
}

struct Written {
    agent: Agent,
    ticket: TicketId,
    assignment: AssignmentId,
    events: Vec<Event>,
    tickets: BTreeMap<TicketId, Ticket>,
    steps: Vec<SignedStep>,
    trust: Trust,
}

async fn written(name: &str) -> Written {
    let agent = Agent::new(name);
    agent.ask();
    agent.serve_until_idle(sign_in_script()).await;
    let store = SqliteStore::open_read_only(agent.store()).unwrap();
    let events = store.events().unwrap();
    let tickets = store
        .tickets()
        .unwrap()
        .into_iter()
        .map(|ticket| (ticket.id.clone(), ticket))
        .collect();
    let steps = signed(&trail(&agent.store()));
    assert_eq!(steps.len(), 5, "the worker's script takes five steps");
    let (ticket, assignment) = (
        steps[0].body.ticket.clone(),
        steps[0].body.assignment.clone(),
    );
    let trust = Trust::new(Some(&agent.config()), Vec::new()).unwrap();
    Written {
        agent,
        ticket,
        assignment,
        events,
        tickets,
        steps,
        trust,
    }
}

struct Tampered {
    state: PathBuf,
}

impl Tampered {
    fn blobs(&self) -> PathBuf {
        self.state.join("trail")
    }

    fn blob(&self, ticket: &TicketId, digest: &Digest) -> PathBuf {
        self.blobs().join(ticket.as_str()).join(digest.digits())
    }
}

impl Written {
    fn forge(&self, name: &str, edit: impl FnOnce(&mut Vec<Event>)) -> Tampered {
        let state = self.agent.root.join("copies").join(name);
        std::fs::create_dir_all(&state).unwrap();
        let mut events = self.events.clone();
        edit(&mut events);
        let mut store = SqliteStore::open(state.join(STORE_FILE)).unwrap();
        store
            .commit(ChangeSet {
                tickets: self.tickets.clone(),
                events,
                ..ChangeSet::default()
            })
            .unwrap();
        drop(store);
        let from = self.agent.state().join("trail");
        for dir in files(&from) {
            let to = state.join("trail").join(dir.file_name().unwrap());
            std::fs::create_dir_all(&to).unwrap();
            for blob in files(&dir) {
                std::fs::copy(&blob, to.join(blob.file_name().unwrap())).unwrap();
            }
        }
        Tampered { state }
    }

    fn at(&self, n: u32) -> StepAt {
        StepAt {
            ticket: self.ticket.clone(),
            assignment: self.assignment.clone(),
            n,
        }
    }

    fn check(&self, forged: &Tampered, strict: bool) -> TrailReport {
        verify(&forged.state, &forged.blobs(), &self.trust, None, strict).unwrap()
    }

    fn step(&self, n: u32) -> &SignedStep {
        &self.steps[n as usize - 1]
    }

    fn session(&self) -> String {
        self.step(1).body.session.clone()
    }

    fn view(&self, n: u32) -> Digest {
        page_seen(self.step(n)).view.clone()
    }

    fn typed(&self) -> Digest {
        match &self.step(2).body.action {
            TrailAction::Act {
                text: Some(ActText::Typed { digest, .. }),
                ..
            } => digest.clone(),
            other => panic!("step 2 typed nothing: {other:?}"),
        }
    }
}

#[track_caller]
fn assert_found(report: &TrailReport, expected: Vec<Found>, ok: bool) {
    assert_eq!(report.problems, expected);
    assert_eq!(report.ok, ok, "ok");
}

#[tokio::test]
async fn a_trail_the_browser_tools_wrote_checks_clean() {
    let w = written("trail-clean").await;
    let forged = w.forge("clean", |_| {});
    let writing = forged.blobs().join(w.ticket.as_str()).join(".half.tmp");
    std::fs::write(&writing, b"half a blob").unwrap();

    let report = w.check(&forged, false);

    assert_found(&report, Vec::new(), true);
    let chain = report.chain.as_ref().unwrap();
    assert_eq!(chain.events, w.events.len() as u64);
    let ticket = &report.tickets[0];
    assert_eq!(ticket.ticket, w.ticket);
    assert_eq!(
        ticket.title.as_deref(),
        Some(w.tickets[&w.ticket].title.as_str())
    );
    let kept = files(&forged.blobs().join(w.ticket.as_str())).len();
    assert_eq!(
        (
            ticket.tally.steps,
            ticket.tally.signatures_ok,
            ticket.tally.blobs_ok as usize
        ),
        (5, 5, kept),
        "every step, signature and blob the store holds is counted once"
    );
    assert!(
        writing.exists(),
        "a blob the agent is still writing is left alone"
    );
    let DomainEvent::AssignmentEnded { reason, .. } =
        &w.events[position(&w.events, is_ended)].event
    else {
        unreachable!()
    };
    assert_eq!(
        ticket.assignments[0].ended.as_deref(),
        Some(reason.as_str())
    );
}

#[tokio::test]
async fn a_flipped_byte_in_a_view_blob_is_corrupt_at_the_step_that_names_it() {
    let w = written("trail-corrupt").await;
    let forged = w.forge("corrupt", |_| {});
    let path = forged.blob(&w.ticket, &w.view(1));
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[1] ^= 1;
    std::fs::write(&path, bytes).unwrap();

    let report = w.check(&forged, false);

    let corrupt = Problem::BlobCorrupt {
        at: w.at(1),
        digest: w.view(1),
    };
    assert_found(&report, vec![fatal(corrupt)], false);
    let kept = files(&forged.blobs().join(w.ticket.as_str())).len();
    assert_eq!(
        report.tickets[0].tally.blobs_ok as usize,
        kept - 1,
        "the corrupt blob is the one not counted"
    );
}

#[tokio::test]
async fn a_blob_two_steps_name_is_corrupt_once_at_the_first_of_them() {
    let w = written("trail-shared").await;
    let (first, again) = (page_seen(w.step(1)), page_seen(w.step(4)));
    assert_eq!(
        first.snapshot, again.snapshot,
        "going back captures the first page again, which is the one blob"
    );
    let forged = w.forge("shared", |_| {});
    let path = forged.blob(&w.ticket, &first.snapshot);
    std::fs::write(&path, b"another page").unwrap();

    let report = w.check(&forged, false);

    let corrupt = Problem::BlobCorrupt {
        at: w.at(1),
        digest: first.snapshot.clone(),
    };
    assert_found(&report, vec![fatal(corrupt)], false);
}

#[tokio::test]
async fn a_deleted_typed_text_blob_is_missing_at_the_step_that_typed_it() {
    let w = written("trail-missing").await;
    let forged = w.forge("missing", |_| {});
    std::fs::remove_file(forged.blob(&w.ticket, &w.typed())).unwrap();

    let report = w.check(&forged, false);

    let missing = Problem::BlobMissing {
        at: w.at(2),
        digest: w.typed(),
    };
    assert_found(&report, vec![fatal(missing)], false);
}

#[tokio::test]
async fn a_body_edited_in_a_rehashed_chain_has_a_bad_signature() {
    let w = written("trail-edited").await;
    let forged = w.forge("edited", |events| {
        let StepOutcome::Page(page) = &mut signed_mut(events, 4).body.outcome else {
            panic!("step 4 saw no page")
        };
        page.title.push_str(" (edited)");
    });

    let report = w.check(&forged, false);

    assert!(report.chain.is_some(), "the edited chain was hashed again");
    let bad = Problem::BadSignature { at: w.at(4) };
    assert_found(&report, vec![fatal(bad)], false);
}

#[tokio::test]
async fn a_step_signed_by_another_key_is_foreign() {
    let w = written("trail-foreign").await;
    let other = CryptoSigner::from_seed([7; 32]);
    let forged = w.forge("foreign", |events| {
        let step = signed_mut(events, 2);
        let resigned = other.sign(TRAIL_DOMAIN, &step.body).unwrap();
        step.signer = TrailKey::try_from(format!("ed25519:{}", hex(&resigned.signer))).unwrap();
        step.signature = Signature::try_from(hex(&resigned.signature)).unwrap();
    });

    let report = w.check(&forged, false);

    let foreign = Problem::Foreign {
        at: w.at(2),
        signer: TrailKey::try_from(format!("ed25519:{}", hex(&other.public_key()))).unwrap(),
    };
    assert_found(&report, vec![fatal(foreign)], false);
}

#[tokio::test]
async fn a_dropped_answer_with_a_later_step_is_overtaken_and_fatal() {
    let w = written("trail-dropped").await;
    let forged = w.forge("dropped", |events| {
        let at = position(events, is_done(3));
        events.remove(at);
    });

    let report = w.check(&forged, false);

    let overtaken = Problem::Overtaken { at: w.at(3), by: 4 };
    assert_found(&report, vec![fatal(overtaken)], false);
}

#[tokio::test]
async fn a_chain_cut_off_before_the_last_answer_warns_and_only_strict_fails_it() {
    let w = written("trail-cut").await;
    let forged = w.forge("cut", |events| {
        let at = position(events, is_done(5));
        events.truncate(at);
    });
    let unanswered = Problem::Unanswered { at: w.at(5) };

    assert_found(
        &w.check(&forged, false),
        vec![warning(unanswered.clone())],
        true,
    );
    assert_found(&w.check(&forged, true), vec![fatal(unanswered)], false);
}

#[tokio::test]
async fn two_steps_answered_with_each_others_signed_bodies_mismatch_and_nothing_else() {
    let w = written("trail-swapped").await;
    let forged = w.forge("swapped", |events| {
        let (two, three) = (answer_mut(events, 2).clone(), answer_mut(events, 3).clone());
        *answer_mut(events, 2) = three;
        *answer_mut(events, 3) = two;
    });

    let report = w.check(&forged, false);

    let refused = |n: u32, body: u32| {
        let asked = Pending {
            n,
            action: w.step(n).body.action.clone(),
            asked_ms: 0,
        };
        let why = answers(&asked, &w.ticket, &w.assignment, w.step(body)).unwrap_err();
        fatal(Problem::Mismatch { at: w.at(n), why })
    };
    assert_found(&report, vec![refused(2, 3), refused(3, 2)], false);
}

#[tokio::test]
async fn a_removed_step_is_a_gap_in_the_numbers_and_in_the_signers_count() {
    let w = written("trail-gap").await;
    let forged = w.forge("gap", |events| without_step(events, 3));

    let report = w.check(&forged, false);

    let problems = vec![
        fatal(Problem::StepGap {
            at: w.at(4),
            expected: 3,
        }),
        fatal(Problem::SessionGap {
            at: w.at(4),
            session: w.session(),
            expected: 2,
            found: 3,
        }),
    ];
    assert_found(&report, problems, false);
}

#[tokio::test]
async fn a_session_the_ended_assignment_never_closed_is_unclosed_at_its_last_step() {
    let w = written("trail-unclosed").await;
    assert!(
        position(&w.events, is_ended) < position(&w.events, is_asked(5)),
        "the chain ends the Assignment before the executor closes its browser"
    );
    let forged = w.forge("unclosed", |events| without_step(events, 5));

    let report = w.check(&forged, false);

    let unclosed = Problem::Unclosed {
        at: w.at(4),
        session: w.session(),
    };
    assert_found(&report, vec![fatal(unclosed)], false);
}

#[tokio::test]
async fn a_browser_left_open_while_its_assignment_runs_is_live_not_unclosed() {
    let w = written("trail-live-prefix").await;
    assert!(
        position(&w.events, is_done(4)) < position(&w.events, is_ended),
        "the Assignment ends after the worker's last page step"
    );
    let forged = w.forge("running", |events| {
        let at = position(events, is_ended);
        events.truncate(at);
    });

    let report = w.check(&forged, true);

    assert_found(&report, Vec::new(), true);
    let assignment = &report.tickets[0].assignments[0];
    assert_eq!((assignment.steps.len(), &assignment.ended), (4, &None));
}

#[tokio::test]
async fn a_session_a_restarted_host_ended_without_a_close_is_not_unclosed() {
    let w = written("trail-restarted").await;
    let forged = w.forge("restarted", |events| {
        without_step(events, 5);
        let at = position(events, is_ended);
        if let DomainEvent::AssignmentEnded { reason, .. } = &mut events[at].event {
            *reason = HOST_RESTARTED.to_owned();
        }
    });

    let report = w.check(&forged, false);

    assert_found(&report, Vec::new(), true);
    assert_eq!(report.tickets[0].tally.steps, 4);
}

#[tokio::test]
async fn a_second_answer_to_one_step_is_unpaired() {
    let w = written("trail-unpaired").await;
    let forged = w.forge("unpaired", |events| {
        let at = position(events, is_done(2));
        events.insert(at + 1, events[at].clone());
    });

    let report = w.check(&forged, false);

    assert_found(
        &report,
        vec![fatal(Problem::Unpaired { at: w.at(2) })],
        false,
    );
}

#[tokio::test]
async fn an_event_edited_without_hashing_again_breaks_the_chain_and_nothing_after_it_is_read() {
    let w = written("trail-broken").await;
    let forged = w.forge("broken", |_| {});
    let seq = position(&w.events, is_done(4)) as u64 + 1;
    let title = page_seen(w.step(4)).title.clone();
    let conn = rusqlite::Connection::open(forged.state.join(STORE_FILE)).unwrap();
    conn.execute_batch("DROP TRIGGER events_no_update").unwrap();
    let edited = conn
        .execute(
            "UPDATE events SET body = replace(body, ?1, ?2) WHERE seq = ?3",
            rusqlite::params![
                format!("\"title\":\"{title}\""),
                "\"title\":\"edited\"",
                seq
            ],
        )
        .unwrap();
    assert_eq!(edited, 1, "the edit reached one event");
    drop(conn);

    let report = w.check(&forged, false);

    let broken = Problem::Chain {
        seq,
        reason: "does not match its hash".to_owned(),
    };
    assert_found(&report, vec![fatal(broken)], false);
    assert!(report.chain.is_none() && report.tickets.is_empty());
}

#[tokio::test]
async fn a_step_whose_environment_has_no_trusted_key_is_untrusted_once() {
    let w = written("trail-untrusted").await;
    let forged = w.forge("untrusted", |_| {});

    let nobody = Trust::new(None, Vec::new()).unwrap();
    let report = verify(&forged.state, &forged.blobs(), &nobody, None, false).unwrap();

    let untrusted = Problem::Untrusted {
        at: w.at(1),
        environment: "local".to_owned(),
    };
    assert_found(&report, vec![fatal(untrusted)], false);
    assert_eq!(report.tickets[0].tally.signatures_ok, 0);
}

fn trail_cli(args: &[&str]) -> std::process::Output {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_solos-swarm"))
        .args(["agent", "trail"])
        .args(args)
        .output()
        .unwrap();
    println!("$ solos-swarm agent trail {}", args.join(" "));
    print!("{}", String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        println!(
            "[stderr] {}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    println!("[exit {:?}]\n", output.status.code());
    output
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[tokio::test]
async fn agent_trail_prints_a_clean_trail_step_by_step_and_exits_0() {
    let w = written("trail-cli-clean").await;
    let config = w.agent.root.join("agent.yaml");
    let (state, ticket) = (w.agent.state(), w.ticket.as_str());

    let output = trail_cli(&[
        "--state-dir",
        path(&state),
        "--config",
        path(&config),
        "--ticket",
        ticket,
    ]);

    assert!(output.status.success());
    let out = text(&output.stdout);
    assert!(
        out.starts_with(&format!("chain ok: {} events, head ", w.events.len())),
        "{out}"
    );
    assert!(out.contains(&w.tickets[&w.ticket].title), "{out}");
    for n in 1..=5 {
        let step = format!("n{n} ");
        assert!(
            out.lines().any(|line| line.trim_start().starts_with(&step)),
            "no line for step {n}: {out}"
        );
    }
    let kept = files(&state.join("trail").join(ticket)).len();
    let tally = format!("5 steps, 5 signatures ok, {kept} blobs ok, 0 problems");
    assert!(out.contains(&tally), "{out}");
}

#[tokio::test]
async fn agent_trail_prints_json_and_the_exact_bytes_of_a_steps_blob() {
    let w = written("trail-cli-json").await;
    let config = w.agent.root.join("agent.yaml");
    let (state, ticket) = (w.agent.state(), w.ticket.as_str());
    let base = [
        "--state-dir",
        path(&state),
        "--config",
        path(&config),
        "--ticket",
        ticket,
    ];

    let json = trail_cli(&[&base[..], &["--json"]].concat());

    assert!(json.status.success());
    let report: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(report["chain"]["events"], w.events.len());
    assert_eq!(report["tickets"][0]["tally"]["steps"], 5);
    assert_eq!(report["problems"], json!([]));

    let view = trail_cli(&[&base[..], &["--step", "1", "--show", "view"]].concat());
    let blob = state.join("trail").join(ticket).join(w.view(1).digits());
    assert!(view.status.success());
    assert_eq!(
        view.stdout,
        std::fs::read(blob).unwrap(),
        "the exact bytes the model got"
    );

    let snapshot = trail_cli(&[&base[..], &["--step", "4", "--show", "snapshot"]].concat());
    let blob = state
        .join("trail")
        .join(ticket)
        .join(page_seen(w.step(4)).snapshot.digits());
    assert_eq!(snapshot.stdout, std::fs::read(blob).unwrap());

    let none = trail_cli(&[&base[..], &["--step", "9"]].concat());
    assert_eq!(none.status.code(), Some(1));
    assert!(text(&none.stderr).contains("has no step 9"));
    let closed = trail_cli(&[&base[..], &["--step", "5"]].concat());
    assert_eq!(closed.status.code(), Some(1));
    assert!(text(&closed.stderr).contains("captured no page"));
}

#[tokio::test]
async fn agent_trail_exits_1_on_a_problem_and_only_strict_fails_a_warning() {
    let w = written("trail-cli-exit").await;
    let config = w.agent.root.join("agent.yaml");
    let corrupt = w.forge("corrupt", |_| {});
    let blob = corrupt.blob(&w.ticket, &w.view(1));
    let mut bytes = std::fs::read(&blob).unwrap();
    bytes[1] ^= 1;
    std::fs::write(&blob, bytes).unwrap();
    let cut = w.forge("cut", |events| {
        let at = position(events, is_done(5));
        events.truncate(at);
    });

    let output = trail_cli(&[
        "--state-dir",
        path(&corrupt.state),
        "--config",
        path(&config),
        "--ticket",
        w.ticket.as_str(),
    ]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output.stdout)
            .lines()
            .any(|line| line.starts_with("  problem: step 1 of ")),
        "{}",
        text(&output.stdout)
    );
    assert!(text(&output.stderr).contains("the trail has problems (1)"));

    let args = ["--state-dir", path(&cut.state), "--config", path(&config)];
    let lenient = trail_cli(&args);
    assert!(lenient.status.success());
    assert!(
        text(&lenient.stdout)
            .lines()
            .any(|line| line.starts_with("  warning: step 5 of ")),
        "{}",
        text(&lenient.stdout)
    );
    let strict = trail_cli(&[&args[..], &["--strict"]].concat());
    assert_eq!(strict.status.code(), Some(1));
    assert!(text(&strict.stdout).contains("  problem: step 5 of "));
}

#[tokio::test]
async fn a_verifier_that_holds_only_public_keys_checks_a_trail_without_the_seed() {
    let w = written("trail-cli-public").await;
    let key_file = w.agent.root.join("keys").join("trail.key");
    let key = TrailSigner::load(&key_file, &SandboxView::new([]))
        .unwrap()
        .public();
    let config = w.agent.root.join("agent.yaml");
    let state = w.agent.state();
    std::fs::remove_file(&key_file).unwrap();
    let trusting = format!("local={key}");

    let alone = trail_cli(&["--state-dir", path(&state), "--trust", &trusting]);
    assert!(alone.status.success());
    assert!(text(&alone.stdout).contains("5 steps, 5 signatures ok"));

    let with_config = trail_cli(&[
        "--state-dir",
        path(&state),
        "--config",
        path(&config),
        "--trust",
        &trusting,
    ]);
    assert!(with_config.status.success());

    let seedless = trail_cli(&["--state-dir", path(&state), "--config", path(&config)]);
    assert_eq!(seedless.status.code(), Some(1));
    assert!(text(&seedless.stderr).contains("browser.audit_key"));

    let pinned = Trust::new(
        None,
        vec![Trusted {
            environment: "local".to_owned(),
            key,
        }],
    )
    .unwrap();
    let report = verify(&state, &state.join("trail"), &pinned, None, false).unwrap();
    assert_found(&report, Vec::new(), true);
}

#[tokio::test]
async fn a_store_file_of_schema_3_or_4_holding_no_browser_steps_reads_as_an_empty_trail() {
    for version in [3, 4] {
        let root = scratch(&format!("trail-v{version}"));
        let state = root.join("state");
        let file = state.join(STORE_FILE);
        let mut store = SqliteStore::open(&file).unwrap();
        let received = DomainEvent::TriggerReceived {
            trigger: TriggerId::new("slack:t1"),
            threads: Vec::new(),
            reached: Vec::new(),
            queued: false,
        };
        store
            .commit(ChangeSet {
                events: vec![Event {
                    at_ms: 1_000,
                    event: received,
                }],
                ..ChangeSet::default()
            })
            .unwrap();
        drop(store);
        let conn = rusqlite::Connection::open(&file).unwrap();
        if version == 3 {
            conn.execute_batch("DROP TABLE trails").unwrap();
        }
        conn.execute("DELETE FROM schema_version WHERE version > ?1", [version])
            .unwrap();
        drop(conn);
        let old = SqliteStore::open_read_only(&file).unwrap();
        assert_eq!(old.schema_version().unwrap(), version);
        drop(old);

        let report = verify(&state, &state.join("trail"), &Trust::default(), None, false).unwrap();
        assert_found(&report, Vec::new(), true);
        assert_eq!(report.chain.as_ref().unwrap().events, 1, "schema {version}");
        assert!(report.tickets.is_empty(), "schema {version}");

        let output = trail_cli(&["--state-dir", path(&state)]);
        assert!(output.status.success(), "schema {version}");
        let out = text(&output.stdout);
        assert!(out.contains("chain ok: 1 event, head "), "schema {version}");
        assert!(out.contains("no browser steps"), "schema {version}");
        std::fs::remove_dir_all(&root).unwrap();
    }
}

async fn serve_until_a_step_hangs<T: Send + 'static>(
    agent: &Agent,
    beside: impl FnOnce() -> T + Send + 'static,
) -> T {
    let _never = agent.fake.hold_on("Page.navigate");
    let (config, state, store) = (agent.config(), agent.state(), agent.store());
    let host = agent.host(Box::new(|prompt| {
        if first_prompt(prompt) {
            vec![
                ("browser_read", json!({})),
                ("browser_open", json!({"url": format!("{SITE}/login")})),
            ]
        } else {
            Vec::new()
        }
    }));
    let serving = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let found = runtime.block_on(async {
            let besides = async {
                until("the second step to hang", || {
                    (lines(&trail(&store)).len() == 3).then_some(())
                })
                .await;
                tokio::task::spawn_blocking(beside).await.unwrap()
            };
            tokio::select! {
                ended = serve_with(config, &state, false, Some(host)) => {
                    panic!("serve ended: {ended:?}")
                }
                found = besides => found,
            }
        });
        runtime.shutdown_timeout(Duration::from_secs(5));
        found
    });
    let found = tokio::task::spawn_blocking(move || serving.join().unwrap())
        .await
        .unwrap();
    gone(&agent.fake, "the killed host's browser").await;
    found
}

fn nth_ask(events: &[Event], n: u32, which: usize) -> (TicketId, AssignmentId) {
    events
        .iter()
        .filter_map(|event| match &event.event {
            DomainEvent::TrailAsked {
                ticket,
                assignment,
                n: at,
                ..
            } if *at == n => Some((ticket.clone(), assignment.clone())),
            _ => None,
        })
        .nth(which)
        .expect("no such ask")
}

#[tokio::test]
async fn a_step_still_running_beside_a_serving_agent_warns_and_its_open_browser_is_not_unclosed() {
    let agent = Agent::new("trail-live");
    agent.ask();
    let trust = Trust::new(Some(&agent.config()), Vec::new()).unwrap();
    let state = agent.state();

    let (lenient, strict) = serve_until_a_step_hangs(&agent, move || {
        let check = |strict| verify(&state, &state.join("trail"), &trust, None, strict).unwrap();
        (check(false), check(true))
    })
    .await;

    let events = SqliteStore::open_read_only(agent.store())
        .unwrap()
        .events()
        .unwrap();
    let (ticket, assignment) = nth_ask(&events, 2, 0);
    let unanswered = Problem::Unanswered {
        at: StepAt {
            ticket,
            assignment,
            n: 2,
        },
    };
    assert_found(&lenient, vec![warning(unanswered.clone())], true);
    assert_found(&strict, vec![fatal(unanswered)], false);
    assert_eq!(
        lenient.tickets[0].assignments[0].ended, None,
        "the Assignment is still running, so its open browser is a live one"
    );
}

#[tokio::test]
async fn a_ticket_a_crash_gave_two_assignments_replays_both_and_asks_which_a_step_is_of() {
    let agent = Agent::new("trail-restart");
    agent.ask();
    serve_until_a_step_hangs(&agent, || ()).await;
    agent
        .serve_until_idle(Box::new(|prompt| {
            if !prompt.contains("This ticket was worked on before") {
                return Vec::new();
            }
            vec![
                ("browser_open", json!({"url": format!("{SITE}/login")})),
                (
                    "worker_report",
                    json!({"status": "ready", "text": "Opened again."}),
                ),
            ]
        }))
        .await;
    let events = SqliteStore::open_read_only(agent.store())
        .unwrap()
        .events()
        .unwrap();
    let (ticket, crashed) = nth_ask(&events, 2, 0);
    let (_, restarted) = nth_ask(&events, 1, 1);
    assert_ne!(crashed, restarted);
    let (state, config) = (agent.state(), agent.root.join("agent.yaml"));

    let trust = Trust::new(Some(&agent.config()), Vec::new()).unwrap();
    let report = verify(&state, &state.join("trail"), &trust, None, false).unwrap();

    let lost = Problem::Unanswered {
        at: StepAt {
            ticket: ticket.clone(),
            assignment: crashed.clone(),
            n: 2,
        },
    };
    assert_found(&report, vec![warning(lost)], true);
    let assignments = &report.tickets[0].assignments;
    let shape: Vec<_> = assignments
        .iter()
        .map(|each| (each.assignment.clone(), each.steps.len()))
        .collect();
    assert_eq!(shape, [(crashed.clone(), 2), (restarted.clone(), 2)]);
    assert_eq!(
        assignments[0].ended.as_deref(),
        Some(HOST_RESTARTED),
        "the restart ended the Assignment the dead host left running"
    );

    let base = [
        "--state-dir",
        path(&state),
        "--config",
        path(&config),
        "--ticket",
        ticket.as_str(),
        "--step",
        "1",
    ];
    let ambiguous = trail_cli(&base);
    assert_eq!(ambiguous.status.code(), Some(1));
    let why = text(&ambiguous.stderr);
    assert!(why.contains("name one with --assignment"), "{why}");
    assert!(
        why.contains(crashed.as_str()) && why.contains(restarted.as_str()),
        "{why}"
    );
    let named = trail_cli(&[&base[..], &["--assignment", restarted.as_str()]].concat());
    let steps = signed(&trail(&agent.store()));
    let opened = steps
        .iter()
        .find(|step| step.body.assignment == restarted && step.body.n == 1)
        .unwrap();
    let blob = state
        .join("trail")
        .join(ticket.as_str())
        .join(page_seen(opened).view.digits());
    assert!(named.status.success(), "a warning does not fail the check");
    assert_eq!(named.stdout, std::fs::read(blob).unwrap());
}
