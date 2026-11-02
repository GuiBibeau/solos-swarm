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
- Agent mode, P3a: workspace crate `crates/agent-store` (`solos-agent-store`), the SQLite Store for agent-core
  (`rusqlite` 0.41.1, bundled SQLite, WAL). It implements `Repositories` with one transaction per commit, and adds a
  durable Effect queue (due oldest first, done, failed with an attempt count and a retry time, Sends kept in commit
  order per Channel address, every call safe to repeat after a crash) and an event log hash-chained like
  `events.jsonl`, with `verify_chain()`. A seeded differential walk checks it against `MemoryRepositories`.
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
