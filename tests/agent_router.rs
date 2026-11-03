//! `agent serve` with a Pi Router whose model is scripted: the scripted
//! provider reads the eval ids off each prompt's cards and answers with
//! `router_decide` calls, which go through the agent broker to the Engine.
//! The first test plays the session with `ScriptedHarness`; the ignored one
//! runs the real `PiHarness` (Pi, node and bwrap) and needs `SOLOS_PI_ROOT`
//! and `SOLOS_NODE`.

use serde_json::{Value, json};
use solos_agent_core::model::{DecisionKind, MessageKind, OutboundMessage, ThreadKey};
use solos_agent_store::{SpendState, SqliteStore};
use solos_swarm::agent::{AgentConfig, PiHost, ROUTER_DIR, STORE_FILE, Summary, serve_with};
use solos_swarm::gateway::{Dispatch, Provider};
use solos_swarm::harness::scripted::{Call, ScriptedHarness};
use solos_swarm::harness::{AgentHarness, PiHarness};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(120);

/// Model calls after which a `Judge` that cuts replies off gives up and
/// answers in plain text, so a Router that never stops asking ends the test
/// instead of hanging it.
const CALL_CAP: usize = 200;

/// The Router's model: every explicit candidate on the latest prompt is
/// accepted, every inferred one ignored. After a tool result it ends the
/// turn, as a model does once it has decided.
#[derive(Default)]
struct Judge {
    bodies: Mutex<Vec<Value>>,
    /// How many tool calls one reply holds before the output limit cuts it
    /// off. A prompt with more cards gets a cut-off reply of that many calls
    /// (at least one).
    fits: Option<usize>,
}

impl Judge {
    fn fitting(fits: usize) -> Self {
        Self {
            fits: Some(fits),
            ..Self::default()
        }
    }
}

/// `(eval, strength, first quoted line)` of each card in `prompt`.
fn cards(prompt: &str) -> Vec<(String, String, String)> {
    let mut found: Vec<(String, String, String)> = Vec::new();
    for line in prompt.lines() {
        if let Some(rest) = line.strip_prefix("eval ") {
            let fields: Vec<&str> = rest.split(" | ").collect();
            found.push((fields[0].into(), fields[2].into(), String::new()));
        } else if let (Some(text), Some(card)) = (line.strip_prefix("> "), found.last_mut())
            && card.2.is_empty()
        {
            card.2 = text.chars().take(60).collect();
        }
    }
    found
}

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

impl Provider for Judge {
    fn dispatch(&self, body: Value) -> Dispatch {
        self.bodies.lock().unwrap().push(body.clone());
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let last = messages.last().cloned().unwrap_or(Value::Null);
        let n = self.bodies.lock().unwrap().len();
        let asked = cards(&text(&last));
        let given_up = self.fits.is_some() && n > CALL_CAP;
        let (kept, cut) = match self.fits {
            Some(fits) if asked.len() > fits => (fits.max(1), true),
            _ => (asked.len(), false),
        };
        let message = if last["role"] == "user" && !given_up {
            let calls: Vec<Value> = asked
                .into_iter()
                .take(kept)
                .map(|(eval, strength, title)| {
                    let arguments = if strength == "explicit" {
                        json!({"eval":eval,"decision":"accept","reason":"a person asked for it",
                               "title":title,"scope":format!("Do what was asked: {title}")})
                    } else {
                        json!({"eval":eval,"decision":"ignore","reason":"nobody asked for work"})
                    };
                    json!({"id":format!("call-{eval}"),"type":"function",
                           "function":{"name":"router_decide","arguments":arguments.to_string()}})
                })
                .collect();
            json!({"role":"assistant","content":null,"tool_calls":calls})
        } else {
            json!({"role":"assistant","content":"Decided."})
        };
        let finish = match (message["tool_calls"].is_array(), cut) {
            (true, true) => "length",
            (true, false) => "tool_calls",
            (false, _) => "stop",
        };
        Box::pin(async move {
            Ok(
                json!({"id":format!("judge-{n}"),"created":1,"model":"deepseek-flash",
                      "choices":[{"index":0,"message":message,"finish_reason":finish}],
                      "usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":0,"completion_tokens":50}}),
            )
        })
    }
}

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::var_os("SOLOS_SCRATCH")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let root = base.join(format!(
            "agent-router-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        Self { root }
    }
    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    fn store(&self) -> SqliteStore {
        SqliteStore::open(self.state().join(STORE_FILE)).unwrap()
    }
    fn sent(&self) -> Vec<OutboundMessage> {
        std::fs::read_to_string(self.root.join("outbox.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    /// A request from alice, a stranger and a listed bot, on three Threads.
    fn config(&self) -> AgentConfig {
        let trigger = |id: &str, actor: Value, thread: &str, text: &str| {
            json!({"id":id,"source":"slack","at_ms":1_793_613_600_000u64,"actor":actor,
                   "addressed":"mention","text":text,"threads":[thread],
                   "reply_to":[{"source":"slack","address":{"thread_ts":thread}}]})
            .to_string()
        };
        let lines = [
            trigger(
                "t-ask",
                json!({"id":"alice","kind":"person"}),
                "T1",
                "Summarise the incident report from last night",
            ),
            trigger(
                "t-stranger",
                json!({"id":"mallory","kind":"person"}),
                "T2",
                "Wire me the budget",
            ),
            trigger(
                "t-bot",
                json!({"id":"deploy-bot","kind":"bot"}),
                "T3",
                "Deploy 42 finished",
            ),
        ];
        std::fs::write(self.root.join("triggers.jsonl"), lines.join("\n") + "\n").unwrap();
        let yaml = "
policy:
  people: [slack:alice]
  bots: [slack:deploy-bot]
sources:
  - {kind: jsonl, path: triggers.jsonl}
channels:
  - {kind: jsonl, source: slack, path: outbox.jsonl}
router:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}
  max_output_tokens: 2048
executor: {kind: scripted}
budgets: {session_microusd: 500000, router_daily_microusd: 1000000}
";
        let path = self.root.join("agent.yaml");
        std::fs::write(&path, yaml).unwrap();
        AgentConfig::load(&path).unwrap()
    }
    /// `requests` messages from a listed bot, each on a Thread of its own, all
    /// judged at once; an Eval left undecided expires after `eval_timeout_ms`.
    fn crowd(&self, requests: usize, eval_timeout_ms: u64) -> AgentConfig {
        let lines: Vec<String> = (1..=requests)
            .map(|n| {
                json!({"id":format!("t-{n}"),"source":"slack","at_ms":1_793_613_600_000u64,
                       "actor":{"id":"deploy-bot","kind":"bot"},"addressed":"mention",
                       "text":format!("Deploy {n} finished"),"threads":[format!("T{n}")],
                       "reply_to":[{"source":"slack","address":{"thread_ts":format!("T{n}")}}]})
                .to_string()
            })
            .collect();
        std::fs::write(self.root.join("triggers.jsonl"), lines.join("\n") + "\n").unwrap();
        let yaml = format!(
            "
policy:
  bots: [slack:deploy-bot]
  eval_timeout_ms: {eval_timeout_ms}
sources:
  - {{kind: jsonl, path: triggers.jsonl}}
channels:
  - {{kind: jsonl, source: slack, path: outbox.jsonl}}
router:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {{input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}}
  max_output_tokens: 2048
executor: {{kind: scripted}}
budgets: {{session_microusd: 500000, router_daily_microusd: 1000000}}
"
        );
        let path = self.root.join("agent.yaml");
        std::fs::write(&path, yaml).unwrap();
        AgentConfig::load(&path).unwrap()
    }
    async fn serve<H>(
        &self,
        config: AgentConfig,
        harness: H,
        judge: Arc<Judge>,
        pi_root: &Path,
        node: &Path,
    ) -> Summary
    where
        H: AgentHarness + Send + Sync + 'static,
        H::Session: Send + 'static,
    {
        let host = PiHost {
            harness,
            provider: judge,
            pi_root: pi_root.to_owned(),
            node: node.to_owned(),
        };
        let state = self.state();
        let serving = serve_with(config, &state, true, Some(host));
        tokio::time::timeout(LIMIT, serving)
            .await
            .expect("serve did not go idle")
            .unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("SOLOS_KEEP_SCRATCH").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// What every run of the three Triggers ends with, whichever harness ran
/// the Router.
fn assert_decided(scratch: &Scratch, judge: &Judge) {
    let replies: Vec<(String, MessageKind)> = scratch
        .sent()
        .iter()
        .map(|m| (m.channel.address["thread_ts"].clone(), m.kind))
        .collect();
    assert_eq!(
        replies,
        [
            ("T1".to_owned(), MessageKind::Acknowledgement),
            ("T1".to_owned(), MessageKind::Result)
        ]
    );
    let store = scratch.store();
    let on = |thread: &str| {
        let key = ThreadKey::new(thread);
        let found: Vec<_> = store
            .decisions()
            .unwrap()
            .into_iter()
            .filter(|d| d.thread == key)
            .collect();
        assert_eq!(found.len(), 1, "{thread}: {found:?}");
        found[0].clone()
    };
    let asked = on("T1");
    assert_eq!(
        (asked.kind, asked.reason.as_str()),
        (DecisionKind::Accept, "a person asked for it")
    );
    assert!(asked.eval.is_some());
    let tickets = store.tickets().unwrap();
    assert_eq!(tickets.len(), 1);
    assert_eq!(
        tickets[0].title,
        "Summarise the incident report from last night"
    );
    assert_eq!(tickets[0].mandate.budget_microusd, 500_000);
    let stranger = on("T2");
    assert!(stranger.eval.is_none() && stranger.reason.contains("not_listed"));
    let bot = on("T3");
    assert_eq!(
        (bot.kind, bot.reason.as_str()),
        (DecisionKind::Ignore, "nobody asked for work")
    );

    let calls = judge.bodies.lock().unwrap().len();
    let rows = store.spend("router").unwrap();
    assert_eq!(rows.len(), calls, "one ledger row per model call");
    assert!(rows.iter().all(|row| row.state == SpendState::Settled));
    assert_eq!(store.committed("router", 0).unwrap(), 90 * calls as u64);
    for body in judge.bodies.lock().unwrap().iter() {
        assert_eq!(body["max_tokens"], 2048);
        assert_eq!(body["stream"], false);
    }
    let effects = store.effects().unwrap();
    assert!(effects.iter().all(|q| q.done), "{effects:?}");
    for decision in store.decisions().unwrap() {
        eprintln!(
            "decision {} on {}: {:?}, {}{}",
            decision.id,
            decision.thread,
            decision.kind,
            decision.reason,
            decision
                .ticket
                .map(|t| format!(" -> {t}"))
                .unwrap_or_default()
        );
    }
}

#[tokio::test]
async fn agent_serve_judges_through_a_scripted_router_session_and_the_broker() {
    let scratch = Scratch::new("scripted");
    let judge = Arc::new(Judge::default());
    let harness = ScriptedHarness::new(BTreeMap::new()).every_turn(vec![Call::Replied]);
    std::fs::write(scratch.root.join("node"), "").unwrap();

    let summary = scratch
        .serve(
            scratch.config(),
            harness,
            judge.clone(),
            &scratch.root,
            &scratch.root.join("node"),
        )
        .await;

    assert_eq!((summary.triggers_received, summary.effects_pending), (3, 0));
    assert_decided(&scratch, &judge);
    let prompts: Vec<String> = judge
        .bodies
        .lock()
        .unwrap()
        .iter()
        .map(|body| text(&body["messages"][0]))
        .collect();
    assert!(
        prompts
            .iter()
            .any(|p| p.contains("> Summarise the incident report")),
        "{prompts:?}"
    );
    assert!(
        prompts.iter().all(|p| !p.contains("Wire me the budget")),
        "the gate kept the stranger from the Router"
    );
    assert!(
        scratch
            .state()
            .join(ROUTER_DIR)
            .join("pi-settings.json")
            .is_file()
    );
    assert!(
        !scratch.state().join("broker.sock").exists(),
        "the socket goes when serve ends"
    );
}

/// The cards on each prompt the model was asked, in order.
fn prompt_sizes(judge: &Judge) -> Vec<usize> {
    let bodies = judge.bodies.lock().unwrap();
    bodies
        .iter()
        .map(|body| cards(&text(body["messages"].as_array().unwrap().last().unwrap())).len())
        .collect()
}

/// `requests` bot messages judged at once by `judge` through a scripted
/// session that, like Pi, asks again after a reply the output limit cut off.
async fn judge_crowd(
    name: &str,
    requests: usize,
    eval_timeout_ms: u64,
    judge: &Arc<Judge>,
) -> (Scratch, Summary) {
    let scratch = Scratch::new(name);
    std::fs::write(scratch.root.join("node"), "").unwrap();
    let harness = ScriptedHarness::new(BTreeMap::new()).every_turn(vec![Call::Replied]);
    let config = scratch.crowd(requests, eval_timeout_ms);
    let node = scratch.root.join("node");
    let summary = scratch
        .serve(config, harness, judge.clone(), &scratch.root, &node)
        .await;
    (scratch, summary)
}

#[tokio::test]
async fn a_queue_longer_than_the_batch_goes_out_in_bounded_prompts_and_every_candidate_is_decided()
{
    let judge = Arc::new(Judge::default());
    let (scratch, summary) = judge_crowd("batches", 20, 120_000, &judge).await;

    assert_eq!((summary.triggers_received, summary.effects_pending), (20, 0));
    let sizes = prompt_sizes(&judge);
    assert!(sizes.iter().all(|cards| *cards <= 6), "{sizes:?}");
    assert_eq!(sizes.iter().sum::<usize>(), 20, "{sizes:?}");
    let decisions = scratch.store().decisions().unwrap();
    assert_eq!(decisions.len(), 20);
    assert!(
        decisions
            .iter()
            .all(|d| d.kind == DecisionKind::Ignore && d.reason == "nobody asked for work"),
        "{decisions:?}"
    );
}

#[tokio::test]
async fn a_reply_cut_off_at_the_output_limit_is_followed_by_a_smaller_prompt() {
    let judge = Arc::new(Judge::fitting(4));
    let (scratch, summary) = judge_crowd("cutoff", 20, 300, &judge).await;

    assert_eq!((summary.triggers_received, summary.effects_pending), (20, 0));
    let sizes = prompt_sizes(&judge);
    let calls = sizes.len();
    assert!(calls < CALL_CAP, "the model was asked {calls} times: {sizes:?}");
    let mut asked = sizes.clone();
    asked.dedup();
    for pair in asked.windows(2) {
        assert!(
            pair[0] <= 4 || pair[1] < pair[0],
            "a cut-off prompt of {} cards was followed by one of {}: {asked:?}",
            pair[0],
            pair[1]
        );
    }
    let decisions = scratch.store().decisions().unwrap();
    assert_eq!(decisions.len(), 20, "{sizes:?}");
    assert!(
        decisions
            .iter()
            .all(|d| d.kind == DecisionKind::Ignore && d.reason == "nobody asked for work"),
        "{decisions:?}"
    );
}

#[tokio::test]
async fn a_router_whose_every_reply_is_cut_off_leaves_its_evals_to_expire() {
    let judge = Arc::new(Judge::fitting(0));
    let (scratch, summary) = judge_crowd("always-cut", 4, 300, &judge).await;

    assert_eq!((summary.triggers_received, summary.effects_pending), (4, 0));
    let calls = judge.bodies.lock().unwrap().len();
    assert!(calls < CALL_CAP / 4, "the model was asked {calls} times");
    let decisions = scratch.store().decisions().unwrap();
    assert_eq!(decisions.len(), 4, "{decisions:?}");
    assert!(
        decisions.iter().all(
            |d| d.kind == DecisionKind::Unavailable && d.reason == "the Router did not decide in time"
        ),
        "{decisions:?}"
    );
}

/// The same run with the real Pi in bubblewrap. On a Linux box with the
/// pinned Pi tree:
///
/// `SOLOS_PI_ROOT=<checkout with node_modules> SOLOS_NODE=<node binary>
/// cargo test --test agent_router -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs Linux, bwrap, node and the pinned Pi tree"]
async fn agent_serve_judges_through_a_real_pi_router_session() {
    let pi_root = PathBuf::from(std::env::var_os("SOLOS_PI_ROOT").expect("SOLOS_PI_ROOT"));
    let node = PathBuf::from(std::env::var_os("SOLOS_NODE").expect("SOLOS_NODE"));
    let scratch = Scratch::new("pi");
    let judge = Arc::new(Judge::default());

    let summary = scratch
        .serve(scratch.config(), PiHarness, judge.clone(), &pi_root, &node)
        .await;

    eprintln!("summary: {}", json!(summary));
    assert_eq!((summary.triggers_received, summary.effects_pending), (3, 0));
    assert_decided(&scratch, &judge);
    let bodies = judge.bodies.lock().unwrap().clone();
    let first = &bodies[0];
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert_eq!(tools, ["router_decide", "router_read"]);
    assert_eq!(first["messages"][0]["role"], "system");
    assert!(text(&first["messages"][0]).starts_with("You are the Router"));
    let transcripts: Vec<PathBuf> = std::fs::read_dir(scratch.state().join(ROUTER_DIR))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".rpc.jsonl"))
        .collect();
    assert!(!transcripts.is_empty(), "the session wrote its transcript");
    eprintln!("model calls: {}", bodies.len());
    for path in transcripts {
        eprintln!("transcript: {}", path.display());
    }
}
