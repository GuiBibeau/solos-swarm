# solos-swarm improvements from the arch-risk-a3 / arch-path-b3 evidence

Source evidence (read-only): `work/arch-swarm-20261026/evidence-a3` and `evidence-b3`
(`events.jsonl`, `state.json`, `agent-NN.rpc.jsonl`). Both runs were cancelled by the operator
at about 23.5 minutes (`goal_stopped`, "operator cancelled run"), so the "90 minutes" for B3 is not
in this evidence; B3 stopped at 1418 s with 836 model calls.

## Diagnosis

### 1. A3: 15 of 18 findings were empty defers
- Cause per agent: the **per-task envelope** ($100 cap / 20 = $5.00, `config.yaml`
  `budget_microusd`, no per-task budgets) was spent. 15 agents carry
  `terminal: "Task budget envelope exhausted"` in `state.json`; the first `provider_denied` is
  `events.jsonl` seq 11639 (agent-08), the last seq 18499. Not context, not turn cap
  (`max_calls_per_agent` 500, max used 227), not run_code failures (125 of 2582 non-zero exits, 2
  timeouts), not fetch failures (0 `fetch_failed` in A3).
- The envelope was consumed by **re-sent context priced at the cache-miss rate**. Context grows
  about 8k tokens per call (agent-04: 1.8k -> 113k input tokens over 227 calls) and
  `broker.rs::cost` charges all input at $0.30/M. But the provider's own receipts
  (`provider_response.body.usage`) show 257.4M `prompt_cache_hit_tokens` vs 1.94M misses (99.3%
  hits). Re-priced at the cache-read rate the extension already declares
  (`extensions/broker.ts` `cacheRead: 0.03`) the run cost about $10, not $79.62. The ledger
  overstated spend about 8x and cut agents off at roughly 1/8 of their intended work.
- Nobody told the agents. Agent-04 made 226 `run_code` calls (mostly `grep`/`sed` over `/regimes`
  for "28 features"), never called `submit_finding`, and learned of the limit only as a refused
  provider call. `deferred_record` then wrote "Deferred before execution ... No code was run" for
  agents that had run up to 226 commands, discarding all their progress. Only agents 05/15/18
  (13-14% of envelope used, 20-51 calls) filed real findings; agents 02 and 14 were still running
  at cancel (tool_denied seq 18651/18652).

### 2. B3: 0 findings
- Not provider limits (median model latency 2.5 s, p90 7.7 s, max 17 s, no 429s) and not planning
  loops. Per agent only 2.4 of 23.6 minutes were model time.
- **run_code queueing.** `CodeExecutor` has `DEFAULT_CONCURRENCY = 4` permits for 20 agents.
  `duration_ms` excludes the wait for a permit, so it was invisible. Pairing `code_started` ->
  `tool_completed` in B3: 401 min of run_code wall time vs 88 min reported duration; median queue wait 21 s,
  p90 55 s, 313 minutes (78%) spent waiting; a few 150-300 s runs held slots. A3 ran on the
  same VM at the same start time with its own 4 permits (CPU contention).
- 7 `fetch_failed` ("fetch response headers exceed 4096 bytes") on the github.com seed directory
  URL, agents 00/02/04/08/11/13/17, each wasting a turn: the 4096-byte header cap was a hard error.
- No agent had filed after ~35-60 calls each; with no budget signal they kept exploring.

### 3. Coordinator never synthesized
- `claim_task` handed out the **first open task regardless of role**. In A3 the coordinator task
  `a00` (`input.role: "coordinator"`) was claimed by **agent-05** (state.json `tasks[0].claimed_by`),
  and agent-00, the only agent with `read_findings`/`submit_artifact`, got worker task `a06`
  (finding index 6) and spent its envelope. Even with all findings the synthesis relied solely on
  the model (12 retries) and otherwise `bail!("coordinator did not submit the artifact")`; and a
  deadline/cancel produced no artifact at all (`artifacts/final.json` missing; checker
  "read:artifacts/final.json INCONCLUSIVE").

## What changed (all in `/Users/guillaume/Github/solos-swarm-improve`, see `improvements.patch`)
| File | Change |
|---|---|
| `src/progress.rs` (new) | Pure helpers: 70% warning, 90% final notice, one-line work notes, partial-finding text. Unit tests. |
| `src/broker.rs` | (a) coordinator-role task goes to agent-00 and workers skip it; (b) `budget_notice` appended to tool results at >=70%, and `fetch_url`/`run_code` refused at >=90% (`tool_budget_blocked` event) so the last 10% of the envelope buys the filing turn; fraction is max(money, turn ceiling); (c) `defer_task` writes a **PARTIAL** record (Board posts, last 8 commands, calls, spend) when the agent did work, the old defer text otherwise; (d) `file_partial_findings` + `synthesize_artifact` build the exact-union artifact host-side under `same_records`; (e) optional Goal field `cache_hit_price_microusd_per_million` (default absent = unchanged) settles `prompt_cache_hit_tokens` at that rate; holds stay all-miss so settle never exceeds the hold; (f) oversized response headers are trimmed (`headers_truncated`) instead of failing the fetch. |
| `src/code.rs` | `run_code` receipt gains `queue_wait_ms`. |
| `src/main.rs` | Host files partial findings for unfiled claimed tasks, and synthesizes the artifact if the coordinator did not (`artifact_synthesized` event) and on deadline/cancel; price table in the manifest shows the cache rate. |
| `scripts/swarm_progress.py` (new) | `python3 scripts/swarm_progress.py RUN_DIR [--json]` or `--run-id NAME`: agents running/filed/partial/defer/unfiled, ledger vs cap, per-agent spend vs envelope. Verified on evidence-a3. |
| `scripts/remote.py` | `status` output gains a compact `progress` block. |
| tests | `src/broker_progress_tests.rs` (6), `src/progress.rs` (4), `tests/test_swarm_progress.py` (3). |

## Tests run
- `cargo test --locked --lib broker`: 27 pass (6 new). `cargo test --locked --test broker_http`: 34 pass.
  `--bin solos-swarm`: 6 pass. `--test evaluator`/`domain`: pass. `cargo fmt --check` clean.
- Full `cargo test --lib`: 170 pass, 2 fail (`cluster::tests::actual_raft_over_mtls...`,
  `kernel::final_tests::final_selection_drains...`). **Identical failures on the untouched baseline**
  under the full parallel run; both pass alone. `tests/context_artifacts` fails (2) because `work/`
  tokenizer assets were excluded from the copy.
- `python3 -m unittest discover -s tests -p 'test_*.py'`: 50 run, 1 fail
  (`test_goal_limits_admit_thirty_agents_and_refuse_thirty_one`), failing identically on baseline.
- `npm run test:broker` (node_modules symlinked read-only from the original): 6 pass.
- Linux-only sandbox tests (bwrap) skipped/not exercised on macOS. No live model calls.

## Risks
- Cache-aware ledger is a policy change (the ledger was deliberately conservative). It is opt-in per
  Goal (`cache_hit_price_microusd_per_million: 30000`, validated >= 30000 and <= input price) and
  needs a human decision; without it only the notices help. With it, agents get about 8x more turns,
  so also lower `max_calls_per_agent` or keep the 70/90% notices on.
- Partial records keep `severity: "defer"` for artifact compatibility; contracts that list severities
  (A3: breakthrough/useful/background) already tolerated `defer`. `evidence` carries the summary.
- A host-synthesized artifact after a coordinator failure still shows in evidence
  (`artifact_synthesized`, no `read_findings`), so the checker's event-completeness check will still
  flag it, deliberately. After a deadline the artifact covers only claimed tasks.
- Blocking run_code at 90% is only for non-coordinator agents; the coordinator is exempt.
- `queue_wait_ms` adds a key to the run_code result (additive).
- Not changed: run_code concurrency (4). Evidence says raise `SWARM_CODE_CONCURRENCY` (max 8) and do
  not run two 20-agent swarms on one VM; that is a deployment choice. Worker prompts that ask for the
  filing earlier (e.g. "file after at most 40 run_code calls") would help too and are Goal data, not code.

## Deploy (not done)
```sh
cd /Users/guillaume/Github/solos-swarm-improve
SWARM_SSH_CONFIG=$HOME/swarm-vm/ssh_config SWARM_SSH_HOST=swarm-vm python3 scripts/remote.py deploy
```
Do it only between runs (a deploy freezes a new candidate; running runs keep their candidate). Then start
runs with `--candidate <new id>`; add `"cache_hit_price_microusd_per_million": 30000` to the Goal only if approved,
and `SWARM_CODE_CONCURRENCY=8` in the host environment.
