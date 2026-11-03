use anyhow::Result;
use axum::response::Html;
use axum::routing::{get, post};
use serde_json::{Value, json};
use solos_agent_core::AllowlistPolicy;
use solos_agent_core::model::{
    Actor, Addressed, Ask, AssignmentId, ChannelRef, DomainEvent, Effect, InputBody, SecretGrant,
    SecretId, ThreadKey, Ticket, TicketId, TicketOutcome, Trigger, TriggerId,
};
use solos_agent_core::trail::{
    ActText, CloseReason, SignedStep, StepOutcome, TrailAction, TrailKey, TrailOutcome,
};
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
use solos_swarm::agent::trail::{BlobStore, TrailStoreConfig};
use solos_swarm::agent::{
    Adapters, AgentConfig, BROWSER_DIR, PiHost, RunOptions, Runtime, STORE_FILE, Summary,
    serve_with,
};
use solos_swarm::browse::fake::{Element, FakeCdp, Form, Page};
use solos_swarm::browse::{
    Blob, BlobKind, BrowseCall, Browsed, BrowserConfig, Browsers, Launcher, TrailSigner,
    verify_step,
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
                default_budget_microusd: BUDGET,
                max_budget_microusd: None,
                swarm: None,
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
                },
                code: None,
                vault: Some(VaultParts {
                    vault,
                    asks_on: setup.asks_on.iter().map(|s| (*s).to_owned()).collect(),
                }),
                browser,
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
        let (token, _seat) = w.admit_worker().await;
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

#[tokio::test]
async fn a_worker_signs_in_through_its_tools_and_its_browser_closes_when_the_ticket_ends() {
    let agent = Agent::new("serve-login");
    agent.ask();
    let summary = agent
        .serve_until_idle(Box::new(|prompt| {
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
        }))
        .await;

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
