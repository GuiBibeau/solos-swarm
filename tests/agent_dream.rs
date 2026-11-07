//! The daily dream end to end, through `agent serve` with Pi workers whose
//! model is scripted. Two Tickets end; their workers' transcripts (fixtures)
//! show that `cargo test` fails and `./check` passes, while an active memo
//! says the tests run with `cargo test`. The dream, asked for by the host at
//! its time of day and judged by the host in the Router's place, finds the
//! day's record staged read-only in its `/work`, proposes a correction that
//! supersedes the memo, citing lines of the original transcripts, and
//! retires a stale memo; the review keeps the correction, so the old memo
//! leaves the index. A citation to a line that does not hold its quote, or
//! to a staged copy the dream edited, is refused. The dream's calls, and
//! only the dream's, go with thinking on, pass every earlier reasoning back,
//! and are charged to the `memory` account. The owner gets one digest,
//! once, across a restart; a day with no ended Ticket asks for no dream.

use serde_json::{Value, json};
use solos_agent_core::memo::{
    DREAM, Memo, MemoActor, MemoAudience, MemoScope, MemoState, memo_index,
};
use solos_agent_core::model::{OutboundMessage, Ticket, TicketId};
use solos_agent_core::trail::Digest;
use solos_agent_store::SqliteStore;
use solos_swarm::agent::broker::review::INSTRUCTIONS;
use solos_swarm::agent::pi_executor::WORK_DIR;
use solos_swarm::agent::{AgentConfig, PiHost, STORE_FILE, TICKETS_DIR, serve_with};
use solos_swarm::gateway::{Dispatch, Provider, Temperature, sampled, upstream_body};
use solos_swarm::harness::scripted::{Call, ScriptedHarness};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(120);
const M_TEXT: &str = "Tests run with `cargo test`.";
const N_TEXT: &str = "The build needs cargo.";
const P_TEXT: &str = "Tests run with ./check; `cargo test` fails here, as there is no Cargo.toml.";

fn text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        parts => parts
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|part| part["text"].as_str())
            .collect(),
    }
}

fn call(id: &str, name: &str, arguments: &Value) -> Value {
    json!({"id": id, "type": "function",
           "function": {"name": name, "arguments": arguments.to_string()}})
}

/// The fixture transcript of a worker that ran `cargo test`, which failed,
/// then `./check`, which passed, as Pi's RPC frames.
fn transcript(task: &str) -> String {
    [
        json!({"type":"message_end","message":{"role":"user","content":[{"type":"text","text":format!("Ticket: {task}")}]}}),
        json!({"type":"message_end","message":{"role":"assistant","content":[
            {"type":"toolCall","id":"c1","name":"run_code","arguments":{"code":"cargo test"}}]}}),
        json!({"type":"tool_execution_end","toolCallId":"c1","toolName":"run_code","args":{"code":"cargo test"},
               "result":{"details":{"exit_code":101,"stdout":"","stderr":"error: could not find `Cargo.toml` in `/work` or any parent directory"}}}),
        json!({"type":"tool_execution_end","toolCallId":"c2","toolName":"run_code","args":{"code":"./check"},
               "result":{"details":{"exit_code":0,"stdout":"all checks passed","stderr":""}}}),
    ]
    .iter()
    .map(|record| format!("{record}\n"))
    .collect()
}

/// The scripted model: ordinary workers (thinking off), the review (thinking
/// on, no tools) and the dream (thinking on, with tools).
struct Model {
    state: PathBuf,
    bodies: Mutex<Vec<Value>>,
}

impl Model {
    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
    fn dream_bodies(&self) -> Vec<Value> {
        self.bodies()
            .into_iter()
            .filter(|b| b["thinking"]["type"] == "enabled" && b.get("tools").is_some())
            .collect()
    }
    fn review_bodies(&self) -> Vec<Value> {
        self.bodies()
            .into_iter()
            .filter(|b| b["thinking"]["type"] == "enabled" && b.get("tools").is_none())
            .collect()
    }
    fn worker_bodies(&self) -> Vec<Value> {
        self.bodies()
            .into_iter()
            .filter(|b| b["thinking"]["type"] == "disabled")
            .collect()
    }

    /// An ordinary worker: ticket A remembers what its asker said and is
    /// done, ticket B remembers that the build needs cargo and is done.
    fn worker(&self, messages: &[Value]) -> Vec<Value> {
        if messages.last().is_some_and(|m| m["role"] == "tool") {
            return Vec::new();
        }
        let prompt = text(&messages[0]);
        let (trigger, quote, memo) = if prompt.contains("the tests here run with cargo test") {
            ("t-a", "the tests here run with cargo test", M_TEXT)
        } else {
            ("t-b", "build it with cargo", N_TEXT)
        };
        vec![
            call(
                "c-memo",
                "memo_propose",
                &json!({"text": memo, "scope": "global",
                        "evidence": [{"trigger": trigger, "quote": quote}]}),
            ),
            call(
                "c-ready",
                "worker_report",
                &json!({"status": "ready", "text": "Done."}),
            ),
        ]
    }

    /// The dream, one step per model call: read what was staged; then
    /// correct M, retire N and try three citations the host must refuse;
    /// then report.
    fn dream(&self, messages: &[Value]) -> (Vec<Value>, String) {
        let step = messages.iter().filter(|m| m["role"] == "assistant").count();
        let reasoning = format!("R{}: thinking about step {step}.", step + 1);
        let prompt = text(&messages[0]);
        let dream = prompt
            .strip_prefix("Dream ")
            .and_then(|rest| rest.split(':').next())
            .expect("the dream's first prompt names its ticket")
            .to_owned();
        let calls = match step {
            0 => vec![
                call(
                    "c-index",
                    "read",
                    &json!({"path": "/work/tickets/index.json"}),
                ),
                call(
                    "c-memos",
                    "read",
                    &json!({"path": "/work/memos/index.json"}),
                ),
            ],
            1 => {
                let store = SqliteStore::open(self.state.join(STORE_FILE)).unwrap();
                let id_of = |text: &str| {
                    store
                        .memos()
                        .unwrap()
                        .into_iter()
                        .find(|m| m.text == text && m.is_active())
                        .map(|m| m.id.to_string())
                        .unwrap_or_default()
                };
                let (m, n) = (id_of(M_TEXT), id_of(N_TEXT));
                let tickets = worked(&store);
                let (a, b) = (&tickets[0], &tickets[1]);
                let (a_as, a_sha) = original(&self.state, a);
                let (b_as, b_sha) = original(&self.state, b);
                // Edit the staged copy of B's transcript, as a dream could.
                let copy = self
                    .state
                    .join(TICKETS_DIR)
                    .join(&dream)
                    .join(WORK_DIR)
                    .join("transcripts")
                    .join(b.id.as_str())
                    .join(format!("{b_as}.rpc.jsonl"));
                let mode = std::fs::metadata(&copy).unwrap().permissions().mode();
                assert_eq!(mode & 0o222, 0, "the staged copy is read-only");
                std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o644)).unwrap();
                let mut edited = std::fs::read_to_string(&copy).unwrap();
                edited.push_str(
                    &json!({"type":"tool_execution_end","toolCallId":"c9","toolName":"run_code",
                            "result":{"details":{"exit_code":0,"stdout":"tests run with make"}}})
                    .to_string(),
                );
                edited.push('\n');
                std::fs::write(&copy, &edited).unwrap();
                let edited_sha = Digest::of(edited.as_bytes()).to_string();
                vec![
                    call(
                        "c-correct",
                        "memo_propose",
                        &json!({"text": P_TEXT, "scope": "global", "supersedes": m, "evidence": [
                            {"transcript": a_as, "sha256": a_sha, "line": 3, "quote": "could not find `Cargo.toml`"},
                            {"transcript": b_as, "sha256": b_sha, "line": 4, "quote": "all checks passed"},
                        ]}),
                    ),
                    call(
                        "c-wrong-line",
                        "memo_propose",
                        &json!({"text": "The checks pass.", "scope": "global", "evidence": [
                            {"transcript": a_as, "sha256": a_sha, "line": 2, "quote": "all checks passed"},
                        ]}),
                    ),
                    call(
                        "c-edited-copy",
                        "memo_propose",
                        &json!({"text": "Tests run with make.", "scope": "global", "evidence": [
                            {"transcript": b_as, "sha256": edited_sha, "line": 5, "quote": "tests run with make"},
                        ]}),
                    ),
                    call(
                        "c-edited-line",
                        "memo_propose",
                        &json!({"text": "Tests run with make.", "scope": "global", "evidence": [
                            {"transcript": b_as, "sha256": b_sha, "line": 5, "quote": "tests run with make"},
                        ]}),
                    ),
                    call(
                        "c-retire",
                        "memo_retire",
                        &json!({"memo": n, "reason": "The repository has no Cargo.toml.", "evidence": [
                            {"transcript": b_as, "sha256": b_sha, "line": 3, "quote": "could not find `Cargo.toml`"},
                        ]}),
                    ),
                ]
            }
            2 => vec![call(
                "c-done",
                "worker_report",
                &json!({"status": "ready", "text": "Corrected the test command and retired a stale memo."}),
            )],
            _ => Vec::new(),
        };
        (calls, reasoning)
    }
}

/// The two worked Tickets, A then B.
fn worked(store: &SqliteStore) -> Vec<Ticket> {
    let mut tickets: Vec<Ticket> = store
        .tickets()
        .unwrap()
        .into_iter()
        .filter(|t| !t.is_dream())
        .collect();
    tickets.sort_by_key(|t| t.origin.clone());
    tickets
}

/// The Ticket's one Assignment and the sha256 of its original transcript.
fn original(state: &Path, ticket: &Ticket) -> (String, String) {
    let assignment = ticket.assignments[0].id.to_string();
    let path = state
        .join(TICKETS_DIR)
        .join(ticket.id.as_str())
        .join(format!("{assignment}.rpc.jsonl"));
    let bytes = std::fs::read(path).unwrap();
    (assignment, Digest::of(&bytes).to_string())
}

impl Provider for Model {
    fn dispatch(&self, body: Value) -> Dispatch {
        self.bodies.lock().unwrap().push(body.clone());
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let thinking = body["thinking"]["type"] == "enabled";
        let (calls, reasoning, content) = if thinking && body.get("tools").is_none() {
            let keep = json!({"decision": "keep", "reason": "Its evidence is the output of commands that show it."});
            (
                Vec::new(),
                Some("Weighing the evidence.".to_owned()),
                keep.to_string(),
            )
        } else if thinking {
            let (calls, reasoning) = self.dream(&messages);
            (calls, Some(reasoning), "Done.".to_owned())
        } else {
            (self.worker(&messages), None, "Done.".to_owned())
        };
        let mut message = if calls.is_empty() {
            json!({"role": "assistant", "content": content})
        } else {
            json!({"role": "assistant", "content": null, "tool_calls": calls})
        };
        if let Some(reasoning) = reasoning {
            message["reasoning_content"] = json!(reasoning);
        }
        let finish = if calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        let n = self.bodies.lock().unwrap().len();
        Box::pin(async move {
            Ok(
                json!({"id": format!("model-{n}"), "created": 1, "model": "deepseek-flash",
                      "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                      "usage": {"prompt_tokens": 100, "prompt_cache_hit_tokens": 0, "completion_tokens": 50}}),
            )
        })
    }
}

/// A test's directory, removed at its end.
struct Scratch(View);

/// What a test reads and writes in its directory.
#[derive(Clone)]
struct View {
    root: PathBuf,
}

impl std::ops::Deref for Scratch {
    type Target = View;
    fn deref(&self) -> &View {
        &self.0
    }
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agent-dream-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("node"), "").unwrap();
        std::fs::write(root.join("triggers.jsonl"), "").unwrap();
        Self(View { root })
    }
}

impl View {
    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    fn store(&self) -> SqliteStore {
        SqliteStore::open(self.state().join(STORE_FILE)).unwrap()
    }
    fn ask(&self, id: &str, thread: &str, text: &str) {
        let line = json!({"id":id,"source":"slack","at_ms":1_793_613_600_000u64,
            "actor":{"id":"alice","kind":"person"},"addressed":"mention","text":text,
            "threads":[thread],"reply_to":[{"source":"slack","address":{"thread_ts":thread}}]});
        let mut lines = std::fs::read_to_string(self.root.join("triggers.jsonl")).unwrap();
        lines.push_str(&format!("{line}\n"));
        std::fs::write(self.root.join("triggers.jsonl"), lines).unwrap();
    }
    /// What the operator was told; a line being written may not be whole.
    fn told(&self) -> Vec<OutboundMessage> {
        std::fs::read_to_string(self.root.join("ops.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
    fn digests(&self) -> Vec<OutboundMessage> {
        self.told()
            .into_iter()
            .filter(|m| m.text.starts_with("Memory, "))
            .collect()
    }
    /// The agent's config; `memory` is its `memory:` section.
    fn config(&self, memory: &str) -> AgentConfig {
        let yaml = format!(
            "policy:\n  people: [slack:alice]\n\
             sources:\n  - {{kind: jsonl, path: triggers.jsonl}}\n\
             channels:\n  - {{kind: jsonl, source: slack, path: outbox.jsonl}}\n\
             \x20 - {{kind: jsonl, source: ops, path: ops.jsonl}}\n\
             router: {{kind: scripted}}\n\
             executor:\n  kind: pi\n  provider: deepseek\n  model: deepseek-flash\n\
             \x20 prices: {{input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}}\n\
             \x20 max_output_tokens: 2048\n\
             budgets: {{session_microusd: 500000}}\n\
             watchdog: {{operator: {{source: ops, address: {{channel: D-OPS}}}}, every: 1s}}\n\
             web: {{public_url: \"https://agent.example.com\"}}\n\
             memory: {memory}\n"
        );
        let path = self.root.join("agent.yaml");
        std::fs::write(&path, yaml).unwrap();
        AgentConfig::load(&path).unwrap()
    }
    fn host(&self, model: Arc<Model>) -> PiHost<ScriptedHarness> {
        PiHost {
            harness: ScriptedHarness::new(BTreeMap::new())
                .every_turn(vec![Call::Agent])
                .ending_the_turn_on_a_model_error(),
            provider: model,
            pi_root: self.root.clone(),
            node: self.root.join("node"),
            browser: solos_swarm::browse::Launcher::Lightpanda,
        }
    }
    /// Serve, following the Sources, until `done` holds of the Store and
    /// this scratch, then for `settle` more, and drop it all as a crash
    /// would.
    fn serve_until(
        &self,
        config: AgentConfig,
        model: Arc<Model>,
        settle: Duration,
        done: impl Fn(&View) -> bool + Send + 'static,
    ) {
        let (state, host) = (self.state(), self.host(model));
        let me = self.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let until = async {
                    let started = Instant::now();
                    while !done(&me) {
                        assert!(started.elapsed() < LIMIT, "serve never got there");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    tokio::time::sleep(settle).await;
                };
                tokio::select! {
                    ended = serve_with(config, &state, false, Some(host)) => {
                        panic!("serve ended: {ended:?}")
                    }
                    () = until => {}
                }
            });
            runtime.shutdown_timeout(Duration::from_secs(5));
        })
        .join()
        .unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("SOLOS_KEEP_SCRATCH").is_none() {
            let _ = std::fs::remove_dir_all(&self.0.root);
        }
    }
}

fn memo_with(store: &SqliteStore, text: &str) -> Memo {
    store
        .memos()
        .unwrap()
        .into_iter()
        .find(|m| m.text == text)
        .unwrap_or_else(|| panic!("no memo says {text:?}"))
}

fn dreams(store: &SqliteStore) -> Vec<Ticket> {
    store.dream_tickets().unwrap()
}

fn dream_triggers(store: &SqliteStore) -> usize {
    store
        .triggers()
        .unwrap()
        .iter()
        .filter(|t| t.source == DREAM)
        .count()
}

#[test]
fn the_dream_corrects_a_memo_the_transcripts_contradict_and_tells_the_owner_once() {
    let scratch = Scratch::new("corrects");
    let model = Arc::new(Model {
        state: scratch.state(),
        bodies: Mutex::default(),
    });

    // The day's work, with no dream: two Tickets, each leaving a memo the
    // review keeps.
    scratch.ask(
        "t-a",
        "T1",
        "@agent the tests here run with cargo test, remember it",
    );
    scratch.ask("t-b", "T2", "@agent build it with cargo please");
    scratch.serve_until(
        scratch.config("{dream: {enabled: false}}"),
        model.clone(),
        Duration::from_millis(200),
        |s| {
            let store = s.store();
            let ended = worked(&store).iter().filter(|t| t.ended.is_some()).count();
            let active = store
                .memos()
                .unwrap()
                .iter()
                .filter(|m| m.is_active())
                .count();
            ended == 2 && active == 2
        },
    );
    let store = scratch.store();
    assert!(dreams(&store).is_empty(), "no dream while it is off");
    assert!(!scratch.state().join("dream-slot").exists());
    let m = memo_with(&store, M_TEXT);
    let n = memo_with(&store, N_TEXT);
    assert!(m.is_active() && n.is_active());
    // What the workers' sessions wrote: the fixtures.
    let tickets = worked(&store);
    for ticket in &tickets {
        let path = scratch
            .state()
            .join(TICKETS_DIR)
            .join(ticket.id.as_str())
            .join(format!("{}.rpc.jsonl", ticket.assignments[0].id));
        assert!(!path.exists());
        std::fs::write(&path, transcript(&ticket.title)).unwrap();
    }
    let originals: Vec<(String, String)> = tickets
        .iter()
        .map(|t| original(&scratch.state(), t))
        .collect();
    let workers_before = model.worker_bodies().len();

    // The dream: asked for at its time, staged, run, reviewed and told.
    let config = scratch.config("{dream: {at: \"00:00\"}}");
    scratch.serve_until(
        config.clone(),
        model.clone(),
        Duration::from_millis(1_500),
        |s| !s.digests().is_empty(),
    );
    let store = scratch.store();
    let [dream] = &dreams(&store)[..] else {
        panic!("one dream: {:?}", dreams(&store))
    };
    assert!(dream.ended.is_some());
    assert_eq!(dream.mandate.requester.id, DREAM);
    assert_eq!(dream_triggers(&store), 1);

    // Staged read-only: the digests, the transcripts as copies with their
    // originals' digests, and the memos.
    let work = scratch
        .state()
        .join(TICKETS_DIR)
        .join(dream.id.as_str())
        .join(WORK_DIR);
    let index: Value =
        serde_json::from_slice(&std::fs::read(work.join("tickets").join("index.json")).unwrap())
            .unwrap();
    let staged: Vec<(String, String)> = index["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["assignments"].as_array().unwrap().clone())
        .map(|a| {
            (
                a["assignment"].as_str().unwrap().to_owned(),
                a["sha256"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    for original in &originals {
        assert!(staged.contains(original), "{staged:?} lacks {original:?}");
    }
    let memos = std::fs::read_to_string(work.join("memos").join("index.json")).unwrap();
    assert!(memos.contains(M_TEXT) && memos.contains(m.id.as_str()));
    let digest: Value = serde_json::from_slice(
        &std::fs::read(work.join("tickets").join(format!("{}.json", tickets[0].id))).unwrap(),
    )
    .unwrap();
    assert!(
        digest["request"]["text"]
            .as_str()
            .unwrap()
            .contains("cargo test")
    );
    assert_eq!(digest["replies"][0]["status"], "ready");

    // The correction is kept, cites the original lines, and supersedes M.
    let p = memo_with(&store, P_TEXT);
    assert!(p.is_active(), "{:?}", p.state);
    assert_eq!(p.supersedes.as_ref(), Some(&m.id));
    assert_eq!(p.sources.len(), 2);
    let m = store.read_memo(&m.id).unwrap().unwrap();
    match &m.state {
        MemoState::Retired {
            replaced_by, by, ..
        } => {
            assert_eq!(replaced_by.as_ref(), Some(&p.id));
            assert!(matches!(by, MemoActor::Review { .. }), "{by:?}");
        }
        other => panic!("M is {other:?}"),
    }
    let n = store.read_memo(&n.id).unwrap().unwrap();
    match &n.state {
        MemoState::Retired { by, evidence, .. } => {
            assert!(matches!(by, MemoActor::Dream { ticket, .. } if *ticket == dream.id));
            assert_eq!(evidence.len(), 1);
        }
        other => panic!("N is {other:?}"),
    }
    let index = memo_index(
        &store.memos().unwrap(),
        &[MemoScope::Global],
        3_072,
        &MemoAudience::Router,
    );
    assert!(
        index.text.contains("Tests run with ./check"),
        "{}",
        index.text
    );
    assert!(!index.text.contains(M_TEXT) && !index.text.contains(N_TEXT));
    // Nothing the host refused was kept.
    assert!(
        !store
            .memos()
            .unwrap()
            .iter()
            .any(|memo| memo.text.contains("make") || memo.text == "The checks pass.")
    );

    // What the dream was told of each call: the refusals.
    let bodies = model.dream_bodies();
    assert!(bodies.len() >= 3, "{}", bodies.len());
    let results: BTreeMap<String, String> = bodies
        .iter()
        .flat_map(|b| b["messages"].as_array().unwrap().clone())
        .filter(|m| m["role"] == "tool")
        .map(|m| (m["tool_call_id"].as_str().unwrap().to_owned(), text(&m)))
        .collect();
    assert!(
        results["c-index"].contains("sha256:"),
        "{}",
        results["c-index"]
    );
    assert!(
        results["c-correct"].contains("proposed"),
        "{}",
        results["c-correct"]
    );
    assert!(
        results["c-wrong-line"].contains("does not say"),
        "{}",
        results["c-wrong-line"]
    );
    assert!(
        results["c-edited-copy"].contains("is not the sha256"),
        "{}",
        results["c-edited-copy"]
    );
    assert!(
        results["c-edited-line"].contains("there is no line 5"),
        "{}",
        results["c-edited-line"]
    );
    assert!(
        results["c-retire"].contains("retired"),
        "{}",
        results["c-retire"]
    );

    // Thinking on, at max, for the dream's calls only; each carries every
    // earlier reasoning back.
    for (k, body) in bodies.iter().enumerate() {
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert_eq!(body["reasoning_effort"], "max");
        assert_eq!(body["max_tokens"], 131_072);
        assert!(body.get("temperature").is_none());
        let reasoned: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "assistant")
            .map(|m| m["reasoning_content"].as_str().unwrap())
            .collect();
        let expected: Vec<String> = (0..k)
            .map(|step| format!("R{}: thinking about step {step}.", step + 1))
            .collect();
        assert_eq!(reasoned, expected, "call {k}");
    }
    let workers = model.worker_bodies();
    assert_eq!(
        workers.len(),
        workers_before,
        "the dream run asked no worker"
    );
    for body in &workers {
        let request = json!({"model": body["model"], "messages": body["messages"]});
        let shared = sampled(
            upstream_body("deepseek-flash", &request, 2_048),
            Temperature::WORKER,
        );
        assert_eq!(
            serde_json::to_vec(body).unwrap(),
            serde_json::to_vec(&shared).unwrap(),
            "an ordinary worker's body is the shared one, byte for byte"
        );
    }
    let reviews = model.review_bodies();
    let correction = reviews
        .iter()
        .find(|b| text(&b["messages"][1]).contains("Tests run with ./check"))
        .expect("the correction was reviewed");
    assert_eq!(text(&correction["messages"][0]), INSTRUCTIONS);
    let case: Value = serde_json::from_str(&text(&correction["messages"][1])).unwrap();
    assert_eq!(case["supersedes"]["id"], json!(m.id));
    assert_eq!(case["evidence"][0]["kind"], "transcript");
    assert!(
        case["evidence"][0]["from"]
            .as_str()
            .unwrap()
            .contains("run_code")
    );

    // Charged to the memory account, not the dream's Ticket.
    assert!(store.committed("memory", 0).unwrap() > 0);
    assert_eq!(
        store.committed(&format!("ticket:{}", dream.id), 0).unwrap(),
        0
    );

    // One digest, to the operator, linking the Tickets it read.
    let [told] = &scratch.digests()[..] else {
        panic!("{:?}", scratch.digests())
    };
    assert_eq!(told.ticket.as_ref(), Some(&dream.id));
    assert!(
        told.text.contains(&format!("Kept {}", p.id)),
        "{}",
        told.text
    );
    assert!(
        told.text.contains(&format!("replaces {}", m.id)),
        "{}",
        told.text
    );
    assert!(
        told.text.contains(&format!("Retired {}", n.id)),
        "{}",
        told.text
    );
    let page = |id: &TicketId| format!("https://agent.example.com/tickets/{id}");
    for ticket in &tickets {
        assert!(told.text.contains(&page(&ticket.id)), "{}", told.text);
    }

    // A restart the same day asks for no dream and tells nothing again.
    let started = Instant::now();
    scratch.serve_until(config, model.clone(), Duration::ZERO, move |_| {
        started.elapsed() > Duration::from_millis(2_500)
    });
    let store = scratch.store();
    assert_eq!(dreams(&store).len(), 1);
    assert_eq!(dream_triggers(&store), 1);
    assert_eq!(scratch.digests().len(), 1);
}

#[test]
fn a_day_with_no_ended_ticket_asks_for_no_dream() {
    let scratch = Scratch::new("quiet");
    let model = Arc::new(Model {
        state: scratch.state(),
        bodies: Mutex::default(),
    });
    let started = Instant::now();
    scratch.serve_until(
        scratch.config("{dream: {at: \"00:00\"}}"),
        model.clone(),
        Duration::ZERO,
        move |_| started.elapsed() > Duration::from_millis(1_500),
    );
    let store = scratch.store();
    assert_eq!(dream_triggers(&store), 0, "nothing asked");
    assert!(dreams(&store).is_empty());
    assert!(store.decisions().unwrap().is_empty());
    assert!(model.bodies().is_empty());
    assert!(
        scratch.state().join("dream-slot").exists(),
        "the day's slot was looked at"
    );
    assert!(scratch.digests().is_empty());
}
