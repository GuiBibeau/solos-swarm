# Self-update: channels, candidates, drain, doctor

This page describes what `main` holds for candidate channels, drain, pause and host health. For the steps of a
run, see the [runbook](RUNBOOK.md). The design, the failure-mode table and the reasoning are in
[self-update-and-graceful-restart.md](self-update-and-graceful-restart.md).

**`run --resume` is not built.** The `run` command in `src/main.rs` has no `--resume` flag, so nothing restarts the
agents of a drained or crashed run. A drained run writes `resume.json`, `Store::resume` can rebuild its ledger, and
`remote.py resume` prints a plan with `"executable": false`. To continue the work, start a new run:
`scripts/recover_run.py` can fork its Goal from the dead run's transcripts. Do not drain a run that you expect to
continue.

## Candidates and channels

A candidate is an immutable build under `~/.local/share/solos-swarm/<sha256>`. It is read-only and holds
`candidate.json`, the manifest of every file hash with their tree hash. A channel, `stable` or `canary`, is a pointer
file, `channels/<channel>.json`, that names one candidate and the five before it. `start --candidate current` reads
the `stable` pointer before it sends the key, and the run's `request.json` records the candidate id it got, so moving a
pointer never changes a running run. To start a run on `canary`, pass the id that
`candidates current --channel canary` prints.

```sh
python3 scripts/remote.py deploy                                    # build a candidate on the host and print its id
python3 scripts/remote.py candidates list
python3 scripts/remote.py candidates current [--channel canary]
python3 scripts/remote.py candidates promote --candidate ID [--channel canary]   # runs the gate, then switches
python3 scripts/remote.py candidates rollback [--channel canary] [--candidate ID]
python3 scripts/remote.py candidates prune [--keep 3] [--yes]       # dry run unless --yes
```

- **promote** takes a per-channel lock and runs the gate, in this order:
  1. `integrity`: the tree matches its manifest.
  2. `doctor`: the candidate's own `solos-swarm doctor` checks Pi, node and bubblewrap.
  3. `smoke`: `solos-swarm smoke` drives a scripted provider through one settled call, a pause that refuses a call
     without spending anything, unpause, a drain to zero held, and a resume of that directory. It needs no key and
     no network, and spends nothing.
  4. `idle_probe`: the lifecycle probe under systemd. It passes on `cleanup_confirmed`.

  A failing step leaves the pointer alone and appends `promotion_refused` to `channels/<channel>.log.jsonl`. The
  pointer is written atomically and verified again after the switch. If that check fails, the old pointer is put
  back.
- **rollback** points the channel at its previous candidate, or at `--candidate`, and does not put the bad one back
  on `previous`.
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
undercharge. A drain files no partial findings and synthesizes no artifact, because those belong to a resumed run,
and `run --resume` is not built. So a drain ends a run without its results. Use `pause` to stop spending while
keeping the run alive.

## Outage pause

If five provider calls in a row fail as an outage, the broker pauses the run through `control.json` and writes
`alert.json`. The count runs across agents. An outage is a transport error or an HTTP 408, 425, 429 or 5xx
(`broker_control.rs`). A 401, 402 or 403 pauses the run at once. Retries back off from 250 ms to 8 s,
and refusals while paused burn no call ceilings. The pause shows in `state.json`, in `status` (`progress.pause`)
and as a `doctor` warning. An operator `unpause` reopens it. A run that stays paused until its deadline stops
through the normal path and files partial findings.

## Doctor

```sh
python3 scripts/remote.py doctor [--provider deepseek]
```

`doctor` prints one JSON document with a `verdict` (`PASS`, `WARN` or `FAIL`) and a list of checks:

- `channel-<channel>`: the pointer's candidate verifies. A `stable` channel with no pointer is a warning.
- `disk-data` and `disk-state`: free space for candidates and runs. A warning under 10 GiB, a failure under 2 GiB.
- `systemd-user`: `systemctl --user` answers.
- `broker-socket-<run>`: the broker socket of each active run accepts a connection.
- `alert-<run>`: a warning for each run that holds an `alert.json`.
- `stale-credentials`: a warning for key files older than 10 minutes.
- `clock`: the skew against the controller. A warning over 30 s, a failure over 120 s.
- `bubblewrap`: `bwrap` is on `PATH`.
- `provider-key-<provider>`: the provider key is in the controller's Pi auth file. The check never reads the
  value into the output.
The broker-socket check can report a failure for a healthy run. Confirm with `status` and the ledger before you
act.

## Smoke

`solos-swarm smoke` is the zero-spend end-to-end check of one binary and is step three of the promotion gate.
It starts the broker with a scripted provider and drives claim, a settled call, pause, unpause, drain and resume.
It reports `live_spend: 0`.

## Not run live

The gate's idle-probe step has run only against stubs, and no drain of an idle probe has run on a host. On
2026-11-07, `remote-ai` had no `~/.local/share/solos-swarm/channels/` directory: no candidate has been promoted
there, so `--candidate current` fails on it until one is. Run the probe procedure in
[section 5 of the design](self-update-and-graceful-restart.md#5-manual-probe-procedure) when no live run needs the
host.
