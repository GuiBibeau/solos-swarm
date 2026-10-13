---
name: verify-solos-swarm
description: Launches and verifies solos-swarm candidates through normal authenticated APIs, checks retained evidence, and inspects owned cleanup. Use when testing, diagnosing, or qualifying this repository's MVP or an immutable swarm runtime candidate.
---

# Verify solos-swarm

Read [the runbook](../../docs/MVP-RUNBOOK.md) for provisioning and normal API configuration, [the feature map](../../docs/MVP-TRACEABILITY.md) for requirement coverage, and [campaigns](../../docs/CAMPAIGNS.md) for matched comparisons. These instructions grant no operator role, Capability, or spending authority.

## Identify the candidate

Record the source revision, actual Linux executable digest, immutable bundle manifest, dependency/model/tokenizer digests, topology and hardware. Use the candidate's installed CLI and configuration. A rebuild produces a different candidate; the historical `scripts/remote.py` preview report does not qualify `mvp serve`.

```sh
solos-swarm mvp --help
python3 scripts/bundle.py --help
python3 scripts/qualify.py --help
```

Do not import ambient Pi configuration, extensions, sessions or provider credentials into a candidate. Operator-provisioned identities, evaluator corpus/key and witness key stay outside the worker-visible bundle.

## Check source contracts

Online build preparation is separate from offline runtime startup:

```sh
python3 scripts/bootstrap_tokenizers.py --output work/tokenizers
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python3 -m unittest discover -s tests -p 'test_*.py'
npm ci --ignore-scripts --no-audit --no-fund
npm run test:broker
```

Run required checks and report their actual exit statuses. Linux process/VM, separate UID, physical host, air-gap and performance requirements need retained execution evidence; a Mac unit test or parser check cannot qualify them.

## Launch and drive

Validate the runbook's private-file ownership, current signed policy/Goal contract, real operator session, pinned workload identities, shared quota, provider price expiry, evaluator socket and independent witness readiness. Refuse missing authority; do not construct pretend production humans or MFA.

```sh
solos-swarm mvp witness-serve --config /absolute/owned/witness.json
solos-swarm mvp serve --config /absolute/owned/runtime.json
solos-swarm mvp qa --config /absolute/owned/qa.json --profile integration --output /absolute/owned/new-evidence
```

Launch only the configured owned services. Use normal OIDC operator registration and workload-signed broker tools. The QA configuration names frozen recipes, independent receiver observations and already owned units; it has no database setter or authorization bypass. Missing selected recipes/observations are inconclusive.

For live provider runs, obtain authorization for the bounded total budget and frozen case before dispatch. Supply a provider key only via the runtime's admitted inherited descriptor or one-use peer-checked socket. Never print it, write it to a bundle, or put it in a worker environment. Retain failed cases and unknown liabilities; do not reroll a failure under the same case ID.

## Read evidence without replay

Use normal `ReadState` for the authoritative Raft revision and signed audit head. Saved raw snapshots have a Domain revision only; preserve the original readback packet separately.

```sh
solos-swarm mvp inspect --snapshot /absolute/owned/snapshot.json --kind board --id GOAL_ID
solos-swarm mvp inspect --snapshot /absolute/owned/snapshot.json --kind intent --id INTENT_ID
solos-swarm mvp inspect --snapshot /absolute/owned/snapshot.json --kind review --id INTENT_ID
solos-swarm mvp verify --snapshot /absolute/owned/snapshot.json --head /absolute/owned/head.json --trusted-key /absolute/owned/public-key.json --output /absolute/owned/verification.json
```

Check current Goal/evaluator versions, broker-signed input lineage, retractions, exact actual receipts, spent/held totals and witness inclusion/consistency. Inspect corruption controls on separate copies. Never modify original evidence or turn an unknown outcome into a refund. Evidence verification alone does not establish runtime success, Goal correctness or cleanup.

## Cleanup and qualification

Engage the appropriate stop through the normal operator API. Inspect only recorded run-owned unit names and process/cgroup records. Confirm every owned worker/tool/evaluator is stopped before reporting cleanup success or returning resource capacity. Keep evidence and unknown external outcomes.

Use the protected exact-artifact qualification command from the runbook. All required runtime, Goal, evidence and cleanup verdicts must pass; a missing protected profile/signature, idle contributors or a wrong candidate digest cannot qualify. Preserve independent-host and numeric qualification as pending until the declared original conditions actually pass.
