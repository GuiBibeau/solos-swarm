# Watch and control a running agent

These tasks keep an agent that serves healthy. [production.md](../production.md) has the systemd units, the rollout,
and the production values of each command.

## Get alerts from the Watchdog

The Watchdog writes to one conversation that an operator reads, through a channel the config already has:

```yaml
watchdog:
  operator: {source: slack, address: {channel: D0123OPS}}  # a DM or channel the bot can post in
  every: 1m                    # how often it looks
  retry_stalled: true          # retry a stalled Assignment, once per Ticket
  thresholds:                  # each optional; these are the defaults
    waiting: 1h                # a Ticket waiting for a person
    stalled: 30m               # a working Assignment with no Report
    failed_attempts: 3         # an Effect's failed attempts
    unavailable: 1             # unavailable Decisions...
    unavailable_within: 1h     # ...within this window
    spend_percent: 80          # of the Router's daily cap or a Ticket's budget
    paused: 1h                 # a pause held by control.json or PAUSE
    repeat: 6h                 # a problem that lasts is told again this often
```

The `address` takes these keys:

- Slack: `channel`, and `thread_ts` for a thread.
- Discord: `channel`, a channel or thread id, and `message_id` to reply to a message.
- GitHub: `repo` and `number`, an issue.
- MCP: `operator` and `thread`.

On Slack, a top-level post cannot be searched for the way a thread reply can, so a crash between a post and its record
can post one alert twice. A Discord post carries a `nonce`, which Discord checks for a few minutes, so a repeat soon
after is not posted again.

What each alert means is in [The Watchdog](../runtime.md#the-watchdog).

## Watch jobs that are not Tickets

Work that runs outside agent mode, such as a downloader, a data build or a training run, can be listed under
`watchdog.jobs`. The Watchdog checks each one every `every`, the dashboard lists them, and the operator hears once
when one dies, stalls or runs its disk low, and once when it is back.

```yaml
watchdog:
  operator: {source: discord, address: {channel: "123456789012345678"}}
  jobs:
    - name: l2book                              # a systemd user unit (systemd-run --user --unit l2book-download ...)
      unit: l2book-download.service
      progress: {file: /data/l2book/progress.log, stalled_after: 20m}   # stalled if it stops writing this
      disk: {path: /data/l2book, min_free_gb: 50}                       # low on disk below 50 GB free
    - name: data-build                          # a tmux session
      tmux: data-build
    - name: wave-4                              # a pid file
      pid_file: /home/agent/jobs/wave-4.pid
      progress: {file: /home/agent/jobs/wave-4.log, stalled_after: 30m}
    - name: modal-training                      # a status command: exit 0 healthy, 2 stalled, else down
      command: [/home/agent/bin/modal-status, --app, training]
      timeout: 20s
```

Restart `agent serve` after you edit the list. Remove a job from it when it finishes on purpose, or its end is told as
a death. To see what the Watchdog found without waiting for an alert, open the dashboard, or read
`state/watchdog-jobs.json`.

- A status command prints one line, which is the job's detail: `wave 3 of 8, 41%`. It runs as the agent's user with no
  shell and a short environment, so use an absolute path and put anything else in the program itself.
- A pid file that holds a number the system gave to another process since reads as up. Add a `progress` file to catch
  that, or use a unit.
- A tmux session counts as up while it exists. A session that keeps a shell after its program ends is still up, so
  start the program as the session's command (`tmux new -d -s data-build 'make data'`), or add `progress`.
- Alerts are told once an incident, so `thresholds.repeat` does not apply to jobs. The field list is in
  [configuration.md](../configuration.md#watchdog), and how a check and an incident work is in
  [External jobs](../runtime.md#external-jobs).

## See what is running and why

```sh
solos-swarm agent status --state-dir DIR [--config agent.yaml] [--json]
```

The report lists the open Tickets with their stage, age, plan, spend against budget, the request that started them and
their latest Assignment. It also lists the latest Decisions, the pending and failing Effects, the Router's spend
today, the pause, and the alerts the Watchdog would raise now. It opens the Store read-only (no migration, no write),
so it is safe while the agent serves. `--config` adds the Router's cap, the Watchdog's thresholds and a pause set in
agent.yaml.

## Stop a Ticket

```sh
solos-swarm agent stop --state-dir DIR --operator NAME --ticket ID --reason TEXT
```

The command stops a Ticket as a person's Stop does. It is filed in `state/mcp/commands.jsonl` as `mcp:NAME`, and the
running agent's Engine applies it only if `policy.people` or `policy.approvers` names that principal. The agent needs
a `kind: mcp` source. The command exits 0 once the Ticket is stopped, 1 if the agent refused, and 2 if it did not
answer within `--wait-secs` (20 by default).

## Check the agent from outside it

The Watchdog runs inside `agent serve`, so it cannot tell anyone that `serve` is down. `agent health` can:

```sh
solos-swarm agent health --config agent.yaml --state-dir DIR --token-file F [--dry-run] [--json]
```

It checks that the systemd unit (`--unit`, default `solos-agent`) is active and not restart-looping, that the broker
socket answers, that the state directory's disk has room, and that the bot can reach the Watchdog's Discord channel.
Where `executor.claude` names an installed `claude`, it also runs `claude auth status --json` to see whether the host's
Claude Code login has ended; sign in again with `claude auth login` as the agent's user. It
posts once per incident to that channel, with no help from `serve`, and exits 1 while anything fails. Below
`health.min_free_gb` it pauses new work by writing `PAUSE`, alerts, and lifts that pause again once space returns.
`--dry-run` only looks. Without `--token-file`, nothing is posted and the Discord check is skipped.

## Free disk that finished work holds

```sh
solos-swarm agent prune --state-dir DIR --keep-days N [--dry-run] [--include-trail]
```

For Tickets that ended more than N days ago, it removes the worker's clone and transcripts. It also removes forge
mirrors not fetched into for that long, dependency caches no Ticket has mounted for that long, and the Router's old
transcripts. It never removes an open Ticket or `agent.db`, and it keeps the trail's blobs unless you add
`--include-trail` (with `--config` for `trail.path`). It opens the Store read-only, so it is safe while the agent
serves.

## Pause the agent

While paused, the agent starts no new Eval and holds every Effect. Sources keep reading and storing, and what arrives
is judged after the pause lifts. Any of these pauses the agent:

- A `PAUSE` file in the state directory.
- A `control.json` in the state directory with `"mode":"pause"`, which `scripts/agent-deploy` writes during a drain.
- `policy.paused: true` in agent.yaml.

The two files also pause the broker: it answers every model call with a 503 before any hold, so running workers wait
and spend nothing. `policy.paused` does not reach the broker, so workers that are already running keep calling the
model.

[Drain running work](../production.md#drain-running-work) says how a deploy uses the pause.
