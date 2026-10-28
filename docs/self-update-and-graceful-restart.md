# Self-update, graceful degradation and graceful restart

Status: partly implemented on branch `self-update-graceful-restart`. Sections marked
**Implemented** have code and tests. Sections marked **Designed** do not.

## 1. How it works today

- `remote.py deploy` builds one immutable candidate on the host. The directory is
  `~/.local/share/solos-swarm/<sha256 of the source archive>`. It is read-only (0555) and holds
  `candidate.json`, a manifest of every file hash. `verify_candidate` re-hashes the tree before
  every start.
- `remote.py start` pins a candidate in the run's `request.json`. It then launches
  `solos-swarm run` as a transient `systemd-run --user` unit. `_finalize` runs as `ExecStopPost`.
  It removes the credential and runs the checker. Nothing moves a run to another candidate, and
  nothing says which candidate a new run should use: the operator pastes a hash.
- `solos-swarm run` is a fixed-length run. It creates `events.jsonl` with `create_new` and refuses
  an existing one. It admits N agents, one Pi process per agent inside bubblewrap, and plays them
  to completion, deadline or SIGTERM. A stop files a partial finding for each unfiled task and has
  the host synthesize the artifact.
- `Store` (`src/broker.rs`) is the only mutable state. Every change appends to a hash-chained
  `events.jsonl` (fsync) and then rewrites `state.json`. The state holds agents, tasks, findings,
  intents, board, `spent_microusd` and `held_microusd`. A model call is `reserve` (hold at the
  peak price) → `dispatched` → `settle` (once) or a release on provider error.
  `accounting_failed` is terminal. The chain is verified offline by `check_run.py`.
- The `mvp`/kernel path (`src/kernel.rs`, `runtime.rs`, Raft) has its own durable state with
  `budget_pauses`, `rate_pauses`, `ResumeGoal` and quota pauses (`quota.rs`). It resumes a goal
  after a pause. It is a different product path from `run` and shares no code with it.
- Pause mode (PR #12) refuses `/model` with 503 `{paused, retry_after_s}` before any reservation, in a
  weekday peak window (`SWARM_REFUSE_PEAK=1`) or while `<run-dir>/PAUSE` exists. The Pi extension waits and
  asks again. The Goal deadline stops counting while paused. This branch makes `control.json` the single
  source: a broker pause (outage, drain) and an operator pause both write it, and the `PAUSE` file and the peak
  window are inputs to the same `pause_retry_after` answer. The wire behaviour is unchanged.

What exists for resume after a crash: nothing on the `run` path. `recover_run.py` builds a new
Goal from a dead run's transcripts. That starts a fresh run and re-spends the context it recovers.
The kernel path resumes goals, but not `run`.

Gaps this design closes:

| Gap | Effect today |
|---|---|
| No pointer to "the candidate new runs use" | Operator hand-copies a hash. No rollback target. |
| Candidate trust is re-hash vs. its own manifest | A consistent swap of files plus manifest passes. |
| No drain | The only graceful stop is SIGTERM, which files partial findings and ends the run. |
| Provider 429/5xx | Each agent retries at once; every retry burns `max_calls_per_agent`. |
| Crash | Holds and in-flight calls are orphaned. The run is lost, and its spend with it. |
| Health | Nothing checks disk, units, stale credentials, sockets or clock before a run. |

## 2. Claude Code, as the model

Claude Code keeps versioned installs in a directory and a `current` pointer to one of them. A
background check downloads the new version beside the old one. The pointer switches atomically.
The running process is never replaced, and the update applies on the next start. It keeps the last
N versions for rollback. `doctor` reports the install, channel and problems. Channels are `stable`
and `latest`/canary.

Mapping:

| Claude Code | solOS swarm |
|---|---|
| versions directory | `~/.local/share/solos-swarm/<hash>` (already content-addressed) |
| `current` pointer | `channels/<channel>.json`: id, tree sha256, history, gate result |
| background check | none by design. The operator or CI runs `deploy` then `candidates promote`. A host timer that runs `promote` for a built canary is a later step. |
| never swap under a running process | A run pins its candidate in `request.json`. Pointers are only read by `start`. |
| applies on next start | New runs use `current`. A running run changes candidate only through drain → resume. |
| keep N versions | `candidates prune --keep N`, which also protects anything a run still needs. |
| rollback | `candidates rollback`, and automatic restore if the switch does not verify. |
| `doctor` | `remote.py doctor`; the binary's own `doctor` still checks Pi and bwrap. |

## 3. Design

### (a) Channels and the pointer — Implemented

`DATA/channels/<channel>.json`, channel is `stable` or `canary`:

```json
{"schema_version":1,"channel":"stable","candidate_id":"<id>","tree_sha256":"<sha256 over every file hash>",
 "previous":["<id>", "..."],"gate":{"ok":true,"steps":[...]},"source":"operator","promoted_unix":1.0}
```

- `deploy` now records `tree_sha256` in `candidate.json` at build time, and `verify_candidate`
  checks it against `files`. The pointer carries its own copy. A swap of files plus manifest
  changes the tree hash and no longer matches the pointer.
- The pointer is written to a temp file, fsynced, `os.replace`d, then the directory is fsynced.
  A reader sees the old pointer or the new one. A failed switch leaves the old pointer whole.
- After the switch the pointer is re-read and re-verified. A failure restores the old pointer.
- `previous` keeps the last five. `rollback` repoints at `previous[0]` (or `--candidate`), records
  `rolled_back_from`, and does not push the bad candidate back onto `previous`.
- `start --candidate current` resolves the pointer on the controller before it asks for the key.
  `request.json` records the resolved hash, so a later pointer move cannot change a run. Pinned
  hashes still work.
- Signatures: **Designed.** The tree hash proves the candidate is the one that was promoted. It
  does not prove who built it. Minimum next step: the build host signs `tree_sha256` with an
  ed25519 key held off the run user and the pointer stores the signature. `ed25519-dalek` is
  already a dependency.

### (b) Promotion gate — Implemented

`candidates promote --candidate <id> [--channel]` takes a per-channel lock and runs, in order:

1. `integrity`: the tree matches its manifest.
2. `doctor`: the candidate's own `solos-swarm doctor` (Pi installed, node runs, bubblewrap works).
3. `smoke`: `solos-swarm smoke`. The binary starts its broker with a scripted provider and drives
   claim, one model call (settles once), a pause (the call is refused and burns nothing), unpause,
   a drain through `control.json` (settles to zero held), then a resume of that directory
   (ledger preserved). No key, network or spend (`live_spend: 0`).
4. `idle_probe`: the existing lifecycle probe under systemd (`--idle-probe`, shortened deadline).
   It passes on `cleanup_confirmed`, not on the runtime verdict, because an idle probe always ends
   on its deadline.

A failing step stops the gate. The pointer is unchanged, and `channels/<channel>.log.jsonl` gets
a `promotion_refused` row. Only a passing gate writes the pointer. "Rollback on a failed probe" is
therefore a refusal to switch, plus a restore if the post-switch re-verify fails.

The idle-probe step is exercised against stubs offline. It has **not** run against systemd yet.

### (c) Drain and resume of a running run

**Drain — Implemented.** `remote.py drain --run-id R [--grace-seconds N]` writes
`control.json` (`{"mode":"drain"}`) into the run directory, then waits for the unit to stop.
The runtime polls it every 200 ms (`control_watch`).

1. The pause gate closes. `Store::reserve` refuses before it touches any counter, so a refused call
   burns neither a call ceiling nor budget. `claim_task` refuses an agent that holds no task. An
   agent that holds one keeps it, and `submit_finding`, `board_post` and the other tools still
   work, so findings in flight are filed.
2. Wait for quiescence: no intent in `reserved`/`dispatched` and `held_microusd == 0`. Each call
   settles through the normal path, or is released by the existing provider-error path.
3. Grace expiry (default 120 s, 1–900): an intent still `dispatched` is charged at its hold
   ceiling and marked `unknown_charged`, and `reserved` ones are released. This is fail-closed.
   It can overcharge by up to one hold and never undercharges.
4. The run ends as `drained`, not `stopped`: **no partial findings are filed and no artifact is
   synthesized.** The unfinished tasks belong to the resumed run. Workers are killed after the
   drain, as for any stop. The runtime then writes `resume.json` last: `last_seq`, `state_sha256`,
   spent/held, task counts, the runtime sha. Exit code is 0.

`pause` and `unpause` use the same file (`mode: pause|run`). A pause leaves the run up with the
gate shut and does not end it. The deadline keeps running.

**What can be resumed, and what cannot.**

| State | Survives | How |
|---|---|---|
| Ledger (spent, held, every intent) | yes | `state.json`, proven against the chain |
| Envelopes, call counters, terminal reasons, fetch bytes | yes | per-agent record in `state.json` |
| Tasks and who claimed them, findings, board, artifact | yes | `state.json` |
| Event history | yes | the log is appended to, one chain, with `run_resumed` |
| Orphaned model calls | settled, see below | `close_orphans` |
| Pi agent processes | **no** | killed; re-spawned |
| An in-flight model call's response | **no** | the outcome is unknown, so the hold is charged |
| Pi session context, in-flight tool calls | **no** | `run_code`/`fetch_url` are read-only and are re-run |

**Resume — Implemented as validation and the Store; the runtime entry is Designed.**
`Store::resume(dir, goal)` rebuilds the Store from `state.json` after these checks:

- the hash chain verifies end to end (a torn final line is trimmed and reported; anything else is
  an error);
- `state.json.last_seq` equals the log's last `seq` (state is written after the event, so a crash
  between the two leaves `state.json` one event behind. That is refused, not guessed);
- `schema_version` is supported (1). The goal's task ids match the run's;
- `goal_status` is not `accounting_failed` or `storage_failed` (fail closed);
- spent + held is within the Goal budget.

It then records `run_resumed` and calls `close_orphans`: unsent reservations are released.
Sent-but-unsettled intents are charged at their hold and marked `unknown_charged`, so a late
receipt cannot settle (`settle` requires `dispatched`). Totals after a resume equal totals before
plus charged ceilings, and a second crash converges on the same rule.
`solos-swarm resume-plan --run-dir D` is the read-only version. `remote.py resume --run-id R
--candidate current` runs it under the chosen candidate and prints what would happen. It
also tells you whether that candidate can read this state: "same or newer compatible" means the
state schema equals what the new binary supports.

**Not yet built: `run --resume`.** It must (each is a change in the spawn path of `run()`):

1. Call `Store::resume` in place of `Store::new`. Skip the "run already exists" refusal when
   `--resume` is given and `resume.json` verifies against `state.json`.
2. Spawn only agents in `respawn_agents` (unfiled). Each uses its old id, so `admit` returns it
   with its counters and `claim_task` returns its old task (the claim is idempotent).
3. Open `agent-NN.rpc.jsonl` in append mode, write a `resume` separator, and give the agent a
   resume prompt built from its task, its last `notes` (already stored per agent), the board, and
   the tail of its previous transcript. This is the `recover_run.py` idea applied inside one run.
   Pi's own session file is not reused, because it is bound to the dead process.
4. Remaining deadline = original deadline minus elapsed active time. It must not widen the Goal.
5. Write `resumed.json` into the run directory once resumed (`prune` protects a drained run until
   then). Update `check_run.py` to accept `agent_resumed` and `run_resumed`. Today it requires exactly one
   `agent_started`/`agent_stopped` per agent, so a resumed run does not qualify until then. Mark
   resumed runs as segmented in the manifest.

No double spend: unsettled calls are never retried. A retry is a new intent with a new hold, made
only after the old one was charged or released. Re-queued tasks keep their claim and their
envelope accounting (`committed` sums by agent id across segments).

A crash with no `resume.json` (OOM, power) uses the same path: chain verified, `state.json` checked
against the log tail, orphans charged. That works today in the Store and `resume-plan`.

### (d) Graceful degradation

| Condition | Behaviour | Status |
|---|---|---|
| Provider 429/5xx/transport | Backoff 250 ms doubling to 8 s, held under the dispatch slot (the cohort slows down). After 5 consecutive failures across agents the run **pauses** and writes `alert.json`. Refusals while paused do not touch call ceilings. 400 and other errors are not outages. | Implemented |
| Provider 401/402/403 | Immediate pause: retrying cannot help and burns calls. | Implemented |
| Pause visible to operator | `state.json.pause`, `alert.json`, `status` (`progress.pause`), `doctor` warn. An operator `unpause` reopens. | Implemented. Pushing the alert off the host: **Designed.** |
| Provider keeps failing while paused | Run stays paused until deadline, then the normal stop files partial findings and synthesizes. Never a silent retry loop. | Existing + gate |
| Context overflow | Pi compaction on, reserve 65 536 tokens; input cap enforced by the broker. | Existing |
| Model fallback | Hook only: a Goal-declared secondary `upstream`, used after a pause with operator consent. Not built. Switching model changes the price table, so it must be a new reservation under the second profile and recorded. | Designed |
| Coordinator fails | Host files partial findings for unfiled tasks and synthesizes the exact-union artifact (`file_partial_findings`, `synthesize_artifact`). | Existing |
| Budget accounting fails (`settle` exceeds hold or ceiling) | `accounting_failed`: terminal, no more admission. Resume refuses it. | Existing + resume guard |
| Disk / transfer guard | `doctor` checks free space on data and state (warn < 10 GiB, fail < 2 GiB). A mid-run guard that pauses at a floor is **Designed** (reuse `begin_pause`, reason `disk`; the 1 TB/day transfer alert from the data side would call the same gate). | doctor only |
| Stalled agent | **Designed.** Per-agent deadline: no broker call for `stall_seconds` (an agent with `reserved` calls older than the provider timeout is not stalled). The runtime then retires the agent: `defer_task` files its partial finding from `notes`, and the process is killed. The nudge loop already caps at 200 attempts but has no clock. | Designed |
| Partial results | Every stop path except a drain files them. A drained run's results are in `state.json` and the next segment. | Existing |

### (e) `doctor` — Implemented

`remote.py doctor` runs on the host and adds a controller-side check. Output is one JSON document
with `verdict` PASS/WARN/FAIL and a list of checks: channel pointers verify (tree hash), disk,
`systemctl --user`, broker socket reachable for each active run, stale credential files older than
10 minutes, `alert.json` on any run, clock skew against the controller (warn > 30 s, fail
> 120 s), bubblewrap on PATH, and the provider key. The key check reports only *present* or
*missing* in the Pi auth file; the value is never read into the output (a test asserts this).

## 4. Failure modes

| Failure | Detected by | Result | Test |
|---|---|---|---|
| Crash during a model call | `resume` finds `dispatched` | Charged at ceiling, never settled again | `resume_after_a_crash_preserves_the_ledger_and_never_double_charges` |
| Crash between event and `state.json` write | `last_seq` mismatch | Resume refused with a clear error | `resume_refuses_what_it_cannot_prove` |
| Torn last log line | chain read | Trimmed, reported | same |
| Log edited | hash chain | Resume refused | same |
| State schema is newer | `schema_version` | Refused | `resume_refuses_an_untrustworthy_ledger...` |
| `accounting_failed` | status | Refused | same |
| Drain with a long call | grace | Charge ceiling, `unknown_charged` | `an_expired_drain_grace_charges...` |
| Drain, normal | quiescent | `held == 0`, `resume.json` | `the_control_file_drains_once...`, `a_drain_cannot_be_lifted...` |
| Provider outage | 5 consecutive | Pause, alert, no call burned | `broker_outage_http`, `outages_pause_after...` |
| Pointer write fails mid-switch | `os.replace` error | Old pointer intact | `test_switch_is_atomic...` |
| Gate fails | gate verdict | `current` unchanged, log row | `test_failed_gate_leaves_current_unchanged...` |
| New candidate does not verify after the switch | re-verify | Old pointer restored | `test_post_switch_verification_failure...` |
| Candidate altered on disk | tree hash vs. pointer | `doctor` FAIL, `current` raises | `test_a_tampered_candidate...` |
| Update while runs are live | pin in `request.json` | Run unchanged | `test_switch_is_atomic_and_never_edits_a_pinned_run` |
| Prune of something in use | `run_pins` | Kept | `test_prune_keeps...` |

**Not tested offline:** `run --resume` end to end (not built), the idle-probe step under real
systemd, `doctor` on a real host, the drain against real Pi agents under bubblewrap. The Linux-only
sandbox path cannot be run on a Mac. A live drain-then-resume of an idle probe is the first thing
to run on the box once approved.

## 5. Manual probe procedure (not run; the tournament run is live)

Run only when no live run needs the host. All steps are read-only or zero-spend.

1. `remote.py deploy`, then `remote.py doctor`. Expect no FAIL.
2. `remote.py candidates promote --candidate <id> --channel canary`. This runs the gate, including the idle probe.
3. `remote.py start --candidate current --run-id probe-1 --seed 1 --idle-probe --deadline-seconds 60`, then
   `remote.py drain --run-id probe-1 --grace-seconds 10`. Expect `drained: true` and `held_microusd: 0`.
4. `remote.py resume --run-id probe-1 --candidate current`. Expect a plan with `charge_microusd: 0`.
5. `remote.py candidates rollback --channel canary`, then `status` to confirm `current` moved back.

## 6. Rollout order

1. This branch: pointer, gate, prune, doctor, pause/drain gate, `Store::resume`, `resume-plan`,
   `smoke`. It changes only the host controller and the broker. Candidates that were already
   deployed keep working. Their manifests lack `tree_sha256`, and `promote` computes the hash
   itself.
2. `run --resume` (deliberately not shipped in the first PR; no experimental half-built path) plus the `check_run.py` segment rule (section 3c list), then a live drain and
   resume of an idle probe.
3. Stall watchdog, disk floor pause, off-host alert, fallback hook.
4. Signed pointers; a host timer for canary promotion.
