# Running agent mode in production

One box runs `solos-swarm agent serve` as the systemd user unit `solos-agent`. `scripts/agent-deploy` builds and
switches releases from the Mac. Everything on the box is under `~/agent`:

| Path | What it is |
|---|---|
| `releases/<sha>/` | One immutable build of a commit: `bin/solos-swarm`, `node_modules`, `extensions`, `vendor/pstack`. Read-only. The newest five are kept, plus current and previous. |
| `current`, `previous` | Links to the release that serves and the one before it. `--pi-root` is `current`. |
| `agent.yaml` | The production config. It is not in git. |
| `state/` | `agent.db`, `tickets/`, `router/`, `forge/`, `trail/`, `mcp/`, the broker socket. Kept across releases. |
| `tokens/` | One file per one-use credential, mode 0600: `model.key` (the DeepSeek key), `discord-token`, `github-app.pem` (the GitHub App's private key). |
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
scripts/agent-deploy status            # the unit, its release, and `agent status` (open Tickets, spend, alerts)
scripts/agent-deploy logs              # the last 100 log lines; `logs -f` follows, `logs --since 1h`
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
    <owner>/<repo>: {about: What the Router reads to tell which repository a request is about, base: main}
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

A config that still has `token_file` on the github Source or on `forge: github`, or `identity` on `forge: github`, fails to
load with a sentence that points at [Moving from token files](README.md#moving-from-token-files). The `forge-token` file is no
longer read; delete it from `~/agent/tokens`.

## Not handled yet

- No drain: a deploy interrupts running Assignments, which recover.
- Alerts go through the Watchdog, which runs inside the agent, so the agent being down is not itself alerted.
  `systemctl --user status solos-agent` and `scripts/agent-deploy status` show it.
- `state/` grows without pruning; the disk is shared with other production services.
