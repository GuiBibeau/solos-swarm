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
- Agent mode, P3c: a Pi Router. `router: {kind: pi, ...}` in `agent.yaml` and `agent serve --pi-root --node
  --credential-file` run the Router as one long-lived, keyless, bwrap-sandboxed Pi session that judges batched
  Candidate cards through `router_decide` and `router_read` on a new agent broker (`src/agent/broker.rs`). The
  broker holds and settles every Router model call against `budgets.router_daily_microusd` in a `spend` table
  (agent store schema v2), so the cap survives a restart, and refuses calls while the state dir is paused, which
  also holds the agent's Effects. A Router accept above `policy.auto_approve_microusd` stands only when a Slack
  channel and a `slack:` approver can take the approval its Ticket then waits for; otherwise it is refused.
  `SWARM_TOOLSET=router` selects the Router's tools in `extensions/broker.ts`; unset keeps the swarm set. Pricing,
  the upstream request, the providers and the pause gate moved to `src/gateway/` and swarm mode calls them unchanged.
- Agent mode, P8: GitHub and schedules (`kind: github` source and channel, `kind: schedule` source). The GitHub Source
  polls the listed repositories' issues, pull requests, comments, review comments and failed workflow runs (CI as bot
  Triggers on the pull request's Thread) with ETags, rate-limit waits and an atomic cursor in the state directory; the
  Channel comments with a hidden marker, so a crash never posts twice. Schedules fire named slots (`every`, `daily_at`
  UTC) as System Triggers a restart cannot repeat, with an optional `reply_to`. No new dependencies.
- Agent mode, P3d: Pi workers. `executor: {kind: pi, ...}` runs each session Ticket on a keyless, bwrap-sandboxed Pi
  worker in its own directory under the state dir (optional prepare command, `max_workers` at once), admitted by the
  agent broker as a Worker on the Ticket's account with `worker_report`, `worker_handback`, `run_code` and
  `fetch_url` (`SWARM_TOOLSET=worker`); a start relaunches the work the last run left running with `recovery` set
  (new `Engine::restart`). `run_code` takes configured mounts and a caller-owned scratch directory; swarm runs keep
  their exact sandbox command.
- Agent mode, P6: the Watchdog (`src/agent/watchdog.rs`, `watchdog:` in agent.yaml). A pure assessment of the Store
  (Tickets waiting for a person, stalled Assignments, failing Effects, `unavailable` Decisions, spend near a cap,
  long pauses) runs inside `agent serve` and tells the operator's conversation each new alert once, recorded in a
  `watchdog_alerts` table (agent store schema v3) so a restart repeats none; `retry_stalled` retries a stalled
  Assignment once per Ticket through the runtime. `solos-swarm agent status` reports what is running and why from a
  read-only Store, as text or `--json`.
- Agent mode, P5: swarm Tickets. `executor.swarm` runs each `Plan::Swarm` Assignment as one governed swarm through
  the new `swarm::run_goal_until` (a cancel input beside the signals `run_goal` keeps), with a Goal rendered from a
  YAML template under what the Ticket has left to spend and checked by `Goal::validate`, at most `max_runs` at once,
  its spend charged on the Ticket's account, a Stop that cancels it cleanly, and a restart that relaunches it in a
  new run directory; the Router learns when a swarm pays for itself. `solos-swarm run` is unchanged.
- Agent mode, P3e: background jobs for workers (`src/agent/jobs.rs`). `job_start`, `job_wait` and `job_stop` run a
  command in the Ticket's sandbox without blocking the turn, and a `run_code` still running after 5 s moves to the
  background; a job's end reaches the worker once, in its turn or as a Note that wakes it, and the job dies with its
  Assignment. Limits under `executor.run_code` (`background_after_secs`, `max_jobs`, `job_max_secs`,
  `job_output_bytes`); swarm `run_code` is unchanged.
- Agent mode, P2b, which closes P2: one bwrap builder (`src/bwrap.rs`) for the Pi launch, `run_code`'s sandbox and
  `doctor`'s probe, with the same argument vectors (the golden tests pass unedited; the frozen mvp sandboxes keep
  their own lines). The research words in the swarm runtime are now optional Goal fields: `nudges`, one template
  per escalation stage, and `record_fields`, the Task fields a host-filed record reads. `Goal::validate` checks
  them, a Goal template can set them, and a Goal without them runs exactly as before.
- Agent mode, P7a: `solos-swarm mcp`, a hand-written stdio MCP server. `--socket --token-file` offers one principal's
  broker tools (the broker's new `GET /tools`, from a Rust tool registry the Pi extension's schemas are tested
  against), with refusals as `isError` results and pauses waited out; `--state-dir --operator` offers `agent_status`,
  `agent_request` (a Trigger from `mcp:<operator>` through the new `kind: mcp` Source) and `agent_replies` (the
  `kind: mcp` Channel's outbox).
- Agent mode, P7b: Claude Code workers. `executor.harness: claude-code` (or a Plan naming it) runs a session Ticket
  on the installed `claude`, headless in bwrap with no network and no key, its only tools the broker's through
  `solos-swarm mcp`, and its model the agent broker's new Anthropic Messages facade (`POST /v1/messages`, SSE too,
  translated in `src/gateway/anthropic.rs`), held and settled on the Ticket's account and refused while paused,
  through a loopback port that the hidden `solos-swarm bridge` pipes to the broker socket.
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
