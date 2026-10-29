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

### 10:24–10:32 UTC: tick 3, incident and repair
- **Incident:** 10 a6 agents (including the coordinator) and 8 b6 agents died with `403 request exceeds Goal limit`.
  - **Root cause:** `extensions/broker.ts` advertised the model window as 1,048,576 tokens, so pi compaction (reserve 65,536) would fire only near 980k tokens. The broker refuses any request over `max_input_bytes` = 1 MiB (~250–300k tokens), so agents whose context grew with fetched pages or many `run_code` outputs hit that wall first.
  - **Recovery failed too:** the broker's forced "Filing is now the ONLY remaining action" prompt carried the same oversized context, so it was refused as well, and the agent settled without filing.
  - **Hidden from monitoring:** these agents showed as "running" until the goal ended. b6 completed with 11 filed and 9 host-recorded PARTIAL stubs, and the host assembled its artifact.
- **Fix (commit 60056f9):** advertise `contextWindow` = `SWARM_CONTEXT_TOKENS` (default 200,000) and `maxTokens` 65,536, so compaction fires around 135k tokens, well under the cap. Candidate 4ec772e2.
- **Repair runs** (`work/offpeak-20261027/gen_repair.py`):
  - **a6r:** 10 tasks, $50 cap. Workers get a "context is capped; keep excerpts short; file early" line.
  - **b6r:** 9 tasks, $50 cap, `max_calls_per_agent` 1,500, plus a "batch analysis into larger scripts" line.
  - Each coordinator reads the first run's findings from `/regimes/fm-notes/<run>-filed-findings.json`.
  - **Spend:** a6 is $10.35 + ≤ $50; b6 is $38.84 + ≤ $50.
- **b6 coordinator:** it hit the 500-call cap after making 499 `run_code` calls.

Proposals:
- *System (critical):*
  1. Derive the advertised context window from the Goal (`max_input_bytes / 4.5`) instead of a constant, and add a test that compaction fires before the broker cap.
  2. When a request would exceed the cap, the broker should not just refuse. It should send a compaction signal, or replace the history with a summary plus the filing prompt, so the final filing always fits.
  3. Mark an agent "dead" in progress and state once its pi process has settled without filing; today it shows as "running" until the deadline.
  4. Add an operator command to retry a single task (`remote.py retry --task`) instead of building a repair goal by hand.
- *Prompts:* tell agents the context budget in numbers ("~150k tokens; summarise fetched pages to < 5 KB").
- *Goal structure (RSI):* repair runs should be first-class: a run ending with partials or stubs should emit a ready repair goal for exactly those tasks, seeded with the filed findings.

### 10:37 UTC: tick 4 (repair runs +6 min)
- **Fix holding:** no new `exceeds Goal limit` errors in a6r or b6r.
- **a6r:** 7 of 10 filed in ~6 minutes, $2.85. a03 Tessera, a10 Scorch and a19 RSI are still running. The coordinator filed early (it is listed as filed); artifact synthesis comes at the end.
- **b6r:** 0 of 9 filed, $3.75. Agents are at 95–195 calls each after 6 minutes, so the "batch your analysis" prompt line has not changed behaviour much. They do have the 1,500-call budget.
- **a6, original:** only a17 is alive (58 calls, still progressing). It will be cancelled once it files.

Observations:
- *Prompts:* a single "batch into larger scripts" sentence does not change the REPL habit. Next run: make it structural, through a per-agent call budget shown in every tool result ("calls left: N") or a minimum interval between `run_code` calls.
- *Goal structure:* a coordinator that files its own task early, before the workers, writes its integration record without their findings. Next run: the coordinator's own finding should be due only after `read_findings` shows the other tasks filed; the broker could enforce that.

### 10:47 UTC: tick 5
- **a6r:** 9 of 10 filed, $4.69; only a03 Tessera is still running.
- **b6r:** 2 of 9 filed, $9.33 (19%). Agents are at 215–435 calls, well under the 1,500 cap. No context-cap errors.
- **a6, original:** a17 is still progressing slowly (71 calls).
- **Box traffic:** 53 GB in the 10:00 UTC hour (47.6 GB in, 5.4 GB out). Mostly the solos-data augment sync and normal research; far under the 1 TB alert. The sync is pulling Polymarket prices (237 files in 15 min). Polymarket is flagged as restricted in the data-licence notes; raise it with the owner.

### 10:57 UTC: tick 6
- **a6, original:** ended with 9 filed and 11 partial stubs; the host built its artifact.
- **a6r:** 9 of 10 filed, $5.16. a03 is still running.
- **b6r:** 2 of 9 filed, $14.07 (28%). Still no context-cap errors.
- **Box traffic:** the day total rose to 318 GB, with a burst of about 95 GB in the 10:50 five-minute window. It does not come from the swarms or the solos-data services; it comes from other jobs on the box. Still under the 750 GB warning.

### 11:21 UTC: tick 7
- **a6r:** 9 of 10 filed, $6.20. a03 is still running.
- **b6r:** 6 filed and 1 partial of 9, $23.42 (47%). No context-cap errors in either run.
- **Box traffic:** 357 GB for the day.
- **Controller:** the Mac's network dropped again and killed the round-7 research agent mid-design. The box-side runs were unaffected.
- *System:* long research agents should run on the box, not from the Mac. Their controller (this session) is a single point of failure on a flaky link.

### 11:22 UTC: tick 8
- **b6r:** the coordinator b00 went PARTIAL at 94% of its $3 envelope after 603 `run_code` calls. This is the second coordinator in a row (after b6) to burn itself on analysis instead of coordinating.
- **b6r:** b14 is at 616 calls and b15 at 1,098, both still running.
- **a6r:** a03 Tessera is at 334 calls, 54% of its envelope.

Proposals:
- *Goal structure (repeat, now high priority):* coordinators must not get `run_code`, or get a hard cap of about 20 calls. Their envelope should be reserved for synthesis, and the integration record should be written after workers file, not instead of their work.
- *System:* a per-role tool grant in the Goal (the coordinator role gets `board_*`, `read_findings`, `submit_*`) rather than one grant list for all agents.

### 11:27 UTC: tick 9
- **a6r:** all 10 filed ($6.40); the run is now in the artifact-synthesis phase.
- **b6r:** b14 and b15 are still running (654 and 1,117 calls), $23.80.

### 11:40 UTC: tick 10
- **a6r:** all 10 filed. The coordinator's artifact synthesis is in progress (provider calls are still being dispatched); $7.20 so far.
- **b6r:** b14 at 85% of its envelope (803 calls) and b15 at 1,179 calls, both still running; $24.75 so far.
- *System:* the synthesis phase is invisible in progress output ("filed 10, artifact absent"). Add a 'synthesizing' state with elapsed time.

### 11:47 UTC: tick 11
- **a6r:** still synthesizing the artifact (~20 min, $7.68). Watch for a stalled synthesis.
- **b6r:** 7 filed + 1 partial; b15 still running (1,209 calls).
- **Free-data phase 1:** started 125,943 files (mostly small daily bookDepth and metrics files); 58 done so far. Box at 372 GB for the day.

### 11:57 UTC: tick 12
- **a6r:** completed. All 10 filed, artifact present, $8.16 total. The prop AMM total is $18.84 across a6 and a6r.
- **b6r:** 7 filed + 1 partial; b15 still running; $25.20.
- **Box traffic:** 445 GB for the day (the free-data download is active).

- 12:17 UTC tick: a6 completed (9 filed/11 partial, 11 x 403 pre-fix), a6r completed 10/10 ($8.16), b6 completed (11/9, 8 x 403 pre-fix), b6r active (7 filed, 1 partial, 1 running, $25.42/$50, 0 x 403). No steering needed; a6 no longer needs cancel. Transfer 560 GB (ok).
