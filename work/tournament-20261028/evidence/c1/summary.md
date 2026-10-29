# Run tourney-20261028-c1: FAIL

Scope: single-host 1-30 agent live preview.

- runtime_verdict: FAIL
- goal_verdict: INCONCLUSIVE
- evidence_verdict: INCONCLUSIVE
- cleanup_verdict: FAIL

Failed or missing checks:

- private-evaluator: INCONCLUSIVE — Malformed private evaluator payload
- admitted-identities: FAIL — One distinct admitted identity per admitted agent required
- contribution:agent-00: INCONCLUSIVE — An independently verifiable attributed shard result is required
- contribution:agent-01: INCONCLUSIVE — An independently verifiable attributed shard result is required
- inference:agent-02: FAIL — Real completed inference with consistent count, receipt and usage required
- turn-cap:agent-02: FAIL — Agent call count must match recorded intents and admitted turn ceiling
- lifetime:agent-02: FAIL — Recorded process start and termination required
- contribution:agent-02: INCONCLUSIVE — An independently verifiable attributed shard result is required
- inference:agent-03: FAIL — Real completed inference with consistent count, receipt and usage required
- turn-cap:agent-03: FAIL — Agent call count must match recorded intents and admitted turn ceiling
- lifetime:agent-03: FAIL — Recorded process start and termination required
- contribution:agent-03: INCONCLUSIVE — An independently verifiable attributed shard result is required
- inference:agent-04: FAIL — Real completed inference with consistent count, receipt and usage required
- turn-cap:agent-04: FAIL — Agent call count must match recorded intents and admitted turn ceiling
- lifetime:agent-04: FAIL — Recorded process start and termination required
- contribution:agent-04: INCONCLUSIVE — An independently verifiable attributed shard result is required
- inference:agent-05: FAIL — Real completed inference with consistent count, receipt and usage required
- turn-cap:agent-05: FAIL — Agent call count must match recorded intents and admitted turn ceiling
- lifetime:agent-05: FAIL — Recorded process start and termination required
- contribution:agent-05: INCONCLUSIVE — An independently verifiable attributed shard result is required
- inference:agent-06: FAIL — Real completed inference with consistent count, receipt and usage required
- turn-cap:agent-06: FAIL — Agent call count must match recorded intents and admitted turn ceiling
- lifetime:agent-06: FAIL — Recorded process start and termination required
- contribution:agent-06: INCONCLUSIVE — An independently verifiable attributed shard result is required
- inference:agent-07: FAIL — Real completed inference with consistent count, receipt and usage required
- turn-cap:agent-07: FAIL — Agent call count must match recorded intents and admitted turn ceiling
- lifetime:agent-07: FAIL — Recorded process start and termination required
- contribution:agent-07: INCONCLUSIVE — An independently verifiable attributed shard result is required
- inference:agent-08: FAIL — Real completed inference with consistent count, receipt and usage required
- turn-cap:agent-08: FAIL — Agent call count must match recorded intents and admitted turn ceiling
- lifetime:agent-08: FAIL — Recorded process start and termination required
- contribution:agent-08: INCONCLUSIVE — An independently verifiable attributed shard result is required
- inference:agent-09: FAIL — Real completed inference with consistent count, receipt and usage required
- turn-cap:agent-09: FAIL — Agent call count must match recorded intents and admitted turn ceiling
- lifetime:agent-09: FAIL — Recorded process start and termination required
- contribution:agent-09: INCONCLUSIVE — An independently verifiable attributed shard result is required
- overlapping-lifetimes: FAIL — Every admitted agent lifetime must overlap

Deferred coverage: 500-agent/three-node acceptance; Raft/QUIC and HA; local-provider/air-gap; full Tier P security; signed audit witness; three-family rotation.
