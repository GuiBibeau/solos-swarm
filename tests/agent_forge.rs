use serde_json::{Value, json};
use solos_agent_core::change::{Deliverable, RepoName};
use solos_agent_core::model::{Method, Plan, Ticket, TicketOutcome};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::pi_executor::WORK_DIR;
use solos_swarm::agent::{AgentConfig, PiHost, STORE_FILE, TICKETS_DIR, serve_with};
use solos_swarm::gateway::{Dispatch, Provider};
use solos_swarm::harness::scripted::{Call, ScriptedHarness, ScriptedSession};
use solos_swarm::harness::{AgentHarness, Observation, PiHarness, Role, Session, SessionSpec};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

const LIMIT: Duration = Duration::from_secs(120);
const ROUTER: &str = "router:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}
  max_output_tokens: 2048
";
const WORKERS: &str = "executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}
  max_output_tokens: 2048
  run_code: {}
";
const FORGE: &str = "forge:
  kind: local
  remote: remote
  identity: {name: solos-bot, email: bot@example.com}
  repos:
    acme/widgets:
      about: Widgets, in Rust.
      base: main
      budget_microusd: 400000
      notes: Tests run with ./check.
      owes: change
";
const ANSWER: &str = "acme/widgets tests its widgets with ./check.";
const COMMIT: &str = "git log --format=%s && echo checked >> README.md && \
                      git commit --quiet -am 'worker change' && \
                      git log -1 --format='%an <%ae> | %s'";

fn sandbox() -> bool {
    let found = cfg!(target_os = "linux")
        && Path::new("/usr/bin/bwrap").is_file()
        && Path::new("/usr/bin/prlimit").is_file();
    if !found {
        eprintln!("skipped: run_code needs Linux, /usr/bin/bwrap and /usr/bin/prlimit");
    }
    found
}

fn call(name: &str, arguments: &Value) -> Value {
    json!({"id":format!("call-{name}"),"type":"function",
           "function":{"name":name,"arguments":arguments.to_string()}})
}

struct Model;

impl Provider for Model {
    fn dispatch(&self, body: Value) -> Dispatch {
        let prompt = body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .and_then(|last| last["content"].as_str())
            .unwrap_or_default()
            .to_owned();
        let calls: Vec<Value> = if prompt.contains(" new candidate") {
            prompt
                .lines()
                .filter_map(|line| line.strip_prefix("eval "))
                .map(|card| {
                    let eval = card.split(" | ").next().unwrap();
                    call(
                        "router_decide",
                        &json!({"eval":eval,"decision":"accept","reason":"a question about a repository",
                                "title":"How widgets are tested","repo":"acme/widgets",
                                "scope":"Answer from acme/widgets' code how its widgets are tested."}),
                    )
                })
                .collect()
        } else {
            vec![call(
                "worker_report",
                &json!({"status":"ready","text":ANSWER}),
            )]
        };
        Box::pin(async move {
            Ok(json!({"id":"model-1","created":1,"model":"deepseek-flash",
                      "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":calls},
                                  "finish_reason":"tool_calls"}],
                      "usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":0,"completion_tokens":50}}))
        })
    }
}

#[derive(Default)]
struct Seen {
    grants: Vec<String>,
    /// The pstack tree the session's spec names, and the argv Pi would be
    /// launched with from it.
    pstack: Option<PathBuf>,
    argv: Vec<String>,
    system_prompt: String,
    calls: Vec<(String, u16, Value)>,
}

struct Recording {
    inner: ScriptedHarness,
    seen: Arc<Mutex<Seen>>,
}

struct RecordedSession {
    inner: ScriptedSession,
    spec: SessionSpec,
    seen: Arc<Mutex<Seen>>,
    first: bool,
}

impl AgentHarness for Recording {
    type Session = RecordedSession;
    async fn start(
        &self,
        spec: &SessionSpec,
        events: mpsc::Sender<Observation>,
    ) -> anyhow::Result<RecordedSession> {
        Ok(RecordedSession {
            inner: self.inner.start(spec, events).await?,
            spec: spec.clone(),
            seen: self.seen.clone(),
            first: matches!(spec.role, Role::TicketWorker),
        })
    }
}

impl RecordedSession {
    async fn post(&self, path: &str, body: Value) -> anyhow::Result<(u16, Value)> {
        let client = reqwest::Client::builder()
            .unix_socket(self.spec.broker_socket.clone())
            .build()?;
        let response = client
            .post(format!("http://broker/{path}"))
            .bearer_auth(&self.spec.token)
            .json(&body)
            .send()
            .await?;
        let status = response.status().as_u16();
        Ok((status, response.json().await.unwrap_or(Value::Null)))
    }
}

impl Session for RecordedSession {
    fn id(&self) -> &str {
        self.inner.id()
    }
    async fn prompt(&mut self, text: &str) -> anyhow::Result<()> {
        if std::mem::take(&mut self.first) {
            let (_, session) = self.post("session", json!({})).await?;
            let mut calls = Vec::new();
            for (tool, arguments) in [
                ("read", json!({"path": "/work/README.md"})),
                ("read", json!({"path": "/host/repo.json"})),
                ("run_code", json!({"code": COMMIT})),
                ("read", json!({"path": "/work/../../etc/passwd"})),
                ("read", json!({"path": "/pstack/skills"})),
            ] {
                let (status, reply) = self.post(&format!("tool/{tool}"), arguments).await?;
                calls.push((tool.to_owned(), status, reply));
            }
            let argv = PiHarness::command(&self.spec)
                .as_std()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            *self.seen.lock().unwrap() = Seen {
                grants: self.spec.grants.clone(),
                pstack: self.spec.pstack.clone(),
                argv,
                system_prompt: session["system_prompt"].as_str().unwrap_or("").to_owned(),
                calls,
            };
        }
        self.inner.prompt(text).await
    }
    async fn stop(&mut self) -> anyhow::Result<bool> {
        self.inner.stop().await
    }
    async fn join(self) -> usize {
        self.inner.join().await
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Alice",
            "-c",
            "user.email=alice@example.com",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agent-forge-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("node"), "").unwrap();
        let upstream = root.join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        git(&upstream, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(upstream.join("README.md"), "Widgets\n").unwrap();
        git(&upstream, &["add", "README.md"]);
        git(&upstream, &["commit", "--quiet", "-m", "first"]);
        std::fs::write(upstream.join("README.md"), "Widgets\nRun ./check.\n").unwrap();
        git(&upstream, &["commit", "--quiet", "-am", "second"]);
        std::fs::create_dir_all(root.join("remote/acme")).unwrap();
        git(
            &root,
            &[
                "clone",
                "--quiet",
                "--bare",
                "upstream",
                "remote/acme/widgets.git",
            ],
        );
        let ask = json!({"id":"t-ask","source":"slack","at_ms":1_793_613_600_000u64,
            "actor":{"id":"alice","kind":"person"},"addressed":"mention",
            "text":"How does acme/widgets test its widgets?","threads":["T1"],
            "reply_to":[{"source":"slack","address":{"thread_ts":"T1"}}]});
        std::fs::write(root.join("triggers.jsonl"), format!("{ask}\n")).unwrap();
        Self { root }
    }

    fn config(&self) -> AgentConfig {
        let yaml = format!(
            "policy:\n  people: [slack:alice]\n\
             sources:\n  - {{kind: jsonl, path: triggers.jsonl}}\n\
             channels:\n  - {{kind: jsonl, source: slack, path: outbox.jsonl}}\n\
             {ROUTER}{WORKERS}{FORGE}\
             budgets: {{session_microusd: 500000, router_daily_microusd: 1000000}}\n"
        );
        let path = self.root.join("agent.yaml");
        std::fs::write(&path, yaml).unwrap();
        AgentConfig::load(&path).unwrap()
    }

    async fn serve(&self) -> Arc<Mutex<Seen>> {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let harness = Recording {
            inner: ScriptedHarness::new(BTreeMap::new()).every_turn(vec![Call::Replied]),
            seen: seen.clone(),
        };
        let host = PiHost {
            harness,
            provider: Arc::new(Model),
            pi_root: self.root.clone(),
            node: self.root.join("node"),
            browser: solos_swarm::browse::Launcher::Lightpanda,
        };
        let state = self.root.join("state");
        let serving = serve_with(self.config(), &state, true, Some(host));
        tokio::time::timeout(LIMIT, serving)
            .await
            .expect("serve did not go idle")
            .unwrap();
        seen
    }

    fn ticket(&self) -> Ticket {
        let store = SqliteStore::open(self.root.join("state").join(STORE_FILE)).unwrap();
        let tickets = store.tickets().unwrap();
        let [ticket] = &tickets[..] else {
            panic!("{tickets:?}")
        };
        ticket.clone()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stripped(lines: &Value) -> String {
    lines
        .as_str()
        .unwrap_or_else(|| panic!("{lines}"))
        .split_inclusive('\n')
        .map(|line| line.split_once('\t').unwrap().1)
        .collect()
}

#[tokio::test]
async fn a_question_about_a_repository_gets_a_worker_on_a_clone_it_reads_and_commits_in() {
    if !sandbox() {
        return;
    }
    let scratch = Scratch::new("clone");
    let tip = git(&scratch.root.join("upstream"), &["rev-parse", "main"]);

    let seen = scratch.serve().await;

    let ticket = scratch.ticket();
    let widgets = RepoName::try_from("acme/widgets".to_owned()).unwrap();
    assert_eq!(ticket.mandate.repo, Some(widgets));
    assert_eq!(
        ticket.plan,
        Plan::Session {
            harness: None,
            environment: None,
            method: Some(Method {
                deliverable: Deliverable::Change
            }),
        },
        "the repository's owes, since the Router named none"
    );
    assert_eq!(
        ticket.mandate.budget_microusd,
        Some(400_000),
        "the repository's budget"
    );
    assert_eq!(
        ticket.ended.as_ref().map(|e| e.outcome),
        Some(TicketOutcome::Completed),
        "every scripted call was served: a refused one ends the session"
    );
    assert_eq!(ticket.latest_report().unwrap().text, ANSWER);

    let seen = seen.lock().unwrap();
    for tool in ["read", "run_code"] {
        assert!(seen.grants.iter().any(|t| t == tool), "{:?}", seen.grants);
    }
    assert_eq!(seen.pstack, None, "this agent has no pstack");
    assert!(
        !seen.argv.iter().any(|arg| arg.contains("pstack")),
        "read is granted, but no pstack is bound: {:?}",
        seen.argv
    );
    assert!(!seen.system_prompt.contains("/pstack"));
    let told = format!(
        "Repository: acme/widgets, base main. /work is a clone of it at main's tip ({}), with \
         its history and your commit identity: run git and its code there with run_code, and \
         read its files with read. /host/repo.json says the same.\n\
         Notes on acme/widgets: Tests run with ./check.",
        &tip[..7]
    );
    assert!(seen.system_prompt.contains(&told), "{}", seen.system_prompt);

    let [readme, repo_json, commit, climbed, pstack] = &seen.calls[..] else {
        panic!("{:?}", seen.calls)
    };
    assert_eq!(readme.1, 200, "{readme:?}");
    assert_eq!(stripped(&readme.2["lines"]), "Widgets\nRun ./check.\n");
    assert_eq!(repo_json.1, 200, "{repo_json:?}");
    let written: Value = serde_json::from_str(&stripped(&repo_json.2["lines"])).unwrap();
    assert_eq!(
        written,
        json!({"repo":"acme/widgets","base":"main","base_commit":tip,
               "notes":"Tests run with ./check."})
    );
    assert_eq!(commit.1, 200, "{commit:?}");
    assert_eq!(commit.2["exit_code"], 0, "{commit:?}");
    assert_eq!(
        commit.2["stdout"], "second\nfirst\nsolos-bot <bot@example.com> | worker change\n",
        "run_code sees the history and commits as the agent"
    );
    assert_eq!(climbed.1, 403, "{climbed:?}");
    assert!((400..500).contains(&pstack.1), "{pstack:?}");

    let work = scratch
        .root
        .join("state")
        .join(TICKETS_DIR)
        .join(ticket.id.as_str())
        .join(WORK_DIR);
    assert_eq!(
        git(&work, &["log", "--format=%s"]),
        "worker change\nsecond\nfirst"
    );
    assert_eq!(git(&work, &["rev-parse", "HEAD~1"]), tip);
    let mirror = scratch.root.join("state/forge/acme/widgets.git");
    assert_eq!(git(&mirror, &["rev-parse", "--is-bare-repository"]), "true");
    assert_eq!(git(&mirror, &["rev-parse", "refs/heads/main"]), tip);
}
