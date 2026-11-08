# Running agent mode in production

This runbook is for the person who operates the always-on agent. One box runs `solos-swarm agent serve` as the systemd
user unit `solos-agent`, with three helper units beside it: the dashboard publisher, the health check and the daily
prune. You run `scripts/agent-deploy` on the Mac, in this repository, and it does the work on the box over SSH. The box
is `remote-ai` unless `AGENT_HOST` names another. Everything on the box is under `~/agent`
([layout](#layout-on-the-box)).

What each config key means is in [the agent-mode plan](README.md). This page says how to run the agent. The
[Reference](#reference) at the end lists the units, the commands, the files and the limits.

## Before you start

Once per box:

1. Give the Mac SSH access to the box with a key. `scripts/agent-deploy` connects with `ssh -o BatchMode=yes`, so it
   cannot answer a password prompt.
2. Install the build tools for the user that runs the agent: rustup with toolchain 1.96.0 under `~/.cargo/bin`, `npm`,
   and Node 24.18.0 at `~/.local/opt/node-v24.18.0-linux-x64/`.
3. Install `bwrap` and `prlimit`, which the sandboxes need. For a repository with `deps`
   ([below](#install-a-repositorys-dependencies-and-require-evidence)), also install `bun` at `~/.bun/bin/bun`.
4. Turn on lingering for that user (`loginctl enable-linger <user>`), so its units start at boot with nobody logged in.
5. Make the directory the units go in: `mkdir -p ~/.config/systemd/user`.
6. Write `~/agent/agent.yaml` ([the production config](#the-production-config)) and store the tokens it names
   ([Store a token](#store-a-token)). A deploy refuses to switch without the config.

The units' runtime directories are `/run/user/1000/...`, so the examples assume the agent runs as uid 1000.

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
`<...>`:

```yaml
# ~/agent/agent.yaml on the box
policy:
  people: [discord:<your Discord user id>, mcp:<your name>, github:<your GitHub login>]
  approvers: [discord:<your Discord user id>, github:<your GitHub login>]
  auto_approve_microusd: 500000            # a budget above this waits for Approve
sources:
  - kind: discord
    guilds: [<server id>]
    token_file: /run/user/1000/solos-agent/discord-token
  - kind: mcp                              # `solos-swarm mcp`, `agent stop` and `agent memo` on the box
  - kind: github                           # no fields: it reads forge.repos as the App
channels:
  - kind: discord
  - kind: mcp
  - kind: github                           # comments as the App's bot
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
      deps: {kind: bun}                    # the host installs bun.lock's dependencies
      evidence: {command: "bun run solos dev verify --scope unit --json"}
    GuiBibeau/solos-swarm:
      about: solos-swarm, the agent's own code (Rust and TypeScript).
      base: main                           # no owes: it defaults to answer, so the agent only reads here
memory:                                    # optional; these are the defaults
  proposals: true                          # workers propose memos, and a review decides each
  review: {thinking: max}
  dream: {enabled: true, at: "03:30", thinking: max}   # UTC, before the 04:30 prune
  # daily_microusd: 500000                 # the review's and the dream's cap per UTC day; absent: no cap
watchdog:
  operator: {source: discord, address: {channel: <a channel id you read>}}
  every: 1m
health:
  min_free_gb: 50                          # pause new work below 50 GB free on the state directory's disk
web:
  public_url: https://agent.guibibeau.com
  publish:
    token_file: /run/user/1000/solos-agent-publish/vercel-blob-token
```

Read these before you change it:

- Every `token_file` and `github.key_file` names the unit's runtime copy, as [Store a token](#store-a-token) says.
  `serve` refuses a `key_file` under any path a sandbox can read.
- `mcp:<your name>` in `policy.people` lets you run `agent stop` and `agent memo` from the box.
- There is no `budgets:` section, so a session Ticket and the Router have no cap. A swarm's budget comes from the Router
  or from `executor.swarm.default_budget_microusd`. To cap them, add
  `budgets: {session_microusd: 500000, router_daily_microusd: 3000000}` ([Budgets](concepts.md#budgets)).
- A Ticket from GitHub, MCP or a schedule has no Approve button, so its budget is capped at `auto_approve_microusd`.
- `watchdog.operator` is where the Watchdog's alerts, the dream's daily digest and the health check's posts go. The
  health check posts only to a `source: discord` operator.
- Write `web.publish` in block style, as above. `scripts/agent-deploy` looks for a `publish:` line under `web:` to know
  whether to run the publisher.
- `memory:` and `health:` can be left out. Their defaults are in [memory](configuration.md#memory) and
  [The disk floor](#the-disk-floor).

The sections below say how to set up GitHub, dependencies, memory and the dashboard. The keys are in
[github](configuration.md#github), [forge](configuration.md#forge), [memory](configuration.md#memory),
[watchdog](configuration.md#watchdog) and [web](configuration.md#web).

## Deploy a release

```sh
scripts/agent-deploy deploy            # origin/main
scripts/agent-deploy deploy <rev>      # any commit, branch or tag
```

A deploy does these steps, in order:

1. It fetches `origin`, resolves the revision to a 12-digit sha, and sends `git archive` of it to the box. The box skips
   the build when that release is already built.
2. It refuses to build with less than 30 GB free.
3. It builds on the box: `npm ci`, then `cargo build --locked --release` with toolchain 1.96.0 and the build cache
   `~/agent/target`. It runs `doctor` on the new release and makes the release read-only.
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

A deploy that went well ends like this:

```text
backed up agent.db to /home/debian/agent/backups/20261107T142301Z-ebb9f39ae70a
release 53f30cbf570b is serving (pid 812345)
unpaused
publisher: active
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
  `.github/**`). Nothing merges. The config accepts `merge`, `ci`, `max_repairs` and `github.reconcile_secs`, and no
  release acts on them yet. A config with `owes: pull_request` and `github.writes: false` does not load.

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

## Memory and the dream

Memory is on by default wherever workers run on Pi. Workers propose memos, a review call at DeepSeek's highest thinking
effort decides each one, and once a day the dream checks the memos against the transcripts of the Tickets that ended.
How it works is in [Memory](runtime.md#memory) and [The dream](runtime.md#the-dream).

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
                                                                                                  |
                                       you  -->  agent.guibibeau.com (web/agent-dashboard)  <--reads, server side
```

`solos-swarm agent publish` runs as the unit `solos-agent-publish`. It opens `agent.db` read-only and looks at it every
`web.publish.every_secs` (5). A look that finds nothing written costs one SQLite pragma. When the Store changed, it
builds `tickets/index.json` (the open tickets first, then the latest 100 ended) and one `tickets/<ticket id>.json` per
ticket in the index. It uploads only a snapshot whose content changed, tickets first and the index last, so the index
never names a blob that is not there. When nothing changed, it uploads the index again every `heartbeat_secs` (300),
which is how the dashboard tells a quiet agent from a dead publisher. Its only file is
`state/publish-state.json`, the hash of each blob it last uploaded to the store its token names. A different store gets
everything again. The web app is [web/agent-dashboard](../../web/agent-dashboard/README.md).

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
   ```

4. Run `scripts/agent-deploy deploy`, or `scripts/agent-deploy restart` on the current release.
5. Check it:

   ```sh
   scripts/agent-deploy status           # "publisher: active (configured: yes)", "dashboard index last uploaded 3s ago"
   scripts/agent-deploy publish-logs     # "publish: uploaded tickets/tk_...json, tickets/index.json (2 unchanged)"
   ```

From then on, `deploy`, `rollback`, `start`, `stop` and `restart` install and restart the publisher with the agent. They
stop it while `agent.db` is backed up or restored, so it never reads a file being replaced.

The publisher's unit copies `tokens/vercel-blob-token`, and only that, into its own runtime directory,
`/run/user/1000/solos-agent-publish/`. `agent publish` reads it once and deletes it. The agent's unit does not copy this
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
`/run/user/1000/solos-agent-health/` before each run, and the directory goes when the run ends. The check never starts,
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
2. Run `cargo clean` in `~/agent/target`, the build cache. The prune never touches it, and the next deploy rebuilds it.
3. Delete old releases or backups only by hand. A deploy keeps five releases and twenty backups.

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
| `target/` | The cargo build cache. Not pruned. |
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
| `solos-agent.service` | `agent serve` | Always. Restarts 15 s after a failure. When systemd gives up (five starts in 15 minutes), the unit stays failed and the health check runs. | `/run/user/1000/solos-agent/`: every file in `tokens/` except `vercel-blob-token` |
| `solos-agent-publish.service` | `agent publish` | Always, when `agent.yaml` has `web.publish`. Restarts 30 s after a failure; five starts in 15 minutes leave it failed. | `/run/user/1000/solos-agent-publish/`: `vercel-blob-token` |
| `solos-agent-health.service`, `.timer` | `agent health` | 2 minutes after boot, then 3 minutes after each start; and on the agent's failure | `/run/user/1000/solos-agent-health/`: `discord-token` |
| `solos-agent-prune.service`, `.timer` | `agent prune --keep-days 14` | Daily at 04:30, up to 15 minutes late, made up after downtime | none |

The publisher, the health check and the prune run with `TOKIO_WORKER_THREADS=2`. The box has 128 cores, and one runtime
thread per core filled the publisher's `TasksMax` and hung its uploads.

### `scripts/agent-deploy`

| Command | What it does |
|---|---|
| `deploy [REV]` | Builds `REV` (default `origin/main`) as a release, then switches to it ([Deploy a release](#deploy-a-release)). |
| `rollback` | Switches to `previous`. |
| `start`, `stop`, `restart` | The agent's unit, with the publisher and the timers. `stop` and `restart` drain. |
| `status` | The release, the unit, `agent status`, the publisher, the timers, any health incident, and `agent health --dry-run`. |
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
