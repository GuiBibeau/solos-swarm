# Watch jobs that are not Tickets

The owner lists work that runs outside agent mode, such as a downloader as a systemd user unit or a data build in tmux,
under `watchdog.jobs`. The Watchdog checks each job on every look as the agent's user, the dashboard lists them, and
the operator's conversation gets one message when a job dies, stalls or runs its disk low, and one when it is back. The
state it keeps is `state/watchdog-jobs.json`.

## Sub-features

- `listed`: a registered unit and a registered tmux session both appear in `state/watchdog-jobs.json` with status `up`, and the operator is told nothing.
- `one-alert`: stopping the unit gives exactly one `The job "verify-unit" (systemd unit verify-job.service) is down: ...` message, however many looks pass, and a restart of the agent says nothing new.
- `clears`: starting the unit again gives one `is back up. It had been down for ...` message, and the job is `up` again.
- `separate`: stopping the tmux session gives its own alert, and the unit's is not told again.
- `stalled`: a job whose `progress.file` stops changing is `stalled`, told once.
- `dashboard`: the home page's "External jobs" section shows both jobs with their badge and, for a job in trouble, what was found.

## How to get to it (user POV)

- The conversation that `watchdog.operator` names: the alerts and the "back up" messages arrive there.
- The dashboard's home page, when the publisher runs ([production.md, Dashboard](../../../../docs/agent-mode/production.md#dashboard)).
- On the box: `state/watchdog-jobs.json`, and `solos-swarm agent status --state-dir <state> --config <agent.yaml>`, whose alerts include the jobs now in trouble.

## Driving it with bin/mcp.sh

Preconditions: a run launched with `overlays/external-jobs.yaml` (the operator is the MCP thread `ops`, the Watchdog
looks every 5 s), and a clean doctor. The box needs its user manager (`systemctl --user`) and `tmux`. Not yet driven
live: the recipe below is the plan.

- `listed`: `ssh remote-ai "systemd-run --user --unit verify-job sleep 3600; tmux new -d -s verify-job 'sleep 3600'"`, wait 15 s, then `ssh remote-ai "jq -c '.jobs[] | [.name, .status]' ~/work/verify/$RUN/state/watchdog-jobs.json"` prints `["verify-unit","up"]` and `["verify-tmux","up"]`, and `bin/mcp.sh $RUN replies ops` prints no message.
- `one-alert`: `ssh remote-ai "systemctl --user stop verify-job.service"`, wait 20 s, then `bin/mcp.sh $RUN replies ops attention 60` prints one message that names `verify-unit` and says `is down`. Wait 20 s more and run `bin/mcp.sh $RUN replies ops`: still one message. The independent check is `ssh remote-ai "systemctl --user is-active verify-job.service"`, which prints `inactive` or fails.
- `separate`: `ssh remote-ai "tmux kill-session -t verify-job"`, wait 20 s: `replies ops` lists a second message that names `verify-tmux`, and the first one is not repeated. `tmux has-session -t =verify-job` on the box exits 1.
- `clears`: `ssh remote-ai "systemd-run --user --unit verify-job sleep 3600"`, wait 20 s: `replies ops` lists `The job "verify-unit" ... is back up. It had been down for ...`, and the `jq` command above prints `["verify-unit","up"]` again. Start the tmux session again and a "back up" message for `verify-tmux` follows.
- Restart: restart the agent on the same state ("Restart a run and keep its state" in SKILL.md), wait two intervals, and make a job down again and up again: only the new incident is told, never the old one.
- `stalled`: add `progress: {file: <path under ~/work/verify/$RUN>, stalled_after: 30s}` to a job in a copy of the overlay, `touch` the file, wait over 30 s without touching it: one `has stalled` message that names the time the file last changed.
- `dashboard`: with the publisher running against the same state, the page lists both jobs, the one in trouble first with its detail.

## Gotchas

- The verify agent runs as the box's user, so the unit and the session share the user manager and tmux server with everything else that user runs. The names `verify-job` are shared between runs: drive one run at a time, and stop both when done (`systemctl --user stop verify-job.service; tmux kill-session -t verify-job`).
- A transient unit that ends is removed by systemd, and the Watchdog reads that as down: `systemctl --user show` says `LoadState=not-found`.
- The first look comes one interval after the agent starts, so wait before reading `state/watchdog-jobs.json`.
- Job alerts have the `attention` kind and the "back up" message has the `result` kind, so `replies ops result 60` waits for it.
