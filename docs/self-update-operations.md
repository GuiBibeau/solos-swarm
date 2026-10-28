# Self-update: channels, candidates, drain, doctor

Operator-facing summary of what is in `main`. The design, the failure-mode table and the reasoning are in
[self-update-and-graceful-restart.md](self-update-and-graceful-restart.md); read that for why.

**`run --resume` is NOT implemented.** A drained run writes `resume.json`, `Store::resume` can rebuild the ledger
and `remote.py resume` prints a plan, but nothing re-spawns the agents of a drained or crashed run. Resuming
today means starting a new run (`recover_run.py` can seed one from a dead run's transcripts). Do not drain a run
expecting it to continue.

## Candidates and channels

A candidate is an immutable build under `~/.local/share/solos-swarm/<sha256>` (read-only, with `candidate.json`
and a tree hash). A channel (`stable` or `canary`) is a pointer file, `channels/<channel>.json`, to one candidate,
with the last five previous ones. New runs resolve `--candidate current` through the pointer on the controller and
record the resolved hash in the run's `request.json`, so moving a pointer never changes a running run.

```sh
python3 scripts/remote.py deploy                                    # build a candidate on the host; prints its id
python3 scripts/remote.py candidates list
python3 scripts/remote.py candidates current [--channel canary]
python3 scripts/remote.py candidates promote --candidate ID [--channel canary]   # runs the gate, then switches
python3 scripts/remote.py candidates rollback [--channel canary] [--candidate ID]
python3 scripts/remote.py candidates prune [--keep 3] [--yes]       # dry run unless --yes
```

- **promote** takes a per-channel lock and runs a gate in order: `integrity` (tree matches manifest), `doctor`
  (the candidate's own `solos-swarm doctor`: Pi, node, bubblewrap), `smoke` (`solos-swarm smoke`: scripted provider,
  one settled call, pause refuses a call without burning anything, unpause, drain to zero held, resume of that
  directory; no key, no network, zero spend), and `idle_probe` (the lifecycle probe under systemd, passing on
  `cleanup_confirmed`). A failing step leaves the pointer alone and appends `promotion_refused` to
  `channels/<channel>.log.jsonl`. The pointer is written atomically and re-verified after the switch, and restored
  if that fails.
- **rollback** repoints at the previous candidate (or `--candidate`) and does not push the bad one back onto
  `previous`.
- **prune** keeps `--keep` promoted candidates per channel including the current one, plus anything a run still
  pins. It is a dry run (`would_remove`) unless `--yes`.
- Pointers carry a tree hash, not a signature: they prove the candidate is the promoted one, not who built it.
  Signed pointers and a timer that promotes canaries are designed, not built.

## Drain, pause, unpause

```sh
python3 scripts/remote.py drain   --run-id R [--grace-seconds 120] [--reason ...]
python3 scripts/remote.py pause   --run-id R
python3 scripts/remote.py unpause --run-id R
python3 scripts/remote.py resume  --run-id R --candidate current    # read-only plan, see the warning above
```

Drain closes the gate (see [pause-mode.md](pause-mode.md)), waits until no model call is in flight and nothing is
held, then ends the run as `drained` and writes `resume.json`. If a call outlives the grace period it is charged
at its hold ceiling and marked `unknown_charged`: the ledger can overcharge by up to one hold per call, never
undercharge. A drain files no partial findings and synthesizes no artifact; those belong to a resumed run that
does not exist yet, so a drain currently ends a run without its results. Use `pause` to stop spending while
keeping the run alive.

## Outage pause

If five consecutive provider calls fail across agents (429, 5xx, transport) the broker pauses the run through
`control.json` and writes `alert.json`; a 401, 402 or 403 pauses immediately. Retries back off from 250 ms to 8 s,
and refusals while paused burn no call ceilings. The pause shows in `state.json`, in `status` (`progress.pause`)
and as a `doctor` warning. An operator `unpause` reopens it. A run that stays paused until its deadline stops
through the normal path and files partial findings.

## Doctor

```sh
python3 scripts/remote.py doctor [--provider deepseek]
```

One JSON document with `verdict` PASS, WARN or FAIL and a list of checks: channel pointers verify, disk on the
data and state directories (warn under 10 GiB, fail under 2 GiB), `systemctl --user` reachable, the broker socket
of each active run accepts connections, stale credential files (older than 10 minutes), `alert.json` on any run,
clock skew against the controller (warn over 30 s, fail over 120 s), `bwrap` on PATH, and whether the provider
key is present in the controller's Pi auth file. Key values are never read into the output.
The broker-socket check can report FAIL for a healthy run; confirm with `status` and the ledger before acting.

## Smoke

`solos-swarm smoke` is the zero-spend end-to-end check of one binary and is step three of the promotion gate.
It starts the broker with a scripted provider and drives claim, a settled call, pause, unpause, drain and resume.
It reports `live_spend: 0`.

## Not run live

The idle-probe gate step has been exercised against stubs, and a live drain of an idle probe on the host was
still to be done when this was written. Treat the probe procedure in section 5 of the design document as the first
thing to run when no live run needs the host.
