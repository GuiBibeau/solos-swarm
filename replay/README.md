# Replay-scored policy loop

Today's self-review loop (see `notes/offpeak-20261027-results.md`, `work/tournament-20261028/`) reads findings and
perf, then writes the next goal and a `prompt_patch` of at most 500 characters. Nothing scores that advice, nothing
replays past runs, and the policy is prompt text. This package closes that gap in the style of Dream-RSI
(arXiv 2609.14858): record every run as a replayable tree, express the exploration policy as code, score candidate
policies on recorded runs at zero model cost, and adopt one only if a guard says it beats the current policy on runs
it was not tuned on. Plain Python, stdlib only, no Rust changes.

## Pieces

| File | What |
|---|---|
| `schema.py` | History schema (one JSON line per run, per-attempt rows) and `survives_screen` |
| `ingest.py` | `state.json` (+`events.jsonl`) of a collected run -> history record; links referee verdicts to candidates |
| `policy.py` | Policy interface (wave size, rounds, budget, per-attempt call cap, trial budget, stop-after-N-null, who gets the next attempt) with the current tournament policy as `Baseline`; `Adaptive` as an example candidate; JSON specs |
| `simulate.py` | Replay simulator and reward |
| `guard.py` | Deployment guard: dev/held-out split, paired bootstrap CI, minimum history, unexplored-branch check |
| `propose.py` | Grid search on dev runs only (the stand-in for Dream-RSI's policy-development agent) |
| `hook.py`, `drive-hook.patch` | Optional `drive.py --replay-check` step |

## Use

```sh
# 1. collect evidence (box side, once per run; not done by this package, no ssh here)
python3 scripts/remote.py collect --run-id RUN --output work/tournament-20261028/evidence/w1
# 2. ingest
python3 -m replay ingest work/*/evidence-* work/tournament-20261028/evidence/* --out history.jsonl
# 3. score a policy spec, search on dev runs, ask the guard
python3 -m replay score   --history history.jsonl --policy spec.json
python3 -m replay propose --history history.jsonl > proposal.json   # {"policy", "tuned_on", "dev_mean"}
python3 -m replay check   --history history.jsonl --proposal proposal.json
```

Run it on the history that exists today (15 collected runs under `work/`): 0 are scorable, because none of them
carries counted trials, a placebo bar or referee verdicts, so the guard says `insufficient history`. That is the
correct answer until tournament waves are ingested.

## Replay semantics

A run's worker attempts are grouped by family and ordered. A policy asks for the next attempt in a family and gets
the recorded outcome back; its view holds screen survival and cost, never referee verdicts. A call cap or trial budget
below what an attempt used makes that attempt lose its result (no survivor), at a proportional cost. Asking beyond the
recorded depth returns nothing and is counted in `unexplored_share`. The guard refuses a candidate above 10 percent.

**Survival is derived, not self-reported** (`survives_screen`): filed, `trials_used` counted and within
`trials_budget`, `beats_placebo_max` (the 200 shuffled-day placebo maximum) and `mde_ok` true, `net_bps_per_trade` > 0.

**Reward per run** = confirmed survivors (+1) + unrefereed survivors (+0.2 each, at most 3 count) - refuted survivors
(0.5 each) - 0.001 per counted trial - 0.03 per dollar - 0.05 per wall-clock hour + 0.005 per attempt per round
(capped at 24, the Dream-RSI parallelism bonus). All weights are in `simulate.Weights`. More false positives cannot
buy reward: unrefereed survivors saturate, refuted ones cost, trials cost.

## Deployment guard (`guard.decide`)

Adopt only if all hold: at least 12 scorable runs and 6 held-out ones, otherwise `insufficient history`; the
candidate was not tuned on held-out runs (`tuned_on`); unexplored share <= 10 percent; mean paired reward difference
>= 0 on dev runs and > 0 on held-out runs; the 95 percent bootstrap CI over held-out runs excludes 0 from below. The
split is a hash of the run id, so it is stable as history grows. In pure-noise synthetic histories, a policy tuned
by grid search on dev is adopted in about 7 percent of cases (4 of 60 at 24 runs), see `tests/test_replay.py`.
Each adoption shrinks the held-out set's value: after adopting, treat those runs as spent and wait for new ones.

## drive.py wiring (optional, off by default)

`drive-hook.patch` adds `drive.py run --replay-check` to `work/tournament-20261028/drive.py` (that directory lives on
the local tournament branch, not on this one, so the patch is not applied here; check it with `git apply --check`).
After each close step (c1..c4) it ingests all collected evidence, looks for a proposal in
`replay-proposal-<step>.json` or in the close step's findings, writes `evidence/<step>-replay-verdict.json`
and appends the verdict to `launch-log.jsonl`. A `prompt_patch` alone is logged as `unscored`: prompt text has no
replay semantics. A `policy` spec is applied only through `hook.approved_policy()`, which returns it solely when the
stored verdict says `adopt`. Nothing in drive.py or gen.py consumes it yet; a later change can make gen.py read it.
The close-step record contract does not yet allow a `policy` field, so today the proposal file is the way in.

## Comparison to Dream-RSI

Copied: completed runs become a replay simulator (no model or evaluator calls); the policy is explored offline against
all history; the reward is quality minus per-attempt cost plus a parallelism bonus; the current policy is a candidate,
so adoption cannot lose on the history; replay reveals only what was recorded.

Different: (1) a hard deployment guard with held-out runs, bootstrap CI and a minimum history, since Dream-RSI's
guarantee holds only on the history it was selected on; (2) quality is survivors after an independent referee, with
trial counting and the placebo bar enforced by the scorer, because a discovery swarm can be rewarded for false
positives; (3) policies are JSON specs of a fixed interface, not agent-written code, so a proposal is data; (4) our
"tree" is families of independent attempts, not an expanding node tree; (5) proposals that are prompt text are never
scored.

Limits: the history is tiny and mostly null, so survivors are rare and rewards are dominated by cost; replay cannot
test branches never explored (a policy that wants more depth than any run recorded is refused); each recorded attempt
is one sample of a stochastic agent; referee verdicts exist only for the few candidates that were refereed; the reward
weights are hand-chosen and steer which policy wins; reward hacking remains possible through the weights and the
unrefereed credit, which is why that credit is small and capped; the guard bounds, not removes, overfitting to
the history (about 7 percent false adoption in noise at the default thresholds); the cost of the proposing step
itself is not in the reward.
