---
name: verify-agent-mode
description: Prove solos-swarm agent mode (`solos-swarm agent serve`) works for real on the remote box remote-ai, with a real DeepSeek Router and real Pi workers, driven the way a person drives it through the MCP operator surface (and Discord for chat). Use before claiming an agent-mode feature works, after changing the Router, workers, broker tools, vault or browser, and as the base of every `/pstack:maintain-verification-skill` pass.
---

# Verify agent mode

Agent mode is a long-lived server. One run is one directory on the box, `~/work/verify/<run>/`, holding the shipped
checkout (`src/`), its release binary (`bin/`), the run's `agent.yaml`, the agent's `state/`, its `secrets/` (vault,
trail key, test values) and `evidence/`. Every command below runs on the Mac and reaches the box over ssh. Never
build or test on the Mac: it drains the battery. The box also runs production `solos-data-*` services: never start
systemd units, never touch `~/.local/state/solos-swarm/runs`, and stop what a run starts.

The person you act as is `mcp:verifier`, through `solos-swarm mcp --operator verifier`, the same stdio MCP server a
Claude Code or Codex host starts. Its requests go through the production `kind: mcp` Source and the Policy, so they
are judged exactly as a Slack or Discord person's are. Feature recipes are in [features/README.md](features/README.md).

Real models cost money. Get the operator's approval of a spend cap before the first launch of a session; the caps in
`agent.verify.yaml` (Router $1 a day, $0.50 a Ticket) bound one run, and a full pass costs cents.

## Launch

```sh
RUN=v$(date +%Y%m%d-%H%M)
S=.claude/skills/verify-agent-mode
$S/bin/ship.sh "$(git rev-parse --show-toplevel)" $RUN     # copy the checkout, npm ci the Pi tree, cargo build --release
$S/bin/launch.sh $RUN                                       # or: launch.sh $RUN $S/overlays/vault.yaml $S/overlays/browser.yaml
```

`ship.sh` sends tracked and untracked-but-not-ignored files, so uncommitted work is what gets verified, and writes
`bin/REVISION` (`<sha>` or `<sha>-dirty`). `launch.sh` installs `agent.verify.yaml` plus any overlays as the run's
`agent.yaml`, creates the vault (`vault init`) and the trail key (`browser init-key`) when an overlay asks for them,
writes the DeepSeek key from `~/.pi/agent/auth.json` over ssh stdin into a 0600 file on the box's tmpfs, and starts
`agent serve` detached, with `timeout ${VERIFY_HOURS:-2}h`. It prints `run <run> ready` once the process is up, the
broker socket and the MCP inbox exist and the key file has been consumed. Every launch starts from an empty `state/`:
snapshot evidence before you relaunch.

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
```

`replies` prints the thread's acknowledgement, attention (questions) and result in order, and exits 1 if the kind it
waits for has not come. A request the Policy does not admit, or one the Router ignores, gets no reply at all: read
the decision with `mcp.sh $RUN status` (its `decisions`) before you call a missing reply a failure.

## Evidence

```sh
$S/bin/evidence.sh $RUN <feature-id>
```

Snapshots, read-only, into `~/work/verify/<run>/evidence/<feature-id>/` on the box and copies it to
`~/.cache/solos-swarm-verify/<run>/<feature-id>/` on the Mac: `status.json`, `events.jsonl` (the hash-chained event
log), `effects.jsonl`, `spend.jsonl`, `tickets.jsonl`, `replies.jsonl` (what the MCP Channel sent), `serve.log`,
`REVISION`, and with a browser `trail.txt` (`agent trail`, its exit status last) and `trail.json`.

Proof standard, every feature:
- Drive the real user path: a request through `mcp.sh`, never a write to the inbox file or a call to the broker socket.
- Prove the work with a side effect the model could not invent: a value checked against an independent source (`gh`,
  `curl`), a server log, a chain event. A reply that only claims success proves nothing.
- Keep the request, the thread id, the reply and the independent check together in the feature's evidence.

## Cleanup

```sh
$S/bin/cleanup.sh $RUN               # stop the agent (and a key probe server) by pid, remove the socket and any unread key file
$S/bin/cleanup.sh $RUN --purge-src   # at the end of a pass: also remove the shipped source tree
```

Cleanup stops only what the run started, by its pid files, never by name. It keeps `evidence/`, `state/`,
`secrets/`, `serve.log` and `bin/` on the box, and never touches the Mac's copy of the evidence. Check both copies
still exist after the last cleanup.

## Isolation

Runs never share a directory, a state dir or a key file, so two runs can serve at once. They do share
`~/work/ci/target-verify`: ship one run at a time. A run's agent answers only `mcp:verifier`; a feature that needs a
chat surface says so (Discord needs a bot token from the operator and a person to click).

## Helpers

All in `bin/`, executable, each with its usage in its first lines: `lib.sh` (sourced), `ship.sh`, `launch.sh`,
`doctor.sh`, `mcp.sh` with `mcp_client.py` (the MCP host, run on the box), `evidence.sh`, `cleanup.sh`, and
`key_probe_server.py` (the keyed-fetch feature's account API). `overlays/` holds config sections a feature adds.
