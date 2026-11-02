# Changelog

Pull requests merged to `main` after the minimal reference (#1-#5). Dates are merge dates.

## Unreleased

### Added
- Agent mode, P0: workspace crate `crates/agent-core` (pure domain, ports and Engine for Triggers, Threads, Evals,
  Tickets and Assignments, with `Plan::Session` or `Plan::Swarm`), ADR 0001 and the plan in `docs/agent-mode/`.
  Root lints move to `[workspace.lints]`; `cargo test`/`clippy` cover both crates. Swarm mode is unchanged.
- Agent mode, P1: the `AgentHarness` port in `src/harness.rs` (`SessionSpec`, `SessionEvent`, `Session`). `PiHarness`
  is the Pi launch moved out of `main.rs` unchanged: same argv and env (pinned by a golden test), transcript and stderr
  files, caps, messages, Store events and shutdown accounting. `harness::scripted::ScriptedHarness` plays scripted
  tool calls against the real broker, and a new end-to-end test runs `work()` with three scripted agents and
  `smoke::Scripted`.
- Agent mode, P2a: `swarm::run_goal(GoalRun, harness, provider) -> GoalOutcome` in `src/swarm.rs`, so agent mode can
  run a swarm as a library call. The run machinery moved out of `main.rs` unchanged (spawn loop, `work`, nudges,
  deadline, broker socket, shutdown); `solos-swarm run` is a thin wrapper with the same run-dir files, Store events,
  stdout line and exit code. The end-to-end test drives `run_goal` with `ScriptedHarness` and also checks the spawn
  loop, shutdown, `cleanup.json`, `runtime-report.json` and a deadline stop.
- Agent mode, P3a: workspace crate `crates/agent-store` (`solos-agent-store`), the SQLite Store for agent-core
  (`rusqlite` 0.41.1, bundled SQLite, WAL). It implements `Repositories` with one transaction per commit, and adds a
  durable Effect queue (due oldest first, done, failed with an attempt count and a retry time, Sends kept in commit
  order per Channel address and Launch, Deliver and Stop per Ticket, every call safe to repeat after a crash) and an event log hash-chained like
  `events.jsonl`, with `verify_chain()`. A seeded differential walk checks it against `MemoryRepositories`.
- Agent mode, P3b: `solos-swarm agent serve --config agent.yaml --state-dir DIR [--until-idle]` (`src/agent/`): the
  `Source`, `Channel`, `Router` and `Executor` ports, a single-writer runtime over the Engine and the SQLite Store
  (Effects retried with exponential backoff, all held while paused), and the jsonl and scripted adapters, so the
  agent runs with no model, Slack or sandbox.
- Agent mode, P4: Slack (`src/agent/slack/`, `kind: slack` source and channel). A Socket Mode Source (outbound
  WebSocket, every envelope acknowledged, reconnect with backoff) and a Web API Channel that replies in the thread,
  never posts a message twice after a crash, and offers Approve and Stop buttons that the runtime applies as
  `approve` and `stop_ticket`; `tokio-tungstenite` 0.29.0 on the existing rustls.
- Agent mode, P8: GitHub and schedules (`kind: github` source and channel, `kind: schedule` source). The GitHub Source
  polls the listed repositories' issues, pull requests, comments, review comments and failed workflow runs (CI as bot
  Triggers on the pull request's Thread) with ETags, rate-limit waits and an atomic cursor in the state directory; the
  Channel comments with a hidden marker, so a crash never posts twice. Schedules fire named slots (`every`, `daily_at`
  UTC) as System Triggers a restart cannot repeat, with an optional `reply_to`. No new dependencies.
- `collect` leaves the per-run `code/` scratch directory on the host and writes `collect-manifest.json`; `--include-code` opts back in.
- Docs: pause mode, self-update operations, dashboard tunnel, tournament runbook, replay status.

## 2026-10-28

- **#14 Self-update channels, drain, doctor and outage pause.** `stable`/`canary` pointers to immutable candidates with
  an atomic switch, `candidates list|current|promote|rollback|prune`, a promotion gate (integrity, doctor, `smoke`,
  idle probe), `drain`/`pause`/`unpause` through `control.json`, `Store::resume` and `resume-plan`, `remote.py doctor`,
  and an outage pause after five consecutive provider failures. `run --resume` is not implemented. See
  [docs/self-update-operations.md](docs/self-update-operations.md).
- **#13 Rust style.** rustfmt and clippy `-D warnings` gates in CI, lint set, `// SAFETY:` comments on unsafe blocks.
  See [docs/rust-style.md](docs/rust-style.md).
- **#12 Pause mode.** Broker refuses model calls with a 503 pause reply in weekday peak windows
  (`SWARM_REFUSE_PEAK=1`) or while a `PAUSE` file exists; Pi waits instead of erroring; the Goal deadline counts only
  unpaused time. See [docs/pause-mode.md](docs/pause-mode.md).
- **#11 Replay loop follow-ups.** Tournament evidence ingester, `gen.py` policy contract patch, worked example.
- **#10 Replay-scored policy loop.** `replay/`: history schema, simulator, guard, proposer, dashboard tunnel helper
  and systemd template unit. See [replay/README.md](replay/README.md) and [docs/dashboard.md](docs/dashboard.md).
- **#9** Merge of the local-VM swarm branch into `main` (cache-aware pricing and the 200k context-window fix).

## 2026-10-27

- **#8 Cache-aware pricing.** Reported cache hits are priced separately from uncached input; the ledger holds at
  conservative cache-miss prices and settles real hits at the cache price.
- **#7 Spatial dashboard and runtime telemetry; #6 monochrome dashboard.** Read-only dashboard that follows a run's
  state file.
