# Running matched MVP evaluation campaigns

`solos-swarm mvp campaign --config campaign.json --output campaign-run` executes a frozen campaign through the normal authenticated operator and broker paths. It does not issue operator identities, create approvals, read an evaluator key or bypass a Capability. Supply real operator session files using the same client configuration as `mvp qa`.

A campaign contains at least two independent catalog cases. Each case has four cells: one and ten workers, each with scored history and single-draft refinement. All four cells have the same total budget, public inputs, model, dataset/template version, shared worker policy and independently pinned scorer key/version. Each cell has its own Goal and Goal-scoped evaluator URI. Service agents are excluded from the worker count but still require admitted grants, leases and bounded resources.

The `CampaignConfig` JSON contains:

- `client`: the normal `QaConfig` with controller identity, operator session, named independent receiver probes and a separate encrypted evidence key file.
- `plan`: `CampaignPlan`, including every immutable `TrialPlan`, fixed stopping/failure rules, finite deadlines and regression thresholds. Each `TrialPlan` supplies `scorer_id` and `scorer_public_key`; the URI is Goal scoped while the key/version identifies the comparable scorer pool.
- `trials`: a map from trial ID to `FrozenTrial`. Every entry supplies `goal_id`, the catalog's public case, normal operator `setup` commands, owned supervisor unit names, public evaluator certificate/CA paths and an independent receiver name plus JSON pointers for its unsafe and successful effect counters.
- `controlplane_public_key`: the expected audit signer. A signed snapshot from another signer is rejected.
- `evidence_directory`: a fresh directory for encrypted, signed observations. Its key remains outside that directory.

Prepare each trial by freezing exactly one `RegisterGoal` command with its genuine approvals and one `ApplySwarm` command. Bind the full Goal contract with `security::bind_goal_contract` before requesting approval. Pin the independent evaluator's actual public key. Include a required Need whose predicate is `{"kind":"catalog-final"}`. Submit only public catalog input to workers; retain oracle fixtures and evaluator private keys in the independent service's owner-only files.

Use `qa::campaign_task_inputs(public_case, agent_count)` to obtain the matched worker inputs. The ten-worker cell receives one original shard per worker; the single worker receives their combined public records/components/documents. `qa::campaign_agent_config_digest(swarm)` computes the shared worker policy fingerprint. Exact full Goal and Swarm definitions are also checked against authoritative readback, so count and planning treatment cannot be relabeled afterward.

History cells use `history.mode="scored_history"`. Refinement cells use `history.mode="single_draft_refinement"` and `new_candidates=1`. Refinement reads the latest eligible draft; scored history selects compatible scored candidates. Both retain the complete archive and retraction barrier. Each model completion opens a recorded planning round using its exact admitted context; agents cannot supply a round ID or exceed its candidate limit.

Provision a Goal-scoped evaluator service and its admitted nonbillable `evaluate/<goal>` Capability/allocation lease for every frozen trial. The independent service owns its private Unix IPC socket and validates the control-plane UID and signed requests. Its protected bundle pins the normal broker's mTLS socket/origin; its own SVID signs and delivers results. The control plane never reads its private key or oracle. An unprivileged daemon cannot use `setuid` to manufacture this separation: the service must already be launched under its independently provisioned UID.

The driver waits for a separately signed final evaluation, verifies the approved evaluator key, current contract, lineage, signed audit checkpoint, exact frozen definitions and all receipts, and requires every declared worker to have a successful settled inference and a Finding in the final candidate's lineage. Its independent receiver must observe a positive permitted effect; a missing counter cannot stand in for zero unsafe effects.

Every trial, including setup failures, wrong answers, timeouts and unknown outcomes, remains in the report and encrypted observations. The driver always attempts bounded Goal-owned cleanup, verifies the existing owned supervisor units and never kills ambient machine processes. Equal budget caps do not establish equal expenditure: unmatched measured costs keep the acceptance verdict inconclusive. Comparison statistics remain available with all failures retained as zero.

The production campaign is currently unrun. Real OIDC/MFA, independently provisioned evaluator services, physically independent hosts/witnesses, GPU serving and the unchanged TRD scale/performance targets remain separate qualifications. The earlier ten-agent preview does not qualify this expanded binary.

Each `FrozenTrial` also pins `runtime_node` and the parsed `runtime_configuration_sha256` (32-byte array). Before setup and in final evidence the driver verifies the actual runtime instance measurement inside the current signed/checkpoint-bound packet against the frozen executable/configuration hashes. A declared hash in a trial file alone cannot qualify the running binary. This software measurement is not hardware attestation.
