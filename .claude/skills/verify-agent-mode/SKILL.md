---
name: verify-agent-mode
description: Prove solos-swarm agent mode (`solos-swarm agent serve`) works for real on the remote box remote-ai, with a real DeepSeek Router and real Pi workers, driven the way a person drives it through the MCP operator tools (and Discord for chat). Use before claiming an agent-mode feature works, after changing the Router, workers, broker tools, vault, browser, forge, GitHub App or memory, and as the base of every `/pstack:maintain-verification-skill` pass.
---

# Verify agent mode

Agent mode is a long-lived server. One run is one directory on the box, `~/work/verify/<run>/`. It holds the shipped
checkout (`src/`), its release binary (`bin/`), the run's `agent.yaml`, the agent's `state/`, its `secrets/` (vault,
trail key, test values) and `evidence/`. Every command below runs on the Mac and reaches the box over ssh. Never
build or test on the Mac: it drains the battery. The box also runs production `solos-data-*` and `solos-agent*`
services: never start systemd units, never touch `~/.local/state/solos-swarm/runs`, and stop what a run starts.

You act as the person `mcp:verifier`, through `solos-swarm mcp --operator verifier`, the same stdio MCP server that a
Claude Code or Codex host starts. Its requests go through the production `kind: mcp` Source and the Policy, so they
are judged exactly as a Slack or Discord person's are. The feature recipes are in
[features/README.md](features/README.md).

Real models cost money. Get the operator's approval of a spend cap before the first launch of a session. The caps in
`agent.verify.yaml` bound one run: the Router at $1 a day and each Ticket at $0.50. A full pass costs cents.

## Launch

```sh
RUN=v$(date +%Y%m%d-%H%M)
S=.claude/skills/verify-agent-mode
$S/bin/ship.sh "$(git rev-parse --show-toplevel)" $RUN     # copy the checkout, npm ci the Pi tree, cargo build --release
$S/bin/launch.sh $RUN                                       # or with overlays: launch.sh $RUN $S/overlays/vault.yaml $S/overlays/browser.yaml
```

`ship.sh` sends the tracked files and the untracked files that git does not ignore, so uncommitted work is what gets
verified. It builds with Rust 1.96.0 into the shared `~/work/ci/target-verify` and writes `bin/REVISION`
(`<sha>` or `<sha>-dirty`).

`launch.sh` writes `agent.verify.yaml`, with each overlay appended, as the run's `agent.yaml`. It writes the DeepSeek
key from `~/.pi/agent/auth.json` over ssh stdin into a 0600 file on the box's tmpfs, and starts `agent serve`
detached under `timeout ${VERIFY_HOURS:-2}h`, with the box's `~/.local/bin/node`. It prints `run <run> ready` once the
process is up, the broker socket and the MCP inbox exist, and the agent has read and deleted the key file. Every
launch starts from an empty `state/`, so snapshot the evidence before you relaunch.

The overlays in `overlays/` each add top-level sections. Relative paths in them resolve against
`~/work/verify/<run>/`.

| Overlay | Adds | What `launch.sh` does for it |
|---|---|---|
| `vault.yaml` | `vault:` at `secrets/vault.enc` | Runs `vault init` once. |
| `browser.yaml` | `browser:` with Lightpanda 1.0.2 and the trail key `secrets/audit.key` | Runs `browser init-key` once, which prints the key's public half (`ed25519:…`). |
| `pstack.yaml` | `pstack: {}`: every worker enters the default `entry`, `poteto-mode` | Nothing. |
| `forge.yaml` | `forge: local` with `GuiBibeau/solos-swarm` and the identity `solos-swarm-verify` | Seeds the bare repository `forge-remote/GuiBibeau/solos-swarm.git` with one commit of the shipped checkout. |
| `github-app.yaml` | `github:` (the App's id and `secrets/github-app.pem`) and `forge: github` with `GuiBibeau/solos-swarm-sandbox` | Nothing. Set `app_id` and put the key in place first; see [github-app-auth.md](features/github-app-auth.md). |
| `external-jobs.yaml` | `watchdog:` with the MCP thread `ops` as the operator, a 5 s look, and the jobs `verify-unit` (`verify-job.service`) and `verify-tmux` (the tmux session `verify-job`) | Nothing. Start the unit and the session yourself; see [external-jobs.md](features/external-jobs.md). |

An overlay cannot repeat a top-level key, and it cannot add items to a list such as `sources`. So `forge.yaml` and
`github-app.yaml` do not go together, and a feature that needs another Source or Channel needs its own copy of
`agent.verify.yaml`.

## Doctor

```sh
$S/bin/doctor.sh $RUN
```

Read-only. Prints `ok` lines for the process (with its revision), the broker socket, the MCP inbox, the consumed key
file and `agent status` (open Tickets, the Router's spend today against its cap), and warns on errors in `serve.log`.
Exits 1 on the first failure. Run it before the first drive, after any failed drive, and after a relaunch.

## Drive

```sh
$S/bin/mcp.sh $RUN request "<the request, complete on its own>"     # prints {"thread": "t-…", …}
$S/bin/mcp.sh $RUN replies <thread> result 540                       # waits up to 540 s for the result
$S/bin/mcp.sh $RUN request "<follow-up>" <thread>                    # a follow-up on the same thread
$S/bin/mcp.sh $RUN status                                            # agent_status as JSON
$S/bin/mcp.sh $RUN memos [all]                                       # agent_memos; all adds retired and forgotten ones
$S/bin/mcp.sh $RUN memo-add "<text>" [<scope>]                       # agent_memo_add; scope global or repo:<owner>/<name>
$S/bin/mcp.sh $RUN memo-retire <memo> "<reason>"                     # agent_memo_retire
```

`replies` prints the thread's acknowledgement, attention (questions) and result in order, and exits 1 if the kind it
waits for has not come. A request that the Policy does not admit, or one that the Router ignores, gets no reply at all.
Read the decision in `mcp.sh $RUN status` (its `decisions`) before you call a missing reply a failure.

The MCP operator tools are the six above. `agent stop` and `agent memo forget` exist only as commands on the box.

## Evidence

```sh
$S/bin/evidence.sh $RUN <feature-id>
```

`evidence.sh` takes a read-only snapshot into `~/work/verify/<run>/evidence/<feature-id>/` on the box and copies it to
`~/.cache/solos-swarm-verify/<run>/<feature-id>/` on the Mac. The snapshot holds:

- `status.json`: `agent status --json`.
- `events.jsonl`, `effects.jsonl`, `spend.jsonl` and `tickets.jsonl`: rows of `state/agent.db`, one JSON array per
  row. An `events.jsonl` row is `[seq, "<event JSON>"]`, so filter it with
  `jq -c '.[1] | fromjson | select(.type == "ticket_created")' events.jsonl`. A `spend.jsonl` row is
  `[account, state, hold_microusd, cost_microusd, input_tokens, cached_input_tokens, output_tokens]`.
- `replies.jsonl`: what the MCP Channel sent.
- `serve.log`, `REVISION` and `ticket-files.txt` (the files under `state/tickets/`).
- With a browser: `trail.txt` (`agent trail`, with its exit status last) and `trail.json`.

To read a table that the snapshot leaves out, such as `triggers` or `memos`, query the live store. The box has no
`sqlite3` command, so use Python's, read-only:

```sh
q() { ssh remote-ai "python3 -c 'import sqlite3,sys; [print(*r, sep=\"\t\") for r in sqlite3.connect(\"file:\" + sys.argv[1] + \"?mode=ro\", uri=True).execute(sys.argv[2])]' ~/work/verify/$RUN/state/agent.db $(printf %q "$1")"; }
q "select account, count(*), sum(cost_microusd) from spend group by account"
```

The proof standard for every feature:

- Drive the real user path: a request through `mcp.sh`, never a write to the inbox file or a call to the broker
  socket.
- Prove the work with a side effect that the model could not invent: a value checked against an independent source
  (`gh`, `curl`), a server log, or a chain event. A reply that only claims success proves nothing.
- Keep the request, the thread id, the reply and the independent check together in the feature's evidence.

## Cleanup

```sh
$S/bin/cleanup.sh $RUN               # stop the agent (and a key probe server) by pid, remove the socket and any unread key file
$S/bin/cleanup.sh $RUN --purge-src   # at the end of a pass: also remove the shipped source tree
```

Cleanup stops only what the run started, by its pid files, never by name. It keeps `evidence/`, `state/`,
`secrets/`, `serve.log` and `bin/` on the box, and never touches the Mac's copy of the evidence. After the last
cleanup, check that both copies still exist.

## Restart a run and keep its state

`launch.sh` always empties `state/`. A feature that restarts the agent on the same state, such as a crash or a
second start on the same day, uses `launch.sh`'s own start commands without its `rm -rf state`. No recipe has run
these yet.

```sh
python3 -I -c 'import json,os,sys; sys.stdout.write(json.load(open(os.path.expanduser("~/.pi/agent/auth.json")))["deepseek"]["key"].strip())' |
	ssh remote-ai "umask 077; cat > /run/user/\$(id -u)/verify-$RUN-key"
ssh remote-ai "cd ~/work/verify/$RUN && setsid -f bash -c 'echo \$\$ > serve.pid; exec timeout 2h bin/solos-swarm agent serve --config agent.yaml --state-dir state --pi-root src --node \$HOME/.local/bin/node --credential-file /run/user/\$(id -u)/verify-$RUN-key >> serve.log 2>&1 < /dev/null' > /dev/null 2>&1 < /dev/null"
$S/bin/doctor.sh $RUN
```

## Isolation

Runs never share a directory, a state dir or a key file, so two runs can serve at once. They do share
`~/work/ci/target-verify`, so ship one run at a time. A run's agent answers only `mcp:verifier`. A feature that needs
a chat platform says so: Discord needs a bot token from the operator and a person to click.

## Helpers

All in `bin/`, each with its usage in its first lines: `lib.sh` (sourced), `ship.sh`, `launch.sh`, `doctor.sh`,
`mcp.sh` with `mcp_client.py` (the MCP host, run on the box), `evidence.sh`, `cleanup.sh`, and
`key_probe_server.py` (the keyed-fetch feature's account API). `overlays/` holds the config sections that a feature
adds.
