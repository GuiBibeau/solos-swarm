# Changelog

Pull requests merged to `main` after the minimal reference (#1-#5). Dates are merge dates.

## Unreleased

### Added
- Agent mode, P0 (#18): workspace crate `crates/agent-core` (pure domain, ports and Engine for Triggers, Threads, Evals,
  Tickets and Assignments, with `Plan::Session` or `Plan::Swarm`), ADR 0001 and the plan in `docs/agent-mode/`.
  Root lints move to `[workspace.lints]`; `cargo test`/`clippy` cover both crates. Swarm mode is unchanged.
- Agent mode, P1 (#19): the `AgentHarness` port in `src/harness.rs` (`SessionSpec`, `SessionEvent`, `Session`). `PiHarness`
  is the Pi launch moved out of `main.rs` unchanged: same argv and env (pinned by a golden test), transcript and stderr
  files, caps, messages, Store events and shutdown accounting. `harness::scripted::ScriptedHarness` plays scripted
  tool calls against the real broker, and a new end-to-end test runs `work()` with three scripted agents and
  `smoke::Scripted`.
- Agent mode, P2a (#20): `swarm::run_goal(GoalRun, harness, provider) -> GoalOutcome` in `src/swarm.rs`, so agent mode can
  run a swarm as a library call. The run machinery moved out of `main.rs` unchanged (spawn loop, `work`, nudges,
  deadline, broker socket, shutdown); `solos-swarm run` is a thin wrapper with the same run-dir files, Store events,
  stdout line and exit code. The end-to-end test drives `run_goal` with `ScriptedHarness` and also checks the spawn
  loop, shutdown, `cleanup.json`, `runtime-report.json` and a deadline stop.
- Agent mode, P3a (#21): workspace crate `crates/agent-store` (`solos-agent-store`), the SQLite Store for agent-core
  (`rusqlite` 0.41.1, bundled SQLite, WAL). It implements `Repositories` with one transaction per commit, and adds a
  durable Effect queue (due oldest first, done, failed with an attempt count and a retry time, Sends kept in commit
  order per Channel address and Launch, Deliver and Stop per Ticket, every call safe to repeat after a crash) and an event log hash-chained like
  `events.jsonl`, with `verify_chain()`. A seeded differential walk checks it against `MemoryRepositories`.
- Agent mode, P3b (#22): `solos-swarm agent serve --config agent.yaml --state-dir DIR [--until-idle]` (`src/agent/`): the
  `Source`, `Channel`, `Router` and `Executor` ports, a single-writer runtime over the Engine and the SQLite Store
  (Effects retried with exponential backoff, all held while paused), and the jsonl and scripted adapters, so the
  agent runs with no model, Slack or sandbox.
- Agent mode, P4 (#23): Slack (`src/agent/slack/`, `kind: slack` source and channel). A Socket Mode Source (outbound
  WebSocket, every envelope acknowledged, reconnect with backoff) and a Web API Channel that replies in the thread,
  never posts a message twice after a crash, and offers Approve and Stop buttons that the runtime applies as
  `approve` and `stop_ticket`; `tokio-tungstenite` 0.29.0 on the existing rustls.
- Agent mode, P11 (#35): Discord (`src/agent/discord/`, `kind: discord` source and channel). A Gateway v10 Source (outbound WebSocket, heartbeats with zombie detection, resume after a drop, op 7 or a resumable invalid session, fresh identify when the session is gone, fatal close codes that name the fix such as Message Content Intent) and a REST Channel that replies to the message, never posts a message twice within Discord's nonce window, and offers Approve and Stop buttons that feed the same commands Slack's do. A thread is one id, a message or a Discord thread. No new dependencies.
- Agent mode, P3c (#24): a Pi Router. `router: {kind: pi, ...}` in `agent.yaml` and `agent serve --pi-root --node
  --credential-file` run the Router as one long-lived, keyless, bwrap-sandboxed Pi session that judges batched
  Candidate cards through `router_decide` and `router_read` on a new agent broker (`src/agent/broker.rs`). The
  broker holds and settles every Router model call against `budgets.router_daily_microusd` in a `spend` table
  (agent store schema v2), so the cap survives a restart, and refuses calls while the state dir is paused, which
  also holds the agent's Effects. A Router accept above `policy.auto_approve_microusd` stands only when a Slack
  channel and a `slack:` approver can take the approval its Ticket then waits for; otherwise it is refused.
  `SWARM_TOOLSET=router` selects the Router's tools in `extensions/broker.ts`; unset keeps the swarm set. Pricing,
  the upstream request, the providers and the pause gate moved to `src/gateway/` and swarm mode calls them unchanged.
- Agent mode, P8 (#25): GitHub and schedules (`kind: github` source and channel, `kind: schedule` source). The GitHub Source
  polls the listed repositories' issues, pull requests, comments, review comments and failed workflow runs (CI as bot
  Triggers on the pull request's Thread) with ETags, rate-limit waits and an atomic cursor in the state directory; the
  Channel comments with a hidden marker, so a crash never posts twice. Schedules fire named slots (`every`, `daily_at`
  UTC) as System Triggers a restart cannot repeat, with an optional `reply_to`. No new dependencies.
- Agent mode, P3d (#26): Pi workers. `executor: {kind: pi, ...}` runs each session Ticket on a keyless, bwrap-sandboxed Pi
  worker in its own directory under the state dir (optional prepare command, `max_workers` at once), admitted by the
  agent broker as a Worker on the Ticket's account with `worker_report`, `worker_handback`, `run_code` and
  `fetch_url` (`SWARM_TOOLSET=worker`); a start relaunches the work the last run left running with `recovery` set
  (new `Engine::restart`). `run_code` takes configured mounts and a caller-owned scratch directory; swarm runs keep
  their exact sandbox command.
- Agent mode, P6 (#27): the Watchdog (`src/agent/watchdog.rs`, `watchdog:` in agent.yaml). A pure assessment of the Store
  (Tickets waiting for a person, stalled Assignments, failing Effects, `unavailable` Decisions, spend near a cap,
  long pauses) runs inside `agent serve` and tells the operator's conversation each new alert once, recorded in a
  `watchdog_alerts` table (agent store schema v3) so a restart repeats none; `retry_stalled` retries a stalled
  Assignment once per Ticket through the runtime. `solos-swarm agent status` reports what is running and why from a
  read-only Store, as text or `--json`.
- Agent mode, P5 (#28): swarm Tickets. `executor.swarm` runs each `Plan::Swarm` Assignment as one governed swarm through
  the new `swarm::run_goal_until` (a cancel input beside the signals `run_goal` keeps), with a Goal rendered from a
  YAML template under what the Ticket has left to spend and checked by `Goal::validate`, at most `max_runs` at once,
  its spend charged on the Ticket's account, a Stop that cancels it cleanly, and a restart that relaunches it in a
  new run directory; the Router learns when a swarm pays for itself. `solos-swarm run` is unchanged.
- Agent mode, P3e (#29): background jobs for workers (`src/agent/jobs.rs`). `job_start`, `job_wait` and `job_stop` run a
  command in the Ticket's sandbox without blocking the turn, and a `run_code` still running after 5 s moves to the
  background; a job's end reaches the worker once, in its turn or as a Note that wakes it, and the job dies with its
  Assignment. Limits under `executor.run_code` (`background_after_secs`, `max_jobs`, `job_max_secs`,
  `job_output_bytes`); swarm `run_code` is unchanged.
- Agent mode, P2b (#30), which closes P2: one bwrap builder (`src/bwrap.rs`) for the Pi launch, `run_code`'s sandbox and
  `doctor`'s probe, with the same argument vectors (the golden tests pass unedited; the frozen mvp sandboxes keep
  their own lines). The research words in the swarm runtime are now optional Goal fields: `nudges`, one template
  per escalation stage, and `record_fields`, the Task fields a host-filed record reads. `Goal::validate` checks
  them, a Goal template can set them, and a Goal without them runs exactly as before.
- Agent mode, P7a (#31): `solos-swarm mcp`, a hand-written stdio MCP server. `--socket --token-file` offers one principal's
  broker tools (the broker's new `GET /tools`, from a Rust tool registry the Pi extension's schemas are tested
  against), with refusals as `isError` results and pauses waited out; `--state-dir --operator` offers `agent_status`,
  `agent_request` (a Trigger from `mcp:<operator>` through the new `kind: mcp` Source) and `agent_replies` (the
  `kind: mcp` Channel's outbox).
- Agent mode, P7b (#32): Claude Code workers. `executor.harness: claude-code` (or a Plan naming it) runs a session Ticket
  on the installed `claude`, headless in bwrap with no network and no key, its only tools the broker's through
  `solos-swarm mcp`, and its model the agent broker's new Anthropic Messages facade (`POST /v1/messages`, SSE too,
  translated in `src/gateway/anthropic.rs`), held and settled on the Ticket's account and refused while paused,
  through a loopback port that the hidden `solos-swarm bridge` pipes to the broker socket.
- Agent mode, P9 (#33): remote nodes (`src/remote/`, `src/agent/environment.rs`). `solos-swarm node serve` dials the
  agent over TLS where each side pins the other's certificate (`node init`, `agent init-nodes`), and runs the
  `run_code` and background jobs of the Tickets bound to it in its own bwrap sandbox, with its own mounts and
  secrets that never leave it; `Plan::Session` gains an optional `environment` (stored Plans read as before), an
  offline node holds its Tickets' launches until it connects, and swarm mode is unchanged.
- Agent mode, P10 (#34): `solos-swarm agent bench` reproduces the super-agent article's ablation on a seeded stream of chat
  and CI Triggers with work requests (`agent workload` writes one), simulated work (`executor: {kind: simulated}`)
  and three designs (`super-agent`, `no-pass-through` through the new `pass_through: false` and
  `Engine::set_pass_through`, and `serial`), writing `report.json` and `report.md`; `--router pi` refuses to start
  without `--max-spend-usd` and caps each run's Router account.
- Agent mode, vault and browser, PR 1 of the design in #36 (#38): capabilities live in the tool registry
  (`src/agent/broker/registry.rs`). `ToolSpec.needs` is a set of capabilities (`run_code`, `browser`, `vault`), a tool
  is offered and callable only when its seat has all of them, and `ROUTER_TOOLS`, `WORKER_TOOLS`, a worker's grants,
  `GET /tools` and the refusal in `/tool` all read the registry, which replaces `job_host`'s own check. The three
  sandbox builders bind `bwrap::SYSTEM_BINDS`. No change in behavior: every tool-list test and golden argv test
  passes unedited.
- Agent mode, the vault (#43, PR 2 of PR #36's design): `src/vault.rs` and `src/vault/` keep one sealed file of logins
  and API keys per machine, in the AES-256-GCM envelope that moved from `src/artifacts.rs` to `src/sealed.rs` with
  its bytes unchanged. A value leaves the vault only inside an allowed admission, as a login fill or a key header.
  `solos-swarm vault init | add-login | add-key | list | remove` reads `vault: {path, key_file}` from agent.yaml or
  node.yaml and prints no value. Nothing in agent mode used the vault yet.
- Agent mode, the trail (#39, first step of the verifiable browser, PR #36's design): `agent-core` gains `trail`
  (`TrailAction`, `StepBody`, `SignedStep` and the `Digest`, `TrailKey` and `Signature` hex newtypes, with no crypto
  type) and `Engine::{trail_ask, trail_done}`, which write `TrailAsked` before a browser acts and `TrailDone` after,
  one pending step per Assignment, numbered by the core; a `Close` is admitted after its Assignment or Ticket ends. The
  store's schema is v4 (a `trails` table, one row per Assignment), so an older build refuses the file instead of
  failing on an event it cannot read, and `Fact::{TrailAsk, TrailDone}` reach the Engine through the runtime. Nothing
  writes a step yet.
- Agent mode, the browser module (#46, PR 6 of PR #36's design): `src/browse/` runs one Lightpanda per Assignment in a
  bwrap sandbox, the only caller of the new `Bwrap::share_net`, and drives it over CDP on a Unix socket. Each answer
  is scrubbed of vault values, digested and signed with this machine's Ed25519 trail key, and `verify_step` checks it.
  A login fills only a password input in the main document of one of its sites, after an origin check.
  `solos-swarm browser init-key` writes the trail key. No broker tool called it yet.
- Agent mode, approvals of vault items (#47, PR #36's design): a Ticket holds a grant per item (`SecretId`, `local/github-gui`;
  `SecretGrant` Waiting, Granted or Denied), and `Engine::{ask_secret, answer_secret, use_key}` ask a person once per
  Ticket, take only a click from the person the item names (or a Policy approver when it names none), and record a keyed
  use before it leaves (`SecretAsked`, `SecretAnswered`, `KeyUsed`). The answer reaches the worker as
  `InputBody::Secret`; typed "allow" is only a Trigger and grants nothing. A message now says what it asks
  (`OutboundMessage.ask`, `Ask::{Budget, Secret}`) and the runtime keeps the ask only while it stands, so the budget
  buttons no longer depend on matching attention text. Slack and Discord draw Allow and Deny for a vault item beside
  Approve and Stop, and `Fact::{SecretAsk, KeyUsed}` reach the Engine through the runtime. The store's schema is v5 (no
  new table), so an older build refuses a file that can hold grants; stored Tickets and messages read back unchanged.
  Nothing asks yet.
- Agent mode, keyed `fetch_url` and `vault_list` (#48, PR #36's design): with `vault:` in agent.yaml, `agent serve` opens
  the vault, and refuses to start when it or its key sits under the Tickets' directory, `--pi-root` or `--node`'s
  install. A worker's `fetch_url` takes `key`: the broker writes `KeyUsed` to the chain first, then sends the key as
  its header only on hops whose own URL is one of the key's hosts (`broker::fetch_with`), over a client that ignores
  the proxy variables, and scrubs the response and any error of every vault value. A first use that needs a person
  asks once in the thread and answers 409 until the click comes back as an Input. `vault_list` (only with a vault)
  lists the keys and logins a Ticket may use, without values, and where the Ticket stands with each. Swarm mode's
  `fetch_url` is unchanged.
- Agent mode, the browser tools (#49, PR #36's design): with `browser:` in agent.yaml, `agent serve` pins this host's trail
  key, opens `Browsers` and a trail blob store under the state directory, and refuses to start when the key, the
  blobs or the browser sockets sit where a sandbox can read them. Workers get `browser_open`, `browser_act`,
  `browser_read` and, with a vault, `browser_login`. Each step is `TrailAsked` on the chain before the browser acts
  and `TrailDone` after, with the step the browser signed, checked under the pinned key (`lost` when it does not
  verify). Its snapshot, View and typed text are kept by digest, capped at 8 GiB and 256 MiB per Ticket, and the
  model gets the View's exact bytes. An Assignment's steps run one at a time, every asked step is answered even when
  the session hangs up, and the browser closes with a signed step when its Assignment ends, after `idle_secs`, or as
  the agent stops, before the runtime does. A first use of a login or key is refused at once on a Ticket whose
  channels cannot show the Allow and Deny buttons. The worker's system prompt and the Router's environment note tell
  of the browser.
- Agent mode, `solos-swarm agent trail` (#51, PR #36's design, the last PR of its first build): it opens the Store
  read-only, checks the event chain, then walks each Ticket's browser steps from the events, so a schema 3 or 4 file
  reads too. Each signed step must verify under the key trusted for its environment (the public half of
  `browser.audit_key`, or `--trust <environment>=ed25519:<hex>`, so a verifier elsewhere needs no secret), answer the
  step asked under its number, and name blobs that are in the trail store and still hash to their digests. Each
  browser session must count its steps with no gap, name its build on its first step, and end with a signed close
  once its Assignment ends. It prints each step's time, action, result, URL, status, title and digests.
  `--step N --show view` prints the exact bytes the model got, and `--json` prints the report. It exits 1 on any
  problem. An Assignment's last step asked and never answered, and a lost answer, are warnings that `--strict`
  counts too. `browse::verify_signature` is the signature half of `verify_step`, which now calls it.
- `verify-agent-mode` (#52): `.claude/skills/verify-agent-mode/`, a skill that ships the checkout to the remote box,
  starts `agent serve` with a real DeepSeek Router and Pi workers, drives it through `solos-swarm mcp --operator`,
  and keeps the evidence (chain, effects, spend, replies, `agent trail`). Its `features/` hold one recipe per feature.
- Agent mode, pstack PR 1 (#54, design in #53): with `pstack:` in agent.yaml, every worker starts in poteto-mode.
  `vendor/pstack/` holds pstack 0.15.36 verbatim, pinned by a `MANIFEST` that `scripts/pstack-sync` writes, and
  `agent serve` refuses to start when one byte differs. The system prompt gains the solos-swarm adapter
  (`docs/harness/solos-swarm.md`), and a new worker tool, `read`, reads files under `/pstack`, confined by the kernel
  (`openat2` with `RESOLVE_BENEATH`). Off by default.
- Agent mode, pstack PR 2 (#57): repository workspaces. `forge:` in agent.yaml lists repositories (`kind: github`
  with a token file read once, or `kind: local` bare repositories for dry runs). The Router names a repository when it
  accepts work on it; the host keeps a mirror per repository and clones each repository Ticket's `work/` from it
  (`--no-local`, no hooks, fsynced before `host/repo.json` is written), recovering from a killed fetch's stale locks.
  The worker reads the clone with `read` (`/work`, `/host`) and commits in it with `run_code`; nothing is pushed yet.
  The Pi sandbox binds `/pstack` only for the checked tree of a configured `pstack:`.
- Agent mode, refactors from an architecture review: the Engine decides who may stop or approve a Ticket, and the fuzz
  walk generates stops by people the Policy refuses (#55); one module, `src/exposure.rs`, gives both harnesses and
  `run_code` their binds and computes the `SandboxView` the secret-path checks use, so `solos-swarm vault`
  (`--state-dir`, `--pi-root`, `--node`) refuses what `agent serve` refuses (#56); each worker tool is one entry bound
  to the host resource it needs and held per seat, and `extensions/broker.ts` reads its tool table from
  `extensions/agent-tools.json`, generated from the registry and recorded in the run manifest as
  `agent_tools_sha256` (#58).
- `verify-agent-mode` (#59): `overlays/pstack.yaml` and `overlays/forge.yaml` (a local forge seeded from the shipped
  checkout), `run_code` in the base config, and the `pstack-repo` feature, driven live.
- Agent mode in production (#60): `scripts/systemd/solos-agent.service` runs `agent serve` as a systemd user unit
  whose `ExecStartPre` copies `~/agent/tokens/` into its tmpfs runtime directory before every start.
  `scripts/agent-deploy`, run from the Mac, builds an immutable release on the box, backs up `agent.db`, switches
  `current`, restarts and waits until healthy. On a failure it switches back and restores `agent.db`. It also has
  `rollback`, `status`, `logs`, `token NAME`, `start`, `stop`, `restart` and `releases`. How-to:
  `docs/agent-mode/production.md`.
- Agent mode, the GitHub App, App PR 1 (#63, design in #62): one App key replaces both personal access tokens. A
  top-level `github:` section (`app_id`, `key_file`, `poll_secs`, `reconcile_secs`, `writes`, `api_base`,
  `git_base`), and the github Source reads its repositories from `forge.repos`. The old `token_file`, `identity` and
  per-Source fields fail config load with a sentence that names the migration. `src/agent/github/app.rs` signs an
  RS256 JWT and mints installation tokens per repository and per read or write (a `GET` is a read), never with
  `workflows` or `administration`, cached until 15 minutes before expiry. A suspended or missing installation is a
  5-minute wait, not a crash. `github/identities.rs` pins each listed `github:<login>` to its account id. The agent
  comments and commits as `<slug>[bot]`, and `solos-swarm agent github check` prints each repository's installation,
  permissions and pins. An accept from a source with no Approve button is capped at `auto_approve_microusd`. New
  direct dependency: `rsa`, already in `Cargo.lock`.
- Agent mode, every budget optional (#68): `budgets.session_microusd`, `budgets.router_daily_microusd` and the whole
  `budgets:` section may be left out. An uncapped account still holds and settles every model call, so spend is
  recorded, but nothing is refused for spend. A session Ticket with no budget needs no approval. A swarm still needs
  one: the Router's, else the new `executor.swarm.default_budget_microusd`, else `session_microusd`. `agent status`
  prints `no cap`, and the Watchdog's spend alert covers capped accounts only. Swarm-mode Goals are unchanged.
- Agent mode, replies for each medium (#69): with `web.public_url`, the host adds a link to the Ticket's dashboard page
  after a Result and an Attention (`src/agent/links.rs`). On Discord a table becomes an aligned monospace block or one
  line per row, a heading becomes a bold line, and a long result is split into up to three messages, each with its
  own nonce (`src/agent/markdown.rs`, `src/agent/discord/format.rs`). Slack gets mrkdwn (`src/agent/slack/mrkdwn.rs`).
  GitHub and MCP get only the link.
- Agent mode, the web dashboard (#70): `solos-swarm agent publish` is a sidecar that opens `agent.db` read-only and
  uploads a JSON snapshot of each open and recently ended Ticket, and an index, to a private Vercel Blob store
  (`web.publish` in agent.yaml), only when a snapshot's SHA-256 changed. `web/agent-dashboard/` is a Next.js app on
  Vercel that reads those blobs behind Sign in with Vercel and an email allowlist.
  `scripts/systemd/solos-agent-publish.service` runs the sidecar, and `scripts/agent-deploy` installs and stops it
  with the agent. `agent-store` gains `events_after`, `effects_after` and `data_version`. CI gains a `web` job.
- Swarm mode, a Goal's fetch policy (#73): the broker enforces an optional `fetch` section of the Goal (`hosts`,
  `methods`, `per_agent_rps`, `follow_redirects_off_list`, `allow_private_networks`) on the URL and on every redirect
  hop before it is requested (`src/broker_fetch.rs`). A refusal is a tool result and a `fetch_refused` event, and
  `scripts/check_run.py` gains a `fetch-scope` check. Every swarm `fetch_url`, with or without a policy, now refuses
  loopback, private, link-local, CGNAT and other non-public addresses unless the policy allows them, and pins the
  connection to the addresses it checked. `authorized-audit:zelix` joins the audit scopes, and its Goals need a
  `fetch` policy to grant `fetch_url`. `scripts/prepare_platform_case.py` writes the policy.
- Agent mode, operations (#74): `solos-swarm agent health` checks the agent from outside it (the unit, restart loops,
  the broker socket, free disk, the bot's access to the Watchdog's Discord channel, whether `agent.yaml` loads) and
  posts once per incident to that channel. `solos-agent-health.timer` runs it every 3 minutes, and the agent unit's
  `OnFailure=` runs it too. Below `health.min_free_gb` (default 5) it writes `state/PAUSE`, and it lifts only a pause
  it wrote. `solos-swarm agent prune` removes the clones and transcripts of Tickets that ended more than
  `--keep-days` ago, idle forge mirrors and old Router transcripts. `solos-agent-prune.timer` runs it daily with 14
  days. `solos-swarm agent stop` files an operator's stop as `mcp:<operator>`, which the Engine judges under the
  Policy. `scripts/agent-deploy` pauses the agent and waits up to 90 s for running Assignments to report before it
  stops the unit (`--drain SECONDS`, `--no-drain`).
- Agent mode, `ready` delivers a pull request, App PR 2 (#75): on a Ticket that owes a pull request, the host admits
  a worker's `worker_report ready` only after it harvests the commits as a sandbox-made `git bundle`, fetches them
  into its own mirror and checks ancestry, the bot as every commit's author and committer, protected paths (default
  `.github/**`), size and file count. An admitted `ready` commits `Effect::Forge(Publish)`. The host then pushes
  `solos/<ticket>/<slug>` with `--force-with-lease` and opens or updates a ready pull request with `Closes #n`, each
  step safe to repeat after a crash, while the Ticket waits in `Stage::Delivering`. No new worker tool. Store schema
  7. `src/agent/forge.rs` becomes `src/agent/forge/`. New per-repository keys: `owes`, `protected`, `max_pushes`,
  `max_change_bytes`, `merge`, `ci` and `max_repairs`. Nothing acted on the last three yet.
- Agent mode, memory PR 1 (#82, design in `docs/agent-mode/memory/`): a relaunched worker's first prompt quotes the
  Ticket's earlier `ready`, `blocked` and `failed` Reports and its latest `progress`, oldest first, in at most 6,000
  characters (`src/agent/earlier.rs`). No core or schema change.
- Agent mode, a repository's dependencies and evidence (#83): `forge.repos.<repo>.deps: {kind: bun}` installs a
  repository's dependencies on the host once per lockfile, with `bun install --frozen-lockfile --ignore-scripts` in a
  bwrap sandbox that shares the network and nothing else of the host. The key is a digest of the lockfile and
  manifests at the clone's base commit in the host's mirror. Each Ticket's `run_code` mounts the result read-only. A
  failed build is told to the worker and never fails the Ticket. `forge.repos.<repo>.evidence: {command, heading}`
  makes `ready` require that command's JSON (`ok: true`, not `dirty: true`) under the heading in the pull request
  description. `agent prune` removes caches nobody mounted.
- Agent mode, memory PR 2 (#84): memos are a core aggregate (`crates/agent-core/src/memo.rs`) in a `memos` table of
  `agent.db` (schema 8), with trust (`person`, `model` or `external`) that the core derives from their sources. An
  operator's memo is active at once, and any other waits as `Proposed`. Events carry digests only, and a forgotten
  memo's text is erased from the database and its WAL (`secure_delete=ON`, a WAL checkpoint). The Router's system
  prompt and each worker's first prompt end with a quoted index of memos, recorded as `MemosShown`. New:
  `solos-swarm agent memo list | add | retire | forget` and the MCP tools `agent_memos`, `agent_memo_add` and
  `agent_memo_retire`.
- Agent mode, memory PR 3 (#85): workers propose memos with `memo_propose {text, scope, evidence}`. The broker finds
  each quote in the Ticket's Triggers or in an earlier tool answer, and refuses one it cannot find. A separate model
  call at DeepSeek's highest thinking effort (`reasoning_effort: max`) reviews each proposal the core does not
  activate and keeps, rewrites, merges or rejects it (`src/agent/broker/review.rs`). Its spend goes on a new `memory`
  ledger account (`memory.daily_microusd`, no cap by default). A proposal no review decided expires after 7 days.
  `read` serves `/memos/global.md` and `/memos/repo/<owner>/<name>.md`, and `router_read {kind: memo}` reads memos.
  `memory:` in agent.yaml is on by default where workers run on Pi.
- Agent mode, memory PR 4 (#86): the dream (`src/agent/dream.rs`). Each day at `memory.dream.at` (03:30 UTC by
  default), when a Ticket ended since the last dream, the host asks for one dream Ticket and judges it in the Router's
  place. The dream reads the ended Tickets' digests, their transcripts and the live memos, staged read-only in its
  `/work`, with thinking on at max effort on the `memory` account. It proposes corrections (`supersedes`) and new
  memos, and retires memos with `memo_retire`, citing transcript lines whose digest and text the broker checks
  against the original transcripts. The review decides its proposals, and the Watchdog sends the operator one digest
  of what changed. Store schema 9.
- Agent mode, a worker that fails the same way and a person who was asked (#106): the loop guard also stops a turn
  after 3 calls of one shape (the same tool, programs and files, whatever the whitespace, quotes or heredoc body)
  failed with the same output. The stop's Report now asks one question instead of saying "Reply here", so the Ticket
  waits at `Stage::Answer` and a reply answers it. A Ticket that waits `remind_after` (4h unless set, `null` to tell
  nobody) for a person has one reminder sent to its own thread, once per wait, naming what the wait is for; a wait
  older than 7 days is left alone. Store schema 10 adds `Ticket.reminded_ms` and the `Reminded` event.
- `collect` (#15) leaves the per-run `code/` scratch directory on the host and writes `collect-manifest.json`; `--include-code` opts back in.
- `work/tournament-20261028/` (#17): the record of the 2026-10-28 discovery tournament (frozen Goals, driver, dataset
  builder, gate, systemd units and evidence summaries), without raw logs.
- Docs (#16): pause mode, self-update operations, dashboard tunnel, tournament runbook, replay status.
- Design records: the vault and verifiable browser (#36, `docs/agent-mode/vault-browser/`), pstack as the coding method
  (#53, `docs/agent-mode/pstack/`) and the agent's GitHub App (#62, `docs/agent-mode/github-app/`). Docs matched to
  the code in #40, #41, #42, #44 and #50 (the live P10 run with a DeepSeek Router).
- Agent mode, external jobs (#122): `watchdog.jobs` lists work that runs outside agent mode, and the Watchdog
  checks each job on every look as the agent's user: a systemd user unit, a tmux session, a pid file or an
  owner-written status command (exit 0 healthy, 2 stalled, anything else down), with optional progress-file and
  free-disk floors. It tells the operator once an incident, keeps `state/watchdog-jobs.json`, and tells the same
  conversation once when the job is back. `agent publish` uploads the file as `jobs/index.json`, and the dashboard
  lists the jobs above the tickets.
- Agent mode, Rust dependencies for a repository's workers (#127): `deps: {kind: cargo, binary:
  <toolchain>/bin/cargo}` has the host run `cargo vendor --locked` for the base branch's `Cargo.lock`, once per
  lockfile, in a bwrap sandbox that reaches crates.io and nothing else and compiles nothing. Every Ticket on the
  repository mounts the vendored crates and the toolchain read-only and builds against them offline, with
  `CARGO_HOME` of its own, so `cargo build` and `cargo test` run in `run_code` with no network. A lockfile that names
  another source, and a manifest with a `git` or `registry` dependency or a `[patch]` table, is refused. New docs:
  "Build a Rust repository offline".
- Agent mode, worker file tools (the loop #106 describes): `write_file` creates or replaces one file in a Ticket's
  `/work`, and `edit_file` replaces the one place a text is in one, so a worker no longer quotes a file into a heredoc
  or a `python3` one-liner.
  A path is relative to `/work` or absolute under it, at most 1 MiB, and a link, a `..` part, an absolute path outside
  it or a directory is refused. Both write through `src/beneath.rs`, which walks a path one name at a time from `/work`
  opened once with `O_NOFOLLOW` and renames a fresh `O_CREAT | O_EXCL` file over its target, so a link the worker made
  cannot take a write outside; `run_code`'s `files` use the same writer. The worker's system prompt tells it to use the
  tools rather than a heredoc.
- Agent mode, Zhipu AI as a provider (#147): `zhipu` (the pay-as-you-go API) and `zhipu-coding` (the GLM Coding Plan,
  a separate provider because Z.ai refuses a plan key on the pay-as-you-go endpoint with error 1113) are admitted in a
  repository's `models.allow` and as upstreams (`Provider::Zhipu`/`ZhipuCoding`, `Dialect::Zhipu`), with `glm-5.3`,
  `glm-5.3-flash` and `glm-5.3-flashx` on `https://api.z.ai/api/paas/v4/chat/completions` and
  `https://api.z.ai/api/coding/paas/v4/chat/completions` under a Bearer key. GLM-5.3 rejects a disabled `thinking` and
  cannot turn thinking off, so a Zhipu call never carries the field: a session that wanted thinking off asks for
  `reasoning_effort: "low"`, and every other call's effort is clamped to low, high or max. No Anthropic Messages wire
  is shipped for it, so a `claude-code` harness reaches it through the bridge's translation onto chat completions.
  [The provider docs](docs/agent-mode/providers/zhipu.md) include the Claude Code direct setup against
  `https://api.z.ai/api/anthropic` with `ANTHROPIC_AUTH_TOKEN` and the `[1m]` model names, and a Pi `models.json`
  entry. `anthropic` remains policy-only.

### Changed

- CI (#66): the `ci` workflow runs `lint`, `scripts`, `tokenizers`, `build` (one `cargo nextest archive`), four `test`
  shards and four `test-isolated` jobs, then a `check` job that passes only when every job before it passed. `check`
  is the status to require. CI builds without debug info, and small crypto, SQLite and JSON crates build optimized in
  the dev profile (`[profile.dev.package.*]` in `Cargo.toml`). The warm median run went from 17.6 minutes to 3.5.
  The flaky tests it exposed were fixed in the tests.
- Agent mode, how the agent talks (#67): the Router writes the first reply in its own words, and the Engine's default
  is `Looking into it.` (`STANDARD_ACKNOWLEDGEMENT`) instead of `On it: <title>.`. A worker's `ready` text is a chat
  message that leads with the answer, in about 1,500 characters at most. `docs/harness/solos-swarm.md` gains
  "Talking to people", which maps each pstack reply rule it overrides. Host-written text, such as a failed worker's
  reply and the Watchdog's alerts, is in the first person.
- Agent mode in production, hardened units and a dedicated user (#117): the five systemd user units take
  `NoNewPrivileges`, a read-only file system except `~/agent/state`, a `SystemCallFilter` deny-list, restricted socket
  families and namespaces, and the other options that work in a user unit with `bwrap` (`RestrictSUIDSGID` and
  `ProtectKernelTunables` break it, so `solos-agent.service` leaves them out). `scripts/agent-deploy` takes
  `AGENT_USER`, makes the unit and state directories a user's first deploy needs, and refuses a config that names
  another uid's `/run/user` directory. `docs/agent-mode/production.md` gains "Hardening the units" and "Run the agent
  as its own user", a migration with rollback.
- Verify skill (#120): `bin/launch.sh` takes `--keep-state`, which skips the `rm -rf` of a run's `state/`, so a crash
  restart or a second start on the same day keeps its tickets, threads, memos and dream slot. `SKILL.md`'s restart
  recipe uses it, and the feature docs that said a relaunch always empties `state/` say how to keep it.
- Deploy and CI speed (#119): a deploy pushes the commit to a bare repository on the box (`~/agent/src.git`) and
  builds it in one tree updated in place (`~/agent/src`) against the shared cache, one build at a time
  (`~/agent/build.lock`), instead of sending a `git archive` and extracting a tree per release. `agent-deploy
  prebuild` builds a revision without switching, so `scripts/ship` can build a pull request's head while CI runs,
  merge on green, and deploy the merge in one command. CI runs the jobs a change needs (`scripts/ci-changes.sh`):
  documentation-only and dashboard-only pull requests skip the Rust jobs, one with no dashboard file skips `web`.
  Five test shards replace four, the tokenizers are fetched inside `build`, and doc tests run only when a doc
- Agent dashboard (#131): the hosted site is one command center in the swarm's own field — Command (the field, what
  needs a person, the numbers, recent tickets and external jobs), Chat (the latest conversations with each worker's
  reports, and a link to the thread the agent reads), Tickets, one ticket, Swarm and Jobs — with the brand's matte
  carbon, hairline panels and lavender only on live data. A failed or stale read shows a quiet notice instead of
  taking the page down. `DASHBOARD_FIXTURES=1 npm run dev` previews every screen from the test fixtures without a
  deployment, and cannot open a production build.
- Agent dashboard, web chat (#133): the hosted command center's home is now a live chat with the agent — the thread
  with each worker's reports in the middle, recent web conversations on the left, and what the swarm is doing on the
  right — polled every two seconds, with the thread kept in the address and a server-side message write to a private
  Blob inbox. Discord and GitHub stay their own sources; the full field moved to `/command`.
- Agent dashboard, design review (#136): the sign-in, 404 and error screens are composed (a split sign-in with the
  beacon field), the empty chat leads with what waits on the person and one-line commands instead of suggestion cards,
  the activity rail shows the active ticket's last worker report instead of a miniature field, and the field labels its
  rings and tickets; the Swarm field is sized so the workers land on the first screen.
- Agent dashboard, chat notes (#137): a web message the Router decides to ignore (or cannot take) appears in the chat as
  the agent's note with its reason, instead of sending forever; the publisher carries the recent decisions for web
  threads as `notes/index.json`.
- Agent dashboard and agent mode, a chat that answers (#139): `policy.answer_sources` names sources whose explicit
  messages are always answered — where the Router would ignore a web message, the host starts a session with the
  message as its scope. The production config sets `[web]`. Each conversation carries an idle/working/needs-you mark,
  and the chat no longer shows one answer twice when the delivered result and the worker's ready report share a text.
- Agent mode, the Router as the chat's supervisor (#140): `router_status` reads the agent's own state (open Tickets with
  stage, age, plan, spend, waiting question and latest report, recent Decisions, the pause and the Watchdog's alerts
  through the Store's read-only connection), `router_note` steers a running Ticket with a fact its worker reads as new
  input, and `router_decide` takes `decision: reply`: the Router answers the person itself, once, on the Trigger's own
  channels, with no Ticket and no worker (Decision kind `answered`, kept with the `reply` text; only an explicit request
  is answered). The Router's prompt tells it to supervise with `router_status`, steer with `router_note` and answer
  conversation with `reply`; the dashboard renders an `answered` note as the agent's chat message from
  `notes/index.json`, and `router_read` of a Thread carries each reply so the next message continues the conversation.
- Agent mode, the publisher's web notes (#138): `agent publish` uploads `notes/index.json` with the latest 50 Decisions
  on `web:` Threads (newest last; Thread, time, kind and reason, every kind including `declined`), so the dashboard's
  chat can show why a web message became no Ticket instead of leaving it silent. It goes up only when its content
  changes, like a ticket snapshot; `NOTES_FORMAT` counts its shape.
- Agent mode, the dashboard's web chat (#135): `web.chat` makes `agent publish` read `web/inbox.json` from the private
  Blob store each look and spool new messages from `web.chat.people` as Trigger JSON lines in
  `state/web/triggers.jsonl`, which the agent's `kind: jsonl` Source follows; a cursor in `state/web/cursor.json` keeps
  a restart from repeating one, and a missing inbox is not a failure. The Blob client reads a private blob at its
  documented host with the bearer token, `cache=0` and a put's retry rules. `tickets/index.json` rows and ticket
  snapshots gain `source_url`, a web one carrying `?t=<thread>` for the dashboard's thread match, and a followed jsonl
  Source waits for its file to appear, so the agent can start before the first message. `configuration.md` and
  `production.md` carry the block, the required source/channel/policy lines and the trust model.
- Agent mode, a model policy per repository, PR 2 (#134): at accept the host picks the repository's worker upstream from
  `forge.repos.<repo>.models` and records it in `Mandate.upstream` (name, provider, model, harness), with its harness in
  the Plan; the Router never names a model or a harness. A repository that admits nothing, or whose allowed providers no
  upstream here serves, is declined with one sentence, sent to an explicit requester once, and nothing starts. An
  accept whose GitHub thread carries a restricted repository's text must name that repository, and before the clone the
  executor checks the Ticket's upstream against the policy as it is now. Store schema 11 with an empty migration.

### Changed

- Agent mode, a worker's turn has no ceiling (#146): the broker stopped a turn at 200 tool calls or 250 model calls,
  whatever the calls were, so a long, productive investigation ended with "I made 200 tool calls without finishing,
  so I stopped. Should I keep going from where I am?" and waited on a person (the GitHub review on #143 did). Those
  ceilings are gone: the loop guard's circle, refused-call, repeated-failure and nothing-new stops remain — they stop
  a loop, never work — and a turn is bounded by the Ticket's budget and by a person's stop. The scripted harness keeps
  its own anti-hang bound (`AGENT_CALLS`, 300 model calls) so a test that hangs fails.

### Fixed

- Agent mode, a follow-up on an answered ticket (#141): a person's message on a web (or any) thread whose ticket
  already delivered `ready` resumes that ticket, and the fresh worker used to redo the work and send the same answer
  again — the quant thread showed the same solos-data answer three times. The resumed assignment's first prompt now
  says the ticket already delivered the replies quoted above, that the newest input is a follow-up to answer from them
  without repeating an earlier reply or redoing the reported work, and that input about other work belongs to the
  Router via `worker_handback` (so a "what are you working on?" follow-up reaches the supervisor's `router_status`
  answer instead of another re-run).
- Agent mode, the bench (#37): the workload has 24 distinct tasks, and `WorkloadSpec::validate` refuses more requests
  than tasks, so no two requests ask for the same work.
- Agent mode, the Pi Router (#45): a prompt holds at most `router.max_batch` cards (6). A reply cut off at the output
  limit ends the session, and the cut cards return in prompts of half the size. A Router model call is dropped after
  half of `policy.eval_timeout_ms`. A decision on a closed Eval is refused with the open Evals listed.
- Agent mode, a looping worker and a silent end (#65): a repetition guard (`src/agent/broker/repeat.rs`) refuses a
  worker's call that repeats a cycle of one to three identical calls for the third time, with HTTP 429 and a reason
  the model reads. Tools marked `repeatable` (`job_wait`, `browser_act`, `browser_read`, `worker_report`,
  `worker_handback`) are never refused. When a turn ends on a failed model call with no report, the host files
  `blocked` (no budget left) or `failed` in the worker's place, so the person hears once.
- Agent mode, a worker's plain-text answer (#72): when a Pi worker ends its turn with text and no `worker_report`, the
  host files that text as the Report (`src/agent/plain_answer.rs`): `blocked` for a short question, `ready` otherwise.
  Empty text, reasoning and a tool call written as text stay `waiting`. Claude Code workers keep the old behavior.
- Agent mode, a worker that ignores the repeat guard (#77): 3 refused calls in a row, or 10 in one turn, stop the turn
  (`ModelFailure::Stuck`). The seat's next model call is refused before any hold, and the host files one `failed`
  Report. Each refusal quotes what the call answered before. The worker's prompt now says that `run_code` has no
  `/host`.
- Agent mode, a worker that makes no progress (#78): cycles of up to 10 calls, 30 calls in a row that bring nothing
  new, and ceilings of 200 tool calls and 250 model calls per turn all end in #77's stop, with a sentence that names
  what the worker repeated (`src/agent/broker/repeat/gist.rs`). Workers sample at `executor.temperature` (default
  0.6). The Router stays at 0, and swarm mode's request is unchanged.
- Agent dashboard (#71): a failed sign-in logs the OAuth `error` and `error_description`.
- Agent mode, systemd units (#76, #80): the health check, prune and publish units set `TOKIO_WORKER_THREADS=2`, and
  the health and publish units set `TasksMax=256`. One async worker thread per core (128 on the box) had used up
  their task limit.
- `src/cluster.rs` (#79): a raft test hung when its first write was in flight as the certificates expired.
  `Cluster::authoritative` gives a request on the leader a 30 s deadline, `Cluster::shutdown` bounds its drain to 2 s
  and frees the UDP address, and replication to an unreachable peer backs off.
- Agent mode, `fetch_url` (#81): keyed and unkeyed, it goes through swarm mode's guard (`FetchGuard::open`), so
  loopback, private, link-local (cloud metadata) and CGNAT addresses are refused on every hop.
  `executor.fetch.allow_private_networks` turns the check off. The unkeyed fetch no longer uses proxy variables.
- Agent mode, admission of merge commits (#121): the protected-path check read only non-merge commits, so a merge
  commit could carry a change to `.github/actions/**`, `CODEOWNERS` or an owner's own `protected` globs and be
  admitted. `Forge::admit` now reads every commit with `diff-tree -c`, so a merge commit counts by the paths it
  changes itself, and checks the range's net diff from where the head forked from the base, which catches a merge
  that puts an older protected file back.

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
