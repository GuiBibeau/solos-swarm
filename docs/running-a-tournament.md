# Running a tournament

Runbook distilled from the first live tournament (October 2026, a $100 ceiling, seven sequential steps). The
driver and goal generator for it (`drive.py`, `gen.py`, `perf_report.py`, `status_line.py`, the build scripts) live
in the local tournament directory `work/tournament-<date>/`, which is working data and not part of `main`. What is in
`main` and used here: `scripts/remote.py`, `scripts/swarm_progress.py`, the broker's pause mode and the candidate
channels. Commands below name the tournament scripts where the run used them. The host is called `remote-ai`.

## Shape

A Goal admits at most 30 agents and a bounded deadline, so a "tournament" is a chain of Goals (steps) under one
total ceiling. Each step is a separate run with id `tourney-<date>-<step>`.

| Step | What | Agents | Goal cap | Per-agent envelope | Max calls/agent |
|---|---|---|---|---|---|
| w1 | explore: free-form workers, one family seed each | 25 (24 + collector) | $27.00 | $1.10 | 99 |
| c1 | close: coordinator plans wave 2, plus a self-review (RSI) agent | 2 | $2.50 | 1.70 / 0.80 | 72 |
| w2 | deepen survivors, combine, robustness, new ideas | 25 | $31.80 | $1.30 | 118 |
| c2 | close: coordinator ranks candidates | 2 | $2.50 | 1.70 / 0.80 | 72 |
| w3 | one independent referee per surviving candidate (max 24), one look at held-back data | up to 25 | up to $24.60 | $1.00 | 90 |
| c3 | close: adjudication | 2 | $2.50 | 1.70 / 0.80 | 72 |
| c4 | close: final report and performance digest | 2 | $3.20 | 2.20 / 1.00 | 90 |

The caps sum to $94.10, leaving $5.90 of reserve under the $100 ceiling. The broker enforces every Goal budget
and every task envelope (spent plus held never exceeds them); the driver additionally refuses to start a step if
spend so far plus that step's cap would pass $100. Close steps give coordinators no `run_code`.

## Before launch: the dataset

Workers read a prebuilt dataset mounted at `/dataset` in the sandbox. The build is a long, memory-bound job on the
host, so it runs as a transient systemd unit.

Prerequisites: the source download has finished (the pull log ends in a `done` marker) and the transfer guard
is not paused; the host has the disk for the build; an operator-approved launch marker exists on the controller.

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
  writes `collect-manifest.json` into the bundle (skipped path, file count, bytes); `--include-code` opts back in.
  Run directories are around 25 GB each, so check disk before starting each step and prune old scratch deliberately.

## Launch and gating

```sh
caffeinate -dimsu python3 drive.py run --candidate <candidate-id>     # on the controller, keep it running
python3 drive.py plan                                                 # estimated schedule (active-time model)
```

`drive.py` refuses to run unless a `LAUNCH_APPROVED` file next to it contains the candidate id. For each step in
order `w1 c1 w2 c2 w3 c3 c4` it:

1. checks the cap guard (spent so far + this step's cap <= $100);
2. waits until the transfer guard is `ok`, and for w1 until the data is ready (pull done, `BUILD_OK`, workers view
   present, transfer guard under a used-GB threshold);
3. sets the dataset root for the step and commits the frozen goal locally (it never pushes);
4. calls `remote.py start --refuse-peak` with the goal and expected-results files;
5. records `{step, run_id, candidate}` under `running` in `drive-state.json`, then polls `remote.py status` every
   five minutes into `launch-log.jsonl`;
6. when the service has stopped, runs `remote.py collect`, summarizes with `perf_report.py`, and generates the next
   step's goal.

State is `drive-state.json` (`done` list plus `running`). A restarted driver re-attaches to the running step; a
duplicate start of the same run id is refused. If the controller sleeps, the box run continues to its deadline and
the next step simply waits.

## Peak windows, pause and resume

Steps start with `--refuse-peak`, so the broker holds model calls Mon-Fri 01-04 and 06-10 UTC and the deadline
counts only unpaused time ([pause-mode.md](pause-mode.md)). A step may therefore span a peak window. Manual control:

```sh
python3 drive.py pause     # touch <run-dir>/PAUSE on the box for the running step
python3 drive.py resume    # remove it
```

In w1 the pauses (peak plus an operator pause) show up as event gaps of hours; the performance summary's
"stalled agent" list and `max_event_gap_s` are mostly pause time, not stalls.

## Monitoring

```sh
python3 status_line.py [run_id]                      # one line: status, agents, running, filed, spent vs cap
python3 scripts/swarm_progress.py --run-id RUN       # on the box: agents, ledger vs cap, spend vs envelope (--json)
python3 perf_report.py summarize EVIDENCE --step w1  # writes evidence/<step>-perf.json
python3 perf_report.py report                        # PERF-REPORT.md from all summaries
python3 scripts/remote.py status --run-id RUN
python3 scripts/remote.py doctor                     # host health
```

Watch for: spend above the expected curve, no findings filed after 45 minutes, running agents at zero while the
goal is running, any HTTP 403 "exceeds Goal limit", transfer guard not `ok`. Spend flat with agents running means
the broker is paused (peak window, `PAUSE`, or an outage; check `alert.json`). A dashboard for one run is
described in [dashboard.md](dashboard.md). The `doctor` broker-socket check can fail on a healthy run: confirm with
`status` and watch whether spend keeps rising.

## Cost facts observed

| Measure | w1 |
|---|---|
| Agents | 25, all filed |
| Model calls | 2,098 |
| Ledger spend | $5.25 against a $27.00 cap |
| Cache hit rate | 97% |
| Agents that hit their task envelope | 17 |
| Scratch under `code/` | about 27 GB per run |
| Wall time | about 6.6 h including peak and operator pauses |

The ledger uses conservative prices, charging all input as cache-miss at hold time and settling real cache hits
at the cache price, so most of the cap stays unspent. Envelopes, not the Goal cap, were the binding limit.
Treat the invoice as bounded by the ledger, and confirm the off-peak price assumptions against the first
provider invoice.

## Cancel

```sh
python3 scripts/remote.py cancel --run-id tourney-<date>-<step>      # stops that run's service, confirms it stopped
```

Cancelling a step stops spending for it; run files stay for `collect`. To stop the whole tournament also stop the
controller (Ctrl-C the `drive.py` process), remove or empty `LAUNCH_APPROVED` so a restart is refused, and disable
any build or gate timers on the box. A cancelled step is not marked done: remove `running` from `drive-state.json`
deliberately before restarting, and do not re-use the run id.

## Switching candidate between steps

`remote.py start --candidate <id>` pins the candidate in the run's `request.json`; the pointer is read only by
`start`. Consequences:

- A running step keeps the candidate it started on. Promoting a new candidate mid-run changes nothing for it.
- The box rejects restarting a run id under a different candidate or a different frozen case (`Run ID already
  belongs to a different candidate or frozen case`), so a driver restart must use the candidate recorded in
  `drive-state.json`, not the newest one. The driver stores `candidate` under `running` for exactly this reason.
- To change candidates safely: let the current step finish and collect, run `candidates promote` (or `deploy`),
  run `doctor`, then restart the driver with the new id in `LAUNCH_APPROVED`. Later steps use the new candidate;
  the finished step's evidence still names the old one.
- Do not drain to switch: `run --resume` is not implemented ([self-update-operations.md](self-update-operations.md)).

## After the tournament

Collected steps feed the replay-scored policy loop ([../replay/README.md](../replay/README.md)). With one
tournament the loop answers `insufficient history`; that is expected.
