//! Acceptance case 2 of `docs/agent-mode/repo-models/design.md`: a
//! DeepSeek-only host with a repository whose text may go only to Anthropic
//! declines the request at accept, tells the person once, and starts nothing.
//! The Router is a real Pi session whose model calls are answered here, so
//! the `router_decide` goes through the broker to the Engine as it does live.

use serde_json::{Value, json};
use solos_agent_core::model::{DecisionKind, Effect, MessageKind};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::{AgentConfig, PiHost, STORE_FILE, TICKETS_DIR, serve_with};
use solos_swarm::gateway::{Dispatch, Provider};
use solos_swarm::harness::scripted::{Call, ScriptedHarness};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(120);
const SENTENCE: &str = "I can't work on acme/widgets here: its code may go only to anthropic, \
                        and this agent runs no anthropic model. Nothing was started.";

/// Answers the Router: an explicit card on `acme/widgets` is accepted with
/// the repository named; after the tool result the turn ends.
#[derive(Default)]
struct Judge {
    bodies: Mutex<Vec<Value>>,
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
        let message = if last["role"] == "user" {
            let eval = text(&last)
                .lines()
                .find_map(|line| line.strip_prefix("eval "))
                .and_then(|card| card.split(" | ").next().map(str::to_owned));
            match eval {
                Some(eval) => json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call-decide",
                        "type": "function",
                        "function": {
                            "name": "router_decide",
                            "arguments": json!({
                                "eval": eval,
                                "decision": "accept",
                                "reason": "a person asked for it",
                                "title": "Fix the widgets",
                                "scope": "Work on the widgets.",
                                "repo": "acme/widgets",
                            })
                            .to_string(),
                        },
                    }],
                }),
                None => json!({"role": "assistant", "content": "No work to judge."}),
            }
        } else {
            json!({"role": "assistant", "content": "Decided."})
        };
        let finish = if message["tool_calls"].is_array() {
            "tool_calls"
        } else {
            "stop"
        };
        Box::pin(async move {
            Ok(json!({
                "id": "judge-1",
                "created": 1,
                "model": "deepseek-flash",
                "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                "usage": {"prompt_tokens": 100, "prompt_cache_hit_tokens": 0, "completion_tokens": 50},
            }))
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
            "agent-repo-models-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::create_dir_all(root.join("remote")).unwrap();
        Self { root }
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    fn store(&self) -> SqliteStore {
        SqliteStore::open(self.state().join(STORE_FILE)).unwrap()
    }

    fn sent(&self) -> Vec<solos_agent_core::model::OutboundMessage> {
        std::fs::read_to_string(self.root.join("outbox.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn config(&self) -> AgentConfig {
        let trigger = json!({
            "id": "t-ask",
            "source": "slack",
            "at_ms": 1_793_613_600_000u64,
            "actor": {"id": "alice", "kind": "person"},
            "addressed": "mention",
            "text": "Fix the widgets",
            "threads": ["T1"],
            "reply_to": [{"source": "slack", "address": {"thread_ts": "T1"}}],
        });
        std::fs::write(self.root.join("triggers.jsonl"), trigger.to_string() + "\n").unwrap();
        let yaml = "
policy:
  people: [slack:alice]
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
executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}
  max_output_tokens: 2048
  run_code: {}
budgets: {session_microusd: 500000, router_daily_microusd: 1000000}
forge:
  kind: local
  remote: remote
  identity: {name: solos-bot, email: bot@example.com}
  repos:
    acme/widgets:
      about: Widgets, in Rust.
      base: main
      models: {allow: [anthropic]}
";
        let path = self.root.join("agent.yaml");
        std::fs::write(&path, yaml).unwrap();
        AgentConfig::load(&path).unwrap()
    }

    async fn serve(&self, config: AgentConfig, judge: Arc<Judge>) -> solos_swarm::agent::Summary {
        let host = PiHost {
            // Pi's loop, so the model reads the tool result: the reply says
            // the person was told.
            harness: ScriptedHarness::new(BTreeMap::new()).every_turn(vec![Call::Agent]),
            provider: judge,
            pi_root: self.root.clone(),
            node: self.root.join("node"),
            browser: solos_swarm::browse::Launcher::Lightpanda,
        };
        std::fs::write(self.root.join("node"), "").unwrap();
        let state = self.state();
        tokio::time::timeout(LIMIT, serve_with(config, &state, true, Some(host)))
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

/// The runtime builds its workers' `run_code` at start, which needs
/// bubblewrap even when no worker launches. CI's shards have none, so this
/// test skips there as `agent_forge`'s do; run it where `/usr/bin/bwrap` is.
fn sandbox() -> bool {
    let found = cfg!(target_os = "linux")
        && Path::new("/usr/bin/bwrap").is_file()
        && Path::new("/usr/bin/prlimit").is_file();
    if !found {
        eprintln!(
            "skipped: the runtime's run_code needs Linux, /usr/bin/bwrap and /usr/bin/prlimit"
        );
    }
    found
}

#[tokio::test]
async fn a_deepseek_only_host_declines_a_repository_that_allows_only_anthropic() {
    if !sandbox() {
        return;
    }
    let scratch = Scratch::new("decline");
    let judge = Arc::new(Judge::default());
    scratch.serve(scratch.config(), judge.clone()).await;

    // The requester hears the host's sentence once, in the request's thread.
    let sent = scratch.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].kind, MessageKind::Result);
    assert_eq!(sent[0].text, SENTENCE);
    assert_eq!(sent[0].channel.address["thread_ts"], "T1");
    assert_eq!(sent[0].ticket, None);

    // The Engine recorded a declined Decision and started no Ticket.
    let store = scratch.store();
    let decisions = store.decisions().unwrap();
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert_eq!(decisions[0].kind, DecisionKind::Declined);
    assert_eq!(decisions[0].ticket, None);
    assert!(store.tickets().unwrap().is_empty(), "nothing started");

    // Nothing was launched, and the repository was never cloned.
    let effects = store.effects().unwrap();
    assert!(
        effects
            .iter()
            .all(|queued| !matches!(queued.effect, Effect::Launch { .. })),
        "{effects:?}"
    );
    let tickets = scratch.state().join(TICKETS_DIR);
    assert!(
        std::fs::read_dir(&tickets)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true),
        "no Ticket directory, and so no clone"
    );

    // The Router was told the person heard it, so its reply does not claim
    // work that never started.
    let bodies = judge.bodies.lock().unwrap();
    let mut told = false;
    for body in bodies.iter() {
        for message in body["messages"].as_array().into_iter().flatten() {
            let content = text(message);
            if content.contains("The person who asked was told") {
                assert!(content.contains(SENTENCE), "{content}");
                told = true;
            }
        }
    }
    assert!(told, "the Router's tool reply says the person was told");
}
