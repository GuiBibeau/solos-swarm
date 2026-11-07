# Running agent mode in production

One box runs `solos-swarm agent serve` as the systemd user unit `solos-agent`. `scripts/agent-deploy` builds and
switches releases from the Mac. Everything on the box is under `~/agent`:

| Path | What it is |
|---|---|
| `releases/<sha>/` | One immutable build of a commit: `bin/solos-swarm`, `node_modules`, `extensions`, `vendor/pstack`. Read-only. The newest five are kept, plus current and previous. |
| `current`, `previous` | Links to the release that serves and the one before it. `--pi-root` is `current`. |
| `agent.yaml` | The production config. It is not in git. |
| `state/` | `agent.db`, `tickets/`, `router/`, `forge/`, `deps/`, `trail/`, `mcp/`, the broker socket, and the operator's files: `health-state.json`, `health-hold`, `PAUSE`, `control.json`. Kept across releases; finished work is pruned daily ([Pruning state](#pruning-state-and-the-disk-floor)). |
| `tokens/` | One file per one-use credential, mode 0600: `model.key` (the DeepSeek key), `discord-token`, `github-app.pem` (the GitHub App's private key), and `vercel-blob-token` (the dashboard publisher's, see [Dashboard](#dashboard)). |
| `backups/` | `agent.db` copied before every deploy and rollback, while the agent is stopped. The newest twenty are kept. |
| `target/` | The cargo build cache. |

`agent serve` reads each token file once and deletes it. The unit's `ExecStartPre` copies `tokens/` into its private
tmpfs runtime directory, `/run/user/1000/solos-agent/`, before every start, so a crash restart or a reboot starts
again without a person. That directory goes away when the unit stops. No sandbox sees `~/agent/tokens` or the runtime
directory. The tokens rest unencrypted at mode 0600 in the agent user's home, like the vault key.

## Commands (from the Mac, in the repository)

```sh
scripts/agent-deploy deploy            # build origin/main, drain, back up agent.db, switch, restart, wait until healthy
scripts/agent-deploy deploy <rev>      # any commit, branch or tag
scripts/agent-deploy rollback          # back to the previous release
scripts/agent-deploy status            # the unit, its release, `agent status` (open Tickets, spend, alerts), the health check and prune timers, and the dashboard publisher
scripts/agent-deploy logs              # the last 100 log lines; `logs -f` follows, `logs --since 1h`
scripts/agent-deploy publish-logs      # the same for the dashboard publisher, when there is one
scripts/agent-deploy health-logs       # the same for the health check
scripts/agent-deploy prune-logs        # the same for the daily prune: its last line says what it freed
scripts/agent-deploy restart           # also: start, stop, releases
scripts/agent-deploy deploy --drain 300   # wait up to 5 minutes for running work to reach a Report (default 90 s)
scripts/agent-deploy rollback --no-drain  # stop at once; for an emergency
pbpaste | scripts/agent-deploy token discord-token   # store or rotate a token, then restart
scripts/agent-deploy token github-app.pem < ~/Downloads/<name>.<date>.private-key.pem   # the GitHub App's key
```

A deploy builds on the box with the pinned toolchain (`cargo build --locked`, `npm ci`), runs `doctor` on the new
release, refuses to build with less than 30 GB free, drains the agent (below), stops it, copies `agent.db`, switches
`current`, installs the unit files from the release and starts it. Healthy means the unit is active with its broker
socket and keeps the same process for 20 s. If the new release does not get there, the deploy switches back to the
previous release, restores the `agent.db` it copied (a new release may have migrated it), starts that and exits 1.

### Draining on deploy

`deploy`, `rollback`, `restart` and `stop` drain before they stop the unit:

1. They write `state/control.json` as `{"mode":"pause","by":"agent-deploy",...}`, the pause gate `PAUSE` also is. The agent
   starts no new Eval and performs no Effect, and the broker answers every worker's next model call with the pause
   reply, so each worker finishes the call it is in and then waits.
2. They ask `agent status --json` of the running release every 3 seconds, for up to 90 seconds, until each Assignment
   that was running when the pause began has sent a Report (any status: a Report is a turn boundary the Store holds) or
   has ended.
3. They stop the unit and carry on as before. When the new release is healthy they remove `control.json`, but only a file
   marked `"by":"agent-deploy"`: a pause a person wrote is never replaced or lifted. The agent that starts next after a
   `stop` starts unpaused.

Trade-offs, so nobody is surprised:

- **It narrows the loss, it does not remove it.** A worker that is partway through a long tool call (a build, a test run)
  does not reach a boundary in 90 seconds and is killed by the stop, as before; it recovers on the next start as `host
  restarted` and is told to check `/work` before repeating anything. A drain that times out says how many Assignments were
  still mid-turn and stops anyway: a deploy never waits forever.
- **A deploy costs up to the drain time more.** The build happens before the drain, so the agent is paused from the
  drain until the new release is healthy: the drain time, the restart, and 20 seconds. `--drain SECONDS` (or
  `AGENT_DRAIN_SECS`) changes the wait; `--no-drain` skips it. Use `--no-drain` for an emergency rollback.
- **Messages still arrive.** While paused the Sources keep reading and storing; they are judged after the pause lifts. A
  Discord message sent during the few seconds the agent is down is lost (a new Gateway session does not replay).
- **The pause is the same switch an operator has**, so `agent status` shows `pause: held by control.json (mode pause)`
  during a drain, and the Watchdog's own pause alert (after an hour) would fire for a drain that was left behind. If a
  deploy fails and the agent that is running was left paused, the script says so: look at `~/agent/state/control.json`
  and, once the agent is right, `rm` it (it names `"by":"agent-deploy"` when it is the drain's).
- **It uses only what every release has**: `agent status --json` of the release that is running, so the first deploy that
  contains this already drains.

**Rollback and the schema.** A release that migrated `agent.db` to a newer schema cannot be rolled back to a binary
that knows only the older one: the older binary refuses the file and `rollback` says so. Then stop the agent, copy
the matching `agent.db*` back from `backups/` into `state/`, and roll back. A Ticket accepted without a budget (no
`budgets:` section) stores none, and a binary from before budgets were optional fails to read it. The schema version
is the same, so `rollback` does not notice: roll back only when no such Ticket is open, or restore `agent.db*` from
`backups/` as above.

## The production config

```yaml
# ~/agent/agent.yaml on the box
policy:
  people: [discord:<your user id>, mcp:<your name>]
  approvers: [discord:<your user id>]
  auto_approve_microusd: 500000           # above this a swarm's budget waits for Approve
sources:
  - kind: discord
    guilds: [<server id>]
    token_file: /run/user/1000/solos-agent/discord-token
  - kind: mcp                              # `solos-swarm mcp --state-dir ~/agent/state --operator <your name>`
channels:
  - kind: discord
  - kind: mcp
router:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1320000, cache_hit_microusd_per_million: 30000}
executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1320000, cache_hit_microusd_per_million: 30000}
  max_workers: 4
  run_code: {}
  # fetch_url reaches the public internet only: loopback, private, link-local and 100.64/10 (Tailscale) addresses are
  # refused on every hop, so a worker steered by hostile text cannot probe this box's own services. To allow them:
  #   fetch: {allow_private_networks: true}
pstack: {}
# No `budgets:` section: a session Ticket and the Router have no cap, and a swarm's budget is named by the Router
# (or `executor.swarm.default_budget_microusd`). To cap them, add
#   budgets: {session_microusd: 500000, router_daily_microusd: 3000000}
# and see "Budgets" in README.md.
watchdog:
  operator: {source: discord, address: {channel: <a channel id you read>}}   # also where the health check posts
  every: 1m
health:
  min_free_gb: 50        # below this much free space on the state directory's disk: pause new work and alert
```

`mcp:<your name>` in `policy.people` is what lets you stop a Ticket from the box ([below](#stopping-a-ticket-by-hand)).
`health:` is optional; its defaults are in [Pruning state and the disk floor](#pruning-state-and-the-disk-floor).

### GitHub

The agent works on GitHub as a GitHub App's bot, with no personal access token. Create and install the App as
[GitHub App](README.md#github-app) says (by hand, once), then store its key and add a `github:` section, a `forge:` of the
repositories it may act on, and the `github` Source and Channel:

```sh
scripts/agent-deploy token github-app.pem < ~/Downloads/solos-bot.2026-11-05.private-key.pem
rm -P ~/Downloads/solos-bot.2026-11-05.private-key.pem
```

```yaml
# ~/agent/agent.yaml, beside the sections above
policy:
  people: [discord:<your user id>, github:<your login>]    # lowercase; each is pinned to its account id in state/github-identities.json
  approvers: [discord:<your user id>, github:<your login>]
github:
  app_id: <the App's numeric id>
  key_file: /run/user/1000/solos-agent/github-app.pem      # the unit copies tokens/github-app.pem here before each start; serve reads it once and deletes it
forge:
  kind: github                                             # no token_file and no identity: the commits are by the App's bot
  repos:
    <owner>/<repo>: {about: What the Router reads to tell which repository a request is about, base: main, owes: pull_request}
sources:
  - kind: github                                           # no fields: it reads forge.repos
channels:
  - kind: github
```

Deploy, then check from the box (the unit's copy of the key is gone, so name the durable one):

```sh
scripts/agent-deploy deploy
ssh <box> '~/agent/current/bin/solos-swarm agent github check --config ~/agent/agent.yaml --key-file ~/agent/tokens/github-app.pem --state-dir ~/agent/state'
```

The key rests unencrypted at mode 0600 beside the other tokens. `serve` refuses a `key_file` under any path a sandbox can
read, and mints every token for one repository of `forge.repos`, read for a `GET` and write for the rest. Rotate the key by
generating a second one in the App's settings, storing it with `agent-deploy token github-app.pem`, restarting, and then
deleting the first. To stop GitHub work, suspend the App under Settings, Applications, Installed GitHub Apps: every GitHub
call then waits and retries every five minutes, and unsuspending resumes it with no restart. `github.writes: false` and a
restart keeps the agent reading and writing nothing.

`owes: pull_request` on a repository is what makes a worker's `ready` open a pull request there, and it is also the most the
agent does there (`owes: answer` makes the repository read-only whatever the Router asks). The pull request is opened as the
bot, ready for review, on a branch `solos/<ticket>/<slug>` that only the agent writes, and never touches `.github/**`
(`protected` is the list, per repository). Nothing merges: merging is later work. A config with `owes: pull_request` and
`github.writes: false` does not load. Tickets that deliver a change or a pull request need schema V7 of `agent.db`; an older
build refuses a store that has them, so roll the binary forward with the store, not back.

A config that still has `token_file` on the github Source or on `forge: github`, or `identity` on `forge: github`, fails to
load with a sentence that points at [Moving from token files](README.md#moving-from-token-files). The `forge-token` file is no
longer read; delete it from `~/agent/tokens`.

### A repository whose pull requests need its own checks (`GuiBibeau/solos`)

A JavaScript repository's checks cannot run in a worker's sandbox, which has no network and a clone with no
`node_modules`, and a pull request that lacks the checks' proof fails CI. `GuiBibeau/solos` wants the JSON of
`bun run solos dev verify --scope unit --json` under `## Evidence` in every pull request (its `AGENTS.md`, "Evidence").
Two keys on its repository fix both:

```yaml
# ~/agent/agent.yaml
forge:
  repos:
    GuiBibeau/solos:
      about: ...
      base: main
      owes: pull_request
      deps: {kind: bun, binary: /home/debian/.bun/bin/bun}       # the box's bun; the default is $HOME/.bun/bin/bun
      evidence: {command: "bun run solos dev verify --scope unit --json"}
```

`deps.memory_bytes` is 16 GiB of address space (a cap, not memory used) for the `run_code` of Tickets on this repository
alone: bun aborts under 4 or 6 GiB, and its lint step under 8. `executor.run_code.memory_bytes` stays as it is for every
other Ticket.

The host installs the dependencies from `bun.lock` at the base commit the first time a Ticket on the repository starts
(about four seconds, 2.2 GiB and 234,000 files, links and directories on the box), under `state/deps/GuiBibeau/solos/<key>/`, and a later Ticket
mounts the same cache; a new lockfile on `main` is a new cache. The worker runs `verify` itself, offline, and the host refuses
its `ready` without the JSON in its report. The whole mechanism, what it trusts and what it leaves to CI is in
[Installed dependencies and evidence](README.md#installed-dependencies-and-evidence). Needs: `bun`, `bwrap` and `prlimit` on the
box, and the network from the host (the build sandbox is the one place besides the browser that has it). `agent prune`
removes a cache no Ticket has used for its `--keep-days`. Keep `state/` off a tmpfs: a cache has many files.

## Dashboard

A web page of the agent's tickets at `https://agent.guibibeau.com`, behind Sign in with Vercel, for one person. The box
accepts no inbound connection, so it pushes instead of being asked:

```
solos-agent  --writes-->  agent.db  --read-only-->  solos-agent-publish  --HTTPS PUT-->  private Vercel Blob store
                                                                                                  |
                                       you  -->  agent.guibibeau.com (web/agent-dashboard)  <--reads, server side
```

`solos-swarm agent publish` is a second systemd user unit, `solos-agent-publish`
([scripts/systemd/solos-agent-publish.service](../../scripts/systemd/solos-agent-publish.service)). It opens `agent.db`
**read-only** (SQLite refuses any write on that handle; a test compares the file byte for byte after a run), looks
every few seconds, and builds `tickets/index.json` (the open tickets first, then the latest 100 ended) and one
`tickets/<ticket id>.json` per ticket in it. It uploads only a snapshot whose content hash changed, tickets first and the
index last, so the index never names a blob that is not there. What it remembers is `state/publish-state.json`: the hash
of each blob it last uploaded, for the Blob store its token names (a different store gets everything again). A look that
finds nothing written costs one SQLite pragma; an idle agent costs one index upload every five minutes, which is how the
dashboard tells a quiet agent from a dead publisher. The web app and its setup are in
[web/agent-dashboard/README.md](../../web/agent-dashboard/README.md).

### Turning it on

The Vercel side comes first (the project, a **private** Blob store, the Sign in with Vercel app, the environment variables
and the domain: [exact steps](../../web/agent-dashboard/README.md#setup-on-vercel)). Then, on the Mac:

```sh
# The Blob store's read-write token (vercel_blob_rw_...): from the store, or the project's BLOB_READ_WRITE_TOKEN.
pbpaste | scripts/agent-deploy token vercel-blob-token
```

and add to `~/agent/agent.yaml`:

```yaml
web:
  public_url: https://agent.guibibeau.com      # https, no trailing slash: a ticket's page is {public_url}/tickets/{id}
  publish:
    token_file: /run/user/1000/solos-agent-publish/vercel-blob-token   # the unit copies tokens/vercel-blob-token here before each start
    every_secs: 5                              # optional, 1 to 3600
    heartbeat_secs: 300                        # optional: re-upload the index this often even when nothing changed
```

Write `publish:` in block style, as above: `scripts/agent-deploy` looks for it to know whether to run the publisher. Then
`scripts/agent-deploy deploy` (or `restart` on the current release). From then on `switch`, `rollback`, `start`, `stop` and
`restart` install and restart the publisher with the agent, and stop it while `agent.db` is backed up or restored, so it
never reads a file being replaced. `status` shows it and how long ago the index went up; `publish-logs` is its journal.

```sh
scripts/agent-deploy status           # "publisher: active", "dashboard index last uploaded 3s ago"
scripts/agent-deploy publish-logs     # "publish: uploaded tickets/tk_...json, tickets/index.json (2 unchanged)"
```

`token_file` is a one-use token like the others: the unit's `ExecStartPre` copies `tokens/vercel-blob-token`, and only
that, into its own tmpfs runtime directory, `/run/user/1000/solos-agent-publish/`, and `agent publish` reads it once and
deletes it. `solos-agent.service` does not copy this token, so it is never in the agent's runtime directory. A `web:`
section without `publish:` only sets `public_url` (the link the Discord replies carry) and installs nothing.

`solos-swarm agent publish --config ~/agent/agent.yaml --state-dir ~/agent/state --once` makes one look, prints what was
uploaded as JSON and exits 1 if the API refused a blob. It consumes the token file, so put a fresh copy there first.

### What is published, and who can read it

For each ticket: its title and the Router's scope; the request and every later message the ticket received from its
Thread (the people's, bots' and the Router's notes), in full; **every reply the agent sent**, in full, with whether it was
delivered; the Decisions about it; each Assignment's start, end and Reports; the spend and budget; and, for a browser, the
pages it opened (title, status and address **without its query or fragment**, which carry tokens) and how many steps were
refused or unanswered. Not published: what a browser typed (the Store holds only a digest), vault items and their grants'
values, tokens, and anything outside `agent.db`. A message is cut at 100,000 characters, an Assignment keeps its latest 20
Reports and a browser trail its latest 40 pages. The replies are the Store's own record of what was sent; the one-line answer
to a Discord button or `stop` command is not in it and so is not on the page.

Treat the Blob store as holding what the agent says to you: it is private (every read needs a token, and the dashboard reads
it server-side after a sign-in whose email is `ALLOWED_EMAILS`), but anyone who holds its read-write token can read all of
it. The token rests at mode 0600 beside the others, in the same place and with the same exposure as `model.key`.

An old ticket's page keeps working after the index stops listing it: the publisher never deletes a blob, and a ticket the
agent resumes goes back to the index. The discord link in a reply can be followed a few seconds before the first upload
of that ticket; the page says so and checks again by itself. `source_url` (the link to the original conversation) is made
for Discord messages and for issues and pull requests on github.com; a Slack link needs the workspace's name, which the
agent does not hold.

### Rotating the dashboard's secrets

- **The Blob token.** On the Blob store (Vercel dashboard, **Storage**), create a new read-write token or rotate the
  existing one, then `pbpaste | scripts/agent-deploy token vercel-blob-token` and `scripts/agent-deploy restart`. The
  dashboard app needs `BLOB_READ_WRITE_TOKEN` in its project too only if it is not reading with OIDC; update it there and
  redeploy. A refused token is not retried: the publisher exits (`agent publish` says "the Blob token was refused"), systemd
  restarts it five times in fifteen minutes, then leaves the unit failed. `status` shows that, and so does the dashboard
  itself, which warns when its index is fifteen minutes old.
- **The app's client secret, the session secret, the allowlist.** [web/agent-dashboard/README.md](../../web/agent-dashboard/README.md#rotating-things).
- **Turning it off.** Remove the `web.publish` section and `scripts/agent-deploy restart`: it stops and disables the
  publisher. The blobs stay in the store until it is emptied (`vercel blob empty-store`).

### Failures

| What | What happens |
|---|---|
| Vercel Blob or the network is down | Each upload is tried five times with a doubling wait (honouring `retry-after`), then the look fails and the next one, after a growing wait up to five minutes, tries the same uploads again. Nothing is lost; the Store is not touched. |
| The token is refused (401, 403) | The publisher exits with that message and stays down. See above. |
| The API refuses one blob (another 4xx) | It is logged and skipped until that ticket's snapshot changes; the others and the index still go up. |
| `agent.db` is missing, newer than this build understands, or being restored | The look fails with the reason and is tried again; a replaced file is noticed and read again from the start. |
| The agent restarts | Nothing: the publisher holds no connection to it. |
| The disk or the state directory is read-only | The look fails when it cannot write `publish-state.json`; what was uploaded stays uploaded and is uploaded again after the fix, which is harmless. |

Blob operations are billed (a put is an "advanced operation"). A working ticket changes its snapshot with each report and
each reply, so a busy hour is tens of puts; an idle day is about 290.

## Alerts from outside the agent

The Watchdog runs inside `agent serve`, so it cannot say that serve itself is down or crash-looping. Two more units do:

- `solos-agent-health.timer` runs `solos-agent-health.service` (`solos-swarm agent health`) two minutes after boot and
  every three minutes after each start.
- `solos-agent.service` has `OnFailure=solos-agent-health.service`, so the check also runs at once when systemd gives up on
  the agent (five failed starts in 15 minutes).

Each run asks, with no help from the agent:

| Check | Fails when |
|---|---|
| `unit` | `systemctl --user show solos-agent` says anything but `active` (inactive, failed, or `activating` in its restart delay), the unit file is gone, or systemd cannot be asked |
| `restarts` | systemd's `NRestarts` rose by `health.restart_burst` (3) or more since the last run: a crash loop that looks active when looked at |
| `broker` | the unit has been up for 60 s and `GET /api/hello` on `state/broker.sock` does not answer 200 within 5 s |
| `disk` | free space on the state directory's disk is under `health.min_free_gb` ([below](#the-disk-floor)) |
| `discord` | the bot token cannot read the Watchdog's channel: the network is down, the token is refused, or the bot lost access. Nothing is sent to find out |
| `config` | `agent.yaml` does not load, which is the likeliest reason for a crash loop right after an edit |

A failure posts once per incident to the Watchdog's Discord channel (`watchdog.operator`, which must be `source: discord`
for this), as the bot, through the Discord API root of the config's `discord` source. After that it says nothing more
for the same kinds of trouble; it posts again only if a new kind joins the incident, and once when everything is well
again ("`solos-agent` on `<host>` is healthy again after 12m."). A post that fails (Discord is down) is tried again at the
next run, and nothing is told twice. `state/health-state.json` is its memory: the open incident, which kinds were told, the
restart count, whether it made a pause, and the channel the last good config named, so that a config that no longer loads
is still told to the right channel.

**The token.** It is the agent's own: `ExecStartPre` copies `~/agent/tokens/discord-token` into the check's private tmpfs
runtime directory, `/run/user/1000/solos-agent-health/`, before each run, and the directory goes when the run ends.
`agent health` reads that copy and keeps it (the unit's copy is the one that goes), and does not use the agent's runtime
directory, which is gone when the agent is. The token already rests at mode 0600 in `tokens/`; what this adds is a tmpfs
file that lives for the length of one run. The check never starts, stops or restarts anything.

**Deploys do not alert.** `scripts/agent-deploy` writes `state/health-hold` (an expiry time) while it stops and starts the
agent. Until it expires the check records problems and tells nobody. A deploy, rollback or restart removes the hold when it
finishes, so a deploy that went wrong is told by the next run; `stop` leaves the hold for 30 minutes, so an agent you
stopped on purpose is told about only if it is still down half an hour later.

**What it cannot do.** If Discord itself (or the token) is what is broken, there is nobody to tell: the check fails, which
shows in the journal (`scripts/agent-deploy health-logs`) and in `systemctl --user --failed`, and tells Discord the next
time it can. It runs on the same box as the agent, so a box that is off, or a user manager that is gone, alerts nobody.
`scripts/agent-deploy status` shows the timers, the last result, any open incident, and runs `agent health --dry-run`
(checks and prints, posts and changes nothing). Every deploy re-enables the timers; they cannot be left off.

## Pruning state and the disk floor

`state/` grows with every Ticket: each repository Ticket's clone, every worker's transcript, a mirror per repository, the
Router's transcripts. The disk is shared with other production services, so
`solos-agent-prune.timer` runs `solos-swarm agent prune --state-dir ~/agent/state --keep-days 14` daily at 04:30 (local
time, up to 15 minutes late; `Persistent`, so a day the box was off is made up). It opens the Store read-only, runs at the
lowest CPU and disk priority, and its last line says what it freed.

```sh
ssh <box> '~/agent/current/bin/solos-swarm agent prune --state-dir ~/agent/state --keep-days 14 --dry-run'
scripts/agent-deploy prune-logs        # what the daily runs freed
```

| What | Removed when | Never |
|---|---|---|
| `tickets/<id>/work`, plus `host/repo.json` and `prepared` | the Ticket ended more than N days ago (its end time is in the Store) | an open Ticket; a directory the Store has no Ticket for; `pi-settings.json`; a swarm's `swarm/` books |
| `tickets/<id>/*.rpc.jsonl`, `*.stderr` | the same | |
| `forge/<owner>/<name>.git` | nothing has fetched into it for N days (`FETCH_HEAD`, `HEAD`, `config`, `packed-refs`) | a mirror used since. The next Ticket on that repository makes it again; clones are independent, so no clone needs it |
| `deps/<owner>/<name>/<key>` | no Ticket has mounted it for N days (a Ticket's start marks `manifest.json` used), and a `tmp-*` directory of a build that died, by its age | a cache used since. The next Ticket on that repository builds it again |
| `router/*.rpc.jsonl`, `*.stderr` | not written for N days | the newest of each kind, which a live session may be writing |
| `trail/<id>/` | **only with `--include-trail`**, for Tickets that ended more than N days ago. `agent trail` then reports their blobs missing | by default |
| `agent.db` and its WAL, `tokens/`, `backups/`, `releases/`, the vault, `browser/` | | always |

The markers go with the clone because a Ticket that ended can be resumed by a person's follow-up in its thread: with
`repo.json` or `prepared` left behind the worker would find an empty `/work` and believe it was ready. Without them it clones
again from the mirror (or from GitHub). A Ticket resumed *while a prune runs* is checked again just before its files are
removed and keeps them. `--dry-run` prints exactly what a run would remove, with sizes (as allocated blocks, which is what
removing frees). `--json` prints the report as JSON. A failed removal is listed, the rest goes ahead, and the exit
status is 1, which shows the service as failed.

Not pruned: `~/agent/target`, the cargo build cache, which is the other large grower (`cargo clean` by hand when the disk
needs it), and old `releases/` (a deploy keeps the newest five) and `backups/` (twenty).

### The disk floor

`agent health` (above) also guards the disk. When free space on the state directory's filesystem is under
`health.min_free_gb` (default 5; 0 turns the floor off), it writes `state/PAUSE`, starting with
`paused by solos-agent-health: ...`, and alerts. That is the pause gate any operator has: no new Eval starts, no Effect is
performed, and workers' next model calls wait. It frees nothing, so it is a stop before the disk is full, not a cleanup:
run the prune by hand, or free space another way. The pause is lifted again, and only a `PAUSE` that starts with that
line, once free space is back to `health.resume_free_gb` (default the floor plus a fifth of it, at least 1 GB more, so a
disk hovering at the floor does not flap). A `PAUSE` a person wrote is never overwritten or lifted, and a `control.json`
pause is a different file. The check runs every three minutes, so a disk that fills faster than that between two runs is not
caught; the deploy also refuses to build under 30 GB free.

```yaml
health:
  min_free_gb: 50       # this box's disk is 86% full: stop at 50 GB free rather than at none
  # resume_free_gb: 60
  # restart_burst: 3
```

## Stopping a Ticket by hand

A stale Ticket (an Assignment that will never report, a request nobody wants any more) is stopped from the box the way a
person's Stop button does it, through the running agent:

```sh
ssh <box> '~/agent/current/bin/solos-swarm agent stop --state-dir ~/agent/state --operator <your name> --ticket tk_0123456789abcdef --reason "stale after the deploy"'
# tk_...: Stopped by <your name>.
```

The command is filed in `state/mcp/commands.jsonl` as the operator `mcp:<your name>`, which `agent serve` reads (the
`kind: mcp` Source and Channel must be in `agent.yaml`, as in the example above). The Engine then asks the Policy whether
that principal may stop work: `mcp:<your name>` must be in `policy.people` or `policy.approvers`, and anyone else is
refused with "is not allowed to stop this work". The Ticket ends for good as `stopped` with the reason
`stopped by <your name> on mcp: <reason>`, its Assignment ends exactly as it does for a person's Stop, and a later message in
its thread does not resume it. The
answer is posted on the command's own thread in the `mcp` Channel's outbox and printed here. Nothing writes the Store but
the agent.

| Exit | Meaning |
|---|---|
| 0 | stopped, or already stopped or ended (`completed`, `dropped`); nothing more to do |
| 1 | the agent refused (the Policy does not name the operator), or the Ticket does not exist |
| 2 | no answer within `--wait-secs` (20): the agent is not running or not reading. The command stays filed and a running agent applies it if it reads it within 10 minutes; older than that it is not applied, so a restart never carries out last week's |

To clear stale Tickets, list the open ones (`scripts/agent-deploy status` prints them with their age) and stop each:

```sh
ssh <box> 'for t in tk_aaa tk_bbb; do ~/agent/current/bin/solos-swarm agent stop --state-dir ~/agent/state --operator <your name> --ticket $t --reason "stale after the deploy"; done'
```

## Not handled yet

- The disk floor is checked by `agent health` every three minutes, not by `agent serve`, and it pauses; it does not free
  space. `~/agent/target`, old releases and backups are not part of the prune.
- The health check runs on the box it watches. Nothing alerts if the box or the user's systemd is gone.
- The host does not run a repository's `evidence` command itself: the worker does, and the host checks the shape of its
  block. A pull request's description is made once, so a later head pushed for a repair keeps the first head's evidence.
  See [Installed dependencies and evidence](README.md#installed-dependencies-and-evidence).
- A drain narrows what a deploy interrupts; a worker in the middle of a long tool call is still stopped.
