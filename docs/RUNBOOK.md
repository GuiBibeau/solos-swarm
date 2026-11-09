# Runbook: run a swarm on the box

This runbook takes one swarm run on a Linux host from build to evidence with `scripts/remote.py`. The script runs
on the controller, your Mac, and drives the host over SSH. Each run is one Goal played by 1 to 30 sandboxed Pi
agents under a transient systemd user unit. The host is `remote-ai` in the examples.

Related pages:

- [self-update-operations.md](self-update-operations.md): candidate channels, the promotion gate, drain and doctor
  checks in detail.
- [pause-mode.md](pause-mode.md): how `--refuse-peak`, `pause` and the `PAUSE` file hold model calls.
- [goal-fetch-policy.md](goal-fetch-policy.md): the `fetch` policy and the scopes a Goal may name.
- [dashboard.md](dashboard.md): watch one run in a browser through an SSH tunnel.
- [running-a-tournament.md](running-a-tournament.md): a chain of runs under one spend ceiling.
- [mvp-cluster.md](mvp-cluster.md): the frozen `mvp` cluster path (Raft, witness, evaluator). This runbook does not
  cover it.

## Before you start

On the controller you need:

- A checkout of this repository. `deploy` ships its tracked files and its untracked files that git does not
  ignore, so uncommitted work is what gets built.
- An SSH alias for the host whose host key is already in `known_hosts`. The script connects with `BatchMode=yes`
  and `StrictHostKeyChecking=yes`, so it never prompts.
- The Pi auth file, `~/.pi/agent/auth.json`, with an entry of `"type": "api_key"` for the provider the run spends
  against (`deepseek`, `venice`, `zhipu` or `zhipu-coding`, by the entry's own name in that file). The key
  leaves the Mac only over SSH stdin, at `start`.

On the host you need bubblewrap (`bwrap`), a systemd user manager, `python3`, Rust 1.96.0 with `cargo` in
`~/.cargo/bin` or on `PATH`, and Node 24.18.0 unpacked at `~/.local/opt/node-v24.18.0-linux-x64`.

Tell the script which host to use. Without this it uses the alias `solos-node`:

```sh
export SOLOS_SWARM_SSH_TARGET=remote-ai     # or pass --host remote-ai to each command
```

## Check the host

```sh
python3 scripts/remote.py doctor
```

The output is one JSON document with `verdict` `PASS`, `WARN` or `FAIL` and a list of checks. Fix every `fail`
check before you deploy. A `WARN` on `channel-stable` with `no promoted candidate` means no candidate is promoted
yet, which the next two sections fix. Each check is described in
[self-update-operations.md](self-update-operations.md#doctor).

## Build a candidate

```sh
python3 scripts/remote.py deploy
```

`deploy` sends the source as a tar over SSH. The host builds it once with `cargo build --release --locked` and
`npm ci --ignore-scripts`, copies in the pinned Node, and freezes the result read-only under
`~/.local/share/solos-swarm/<candidate id>/`. The candidate id is the SHA-256 of the source archive. The command
prints `{"candidate_id": ..., "path": ..., "existing": ...}`. `existing: true` means the same source was built
before.

If the build fails, the host keeps the staging directory, with its `build.log`, and the error names its path.
`deploy` refuses a source tree that holds `auth.json`, `.env`, `credentials.json` or `node_modules`, and it leaves
out `target/`, `target-linux/`, `runs/` and `work/`.

## Promote the candidate

New runs started with `--candidate current` use the candidate that the `stable` channel points at. To point it at
the new build:

```sh
python3 scripts/remote.py candidates promote --candidate <candidate id>
python3 scripts/remote.py candidates current
```

`promote` runs the promotion gate first: integrity, the candidate's own `doctor`, a zero-spend `smoke`, and an idle
lifecycle probe under systemd. If a step fails, the pointer does not move. To try a build on `canary` first, add
`--channel canary`.

You can skip the channel and pass the candidate id to `start` instead. You must do that while no candidate is
promoted, because `--candidate current` then fails with `Channel stable has no promoted candidate`.

## Prepare the Goal

Pick one of these:

- **The synthetic reconciliation case.** Pass no Goal file. The host runs the candidate's `scripts/prepare_case.py`
  with the run's `--seed` to make the Goal and its private expected result.
- **A Goal you froze yourself.** Pass `--goal-file goal.json --expected-file expected.json` to `start`. The two go
  together. Build them by hand or with a builder:
  - `scripts/prepare_platform_case.py` for the live platform red team. Its Goal carries a `fetch` policy.
  - `scripts/prepare_redteam_case.py --repo <checkout>` for a source review with no network.

`Goal::validate` in `src/broker.rs` holds the full contract. Before the host writes the key to a file, it also
checks these limits: 1 to 30 agents, one task per agent, a deadline of at most 21,600 seconds, a budget of at most
1,000,000,000,000 microUSD, and a `fetch` policy, if any, with a non-empty `hosts` list and the `fetch_url` grant.

### Live-probe Goals: the `fetch` policy

A Goal that grants `fetch_url` to probe a live target must carry a `fetch` policy when its scope is in
`AUDIT_SCOPES`. The policy names the hosts, methods and rate the broker allows. See
[goal-fetch-policy.md](goal-fetch-policy.md) for the fields, the refusals and the audit scopes.

## Start the run

```sh
python3 scripts/remote.py start --candidate current --run-id RUN --seed 1 \
	--goal-file goal.json --expected-file expected.json --refuse-peak
```

- `RUN` is 1 to 48 lowercase letters, digits or hyphens, and must start with a letter or a digit. Use a new id for
  each run.
- `--seed` is always required. Without a Goal file it picks the synthetic case.
- `--refuse-peak` makes the broker hold model calls Monday to Friday, 01:00 to 04:00 and 06:00 to 10:00 UTC. The
  Goal deadline counts only the time that is not paused. See [pause-mode.md](pause-mode.md).
- `--provider deepseek|venice|zhipu|zhipu-coding` picks the auth-file entry. Without it, the script uses the Goal's own
  `upstream.provider`, and then `deepseek`.

The script asks the host to start the run, and sends the key only when the host asks for one. The host writes the
key to a 0600 file and starts `solos-swarm run` as the user unit `solos-swarm-RUN.service`. systemd stops the unit
after the Goal deadline plus 20 seconds, or plus 24 hours more with `--refuse-peak`. When the unit stops, its
`ExecStopPost` step removes the key file and scores the run.

If you run the same `start` again, it prints the run's status with `"existing": true` and starts nothing. If you run
it with the same run id but another candidate or another Goal, the host refuses it.

To test the lifecycle without a paid model call, add `--idle-probe`. With it, and only with it, `--deadline-seconds N`
can shorten the deadline. An idle probe never qualifies as a live run.

## Watch the run

```sh
python3 scripts/remote.py status --run-id RUN
```

`status` prints the unit's systemd state (`service`), `service_stopped`, and a `progress` block with the Goal status,
agents running and filed, spent and held microUSD against the cap, whether an artifact exists, any pause, and
whether the run can be resumed. After the run it also holds the checker's `report` and `cleanup_confirmed`.

For a per-agent table, run `scripts/swarm_progress.py` on the host against the run directory. The host has no
source checkout, so send the script from the Mac:

```sh
ssh remote-ai python3 - .local/state/solos-swarm/runs/RUN < scripts/swarm_progress.py
```

Spend that stays flat while agents run means the broker is paused: by the peak window, by an operator, or by a
provider outage. An outage pause writes `alert.json` in the run directory, and `doctor` warns about it. To watch the
run in a browser, see [dashboard.md](dashboard.md).

## Pause, drain or cancel the run

```sh
python3 scripts/remote.py pause   --run-id RUN --reason "checking spend"    # hold model calls, keep the run up
python3 scripts/remote.py unpause --run-id RUN
python3 scripts/remote.py cancel  --run-id RUN                             # stop the unit and confirm it stopped
```

`cancel` stops the unit, so the normal stop path files a partial finding for each unfiled task, and the run's files
stay for `collect`.

`drain` also exists, but do not use it to continue a run later. A drained run ends with no partial findings and no
artifact, and nothing can restart its agents: `run --resume` is not built. See
[self-update-operations.md](self-update-operations.md#drain-pause-unpause).

## Collect the evidence

When `status` shows `service_stopped: true`:

```sh
python3 scripts/remote.py collect --run-id RUN --output evidence/RUN
```

`collect` refuses a run whose unit has not stopped, and an `--output` directory that already exists. It copies the
run directory, but not its `code/` directory. That directory holds the agents' `run_code` scratch copies, which have
reached 27 GB for one run. `collect-manifest.json` in the output says how many files and bytes it left on the host.
To copy `code/` too, add `--include-code`.

## Read the verdict

When the unit stopped, its `ExecStopPost` step ran `scripts/check_run.py` from the candidate. The checker wrote
these files into the run directory, so `collect` brings them back:

- `report.json`: the verdict (`PASS`, `FAIL` or `INCONCLUSIVE`), the runtime, goal, evidence and cleanup verdicts,
  and every check.
- `summary.md`: the verdicts and the checks that did not pass.
- `checker.log` and `checker-exit.json`: the checker's output and its exit code (0 `PASS`, 1 `FAIL`,
  2 `INCONCLUSIVE`).

Read the failed checks in `summary.md` before you act on the verdict. A `FAIL` can come from checks that do not fit
your Goal: every step of the October 2026 tournament got `FAIL`, with `admitted-identities` among the failed checks.

To score the evidence again on the Mac, pass the expected file that you started the run with:

```sh
python3 scripts/check_run.py --run-dir evidence/RUN --expected expected.json
```

## Recover a run that died

Nothing restarts the agents of a drained or crashed run. `python3 scripts/remote.py resume --run-id RUN` only prints
what a resume would settle and respawn. To carry the work forward, fork a new Goal from the dead run's transcripts
with `scripts/recover_run.py`, and start it as a new run.

1. Collect the dead run's evidence.
2. Copy the Goal file that the dead run started with. `recover_run.py` edits the copy in place.
3. Run `recover_run.py` with the dead run's expected file. It writes the new expected file to `--out`:

```sh
cp goal.json recovered-goal.json
python3 scripts/recover_run.py --run-dir evidence/RUN --expected expected.json --seed 1 \
	--goal recovered-goal.json --out recovered-expected.json
```

Each task of the new Goal carries the completed `fetch_url` calls and the worker's conclusions under
`prior_evidence`. The new run pays again for the context it reads.

## Roll back or remove candidates

```sh
python3 scripts/remote.py candidates rollback              # stable points at its previous candidate again
python3 scripts/remote.py candidates prune                 # dry run: lists what would be removed
python3 scripts/remote.py candidates prune --yes
```

A running run keeps the candidate it started on, whatever the pointer does. `prune` never removes a candidate that a
run still needs. See [self-update-operations.md](self-update-operations.md#candidates-and-channels).

## Files on the host

| Path | What it holds |
|---|---|
| `~/.local/share/solos-swarm/<candidate id>/` | One read-only candidate: the binary, the Pi tree, the scripts, `candidate.json`. |
| `~/.local/share/solos-swarm/channels/<channel>.json` | The channel pointer. `<channel>.log.jsonl` beside it logs every promotion and refusal. |
| `~/.local/state/solos-swarm/runs/<run>/` | The run: `request.json`, `config.yaml` (the Goal, in JSON), `events.jsonl`, `state.json`, the transcripts, `code/`, and the checker's files. |
| `~/.local/state/solos-swarm/evaluators/<run>/expected.json` | The private expected result. `collect` does not copy it. |
| `~/.local/state/solos-swarm/credentials/` | The run's key file, from `start` until the unit stops. |
