# Evaluation campaigns

`solos-swarm mvp campaign` runs a frozen plan through the authenticated operator and broker. It does not mint operator identities, create approvals, read an evaluator key, or bypass a Capability.

```sh
solos-swarm mvp campaign --config /absolute/path/campaign.json --output /absolute/path/campaign-run
```

## Plan

| Rule | Value |
|---|---|
| Cases | At least two independent catalog cases |
| Cells per case | 1 worker and 10 workers, each with scored history and single-draft refinement |
| Shared across cells | Total budget, public inputs, model, dataset/template version, worker policy, scorer key and version |
| Per cell | Own Goal and Goal-scoped evaluator URI |
| Service agents | Excluded from the worker count; still need grants, leases, and resources |

## `CampaignConfig`

| Field | Content |
|---|---|
| `client` | `QaConfig`: controller identity, operator session, receiver probes, evidence key file |
| `plan` | `CampaignPlan`: `TrialPlan`s, stop rules, deadlines, regression thresholds |
| `trials` | Trial ID to `FrozenTrial` |
| `controlplane_public_key` | Expected audit signer |
| `evidence_directory` | Fresh directory; encryption key stays outside it |

Each `TrialPlan` has `scorer_id` and `scorer_public_key`. The URI is Goal-scoped. The key and version identify the scorer pool.

Each `FrozenTrial` supplies:

| Field | Use |
|---|---|
| `goal_id` | Admitted Goal |
| Public case | Worker-visible catalog input |
| `setup` | Normal operator commands |
| Supervisor units | Owned unit names |
| Evaluator certificate and CA | Public only |
| Receiver name and JSON pointers | Unsafe and successful effect counters |
| `runtime_node` | Expected node |
| `runtime_configuration_sha256` | 32-byte config digest |

## Freeze

1. One `RegisterGoal` with real approvals and one `ApplySwarm` per trial.
2. `security::bind_goal_contract` before approval.
3. Pin the evaluator public key.
4. Required Need predicate: `{"kind":"catalog-final"}`.
5. Oracle fixtures and evaluator keys stay in the evaluator's owner-only files.

Worker inputs: `qa::campaign_task_inputs(public_case, agent_count)`. Ten workers get one shard each. One worker gets the combined public records. Policy fingerprint: `qa::campaign_agent_config_digest(swarm)`.

| Cell | History |
|---|---|
| Scored history | `history.mode="scored_history"` |
| Refinement | `history.mode="single_draft_refinement"`, `new_candidates=1` |

Both keep the full archive and the retraction barrier. A model completion opens one planning round from its admitted context.

## Evaluator

One Goal-scoped evaluator service per trial. Capability `evaluate/<goal>`, nonbillable, with an allocation lease. The service owns its Unix socket, checks the control-plane UID, and signs results with its SVID. The protected bundle pins the broker mTLS socket and origin. The control plane does not read the private key or oracle.

## Verdict

The driver waits for a signed final evaluation and checks:

| Check | Required |
|---|---|
| Evaluator key | Approved key |
| Contract and lineage | Current |
| Audit | Signed checkpoint |
| Definitions | Exact frozen Goal and Swarm |
| Workers | Each declared worker has a settled inference and a Finding in the final lineage |
| Receiver | Positive permitted effect |
| Runtime measurement | Signed packet matches frozen executable and config hashes |
| Cost | Matched measured spend; equal caps alone are not equal spend |

Software measurement is not hardware attestation.

Failures, timeouts, and unknown outcomes stay in the report. Cleanup is Goal-owned and bounded. The driver does not kill ambient processes and does not drop a failed cell.

| State | Meaning |
|---|---|
| Production campaign | Unrun |
| Preview in [POC-RESULTS.md](POC-RESULTS.md) | Frozen preview binary only |
| OIDC/MFA, separate hosts, GPU, TRD scale targets | Separate qualifications |
