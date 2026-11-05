# Running agent mode in production

One box runs `solos-swarm agent serve` as the systemd user unit `solos-agent`. `scripts/agent-deploy` builds and
switches releases from the Mac. Everything on the box is under `~/agent`:

| Path | What it is |
|---|---|
| `releases/<sha>/` | One immutable build of a commit: `bin/solos-swarm`, `node_modules`, `extensions`, `vendor/pstack`. Read-only. The newest five are kept, plus current and previous. |
| `current`, `previous` | Links to the release that serves and the one before it. `--pi-root` is `current`. |
| `agent.yaml` | The production config. It is not in git. |
| `state/` | `agent.db`, `tickets/`, `router/`, `forge/`, `trail/`, `mcp/`, the broker socket. Kept across releases. |
| `tokens/` | One file per one-use credential, mode 0600: `model.key` (the DeepSeek key), `discord-token`, later `forge-token`. |
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
the matching `agent.db*` back from `backups/` into `state/`, and roll back.

## The production config

```yaml
# ~/agent/agent.yaml on the box
policy:
  people: [discord:<your user id>, mcp:<your name>]
  approvers: [discord:<your user id>]
  auto_approve_microusd: 500000           # above this a Ticket's budget waits for Approve
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
budgets: {session_microusd: 500000, router_daily_microusd: 3000000}
watchdog:
  operator: {source: discord, address: {channel: <a channel id you read>}}
  every: 1m
```

To let workers clone a repository, add a `forge:` section (see "Repositories" in [README.md](README.md)) with
`token_file: /run/user/1000/solos-agent/forge-token`, store the token with `scripts/agent-deploy token forge-token`,
and restart.

## Not handled yet

- No drain: a deploy interrupts running Assignments, which recover.
- Alerts go through the Watchdog, which runs inside the agent, so the agent being down is not itself alerted.
  `systemctl --user status solos-agent` and `scripts/agent-deploy status` show it.
- `state/` grows without pruning; the disk is shared with other production services.
