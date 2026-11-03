//! A worker's background jobs through the agent broker on its socket, in a
//! real bwrap sandbox: start, wait and end; stop and timeout with no process
//! left; the per-Ticket limit; jobs that die with their Assignment; who may
//! see a job; `run_code` moving to the background; a job's end reaching the
//! worker once, in its turn or as one Input after it; and `job_wait` cut
//! short by new input; and `GET /tools` on a host with `run_code`. Each
//! test skips without Linux, bwrap and prlimit.

use anyhow::Result;
use serde_json::{Value, json};
use solos_agent_core::AllowlistPolicy;
use solos_agent_core::model::{
    Actor, Addressed, AssignmentId, ChannelRef, InputBody, ThreadKey, TicketId, Trigger, TriggerId,
};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::broker::registry::{self, PrincipalKind};
use solos_swarm::agent::broker::{
    self, Account, AgentBroker, BrokerParts, DecideTerms, Grant, ModelTerms, Principal,
    ROUTER_TOOLS, WORKER_TOOLS, WorkerParts, WorkerSeat,
};
use solos_swarm::agent::jobs::JobLimits;
use solos_swarm::agent::jsonl::{JsonlChannel, JsonlSource};
use solos_swarm::agent::ports::{BoxFuture, Channel, Delivery, Executor, Fact, FactSink, Launch};
use solos_swarm::agent::scripted::ScriptedRouter;
use solos_swarm::agent::{Adapters, RunOptions, Runtime, STORE_FILE};
use solos_swarm::code::{CodeConfig, CodeExecutor};
use solos_swarm::gateway::{Dispatch, PriceTable, Provider, Upstream};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

const LIMIT: Duration = Duration::from_secs(60);
const ROUTER: &str = "router-token";
const LIMITS: JobLimits = JobLimits {
    background_after: Duration::from_secs(1),
    max_jobs: 2,
    max_wall: Duration::from_secs(300),
    output_bytes: 65_536,
};

fn sandbox() -> bool {
    let found = cfg!(target_os = "linux")
        && Path::new("/usr/bin/bwrap").is_file()
        && Path::new("/usr/bin/prlimit").is_file();
    if !found {
        eprintln!("skipped: background jobs need Linux, /usr/bin/bwrap and /usr/bin/prlimit");
    }
    found
}

struct Silent;

impl Provider for Silent {
    fn dispatch(&self, _: Value) -> Dispatch {
        Box::pin(async { Err(anyhow::anyhow!("no model in this test")) })
    }
}

/// Starts every Assignment and leaves it running.
struct Starting;

impl Executor for Starting {
    fn launch(&self, launch: Launch, facts: FactSink) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            facts
                .send(Fact::Started {
                    ticket: launch.ticket,
                    assignment: launch.assignment,
                })
                .await
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
    }
}

/// A broker with `run_code` and `LIMITS` on the state directory's socket,
/// a runtime whose facts it sends, and the Router's token.
struct World {
    root: PathBuf,
    runtime: Option<Runtime>,
    broker: AgentBroker,
    client: reqwest::Client,
    server: tokio::task::JoinHandle<()>,
}

impl World {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agent-jobs-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let state = root.join("state");
        std::fs::create_dir_all(state.join("tickets")).unwrap();
        let store = || SqliteStore::open(state.join(STORE_FILE)).unwrap();
        let channel: Arc<dyn Channel> =
            Arc::new(JsonlChannel::open(root.join("outbox.jsonl")).unwrap());
        let adapters = Adapters {
            sources: vec![Box::new(JsonlSource::new(
                root.join("triggers.jsonl"),
                false,
            ))],
            channels: BTreeMap::from([("slack".to_owned(), channel)]),
            router: Arc::new(ScriptedRouter::new(500_000)),
            executor: Arc::new(Starting),
        };
        let policy = AllowlistPolicy {
            people: ["slack:alice".to_owned()].into(),
            ..AllowlistPolicy::default()
        };
        let options = RunOptions {
            tick: Duration::from_millis(10),
            ..RunOptions::default()
        };
        let runtime = Runtime::new(store(), policy, adapters, options);
        let mut code = CodeConfig::new(state.join("tickets"), Vec::new());
        code.concurrency = 4;
        let parts = BrokerParts {
            dir: state.clone(),
            store: store(),
            provider: Arc::new(Silent),
            facts: runtime.facts(),
            decide: DecideTerms {
                default_budget_microusd: 500_000,
                max_budget_microusd: None,
                swarm: None,
            },
            workers: Some(WorkerParts {
                terms: terms(),
                code: Some(Arc::new(CodeExecutor::from_config(code).unwrap())),
                vault: None,
                browser: None,
            }),
        };
        let broker = AgentBroker::with_jobs(parts, LIMITS).unwrap();
        let router = Grant {
            principal: Principal::Router,
            account: Account::Router {
                daily_microusd: 1_000_000,
            },
            terms: terms(),
            system_prompt: None,
        };
        broker.admit(ROUTER.into(), router).unwrap();
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
        }
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    /// A worker on `ticket`'s Assignment `assignment`, in a turn, with its
    /// token.
    fn worker(&self, ticket: &str, assignment: &str) -> (Arc<WorkerSeat>, String) {
        let work = self.state().join("tickets").join(ticket).join("work");
        std::fs::create_dir_all(&work).unwrap();
        let token = format!("worker-{ticket}-{assignment}");
        let seat = WorkerSeat::new(
            TicketId::new(ticket),
            AssignmentId::new(assignment),
            work,
            500_000,
        );
        let seat = self
            .broker
            .admit_worker(token.clone(), seat, "You are a worker.".into())
            .unwrap();
        seat.begin_turn();
        (seat, token)
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

    async fn start(&self, token: &str, code: &str) -> String {
        let (status, reply) = self.post("job_start", token, json!({"code": code})).await;
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["status"], "started", "{reply}");
        reply["job"].as_str().unwrap().to_owned()
    }

    async fn wait(&self, token: &str, job: &str, wait_s: u64) -> (u16, Value) {
        self.post("job_wait", token, json!({"job": job, "wait_s": wait_s}))
            .await
    }

    fn ask(&self, id: &str, text: &str) {
        let trigger = Trigger {
            id: TriggerId::new(id),
            source: "slack".into(),
            at_ms: 1_000,
            actor: Actor::person("alice"),
            addressed: Addressed::Mention,
            text: text.into(),
            threads: vec![ThreadKey::new("T1")],
            reply_to: vec![ChannelRef {
                source: "slack".into(),
                address: BTreeMap::from([("thread_ts".to_owned(), "T1".to_owned())]),
            }],
            meta: json!({}),
        };
        let line = format!("{}\n", serde_json::to_string(&trigger).unwrap());
        std::fs::write(self.root.join("triggers.jsonl"), line).unwrap();
    }

    fn store(&self) -> SqliteStore {
        SqliteStore::open(self.state().join(STORE_FILE)).unwrap()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A command whose every process, the sandbox's included, carries `marker`
/// on its command line: a background sleep and a foreground one.
fn sleepers(marker: &str) -> String {
    format!("(exec -a {marker} sleep 300) & echo up; exec -a {marker} sleep 300")
}

/// The command lines that carry `marker`.
fn command_lines(marker: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.parse::<u32>().is_ok())
        })
        .filter_map(|entry| std::fs::read(entry.path().join("cmdline")).ok())
        .map(|cmdline| String::from_utf8_lossy(&cmdline).into_owned())
        .filter(|cmdline| cmdline.contains(marker))
        .collect()
}

/// Wait until both of `marker`'s sleeps run, so it has printed `up`.
async fn until_running(marker: &str) {
    for _ in 0..400 {
        let sleeping = command_lines(marker)
            .iter()
            .filter(|cmdline| cmdline.starts_with(marker))
            .count();
        if sleeping >= 2 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{marker}'s sleeps never ran: {:?}", command_lines(marker));
}

/// Wait until no process carries `marker`.
async fn until_gone(marker: &str) {
    for _ in 0..400 {
        if command_lines(marker).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "{marker} left processes behind: {:?}",
        command_lines(marker)
    );
}

fn new_marker() -> String {
    format!("job-{}", uuid::Uuid::new_v4().simple())
}

#[tokio::test]
async fn get_tools_lists_run_code_and_the_job_tools_on_a_host_with_run_code_and_each_is_callable() {
    if !sandbox() {
        return;
    }
    let w = World::new("tools");
    let (_seat, token) = w.worker("tk_a", "as_a");
    let response = w
        .client
        .get("http://broker/tools")
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let listed: Value = response.json().await.unwrap();
    let mut specs = registry::tools(PrincipalKind::Worker);
    specs.retain(|tool| tool.name != "vault_list" && !tool.name.starts_with("browser_"));
    assert_eq!(
        listed["tools"],
        serde_json::to_value(specs).unwrap(),
        "every worker tool but vault_list and the browser's, from the registry: this host has \
         no vault and no browser"
    );
    let names: Vec<&str> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    let with_run_code = [
        "worker_report",
        "worker_handback",
        "run_code",
        "job_start",
        "job_wait",
        "job_stop",
        "fetch_url",
    ];
    assert_eq!(names, with_run_code);
    assert_eq!(w.broker.worker_tools(None), with_run_code, "its grants");
    for tool in ROUTER_TOOLS.iter().chain(&WORKER_TOOLS) {
        let (status, reply) = w.post(tool, &token, json!({})).await;
        assert_eq!(
            status != 403,
            names.contains(tool),
            "GET /tools lists exactly what the worker may call: {tool} {status} {reply}"
        );
    }
}

#[tokio::test]
async fn a_job_starts_at_once_shows_its_output_as_it_comes_and_ends_once() {
    if !sandbox() {
        return;
    }
    let w = World::new("start");
    let (_seat, a) = w.worker("tk_a", "as_a");

    let asked = Instant::now();
    let job = w.start(&a, "echo one; sleep 2; echo two").await;
    assert!(asked.elapsed() < Duration::from_secs(1), "job_start waited");
    assert!(job.starts_with("jb_"), "{job}");
    let mut printed = String::new();
    for _ in 0..100 {
        let (status, look) = w.wait(&a, &job, 0).await;
        assert_eq!(
            (status, look["status"].as_str()),
            (200, Some("running")),
            "{look}"
        );
        printed.push_str(look["stdout"].as_str().unwrap());
        if printed.contains("one") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(printed, "one\n", "the first line came while it ran");

    let (status, end) = w.wait(&a, &job, 10).await;
    assert_eq!(status, 200, "{end}");
    assert_eq!(
        (
            &end["job"],
            &end["status"],
            &end["exit_code"],
            &end["timed_out"]
        ),
        (&json!(job), &json!("exited"), &json!(0), &json!(false)),
        "{end}"
    );
    printed.push_str(end["stdout"].as_str().unwrap());
    assert_eq!(printed, "one\ntwo\n", "each line once");
    let (status, again) = w.wait(&a, &job, 0).await;
    assert_eq!(status, 404, "its end was reported: {again}");
    let (status, refused) = w
        .post("job_start", &a, json!({"code": "true", "timeout_s": 301}))
        .await;
    assert_eq!(status, 422, "{refused}");
    let (status, refused) = w
        .post("job_wait", &a, json!({"job": job, "wait_s": 121}))
        .await;
    assert_eq!(status, 422, "{refused}");
}

#[tokio::test]
async fn a_stopped_job_returns_what_it_printed_and_leaves_no_process_behind() {
    if !sandbox() {
        return;
    }
    let w = World::new("stop");
    let (_seat, a) = w.worker("tk_a", "as_a");
    let marker = new_marker();
    let job = w.start(&a, &sleepers(&marker)).await;
    until_running(&marker).await;

    let (status, stopped) = w.post("job_stop", &a, json!({"job": job})).await;

    assert_eq!(status, 200, "{stopped}");
    assert_eq!(
        (
            &stopped["status"],
            &stopped["exit_code"],
            &stopped["stdout"]
        ),
        (&json!("stopped"), &json!(-1), &json!("up\n")),
        "{stopped}"
    );
    until_gone(&marker).await;
    let (status, _) = w.post("job_stop", &a, json!({"job": job})).await;
    assert_eq!(status, 404);
}

/// bwrap's namespace outlives a plain kill in its first milliseconds.
#[tokio::test]
async fn a_job_stopped_as_its_sandbox_starts_leaves_no_process_behind() {
    if !sandbox() {
        return;
    }
    let w = World::new("stop-early");
    let (_seat, a) = w.worker("tk_a", "as_a");
    let marker = new_marker();
    for delay_ms in [0, 1, 2, 3, 4, 5, 6, 8, 10, 15] {
        let job = w.start(&a, &sleepers(&marker)).await;
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let (status, stopped) = w.post("job_stop", &a, json!({"job": job})).await;
        assert_eq!(
            (status, &stopped["status"]),
            (200, &json!("stopped")),
            "{stopped}"
        );
    }
    until_gone(&marker).await;
}

#[tokio::test]
async fn a_job_past_its_timeout_is_killed() {
    if !sandbox() {
        return;
    }
    let w = World::new("timeout");
    let (_seat, a) = w.worker("tk_a", "as_a");
    let marker = new_marker();
    let code = sleepers(&marker);
    let (status, reply) = w
        .post("job_start", &a, json!({"code": code, "timeout_s": 1}))
        .await;
    assert_eq!(status, 200, "{reply}");
    let job = reply["job"].as_str().unwrap();

    let (status, end) = w.wait(&a, job, 15).await;

    assert_eq!(status, 200, "{end}");
    assert_eq!(
        (&end["status"], &end["timed_out"], &end["exit_code"]),
        (&json!("timed_out"), &json!(true), &json!(-1)),
        "{end}"
    );
    until_gone(&marker).await;
}

#[tokio::test]
async fn a_ticket_holds_at_most_its_jobs_and_a_run_code_past_them_stays_in_the_foreground() {
    if !sandbox() {
        return;
    }
    let w = World::new("limit");
    let (seat, a) = w.worker("tk_a", "as_a");
    let (other, b) = w.worker("tk_b", "as_b");
    let marker = new_marker();
    let first = w.start(&a, &sleepers(&marker)).await;
    w.start(&a, &sleepers(&marker)).await;

    let (status, refused) = w.post("job_start", &a, json!({"code": "true"})).await;
    assert_eq!(status, 409, "{refused}");
    assert!(
        refused["error"].as_str().unwrap().contains("2 jobs"),
        "{refused}"
    );
    let asked = Instant::now();
    let (status, ran) = w
        .post("run_code", &a, json!({"code": "sleep 2; echo fore"}))
        .await;
    assert_eq!(status, 200, "{ran}");
    assert!(ran.get("job").is_none(), "no room for a job: {ran}");
    assert_eq!(ran["stdout"], "fore\n");
    assert!(asked.elapsed() >= Duration::from_secs(2));
    w.start(&b, "sleep 300").await;

    w.post("job_stop", &a, json!({"job": first})).await;
    w.start(&a, "true").await;
    seat.stop_jobs().await;
    other.stop_jobs().await;
    until_gone(&marker).await;
}

#[tokio::test]
async fn jobs_die_with_their_assignment_and_with_a_revoked_worker() {
    if !sandbox() {
        return;
    }
    let w = World::new("assignment");
    let (seat, a) = w.worker("tk_a", "as_a");
    let marker = new_marker();
    let job = w.start(&a, &sleepers(&marker)).await;
    let (status, moved) = w
        .post("run_code", &a, json!({"code": sleepers(&marker)}))
        .await;
    assert_eq!(
        (status, &moved["status"]),
        (200, &json!("running")),
        "{moved}"
    );
    until_running(&marker).await;

    seat.stop_jobs().await;

    until_gone(&marker).await;
    assert!(!seat.jobs_running());
    assert_eq!(w.wait(&a, &job, 0).await.0, 404);
    let (status, refused) = w.post("job_start", &a, json!({"code": "true"})).await;
    assert_eq!(status, 409, "{refused}");
    assert!(
        seat.end_turn().is_empty(),
        "a stopped job is never reported"
    );

    let (_seat, c) = w.worker("tk_c", "as_c");
    let revoked = new_marker();
    w.start(&c, &sleepers(&revoked)).await;
    until_running(&revoked).await;
    w.broker.revoke(&c);
    until_gone(&revoked).await;
}

#[tokio::test]
async fn only_its_own_worker_sees_or_touches_a_job() {
    if !sandbox() {
        return;
    }
    let w = World::new("owner");
    let (seat, a) = w.worker("tk_a", "as_a");
    let (_other, b) = w.worker("tk_b", "as_b");
    let marker = new_marker();
    let job = w.start(&a, &sleepers(&marker)).await;

    for tool in ["job_start", "job_wait", "job_stop"] {
        let body = json!({"job": job, "code": "true"});
        let (status, _) = w.post(tool, ROUTER, body).await;
        assert_eq!(status, 403, "the Router may not call {tool}");
    }
    for tool in ["job_wait", "job_stop"] {
        let (status, refused) = w.post(tool, &b, json!({"job": job})).await;
        assert_eq!(status, 404, "another ticket's worker: {tool} {refused}");
    }
    let (status, look) = w.wait(&a, &job, 0).await;
    assert_eq!(
        (status, &look["status"]),
        (200, &json!("running")),
        "{look}"
    );
    let (status, stopped) = w.post("job_stop", &a, json!({"job": job})).await;
    assert_eq!((status, &stopped["status"]), (200, &json!("stopped")));
    until_gone(&marker).await;
    seat.stop_jobs().await;
}

#[tokio::test]
async fn a_run_code_still_running_after_the_threshold_moves_to_the_background() {
    if !sandbox() {
        return;
    }
    let w = World::new("background");
    let (_seat, a) = w.worker("tk_a", "as_a");

    let (status, short) = w.post("run_code", &a, json!({"code": "echo ok"})).await;
    assert_eq!(status, 200, "{short}");
    let keys: BTreeSet<&str> = short
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "duration_ms",
            "exit_code",
            "queue_wait_ms",
            "stderr",
            "stdout",
            "timed_out",
            "truncated"
        ]),
        "run_code's receipt as before: {short}"
    );
    assert_eq!(
        (&short["exit_code"], &short["stdout"]),
        (&json!(0), &json!("ok\n"))
    );

    let asked = Instant::now();
    let (status, moved) = w
        .post("run_code", &a, json!({"code": "sleep 3; echo late"}))
        .await;
    let took = asked.elapsed();
    assert_eq!(status, 200, "{moved}");
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(3),
        "{took:?}"
    );
    let keys: BTreeSet<&str> = moved
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, BTreeSet::from(["job", "note", "status"]), "{moved}");
    assert_eq!(moved["status"], "running");
    assert!(
        moved["note"].as_str().unwrap().contains("job_wait"),
        "{moved}"
    );
    let (status, end) = w.wait(&a, moved["job"].as_str().unwrap(), 10).await;
    assert_eq!(status, 200, "{end}");
    assert_eq!(
        (&end["status"], &end["exit_code"], &end["stdout"]),
        (&json!("exited"), &json!(0), &json!("late\n"))
    );
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

/// The Note texts on `ticket`, oldest first.
fn notes(store: &SqliteStore, ticket: &TicketId) -> Vec<String> {
    let Ok(Some(found)) = store.read_ticket(ticket) else {
        return Vec::new();
    };
    found
        .inputs
        .iter()
        .filter_map(|input| match &input.body {
            InputBody::Note { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

async fn until_notes(w: &World, ticket: &TicketId, count: usize) -> Vec<String> {
    for _ in 0..1_000 {
        let found = notes(&w.store(), ticket);
        if found.len() >= count {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{count} notes never reached {ticket}")
}

#[tokio::test]
async fn a_jobs_end_reaches_the_worker_once_in_its_turn_or_as_one_input_after_it() {
    if !sandbox() {
        return;
    }
    let mut w = World::new("note");
    w.ask("t-ask", "Build the release notes");
    let runtime = w.runtime.take().unwrap();
    let facts = runtime.facts();
    with_runtime(runtime, async {
        let (ticket, assignment) = 'running: {
            for _ in 0..1_000 {
                let found = w.store().tickets().unwrap().into_iter().next();
                if let Some(t) = found
                    && let Some(running) = t.running_assignment()
                {
                    break 'running (t.id.clone(), running.id.clone());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the assignment never ran")
        };
        let (seat, a) = w.worker(ticket.as_str(), assignment.as_str());

        let taken = w.start(&a, "sleep 1; echo taken").await;
        let (_, end) = w.wait(&a, &taken, 10).await;
        assert_eq!(end["stdout"], "taken\n", "{end}");
        let later = w.start(&a, "echo later; sleep 1").await;
        let quick = w.start(&a, "echo quick").await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let woken = seat.end_turn();
        let [Fact::Note { ticket: on, text }] = &woken[..] else {
            panic!("only the job that ended in the turn: {woken:?}")
        };
        assert_eq!(on, &ticket);
        assert!(text.contains(&quick) && text.contains("quick"), "{text}");
        assert!(seat.end_turn().is_empty(), "the end was taken");
        for fact in woken {
            facts.send(fact).await.unwrap();
        }

        let told = until_notes(&w, &ticket, 2).await;
        assert!(told[0].contains(&quick), "{told:?}");
        assert!(
            told[1].starts_with(&format!(
                "Background job {later} (`echo later; sleep 1`) exited with code 0"
            )),
            "{told:?}"
        );
        assert!(told[1].contains("stdout:\nlater\n"), "{told:?}");
        for job in [&taken, &later, &quick] {
            assert_eq!(w.wait(&a, job, 0).await.0, 404, "{job} was reported");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let all = notes(&w.store(), &ticket);
        assert_eq!(
            all.len(),
            2,
            "each end once, and none for the job taken in its turn: {all:?}"
        );
        assert!(!all.iter().any(|note| note.contains(&taken)));
    })
    .await;
}

#[tokio::test]
async fn job_wait_returns_early_once_for_each_batch_of_new_input() {
    if !sandbox() {
        return;
    }
    let w = World::new("input");
    let (seat, a) = w.worker("tk_a", "as_a");
    let marker = new_marker();
    let job = w.start(&a, &sleepers(&marker)).await;

    let asked = Instant::now();
    let (look, ()) = tokio::join!(w.wait(&a, &job, 30), async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        seat.input_waiting();
    });
    let (status, look) = look;
    assert_eq!(status, 200, "{look}");
    assert!(
        asked.elapsed() < Duration::from_secs(5),
        "the wait was cut short"
    );
    assert_eq!(
        (&look["status"], &look["input_waiting"]),
        (&json!("running"), &json!(true))
    );
    assert!(
        look["next"].as_str().unwrap().contains("End your turn"),
        "{look}"
    );

    let asked = Instant::now();
    let (_, look) = w.wait(&a, &job, 1).await;
    assert!(
        asked.elapsed() >= Duration::from_secs(1),
        "told once: {look}"
    );
    assert!(look.get("input_waiting").is_none(), "{look}");
    seat.stop_jobs().await;
    until_gone(&marker).await;
}
