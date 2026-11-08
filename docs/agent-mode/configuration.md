# Configuration reference

`solos-swarm agent serve --config agent.yaml` reads one YAML file. This page lists every section and field of that file
in the order of `AgentConfig` (`src/agent/config.rs` and `src/agent/config/`). It also covers `node.yaml`, which
`solos-swarm node serve` reads, and the workload spec of `solos-swarm agent bench`.

These rules hold for every file on this page:

- An unknown field is an error at every level, so a typo cannot fall back to a default.
- A relative path is relative to the directory of the file that names it.
- A token file is read once and deleted when the process starts. Provide a fresh one for every start.
- Money is in micro-USD: `1000000` is one dollar.
- A duration such as `every` or a Watchdog threshold is written `90s`, `30m`, `6h` or `1d`. A time of day is `HH:MM`
  in UTC.

## agent.yaml

| Section | Required | What it is |
|---|---|---|
| [`policy`](#policy) | yes | Who may ask, approve and stop, and the accept mode. |
| [`sources`](#sources) | yes, at least one | Where Triggers come from. |
| [`channels`](#channels) | yes | Where replies go, one per source. |
| [`router`](#router) | yes | What judges each batch of Triggers. |
| [`executor`](#executor) | yes | What runs a Ticket's work. |
| [`budgets`](#budgets) | no | Caps on spend. Without it, nothing is capped except a swarm. |
| [`watchdog`](#watchdog) | no | Alerts to an operator. Without it, no Watchdog runs. |
| [`remind_after`](#remind_after) | no | How long a Ticket waits for a person before its own thread is reminded. `4h` unless set. |
| [`nodes`](#nodes) | no | Remote machines that run workers' commands. |
| [`pass_through`](#pass_through) | no | Whether a follow-up skips the Router. |
| [`vault`](#vault) | no | This host's sealed logins and API keys. |
| [`browser`](#browser) | no | Workers' headless browser. Without it, there are no browser tools. |
| [`trail`](#trail) | no | Where the browser's trail blobs are kept. |
| [`pstack`](#pstack) | no | pstack in every worker. |
| [`forge`](#forge) | no | The repositories a Ticket may name. |
| [`github`](#github) | with any GitHub source, channel or forge | The GitHub App the agent acts as. |
| [`web`](#web) | no | The dashboard's address and its publisher. |
| [`health`](#health) | no | What `agent health` treats as trouble. |
| [`memory`](#memory) | no | Memos that workers propose, the review, and the daily dream. |

### policy

The allowlist Policy (`AllowlistPolicy` in `crates/agent-core/src/policy.rs`). A principal is `<source>:<id>`:
`slack:U012ALICE`, `discord:333333333333333333`, `github:alice` or `mcp:alice`.

| Field | Default | Meaning |
|---|---|---|
| `paused` | `false` | `true` holds every Effect and starts no Eval. |
| `people` | empty | Who may ask for work. An approver or one of these people may stop a Ticket. |
| `bots` | empty | Bots whose messages may become inferred candidates, never requests. |
| `approvers` | empty | Who may approve a budget above `auto_approve_microusd`, and a vault item's first use when the item names nobody. |
| `accept_mode` | `observe` | `off` takes explicit requests only. `observe` judges inferred candidates and records "would accept", and starts nothing. `auto` starts work for an inferred candidate that matches an enabled rule. |
| `rules` | empty | The enabled proactive rules, by name. Their text is `router.rules`. |
| `auto_approve_microusd` | `2000000` | A budget above this waits for an approver's click. |
| `eval_timeout_ms` | `120000` | How long an Eval waits for the Router. A Pi Router's model call is dropped after half of it. |

`github:` logins are lowercased in `people`, `approvers` and `bots`. A listed GitHub login must be a valid login.

### sources

A list. Each item has a `kind`. At most one source of each of the kinds `slack`, `discord`, `github` and `mcp`.

| Kind | Fields | Notes |
|---|---|---|
| `jsonl` | `path` | Trigger JSON, one per line. For tests and dry runs. |
| `slack` | `channels`, `app_token_file`, `bot_token_file`, `api_base` (`https://slack.com/api/`) | Socket Mode. `channels` lists at least one channel id, and a direct message's `D` id can be listed. The app token is `xapp-` with `connections:write`. The bot token is `xoxb-`, and the `slack` channel uses it too. Setup: [Connect Slack](how-to/slack.md). |
| `discord` | `guilds`, `channels`, `token_file`, `api_base` (`https://discord.com/api/v10/`) | Gateway. Lists at least one guild or channel. Ids are numbers, quoted or not. The `discord` channel uses the same bot token. Setup: [Connect Discord](how-to/discord.md). |
| `github` | none | REST polling of `forge.repos`, as the App in [`github`](#github). Setup: [Set up the GitHub App](how-to/github-app.md). |
| `schedule` | `schedules` | Named slots that fire System Triggers. See [Schedules](#schedules). |
| `mcp` | none | Requests that operators file with `solos-swarm mcp --state-dir`, from `<state dir>/mcp/inbox.jsonl`. Needs the `mcp` channel. Setup: [Hand the agent work from an MCP host](how-to/mcp.md). |

Tests point `api_base` at a fake.

#### Schedules

Each item of `schedules`:

| Field | Meaning |
|---|---|
| `name` | Names the schedule's Thread and Trigger ids. Letters, digits, `-`, `_` and `.`. Unique. |
| `every` | A fixed interval, at least `1m`. Slots start at whole multiples of it since 1970-01-01T00:00Z, so `6h` fires at 00:00, 06:00, 12:00 and 18:00 UTC. |
| `daily_at` | A time of day in UTC. Give either `every` or `daily_at`, not both. |
| `text` | What the agent is asked to do. |
| `reply_to` | Where its Tickets answer and ask. Without it, nobody hears from them, and the Watchdog tells the operator when such a Ticket waits for a person. |

`reply_to` takes one of these forms, and a channel must serve its source:

- `{kind: slack, channel: C0123OPS}`, with `thread_ts` for a thread.
- `{kind: discord, channel: 222222222222222222}`, a channel or a Discord thread, with `message_id` to reply to that
  message.
- `{kind: github, repo: acme/widgets, number: 1}`, an issue or pull request. With the `github` channel, the repository
  must be in `forge.repos`.

### channels

A list. Each item has a `kind`, and no two channels serve one source.

| Kind | Fields | Serves replies to |
|---|---|---|
| `jsonl` | `source`, `path` | The named source. Appends each message to `path`. |
| `slack` | none | `slack`. Needs the `slack` source, whose bot token and `api_base` it uses. |
| `discord` | none | `discord`. Needs the `discord` source, whose bot token and `api_base` it uses. |
| `github` | none | `github`. Comments as the App in [`github`](#github). |
| `mcp` | none | `mcp`. Appends replies to `<state dir>/mcp/outbox.jsonl`, which `solos-swarm mcp --state-dir` reads. |

### router

`kind: scripted` or `kind: pi`.

`scripted` takes `ignore_follow_ups` (default `false`). With it on, an explicit candidate on a Thread whose Ticket is
still open is ignored, as a follow-up that Ticket already has. `agent bench` uses it.

`pi` is a long-lived Pi session, keyless and sandboxed, whose model calls go through the agent broker against
`budgets.router_daily_microusd`.

| Field | Default | Meaning |
|---|---|---|
| `provider` | required | `deepseek` or `venice`, the names `solos-swarm run --provider` takes. When the executor is `pi` too, both name one provider, since one credential serves both. |
| `model` | required | Any model for `deepseek`. For `venice`, a model whose id starts with `deepseek-v4-1-flash-uncensored` or `zai-org-glm-5`. |
| `prices` | required | `input_microusd_per_million` (at least 300000), `output_microusd_per_million` (at least 1200000), both at most 10000000, and optional `cache_hit_microusd_per_million`. Without a cache-hit rate, every input token settles at the input rate. |
| `max_input_bytes` | `1048576` | 1 to 16777216. |
| `max_output_tokens` | `8192` | 1 to 393216. |
| `max_batch` | `6` | 1 to 64. The most candidates one prompt holds, oldest first. A reply decides each of them, so this many decisions (about 300 tokens for an accept) must fit in `max_output_tokens`. |
| `instructions` | built in | Replaces the built-in instructions. The budgets and rules are still added after them. |
| `rules` | empty | What each enabled rule in `policy.rules` means, by name. Each name must be in `policy.rules`. |

### executor

`kind: scripted`, `kind: simulated` or `kind: pi`.

`scripted` takes no fields and reports `ready` at once.

`simulated` is work that only takes time, for `agent bench` (see [About the benchmark](benchmark.md)):

| Field | Default | Meaning |
|---|---|---|
| `max_workers` | `64` | Assignments at once, 1 to 64. |
| `time_factor` | `1` | Wall time per workload time. |
| `default_work_ms` | `600000` | The work of a Ticket whose origin Trigger names none, 1 ms to one day. |
| `router_waits_for_work` | `false` | The Router takes no Candidate while work runs. |

`pi` runs one keyless, sandboxed worker session per Assignment, whose model calls go through the agent broker against
its Ticket's budget:

| Field | Default | Meaning |
|---|---|---|
| `provider`, `model`, `prices`, `max_input_bytes`, `max_output_tokens` | as `router` | The same fields and ranges as a Pi Router's. |
| `temperature` | `0.6` | What workers' model calls are sampled at, 0 to 2. The Router and swarm mode use 0 whatever this is. |
| `max_workers` | `2` | Worker sessions at once, 1 to 64. A launch past it waits for a slot. |
| `prepare` | none | A shell command run once per Ticket in its directory before its first worker, with `SOLOS_TICKET` and `SOLOS_WORK_DIR` (the worker's `/work`) set. Not empty. With `forge`, it must not run `git worktree add` into `$SOLOS_WORK_DIR`. |
| `run_code` | none | The workers' `run_code` sandbox. Without it, workers have no `run_code`, no job tools, and no `write_file` or `edit_file`. See [executor.run_code](#executorrun_code). |
| `fetch` | `{}` | What workers' `fetch_url` may reach. See [executor.fetch](#executorfetch). |
| `swarm` | none | How `Plan::Swarm` Tickets run. Without it, they fail. See [executor.swarm](#executorswarm). |
| `harness` | `pi` | The harness of a session Ticket whose Plan names none: `pi` or `claude-code`. `claude-code` needs `claude`. |
| `claude` | none | Where Claude Code is. See [executor.claude](#executorclaude). |
| `wire` | `chat` | How a Claude Code session's Messages calls reach `provider`: `chat` translates them onto its chat completions, or `anthropic` forwards them to its own Messages endpoint with the broker's key. `anthropic` needs `claude`, and only `deepseek` serves it. See [executor.wire](#executorwire). |
| `web_search` | none | The provider's own web search, which the broker adds to every call it forwards. Needs `wire: anthropic`. See [executor.web_search](#executorweb_search). |
| `environment` | `local` | Where a session's commands run when its Plan names nowhere: `local` or a node in `nodes.allow`. |

#### executor.run_code

| Field | Default | Meaning |
|---|---|---|
| `mounts` | empty | Read-only binds, each `{target: /name, source: /absolute/host/path}`. No target twice. With `pstack`, no mount at `/pstack`. |
| `uv_root` | none | An absolute path to a uv Python store that a mounted venv's interpreter links into, bound read-only at its own path. |
| `concurrency` | `4` | Sandboxes at once across all workers, 1 to 8. Background jobs count. |
| `memory_bytes` | 4 GiB | 256 MiB to 16 GiB. A repository's `deps.memory_bytes` can raise it for that repository's Tickets. |
| `background_after_secs` | `5` | A `run_code` still running after this many seconds becomes a job, when the Ticket has room for one. 1 to 300. |
| `max_jobs` | `2` | Background jobs per Ticket whose end has not reached the worker, 1 to 8. |
| `job_max_secs` | `1800` | The longest a job may run, and the default and most of `job_start`'s `timeout_s`. 300 to 86400. |
| `job_output_bytes` | `65536` | What a job keeps of each output stream until it is read, 1 KiB to 1 MiB. |

A `run_code` command has no network, at most 300 s of CPU time per process and no file over 256 MiB. A Rust
repository gets its toolchain from `forge.repos.<repo>.deps`, not from `mounts` ([deps](#forge)).

#### executor.fetch

| Field | Default | Meaning |
|---|---|---|
| `allow_private_networks` | `false` | Off, `fetch_url` refuses a request, and every redirect hop, whose host resolves to a loopback, private, link-local, shared, unique-local, unspecified or multicast address. On, workers may reach those. The browser has its own `browser.allow_private_networks`. See [What fetch_url can reach](runtime.md#what-fetch_url-can-reach). |

#### executor.swarm

| Field | Default | Meaning |
|---|---|---|
| `templates` | required | Goal templates by name, each a YAML file. A name is a short word of letters, digits, `-` and `_`. See [Write a Goal template](how-to/goal-templates.md). |
| `default_template` | none | The template of a swarm Plan that names none. Must be one of `templates`. |
| `max_runs` | `1` | Swarms at once, 1 to 8, apart from `max_workers`. |
| `default_budget_microusd` | none | What a swarm accept that names no budget gets, before `budgets.session_microusd`. Above `policy.auto_approve_microusd`, it needs someone who can approve. |

Config load renders each template for one and two agents and refuses one that does not make a valid Goal.

#### executor.claude

Every path is absolute.

| Field | Default | Meaning |
|---|---|---|
| `binary` | required | The `claude` executable, or the npm package's `cli.js` with `node`. |
| `node` | none | The node that runs `binary`. Without it, `binary` runs by itself, as the native build does. |
| `solos_swarm` | this binary | The solos-swarm the sandbox runs for the session's tools and model bridge. |

`agent health` runs `claude auth status --json` on `binary` (under `node`, if set) to see whether the host's Claude Code
login has ended. It skips the check when `binary` is not installed, unless `executor.harness` is `claude-code`. See
[Alerts from outside the agent](production.md#alerts-from-outside-the-agent).

#### executor.wire

`chat` (the default) keeps the broker's translation of Anthropic Messages onto the provider's OpenAI-compatible chat
completions: `src/gateway/anthropic.rs` turns each request into a chat body and each completion into a Message.

`anthropic` forwards each admitted Messages request to the provider's own Messages endpoint instead:
`https://api.deepseek.com/anthropic/v1/messages` for `deepseek`. The broker's key is the only credential sent, and the
session's prompt, tools, `thinking` and `output_config` go upstream as the session wrote them, minus fields such as
`metadata` and `mcp_servers`. The broker still sets `model`, `max_tokens` (the lower of the session's and the
executor's), `stream` and, where the call does not think, `temperature`. A streamed reply comes back as the provider
frames it, as it arrives. Only `deepseek` has an admitted Messages endpoint; `venice` is refused at config load.

A forwarded call is held and settled like a chat call. Its usage is counted as every input token: `cache_read_input_tokens`
settle at `prices.cache_hit_microusd_per_million`, and `cache_creation_input_tokens` at the input rate, since the price
table has no cache-write rate of its own. The receipt's `output_tokens` must fit the call's `max_tokens`. A reply whose
usage cannot be read, or a stream that ends before its usage, charges the call's hold as an unknown cost.

Claude Code runs on this wire with the model settings DeepSeek recommends: the session's model with `[1m]` for its
main, opus and sonnet model names, the plain model for haiku and subagents, `CLAUDE_CODE_EFFORT_LEVEL=max`, and an
auto-compaction window of 786,432 tokens or less where `max_input_bytes` is smaller. See
[Run workers on Claude Code](how-to/claude-code.md#run-on-deepseeks-anthropic-api).

#### executor.web_search

Needs `wire: anthropic`. DeepSeek serves Anthropic's server-side web search on its Messages endpoint as the tool
`web_search_20250305`, and bills each search that returns results apart from the tokens. The broker adds the tool to
every call it forwards and refuses a session that names a server tool itself.

The hold includes each search's price and the input its results may add: every search may run a further round that
reads the request and the results so far again, so a call of `max_uses` searches holds the request `max_uses + 1`
times and each result once per round after it. The settlement bills the searches the provider reports in
`server_tool_use.tool_search`, which are the ones that returned results.

| Field | Default | Meaning |
|---|---|---|
| `max_uses` | `1` | The most searches one call may run, 1 to 8. |
| `microusd_per_search` | `750` | What one search that returns results costs, 750 ($0.00075, DeepSeek's $0.75 per 1,000 searches) to 1000000. |
| `result_tokens` | `131072` | The most input tokens one search's results may add to the hold, 1 to 1000000. One search measured on DeepSeek added about 56,000. |

### budgets

Each cap is optional. Where one is absent, what it would cap is not capped, though each model call is still held and
settled in the ledger. [Budgets](concepts.md#budgets) explains the rules.

| Field | Default | Meaning |
|---|---|---|
| `session_microusd` | none | What a session Ticket gets. Absent: a session Ticket has no budget, needs no approval and has no cap. It is also the fallback budget of a swarm. Above `policy.auto_approve_microusd`, every Ticket waits for an approval, so it needs a Slack or Discord channel and an approver on that source. |
| `router_daily_microusd` | none | The most a Pi Router's calls may commit in one UTC day. Absent: no daily cap. |

Neither may be `0`.

### watchdog

| Field | Default | Meaning |
|---|---|---|
| `operator` | required | `{source, address}`: the conversation the operator reads, through the channel that serves `source`. |
| `every` | `1m` | How often the Watchdog looks. |
| `retry_stalled` | `false` | Retry a stalled Assignment, once per Ticket. |
| `thresholds` | see below | Each threshold left out keeps its default. |

`operator.address` by source:

| Source | Address |
|---|---|
| `slack` | `channel`, and `thread_ts` for a thread. |
| `discord` | `channel`, a channel or thread id, and `message_id` to reply to a message. |
| `github` | `repo` (`owner/name`, in `forge.repos`) and `number`, an issue. |
| `mcp` | `operator` and `thread`. |

| Threshold | Default | Raises an alert for |
|---|---|---|
| `waiting` | `1h` | A Ticket waiting for a person. |
| `stalled` | `30m` | A working Assignment with no Report. |
| `failed_attempts` | `3` | An Effect's failed attempts. At least 1. |
| `unavailable` | `1` | `unavailable` Decisions within `unavailable_within`. At least 1. |
| `unavailable_within` | `1h` | The window of `unavailable`. |
| `spend_percent` | `80` | Spend as a share of the Router's daily cap or a Ticket's budget, 1 to 100. |
| `paused` | `1h` | A pause held by `control.json` or `PAUSE`. |
| `repeat` | `6h` | How often a problem that lasts is told again. |

What each signal means: [The Watchdog](runtime.md#the-watchdog).

`jobs` lists external jobs for the Watchdog to check on every look: work that runs outside agent mode and is not a
Ticket. It is under `watchdog`, not a section of its own, because the Watchdog checks the jobs and tells the same
`operator`. Each entry is a `name` and exactly one way to check it, with optional checks beside it. A setup with an
example: [Watch jobs that are not Tickets](how-to/operate.md#watch-jobs-that-are-not-tickets).

| Field | Meaning |
|---|---|
| `name` | Names the job in alerts and on the dashboard. 1 to 64 letters, digits, `-`, `_` or `.`, starting with a letter or digit, and unique. |
| `unit` | A systemd user unit, such as `l2book-download.service`. Up while `systemctl --user show` says it is `active` or `reloading`. |
| `tmux` | A tmux session name of letters, digits, `-` and `_`. Up while `tmux has-session -t =NAME` succeeds. |
| `pid_file` | An absolute path. Up while the file's first number is a process that is alive and not a zombie. Linux only: it reads `/proc`. |
| `command` | The absolute path of a program and its arguments, as a list. No shell reads it. Exit `0` is healthy, exit `2` is stalled, and any other exit, a signal or a timeout is down. The first line of its output is the detail. |
| `timeout` | With `command` only: how long it may run, `10s` by default and at most `2m`. |
| `progress` | `{file, stalled_after}`: an absolute path the job writes as it works, and how long it may stay unchanged, such as `20m`. A running job whose file is older than that, or missing, is stalled. |
| `disk` | `{path, min_free_gb}`: an absolute path, and the free space in GB that its disk must keep. A running job below it is low on disk. |

At most 64 jobs. Every path is absolute, because the agent's working directory is not where an owner would look.

### remind_after

How long a Ticket may wait for a person before the agent sends one reminder to the Ticket's own thread. A wait is an
approval, a question, or a blocked or failed Report. The reminder names what the wait is for (the question, the
block, or the last thing the worker said) and offers the Approve buttons again for a budget. Each wait is told once:
one that begins after a person writes or the worker asks again is told again. A wait older than seven days is left
alone, so the value must be `7d` or less.

The default is `4h`. `null` tells nobody. This is separate from the Watchdog's `waiting` alert: the Watchdog tells
the operator at `watchdog.thresholds.waiting` (1h), and the reminder tells the person who was asked, on their
thread. Nothing is reminded while `policy.paused` is on, and a wait that was due is reminded once the pause lifts.

### nodes

Needs `executor: {kind: pi}`. Setup: [Run commands on your own machine](how-to/nodes.md).

| Field | Meaning |
|---|---|
| `listen` | Where nodes dial in, such as `0.0.0.0:7443`. |
| `cert`, `key` | The agent's certificate and key, from `solos-swarm agent init-nodes`. |
| `allow` | The nodes that may connect, by name. At least one. |

Each entry of `allow` is a name (a short word of letters, digits, `-` and `_`, never `local`) with:

| Field | Meaning |
|---|---|
| `fingerprint` | The node's certificate, as `solos-swarm node init` printed it. No two nodes pin one certificate. |
| `about` | What the Router is told the machine is for, at most 300 characters. |

### pass_through

`true` by default. An allowed person's follow-up on a subscribed Thread goes only to its Ticket. `false` also sends it
to the Router, as the `no-pass-through` design of `agent bench` does.

### vault

`{path, key_file}`: the sealed vault file and its key (32 random bytes, mode 0600), which `solos-swarm vault init`
writes. Neither may sit where a sandbox can read it. Setup: [Give workers an API key](how-to/vault.md).

### browser

Needs `executor: {kind: pi}`. Setup: [Give workers a browser](how-to/browser.md).

| Field | Default | Meaning |
|---|---|---|
| `lightpanda` | required | The Lightpanda binary, a separate program run in its own sandbox and never linked (AGPL-3.0). |
| `audit_key` | required | This host's trail key, from `solos-swarm browser init-key`. |
| `runtime_dir` | `<state dir>/browser` | Session directories and their CDP sockets. |
| `max_sessions` | `2` | Browsers at once. A call past it is refused, as a signed step, rather than queued. |
| `idle_secs` | `1800` | A browser with no call for this long is closed. Above zero. |
| `address_space_bytes` | 64 GiB | `prlimit --as`. V8 reserves about 40 GiB of address space before it loads a page. |
| `v8_heap_mb` | `512` | The JavaScript heap. |
| `subframes` | `false` | Off, frames are not loaded, so a page's main document is the only one a login fill can target. |
| `allow_private_networks` | `false` | Off, the browser runs with `--block-private-networks`. |
| `bwrap`, `prlimit` | `/usr/bin/bwrap`, `/usr/bin/prlimit` | The binaries. |

### trail

A top-level section beside `browser`.

| Field | Default | Meaning |
|---|---|---|
| `path` | `<state dir>/trail` | Where the browser steps' blobs are kept. Refused where a sandbox can read it. |
| `max_bytes` | 8 GiB | Past it, no Ticket's browser starts a step. |
| `ticket_max_bytes` | 256 MiB | Past it, this Ticket's browser starts no step. |

### pstack

Needs `executor: {kind: pi}`. With the section, every worker's first prompt enters a pstack skill, its system prompt
carries the solos-swarm adapter, and it may `read` the vendored tree at `/pstack`.

| Field | Default | Meaning |
|---|---|---|
| `entry` | `poteto-mode` | The skill every worker enters, a directory under `vendor/pstack/skills`. |

### forge

Needs `executor: {kind: pi}` with `run_code`, where workers run git in their clones. Setup:
[Let workers work in a repository](how-to/repositories.md).

| Kind | Fields | Meaning |
|---|---|---|
| `github` | `repos` | GitHub, through the App in [`github`](#github). Each clone's commits are by the App's bot. |
| `local` | `remote`, `identity: {name, email}`, `repos` | Bare repositories at `<remote>/<owner>/<name>.git` on this disk, for tests and dry runs. `identity` is who each clone's commits are by. |

`repos` maps `<owner>/<name>` to its settings. It is the one list of repositories: the `github` source, the `github`
channel and the forge all use it. It may not be empty.

#### Repository settings

Each entry of `forge.repos`, keyed `<owner>/<name>`:

| Field | Default | Meaning |
|---|---|---|
| `about` | required | What the Router reads to tell which repository a request is about. Not empty. |
| `base` | required | The branch each Ticket's clone starts from. |
| `budget_microusd` | none | What an accept that names this repository and no budget gets. Above `policy.auto_approve_microusd`, it needs someone who can approve. |
| `notes` | none | What the worker's prompt says about working in it, such as where its toolchain is. |
| `owes` | `answer` | `answer`, `change` or `pull_request`: what a Ticket owes when the Router names nothing, and the most it may owe. `pull_request` needs `forge: github` and `github.writes: true`. |
| `protected` | `[".github/**"]` | Paths no commit of the agent may change, as globs. A merge commit counts by the paths it changes itself. ASCII case is ignored. |
| `max_pushes` | `6` | Heads the agent publishes for one Ticket, review rounds included. At least 1. |
| `max_change_bytes` | `5000000` | The largest change a `ready` is admitted with, 1 to 50000000. |
| `merge` | `ask` | `off`, `ask` or `auto`. Recorded in the Ticket's terms. Merging is not built yet. `auto` needs `ci: expected` and an approver. |
| `ci` | `expected` | `expected` or `none`: whether a merge waits for CI. |
| `max_repairs` | `2` | CI failures that may resume a delivered Ticket. Recorded in the Ticket's terms. Repair is not built yet. |
| `deps` | none | Dependencies the host installs for the base commit. See below. |
| `evidence` | none | What a pull request's description must carry. Needs `owes: pull_request`. See below. |

`deps`:

| Field | Default | Meaning |
|---|---|---|
| `kind` | required | `bun`, installed from `bun.lock`, or `cargo`, vendored from `Cargo.lock`. |
| `binary` | `$HOME/.bun/bin/bun` for `bun`; none for `cargo` | The package manager, an absolute path. For `cargo`, required: the `cargo` of one toolchain, such as `~/.rustup/toolchains/1.96.0-x86_64-unknown-linux-gnu/bin/cargo`, not rustup's proxy at `~/.cargo/bin/cargo`. The toolchain's whole directory is bound read-only into the build and into every Ticket's `run_code` on this repository, with `cargo` and `rustc` on `PATH`, so `executor.run_code.mounts` needs no entry for Rust. |
| `timeout_secs` | `600` | The longest a build may run, 10 to 3600. |
| `max_bytes` | 8 GiB | The most disk a build may use, scratch included, 1 MiB to 64 GiB. |
| `memory_bytes` | 16 GiB | The address space of this repository's `run_code`, used in place of `executor.run_code.memory_bytes` where that is lower. 256 MiB to 16 GiB. Bun needs about 8 GiB. `rustc` fails to allocate under 4 GiB on the `solos-swarm` crate and builds it at 6 GiB. |

With `cargo`, a Ticket's `run_code` also has `CARGO_NET_OFFLINE=true`, `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0` and `CARGO_BUILD_JOBS=16`, builds into `/work/target`, and finds the vendored crates in
`/runtime/cargo/vendor`. A lockfile that names a source other than crates.io is refused. The host needs the toolchain,
`cc`, `bwrap` and `prlimit`. See [Build a Rust repository offline](how-to/repositories.md#build-a-rust-repository-offline)
and [Cargo](repository-tickets.md#cargo).

`evidence`:

| Field | Default | Meaning |
|---|---|---|
| `command` | required | A one-line shell command, such as `bun run solos dev verify --scope unit --json`. |
| `heading` | `## Evidence` | A one-line Markdown heading. |

How both work: [Installed dependencies and evidence](how-to/repositories.md#install-dependencies-and-require-evidence).

### github

Required when a `github` source, a `github` channel or `forge: github` is configured, and refused when none is. Needs a
`forge` section. Setup: [Set up the GitHub App](how-to/github-app.md).

| Field | Default | Meaning |
|---|---|---|
| `app_id` | required | The App's numeric id. |
| `key_file` | required | The App's private key (PEM), a one-use token file. Refused under any path a sandbox can read. |
| `poll_secs` | `60` | The `github` source's interval, at least 10. |
| `reconcile_secs` | `300` | How often every open change is read again, at least 60. Nothing reads it yet. |
| `writes` | `true` | `false`: no write token is ever minted, so the agent writes nothing on GitHub. A restart applies it. |
| `api_base` | `https://api.github.com/` | For tests. GitHub Enterprise Server is `https://HOST/api/v3/`. |
| `git_base` | `https://github.com/` | Where the mirrors fetch `<owner>/<name>.git` from. Tests use a `file://` URL. |

A config that still names the old token fields (`token_file`, `repos`, `api_base` or `poll_secs` on the `github`
source, or `token_file` or `identity` on `forge: github`) fails to load with a sentence that points at
[Move from token files](how-to/github-app.md#move-from-token-files).

### web

| Field | Default | Meaning |
|---|---|---|
| `public_url` | required | The dashboard's origin: `https://`, no trailing slash, no login, query or fragment. A path is allowed. A Ticket's page is `{public_url}/tickets/{id}`, and replies link to it. |
| `publish` | none | What `solos-swarm agent publish` uploads with. Without it, `agent publish` has nothing to do. |

`publish`:

| Field | Default | Meaning |
|---|---|---|
| `token_file` | required | A Vercel Blob read-write token (`vercel_blob_rw_...`), a one-use token file. |
| `every_secs` | `5` | How often the Store is looked at, 1 to 3600. An upload happens only for a snapshot whose content changed. |
| `heartbeat_secs` | `300` | The index is uploaded again after this long, so the dashboard can tell a quiet agent from a dead publisher. From `every_secs` to 86400. |
| `api_url` | `https://vercel.com/api/blob` | For tests. `http` only for a loopback address. |

`agent serve` ignores the section except for `public_url`. Setup and rotation:
[production.md](production.md#dashboard).

### health

Read by `solos-swarm agent health`, not by `agent serve`. Every field has a default.

| Field | Default | Meaning |
|---|---|---|
| `min_free_gb` | `5` | Free space, in GiB, on the state directory's filesystem below which `agent health` pauses new work and alerts. `0` turns the floor off. |
| `resume_free_gb` | the floor plus a fifth of it, at least 1 GiB more | Free space at which a pause that `agent health` made is lifted. Above `min_free_gb`. |
| `restart_burst` | `3` | Automatic restarts of the unit since the last check that count as a crash loop. At least 1. |

### memory

On by default where workers run on Pi. How memos work: [Memory](runtime.md#memory).

| Field | Default | Meaning |
|---|---|---|
| `proposals` | `true` | Workers may propose memos with `memo_propose` and read `/memos`. `false`: only the owner writes memos, and there is no dream. |
| `daily_microusd` | none | What the review and the dream may commit per UTC day on the `memory` account. Absent: no cap. Not `0`. |
| `review` | see below | The model call that decides each proposal that is not outside text. |
| `dream` | see below | The daily dream. |

`review`, each field the workers' unless set:

| Field | Default | Meaning |
|---|---|---|
| `provider` | the workers' | Must be the workers' provider, since one credential serves both. |
| `model` | the workers' | |
| `prices` | the workers' | |
| `thinking` | `max` | DeepSeek's `reasoning_effort` with thinking on: `low`, `high` or `max`. |
| `max_output_tokens` | `65536`, or `131072` at `max` | What one review may answer, reasoning included. 1 to 393216. |

`dream`:

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Ask for a dream once a day where workers propose memos. |
| `at` | `03:30` | The time of day, UTC. It runs before the 04:30 prune, so the transcripts it reads are still there. |
| `thinking` | `max` | The dream's `reasoning_effort`, with thinking on. |
| `max_output_tokens` | `65536`, or `131072` at `max` | What one of its calls may answer, reasoning included. 1 to 393216. |

`memory.review` and `memory.daily_microusd` are refused unless the executor is `pi`.

## node.yaml

`solos-swarm node serve --config node.yaml` reads it (`NodeConfig` in `src/remote/daemon.rs`). Setup:
[Run commands on your own machine](how-to/nodes.md).

| Field | Default | Meaning |
|---|---|---|
| `name` | required | This node's name in the agent's `nodes.allow`. A short word, never `local`. |
| `agent.address` | required | `host:port` of the agent's `nodes.listen`. |
| `agent.fingerprint` | required | The agent's certificate, as `solos-swarm agent init-nodes` printed it. |
| `cert`, `key` | required | This node's certificate and key, from `solos-swarm node init`. |
| `work_root` | required | Each Ticket's `/work` is a directory under it, kept between commands. |
| `sandbox.mounts` | empty | Read-only binds, each `{target: /name, source: /absolute/path}`. Not over `/secrets`. |
| `sandbox.uv_root` | none | As `executor.run_code.uv_root`. |
| `sandbox.concurrency` | `4` | 1 to 8. |
| `sandbox.memory_bytes` | 4 GiB | 256 MiB to 16 GiB. |
| `sandbox.job_max_secs` | `86400` | The longest a background job may run, 300 to 86400. |
| `sandbox.bwrap`, `sandbox.prlimit` | `/usr/bin/bwrap`, `/usr/bin/prlimit` | The binaries. |
| `secrets.env` | empty | Variables by name, each the text of a file without its trailing newline. |
| `secrets.files` | empty | Files by name, bound read-only at `/secrets/<name>`. |
| `vault` | none | `{path, key_file}`, as in agent.yaml. `solos-swarm vault` takes node.yaml as its config. `node serve` does not use a vault. |

## Bench workload spec

`solos-swarm agent bench --workload spec.yaml` and `solos-swarm agent workload --spec spec.yaml` read a YAML spec.
Every field left out keeps its default:

```yaml
seed: 1                 # run n (from 0) of every design uses seed + n
messages: 300           # every Trigger, requests included
requests: 20            # at most 24
follow_ups: 40
ci_messages: 40
per_minute: 5           # about an hour of stream
arrivals: poisson       # or even
work_minutes: {min: 6, max: 16}
people: 6               # on policy.people
outsiders: 6            # not on it
outsider_share: 0.5     # of the chatter
addressed_share: 0.2    # of the outsiders' chatter
reply_share: 0.5        # chatter that replies in one of the 5 latest chat Threads
unlisted_bot_share: 0.25
request_span: 0.8
time_factor: 0.016666666666666666   # a workload minute lasts a wall second
```

What the stream holds: [About the benchmark](benchmark.md#the-workload).
