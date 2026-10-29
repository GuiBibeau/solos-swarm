# Run tourney-20261028-w2: FAIL

Scope: single-host 1-30 agent live preview.

- runtime_verdict: FAIL
- goal_verdict: INCONCLUSIVE
- evidence_verdict: FAIL
- cleanup_verdict: PASS

Failed or missing checks:

- private-evaluator: INCONCLUSIVE — Malformed private evaluator payload
- runtime-outcome: FAIL — host-side Goal deadline expired
- admitted-identities: FAIL — One distinct admitted identity per admitted agent required
- goal-completed: FAIL — Goal must have completed
- intent-identities: FAIL — Unique effect IDs must belong to admitted agents
- contribution:agent-00: INCONCLUSIVE — An independently verifiable attributed shard result is required
- contribution:agent-01: INCONCLUSIVE — An independently verifiable attributed shard result is required
- contribution:agent-02: INCONCLUSIVE — An independently verifiable attributed shard result is required
- contribution:agent-03: INCONCLUSIVE — An independently verifiable attributed shard result is required
- contribution:agent-04: INCONCLUSIVE — An independently verifiable attributed shard result is required
- contribution:agent-05: INCONCLUSIVE — An independently verifiable attributed shard result is required
- contribution:agent-06: INCONCLUSIVE — An independently verifiable attributed shard result is required
- contribution:agent-07: INCONCLUSIVE — An independently verifiable attributed shard result is required
- contribution:agent-08: INCONCLUSIVE — An independently verifiable attributed shard result is required
- contribution:agent-09: INCONCLUSIVE — An independently verifiable attributed shard result is required
- event-completeness: FAIL — Missing authoritative Finding event; Coordinator must read all Findings before one recorded synthesis

Deferred coverage: 500-agent/three-node acceptance; Raft/QUIC and HA; local-provider/air-gap; full Tier P security; signed audit witness; three-family rotation.
