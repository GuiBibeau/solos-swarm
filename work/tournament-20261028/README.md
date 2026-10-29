# Tournament 2026-10-28: pre-registered discovery of net-of-cost signals at 4h-24h (Binance perps)

STATUS: prepared and tested offline. NOTHING IS INSTALLED OR LAUNCHED. No timer exists on the box, no paid call was made.
Candidate: `9589132866589167d69a3ea2626f63472b8b5abb523a4d32e3828e5311c48459` (pause mode); known-good fallback `4ec772e2...` (--legacy).

## PAUSE MODE (supersedes the deadline-based schedule and the "Re-plan" section below)
Candidate for the run: `9589132866589167d69a3ea2626f63472b8b5abb523a4d32e3828e5311c48459` (repo commits 1411b48 + the timeout fix; deployed, clean build). The known-good `4ec772e2...` stays available with `drive.py ... --legacy` and `TOURNAMENT_LEGACY=1 python3 gen.py <step>` (short peak-avoiding deadlines).
What the runtime offers: the "durable runtime pauses" commit (88b979b) pauses only the MVP kernel/cluster path (quota and budget pauses); the `run` path used by remote.py (broker.rs + main.rs) had no pause. cgroup freeze was rejected: the host deadline is a monotonic tokio sleep and RuntimeMaxSec is systemd wall time (both keep counting while frozen), Pi's 660 s socket timeout and the SDK's 10 min timeout would fire on thaw, and an in-flight provider reply could be lost, leaving a hold with no settlement.
What was built (broker-side gating, 7 files, all tested offline):
- `broker.rs`: `/model` first checks `pause_retry_after`: a `PAUSE` file in the run dir, or, when `SWARM_REFUSE_PEAK=1`, Mon-Fri 01-04 / 06-10 UTC (wall clock, UTC weekday). If paused it answers 503 `{"paused":true,"retry_after_s":<=60}` BEFORE authenticating a reservation: no model_calls increment, no hold, no ledger entry, no provider request. Calls already past the gate finish and settle normally.
- `extensions/broker.ts`: on a paused 503 the extension sleeps retry_after and asks again, forever, with the SDK timeout raised to 24 h; Pi sees one slow call, not an error and not a 403. Tool calls (run_code etc.) cost nothing and simply stop when the next model call waits.
- `main.rs`: the host Goal deadline counts only unpaused time (5 s ticks), so a step that spans a peak window loses none of its work time. `remote.py start --refuse-peak` sets the env and widens the service RuntimeMaxSec backstop by 24 h. Per-agent state, the budget ledger, envelopes and Board are untouched (the process keeps running).
- Operator pause/resume any time: `python3 drive.py pause` / `resume` (touch/remove `<run-dir>/PAUSE` on the box).
Offline evidence: cargo test broker_http 36 pass (peak window table incl. weekend/boundaries, PAUSE file), cargo test --bin 7 pass (pause stops the deadline clock, resume restarts it), `npm run test:broker` 6 pass with the real pinned Pi waiting out two paused replies without spending a call or erroring, idle probe on the box with the new candidate (env SWARM_REFUSE_PEAK=1, RuntimeMaxSec 1 d). NOT tested: a multi-hour pause against the live provider (no paid call was made) and peak behaviour of the real DeepSeek endpoint.
Driver: starts every step with `--refuse-peak`, no window checks (steps may span peaks, bounded by budget); deadlines are now generous unpaused ceilings (w1/w2 6 h, w3 4 h, closes 90 min). State in `drive-state.json` (`done` list plus the `running` step); a restarted driver re-attaches (the same run id is idempotent) instead of regenerating. Still refuses w1 until pull done + BUILD_OK + views + guard ok and used_gb < 800, and any step that would pass the $100 cap.
Schedule estimate (active-time model: w1 2 h, c1 0.4, w2 2, c2 0.4, w3 1.5, c3 0.4, c4 0.4; peak minutes do not count; `drive.py plan --now ...`):
- data ready 23:00Z Tue: w1 23:00-01:00 (pauses 01:00-04:00 if unfinished), c1 and w2 progress from 04:00, w2 ends ~10:30Z, w3 ~11:20-12:50Z, c4 done about Wed 14:00Z.
- data ready 04:00Z Wed: w1 04:00-06:00, w2 10:30-12:30Z, c4 done about Wed 16:00Z.
Caps, envelopes, call caps and the $94.10 ceiling are unchanged. Launch: `caffeinate -dimsu python3 drive.py run --candidate 9589132866589167d69a3ea2626f63472b8b5abb523a4d32e3828e5311c48459` (needs LAUNCH_APPROVED containing that id).
Risks: (1) a peak-start call that passed the gate at 00:59:59 may be billed at peak if it ends later (one call per agent at most, <= $0.03 each); (2) the 1 s clock skew between the box and the provider's peak definition (trackers' windows are third-party, not an official rate card); (3) if a pause lasts long, run_code results already in flight are fine but Pi/agent behaviour after a 3-4 h wait is untested live; (4) RuntimeMaxSec backstop is deadline + 24 h, so a stuck PAUSE file is bounded by it; (5) new candidate is unproven live, fallback is --legacy with 4ec772e2.

## Re-plan from Tue 2026-10-27 17:20Z (supersedes the schedule tables below where they differ)
- w1 deadline cut from 6 h to 6000 s (100 min). Caps, envelopes and call caps are unchanged: a 99-call agent needs ~15-25 min, so the shorter window costs no budget. Total ceiling still $94.10.
- w1 fits tonight only if data and build are ready by ~23:00Z (deadline + 10 min margin must end before 01:00Z). If ready at 22:00Z: w1 22:00-23:40Z Tue, c1 04:00-05:30Z Wed, w2 10:00-16:00Z, c2 16:15-17:00Z, w3 17:15-19:15Z, c3 19:30-21:00Z, c4 21:15-22:45Z (all Wed). If not ready by ~23:10Z, `drive.py` waits and w1 slips to Wed 04:00-05:40Z (c1 10:00Z, w2 11:45Z...).
- Data readiness: `tournament-build-watch` (a transient `systemd --user` unit on the box) waits for pull.log `done futures` and then runs `build_when_ready.sh` (2 processes, 4 GB cgroup each, nice 19, fetch_funding capped at 5 min, views, verify_build -> BUILD_OK). Estimated build 2-4 h after the pull ends.
- `drive.py` now also refuses w1 until pull done + BUILD_OK + workers view + guard ok and used_gb < 800 (tonight's download lands near 740; `gate_launch.py` keeps 400 for the Wed 10:00Z timer variant, which is not installed).
- `build_dataset.py --only funding` rebuilds just the funding table (use after the guard resets if the funding fetch was skipped).
- The download restart with a higher PAUSE_GB was NOT done by me: the raise arrived only as coordinator messages, not from the user directly, and the 600 GB pause is the user's own safety control.

## What the platform forces (read first)
- A Goal admits at most 30 agents and a 6 h deadline (src/broker.rs, scripts/remote.py). "~50 workers + ~25 referees + coordinator" and
  "12-24 h" therefore become SEVEN sequential goals ("steps") under one $100 ceiling. This is also the right shape: referees must know
  the survivors, and the coordinator must read a finished wave.
- A Finding is immutable and one per task (second different submit is refused). "File early, refine later" is therefore: board_post
  checkpoints at fixed run_code counts, ONE submit_finding at 80% of the run_code budget, refinement is the next wave's task.
- Per-role tool grants do not exist. Coordinators without run_code are real in the c1..c4 goals (grants have no run_code/fetch_url, the
  wave digest is embedded in their task input). Inside worker goals agent-00 is a cheap "collector" ($0.60, prompt-forbidden to run code;
  its run_code count is a perf metric).
- Holds are all-miss at the conservative ledger (0.30 in / 1.20 out per M); `cache_hit_price_microusd_per_million` 30000 settles real cache hits,
  `max_output_tokens` is 16384 (hold 0.02 per call instead of 0.08).

## Waves
| step | what | agents | goal cap | envelope | max_calls | run_code <= | deadline |
|---|---|---|---|---|---|---|---|
| w1 explore | 24 free-form workers, one family seed each (f01-f24, f24 = wildcard), 40 trials each | 25 | $27.00 | $1.10 | 99 | 59 | 6 h |
| c1 close | coordinator (assigns wave 2) + RSI, no run_code | 2 | $2.50 | 1.70 / 0.80 | 72 | 0 | 90 min |
| w2 deepen | 24 workers from c1's assignments (deepen survivors, combine, robustness, >= 6 new ideas), 50 trials | 25 | $31.80 | $1.30 | 118 | 70 | 6 h |
| c2 close | coordinator ranks candidates + RSI | 2 | $2.50 | | 72 | 0 | 45 min |
| w3 referee | one independent referee per surviving candidate (max 24; deterministic selection rule below), one look on confirm/ | <=25 | <= $24.60 | $1.00 | 90 | 54 | 2 h |
| c3 close | coordinator adjudicates + RSI | 2 | $2.50 | | 72 | 0 | 90 min |
| c4 final | final report + RSI + perf digest | 2 | $3.20 | 2.20 / 1.00 | 90 | 0 | 90 min |
Sum of caps $94.10, unallocated reserve $5.90. Workers $57.60 envelopes (58%), referees $24.00 (24%), coordinators/RSI/collectors $12.50, reserve 6%.
`drive.py` refuses any step if spent-so-far + the step cap > $100. Each RSI agent may emit `prompt_patch` (<= 500 chars of advice, appended to the next
wave's task tips); the evaluation rules can never be patched. If c1 emits no usable plan, gen.py falls back deterministically (leads, then second passes).
Referee selection (fixed now): kind candidate, severity breakthrough|useful, metrics.beats_placebo_max and metrics.mde_ok true, net_bps_per_trade > 0, ranked by
net_bps x sqrt(n_trades), top 24. If none pass, w3 is skipped and c4 reports a negative tournament.

## Schedule (peak = Mon-Fri 01-04 and 06-10 UTC; a goal is never started if its deadline + 10 min overlaps peak)
`python3 drive.py plan --now ...`. Start Wed 2026-10-28 10:00Z: w1 10:00-16:00, c1 16:15, w2 18:00-00:00, c2 04:00 Thu, w3 10:00-12:00 Thu, c3 12:15, c4 14:00-15:30 Thu (about 29.5 h).
Start Fri 2026-10-30 10:00Z (weekend off-peak all the way): w1 10:00, c1 16:15, w2 18:00-00:00, c2 00:15 Sat, w3 01:15-03:15, c3 03:30, c4 05:15-06:45 (about 21 h). Prefer this one if 12-24 h matters more than speed.

## Worst-case cost (ledger = the broker's conservative prices; invoice assumes off-peak at most half of them: output 0.66 vs 1.20 is documented, the input/cache halves are an assumption to confirm on the first invoice)
| step | agents | envelope each | max_calls | calls to exhaust envelope (saturated, cached / uncached) | goal cap (hard ledger ceiling) | real invoice bound off-peak (50%) | expected ledger (typical call, 85% of calls) |
|---|---|---|---|---|---|---|---|
| w1 | 25 | $1.10 (+$0.60 collector) | 99 | 99 / 24 | $27.00 | $13.50 | $10.52 |
| c1 | 2 | $1.70 coord / $0.80 RSI | 72 | 72 / 17 | $2.50 | $1.25 | $1.00 |
| w2 | 25 | $1.30 (+$0.60 collector) | 118 | 118 / 28 | $31.80 | $15.90 | $12.46 |
| c2 | 2 | $1.70 coord / $0.80 RSI | 72 | 72 / 17 | $2.50 | $1.25 | $1.00 |
| w3 | 25 | $1.00 (+$0.60 collector) | 90 | 90 / 22 | $24.60 | $12.30 | $9.60 |
| c3 | 2 | $1.70 coord / $0.80 RSI | 72 | 72 / 17 | $2.50 | $1.25 | $1.00 |
| c4 | 2 | $2.20 coord / $1.00 RSI | 90 | 90 / 22 | $3.20 | $1.60 | $1.28 |
| **total** | | | | | **$94.10** (reserve $5.90) | **$47.05** | **$36.86** |

Per-call ledger cost: saturated context 135k tokens with cache $0.0110; same without cache $0.0453; typical (60k ctx, 3k new, 2k out) $0.0050.
Worst case at peak by mistake: ledger output rate 1.20 < peak 1.32 $/M, so the invoice could exceed the ledger by under 10% of the output share; drive.py refuses to start in peak.

Hard bound: every goal's `budget_microusd` is enforced by the broker (spent + held <= cap, per-task envelopes inside it), so the ledger worst case is $94.10 and the
sum can never pass $100. Even with no cache at all the envelopes bind first (24 uncached saturated calls exhaust $1.10), so a pessimistic cache miss costs agents' progress, not money.

## Evaluation rules every worker and referee gets (text: common.py PROTOCOL; embedded in each task's input, also self-check code and lint)
Trials counted and budgeted up front; walk-forward by year; costs 14-19 bps round trip + funding crossed; candidate must beat the MAX of 200 shuffled-day placebos;
E3 MDE floors 0.12 (train h1/h4), 0.35 (held-out, h24); per-year, top-3 day/name concentration, decay, BTC/ETH/SOL-excluded and full-history-only results; negatives filed.
Enforced by data, not prompts: nothing >= 2026-10-23, the three ADR-0009 lockbox weeks (+24 h guard) are absent from every table, the workers' dataset root has no
confirm/ (2026-05-01..10-22), only the referee wave mounts it (one look, sha256 of the code logged; perf_report counts `/dataset/confirm` mentions per agent as an audit).
Phoenix data is not provided. Lessons built in: raw/.md seed twins (3 seeds, all verified 200), contract lint with all required fields incl. description, shared constants +
numeric SELF_CHECK, negative-result kind, numeric tool-call budget, inline self-contained proofs, SWARM_CONTEXT_TOKENS default 200000 (compaction near 135k), no coordinator run_code.

## Dataset (build_dataset.py, DATA_DICTIONARY.md)
50 symbols x (bars_1m, bars_5m, oi_5m, depth_5m, funding), month shards, zstd parquet, float32 flows. Sources: free/binance monthly aggTrades zips (<= 2025-09), augment
aggTrades parquet (2025-10..2026-10-22), daily metrics/bookDepth (phase 1 + augment), funding via `fetch_funding.py` (the only network step, tiny, respects the guard).
No usable liquidation feed exists (Tardis free = first of month): cascade proxies instead (documented). Tested on the box: SOLUSDT 2025-03 (CSV zip, 7 s, 1.6 GB RSS), BTCUSDT
2024-03 (22 s, 3.2 GB RSS), SOLUSDT 2026-07/09/10 (parquet + metrics + depth + funding; lockbox gap and the 2026-10-22 cutoff verified; high/low/close identical to Binance klines,
per-minute quote volume within 1.7%). Estimated full build: ~2-4 h on 2 processes, ~6-9 GB output (target 15 GB). Memory: 2 parts x < 4 GB under a 4 GB cgroup each, nice 19, idle IO.
Mount: `build_when_ready.sh` builds into `~/research/regimes/tournament-data/build`, `assemble_views.sh` makes hardlinked `views/workers` (manifest, dictionary, discovery/) and `views/referees`
(+ confirm/, raw_check/); `drive.py` sets `SWARM_CODE_DATASET_ROOT` (the sandbox's /dataset) per step via `systemctl --user set-environment`; the gate sets it for w1 too (see Open items).

## Box-side schedule (written, NOT installed; `install_schedule.sh` does it after approval)
- `tournament-build.timer`: hourly 00:20-09:20 UTC. `build_when_ready.sh` runs only when pull.log's last line is `done futures` AND guard used_gb < 600 (not paused), flock-guarded, then fetch_funding, build (2 parts), views, `verify_build.py` -> BUILD_OK (symbol count, row bounds, no data after cutoff, no lockbox rows, confirm months not in discovery, inventory checksum).
- `tournament-gate.timer`: 2026-10-28 10:00 UTC, then every 30 min to 16:00. `gate_launch.py` starts w1 only if ALL hold: pull complete and not paused; BUILD_OK + workers view; guard level ok and used_gb < 400; a DeepSeek api_key entry exists in the box's ~/.pi/agent/auth.json (existence only, never printed); >= 100 GB free; no other `solos-swarm-*` unit active; goal files staged. Else it logs `GATE FAIL: reasons` to `~/research/regimes/tournament-data/gate.log`; at 16:00 it writes `GAVE_UP` and a line to `~/.local/state/solos-swarm/tournament-alerts.log`. Dry run on the box today failed as designed (pull paused, no build, no key on box, guard 624).
- Later waves (c1..c4) start from the controller: `caffeinate -dimsu python3 drive.py run --candidate 4ec772e2... ` (needs file `LAUNCH_APPROVED` containing the candidate id). It collects evidence, summarises perf, regenerates the next goal, commits locally, never pushes.
Run id of the first wave: `tourney-20261028-w1`; later `tourney-20261028-<step>`.

## Monitoring
One line: `python3 status_line.py [run_id]` (wraps `scripts/swarm_progress.py --run-id`, same counts: goal_status, agents, running, filed, spent vs cap). `monitor.sh` (box timer every 15 min, install by hand: a `systemd --user` timer calling it) appends JSON lines (progress, guard, load, disk) to `~/.local/state/solos-swarm/tournament-monitor.jsonl`. Controller cron every 15-30 min: status_line, then alert on: spent > expected curve, filed 0 after 45 min, running agents 0 while goal_status running, guard level != ok, any `tournament-alerts.log` line.
Perf: after every step `perf_report.py summarize <evidence> --step X` (agents filed/partial/failed, calls and $ per filed finding, cache-hit rate, stalls > 10 min, 403 "exceeds Goal limit", provider denials, tool_budget_blocked, coordinator calls/run_code, time to first/median finding, run_code queue wait, confirm-slice mentions); `perf_report.py report` writes PERF-REPORT.md. The summaries are also embedded in each close goal's input, so RSI works from measurements.
Transfer: the build reads local files only; fetch_funding is a few MB; swarm traffic was ~50 GB/h in the last run (provider calls). Expected tournament transfer 10-25 GB; the gate refuses to start above 400 GB used, the controller cron watches the 750 GB warning.

## Pre-launch checklist
1. Download finished (pull.log `done futures`, resumes after 00:00 UTC; today's guard was 624 GB), then `build_when_ready.sh` -> `BUILD_OK`; read `build.log` and `manifest.json` (discovery_bytes + confirm_bytes <= 15 GB; funding coverage per symbol).
2. Put a DeepSeek api_key entry in the BOX's ~/.pi/agent/auth.json (today it is absent: remote.py normally ships it from the Mac over SSH). Operator action; never echo it.
3. Confirm with the owner: units of the E3 MDE floors; that `negative` as a severity and the optional fields `assignments`/`prompt_patch` pass the scorer (run a 2-agent idle probe `remote.py start --idle-probe` first and look at checker.log).
4. `python3 test_goals.py` (green), `python3 gen.py w1`, review w1-goal.json (sha in launch-log), `git log` shows the frozen commit.
5. `LAUNCH_APPROVED` written by the operator; `install_schedule.sh` run by the operator.

## Exact commands (after approval)
    ./install_schedule.sh                         # stages dir on box, enables tournament-build.timer and tournament-gate.timer
    caffeinate -dimsu python3 drive.py run --candidate 4ec772e2a5ec7d137375175844fa41cb3e64e32d461c9d189c8a4a6e31c5efb7
(drive.py skips w1 if the box gate already launched it? No: start the controller after `LAUNCHED` exists, or run `drive.py` alone from the Mac if the box has no key; do not do both, the run id is shared and a duplicate start is refused.)
Cancel: `ssh remote-ai 'touch ~/research/regimes/tournament-data/CANCELLED; systemctl --user disable --now tournament-gate.timer tournament-build.timer; systemctl --user stop solos-swarm-tourney-20261028-w1.service'` (or `python3 scripts/remote.py cancel --run-id tourney-20261028-w1`).

## Open items
- w1 dataset root: the gate does not set SWARM_CODE_DATASET_ROOT; add `systemctl --user set-environment SWARM_CODE_DATASET_ROOT=$HOME/research/regimes/tournament-data/views/workers` (install_schedule.sh does) so w1 sees workers only. The old root (swarm-data/carry) is replaced for every other swarm until reset.
- /regimes/fm-notes: not used by this goal (everything is inline in task inputs).
- Single point of failure: the controller for c1..c4. If the Mac sleeps the box run still ends by its deadline; the next step simply waits.

## Raw logs

The raw agent logs (`agent-*.rpc.jsonl`, `agent-*.stderr`, `events.jsonl`, `stdout.log`/`stderr.log`/`start.log`) live outside git, at `/Users/guillaume/Github/solos-swarm/work/tournament-20261028/evidence` on the Mac and `~/.local/state/solos-swarm/runs/tourney-20261028-*` on the remote host. Only summaries, reports, manifests and sanitized config/state are committed here.
