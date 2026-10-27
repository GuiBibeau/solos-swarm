# Swarm architecture notes — off-peak runs a6/b6, 2026-10-27

Two runs on the box, candidate `52214da7…` (commit a4960b4), launched at 10:00 UTC to fall inside
DeepSeek's off-peak window. Peak hours are 01–04 and 06–10 UTC on weekdays (third-party trackers).
- **a6, prop AMM decoder dossiers:** `work/offpeak-20261027/a6-propamm-goal.json`.
- **b6, Phoenix funding-carry deep dives:** `work/offpeak-20261027/b6-carry-goal.json`, on 181 MB
  of staged dev data at `/dataset/carry`.

Each run has a coordinator that also files a `next-run-goal` record, and one self-review agent
(a19 or b19) that files `swarm-improvement` records. The worker prompt invites any agent to file
one such record.

Notes below are grouped by layer: prompts, system and tools, and goal structure (RSI).
Monitoring entries are timestamped (UTC).

## Setup observations (before launch)

### System and tools
- `SWARM_CODE_CONCURRENCY` is hard-capped at 8 (`src/code.rs`), even on a 128-core host. The B3
  stall came from `run_code` queueing with 4 slots, so the cap should scale with host cores or be
  configurable per Goal. Proposal: allow `1..=min(64, cores/2)`.
- The sandbox's data and repo roots come from user-level `systemctl --user set-environment`, which
  is global to every run of that user. Proposal: make them per-Goal fields (dataset root, repo
  root, venv), validated host-side, so two runs can see different data.
- Idle probes always report "Malformed private evaluator payload" and "host-side Goal deadline
  expired". That is normal for a probe but reads as a failure. Proposal: give probes their own
  verdict ("lifecycle ok") and skip the checks that can't apply.
- The ledger's price table (input 0.30, output 1.20 $/M, cache hit 0.03) predates DeepSeek's
  August 2026 repricing (Flash output 0.66 off-peak, 1.32 peak). Peak output is now above the
  ledger's rate, so a run during peak hours could overspend its nominal cap. Proposal: a
  time-of-day price schedule in the Goal, and a "refuse to start in peak" flag.
- Launching is done from the Mac (`remote.py start`). If the Mac sleeps or goes offline, no
  launch happens. Proposal: a host-side `remote.py schedule` that installs a systemd timer with
  the frozen case.

### Goal structure
- v5 goals reused the same paper-claim structure for both swarms. The a6 and b6 tasks are
  decomposed by entity (venue or market) with shared cross-cutting tasks. That parallelism is
  natural for a swarm; watch whether the cross-cutting tasks starve while waiting on entity
  findings.
- The scorer contract (`papers` records with `file` limited to the seed URLs) is awkward for
  data-analysis tasks, whose evidence is code and local data, not a URL. Proposal: a `dataset`
  evidence type, so `file` may name a `/dataset/...` path.

## Monitoring log

### 10:15 UTC: tick 1 (+15 min)
- **a6:** 6 of 20 filed (a05 GoonFi, a06 ZeroFi, a07 AlphaQ, a09 Aquifer, a15 collector, a18 referee). Ledger $4.92 spent + $1.96 held (5%).
  - The filings are substantive: verified program IDs, account sizes (AlphaQ 672 B, Aquifer 1056 B, ZeroFi 7456 B) and synthetic parsers.
  - a15 corrects the prop AMM report: LaserStream bills 2 credits **per message**, not per 0.1 MB. That makes capture 18.7M–124M credits, not 3–16M, which changes the collector plan. Several agents already filed `swarm-improvement` records.
- **b6:** 0 of 20 filed. Ledger $3.13 (3%). Every agent is in heavy `run_code` loops: 50–120 code calls each in 15 minutes, which is expected for data analysis. The 8 sandbox slots are shared by 20 agents; check queue waits next tick.
- **Steering:** none needed.

Observations:
- *Goal structure:* the a18 referee filed after 15 minutes, before any venue dossiers existed, so it refereed the plan rather than the findings. Next run: gate referee and synthesis roles on `read_findings` count (e.g. ≥ 12 filed), or give them a second, late filing slot.
- *Goal structure:* the a6 coordinator made 30 `run_code` calls in 15 minutes. It should mostly coordinate; watch whether it duplicates worker tasks.
- *System:* a single-venue dossier costs about $0.15–0.30, so the $5 envelopes are 15–30× too large for a6 entity tasks. Size envelopes by task type, or let a coordinator reallocate unused envelope to deep tasks (a14 harness, a01 BisonFi).

### 10:14 UTC: tick 2 (box clock; launch 10:00)
- **a6:** 7 of 20 filed. Ledger $8.50 (9%). a14 (LiteSVM harness, 385 code calls) and a16 (verification, 276) are the deep tasks, as intended. The thin venues (a03 Tessera, a10 Scorch, a12 discovery) are slow: 2–3 code calls and ~13 calls each.
- **b6:** 2 of 20 filed. Ledger **$24.29 (24%) after 14 minutes**. Agents run 200–350 `run_code` calls each.
  - The sandbox is not the bottleneck. Over 11,000 executions, queue wait has p50 0 ms and max 52 ms; execution takes p50 197 ms.
  - **Risk 1, call caps:** with `max_calls_per_agent` = 500, most b6 agents will hit it within ~10 minutes.
  - **Risk 2, budget blocks:** b04, b12 and b18 are at 50% of their envelopes, so they reach the 90% block soon and may file thin or defer.
- **Steering:** none possible mid-run, since goals are frozen and I have no Board access.

Observations:
- *System:* data-analysis agents use `run_code` as a REPL, one tiny call every few seconds, and each call grows the context. Proposals: (a) a persistent Python kernel per agent, so state lives between calls and the agent need not re-read files; (b) a per-call result size cap with summaries; (c) set call caps per task type (analysis 1,500, research 300); (d) put "batch your analysis into fewer, larger scripts" in the analysis worker prompt.
- *System, monitoring:* the operator (me) should have a way to post to the Board mid-run, e.g. `remote.py board-post`, so steering is possible without editing the goal.
- *Goal structure:* b6 spends ~5× faster per minute than a6. Budget envelopes should follow the task kind (data analysis vs web research). The RSI agent b19 is itself at 290 calls; it is doing analysis rather than observing. Next run: give the self-review role read-only tools (`board_read`, `read_findings`, the ledger) and no `run_code`.
