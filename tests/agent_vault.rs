use anyhow::Result;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use solos_agent_core::AllowlistPolicy;
use solos_agent_core::model::{
    Actor, Addressed, Ask, AssignmentId, ChannelRef, DomainEvent, Effect, SecretGrant, SecretId,
    ThreadKey, TicketId, Trigger, TriggerId,
};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::broker::{
    self, AgentBroker, BrokerParts, DecideTerms, ModelTerms, VaultParts, WorkerParts, WorkerSeat,
};
use solos_swarm::agent::jsonl::{JsonlChannel, JsonlSource};
use solos_swarm::agent::ports::{BoxFuture, Channel, Delivery, Executor, Fact, FactSink, Launch};
use solos_swarm::agent::scripted::ScriptedRouter;
use solos_swarm::agent::{Adapters, RunOptions, Runtime, STORE_FILE};
use solos_swarm::gateway::{Dispatch, PriceTable, Provider, Upstream};
use solos_swarm::vault::{
    Approval, ItemKind, KeyScheme, NewItem, SandboxView, SecretValue, Vault, VaultConfig,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const LIMIT: Duration = Duration::from_secs(30);
const KEY_WITH_JSON_ESCAPES: &str = "sk-Pr0be\"k3y/value~42";
const OTHER_KEY: &str = "sk-other-k3y-0123456789";
const PASSWORD: &str = "pw-Probe-7f3a9c-gui";
const CHILD_PROXY_VAR: &str = "SOLOS_TEST_AMBIENT_PROXY";

struct Silent;

impl Provider for Silent {
    fn dispatch(&self, _: Value) -> Dispatch {
        Box::pin(async { Err(anyhow::anyhow!("no model in this test")) })
    }
}

#[derive(Default)]
struct Starting(Mutex<Vec<(TicketId, AssignmentId)>>);

impl Executor for Starting {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let started = Fact::Started {
                ticket: launch.ticket.clone(),
                assignment: launch.assignment.clone(),
            };
            facts.send(started).await?;
            self.0
                .lock()
                .unwrap()
                .push((launch.ticket, launch.assignment));
            Ok(())
        })
    }
    fn deliver(&self, _: Delivery, _: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&self, _: TicketId, _: AssignmentId, _: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

fn terms() -> ModelTerms {
    ModelTerms {
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
        thinking: None,
    }
}

struct World {
    root: PathBuf,
    runtime: Option<Runtime>,
    broker: AgentBroker,
    client: reqwest::Client,
    server: JoinHandle<()>,
    started: Arc<Starting>,
    vault: VaultConfig,
}

impl World {
    fn new(name: &str) -> Self {
        Self::build(name, Some(true), true)
    }

    fn without_approvers(name: &str) -> Self {
        Self::build(name, Some(false), true)
    }

    fn without_vault(name: &str) -> Self {
        Self::build(name, None, true)
    }

    /// A host whose `fetch_url` reaches the public internet only, as a host
    /// does by default.
    fn on_public_networks_only(name: &str) -> Self {
        Self::build(name, Some(true), false)
    }

    /// `allow_private_networks`: the sites the tests fetch from are on
    /// loopback, so most worlds allow it.
    fn build(name: &str, takes_approvals: Option<bool>, allow_private_networks: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agent-vault-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let state = root.join("state");
        std::fs::create_dir_all(&state).unwrap();
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
            router: Arc::new(ScriptedRouter::new(500_000)),
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
        let config = VaultConfig {
            path: root.join("keys").join("vault.enc"),
            key_file: root.join("keys").join("vault.key"),
        };
        let vault = takes_approvals.map(|takes_approvals| {
            let view = SandboxView::new([]);
            Vault::init(&config, &view).unwrap();
            VaultParts {
                vault: Arc::new(Vault::open(&config, &view).unwrap()),
                asks_on: if takes_approvals {
                    ["slack".to_owned()].into()
                } else {
                    BTreeSet::new()
                },
            }
        });
        let parts = BrokerParts {
            dir: state.clone(),
            store: store(),
            provider: Arc::new(Silent),
            facts: runtime.facts(),
            decide: DecideTerms {
                default_budget_microusd: Some(500_000),
                max_budget_microusd: None,
                swarm: None,
                repos: BTreeMap::new(),
                ..DecideTerms::default()
            },
            workers: Some(WorkerParts {
                terms: terms(),
                code: None,
                vault,
                browser: None,
                allow_private_networks,
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
            runtime: Some(runtime),
            broker,
            client,
            server,
            started,
            vault: config,
        }
    }

    fn store_path(&self) -> PathBuf {
        self.root.join("state").join(STORE_FILE)
    }

    fn store(&self) -> SqliteStore {
        SqliteStore::open(self.store_path()).unwrap()
    }

    fn add(&self, name: &str, kind: ItemKind, users: &[&str], approval: Approval, value: &str) {
        let file = self.root.join(format!("{name}.value"));
        std::fs::write(&file, value).unwrap();
        let item = NewItem {
            name: name.parse().unwrap(),
            kind,
            users: users.iter().map(|user| (*user).to_owned()).collect(),
            approval,
            value: SecretValue::read_once(&file).unwrap(),
        };
        let vault = Vault::open(&self.vault, &SandboxView::new([])).unwrap();
        vault.add(item, false).unwrap();
    }

    fn key(&self, name: &str, host: &str, users: &[&str], approval: Approval, value: &str) {
        let kind = ItemKind::Key {
            hosts: vec![host.parse().unwrap()],
            header: "authorization".parse().unwrap(),
            scheme: KeyScheme::Bearer,
            about: format!("{name} for the tests"),
        };
        self.add(name, kind, users, approval, value);
    }

    fn ask(&self) {
        self.ask_on("slack");
    }

    fn ask_on(&self, source: &str) {
        let trigger = Trigger {
            id: TriggerId::new("t1"),
            source: source.into(),
            at_ms: 1_000,
            actor: Actor::person("alice"),
            addressed: Addressed::Mention,
            text: "use the API".into(),
            threads: vec![ThreadKey::new("T1")],
            reply_to: vec![ChannelRef {
                source: source.into(),
                address: BTreeMap::from([("thread_ts".to_owned(), "T1".to_owned())]),
            }],
            meta: json!({}),
        };
        let line = format!("{}\n", serde_json::to_string(&trigger).unwrap());
        std::fs::write(self.root.join("triggers.jsonl"), line).unwrap();
    }

    async fn admit_worker(&self) -> String {
        let (ticket, assignment) = until("the ticket's launch", || {
            self.started.0.lock().unwrap().first().cloned()
        })
        .await;
        let token = format!("worker-{ticket}");
        let seat = WorkerSeat::new(ticket, assignment, self.root.join("work"), 500_000);
        self.broker
            .admit_worker(token.clone(), seat, "You are a worker.".into())
            .unwrap();
        token
    }

    async fn post(&self, tool: &str, token: &str, body: Value) -> (u16, Value) {
        let response = self
            .client
            .post(format!("http://broker/tool/{tool}"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn listed(&self, token: &str) -> Vec<String> {
        let response = self
            .client
            .get("http://broker/tools")
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        let listed: Value = response.json().await.unwrap();
        listed["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{listed}"))
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

#[derive(Clone, Debug)]
struct Visit {
    path: String,
    authorization: Option<String>,
    keys_used_on_arrival: usize,
}

type Visits = Arc<Mutex<Vec<Visit>>>;

struct Site {
    base: String,
    host: String,
    visits: Visits,
    server: JoinHandle<()>,
}

impl Site {
    async fn start(store: PathBuf) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let visits = Visits::default();
        let app = axum::Router::new()
            .fallback(visit)
            .with_state((visits.clone(), store));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base: format!("http://{host}"),
            host,
            visits,
            server,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn visits(&self) -> Vec<(String, Option<String>)> {
        let visits = self.visits.lock().unwrap();
        visits
            .iter()
            .map(|visit| (visit.path.clone(), visit.authorization.clone()))
            .collect()
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn visit(
    State((visits, store)): State<(Visits, PathBuf)>,
    uri: Uri,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
    visits.lock().unwrap().push(Visit {
        path: uri.path().to_owned(),
        authorization: authorization.clone(),
        keys_used_on_arrival: keys_used(&store).len(),
    });
    if let Some(to) = query.get("to") {
        return (StatusCode::FOUND, [(header::LOCATION, to.clone())]).into_response();
    }
    let echoed: BTreeMap<String, String> = headers
        .iter()
        .map(|(name, value)| {
            let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
            (name.to_string(), value)
        })
        .collect();
    let seen = STANDARD.encode(authorization.unwrap_or_default());
    ([("x-seen", seen)], Json(json!({ "headers": echoed }))).into_response()
}

#[derive(Debug, PartialEq)]
struct KeyUse {
    secret: String,
    method: String,
    url: String,
}

fn keys_used(store: &Path) -> Vec<KeyUse> {
    let Ok(store) = SqliteStore::open_read_only(store) else {
        return Vec::new();
    };
    store
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            DomainEvent::KeyUsed {
                secret,
                method,
                url,
                ..
            } => Some(KeyUse {
                secret: secret.to_string(),
                method,
                url,
            }),
            _ => None,
        })
        .collect()
}

fn redirect(base: &str, to: &str) -> String {
    reqwest::Url::parse_with_params(base, &[("to", to)])
        .unwrap()
        .to_string()
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
async fn a_key_goes_to_its_host_on_every_hop_and_never_to_another_host() {
    let mut w = World::new("hops");
    let (home, away) = (
        Site::start(w.store_path()).await,
        Site::start(w.store_path()).await,
    );
    w.key(
        "probe",
        &home.host,
        &[],
        Approval::None,
        KEY_WITH_JSON_ESCAPES,
    );
    let back = home.url("/back");
    let start = redirect(&home.url("/start"), &redirect(&away.url("/bounce"), &back));
    w.ask();

    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let token = w.admit_worker().await;
        let fetch = json!({"url": start, "key": "probe"});
        let (status, reply) = w.post("fetch_url", &token, fetch).await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["final_url"], back);
        assert_eq!(reply["redirects"].as_array().unwrap().len(), 2, "{reply}");
    })
    .await;

    let bearer = Some(format!("Bearer {KEY_WITH_JSON_ESCAPES}"));
    assert_eq!(
        home.visits(),
        [
            ("/start".to_owned(), bearer.clone()),
            ("/back".to_owned(), bearer)
        ],
        "the key's host gets it on the first hop and on the hop back to it"
    );
    assert_eq!(
        away.visits(),
        [("/bounce".to_owned(), None)],
        "a redirect to another host:port gets no key"
    );
}

#[tokio::test]
async fn a_keyed_fetch_to_a_private_address_is_refused_before_the_key_leaves() {
    let mut w = World::on_public_networks_only("private");
    let (site, away) = (
        Site::start(w.store_path()).await,
        Site::start(w.store_path()).await,
    );
    // An operator may list a loopback host for a key on purpose; the host's
    // own rule still refuses to send anything there.
    w.key("probe", &site.host, &[], Approval::None, "sk-private-probe");
    w.ask();

    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let token = w.admit_worker().await;
        let keyed = json!({"url": site.url("/echo"), "key": "probe"});
        let (status, reply) = w.post("fetch_url", &token, keyed).await;
        assert_eq!(status, 403, "{reply}");
        assert_eq!(reply["refused"], true);
        assert_eq!(reply["reason"], "private_address");
        let why = reply["error"].as_str().unwrap();
        assert!(
            why.starts_with("fetch_url refused: 127.0.0.1 resolves to a private address"),
            "{why}"
        );
        assert!(!reply.to_string().contains("sk-private-probe"), "{reply}");
        // A keyless call does not reach it either, nor does one to a site
        // the key is not for.
        for url in [site.url("/echo"), away.url("/echo")] {
            let (status, reply) = w.post("fetch_url", &token, json!({"url": url})).await;
            assert_eq!(status, 403, "{reply}");
            assert_eq!(reply["reason"], "private_address");
        }
    })
    .await;

    assert!(site.visits().is_empty(), "{:?}", site.visits());
    assert!(away.visits().is_empty(), "{:?}", away.visits());
}

#[tokio::test]
async fn a_site_that_echoes_the_key_comes_back_scrubbed() {
    let mut w = World::new("echo");
    let site = Site::start(w.store_path()).await;
    w.key(
        "probe",
        &site.host,
        &[],
        Approval::None,
        KEY_WITH_JSON_ESCAPES,
    );
    w.ask();

    let runtime = w.runtime.take().unwrap();
    let mut reply = Value::Null;
    with_runtime(runtime, async {
        let token = w.admit_worker().await;
        let fetch = json!({"url": site.url("/echo"), "key": "probe"});
        let (status, got) = w.post("fetch_url", &token, fetch).await;
        assert_eq!(status, 200, "{got}");
        reply = got;
    })
    .await;

    assert_eq!(
        site.visits(),
        [(
            "/echo".to_owned(),
            Some(format!("Bearer {KEY_WITH_JSON_ESCAPES}"))
        )],
        "the site had the key to echo"
    );
    let text = reply.to_string();
    let json_escaped = KEY_WITH_JSON_ESCAPES.replace('"', "\\\"");
    for form in [
        KEY_WITH_JSON_ESCAPES.to_owned(),
        json_escaped,
        STANDARD.encode(KEY_WITH_JSON_ESCAPES),
        STANDARD.encode(format!("Bearer {KEY_WITH_JSON_ESCAPES}")),
    ] {
        assert!(!text.contains(&form), "{form} reached the model: {text}");
    }
    let body: Value = serde_json::from_str(reply["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["headers"]["authorization"], "Bearer [vault:probe]");
    assert!(
        reply["headers"]["x-seen"]
            .as_str()
            .unwrap()
            .contains("[vault:probe]"),
        "{reply}"
    );
}

#[tokio::test]
async fn key_used_is_on_the_chain_before_the_site_sees_the_request() {
    let mut w = World::new("ahead");
    let site = Site::start(w.store_path()).await;
    w.key(
        "probe",
        &site.host,
        &[],
        Approval::None,
        KEY_WITH_JSON_ESCAPES,
    );
    w.ask();
    let url = site.url("/echo?n=1");

    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let token = w.admit_worker().await;
        let fetch = json!({"url": url, "key": "probe", "method": "post", "body": "{}"});
        let (status, reply) = w.post("fetch_url", &token, fetch).await;
        assert_eq!(status, 200, "{reply}");
        let (status, reply) = w.post("fetch_url", &token, json!({"url": url})).await;
        assert_eq!(status, 200, "{reply}");
    })
    .await;

    let visits = site.visits.lock().unwrap().clone();
    let seen: Vec<usize> = visits
        .iter()
        .map(|visit| visit.keys_used_on_arrival)
        .collect();
    assert_eq!(
        seen,
        [1, 1],
        "the keyed request arrived after its KeyUsed, and the unkeyed one added none"
    );
    assert_eq!(
        keys_used(&w.store_path()),
        [KeyUse {
            secret: "local/probe".into(),
            method: "POST".into(),
            url
        }]
    );
    w.store().verify_chain().unwrap();
}

#[tokio::test]
async fn no_fetch_goes_through_an_ambient_proxy() {
    if let Ok(proxy) = std::env::var(CHILD_PROXY_VAR) {
        return behind_a_proxy(&proxy).await;
    }
    let (proxy, requests) = fake_proxy();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "no_fetch_goes_through_an_ambient_proxy",
            "--exact",
            "--nocapture",
        ])
        .env(CHILD_PROXY_VAR, &proxy)
        .env("HTTP_PROXY", &proxy)
        .env("HTTPS_PROXY", &proxy)
        .env("ALL_PROXY", &proxy)
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .status()
        .unwrap();
    assert!(status.success(), "the run behind the proxy failed");
    let requests = requests.lock().unwrap().clone();
    assert!(
        requests.is_empty(),
        "a proxy opens the connection itself, where the private-address check cannot see \
         it, so neither fetch went through it: {requests:?}"
    );
}

async fn behind_a_proxy(proxy: &str) {
    let mut w = World::new("proxy");
    let site = Site::start(w.store_path()).await;
    w.key(
        "probe",
        &site.host,
        &[],
        Approval::None,
        KEY_WITH_JSON_ESCAPES,
    );
    w.ask();

    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let token = w.admit_worker().await;
        let url = site.url("/echo");
        let (status, keyed) = w
            .post("fetch_url", &token, json!({"url": url, "key": "probe"}))
            .await;
        assert_eq!(status, 200, "{keyed}");
        assert_ne!(keyed["body"], "proxied", "{keyed}");
        let (status, plain) = w.post("fetch_url", &token, json!({"url": url})).await;
        assert_eq!(status, 200, "{plain}");
        assert_ne!(
            plain["body"], "proxied",
            "the unkeyed fetch ignores {proxy} too: {plain}"
        );
    })
    .await;

    assert_eq!(
        site.visits(),
        [
            (
                "/echo".to_owned(),
                Some(format!("Bearer {KEY_WITH_JSON_ESCAPES}"))
            ),
            ("/echo".to_owned(), None)
        ],
        "both requests went straight to the site, the keyed one with its key"
    );
}

fn fake_proxy() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let kept = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                continue;
            };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                head.push(byte[0]);
            }
            kept.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&head).into_owned());
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\nconnection: close\r\n\r\nproxied",
            );
        }
    });
    (url, requests)
}

#[tokio::test]
async fn a_key_is_refused_off_its_hosts_and_where_the_ticket_may_not_use_it() {
    let mut w = World::new("refused");
    let site = Site::start(w.store_path()).await;
    w.key(
        "probe",
        "api.example.com",
        &[],
        Approval::None,
        KEY_WITH_JSON_ESCAPES,
    );
    w.key(
        "bobs",
        &site.host,
        &["slack:bob"],
        Approval::None,
        OTHER_KEY,
    );
    let login = ItemKind::Login {
        sites: vec!["https://github.com".parse().unwrap()],
        username: "gui".into(),
        hint: "Gui's work account".into(),
    };
    w.add("github-gui", login, &[], Approval::None, PASSWORD);
    w.ask();

    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let token = w.admit_worker().await;
        for (case, url, key, status, why) in [
            (
                "plain http to its own host",
                "http://api.example.com/v1".to_owned(),
                "probe",
                403,
                "probe goes only to api.example.com over https, not to http://api.example.com/v1",
            ),
            (
                "another host",
                site.url("/echo"),
                "probe",
                403,
                "probe goes only to api.example.com over https",
            ),
            (
                "no such key",
                site.url("/echo"),
                "nope",
                403,
                "there is no key nope on local",
            ),
            (
                "a login",
                site.url("/echo"),
                "github-gui",
                403,
                "github-gui is a login, not an API key",
            ),
            (
                "someone else's key",
                site.url("/echo"),
                "bobs",
                403,
                "bobs is not for this ticket's requester",
            ),
            (
                "not a name",
                site.url("/echo"),
                "Bad Name",
                422,
                "cannot name a vault item",
            ),
        ] {
            let fetch = json!({"url": url, "key": key});
            let (got, reply) = w.post("fetch_url", &token, fetch).await;
            let error = reply["error"].as_str().unwrap_or_default();
            assert_eq!(got, status, "{case}: {reply}");
            assert!(error.contains(why), "{case}: {error}");
        }
    })
    .await;

    assert!(site.visits().is_empty(), "no refused request left");
    assert!(keys_used(&w.store_path()).is_empty());
}

#[tokio::test]
async fn a_first_use_asks_a_person_once_and_is_refused_until_they_answer() {
    let mut w = World::new("first-use");
    let site = Site::start(w.store_path()).await;
    w.key(
        "probe",
        &site.host,
        &["slack:alice"],
        Approval::FirstUse,
        KEY_WITH_JSON_ESCAPES,
    );
    w.ask();
    let label = format!("send the API key probe to {}", site.host);

    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let token = w.admit_worker().await;
        let fetch = json!({"url": site.url("/echo"), "key": "probe"});
        let (status, reply) = w.post("fetch_url", &token, fetch.clone()).await;
        assert_eq!(status, 409, "{reply}");
        assert_eq!(
            reply["error"],
            format!(
                "A person must allow this ticket to {label} first; they were asked in the \
                 thread. End your turn: their answer comes back as new input."
            )
        );
        let (status, listed) = w.post("vault_list", &token, json!({})).await;
        assert_eq!(status, 200, "{listed}");
        assert_eq!(listed["keys"][0]["approval"], "waiting");
        let (status, reply) = w.post("fetch_url", &token, fetch).await;
        assert_eq!(status, 409, "{reply}");
        assert_eq!(
            reply["error"],
            format!(
                "A person has not yet allowed this ticket to {label}; they were asked in the \
                 thread. End your turn: their answer comes back as new input."
            )
        );
    })
    .await;

    let secret = SecretId::try_from("local/probe".to_owned()).unwrap();
    let ticket = w.store().tickets().unwrap().remove(0);
    let Some(SecretGrant::Waiting {
        label: asked,
        answerable_by,
        ..
    }) = ticket.secrets.get(&secret)
    else {
        panic!("{:?}", ticket.secrets)
    };
    assert_eq!(
        (asked, answerable_by),
        (&label, &vec!["slack:alice".to_owned()])
    );
    let asks: Vec<Ask> = w
        .store()
        .effects()
        .unwrap()
        .into_iter()
        .filter_map(|queued| match queued.effect {
            Effect::Send(message) => message.ask,
            _ => None,
        })
        .collect();
    assert_eq!(asks, [Ask::Secret { secret }], "one ask for two calls");
    assert!(site.visits().is_empty());
    assert!(keys_used(&w.store_path()).is_empty());
}

#[tokio::test]
async fn an_item_that_needs_approval_is_refused_where_nobody_can_approve() {
    let mut w = World::without_approvers("no-approver");
    let site = Site::start(w.store_path()).await;
    w.key(
        "probe",
        &site.host,
        &[],
        Approval::FirstUse,
        KEY_WITH_JSON_ESCAPES,
    );
    w.ask();

    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let token = w.admit_worker().await;
        let fetch = json!({"url": site.url("/echo"), "key": "probe"});
        let (status, reply) = w.post("fetch_url", &token, fetch).await;
        assert_eq!(
            (status, &reply["error"]),
            (
                403,
                &json!(
                    "probe needs a person's approval, and none of this ticket's channels can ask \
                     for one: only a Slack or Discord thread with an approver shows the buttons"
                )
            )
        );
        let (_, listed) = w.post("vault_list", &token, json!({})).await;
        assert_eq!(listed["keys"][0]["approval"], "no_approver");
    })
    .await;

    assert!(w.store().tickets().unwrap()[0].secrets.is_empty());
    assert!(site.visits().is_empty());
}

#[tokio::test]
async fn a_first_use_on_a_ticket_whose_channels_cannot_ask_is_refused_not_left_waiting() {
    let mut w = World::new("no-buttons");
    let site = Site::start(w.store_path()).await;
    w.key(
        "probe",
        &site.host,
        &[],
        Approval::FirstUse,
        KEY_WITH_JSON_ESCAPES,
    );
    w.ask_on("github");

    let runtime = w.runtime.take().unwrap();
    with_runtime(runtime, async {
        let token = w.admit_worker().await;
        let fetch = json!({"url": site.url("/echo"), "key": "probe"});
        let (status, reply) = w.post("fetch_url", &token, fetch).await;
        assert_eq!(
            (status, &reply["error"]),
            (
                403,
                &json!(
                    "probe needs a person's approval, and none of this ticket's channels can ask \
                     for one: only a Slack or Discord thread with an approver shows the buttons"
                )
            ),
            "this agent takes approvals on Slack, and this Ticket answers on GitHub"
        );
        let (_, listed) = w.post("vault_list", &token, json!({})).await;
        assert_eq!(listed["keys"][0]["approval"], "no_approver");
    })
    .await;

    assert!(
        w.store().tickets().unwrap()[0].secrets.is_empty(),
        "nobody was asked"
    );
    assert!(site.visits().is_empty());
}

#[tokio::test]
async fn vault_list_shows_the_items_a_ticket_may_use_and_none_of_their_values() {
    let mut w = World::new("list");
    w.key(
        "probe",
        "api.example.com",
        &[],
        Approval::None,
        KEY_WITH_JSON_ESCAPES,
    );
    w.key(
        "gated",
        "api.example.com",
        &[],
        Approval::FirstUse,
        OTHER_KEY,
    );
    w.key(
        "bobs",
        "api.example.com",
        &["slack:bob"],
        Approval::None,
        "sk-bobs-k3y-0123456789",
    );
    let login = ItemKind::Login {
        sites: vec!["https://github.com".parse().unwrap()],
        username: "gui".into(),
        hint: "Gui's work account".into(),
    };
    w.add(
        "github-gui",
        login,
        &["slack:alice"],
        Approval::FirstUse,
        PASSWORD,
    );
    w.ask();

    let runtime = w.runtime.take().unwrap();
    let mut listed = Value::Null;
    with_runtime(runtime, async {
        let token = w.admit_worker().await;
        assert!(w.listed(&token).await.contains(&"vault_list".to_owned()));
        let (status, got) = w.post("vault_list", &token, json!({})).await;
        assert_eq!(status, 200, "{got}");
        listed = got;
    })
    .await;

    assert_eq!(
        listed["logins"],
        json!([{
            "login": "github-gui", "sites": ["https://github.com"], "username": "gui",
            "hint": "Gui's work account", "where": "local", "use": [], "approval": "needed",
        }])
    );
    assert_eq!(
        listed["keys"],
        json!([
            {"key": "gated", "hosts": ["api.example.com"], "about": "gated for the tests",
             "where": "local", "use": ["fetch_url"], "approval": "needed"},
            {"key": "probe", "hosts": ["api.example.com"], "about": "probe for the tests",
             "where": "local", "use": ["fetch_url"], "approval": "none"},
        ]),
        "bobs is for someone else"
    );
    assert!(
        listed["note"]
            .as_str()
            .unwrap()
            .starts_with("You never see these values.")
    );
    let text = listed.to_string();
    for value in [
        KEY_WITH_JSON_ESCAPES,
        OTHER_KEY,
        PASSWORD,
        "sk-bobs-k3y-0123456789",
    ] {
        assert!(!text.contains(value), "{value} is listed: {text}");
        assert!(!text.contains(&STANDARD.encode(value)), "{text}");
    }
}

#[tokio::test]
async fn without_a_vault_vault_list_is_not_offered_and_a_key_is_refused() {
    let w = World::without_vault("none");
    let seat = WorkerSeat::new(
        TicketId::new("tk_1"),
        AssignmentId::new("as_1"),
        w.root.join("work"),
        500_000,
    );
    w.broker
        .admit_worker("worker".into(), seat, "You are a worker.".into())
        .unwrap();

    assert_eq!(
        w.listed("worker").await,
        ["worker_report", "worker_handback", "fetch_url"]
    );
    assert_eq!(w.broker.worker_tools(None, false), w.listed("worker").await);
    let no_vault = json!({"error": "no vault on this host"});
    assert_eq!(
        w.post("vault_list", "worker", json!({})).await,
        (403, no_vault.clone())
    );
    let fetch = json!({"url": "https://api.example.com/v1", "key": "probe"});
    assert_eq!(w.post("fetch_url", "worker", fetch).await, (403, no_vault));
}
