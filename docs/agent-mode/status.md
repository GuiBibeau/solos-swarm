# Agent mode status

This page records what each phase of agent mode built, in which pull requests, and what is still to do. How the built
pieces work today is in the pages each phase links to. Every phase was one or more PRs, gated on green CI, swarm parity
(the same event kinds and `state.json` schema for the external readers), and a passing `solos-swarm smoke`.

## Phases

| Phase | State | PRs |
|---|---|---|
| [P0 Core and plan](#p0-core-and-plan) | Done | #18 |
| [P1 Harness seam](#p1-harness-seam) | Done | #19 |
| [P2a `run_goal`](#p2a-run_goal) | Done | #20 |
| [P2b One bwrap builder, Goal words](#p2b-one-bwrap-builder-goal-words) | Done | #30 |
| [P3a SQLite Store](#p3a-sqlite-store) | Done | #21 |
| [P3b `agent serve`](#p3b-agent-serve) | Done | #22 |
| [P3c Pi Router](#p3c-pi-router) | Done | #24 |
| [P3d Pi workers](#p3d-pi-workers) | Done | #26 |
| [P3e Background jobs](#p3e-background-jobs) | Done | #29 |
| [P4 Slack](#p4-slack) | Done | #23 |
| [P5 Swarm Tickets](#p5-swarm-tickets) | Done | #28 |
| [P6 Watchdog](#p6-watchdog) | Done | #27 |
| [P7a MCP server](#p7a-mcp-server) | Done | #31 |
| [P7b Claude Code workers](#p7b-claude-code-workers) | Done | #32 |
| [P8 GitHub and schedules](#p8-github-and-schedules) | Done | #25 |
| [P9 Remote nodes](#p9-remote-nodes) | Done | #33 |
| [P10 Benchmark](#p10-benchmark) | Done | #34, #37, #50 |
| [P11 Discord](#p11-discord) | Done | #35 |
| [P12 Vault and verifiable browser](#p12-vault-and-verifiable-browser) | First build done | #36, #38, #39, #43, #46, #47, #48, #49, #51 |
| [P13 pstack as the coding method](#p13-pstack-as-the-coding-method) | PRs 1 and 2 of 7 built | #53, #54, #57 |
| [P14 The agent's own GitHub App](#p14-the-agents-own-github-app) | PRs 1 and 2 of 6 built | #62, #63, #75 |
| [P15 Memory](#p15-memory) | PRs 1 to 4 of 5 built | #82, #84, #85, #86 |

## What each phase built

### P0 Core and plan

`crates/agent-core`: the domain, the ports, the Engine, an in-memory store, the allowlist Policy, 91 tests and a seeded
fuzzer. Most tests are regressions from the adversarial review and the swarm review of PR #18. Also ADR 0001 and this
plan. Swarm mode was untouched.

The Engine covered intake (idempotent, pass-through, multi-Thread), Evals (one per Thread, batched by strength), the
Router's verdicts (accept, ignore, stop), budget approval, Assignments, Reports, resumes, handbacks, retries, and
Policy changes during an Eval. See [The Engine](runtime.md#the-engine).

Open in P0: a Ticket whose Trigger names no reply channel, such as a schedule's, asks nobody when it needs a person. P8
let a schedule name its `reply_to`, and since P6 the Watchdog tells the operator when such a Ticket waits.

### P1 Harness seam

`src/harness.rs`: the `AgentHarness` port, `PiHarness` (the Pi launch moved out of `main.rs`, with its argv and
environment pinned by a golden test) and `harness::scripted::ScriptedHarness`. `run`, `work()`, `settled` and `rpc`
use the port. A new end-to-end test runs `work()` with three scripted agents and the `smoke::Scripted` provider through
the real broker, with no Pi, node or bwrap, on macOS too. Swarm runs were unchanged.

### P2a `run_goal`

`swarm::run_goal(GoalRun, harness, provider) -> GoalOutcome` in `src/swarm.rs` holds the run machinery that was in
`main.rs`, moved unchanged, and `run` keeps the CLI part. The end-to-end test now drives `run_goal`, so it also covers
the spawn loop, shutdown and the run's files. See [Running a Goal as a library](architecture.md#running-a-goal-as-a-library).
Swarm runs were unchanged.

### P2b One bwrap builder, Goal words

`src/bwrap.rs` builds the Pi launch, `run_code`'s sandbox and `doctor`'s probe. The golden tests of the Pi launch and the
swarm `run_code` command pass unedited, and a new test pins `doctor`'s probe. The research words of the nudges and the
fields of a host-filed record became optional Goal fields, `nudges` and `record_fields`, whose defaults are the old
values byte for byte. See [The sandbox builder](architecture.md#the-sandbox-builder) and
[Swarm Goal words](architecture.md#swarm-goal-words). This closed P2: P3b's design change settled the rest of its list
([Decisions along the way](#decisions-along-the-way)). Swarm runs were unchanged.

### P3a SQLite Store

`crates/agent-store` (`solos-agent-store`): `SqliteStore` implements `Repositories` on one SQLite file, with the durable
Effect queue and the hash-chained event log. See [The Store](architecture.md#the-store).

### P3b `agent serve`

`src/agent/`: the async shell ports `Source`, `Channel`, `Router` and `Executor`, a runtime with one task that owns the
Engine on the SQLite Store, and `solos-swarm agent serve --config agent.yaml --state-dir DIR [--until-idle]`. The
effect runner retries with backoff and keeps order per lane. The first adapters let it run with no model, Slack or
sandbox: `JsonlSource`, `JsonlChannel` (once per `MessageId`, safe after a crash), `ScriptedRouter` (it accepts
explicit requests as one session) and `ScriptedExecutor` (it reports `ready` at once). Tests cover a full run, a crash
between a Launch and its done mark, a restart, retries, a pause, and Thread context reaching the worker. The exit
criterion was the article's flow end to end on a laptop, with scripted Triggers. See
[The serving process](runtime.md#the-serving-process).

### P3c Pi Router

The Router became a long-lived Pi session behind the same `Router` port (`src/agent/pi_router.rs`), with batching and
recovery. It runs keyless in bwrap and calls the agent broker (`src/agent/broker.rs`), which holds and settles each
model call against the Router's daily cap in the Store's new spend ledger (schema v2), refuses calls while the state
directory is paused, and serves `router_decide` and `router_read`. The pricing, request contract, providers and pause
gate that both brokers use moved to `src/gateway/`, with swarm mode unchanged. #45 later hardened the Router against
long batches, hung calls and stale Eval ids. See [The Router](runtime.md#the-router) and
[The agent broker](architecture.md#the-agent-broker).

### P3d Pi workers

`src/agent/pi_executor.rs`: `PiExecutor` runs each `Plan::Session` Assignment as a keyless Pi worker session in bwrap,
in its Ticket's directory, with an optional `prepare` command and at most `max_workers` sessions at once. The agent
broker admits each worker as a `Worker` principal on its Ticket's account, so the Mandate budget caps every Assignment
of the Ticket together, and serves `worker_report`, `worker_handback`, `run_code` and `fetch_url`. A start ends what the
last run left without a worker ("host restarted") and relaunches working Tickets with `recovery`, through the new
`Engine::restart`. `run_code`'s mounts and scratch directory became configurable, with swarm runs' sandbox command
pinned unchanged. `Plan::Swarm` reported `failed` until P5. See [Workers](runtime.md#workers).

### P3e Background jobs

`src/agent/jobs.rs`: `job_start`, `job_wait` and `job_stop`, a `run_code` that moves to the background after
`background_after_secs`, and job ends that wake the worker as a Note (`Fact::Note`, applied as `Engine::note`). No core
change. See [Background jobs](runtime.md#background-jobs).

### P4 Slack

`src/agent/slack/`: `SlackSource` (Socket Mode) and `SlackChannel` (Web API), configured as `kind: slack`, with Approve
and Stop buttons. A budget above `auto_approve_microusd` became acceptable when a Slack channel and a `slack:` approver
are configured. New dependency: `tokio-tungstenite` 0.29.0, on the existing rustls. Tests run against a fake Slack,
including one `agent serve` end to end. See [Slack](channels.md#slack) and [Connect Slack](how-to/slack.md).

### P5 Swarm Tickets

`src/agent/swarm_executor.rs`: a `Plan::Swarm` Ticket runs one governed swarm per Assignment through
`swarm::run_goal_until`, from a Goal template (`src/agent/goal_template.rs`), charged on the Ticket's account
(`SqliteStore::charge`). A Stop cancels a run through the new cancel input of `run_goal_until`. `router_decide` takes
`goal_template`. See [Swarm Tickets](runtime.md#swarm-tickets) and
[Write a Goal template](how-to/goal-templates.md).

### P6 Watchdog

`src/agent/watchdog.rs`: a pure `assess(snapshot, now, thresholds)`, a loop in `agent serve` that tells the operator
each new alert once, the `watchdog_alerts` table (schema v3), the `retry_stalled` action, and
`solos-swarm agent status`. See [The Watchdog](runtime.md#the-watchdog) and
[Watch and control a running agent](how-to/operate.md).

### P7a MCP server

`solos-swarm mcp` (`src/mcp.rs`, `src/mcp/`): a stdio MCP server that offers one principal's tools through the agent
broker, or an operator's tools over a running agent through the new `kind: mcp` Source and Channel. The broker's tools
moved into one registry (`src/agent/broker/registry.rs`) served at `GET /tools`, and the Pi extension builds its tools
from `extensions/agent-tools.json`, which tests hold equal to the registry. No new dependencies. See
[The tool surface](architecture.md#the-tool-surface) and [Hand the agent work from an MCP host](how-to/mcp.md).

### P7b Claude Code workers

A session Ticket can run on Claude Code under the same governance as a Pi worker: the broker's Anthropic Messages
facade (`src/agent/broker/messages.rs`, `src/gateway/anthropic.rs`), `ClaudeCodeHarness`
(`src/harness/claude_code.rs`) and `solos-swarm bridge`. `executor.harness: claude-code` makes it the default, and a
Plan that names `pi` or `claude-code` picks one. With `executor.wire: anthropic`, the facade forwards the calls to
DeepSeek's own Messages endpoint (`src/gateway/forward.rs`) instead of translating them onto chat. Checked with Claude
Code 2.1.231 in `agent serve`, and the Anthropic wire against a fake upstream. No new dependencies. See
[Claude Code sessions](runtime.md#claude-code-sessions) and [Run workers on Claude Code](how-to/claude-code.md).

### P8 GitHub and schedules

`src/agent/github/`: `GithubSource` polls issues, pull requests, comments and workflow runs over REST, and
`GithubChannel` comments. `src/agent/schedule.rs`: `ScheduleSource` fires named slots as System Triggers, and a
schedule may name a `reply_to`. GitHub first authenticated with personal access token files, which the GitHub App
replaced in #63 (P14). No new dependencies. Tests run against a fake GitHub and a clock that the test moves, including
one `agent serve` end to end. See [GitHub](channels.md#github) and [Schedules](channels.md#schedules).

### P9 Remote nodes

`src/remote/`: `solos-swarm node serve` dials the agent's node listener over TLS 1.3 with pinned self-signed
certificates, and runs a worker's `run_code` and jobs in its own sandbox. The new `ExecutionEnvironment` port picks
this host or a node per Ticket. Core change: `Plan::Session` takes an optional `environment` (stored Plans read as
before), which `router_decide` may name and `executor.environment` defaults. Swarm runs were unchanged. See
[Remote nodes](runtime.md#remote-nodes) and [Run commands on your own machine](how-to/nodes.md).

### P10 Benchmark

`solos-swarm agent bench` (`src/agent/bench/`) reproduces the article's ablation with a seeded workload, simulated work
(`executor: {kind: simulated}`), and three designs. The one core change is `Engine::set_pass_through`. #37 made every
request unique, and #50 recorded the live run with a DeepSeek V4.1 Flash Router. See
[About the benchmark](benchmark.md) and [Run the benchmark](how-to/benchmark.md).

### P11 Discord

`src/agent/discord/`: `DiscordSource` holds one Gateway v10 connection, and `DiscordChannel` posts over REST,
configured as `kind: discord`, with Approve and Stop buttons. No new dependency. Tests run against a fake Discord: HTTP,
and a Gateway that keeps each session's events for a resume. Production runs on Discord. See [Discord](channels.md#discord)
and [Connect Discord](how-to/discord.md).

### P12 Vault and verifiable browser

Designed in #36, and built in eight PRs:

1. Capabilities in the tool registry (#38).
2. The sealed envelope, and the vault with its `solos-swarm vault` CLI (#43).
3. Approvals of vault items in the core, Slack and Discord (#47).
4. Keyed `fetch_url` and `vault_list` (#48).
5. The trail in the core, and schema V4 (#39).
6. The `browse` module (#46).
7. The browser tools (#49).
8. `solos-swarm agent trail` (#51).

A worker uses an API key it never sees, and browses with a headless Lightpanda whose every step is on the chain and
signed. [`vault-browser/`](vault-browser/rationale.md) holds the design, and [`prs.md`](vault-browser/prs.md) the plan.
Browsers on nodes and keys in the browser (PRs 9 and 10) come later. See
[Give workers an API key](how-to/vault.md), [Give workers a browser](how-to/browser.md) and
[The browser](runtime.md#the-browser).

### P13 pstack as the coding method

Designed in #53. [`pstack/`](pstack/rationale.md) holds the design: pstack vendored verbatim and pinned, entered by the
host, with a `read` tool, delegates that run as jobs on their own pool, repository workspaces cloned from a host mirror,
pull requests published from a sandbox-made bundle so no token reaches a model or sandbox, a `ready` gate on
host-recorded checks, cross-family review, and merges behind a click. PRs 3, 4 and 7 of
[`pstack/prs.md`](pstack/prs.md) are superseded by the GitHub App plan where they conflict.

- PR 1 (#54) puts pstack in every worker, with the vendored tree pinned by its `MANIFEST`, the entry into
  `poteto-mode`, the solos-swarm adapter, and the `read` tool.
- PR 2 (#57) gives a Ticket a repository. The Router names one of `forge.repos`, and the host clones it from its own
  mirror into the Ticket's `work/` before the worker starts. See [How repository Tickets work](repository-tickets.md).
- PRs 5 (delegates) and 6 (two model families) are not built.

### P14 The agent's own GitHub App

Designed in #62. [`github-app/`](github-app/rationale.md) holds the design: the agent works on GitHub as the bot of a
GitHub App that the owner installs on chosen repositories. One App key replaces both personal access tokens, and the
host mints installation tokens per repository and per read or write, never with the `workflows` permission. A pull
request is owed, not done: the worker commits and reports `ready`, and the host admits its commits from a sandbox-made
bundle and delivers the branch and the pull request as a durable Effect, with no new worker tool. Intake stays the
`github` source's REST polling.

- PR 1 (#63): the App replaces the tokens. See [Set up the GitHub App](how-to/github-app.md).
- PR 2 (#75): a worker's `ready` delivers a pull request. See
  [Delivering a pull request](repository-tickets.md#delivering-a-pull-request).
- PRs 3 to 6 of [`github-app/prs.md`](github-app/prs.md) are not built: a pass that reads every open pull request
  again every five minutes (`github.reconcile_secs`), CI failures that resume a Ticket within a bound
  (`max_repairs`), reviewers' comments and labels that ask, merges pinned to the head behind a Discord click or an
  approver's review (`merge`, `ci`), and `agent github init`. The config already accepts `merge`, `ci`, `max_repairs`
  and `reconcile_secs`, and nothing acts on them yet.

### P15 Memory

Designed in #82. [`memory/`](memory/design.md) holds the design, which applies Anthropic's "The components of agent
memory" to agent mode. The Store, the chain, the transcripts and each Ticket's `/work` already kept the agent's
experience, but nothing learned on one Ticket reached a later, different one, and a resumed Ticket's new worker knew
what the person said but not what it had answered. Per the owner's decisions, promotion is autonomous at the highest
thinking effort, with host-checked quotes and provenance, instead of waiting for a click.
[`memory/graph.json`](memory/graph.json) is the design's data.

- PR 1 (#82): a relaunched worker is told what it said. The first prompt of a relaunched Assignment quotes the
  Ticket's earlier `ready`, `blocked` and `failed` Reports and its latest `progress`, with no core or schema change. See
  [How the agent talks](channels.md#how-the-agent-talks).
- PR 2 (#84): `Memo`, a core aggregate in a table of `agent.db` (schema 8) on the hash chain, written by the owner with
  `agent memo` or the MCP memo tools, and shown as a quoted index in the Router's system prompt and each worker's first
  prompt, with `memos_shown` on the chain.
- PR 3 (#85): workers propose memos with `memo_propose`, quoting evidence the broker finds or refuses, and a separate
  review call at DeepSeek's highest thinking effort keeps, rewrites, merges or rejects each one. `/memos` and
  `router_read {kind: memo}` read them.
- PR 4 (#86): a daily dream Ticket checks memos against the day's transcripts (schema 9).
- PR 5, forgetting and review, is not built.

See [Memory](runtime.md#memory) and [Manage the agent's memory](how-to/memory.md).

## Changes outside the phases

| PR | Change | Where it is described |
|---|---|---|
| #45 | The Pi Router survives long batches, hung calls and stale Eval ids. | [The Router](runtime.md#the-router) |
| #52, #59 | The `verify-agent-mode` skill drives agent mode for real on remote-ai. | `.claude/skills/verify-agent-mode/` |
| #55, #56, #58 | Refactors: the Engine decides who may approve or stop, one module decides what sessions can see, and a worker tool is one bound entry held per seat. | [The tool surface](architecture.md#the-tool-surface) |
| #60 | Agent mode in production as a systemd user unit, with `scripts/agent-deploy`. | [production.md](production.md) |
| #65 | A looping worker is refused, and a failed model call that ends a turn is told to the person. | [Workers](runtime.md#workers) |
| #67 | The agent's replies read like a person's. | [How the agent talks](channels.md#how-the-agent-talks) |
| #68 | Every budget is optional. | [Budgets](concepts.md#budgets) |
| #69 | Replies link to the task page, and read well in Discord and Slack. | [How the agent talks](channels.md#how-the-agent-talks) |
| #70, #71, #80 | The web dashboard and its publisher, `agent publish`. | [production.md](production.md#dashboard) |
| #72 | A worker's plain-text answer is told to the person. | [When a worker answers in plain text](runtime.md#when-a-worker-answers-in-plain-text) |
| #73 | A swarm Goal's `fetch` policy is enforced. | [Write a Goal template](how-to/goal-templates.md#limit-what-its-workers-fetch) |
| #74, #76 | `agent health`, `agent prune` with a disk floor, `agent stop`, and a drain on deploy. | [Watch and control a running agent](how-to/operate.md) |
| #77, #78 | The loop guard stops a worker that ignores refusals or makes no progress, and workers sample at 0.6. | [The loop guard](runtime.md#the-loop-guard) |
| #81 | `fetch_url` refuses private, loopback and metadata addresses. | [What fetch_url can reach](runtime.md#what-fetch_url-can-reach) |
| #83 | A repository's dependencies are installed on the host and mounted read-only. | [Installed dependencies](repository-tickets.md#installed-dependencies) |

## Later steps

None of these is scheduled. Each names the phase that recorded it.

- P3c: `router_note`, and an outage pause in the agent broker.
- P4: a per-channel admission table, and operator DMs that carry status and pause.
- P5: new Input for a running swarm. Today a follow-up waits for the Ticket's next Assignment.
- P6: model-based drift judgement in the Watchdog, beyond a worker's silence.
- P6: the provider outage streak, and Evals that keep expiring before they turn `unavailable`, as signals. A pause as
  an action. Operator DMs that answer "what is running and why" with the status report.
- P7: a Codex harness, `CodexHarness`. Codex 0.145.0 refuses `wire_api = "chat"` ("no longer supported"), so it needs a Responses
  facade, `POST /v1/responses`, translated in `src/gateway/` beside the Messages one and answered as its SSE events
  (`response.created`, `response.output_item.added`, `response.output_text.delta`,
  `response.function_call_arguments.delta`, `response.output_item.done`, and `response.completed` with usage). It also
  needs a `model_providers` entry whose `base_url` is the bridge and whose `env_key` holds the token,
  `mcp_servers.swarm` running `solos-swarm mcp`, and `codex exec --json` events mapped to `SessionEvent`. A harness on
  the older chat wire (OpenCode, or Codex before it dropped it) would instead need `/v1/chat/completions` with
  `stream: true` answered as `chat.completion.chunk` deltas and `data: [DONE]`, close to a pass-through, since the
  upstream already speaks it. A DeepSeek harness follows the same pattern.
- P7: the Router on Claude Code or another harness, and a `harness` argument to `router_decide`, so the Router can pick
  a worker's harness per Ticket.
- P7: `swarm_run`, `ticket_*` and the ledger as host tools, so another harness's session can call a governed swarm.
- P7a: approvals over MCP. An MCP operator's budget approvals still take Slack or Discord buttons.
- P8: pull request review events, other CI apps' check runs and commit statuses, and webhooks for lower latency. PRs 3
  and 4 of [`github-app/prs.md`](github-app/prs.md) plan the review and CI events.
- P9 and the security model: per-node capability leases (the `mvp` `DispatchPermit` design) that bound what the agent
  may run on each node.
- P9: scoped tool credentials (`tool_identity.rs`) instead of raw node secrets, an immediate retry when a node
  reconnects instead of the next backoff step, and the live nodes in `agent status`.
- P10: the delegation-prompt variant, and judging whether the Router's decisions were right.
- P11: backfilling the messages sent while the agent process was down (`GET /channels/{id}/messages`), a repeat check
  that looks past the few minutes Discord keeps a `nonce`, and sharding for a bot in more than 2,500 servers.
- P12: browsers on nodes, and keys in the browser (PRs 9 and 10 of [`vault-browser/prs.md`](vault-browser/prs.md)).
- P13: PRs 5 and 6 of [`pstack/prs.md`](pstack/prs.md).
- P14: PRs 3 to 6 of [`github-app/prs.md`](github-app/prs.md).
- P15: PR 5 of [`memory/design.md`](memory/design.md), forgetting and review.
- Ports: bounded recall through `Source::context(trigger, part)`.

## Decisions along the way

**The agent-mode store** is SQLite (`rusqlite`, bundled), built in P3a. A JSON image like `state.json` does not scale to
months of Triggers.

**The Router model** should be the cheapest model that judges reliably, since the article uses a lighter model for the
Router. The live benchmark ran a DeepSeek V4.1 Flash (`deepseek-flash`) Router, and production runs one. Whether it
judges reliably is not measured yet (a P10 later step).

**The Slack app mode** is Socket Mode, built in P4, so there is no public endpoint.

**The `mvp` stack** is frozen and harvested as adapters (P7, P9). Deleting it is a separate decision. P9 did not harvest
the `mvp` QUIC transport or SPIFFE identities: one TLS connection per node with pinned self-signed certificates needs
no CA and no issuance service.

**Agent mode keeps its own accounts (P3b).** Agent mode keeps its own accounts, one per Ticket plus the Router's, in the
agent Store. It shares the swarm code that really is common (pricing, the provider call, the pause gate, `fetch`,
`run_code`) instead of generalizing swarm mode's Goal-shaped broker `Store`. P2b and a planned P2c shrank to extracting
those pieces when P3c and P3d needed them. P3c extracted pricing, the provider call and the pause gate, and P3d
extracted `run_code`'s mounts and scratch directory.

**What that change settled instead of building:** `EventLog` split from `Store`, a ledger with accounts (Goal, Task,
Ticket, Router), broker auth by principal instead of `agent-NN`, and one `ModelUpstream` trait. Agent mode keeps its own
accounts, principals and hash-chained event log in the agent Store, and both brokers share the providers, pricing and
pause gate in `src/gateway/`. Swarm mode's broker `Store` stays as it is, and P2 closed without splitting its pure parts
from its HTTP and file code.
