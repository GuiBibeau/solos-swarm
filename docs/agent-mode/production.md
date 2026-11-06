# Running agent mode in production

One box runs `solos-swarm agent serve` as the systemd user unit `solos-agent`. `scripts/agent-deploy` builds and
switches releases from the Mac. Everything on the box is under `~/agent`:

| Path | What it is |
|---|---|
| `releases/<sha>/` | One immutable build of a commit: `bin/solos-swarm`, `node_modules`, `extensions`, `vendor/pstack`. Read-only. The newest five are kept, plus current and previous. |
| `current`, `previous` | Links to the release that serves and the one before it. `--pi-root` is `current`. |
| `agent.yaml` | The production config. It is not in git. |
| `state/` | `agent.db`, `tickets/`, `router/`, `forge/`, `trail/`, `mcp/`, the broker socket. Kept across releases. |
| `tokens/` | One file per one-use credential, mode 0600: `model.key` (the DeepSeek key), `discord-token`, `github-app.pem` (the GitHub App's private key), and `vercel-blob-token` (the dashboard publisher's, see [Dashboard](#dashboard)). |
| `backups/` | `agent.db` copied before every deploy and rollback, while the agent is stopped. The newest twenty are kept. |
| `target/` | The cargo build cache. |

`agent serve` reads each token file once and deletes it. The unit's `ExecStartPre` copies `tokens/` into its private
tmpfs runtime directory, `/run/user/1000/solos-agent/`, before every start, so a crash restart or a reboot starts
again without a person. That directory goes away when the unit stops. No sandbox sees `~/agent/tokens` or the runtime
directory. The tokens rest unencrypted at mode 0600 in the agent user's home, like the vault key.

## Commands (from the Mac, in the repository)

```sh
scripts/agent-deploy deploy            # build origin/main, back up agent.db, switch, restart, wait until healthy
scripts/agent-deploy deploy <rev>      # any commit, branch or tag
scripts/agent-deploy rollback          # back to the previous release
scripts/agent-deploy status            # the unit, its release, `agent status` (open Tickets, spend, alerts), and the dashboard publisher
scripts/agent-deploy logs              # the last 100 log lines; `logs -f` follows, `logs --since 1h`
scripts/agent-deploy publish-logs      # the same for the dashboard publisher, when there is one
scripts/agent-deploy restart           # also: start, stop, releases
pbpaste | scripts/agent-deploy token discord-token   # store or rotate a token, then restart
scripts/agent-deploy token github-app.pem < ~/Downloads/<name>.<date>.private-key.pem   # the GitHub App's key
```

A deploy builds on the box with the pinned toolchain (`cargo build --locked`, `npm ci`), runs `doctor` on the new
release, refuses to build with less than 30 GB free, then stops the agent, copies `agent.db`, switches `current`,
installs the unit file from the release and starts it. Healthy means the unit is active with its broker socket and
keeps the same process for 20 s. If the new release does not get there, the deploy switches back to the previous
release, restores the `agent.db` it copied (a new release may have migrated it), starts that and exits 1.

**What a deploy does to work in flight.** Stopping kills the workers; the next start ends each lost Assignment as
`host restarted` and starts a recovery Assignment that is told to check `/work` before repeating anything. A Discord
message sent during the few seconds the agent is down is lost (a new Gateway session does not replay).

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
pstack: {}
# No `budgets:` section: a session Ticket and the Router have no cap, and a swarm's budget is named by the Router
# (or `executor.swarm.default_budget_microusd`). To cap them, add
#   budgets: {session_microusd: 500000, router_daily_microusd: 3000000}
# and see "Budgets" in README.md.
watchdog:
  operator: {source: discord, address: {channel: <a channel id you read>}}
  every: 1m
```

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

## Not handled yet

- No drain: a deploy interrupts running Assignments, which recover.
- Alerts go through the Watchdog, which runs inside the agent, so the agent being down is not itself alerted.
  `systemctl --user status solos-agent` and `scripts/agent-deploy status` show it.
- `state/` grows without pruning; the disk is shared with other production services.
