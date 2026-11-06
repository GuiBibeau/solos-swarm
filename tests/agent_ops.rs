//! What an operator needs to run the agent in production, offline: a stop
//! that goes through the running agent's Engine and Policy, pruning against a
//! real Store, and the health check against a fake systemd, a fake broker
//! socket and a fake Discord.

use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use solos_agent_core::model::{
    Accept, Acknowledge, Actor, Addressed, ChannelRef, EndedBy, Plan, ThreadKey, TicketId,
    TicketOutcome, Trigger, TriggerId, Verdict,
};
use solos_agent_core::{AllowlistPolicy, Engine, Repositories};
use solos_agent_store::SqliteStore;
use solos_swarm::agent::mcp::{self, OperatorCommand, OperatorOrder};
use solos_swarm::agent::stop::{self, Outcome};
use solos_swarm::agent::{STORE_FILE, ServeOptions, serve_until};
use solos_swarm::broker::now_ms;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(60);
const DAY_MS: u64 = 86_400_000;
const BIN: &str = env!("CARGO_BIN_EXE_solos-swarm");

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
            "agent-ops-{name}-{}-{}",
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
    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn until<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(found) = ready() {
            return found;
        }
        assert!(start.elapsed() < LIMIT, "gave up waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn store(state: &Path) -> Option<SqliteStore> {
    SqliteStore::open_read_only(state.join(STORE_FILE)).ok()
}

fn open_tickets(state: &Path) -> Vec<TicketId> {
    store(state)
        .and_then(|store| store.open_tickets().ok())
        .unwrap_or_default()
}

/// The CLI, as an operator runs it.
async fn cli(args: &[&str]) -> (i32, String, String) {
    let output = tokio::process::Command::new(BIN)
        .args(args)
        .output()
        .await
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

// ---------------------------------------------------------------- stop

/// A scripted Router that accepts every request, and workers that take a day
/// over it, so a Ticket stays open until someone stops it.
fn stopping_config(scratch: &Scratch) -> PathBuf {
    scratch.write(
        "agent.yaml",
        "policy:\n  people: [mcp:alice]\nsources:\n  - {kind: mcp}\nchannels:\n  - {kind: mcp}\n\
         router: {kind: scripted}\n\
         executor: {kind: simulated, max_workers: 4, time_factor: 1.0, default_work_ms: 86400000}\n\
         budgets: {session_microusd: 500000}\n",
    )
}

fn serving<'a>(
    config: &'a Path,
    state: &'a Path,
    stop: tokio::sync::oneshot::Receiver<()>,
) -> impl std::future::Future<Output = anyhow::Result<solos_swarm::agent::Summary>> + 'a {
    serve_until(config, state, ServeOptions::default(), async move {
        let _ = stop.await;
    })
}

fn ended_by(state: &Path, ticket: &TicketId) -> Option<(EndedBy, TicketOutcome, String)> {
    let found = store(state)?.read_ticket(ticket).ok()??;
    found
        .ended
        .map(|ended| (ended.by, ended.outcome, ended.reason))
}

#[tokio::test]
async fn an_operator_stops_a_ticket_through_the_running_agent_and_the_policy_decides_who() {
    let scratch = Scratch::new("stop");
    let config = stopping_config(&scratch);
    let state = scratch.state();

    // The agent is up: three requests become three open Tickets.
    let (halt, stopped) = tokio::sync::oneshot::channel();
    let driver = async {
        until("the mcp inbox", || {
            mcp::inbox(&state).exists().then_some(())
        })
        .await;
        for (n, text) in ["one", "two", "three"].into_iter().enumerate() {
            mcp::file(&state, "alice", Some(&format!("t{n}")), text, now_ms()).unwrap();
        }
        let tickets = until("three open tickets", || {
            let open = open_tickets(&state);
            (open.len() == 3).then_some(open)
        })
        .await;
        let [first, second, third] = [&tickets[0], &tickets[1], &tickets[2]];

        // A stranger is filed as `mcp:mallory`, which the Policy does not name.
        let (code, out, _) = cli(&[
            "agent",
            "stop",
            "--state-dir",
            p(&state),
            "--operator",
            "mallory",
            "--ticket",
            first.as_str(),
            "--reason",
            "let me in",
        ])
        .await;
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("is not allowed to stop this work"), "{out}");
        assert_eq!(open_tickets(&state).len(), 3, "nothing was stopped");

        // alice is: the Engine ends the Ticket as a person's Stop does.
        let (code, out, err) = cli(&[
            "agent",
            "stop",
            "--state-dir",
            p(&state),
            "--operator",
            "alice",
            "--ticket",
            first.as_str(),
            "--reason",
            "stale after the deploy",
        ])
        .await;
        assert_eq!(code, 0, "{out}{err}");
        assert!(out.contains("Stopped by alice."), "{out}");
        let (by, outcome, reason) = ended_by(&state, first).unwrap();
        assert_eq!((by, outcome), (EndedBy::Person, TicketOutcome::Stopped));
        assert_eq!(reason, "stopped by alice on mcp: stale after the deploy");
        assert_eq!(open_tickets(&state).len(), 2);

        // Again: nothing to do, and no new command is filed.
        let filed = std::fs::read_to_string(mcp::commands(&state))
            .unwrap()
            .lines()
            .count();
        let args = stop::Args {
            state_dir: state.clone(),
            operator: "alice".into(),
            ticket: first.to_string(),
            reason: "again".into(),
            wait: Duration::from_secs(5),
        };
        assert_eq!(stop::run(&args).await.unwrap(), Outcome::AlreadyStopped);
        let after = std::fs::read_to_string(mcp::commands(&state))
            .unwrap()
            .lines()
            .count();
        assert_eq!(filed, after);

        let missing = stop::Args {
            ticket: "tk_nope".into(),
            ..args.clone()
        };
        let err = stop::run(&missing).await.unwrap_err().to_string();
        assert!(err.contains("has no ticket tk_nope"), "{err}");
        let bad = stop::Args {
            ticket: "../x".into(),
            ..args
        };
        assert!(stop::run(&bad).await.is_err());
        let _ = halt.send(());
        (second.clone(), third.clone())
    };
    let (summary, (second, third)) = tokio::join!(serving(&config, &state, stopped), driver);
    summary.unwrap();

    // The agent is down: the command is filed, nobody answers, and a stop
    // that stayed filed past its time is not carried out later.
    let args = stop::Args {
        state_dir: state.clone(),
        operator: "alice".into(),
        ticket: second.to_string(),
        reason: "while it was down".into(),
        wait: Duration::from_secs(1),
    };
    let outcome = stop::run(&args).await.unwrap();
    assert_eq!(outcome, Outcome::NotApplied { waited_secs: 1 });
    assert_eq!(outcome.exit_code(), 2);
    assert!(outcome.render(second.as_str()).contains("Is it running?"));
    let stale = OperatorCommand {
        id: "stale".into(),
        at_ms: now_ms() - mcp::COMMAND_TTL_MS - 60_000,
        operator: "alice".into(),
        thread: "stop-stale".into(),
        ticket: third.clone(),
        order: OperatorOrder::Stop {
            reason: "last week's".into(),
        },
    };
    let mut commands = std::fs::OpenOptions::new()
        .append(true)
        .open(mcp::commands(&state))
        .unwrap();
    writeln!(commands, "{}", serde_json::to_string(&stale).unwrap()).unwrap();
    drop(commands);

    // The agent comes back and reads both: the fresh one stops its Ticket,
    // the stale one stops nothing.
    let (halt, stopped) = tokio::sync::oneshot::channel();
    let driver = async {
        until("the second ticket to be stopped", || {
            ended_by(&state, &second)
        })
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            ended_by(&state, &third).is_none(),
            "a command past its time is not carried out"
        );
        let (_, _, reason) = ended_by(&state, &second).unwrap();
        assert_eq!(reason, "stopped by alice on mcp: while it was down");
        let _ = halt.send(());
    };
    let (summary, ()) = tokio::join!(serving(&config, &state, stopped), driver);
    summary.unwrap();
}

#[test]
fn an_operator_stop_needs_a_reason_a_plain_name_and_an_agent_that_reads_commands() {
    let scratch = Scratch::new("filing");
    let state = scratch.state();
    let ticket = TicketId::new("tk_1");
    let no_agent = mcp::file_stop(&state, "alice", &ticket, "stale", 1)
        .unwrap_err()
        .to_string();
    assert!(no_agent.contains("kind: mcp"), "{no_agent}");
    mcp::open_commands(&state).unwrap();
    assert!(mcp::file_stop(&state, "alice", &ticket, "  ", 1).is_err());
    assert!(mcp::file_stop(&state, "alice", &ticket, &"x".repeat(501), 1).is_err());
    assert!(mcp::file_stop(&state, "a b", &ticket, "stale", 1).is_err());
    let filed = mcp::file_stop(&state, "alice", &ticket, "stale", 7).unwrap();
    assert!(filed.thread.starts_with("stop-"));
    let line = std::fs::read_to_string(mcp::commands(&state)).unwrap();
    let read: OperatorCommand = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(read.ticket, ticket);
    assert_eq!(read.at_ms, 7);
    assert_eq!(
        read.order,
        OperatorOrder::Stop {
            reason: "stale".into()
        }
    );
}

// --------------------------------------------------------------- prune

/// A Ticket in the Store at `at_ms`, stopped there when `end` says so.
fn ticket_at(state: &Path, name: &str, at_ms: u64, end: bool) -> TicketId {
    let policy = AllowlistPolicy {
        people: ["slack:alice".to_owned()].into(),
        ..AllowlistPolicy::default()
    };
    let store = SqliteStore::open(state.join(STORE_FILE)).unwrap();
    let mut engine = Engine::new(store, policy, move || at_ms);
    engine
        .receive(Trigger {
            id: TriggerId::new(format!("t-{name}")),
            source: "slack".into(),
            at_ms,
            actor: Actor::person("alice"),
            addressed: Addressed::Mention,
            text: format!("do {name}"),
            threads: vec![ThreadKey::new(format!("th-{name}"))],
            reply_to: vec![ChannelRef {
                source: "slack".into(),
                address: BTreeMap::from([("channel".to_owned(), "C1".to_owned())]),
            }],
            meta: Value::Null,
        })
        .unwrap();
    let eval = engine.evaluate_due().unwrap().remove(0);
    let accept = Verdict::Accept(Accept {
        title: name.into(),
        scope: format!("scope of {name}"),
        reason: "alice asked".into(),
        plan: Plan::Session {
            harness: None,
            environment: None,
            method: None,
        },
        budget_microusd: Some(500_000),
        acknowledge: Acknowledge::Standard,
        rules: Vec::new(),
        repo: None,
    });
    let decision = engine.decide(&eval, accept).unwrap();
    let ticket = engine.repos().decision(&decision).unwrap().ticket.unwrap();
    if end {
        engine
            .stop_ticket(&ticket, "slack", Actor::person("alice"), "enough")
            .unwrap();
    }
    ticket
}

fn worker_leftovers(state: &Path, ticket: &TicketId) -> PathBuf {
    let dir = state.join("tickets").join(ticket.as_str());
    for (file, size) in [
        ("work/src/lib.rs", 40_000),
        ("work/.git/objects/pack/p.pack", 80_000),
        ("host/repo.json", 40),
        ("prepared", 0),
        ("pi-settings.json", 30),
        ("as_1.rpc.jsonl", 50_000),
        ("as_1.stderr", 2_000),
    ] {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![1u8; size]).unwrap();
    }
    dir
}

fn db_bytes(state: &Path) -> Vec<u8> {
    let mut bytes = std::fs::read(state.join(STORE_FILE)).unwrap();
    bytes.extend(std::fs::read(state.join("agent.db-wal")).unwrap_or_default());
    bytes
}

#[tokio::test]
async fn prune_takes_back_only_what_ended_long_ago_and_never_touches_the_store() {
    let scratch = Scratch::new("prune");
    let state = scratch.state();
    let now = now_ms();
    let old = ticket_at(&state, "old", now - 20 * DAY_MS, true);
    let recent = ticket_at(&state, "recent", now - 3 * DAY_MS, true);
    let open = ticket_at(&state, "open", now - 30 * DAY_MS, false);
    let dirs: Vec<PathBuf> = [&old, &recent, &open]
        .into_iter()
        .map(|ticket| worker_leftovers(&state, ticket))
        .collect();
    let stray = state.join("tickets/tk_not_in_the_store/work");
    std::fs::create_dir_all(&stray).unwrap();
    std::fs::create_dir_all(state.join("trail").join(old.as_str())).unwrap();
    std::fs::write(
        state.join("trail").join(old.as_str()).join("a".repeat(64)),
        b"page",
    )
    .unwrap();
    let before = db_bytes(&state);
    let sd = p(&state);

    let (code, out, err) = cli(&[
        "agent",
        "prune",
        "--state-dir",
        sd,
        "--keep-days",
        "14",
        "--dry-run",
    ])
    .await;
    assert_eq!(code, 0, "{out}{err}");
    assert!(
        out.starts_with("agent prune (dry run): keep 14 days"),
        "{out}"
    );
    assert!(
        out.contains(&format!("would remove {old} work clone")),
        "{out}"
    );
    assert!(
        out.contains(&format!("would remove {old} transcripts")),
        "{out}"
    );
    assert!(
        !out.contains(recent.as_str()) && !out.contains(open.as_str()),
        "{out}"
    );
    assert!(out.contains("would free "), "{out}");
    assert!(
        out.contains("kept 1 open ticket(s) and 1 ended within 14 days"),
        "{out}"
    );
    assert!(
        out.contains("tk_not_in_the_store"),
        "a stranger is named and kept: {out}"
    );
    assert!(
        dirs[0].join("work/src/lib.rs").exists(),
        "a dry run removes nothing"
    );

    let (code, out, err) = cli(&[
        "agent",
        "prune",
        "--state-dir",
        sd,
        "--keep-days",
        "14",
        "--json",
    ])
    .await;
    assert_eq!(code, 0, "{out}{err}");
    let report: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["dry_run"], false);
    let kinds: Vec<&str> = report["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["ticket_work", "ticket_transcript"], "{out}");
    assert!(report["freed_bytes"].as_u64().unwrap() >= 130_000, "{out}");

    assert!(!dirs[0].join("work").exists());
    assert!(!dirs[0].join("host/repo.json").exists() && !dirs[0].join("prepared").exists());
    assert!(!dirs[0].join("as_1.rpc.jsonl").exists());
    assert!(dirs[0].join("pi-settings.json").exists());
    for kept in &dirs[1..] {
        assert!(kept.join("work/src/lib.rs").exists() && kept.join("as_1.stderr").exists());
    }
    assert!(stray.exists());
    assert!(
        state
            .join("trail")
            .join(old.as_str())
            .join("a".repeat(64))
            .exists(),
        "the trail is only touched when asked"
    );
    assert_eq!(
        db_bytes(&state),
        before,
        "the Store is untouched, byte for byte"
    );

    let (code, out, _) = cli(&["agent", "prune", "--state-dir", sd, "--keep-days", "14"]).await;
    assert_eq!(code, 0);
    assert!(
        out.contains("freed 0 B across 0 item(s)"),
        "idempotent: {out}"
    );

    let (code, out, _) = cli(&[
        "agent",
        "prune",
        "--state-dir",
        sd,
        "--keep-days",
        "14",
        "--include-trail",
    ])
    .await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(&format!("removed {old} trail blobs")), "{out}");
    assert!(!state.join("trail").join(old.as_str()).exists());

    let (code, _, err) = cli(&["agent", "prune", "--state-dir", sd, "--keep-days", "0"]).await;
    assert_eq!(code, 1);
    assert!(err.contains("at least 1"), "{err}");
    let (code, _, err) = cli(&[
        "agent",
        "prune",
        "--state-dir",
        p(&scratch.root),
        "--keep-days",
        "14",
    ])
    .await;
    assert_eq!(code, 1);
    assert!(err.contains("agent.db"), "{err}");
}

// -------------------------------------------------------------- health

/// A Discord that checks the bot token and records what is posted.
#[derive(Default)]
struct Discord {
    posts: Mutex<Vec<(String, Value)>>,
    reads: Mutex<usize>,
}

const TOKEN: &str = "bot-token-for-the-health-check";

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(&format!("Bot {TOKEN}"))
}

async fn fake_discord(discord: Arc<Discord>) -> String {
    async fn channel(
        State(discord): State<Arc<Discord>>,
        headers: HeaderMap,
        AxPath(id): AxPath<String>,
    ) -> (StatusCode, Json<Value>) {
        *discord.reads.lock().unwrap() += 1;
        if !authorized(&headers) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"message": "401: Unauthorized"})),
            );
        }
        if id != "42" {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"code": 50001, "message": "Missing Access"})),
            );
        }
        (StatusCode::OK, Json(json!({"id": id})))
    }
    async fn message(
        State(discord): State<Arc<Discord>>,
        headers: HeaderMap,
        AxPath(id): AxPath<String>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        if !authorized(&headers) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"message": "401: Unauthorized"})),
            );
        }
        discord.posts.lock().unwrap().push((id, body));
        (StatusCode::OK, Json(json!({"id": "1"})))
    }
    let app = Router::new()
        .route("/channels/{id}", get(channel))
        .route("/channels/{id}/messages", post(message))
        .with_state(discord);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    base
}

/// A broker that answers `GET /api/hello` on the state directory's socket.
fn fake_broker(state: &Path, status: &'static str) {
    let listener = UnixListener::bind(state.join("broker.sock")).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut seen = [0u8; 512];
            let _ = stream.read(&mut seen);
            let _ = stream.write_all(
                format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .as_bytes(),
            );
        }
    });
}

/// A `systemctl` that prints what `unit` says, from a file the test edits.
fn fake_systemctl(scratch: &Scratch) -> (PathBuf, PathBuf) {
    let bin = scratch.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("systemctl");
    std::fs::write(
        &script,
        "#!/bin/sh\n[ \"$1 $2\" = \"--user show\" ] || { echo \"unexpected: $*\" >&2; exit 2; }\ncat \"$FAKE_UNIT\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let unit = scratch.root.join("unit.txt");
    set_unit(&unit, "active", "running", 0);
    (bin, unit)
}

fn set_unit(file: &Path, active: &str, sub: &str, restarts: u32) {
    std::fs::write(
        file,
        format!(
            "LoadState=loaded\nActiveState={active}\nSubState={sub}\nNRestarts={restarts}\n\
             ActiveEnterTimestampMonotonic=0\n"
        ),
    )
    .unwrap();
}

fn health_config(scratch: &Scratch, api_base: &str, health: &str) -> PathBuf {
    scratch.write(
        "agent.yaml",
        &format!(
            "policy:\n  people: [discord:1]\n\
             sources:\n  - {{kind: discord, guilds: ['7'], token_file: discord-token, api_base: '{api_base}'}}\n\
             channels:\n  - kind: discord\n\
             router: {{kind: scripted}}\nexecutor: {{kind: scripted}}\n\
             watchdog: {{operator: {{source: discord, address: {{channel: '42'}}}}}}\n{health}"
        ),
    )
}

struct Health<'a> {
    scratch: &'a Scratch,
    bin: PathBuf,
    unit: PathBuf,
    config: PathBuf,
    token: PathBuf,
}

impl Health<'_> {
    async fn run(&self, extra: &[&str]) -> (i32, String, String) {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let state = self.scratch.state();
        let mut args = vec![
            "agent",
            "health",
            "--config",
            p(&self.config),
            "--state-dir",
            p(&state),
            "--token-file",
            p(&self.token),
            "--json",
        ];
        args.extend_from_slice(extra);
        let output = tokio::process::Command::new(BIN)
            .args(&args)
            .env("PATH", path)
            .env("FAKE_UNIT", &self.unit)
            .output()
            .await
            .unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

#[tokio::test]
async fn the_health_check_tells_a_down_agent_once_and_its_return_once_and_never_needs_serve() {
    let scratch = Scratch::new("health");
    let discord = Arc::new(Discord::default());
    let base = fake_discord(discord.clone()).await;
    fake_broker(&scratch.state(), "200 OK");
    let (bin, unit) = fake_systemctl(&scratch);
    let config = health_config(&scratch, &base, "health: {min_free_gb: 0}\n");
    let token = scratch.write("run/discord-token", &format!("{TOKEN}\n"));
    let health = Health {
        scratch: &scratch,
        bin,
        unit: unit.clone(),
        config,
        token: token.clone(),
    };

    let (code, out, err) = health.run(&[]).await;
    assert_eq!(code, 0, "{out}{err}");
    let report: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["ok"], true, "{out}");
    let names: Vec<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["config", "unit", "broker", "disk", "discord"],
        "{out}"
    );
    assert!(discord.posts.lock().unwrap().is_empty());
    assert!(
        token.exists(),
        "the check reads the token and keeps the file"
    );

    set_unit(&unit, "inactive", "dead", 0);
    let (code, out, _) = health.run(&[]).await;
    assert_eq!(code, 1, "{out}");
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["posted"],
        "down"
    );
    for _ in 0..3 {
        let (code, out, _) = health.run(&[]).await;
        assert_eq!(code, 1);
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap()["quiet"],
            "already told"
        );
    }
    {
        let posts = discord.posts.lock().unwrap();
        assert_eq!(posts.len(), 1, "{posts:?}");
        assert_eq!(posts[0].0, "42");
        let text = posts[0].1["content"].as_str().unwrap();
        assert!(text.contains("solos-agent is inactive (dead)"), "{text}");
        assert!(text.contains("outside the agent"), "{text}");
        assert_eq!(posts[0].1["allowed_mentions"], json!({"parse": []}));
    }
    let state: Value =
        serde_json::from_slice(&std::fs::read(scratch.state().join("health-state.json")).unwrap())
            .unwrap();
    assert_eq!(state["incident"]["announced"], json!(["unit"]));
    let mode = std::fs::metadata(scratch.state().join("health-state.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    set_unit(&unit, "active", "running", 0);
    let (code, out, _) = health.run(&[]).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["posted"],
        "recovered"
    );
    assert_eq!(discord.posts.lock().unwrap().len(), 2);
    health.run(&[]).await;
    assert_eq!(discord.posts.lock().unwrap().len(), 2, "quiet again");
}

#[tokio::test]
async fn the_health_check_sees_a_broker_that_is_up_in_systemd_but_does_not_answer() {
    let scratch = Scratch::new("broker");
    let discord = Arc::new(Discord::default());
    let base = fake_discord(discord.clone()).await;
    fake_broker(&scratch.state(), "503 Service Unavailable");
    let (bin, unit) = fake_systemctl(&scratch);
    let health = Health {
        scratch: &scratch,
        bin,
        unit,
        config: health_config(&scratch, &base, "health: {min_free_gb: 0}\n"),
        token: scratch.write("run/discord-token", TOKEN),
    };
    let (code, out, _) = health.run(&[]).await;
    assert_eq!(code, 1, "{out}");
    let text = discord.posts.lock().unwrap()[0].1["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(text.contains("the broker socket does not answer"), "{text}");
    assert!(text.contains("503"), "{text}");
}

#[tokio::test]
async fn a_restart_loop_is_told_even_though_the_unit_looks_active() {
    let scratch = Scratch::new("loop");
    let discord = Arc::new(Discord::default());
    let base = fake_discord(discord.clone()).await;
    fake_broker(&scratch.state(), "200 OK");
    let (bin, unit) = fake_systemctl(&scratch);
    let health = Health {
        scratch: &scratch,
        bin,
        unit: unit.clone(),
        config: health_config(&scratch, &base, "health: {min_free_gb: 0}\n"),
        token: scratch.write("run/discord-token", TOKEN),
    };
    assert_eq!(health.run(&[]).await.0, 0);
    set_unit(&unit, "active", "running", 3);
    let (code, out, _) = health.run(&[]).await;
    assert_eq!(code, 1, "{out}");
    let text = discord.posts.lock().unwrap()[0].1["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(text.contains("restarted itself 3 times"), "{text}");
}

#[tokio::test]
async fn a_revoked_token_fails_the_check_and_posts_nothing() {
    let scratch = Scratch::new("revoked");
    let discord = Arc::new(Discord::default());
    let base = fake_discord(discord.clone()).await;
    fake_broker(&scratch.state(), "200 OK");
    let (bin, unit) = fake_systemctl(&scratch);
    set_unit(&unit, "inactive", "dead", 0);
    let health = Health {
        scratch: &scratch,
        bin,
        unit,
        config: health_config(&scratch, &base, "health: {min_free_gb: 0}\n"),
        token: scratch.write("run/discord-token", "a-revoked-token"),
    };
    let (code, out, _) = health.run(&[]).await;
    assert_eq!(code, 1, "{out}");
    let report: Value = serde_json::from_str(&out).unwrap();
    assert!(report.get("posted").is_none(), "{out}");
    let discord_check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "discord")
        .unwrap();
    assert_eq!(discord_check["ok"], false);
    assert!(
        discord_check["detail"]
            .as_str()
            .unwrap()
            .contains("refused the bot token"),
        "{out}"
    );
    assert!(discord.posts.lock().unwrap().is_empty());

    // No token file at all: still a failing check, not a silent one.
    let missing = Health {
        token: scratch.root.join("run/absent"),
        ..health
    };
    let (code, out, _) = missing.run(&[]).await;
    assert_eq!(code, 1);
    assert!(out.contains("cannot read the Discord token file"), "{out}");
}

#[tokio::test]
async fn a_broken_config_is_told_to_the_channel_the_last_good_one_named() {
    let scratch = Scratch::new("config");
    let discord = Arc::new(Discord::default());
    let base = fake_discord(discord.clone()).await;
    fake_broker(&scratch.state(), "200 OK");
    let (bin, unit) = fake_systemctl(&scratch);
    let config = health_config(&scratch, &base, "health: {min_free_gb: 0}\n");
    let health = Health {
        scratch: &scratch,
        bin,
        unit,
        config: config.clone(),
        token: scratch.write("run/discord-token", TOKEN),
    };
    assert_eq!(health.run(&[]).await.0, 0);

    std::fs::write(&config, "policy: {peeple: []}\n").unwrap();
    let (code, out, _) = health.run(&[]).await;
    assert_eq!(code, 1, "{out}");
    let posts = discord.posts.lock().unwrap();
    assert_eq!(posts.len(), 1, "{posts:?}");
    let text = posts[0].1["content"].as_str().unwrap();
    assert!(text.contains("agent.yaml does not load"), "{text}");
}

#[tokio::test]
async fn low_disk_pauses_new_work_and_the_pause_lifts_when_space_returns() {
    let scratch = Scratch::new("disk");
    let discord = Arc::new(Discord::default());
    let base = fake_discord(discord.clone()).await;
    fake_broker(&scratch.state(), "200 OK");
    let (bin, unit) = fake_systemctl(&scratch);
    let token = scratch.write("run/discord-token", TOKEN);
    let high = health_config(&scratch, &base, "health: {min_free_gb: 100000000}\n");
    let health = Health {
        scratch: &scratch,
        bin,
        unit,
        config: high.clone(),
        token,
    };
    let (code, out, _) = health.run(&[]).await;
    assert_eq!(code, 1, "{out}");
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["pause"],
        "paused"
    );
    let pause = std::fs::read_to_string(scratch.state().join("PAUSE")).unwrap();
    assert!(pause.starts_with("paused by solos-agent-health"), "{pause}");
    // It is the pause the agent, its broker and the Watchdog already read.
    let now = now_ms();
    assert!(solos_swarm::gateway::effective_control(&scratch.state(), now / 1_000).is_some());
    let held = solos_swarm::agent::watchdog::read_pause(&scratch.state(), false, now).unwrap();
    assert_eq!(held.by, "the PAUSE file");
    let text = discord.posts.lock().unwrap()[0].1["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        text.contains("free on the state directory's disk"),
        "{text}"
    );
    assert!(text.contains("New work is paused"), "{text}");

    // A dry run changes nothing while the disk is still short.
    let (_, out, _) = health.run(&["--dry-run"]).await;
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["dry_run"],
        true
    );
    assert!(scratch.state().join("PAUSE").exists());

    // The floor is taken away (the stand-in for space coming back, whatever this disk holds): the pause lifts.
    health_config(&scratch, &base, "health: {min_free_gb: 0}\n");
    let (code, out, _) = health.run(&[]).await;
    assert_eq!(code, 0, "{out}");
    let report: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["pause"], "resumed", "{out}");
    assert_eq!(report["posted"], "recovered");
    assert!(!scratch.state().join("PAUSE").exists());
    assert!(
        solos_swarm::gateway::effective_control(&scratch.state(), now_ms() / 1_000).is_none(),
        "new work flows again"
    );
}
