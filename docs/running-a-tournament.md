# Run a tournament

This runbook comes from the first live tournament, which ran from 2026-10-28 under a $100 ceiling in seven
sequential steps. It is a dated record: the numbers and the commands are the ones that run used. The run's driver,
Goal generator, frozen Goals, build scripts and evidence summaries are in
[`work/tournament-20261028/`](../work/tournament-20261028/README.md). That directory's `.gitignore` keeps the raw
agent logs and other bulky outputs out of git. The shared pieces are `scripts/remote.py`, the broker's pause mode and
the candidate channels. The [runbook](RUNBOOK.md) covers them for a single run.

Run the tournament commands (`drive.py`, `gen.py`, `perf_report.py`, `status_line.py`) from
`work/tournament-20261028/`. The driver calls `scripts/remote.py` without `--host`, so set
`SOLOS_SWARM_SSH_TARGET=remote-ai` first. Its own SSH calls name `remote-ai`.

## Shape

A Goal admits at most 30 agents and a deadline of at most 6 hours, so a tournament is a chain of Goals, called
steps, under one total ceiling. Each step is a separate run with the id `tourney-20261028-<step>`.

| Step | What | Agents | Goal cap | Per-agent envelope | Max calls/agent |
|---|---|---|---|---|---|
| w1 | explore: free-form workers, one family seed each | 25 (24 + collector) | $27.00 | $1.10 | 99 |
| c1 | close: coordinator plans wave 2, plus a self-review (RSI) agent | 2 | $2.50 | 1.70 / 0.80 | 72 |
| w2 | deepen survivors, combine, robustness, new ideas | 25 | $31.80 | $1.30 | 118 |
| c2 | close: coordinator ranks candidates | 2 | $2.50 | 1.70 / 0.80 | 72 |
| w3 | one independent referee per surviving candidate (max 24), one look at held-back data | up to 25 | up to $24.60 | $1.00 | 90 |
| c3 | close: adjudication | 2 | $2.50 | 1.70 / 0.80 | 72 |
| c4 | close: final report and performance digest | 2 | $3.20 | 2.20 / 1.00 | 90 |

The caps sum to $94.10, which leaves $5.90 of reserve under the $100 ceiling. The broker enforces every Goal budget
and every task envelope: spent plus held never exceeds them. The driver also refuses to start a step if the spend so
far plus that step's cap would pass $100. Close steps give coordinators no `run_code`.

## Before launch: the dataset

Workers read a prebuilt dataset mounted at `/dataset` in the sandbox. The build is a long, memory-bound job on the
host, so it runs as a transient systemd unit.

Before the build starts:

- The source download has finished: the pull log holds a `done` marker.
- The transfer guard is not paused.
- The host has the disk for the build.
- An operator-approved launch marker exists on the controller.

Build, in order: fetch the small funding table (the only network step), build monthly shards in two bounded parts,
write the manifest, assemble hardlinked views (a `workers` view without the held-back confirmation months, a
`referees` view with them), then verify (symbol count, row bounds, no data after the cutoff, no lockbox rows, the
confirmation months absent from the workers view). A `BUILD_OK` marker is written only if verification passes.
The driver mounts the right view per step by setting `SWARM_CODE_DATASET_ROOT` in the user manager before start:
workers for w1/w2, referees for w3.

Pitfalls we hit:

- **`systemd-run` does not keep the working directory.** A transient unit starts in the user manager's directory,
  so relative paths in the build scripts broke. Pass `--working-directory=<dir>` (or use absolute paths) in every
  `systemd-run --user` call.
- **Check completion markers anywhere in the log, not with `tail -1`.** Other processes append lines after the
  `done` line, so a gate that read only the last line never saw it. Use `grep -q ' done futures' log`.
- **pandas 3 string columns break `np.issubdtype`.** Columns read as `StringDtype` are not NumPy dtypes, and
  `np.issubdtype(series.dtype, np.datetime64)` raises. Test with `pd.api.types.is_datetime64_any_dtype`, or guard
  the NumPy call, and re-run a small shard before the full build.
- **`collect` must skip `code/`.** The per-run `code/` directory holds the agents' `run_code` scratch copies, about
  27 GB per run. Collecting it streamed tens of GB into memory. `remote.py collect` now leaves it on the host and
  writes `collect-manifest.json` into the bundle (skipped path, file count, bytes). `--include-code` copies it too.
  Run directories are around 25 GB each, so check disk before starting each step and prune old scratch deliberately.

## Launch and gating

```sh
cd work/tournament-20261028
caffeinate -dimsu python3 drive.py run --candidate <candidate-id>     # on the controller, keep it running
python3 drive.py plan                                                 # estimated schedule (active-time model)
```

`drive.py` refuses to run unless a `LAUNCH_APPROVED` file next to it contains the candidate id. For each step in
order `w1 c1 w2 c2 w3 c3 c4` it:

1. Checks the cap guard: the spend so far plus this step's cap must be $100 or less.
2. Waits until the transfer guard is `ok`. For w1 it also waits until the data is ready: the pull is done,
   `BUILD_OK` exists, the workers view is present, and the transfer guard is under a used-GB threshold.
3. Sets the dataset root for the step and commits the frozen Goal locally. It never pushes.
4. Calls `remote.py start --refuse-peak` with the Goal and expected-results files.
5. Records `{step, run_id, candidate}` under `running` in `drive-state.json`, then polls `remote.py status` every
   five minutes into `launch-log.jsonl`.
6. When the service has stopped, runs `remote.py collect`, summarizes with `perf_report.py`, and generates the next
   step's Goal.

The driver's state is `drive-state.json`: a `done` list plus `running`. A restarted driver attaches again to the
running step. A second `start` of the same run id starts nothing. If the controller sleeps, the run on the box goes on
to its deadline, and the next step waits.

## Peak windows, pause and resume

Steps start with `--refuse-peak`, so the broker holds model calls Monday to Friday, 01:00 to 04:00 and 06:00 to
10:00 UTC, and the deadline counts only time that is not paused ([pause-mode.md](pause-mode.md)). A step can
therefore span a peak window. To pause by hand:

```sh
python3 drive.py pause     # touch <run-dir>/PAUSE on the box for the running step
python3 drive.py resume    # remove it
```

In w1 the pauses, peak and operator, show up as event gaps of hours. The performance summary's "stalled agent"
list and `max_event_gap_s` are mostly pause time, not stalls.

## Watch a step

```sh
python3 status_line.py [run_id]                      # one line: status, agents, running, filed, spent vs cap
python3 perf_report.py summarize EVIDENCE --step w1  # writes evidence/<step>-perf.json
python3 perf_report.py report                        # PERF-REPORT.md from all summaries
# from the repository root:
python3 scripts/remote.py status --run-id RUN
python3 scripts/remote.py doctor                     # host health
ssh remote-ai python3 - .local/state/solos-swarm/runs/RUN < scripts/swarm_progress.py   # agents, ledger vs cap, spend vs envelope
```

Watch for these signs:

- Spend above the expected curve.
- No findings filed after 45 minutes.
- No running agents while the Goal is running.
- Any HTTP 403 "exceeds Goal limit".
- A transfer guard that is not `ok`.

Spend that stays flat while agents run means the broker is paused: by the peak window, by `PAUSE`, or by an outage,
which writes `alert.json`. [dashboard.md](dashboard.md) shows one run in a browser. The `doctor` broker-socket check
can fail on a healthy run, so confirm with `status` and watch whether spend keeps rising.

## What the run cost

The evidence summaries in `work/tournament-20261028/evidence/<step>-perf.json` hold these numbers:

| Step | Agents | Filed | Model calls | Ledger spend | Wall time | Goal status |
|---|---:|---:|---:|---:|---:|---|
| w1 | 25 | 25 | 2,098 | $5.25 | 395.5 min | completed |
| c1 | 2 | 2 | 13 | $0.11 | 1.7 min | completed |
| w2 | 25 | 24 | 2,338 | $5.26 | 360.1 min | stopped |
| c2 | 2 | 2 | 21 | $0.16 | 1.3 min | completed |
| w3 | 3 | 3 | 115 | $0.35 | 139.6 min | completed |
| c3 | 2 | 2 | 13 | $0.13 | 1.1 min | completed |
| c4 | 2 | 2 | 14 | $0.14 | 1.5 min | completed |

The seven steps spent $11.40 on the ledger against $94.10 of caps. `scripts/check_run.py` gave every step `FAIL`:
each failed `admitted-identities` among other checks, and each step's private evaluator payload read as malformed
(`private-evaluator` `INCONCLUSIVE`).

More detail for w1:

| Measure | w1 |
|---|---|
| Agents | 25, all filed |
| Model calls | 2,098 |
| Ledger spend | $5.25 against a $27.00 cap |
| Cache hit rate | 97% |
| Agents that hit their task envelope | 17 |
| Scratch under `code/` | about 27 GB per run |
| Wall time | about 6.6 h including peak and operator pauses |

The ledger uses conservative prices. It holds all input at the cache-miss price and settles real cache hits at
the cache price, so most of the cap stays unspent. Envelopes, not the Goal cap, were the binding limit.
Treat the invoice as bounded by the ledger, and confirm the off-peak price assumptions against the first
provider invoice.

## Cancel a step

```sh
python3 scripts/remote.py cancel --run-id tourney-20261028-<step>    # stops that run's service, confirms it stopped
```

Cancelling a step stops its spending, and the run files stay for `collect`. To stop the whole tournament:

1. Stop the controller: press Ctrl-C in the `drive.py` process.
2. Remove or empty `LAUNCH_APPROVED`, so a restart is refused.
3. Disable any build or gate timers on the box.

A cancelled step is not marked done. Before you restart, remove `running` from `drive-state.json` by hand, and do
not use the run id again.

## Switch candidates between steps

`remote.py start --candidate <id>` pins the candidate in the run's `request.json`. Only `start` reads the channel
pointer. So:

- A running step keeps the candidate it started on. Promoting a new candidate mid-run changes nothing for it.
- The box rejects restarting a run id under a different candidate or a different frozen case (`Run ID already
  belongs to a different candidate or frozen case`), so a driver restart must use the candidate recorded in
  `drive-state.json`, not the newest one. The driver stores `candidate` under `running` for exactly this reason.
- To change candidates safely, let the current step finish and collect it. Then run `deploy` and
  `candidates promote`, run `doctor`, and restart the driver with the new id in `LAUNCH_APPROVED`. Later steps use
  the new candidate, and the finished step's evidence still names the old one.
- Do not drain to switch: `run --resume` is not built ([self-update-operations.md](self-update-operations.md)).

## After the tournament

Collected steps feed the replay-scored policy loop ([../replay/README.md](../replay/README.md)). With one
tournament, the loop answers `insufficient history`, as expected.
