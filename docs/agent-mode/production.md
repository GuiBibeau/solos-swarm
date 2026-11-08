# Running agent mode in production

This runbook is for the person who operates the always-on agent. One box runs `solos-swarm agent serve` as the systemd
user unit `solos-agent`, with three helper units beside it: the dashboard publisher, the health check and the daily
prune. You run `scripts/agent-deploy` on the Mac, in this repository, and it does the work on the box over SSH. The box
is `remote-ai` unless `AGENT_HOST` names another, and the agent runs as the user that SSH logs in as (`AGENT_USER`
names another). Everything on the box is under that user's `~/agent` ([layout](#layout-on-the-box)).

Run the agent as a user of its own, with no sudo and no groups, so that anything that escapes a worker, or runs on the
host from a repository, finds nothing else to take. [Hardening the units](#hardening-the-units) limits what the agent's
processes can do, and [Run the agent as its own user](#run-the-agent-as-its-own-user) limits what the user owns.

What each config key means is in [the agent-mode plan](README.md). This page says how to run the agent. The
[Reference](#reference) at the end lists the units, the commands, the files and the limits.

## Before you start

Once per box:

1. Give the Mac SSH access to the box with a key. `scripts/agent-deploy` connects with `ssh -o BatchMode=yes`, so it
   cannot answer a password prompt.
2. Install the build tools for the user that runs the agent: rustup with toolchain 1.96.0 under `~/.cargo/bin`, `npm`,
   and Node 24.18.0 at `~/.local/opt/node-v24.18.0-linux-x64/`.
3. Install `bwrap` and `prlimit`, which the sandboxes need. For a repository with `deps`
   ([below](#install-a-repositorys-dependencies-and-require-evidence)), also install `bun` at `~/.bun/bin/bun`, or for
   `kind: cargo` the toolchain from step 2 (the repository's `deps.binary` names its `cargo`) and `cc` from
   `build-essential`.
4. Turn on lingering for that user (`loginctl enable-linger <user>`), so its units start at boot with nobody logged in.
5. Write `~/agent/agent.yaml` ([the production config](#the-production-config)) and store the tokens it names
   ([Store a token](#store-a-token)). A deploy refuses to switch without the config. A deploy makes
   `~/.config/systemd/user` and `~/agent/state` itself.

The units' runtime directories are `/run/user/<uid>/...`, where `<uid>` is `id -u` of the agent's user, and
`agent.yaml` names them in `token_file` and `key_file`. The examples below write 1000, the first user of a box. A
deploy refuses to switch when `agent.yaml` names another uid's directory, and says which.

## Store a token

Each credential is one file in `~/agent/tokens/` on the box, mode 0600. From the Mac, pipe the value in:

```sh
pbpaste | scripts/agent-deploy token discord-token
scripts/agent-deploy token github-app.pem < ~/Downloads/solos-bot.2026-11-05.private-key.pem
```

The command reads stdin, refuses an empty value, and prints nothing of it. Then restart the unit that reads the token
(`scripts/agent-deploy restart`). The names the units know are in [Tokens](#tokens).

`agent serve` and `agent publish` read each token file once and delete it. So before every start, crash restarts
included, each unit's `ExecStartPre` copies the tokens it needs into its own tmpfs runtime directory, and the config
names that copy, never `~/agent/tokens`. The runtime directory goes away when the unit stops. No sandbox sees
`~/agent/tokens` or a runtime directory. The tokens rest unencrypted at mode 0600 in the agent user's home, like the
vault key.

## The production config

`~/agent/agent.yaml` on the box is the production config. It is not in git. This is its shape, with your values in
`<...>`. `/run/user/1000` is the runtime directory of uid 1000; write the one of the agent's user (`id -u`):

```yaml
# ~/agent/agent.yaml on the box
policy:
  people: [discord:<your Discord user id>, mcp:<your name>, github:<your GitHub login>, web:g.bibeaulaviolette@gmail.com]
  approvers: [discord:<your Discord user id>, github:<your GitHub login>, web:g.bibeaulaviolette@gmail.com]
  auto_approve_microusd: 500000            # a budget above this waits for Approve
sources:
  - kind: discord
    guilds: [<server id>]
    token_file: /run/user/1000/solos-agent/discord-token
  - kind: mcp                              # `solos-swarm mcp`, `agent stop` and `agent memo` on the box
  - kind: github                           # no fields: it reads forge.repos as the App
  - kind: jsonl                            # the dashboard's chat, spooled by `agent publish`
    path: state/web/triggers.jsonl
channels:
  - kind: discord
  - kind: mcp
  - kind: github                           # comments as the App's bot
  - kind: jsonl                            # replies to the web chat
    source: web
    path: state/web/replies.jsonl
router:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1320000, cache_hit_microusd_per_million: 30000}
executor:
  kind: pi
  provider: deepseek                       # the Router's provider: one key serves both
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1320000, cache_hit_microusd_per_million: 30000}
  max_workers: 4
  run_code: {}                             # the forge needs it: workers run git in their clones
  # fetch: {allow_private_networks: true}  # without it, fetch_url refuses loopback, private and Tailscale addresses
pstack: {}                                 # every worker enters poteto-mode
github:
  app_id: <the App's numeric id>
  key_file: /run/user/1000/solos-agent/github-app.pem
forge:
  kind: github
  repos:
    GuiBibeau/solos:
      about: solos, one binary agents call to trade across venues (JavaScript).
      base: main
      owes: pull_request                   # the most the agent does here
      models: {allow: [deepseek]}          # the only provider its text may reach
      deps: {kind: bun}                    # the host installs bun.lock's dependencies
      evidence: {command: "bun run solos dev verify --scope unit --json"}
    GuiBibeau/solos-swarm:
      about: solos-swarm, the agent's own code (Rust and TypeScript).
      base: main                           # no owes: it defaults to answer, so the agent only reads here
      models: {allow: [deepseek, anthropic]}
memory:                                    # optional; these are the defaults
  proposals: true                          # workers propose memos, and a review decides each that is not outside text
  review: {thinking: max}
  dream: {enabled: true, at: "03:30", thinking: max}   # UTC, before the 04:30 prune
  # daily_microusd: 500000                 # the review's and the dream's cap per UTC day; absent: no cap
watchdog:
  operator: {source: discord, address: {channel: <a channel id you read>}}
  every: 1m
# remind_after: 4h                          # a Ticket waiting on a person is reminded once after this; null tells nobody
health:
  min_free_gb: 50                          # pause new work below 50 GB free on the state directory's disk
web:
  public_url: https://agent.guibibeau.com
  publish:
    token_file: /run/user/1000/solos-agent-publish/vercel-blob-token
  chat:
    inbox: web/inbox.json                  # the dashboard writes it on every send
    people: [g.bibeaulaviolette@gmail.com] # the only author whose messages become Triggers
    # spool: state/web/triggers.jsonl      # the default: <state>/web/triggers.jsonl
```

Read these before you change it:

- Every `token_file` and `github.key_file` names the unit's runtime copy, as [Store a token](#store-a-token) says.
  `serve` refuses a `key_file` under any path a sandbox can read.
- Every repository names `models`, the providers its text may reach, and a config without one does not load. Before
  deploying a release that includes the model policy, add a `models:` line to each repository in the live config, or
  the agent will refuse to start. A repository whose allowed providers no upstream here serves still loads, and
  `agent serve` logs it at start ([Repository models](configuration.md#repository-models)).
- `mcp:<your name>` in `policy.people` lets you run `agent stop` and `agent memo` from the box.
- There is no `budgets:` section, so a session Ticket and the Router have no cap. A swarm's budget comes from the Router
  or from `executor.swarm.default_budget_microusd`. To cap them, add
  `budgets: {session_microusd: 500000, router_daily_microusd: 3000000}` ([Budgets](concepts.md#budgets)).
- A Ticket from GitHub, MCP or a schedule has no Approve button. With no `budgets.session_microusd` a session
  Ticket has no budget at all, so there is nothing to cap. Where a budget applies (a swarm's, or a repository's
  `budget_microusd`), an accept from one of those Sources may name at most `auto_approve_microusd`.
- `watchdog.operator` is where the Watchdog's alerts, the dream's daily digest and the health check's posts go. The
  health check posts only to a `source: discord` operator.
- Write `web.publish` in block style, as above. `scripts/agent-deploy` looks for a `publish:` line under `web:` to know
  whether to run the publisher.
- `web.chat` needs the jsonl Source, the `source: web` jsonl Channel and `web:<your email>` in `policy.people` and
  `policy.approvers`, all as above. The publisher refuses an author not in `web.chat.people`; the agent's Policy refuses
  `web:<author>` again if it is not in `policy.people`. Both lists must name the address
  ([web.chat](configuration.md#web)).
- `memory:` and `health:` can be left out. Their defaults are in [memory](configuration.md#memory) and
  [The disk floor](#the-disk-floor).

The sections below say how to set up GitHub, dependencies, memory and the dashboard. The keys are in
[github](configuration.md#github), [forge](configuration.md#forge), [memory](configuration.md#memory),
[watchdog](configuration.md#watchdog) and [web](configuration.md#web).

## Deploy a release

```sh
scripts/agent-deploy deploy            # origin/main
scripts/agent-deploy deploy <rev>      # any commit, branch or tag
scripts/agent-deploy prebuild <rev>    # steps 1 to 3 only: build the release, change nothing that runs
```

To merge a pull request and deploy it in one command, see [Ship a fix](#ship-a-fix).

A deploy does these steps, in order:

1. It fetches `origin`, resolves the revision to a 12-digit sha, and pushes that commit to a bare repository on the box,
   `~/agent/src.git`, as `refs/deploy/<sha>`. A push sends only the objects the box lacks: a one-line change is a few
   kilobytes, and the first push sends the history, about 9 MB. The box skips the build when that release is already
   built.
2. It refuses to build with less than 30 GB free.
3. It builds on the box, one build at a time (`~/agent/build.lock`). It checks the commit out into one persistent tree,
   `~/agent/src`, and runs `cargo build --locked --release` there with toolchain 1.96.0 and the build cache
   `~/agent/target`. The checkout rewrites only the files that differ, so cargo recompiles what changed and nothing else.
   It then unpacks the commit into `releases/<sha>`, runs `npm ci --ignore-scripts`, copies in the binary, runs `doctor`
   and makes the release read-only. A failed compile prints the end of cargo's log.
4. It [drains](#drain-running-work) the agent, stops the publisher and the agent, and copies `agent.db*` into
   `backups/`.
5. It points `current` at the new release (the old one becomes `previous`), installs and enables the units from the
   release, and starts the agent.
6. It waits until the agent is healthy: the unit is active, its broker socket exists, and the process is the same 20
   seconds later.
7. It lifts the drain's pause, starts the publisher if the config has `web.publish`, and starts the health and prune
   timers.
8. It deletes old releases. It keeps the newest five, plus `current` and `previous`.

If the new release does not become healthy, the deploy switches back to the previous release, restores the
`agent.db` it copied (the new release may have migrated it), starts that, and exits 1.

Every command prints how long its parts took. A deploy that went well ends like this:

```text
sent 53f30cbf570b (3s)
compiled in 71s
built release 53f30cbf570b in 77s
backed up agent.db to /home/debian/agent/backups/20261107T142301Z-ebb9f39ae70a
release 53f30cbf570b is serving (pid 812345)
switch took 24s: drain 0s, stop 2s, backup 0s, units and start 1s, health wait 21s
unpaused
publisher: active
deployed 53f30cbf570b in 105s
```

## Roll back

```sh
scripts/agent-deploy rollback
```

`rollback` switches to the `previous` release with the same drain, backup and health wait as a deploy.

A binary refuses an `agent.db` whose schema is newer than it knows, so a rollback across a schema change fails and
says so. These releases changed the schema:

| Schema | Added by |
|---|---|
| 7 | #75, a Ticket's change and pull request |
| 8 | #84, memos |
| 9 | #86, memos the dream supersedes and retires |

To roll back across one of them:

1. Run `scripts/agent-deploy stop`.
2. On the box, list the backups with `ls -1t ~/agent/backups/`. Each name is the backup's time and the release that
   was serving then.
3. Move `~/agent/state/agent.db*` aside, and copy the matching backup's `agent.db*` into `~/agent/state/`. You lose
   everything the agent stored after that backup.
4. Run `scripts/agent-deploy rollback`.

One change does not move the schema. A Ticket accepted with no budget (no `budgets:` section) stores none, and a binary
from before #68 fails to read it. `rollback` does not notice. Roll back past #68 only when no such Ticket is open, or
restore a backup as above.

## Drain running work

`deploy`, `rollback`, `restart` and `stop` drain before they stop the agent. `start` does not. A drain pauses the
agent, then waits up to 90 seconds for each Assignment that was running to send a Report or end.

```sh
scripts/agent-deploy deploy --drain 300      # wait up to 5 minutes
scripts/agent-deploy rollback --no-drain     # stop at once, for an emergency
AGENT_DRAIN_SECS=180 scripts/agent-deploy restart
```

`--drain SECONDS` and `--no-drain` go anywhere after the command. `--drain 0` is the same as `--no-drain`.

To pause and wait by hand on the box, without a stop, use the copy of the script that every command installs there:

```sh
ssh <box> 'bash ~/agent/.agent-deploy --on-host drain 300'   # pause, then wait up to 300 s
ssh <box> 'bash ~/agent/.agent-deploy --on-host resume'      # lift that pause
```

What a drain does:

- It writes `state/control.json` as `{"mode":"pause","by":"agent-deploy",...}`. If a `control.json` that someone else
  wrote is already there, it keeps that one and waits under it.
- While the agent is paused, it starts no new Eval and performs no Effect. The broker answers each worker's next model
  call with the pause reply, so the worker finishes the call it is in and then waits.
- It asks the running release's `agent status --json` every 3 seconds. An Assignment is done waiting once its Report
  count changed or it is no longer running.
- After the timeout it stops the agent anyway and says how many Assignments were still mid-turn. The stop kills a
  worker in a long tool call, such as a build. That work starts again as `host restarted`, and the new worker is told
  to check `/work` before it repeats anything.
- Once the new release is healthy, the script removes `control.json`, but only a file marked `"by":"agent-deploy"`. It
  never replaces or lifts a pause a person wrote. After `stop`, it removes the drain's file at once, so the next start
  is unpaused.

While paused, the Sources keep reading and storing messages, and the agent judges them after the pause lifts. A Discord
message sent in the seconds the agent is down is lost, because a new Gateway session does not replay. `agent status`
shows `pause: held by control.json (mode pause)` during a drain, and the Watchdog alerts after an hour of pause. If a
deploy fails and leaves the agent paused, the script says so. Look at `~/agent/state/control.json` and, once the agent
is right, remove it.

## Start, stop and restart

```sh
scripts/agent-deploy restart
scripts/agent-deploy stop
scripts/agent-deploy start
```

- `restart` drains, restarts the agent, waits until it is healthy, then restarts the publisher and starts the timers.
- `stop` drains, stops the publisher and then the agent. The health check stays quiet about it for 30 minutes, so an
  agent you stopped on purpose alerts only if it is still down after that.
- `start` starts the agent, waits until it is healthy, then starts the publisher and the timers.

## Check on the agent

```sh
scripts/agent-deploy status            # the release, the unit, `agent status`, the publisher, the timers, a health check
scripts/agent-deploy logs              # the agent's last 100 log lines
scripts/agent-deploy logs -f           # follow them
scripts/agent-deploy logs --since 1h
scripts/agent-deploy publish-logs      # the publisher's journal; same arguments
scripts/agent-deploy health-logs       # the health check's journal
scripts/agent-deploy prune-logs        # the daily prune's journal: its last line says what it freed
scripts/agent-deploy releases          # the releases on the box, with current and previous marked
```

`status` prints `agent status` (open Tickets with their age, spend, alerts and the pause), then
`publisher: active (configured: yes)` and how long ago the dashboard index went up, the two timers and their last
results, any open health incident, and the output of `agent health --dry-run`, which checks and changes nothing.

## Stop a Ticket by hand

To stop a stale Ticket, such as an Assignment that will never report or a request nobody wants any more, run
`agent stop` on the box. It goes through the running agent, as a person's Stop button does:

```sh
ssh <box> '~/agent/current/bin/solos-swarm agent stop --state-dir ~/agent/state --operator <your name> --ticket tk_0123456789abcdef --reason "stale after the deploy"'
# tk_...: Stopped by <your name>.
```

`agent stop` files the command in `state/mcp/commands.jsonl` as the operator `mcp:<your name>`, and `agent serve` reads
it. This needs the `kind: mcp` Source and Channel in `agent.yaml`, and `mcp:<your name>` in `policy.people` or
`policy.approvers`. The agent refuses anyone else with "is not allowed to stop this work". The Ticket ends for good as
`stopped`, with the reason `stopped by <your name> on mcp: <reason>`, and a later message in its thread does not resume
it. Only the agent writes the Store. The exit statuses are in [`agent stop`](#agent-stop).

To clear several stale Tickets, list the open ones with `scripts/agent-deploy status`, then stop each:

```sh
ssh <box> 'for t in tk_aaa tk_bbb; do ~/agent/current/bin/solos-swarm agent stop --state-dir ~/agent/state --operator <your name> --ticket $t --reason "stale after the deploy"; done'
```

## Set up GitHub

The agent works on GitHub as the bot of a GitHub App, with no personal access token.

1. Create and install the App by hand, once, as [Set up the GitHub App](how-to/github-app.md#create-and-install-the-app) says. Note its App ID and download
   its private key.
2. Store the key, then delete the download:

   ```sh
   scripts/agent-deploy token github-app.pem < ~/Downloads/solos-bot.2026-11-05.private-key.pem
   rm -P ~/Downloads/solos-bot.2026-11-05.private-key.pem
   ```

3. Add `github:`, `forge:` and the `github` Source and Channel to `agent.yaml`, as in
   [the production config](#the-production-config). Add `github:<your login>` to `policy.people` and
   `policy.approvers`. Logins are lowercase, and each one is pinned to its account id in
   `state/github-identities.json` the first time the agent looks it up.
4. Deploy with `scripts/agent-deploy deploy`.
5. Check the App from the box. The unit's copy of the key is gone, so name the durable one:

   ```sh
   ssh <box> '~/agent/current/bin/solos-swarm agent github check --config ~/agent/agent.yaml --key-file ~/agent/tokens/github-app.pem --state-dir ~/agent/state'
   ```

   It ends with `ok`, or exits 1 and says what to fix.

What `owes` allows on each repository of `forge.repos`:

- `answer` (the default): the agent reads the repository and answers. No Router choice raises it.
- `change`: the worker's commits stay in the Ticket's clone.
- `pull_request`: a worker's `ready` opens a pull request as the bot, ready for review, on a branch
  `solos/<ticket>/<slug>` that only the agent writes. No commit may touch a path in `protected` (default
  `.github/**`), a merge commit counting by the paths it changes itself. Nothing merges. The config accepts `merge`,
  `ci`, `max_repairs` and `github.reconcile_secs`, and no release acts on them yet. A config with `owes:
  pull_request` and `github.writes: false` does not load.

To rotate the key, generate a second key in the App's settings, store it with `scripts/agent-deploy token
github-app.pem`, run `scripts/agent-deploy restart`, then delete the first key on GitHub.

To stop GitHub work:

- Suspend the App under Settings, Applications, Installed GitHub Apps. Every GitHub call then waits and tries again
  every five minutes. Unsuspending resumes the work with no restart.
- Or set `github.writes: false` and restart. The agent then reads and writes nothing on GitHub.

A config that still has `token_file` on the github Source or on `forge: github`, or `identity` on `forge: github`, fails
to load with a sentence that points at [Move from token files](how-to/github-app.md#move-from-token-files). The agent no longer
reads `forge-token`. Delete it from `~/agent/tokens`.

## Install a repository's dependencies and require evidence

A worker's sandbox has no network, and its clone has no `node_modules`. So a JavaScript repository's checks cannot run
there, and a pull request without their proof fails that repository's CI. `GuiBibeau/solos` wants the JSON of
`bun run solos dev verify --scope unit --json` under `## Evidence` in every pull request (its `AGENTS.md`, "Evidence").
Two keys on its repository handle both:

```yaml
# ~/agent/agent.yaml
forge:
  repos:
    GuiBibeau/solos:
      about: ...
      base: main
      owes: pull_request
      models: {allow: [deepseek]}          # the providers its text may reach, by role
      deps: {kind: bun}                    # binary: the box's bun; default $HOME/.bun/bin/bun
      evidence: {command: "bun run solos dev verify --scope unit --json"}
```

- `deps`: the first time a Ticket on the repository starts, the host installs the dependencies from `bun.lock` at the
  base commit into `state/deps/GuiBibeau/solos/<key>/`. On the box that cache is 2.2 GiB. A later Ticket mounts the same
  cache read-only, and a new lockfile on `main` makes a new cache. The install runs in its own sandbox, the one place
  besides the browser that reaches the network.
- `deps.memory_bytes` is the address space (a cap, not memory used) for this repository's `run_code`, 16 GiB by
  default. bun aborts under 4 or 6 GiB, and its lint step under 8. `executor.run_code.memory_bytes` stays as it is for
  every other Ticket.
- `evidence`: the worker runs the command itself, offline, and the host refuses its `ready` unless the report carries
  the JSON under the heading.

The box needs `bun`, `bwrap` and `prlimit`. Keep `state/` off a tmpfs, because a cache holds many files. `agent prune`
removes a cache no Ticket has used for its `--keep-days`. What the mechanism trusts and what it leaves to CI is in
[Install dependencies and require evidence](how-to/repositories.md#install-dependencies-and-require-evidence).

A Rust repository, `GuiBibeau/solos-swarm` here, asks for `deps: {kind: cargo, binary: <toolchain>/bin/cargo}` instead,
and its Tickets then build and test offline. The box needs the toolchain, `cc`, `bwrap` and `prlimit`, and no
`run_code.mounts` entry:
[Build a Rust repository offline](how-to/repositories.md#build-a-rust-repository-offline).

## Memory and the dream

Memory is on by default wherever workers run on Pi. Workers propose memos, a review call at DeepSeek's highest thinking
effort decides each `model` one, and once a day the dream checks the memos against the transcripts of the Tickets that
ended. A proposal that rests on outside text (`external`: a stranger, a page, any tool's output) is never reviewed and
waits for you: `agent memo list` shows it, and `agent memo add` keeps what is right in your words. How it works is in
[Memory](runtime.md#memory) and [The dream](runtime.md#the-dream).

Operate it from the box:

```sh
ssh <box> '~/agent/current/bin/solos-swarm agent memo list --state-dir ~/agent/state'          # --all adds retired and forgotten memos
ssh <box> '~/agent/current/bin/solos-swarm agent memo add --state-dir ~/agent/state --operator <your name> --scope repo:GuiBibeau/solos "Tests run with bun test"'
ssh <box> '~/agent/current/bin/solos-swarm agent memo retire --state-dir ~/agent/state --operator <your name> mm_0123456789abcdef --reason "no longer true"'
ssh <box> '~/agent/current/bin/solos-swarm agent memo forget --state-dir ~/agent/state --operator <your name> mm_0123456789abcdef'
```

`list` reads the Store. `add`, `retire` and `forget` go through the running agent as `mcp:<your name>`, like
`agent stop`, with the same exit statuses. A memo you add is active at once. `forget` erases a memo's text, quotes and
reasons and keeps its digest, so the event chain still verifies.

- The dream starts at `memory.dream.at` (03:30 UTC by default), only if a Ticket ended since the last dream. Keep `at`
  before the 04:30 prune, which removes transcripts 14 days after their Ticket ended.
- The dream's digest goes to `watchdog.operator` once a day. Without a `watchdog:` section there is no digest.
- The review and the dream are billed to the ledger's `memory` account. `memory.daily_microusd` caps that account per
  UTC day.
- To stop workers proposing memos, and the dream with them, set `memory.proposals: false`. To stop only the dream, set
  `memory.dream.enabled: false`. Restart after either.

## Dashboard

The dashboard is a web page of the agent's tickets at `https://agent.guibibeau.com`, behind Sign in with Vercel, for one
person. The box accepts no inbound connection, so it pushes:

```text
solos-agent  --writes-->  agent.db  --read-only-->  solos-agent-publish  --HTTPS PUT-->  private Vercel Blob store
     ^                                                  |  <--HTTPS GET web/inbox.json------------^
     |                                                  +--> state/web/triggers.jsonl
     +-- its jsonl Source (source: web) follows the spool, and its jsonl Channel writes state/web/replies.jsonl
                                                                                                  |
                                       you  -->  agent.guibibeau.com (web/agent-dashboard)  <--reads, server side
```

`solos-swarm agent publish` runs as the unit `solos-agent-publish`. It opens `agent.db` read-only and looks at it every
`web.publish.every_secs` (5). A look that finds nothing written costs one SQLite pragma. When the Store changed, it
builds `tickets/index.json` (the open tickets first, then the latest 100 ended) and one `tickets/<ticket id>.json` per
ticket in the index. It uploads only a snapshot whose content changed, tickets first and the index last, so the index
never names a blob that is not there. When nothing changed, it uploads the index again every `heartbeat_secs` (300),
which is how the dashboard tells a quiet agent from a dead publisher. Its state file is
`state/publish-state.json`, the hash of each blob it last uploaded to the store its token names. A different store gets
everything again. With `web.chat` it also reads the dashboard's inbox and appends to `state/web/triggers.jsonl`,
remembering progress in `state/web/cursor.json` ([Chat from the web](#chat-from-the-web)). The web app is
[web/agent-dashboard](../../web/agent-dashboard/README.md).

With `watchdog.jobs` in agent.yaml, the Watchdog writes `state/watchdog-jobs.json`, and the publisher reads that file
(the one thing it reads besides `agent.db`) and uploads it as `jobs/index.json`: when its content changes, which leaves
out the time of the Watchdog's last look, and again every `heartbeat_secs`, so the dashboard can tell a quiet Watchdog
from a stopped one. The home page lists the jobs above the tickets and warns when the Watchdog has not looked for 15
minutes ([External jobs](runtime.md#external-jobs)).

### Turn on the dashboard

1. Set up the Vercel side first: the project, the private Blob store, the Sign in with Vercel app, the environment
   variables and the domain ([setup on Vercel](../../web/agent-dashboard/README.md#set-up-on-vercel)).
2. Copy the Blob store's read-write token (`vercel_blob_rw_...`) from the store's page on Vercel, and store it:

   ```sh
   pbpaste | scripts/agent-deploy token vercel-blob-token
   ```

3. Add `web:` to `~/agent/agent.yaml`, in block style:

   ```yaml
   web:
     public_url: https://agent.guibibeau.com      # https, no trailing slash: a ticket's page is {public_url}/tickets/{id}
     publish:
       token_file: /run/user/1000/solos-agent-publish/vercel-blob-token
       every_secs: 5                              # optional, 1 to 3600
       heartbeat_secs: 300                        # optional, every_secs to 86400
     chat:                                        # optional: the dashboard's chat becomes Triggers
       inbox: web/inbox.json
       people: [g.bibeaulaviolette@gmail.com]     # also add web:<this email> to policy.people and approvers
   ```

   With `chat`, also add the jsonl Source and Channel and the `web:` principals from the production config above
   ([Chat from the web](#chat-from-the-web)).

4. Run `scripts/agent-deploy deploy`, or `scripts/agent-deploy restart` on the current release.
5. Check it:

   ```sh
   scripts/agent-deploy status           # "publisher: active (configured: yes)", "dashboard index last uploaded 3s ago"
   scripts/agent-deploy publish-logs     # "publish: uploaded tickets/tk_...json, tickets/index.json (2 unchanged)"
   ```

From then on, `deploy`, `rollback`, `start`, `stop` and `restart` install and restart the publisher with the agent. They
stop it while `agent.db` is backed up or restored, so it never reads a file being replaced.

The publisher's unit copies `tokens/vercel-blob-token`, and only that, into its own runtime directory,
`/run/user/<uid>/solos-agent-publish/`. `agent publish` reads it once and deletes it. The agent's unit does not copy this
token. A `web:` section without `publish:` only sets `public_url`, the link the Discord replies carry, and installs
nothing.

To make one look by hand, put a fresh copy of the token in the runtime directory first, because the command consumes it:

```sh
ssh <box> '~/agent/current/bin/solos-swarm agent publish --config ~/agent/agent.yaml --state-dir ~/agent/state --once'
```

It prints what it uploaded as JSON and exits 1 if the API refused a blob.

### What is published, and who can read it

For each ticket the publisher uploads:

- its title and the Router's scope
- the request and every later message from its thread (people's, bots' and the Router's notes), in full
- every reply the agent sent, in full, with whether it was delivered
- the Decisions about it, each Assignment's start, end and Reports, and the spend and budget
- for a browser, the pages it opened (title, status, and the address without its query or fragment, which carry tokens)
  and how many steps were refused or unanswered

It does not publish what a browser typed (the Store holds only a digest), vault items or their values, tokens, or
anything outside `agent.db`. A message is cut at 100,000 characters, an Assignment keeps its latest 20 Reports, and a
browser trail its latest 40 pages. The one-line answer to a Discord button or a `stop` command is not in the Store, so it
is not on the page.

The Blob store is private: every read needs a token, and the dashboard reads it on the server after a sign-in whose email
is in `ALLOWED_EMAILS`. Anyone who holds the read-write token can read all of it. The token rests at mode 0600 beside
the others, with the same exposure as `model.key`.

The publisher never deletes a blob, so an old ticket's page keeps working after the index stops listing it, and a ticket
the agent resumes goes back to the index. A reply's link can be followed a few seconds before the ticket's first
upload. The page says so and checks again by itself. The link to the original conversation is made for Discord
messages and for GitHub issues and pull requests. A Slack link needs the workspace's name, which the agent does not
hold.

Blob operations are billed, and a put is an "advanced operation". A working ticket changes its snapshot with each report
and each reply, so a busy hour is tens of puts. An idle day is about 290: one index every five minutes.

### Chat from the web

The dashboard's chat (at `/` on `agent.guibibeau.com`) writes `web/inbox.json` into the same private Blob store on every
send. Its own server function is the only writer, after Sign in with Vercel; the browser never holds the Blob token.
With `web.chat` in `web:`, every look of `agent publish` also reads that blob with the publisher's Blob token, at
`https://<storeId>.private.blob.vercel-storage.com/web/inbox.json?cache=0`, and appends each new message from a person
on `web.chat.people` to `state/web/triggers.jsonl` as one Trigger JSON line. One write and one `fsync` per look, so the
agent's `kind: jsonl` Source follows it.

Each message becomes a Trigger with:

- `source: "web"`, `addressed: "mention"`, and the message's id as `tr_<the same 32 hex>`, so a redelivery is dropped
- `actor: {id: "<author email>", kind: "person", name: "<name>"}`, so the principal is `web:<author email>`
- `threads: ["web:<thread>"]` and `reply_to: [{source: "web", address: {thread: "<thread>"}}]`
- `meta: {thread: "<thread>", url: "https://agent.guibibeau.com/?t=<thread>"}`

The ticket's `source_url`, in `tickets/index.json` and in its snapshot, is that same address. The dashboard matches a
conversation's thread to its ticket through it. `state/web/replies.jsonl`, written by the `source: web` jsonl Channel,
holds the agent's replies; the ticket snapshot's conversation carries them to the web page.

What was read is remembered in `state/web/cursor.json`: the last processed id and time, and the last 200 ids. A restart
never turns the same message into a Trigger. If the last id has fallen out of the inbox (the dashboard keeps 200
messages), only messages newer than the stored time are considered, and an id already in the list is never processed
again. Malformed messages and authors off the list are logged and skipped; they do not stop the publisher. A fresh
cursor treats everything in the inbox as new, so the first run can spool up to 200 Triggers.

The trust model, plainly. The private Blob store's read-write token is the boundary: it can read and write every blob,
it rests at mode 0600 beside the others, and only the publisher's unit and the dashboard's server functions hold one.
The browser never does. The dashboard writes only what a signed-in person sent. The publisher then enforces
`web.chat.people` before any text reaches the spool, and the agent's Policy checks the principal
`web:<author email>` against `policy.people` again — so a misconfigured first list still does not make the agent work for
a stranger. The message text is data: it cannot change a permission, a budget or a Policy, and it never appears in a log
line. The agent unit does not hold the Blob token.

### Rotate the dashboard's secrets

- **The Blob token.** On Vercel, under **Storage**, create a new read-write token for the store or rotate the existing
  one. Then run `pbpaste | scripts/agent-deploy token vercel-blob-token` and `scripts/agent-deploy restart`. The app reads the store
  with OIDC through `BLOB_STORE_ID`, so the project needs no change. The publisher does not retry a refused token. It
  exits with "the Blob token was refused", systemd starts it five times in 15 minutes, and then leaves the unit failed.
  `status` shows that, and the dashboard warns when its index is fifteen minutes old.
- **The app's client secret, the session secret and the allowlist.** See
  [Rotate things](../../web/agent-dashboard/README.md#rotate-things).

### Turn off the dashboard

Remove the `web.publish` section and run `scripts/agent-deploy restart`. It stops and disables the publisher. The blobs
stay in the store until you delete them or the store.

## Alerts from outside the agent

The Watchdog runs inside `agent serve`, so it cannot say that `serve` itself is down or crash-looping. The health check,
`solos-swarm agent health`, can. Every deploy installs and enables it. It runs:

- from `solos-agent-health.timer`, two minutes after boot and three minutes after each start of the check
- at once when systemd gives up on the agent (five failed starts in 15 minutes), through `OnFailure=` in
  `solos-agent.service`

Each run checks the unit, its restarts, the broker socket, the free disk, the Discord token and the config, and where
`executor.claude` is set, the host's Claude Code login ([Health checks](#health-checks)). It needs nothing from the
agent.

A failure posts once per incident to the Watchdog's channel, `watchdog.operator`, which must be `source: discord`. It
posts as the bot, through the API root of the config's `discord` Source. It posts again only when a new kind of
problem joins the incident, and once when everything is well again ("`solos-agent` on `<host>` is healthy again after
12m."). The next run tries a failed post again, and nothing is told twice. `state/health-state.json` holds the
open incident, which kinds were told, the restart count, whether the check made a pause, and the channel the last good
config named, so a config that no longer loads is still told to the right channel.

The check uses the agent's own bot token. Its `ExecStartPre` copies `~/agent/tokens/discord-token` into
`/run/user/<uid>/solos-agent-health/` before each run, and the directory goes when the run ends. The check never starts,
stops or restarts anything.

Deploys do not alert. While `scripts/agent-deploy` stops and starts the agent, it writes `state/health-hold`, an expiry
time 30 minutes ahead. Until then the check records problems and tells nobody. A deploy, rollback, start or restart
removes the hold when it finishes, so a deploy that went wrong alerts at the next run. `stop` leaves the hold in place.

**Claude Code's login.** An expired sign-in makes every Claude Code run on the host fail with `Failed to authenticate:
OAuth session expired and could not be refreshed`, and nothing in the agent notices. When `executor.claude` names a
`claude` that is installed, the check runs `claude auth status --json` as the unit's user. That reads what Claude Code
stored on the host: it starts no session and spends no tokens. The check reads two fields of the
answer, `loggedIn` and `loginExpiresAt`. It never reads the credential, and it keeps and posts nothing else the answer
holds, such as the account's email. It fails when Claude Code is signed out, or when `loginExpiresAt`, the date the
stored sign-in ends, has passed. The alert says what to do: sign in again as the agent's user with `claude auth login`
(or `/login` inside `claude`). The next run clears it and posts the recovery once.

- With `executor.harness: claude-code`, a `claude` that is not at the configured path fails the check. With the default
  `pi` harness, where only a Plan may name Claude Code, a `claude` that is not installed is skipped.
- `loginExpiresAt` needs Claude Code 2.1.318 or later. An older one gives only `loggedIn`, so a sign-in that has ended
  is caught only once Claude Code itself says it is signed out.
- A session that Anthropic ended before its recorded date still shows as signed in. Only a real request finds that.
- The check sees the login of the user that owns the unit (`~/.claude`, or `CLAUDE_CONFIG_DIR` if the unit sets it). A
  token passed to other jobs through `CLAUDE_CODE_OAUTH_TOKEN` is not the login the check reads.

If Discord or the token is what is broken, nobody can be told. The check fails, which shows in
`scripts/agent-deploy health-logs` and in `systemctl --user --failed`, and it tells Discord the next time it can. The
check runs on the box it watches, so a box that is off, or a user manager that is gone, alerts nobody.

## Pruning state and the disk floor

`state/` grows with every Ticket: each repository Ticket's clone, every worker's transcript, a mirror per repository,
the dependency caches and the Router's transcripts. The disk is shared with other production services, so
`solos-agent-prune.timer` runs `solos-swarm agent prune --state-dir ~/agent/state --keep-days 14` every day at 04:30
(the box's time, which is UTC, up to 15 minutes late). The timer is `Persistent`, so a day the box was off is made up at
the next boot. The prune opens the Store read-only, runs at the lowest CPU and disk priority, and its last line says
what it freed. What it removes is in [Prune](#prune).

To see what a run would remove, with sizes, and what the daily runs freed:

```sh
ssh <box> '~/agent/current/bin/solos-swarm agent prune --state-dir ~/agent/state --keep-days 14 --dry-run'
scripts/agent-deploy prune-logs
```

`--json` prints the report as JSON. A failed removal is listed, the rest goes ahead, and the exit status is 1, which
shows the service as failed. If a person resumes a Ticket while a prune runs, the prune checks it again just before it
removes the files, and the Ticket keeps them.

### The disk floor

The health check also guards the disk. When free space on the state directory's filesystem drops under
`health.min_free_gb`, it writes `state/PAUSE`, starting with `paused by solos-agent-health: ...`, and alerts. That is the
pause gate an operator has: no new Eval starts, the agent performs no Effect, and workers' next model calls wait. The check
frees nothing.

It lifts that pause once free space is back to `health.resume_free_gb`, and it lifts only a `PAUSE` that starts with
its own line. It never overwrites or lifts a `PAUSE` a person wrote, and a `control.json` pause is a different file.
The check runs every three minutes, so a disk that fills faster than that is not caught. A deploy also refuses to build
under 30 GB free.

```yaml
health:
  min_free_gb: 50        # default 5; 0 turns the floor off
  # resume_free_gb: 60   # default: the floor plus a fifth of it, at least 1 GB more
  # restart_burst: 3     # restarts since the last run that count as a crash loop
```

To free space when the floor trips:

1. Run the prune by hand with a shorter `--keep-days`, after a `--dry-run`.
2. Run `cargo clean` in `~/agent/target`, the build cache. The prune never touches it, and the next deploy rebuilds it
   (measured: 114 s from an empty cache, 70 s after a one-line change).
3. Delete old releases or backups only by hand. A deploy keeps five releases and twenty backups.

## Hardening the units

The agent runs what repositories ask of it: workers in `bwrap` sandboxes, and the host's own `git` and package manager.
If a worker escapes its sandbox, or the host runs something it should not, the attacker is the agent's user. Every
unit therefore limits what that user's processes can do, and the limits apply to every worker too, because a child
inherits its parent's seccomp filters and `NoNewPrivileges`.

A user unit cannot take every systemd sandboxing option, and some break `bwrap`. Each option below was tried on the
box (Debian 13, systemd 257, bubblewrap 0.12.0, kernel 6.12) with transient units, against the real command lines of
a `run_code` call (`src/code.rs`), a Pi session (`src/harness.rs`) and a dependency install (`src/agent/forge/deps.rs`),
and then the finished unit text against the real `solos-swarm doctor`, `agent health --dry-run`, `agent prune
--dry-run` and `agent publish --once` (against a copy of an old `agent.db` and a stand-in for the Blob API).

systemd 257 gives a user unit that asks for a mount namespace an identity user namespace: its user maps to itself and
every other user to `nobody`. So no option needs `PrivateUsers=`, and files that other users own show as `nobody`
inside a unit.

### What the units set

| Option | What it does | Units |
|---|---|---|
| `NoNewPrivileges=yes` | `sudo` and any other setuid or file-capability program cannot raise privileges, in the unit and in every worker. | all |
| `LockPersonality`, `RestrictRealtime`, `SystemCallArchitectures=native` | No `personality()` changes, no realtime scheduling, no 32-bit system calls. | all |
| `SystemCallFilter=~@cpu-emulation @obsolete @raw-io @reboot @swap`, `SystemCallErrorNumber=EPERM` | Drops system calls no worker needs. A blocked call fails with `EPERM`; without the error number it would kill the process. | all |
| `RestrictNamespaces` | `user mnt pid net ipc uts cgroup` for the agent: the seven namespaces `bwrap --unshare-all` makes, and no `time` namespace. `yes` (none) for the others. | all |
| `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK` | No raw packet sockets, no vsock, no other families. Netlink is for name lookups and for `bwrap`'s loopback. | all |
| `ProtectClock`, `ProtectKernelModules`, `ProtectControlGroups` | No setting the clock, no loading modules, a read-only `/sys/fs/cgroup`. | all |
| `PrivateTmp=yes` | The unit's `/tmp` and `/var/tmp` are its own. | all |
| `ProtectSystem=strict`, `ProtectHome=read-only`, `ReadWritePaths=%h/agent/state` | The file system is read-only except the state directory and the unit's runtime directory. The agent cannot change `agent.yaml`, `tokens/`, `releases/` (so not the program the unit starts), the units in `~/.config/systemd/user`, `~/.ssh` or a shell profile. The dashboard template has no `ReadWritePaths`: it writes nothing. | all |
| `RestrictSUIDSGID`, `ProtectKernelTunables`, `RestrictNamespaces=yes` | The options that break `bwrap` ([below](#what-breaks-bwrap)). | every unit but `solos-agent.service` |

`ProtectSystem=strict` alone leaves the home writable in a user unit, so `ProtectHome=read-only` is what confines the
writes. A deploy makes `~/agent/state` before it starts the agent, because a unit whose `ReadWritePaths` is missing does
not start (`226/NAMESPACE`).

`systemd-analyze --user security --offline=true` scores the units 9.4 to 9.6 (UNSAFE) before and 6.7 for
`solos-agent.service` and 5.7 for the helpers (MEDIUM) after.

To check a running agent: `systemctl --user show solos-agent -p NoNewPrivileges -p ProtectHome -p ProtectSystem`, and
`grep -E 'NoNewPrivs|Seccomp' /proc/$(systemctl --user show -p MainPID --value solos-agent)/status` should say
`NoNewPrivs: 1` and `Seccomp: 2`.

### What breaks bwrap

These are not in `solos-agent.service`, and a test (`tests/agent_deploy.rs`) keeps `RestrictSUIDSGID`,
`ProtectKernelTunables` and `MemoryDenyWriteExecute` out of it.

| Option | What happens | Why |
|---|---|---|
| `RestrictSUIDSGID=yes` | `bwrap: Can't open source /usr: Function not implemented` | systemd cannot inspect the argument of `openat2()`, so it answers `ENOSYS`, and bubblewrap 0.12 opens its bind sources with `openat2()` and does not fall back. Denying only `openat2` gives the same error. `NoNewPrivileges` already stops a setuid file from granting anything. |
| `ProtectKernelTunables=yes` | `bwrap: Can't mount proc on /proc: Operation not permitted` | systemd mounts `/proc/sys` and other paths read-only over `/proc`, and the kernel then refuses a fresh `proc` mount in `bwrap`'s user namespace. A user without privileges cannot write them anyway. |
| `RestrictNamespaces=user mnt pid net ipc uts` | `bwrap: No permissions to create a new namespace` | `--unshare-all` also asks for a cgroup namespace. The list needs `cgroup`. |
| `MemoryDenyWriteExecute=yes` | Pi's `node` dies in V8 (exit 133) | V8 needs memory that is writable and executable. |
| `SystemCallFilter=@system-service` | The sandbox dies of `SIGSYS` | `bwrap` needs `mount`, `pivot_root` and others outside the list. `~@privileged` fails the same way (exit 255): `bwrap` needs `capset`. |
| `CapabilityBoundingSet=` (empty) | `218/CAPABILITIES` | A user manager can drop capabilities only inside a user namespace, so it works only beside another option here. The process has no capabilities to drop, and `NoNewPrivileges` keeps it from gaining any. |
| `ProtectHome=yes` or `tmpfs` | The unit cannot find `~/agent` | They hide `/home` and `/run/user`, where the agent and its runtime directory are. |

`PrivateDevices`, `ProtectProc=invisible`, `ProcSubset=pid`, `ProtectHostname` and `PrivateIPC` also work with `bwrap`.
They are not set, for little gain.

After a systemd or bubblewrap upgrade, or to try another option, run `bwrap` the way a worker does inside a transient
unit with the option, and the agent's own check:

```sh
ssh <box> 'systemd-run --user --wait --pipe -p NoNewPrivileges=yes -p ProtectHome=read-only \
  /usr/bin/bwrap --unshare-all --die-with-parent --ro-bind /usr /usr --ro-bind-try /lib /lib --ro-bind-try /lib64 /lib64 \
  --symlink usr/bin /bin --proc /proc --dev /dev /usr/bin/true'
ssh <box> 'systemd-run --user --wait --pipe -p NoNewPrivileges=yes -p ProtectHome=read-only \
  ~/agent/current/bin/solos-swarm doctor --pi-root ~/agent/current --node ~/.local/opt/node-v24.18.0-linux-x64/bin/node'
```

`doctor` ends with `"verdict":"PASS"`. Transient units start nothing of the agent's and go away when they exit.

### What hardening does not do

The units limit the agent's processes. They do not change what the user owns. The agent can still read everything the
user can read: its whole home, including `~/.ssh`, and every other service's files that the user's groups open. On a
box where the agent's user is also the owner of other production services, with sudo, that is a lot. Hence the next
section.

If the agent needs a path outside `~/agent/state` that is not covered here, such as a `trail.path` outside it, add a
drop-in rather than editing the unit, because a deploy rewrites the unit: `systemctl --user edit solos-agent`, then
`[Service]` and `ReadWritePaths=/that/path`. A tool that dies with `Operation not permitted` on a system call, or on
a write to a path it used to write, points at one of these limits.

## Run the agent as its own user

Run the agent as a user that has no sudo, no extra groups and owns nothing else. Then an escape from a worker, or code
that runs on the host from a repository, reaches the agent's own files and the network and nothing more. Today's box
runs it as the main user, which has passwordless sudo and owns the other services.

Nothing in the repository assumes a user name or uid. `scripts/agent-deploy` logs in as `AGENT_USER` when it is set,
and as the SSH config's user otherwise. This section moves an agent from one user (`debian` below) to a new one
(`solos-agent`). Do it when the agent has nothing running that you mind interrupting, and plan for the downtime of one
cold build: the new user builds its own release. The old user's `~/agent` stays untouched until you are sure, so
rolling back is starting it again.

### 1. Make the user

On the box, as a user with sudo. The user gets no `-G`, so no supplementary groups, and no sudo:

```sh
sudo useradd --create-home --shell /bin/bash --user-group solos-agent
sudo loginctl enable-linger solos-agent
id solos-agent                       # one group, solos-agent
sudo -l -U solos-agent               # "User solos-agent is not allowed to run sudo"
ls -d /run/user/$(id -u solos-agent) # exists: lingering started its systemd
```

From the Mac, put the key the Mac logs in with into the new user's `authorized_keys`, and check that it works:

```sh
ssh remote-ai "sudo -Hu solos-agent sh -c 'umask 077; mkdir -p ~/.ssh; cat >> ~/.ssh/authorized_keys'" < ~/.ssh/<key>.pub
ssh -l solos-agent remote-ai id
```

`useradd` leaves the password locked, and keys still work because sshd uses PAM on Debian. If SSH refuses the account,
set its password field to `*` with `sudo usermod -p '*' solos-agent`. `-l` beats the `User` of your ssh config for that
one command, so the `remote-ai` alias and its key serve both users.

### 2. Give it the build tools

As the new user (`ssh -l solos-agent remote-ai`), the same as [Before you start](#before-you-start): rustup with
toolchain 1.96.0, Node 24.18.0 at `~/.local/opt/node-v24.18.0-linux-x64/`, and `bun` at `~/.bun/bin/bun` for a repository
with `deps`. `bwrap` and `prlimit` are the box's. Nothing is shared with the old user's home, which the new user cannot
read.

### 3. Stop the old agent

From the Mac, as the old user. Only one process may hold the Discord bot's token, so the old agent stops first, and its
units are disabled so that neither a reboot nor the old health check brings it or its alerts back:

```sh
scripts/agent-deploy releases                  # note the release marked (current)
scripts/agent-deploy stop                      # drains, then stops the publisher and the agent
ssh remote-ai 'systemctl --user disable --now solos-agent solos-agent-publish solos-agent-health.timer solos-agent-prune.timer'
```

Leave out `solos-agent-publish` if you never turned on the dashboard.

### 4. Copy the data

On the box, as a user with sudo. Copy `agent.yaml`, the tokens, the state and the backups. Do not copy `releases/`,
`current`, `previous`, `target/`, `src/` or `src.git/`: the links are absolute paths into the old home, and the new
user builds its own.

```sh
sudo install -d -m 0700 -o solos-agent -g solos-agent /home/solos-agent/agent
sudo cp -a /home/debian/agent/{agent.yaml,tokens,state,backups} /home/solos-agent/agent/
sudo rm -f /home/solos-agent/agent/state/broker.sock
sudo chown -R solos-agent:solos-agent /home/solos-agent/agent
```

Then edit `/home/solos-agent/agent/agent.yaml`. Every `/run/user/<uid>/` path (`token_file`, `key_file`) takes the new
uid, and every path under `/home/debian/` takes one the new user can read, such as `run_code` mounts of Node and `bun`:

```sh
sudo sed -i -E "s|/run/user/[0-9]+/|/run/user/$(id -u solos-agent)/|g; s|/home/debian/|/home/solos-agent/|g" \
  /home/solos-agent/agent/agent.yaml
sudo grep -n '/home/\|/run/user/' /home/solos-agent/agent/agent.yaml   # nothing of the old user's is left
```

A dataset or a repository checkout that only the old user can read is not readable by the new one. Give it exactly
that, read-only, with an ACL (`sudo setfacl -R -m u:solos-agent:rX <dir>`, and `x` on each parent directory), or
copy it. Do not add the user to a group to make it work.

The state has no path of the old home that the agent needs: a Ticket's clone has the GitHub URL as its `origin`, and
the dependency caches link to each other relatively.

### 5. Deploy as the new user

Deploy the revision the old agent runs, from step 3, so that a rollback never meets a newer `agent.db` schema. Upgrade
afterwards, when it has run for a while:

```sh
AGENT_USER=solos-agent scripts/agent-deploy deploy <the sha from step 3>
```

This sends the release to the new user's `~/agent`, builds it, installs the units into the new user's
`~/.config/systemd/user`, enables them, starts the agent and waits until it is healthy. It refuses to switch, before it
stops anything, if `agent.yaml` names another uid's `/run/user` directory.

### 6. Check it

```sh
AGENT_USER=solos-agent scripts/agent-deploy status             # "user: solos-agent (uid ...)", the release, no health incident
ssh -l solos-agent remote-ai 'id; sudo -n true; systemctl --user show solos-agent -p NoNewPrivileges -p ProtectHome'
```

`id` shows one group, `sudo -n true` is refused, and the unit shows `NoNewPrivileges=yes` and
`ProtectHome=read-only`. Then see that the agent works: a message in its Discord channel gets an answer, and a small
Ticket on a repository runs a worker to a Report. Once it does, make the new user the default. Either `export
AGENT_USER=solos-agent` in the Mac's shell profile, or give the new user its own ssh alias with `User solos-agent` and
set `AGENT_HOST` to it.

### 7. Clean up the old user

After the agent has run as the new user for several days: remove the old agent's units and files, and replace the
tokens the old home held, because they stay readable to its owner.

```sh
ssh remote-ai 'rm -f ~/.config/systemd/user/solos-agent*.service ~/.config/systemd/user/solos-agent*.timer; \
  rm -rf ~/.config/systemd/user/solos-agent-publish.service.d; systemctl --user daemon-reload; \
  shred -u ~/agent/tokens/* ; rm -rf ~/agent'
```

Rotate the Discord token, the GitHub App's key, the model key and the Blob token if you want the old copies dead
([Store a token](#store-a-token), [Rotate the dashboard's secrets](#rotate-the-dashboards-secrets)).

### Roll back

Before step 7 the old user's `~/agent`, its release and its units are as they were. Stop the new agent, disable its
units so a reboot does not start them, and start the old ones:

```sh
AGENT_USER=solos-agent scripts/agent-deploy stop
ssh -l solos-agent remote-ai 'systemctl --user disable --now solos-agent solos-agent-publish solos-agent-health.timer solos-agent-prune.timer'
ssh remote-ai 'systemctl --user enable --now solos-agent solos-agent-publish solos-agent-health.timer solos-agent-prune.timer'
scripts/agent-deploy status
```

Leave out the publisher in both lines if you never turned it on, and unset `AGENT_USER` for the last command. The old
agent comes back with the state it had at step 3. What the new agent did since is in the new user's `state/`. Copy it
back only if you must, with the agent stopped, and only to the same release: an older binary refuses a newer schema.

## Reference

### Layout on the box

| Path under `~/agent` | What it is |
|---|---|
| `releases/<sha>/` | One read-only build of a commit: `bin/solos-swarm`, `node_modules`, `extensions`, `vendor/pstack`, `scripts/systemd`. The newest five are kept, plus `current` and `previous`. |
| `current`, `previous` | Links to the release that serves and the one before it. `--pi-root` is `current`. |
| `agent.yaml` | The production config. Not in git. |
| `state/` | The agent's state, kept across releases ([below](#state-directory)). |
| `tokens/` | One file per one-use credential, mode 0600 ([Tokens](#tokens)). |
| `backups/<time>-<sha>/` | `agent.db*`, copied before every deploy and rollback while the agent is stopped. The newest twenty are kept. |
| `src.git/` | A bare repository. Each deploy pushes its commit here as `refs/deploy/<sha>`; the newest twenty refs are kept. |
| `src/` | The one tree every build compiles in, updated in place by `git checkout`. Not pruned. |
| `target/` | The cargo build cache, shared by every build and kept between deploys. Not pruned, and not under `~/work/ci/target-*`, which the box's CI sweep removes. |
| `build.lock`, `build.log` | The lock that makes builds take turns, and the last build's cargo output. |
| `.agent-deploy` | The copy of `scripts/agent-deploy` that each command installs and runs on the box. |

### State directory

| Path under `~/agent/state` | What it is |
|---|---|
| `agent.db` (and `-wal`, `-shm`) | The Store. Only `agent serve` writes it. |
| `tickets/<id>/` | Each Ticket's clone (`work/`), transcripts and host files. |
| `router/` | The Router's transcripts. |
| `forge/<owner>/<name>.git` | The host's mirror of each repository. |
| `forge-stage/<ticket>/` | The bundles of a worker's commits that the host admitted, beside the mirrors. |
| `deps/<owner>/<name>/<key>/` | A repository's installed dependencies. |
| `trail/` | The browser's trail blobs, when a browser is configured. |
| `mcp/` | `commands.jsonl` (operator commands such as `agent stop`), `inbox.jsonl` and `outbox.jsonl` (the `mcp` Source and Channel). |
| `broker.sock` | The broker's socket. The health check asks it `GET /api/hello`. |
| `github-identities.json`, `github-cursor.json` | The pinned GitHub accounts, and what the github Source has already read. |
| `dream-slot` | The last day's dream slot, so a restart asks for no second dream. |
| `publish-state.json` | The publisher's hash of each blob it uploaded. |
| `health-state.json`, `health-hold` | The health check's memory, and the hold a deploy writes. |
| `PAUSE`, `control.json` | Pause gates. The health check writes `PAUSE`, a drain writes `control.json`, and a person may write either. |

### Units

`scripts/agent-deploy` copies each unit from `releases/<sha>/scripts/systemd/` into `~/.config/systemd/user/` on every
switch.

| Unit | Runs | When | Runtime directory, tokens |
|---|---|---|---|
| `solos-agent.service` | `agent serve` | Always. Restarts 15 s after a failure. When systemd gives up (five starts in 15 minutes), the unit stays failed and the health check runs. | `/run/user/<uid>/solos-agent/`: every file in `tokens/` except `vercel-blob-token` |
| `solos-agent-publish.service` | `agent publish` | Always, when `agent.yaml` has `web.publish`. Restarts 30 s after a failure; five starts in 15 minutes leave it failed. | `/run/user/<uid>/solos-agent-publish/`: `vercel-blob-token` |
| `solos-agent-health.service`, `.timer` | `agent health` | 2 minutes after boot, then 3 minutes after each start; and on the agent's failure | `/run/user/<uid>/solos-agent-health/`: `discord-token` |
| `solos-agent-prune.service`, `.timer` | `agent prune --keep-days 14` | Daily at 04:30, up to 15 minutes late, made up after downtime | none |

The publisher, the health check and the prune run with `TOKIO_WORKER_THREADS=2`. The box has 128 cores, and one runtime
thread per core filled the publisher's `TasksMax` and hung its uploads.

### `scripts/agent-deploy`

| Command | What it does |
|---|---|
| `deploy [REV]` | Builds `REV` (default `origin/main`) as a release, then switches to it ([Deploy a release](#deploy-a-release)). |
| `prebuild [REV]` | The build of a deploy and nothing else: nothing is drained, stopped or switched. A later `deploy` of the same commit, or of one with the same files, finds the release built. |
| `rollback` | Switches to `previous`. |
| `start`, `stop`, `restart` | The agent's unit, with the publisher and the timers. `stop` and `restart` drain. |
| `status` | The user, the release, the unit, `agent status`, the publisher, the timers, any health incident, and `agent health --dry-run`. |
| `logs [ARGS]` | `journalctl` for the agent. Default `-n 100`. Any `journalctl` arguments, such as `-f` or `--since 1h`. |
| `publish-logs [ARGS]`, `health-logs [ARGS]`, `prune-logs [ARGS]` | The same for the publisher, the health check and the prune. |
| `token NAME` | Stores stdin as `~/agent/tokens/NAME`, mode 0600. `NAME` is letters, digits, `.`, `_` and `-`. |
| `releases` | Lists the releases, newest first, with `current` and `previous` marked. |

| Option or variable | Effect |
|---|---|
| `--drain SECONDS` | How long a drain waits. Default 90. |
| `--no-drain` | No drain: stop at once. |
| `AGENT_DRAIN_SECS` | The default for `--drain`. |
| `AGENT_HOST` | The box. Default `remote-ai`. |
| `AGENT_USER` | The user the agent runs as: SSH logs in with `-l`, which beats the `User` of your ssh config. Default: whatever the config or the Mac's login says. Letters, digits, `_` and `-`. |

On the box, `bash ~/agent/.agent-deploy --on-host drain [SECONDS]`, `--on-host resume` and
`--on-host running-assignments` run one piece of a deploy alone. The tests use them.

### Tokens

| File in `tokens/` | What it is | Read by |
|---|---|---|
| `model.key` | The DeepSeek key, for the Router and the workers | `agent serve --credential-file` |
| `discord-token` | The Discord bot token | `agent serve` (the discord Source's `token_file`) and `agent health` |
| `github-app.pem` | The GitHub App's private key | `agent serve` (`github.key_file`) |
| `vercel-blob-token` | The Blob store's read-write token | `agent publish` (`web.publish.token_file`) |

### Health checks

| Check | Fails when |
|---|---|
| `unit` | `systemctl --user show solos-agent` says anything but `active` (inactive, failed, or `activating` in its restart delay), the unit file is gone, or systemd cannot be asked |
| `restarts` | systemd's `NRestarts` rose by `health.restart_burst` (3) or more since the last run: a crash loop that looks active |
| `broker` | the unit has been up for 60 s and `GET /api/hello` on `state/broker.sock` does not answer 200 within 5 s |
| `disk` | free space on the state directory's disk is under `health.min_free_gb` |
| `discord` | the bot token cannot read the Watchdog's channel: the network is down, the token is refused, or the bot lost access. Nothing is sent to find out |
| `config` | `agent.yaml` does not load, the likeliest reason for a crash loop right after an edit |
| `claude` | Only when `executor.claude` is set and `claude` is installed there, or `executor.harness` is `claude-code`. `claude auth status --json` says `loggedIn: false`, or its `loginExpiresAt` has passed, or it cannot be run or does not answer within 20 s, or (with `harness: claude-code`) the binary is missing. It starts no session, so it spends no tokens |

### Prune

| What | Removed when | Never |
|---|---|---|
| `tickets/<id>/work`, with `host/repo.json` and `prepared` | the Ticket ended more than N days ago (its end time is in the Store) | an open Ticket; a directory the Store has no Ticket for; `pi-settings.json`; a swarm's `swarm/` books |
| `tickets/<id>/*.rpc.jsonl`, `*.stderr` | the same | |
| `forge/<owner>/<name>.git` | nothing has fetched into it for N days (`FETCH_HEAD`, `HEAD`, `config`, `packed-refs`) | a mirror used since. The next Ticket on that repository makes it again |
| `deps/<owner>/<name>/<key>` | no Ticket has mounted it for N days (a Ticket's start marks `manifest.json` used); a `tmp-*` directory of a build that died, by its age | a cache used since. The next Ticket on that repository builds it again |
| `router/*.rpc.jsonl`, `*.stderr` | not written for N days | the newest of each kind, which a live session may be writing |
| `trail/<id>/` | only with `--include-trail`, for Tickets that ended more than N days ago. `agent trail` then reports their blobs missing | by default |
| `agent.db` and its WAL, `tokens/`, `backups/`, `releases/`, the vault, `browser/`, `~/agent/target` | | always |

The markers go with the clone because a person's follow-up in its thread can resume an ended Ticket. With `repo.json`
or `prepared` left behind, the worker would find an empty `/work` and believe it was ready. Without them it clones
again from the mirror, or from GitHub. `--dry-run` sizes are allocated blocks, which is what removing frees.

### `agent stop`

| Exit | Meaning |
|---|---|
| 0 | stopped, or already stopped or ended (`completed`, `dropped`) |
| 1 | the agent refused (the Policy does not name the operator), or the Ticket does not exist |
| 2 | no answer within `--wait-secs` (20). The agent is not running or not reading. The command stays filed, and a running agent applies it if it reads it within 10 minutes. An older command is not applied, so a restart never carries out last week's |

`agent memo add`, `retire` and `forget` also exit 0 when done, 1 when the agent refused, and 2 with no answer in time.

### Publisher failures

| What | What happens |
|---|---|
| Vercel Blob or the network is down | Each upload is tried five times with a doubling wait, honouring `retry-after`. Then the look fails, and the next look, after a growing wait of up to five minutes, tries the same uploads again. Nothing is lost, and the Store is not touched. |
| The token is refused (401, 403) | The publisher exits with that message and stays down ([Rotate the dashboard's secrets](#rotate-the-dashboards-secrets)). |
| The API refuses one blob (another 4xx) | It is logged and skipped until that ticket's snapshot changes. The others and the index still go up. |
| `agent.db` is missing, newer than this build understands, or being restored | The look fails with the reason and is tried again. A replaced file is noticed and read again from the start. |
| The agent restarts | Nothing. The publisher holds no connection to it. |
| The disk or the state directory is read-only | The look fails when it cannot write `publish-state.json`. What was uploaded stays, and it is uploaded again after the fix, which is harmless. |

### Limits

- The disk floor pauses work and frees nothing. The health check, not `agent serve`, checks it, every three minutes.
- The health check runs on the box it watches. Nothing alerts if the box or the user's systemd is gone.
- A drain narrows what a deploy interrupts. A worker in the middle of a long tool call is still stopped.
- The host does not run a repository's `evidence` command itself. The worker does, and the host checks the shape of its
  block. A pull request's description is written once, so a later head keeps the first head's evidence.
- Nothing merges, a CI failure does not resume a delivered Ticket, and no timer reads open pull requests again yet.
  Those are PRs 3 to 5 of [the GitHub App design](github-app/prs.md).
