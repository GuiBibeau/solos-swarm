# Campaigns

A campaign compares 1 worker against 10, and scored-history planning against single-draft refinement, on the
[mvp cluster path](mvp-cluster.md). It runs a frozen list of trials, scores each one with the separate evaluator,
and reports the differences. Source: `CampaignConfig`, `CampaignPlan`, `run_campaign` and `execute_campaign` in
`src/qa.rs`.

## Command

```bash
./target/release/solos-swarm mvp campaign \
	--config /path/to/campaign.json \
	--output /path/to/campaign-evidence
```

`--output` is a directory that must not exist yet. Neither may the config's `evidence_directory`. The command writes
one `trial-NNNN.json` per trial as it finishes, then `campaign.json` with the report, and prints the report on stdout.
It exits with an error unless the verdict is `PASS`.

## The matched design

Trials come in groups that share a `seed` and a `goal_case_id`. Each group has exactly four trials, one per cell:

| `agent_count` | `planning` | Worker history (`HistoryMode` in `src/planner.rs`) |
|---:|---|---|
| `1` | `history` | `scored_history`: the top-K scored attempts |
| `1` | `single-draft-refinement` | `single_draft_refinement`: the latest draft, one new candidate per turn |
| `10` | `history` | `scored_history` |
| `10` | `single-draft-refinement` | `single_draft_refinement` |

`CampaignPlan::validate` refuses a plan unless all of these are true:

- It has 8 to 1024 trials, in 2 groups or more, with unique trial ids.
- Every trial has `agent_count` 1 or 10, a positive `budget_microusd`, a `model_config`, and a non-empty dataset
  version, template version, agent config digest, runtime digest, scorer id, scorer key and scorer version.
- The four trials of a group have the same budget, fixture digest, dataset version, Goal template version, agent
  config digest, runtime digest, model config, scorer key and scorer version.
- `stopping_rule` is `fixed-all-trials`: every trial runs, with no early stop on a good result.
- `failed_run_rule` is `retain-failure-as-zero`: a failed trial stays in the report and scores 0.
- `minimum_goal_score` is between 0 and 1, and `trial_deadline_ms` is positive.

`CampaignConfig::validate` then checks each frozen trial against its plan. A trial must register exactly one Goal and
apply exactly one Swarm, both with the trial's `goal_id`, the plan's budget and the plan's evaluator. The Goal must
have a required Need with the predicate `{"kind":"catalog-final"}`. The workers' task inputs must be the exact
inputs of the public case. Their count, provider, model and history mode must match the plan.

## Scoring

The evaluator (`mvp evaluator-serve`, `src/evaluator.rs`) scores attempts in two phases, `search` and `held_out`. A
search attempt cannot satisfy the protected final Need. Only the held-out command can, and it runs a separate
contract check.

The campaign verdict comes from the trial records:

- `INCONCLUSIVE` if any record's evidence verdict is inconclusive or any cleanup verdict is not `PASS`.
- Otherwise `FAIL` if the unsafe effects exceed `maximum_unsafe_successful_effects`, the mean `correctness` is under
  `minimum_goal_score`, or any runtime verdict is `FAIL`.
- Otherwise `PASS`. A `PASS` becomes `INCONCLUSIVE` when the four trials of a group did not record the same
  `spent_microusd`.

A trial that fails or passes `trial_deadline_ms` is kept as a record with `failure` set and scores 0.

## `CampaignConfig`

All objects refuse unknown fields.

| Field | Type | Meaning |
|---|---|---|
| `client` | `QaConfig` | The authenticated client to the control plane, the same config `mvp qa --config` reads. |
| `plan` | `CampaignPlan` | The frozen comparison. |
| `trials` | map of trial id to `FrozenTrial` | One entry per trial in `plan.trials`. |
| `controlplane_public_key` | 32-byte hash | The pinned control-plane key. Must not be all zeros. |
| `evidence_directory` | path | The encrypted observation store. Must not exist yet. |

`CampaignPlan`:

| Field | Meaning |
|---|---|
| `id`, `version` | The campaign's name and version. |
| `trials` | The `TrialPlan` list. |
| `stopping_rule` | Must be `fixed-all-trials`. |
| `failed_run_rule` | Must be `retain-failure-as-zero`. |
| `minimum_goal_score` | The lowest mean `correctness` that passes, from 0 to 1. |
| `maximum_unsafe_successful_effects` | The most unsafe effects, over all trials, that still pass. |
| `trial_deadline_ms` | The time one trial may take. |

`TrialPlan`: `id`, `seed`, `goal_case_id`, `dataset_version`, `goal_template_version`, `fixture_digest`,
`agent_config_digest`, `runtime_digest`, `model_config` (with `provider` and `model`), `scorer_id`,
`scorer_public_key`, `scorer_version`, `budget_microusd`, `agent_count`, and `planning` (`history` or
`single-draft-refinement`).

`FrozenTrial`: `goal_id`, `runtime_node`, `runtime_configuration_sha256`, `public_case` (its `catalog_version` must
equal the plan's `dataset_version`), `setup` (the operator steps that register the Goal and apply the Swarm),
`owned_worker_units`, `evaluator_certificate`, `evaluator_ca`, `independent_receiver`, `unsafe_counter_pointer` and
`successful_effect_counter_pointer`.

## `CampaignReport`

| Field | Meaning |
|---|---|
| `schema_version` | `1`. |
| `plan` | The `CampaignPlan` that ran. |
| `verdict` | `PASS`, `FAIL` or `INCONCLUSIVE`, by the rules in [Scoring](#scoring). |
| `records` | One `TrialRecord` per trial: the plan, the run id, the runtime, goal, evidence and cleanup verdicts, `score_dimensions`, `usage`, the artifact and attempt references, the signed final evaluation, the unsafe and denied counts, `failure` and `elapsed_ms`. |
| `started_ms`, `stopped_ms` | The campaign's start and end. |
| `comparisons` | `history_minus_drafting_correctness`, `ten_minus_one_correctness` and `history_minus_drafting_evidence_quality`: differences of mean scores, with a failed trial counted as 0. |
| `actual_cost_pairs_matched` | `true` when the trials of every group recorded the same `spent_microusd`. |

## Scenario families

`solos-swarm mvp catalog --seed N --output DIR` writes one case: `public-case.json`, and `protected/case.json` in a
`0700` directory. The seed picks the family (`select_family` in `src/scenarios.rs`). The catalog version is
`bounded-goals-v1`.

| `CatalogFamily` | Objective |
|---|---|
| `record-reconciliation` | Reconcile ten synthetic record shards and list every seeded inconsistency once. |
| `dependency-planning` | Combine ten component inventories into a rollout order that respects every dependency, in waves of at most three components. |
| `evidence-synthesis` | Answer three total questions from ten document packets, citing every document and its revision. |
