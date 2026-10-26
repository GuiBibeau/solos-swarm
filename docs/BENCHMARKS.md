# Benchmarks

Measured 10-agent execution metrics, tokenizer conformance, release microbenchmarks, and cluster SLOs.


## 1. Live 10-agent run (`record-reconciliation-v1`)

| Metric | Value |
|---|---|
| Run ID | `live-20260412-132831` (`seed: 20260412`) |
| Runtime | Node `v24.18.0`, `pi-coding-agent` `1.1.0`, Tier `P` |
| Model | `deepseek-chat` |
| Wall-clock duration | `124,782 ms` (`124.78 s`) |
| Completed workers | `10 / 10` (`agent-00` .. `agent-09`) |
| Completed shard tasks | `10 / 10` (`0` duplicate claims) |
| Shard findings | `10 / 10` exact (`15` mismatches) |
| Final artifact | Exact match (`score: 1.0`) |
| Provider calls | `22` (`2` per worker + `2` synthesis) |
| Tokens | `40,264` in / `4,462` out (`44,726` total) |
| Settled spend | `17,438 micro-USD` (`$0.017438` of `$1.00` cap) |
| Remaining holds | `0 micro-USD` |
| Leaked keys | `0` |


### Per-worker breakdown

| Agent | Task | Worker (`ms`) | Synthesis (`ms`) | Calls | Result |
|---|---|---:|---:|---:|---|
| `agent-00` | `shard-01` | `51,384` | `66,188` | `4` | Exact + final artifact |
| `agent-01` | `shard-02` | `51,210` | `0` | `2` | Exact |
| `agent-02` | `shard-00` | `53,941` | `0` | `2` | Exact |
| `agent-03` | `shard-03` | `50,812` | `0` | `2` | Exact |
| `agent-04` | `shard-04` | `55,104` | `0` | `2` | Exact |
| `agent-05` | `shard-05` | `58,619` | `0` | `2` | Exact |
| `agent-06` | `shard-06` | `54,302` | `0` | `2` | Exact |
| `agent-07` | `shard-07` | `56,088` | `0` | `2` | Exact |
| `agent-08` | `shard-08` | `57,411` | `0` | `2` | Exact |
| `agent-09` | `shard-09` | `52,970` | `0` | `2` | Exact |


## 2. Local receiver conformance (`Qwen3-4B` + `llama.cpp`)

| Check | Result |
|---|---|
| `llama.cpp` build | `b11429` (`llama-server` SHA-256 `5faf410b...2231d`) |
| Model | `Qwen3-4B-Q4_K_M.gguf` (`7485fe6f...4fdf5`) |
| Chat format | `qwen3_llama_cpp_b11429` (`enable_thinking=false`) |
| Token ID parity | `100%` match across `19`, `28`, `148`, `186` token vectors |
| Tested slot contexts | `4,096` and `16,384` tokens/slot |


## 3. Kernel microbenchmarks (`cargo test --release --locked`)

| Operation | Workload | Measured p99 | Target SLO |
|---|---|---:|---:|
| Local QUIC mTLS RTT | `1 KiB` frame | `< 1.0 ms` | `<= 1.0 ms` |
| Task CAS claim contention | `10` claimants / `1` task | `< 5.0 ms` | `<= 100 ms` |
| Backward lineage walk | Depth-10 DAG (`1,000` runs) | `< 2.0 ms` | `<= 20 ms` |
| Forward retraction walk | `10,000` descendants | `< 250 ms` | `<= 2,000 ms` |
| Planner history assembly | `10,000` attempts (`1,000` runs) | `< 50 ms` | `<= 200 ms` |
| Planner compaction | `10,000` attempts | `< 25 ms` | `<= 100 ms` |
| Delegation attenuation check | `100,000` random trees | `0` widening | `0` widening |


## 4. Target cluster SLOs (`500` agents, `N >= 3` nodes)

| Metric | Target |
|---|---|
| 500-agent schedule latency | All `Running <= 10 s`; per-agent `p99 <= 500 ms` |
| Infeasible gang reservation | `0` partial allocations |
| Node failure convergence | `<= lease_ttl + 10 s` |
| Raft leader failover | `<= 5 s` |
| Cross-node QUIC mTLS RTT | `p99 <= 5 ms` (`1 KiB` LAN) |
| Transport throughput | `>= 50,000 msgs/s` per node |
| Board publish-to-visible | `p99 <= 50 ms` on LAN |
| Finding write-to-queryable | `p99 <= 100 ms` on LAN |
| Idle actor memory | `<= 256 KiB` per actor |
| 300-task fan-out placement | `<= 5 s` on `5` nodes |


## 5. Security invariants (`SEC-NF-1` .. `SEC-NF-29`)

| ID | Invariant | Target |
|---|---|---|
| `SEC-NF-1` | Provider key isolation | `0` keys in worker memory, disk, env, or logs |
| `SEC-NF-2` | Cap enforcement | Spend `<= cap + sum(dispatched overruns)` |
| `SEC-NF-3` | Kill-switch latency | Block `<= 1 s` (`p99`); stop sandboxes `<= 10 s` |
| `SEC-NF-4` | Kill-switch durability | `100%` retained across `20` restarts |
| `SEC-NF-5` | Crash reservation safety | `0` double settlements or lost holds (`10,000` crashes) |
| `SEC-NF-6` | Broker authz overhead | `<= 10 ms` (`p99`) per call |
| `SEC-NF-7` | Workload SVID TTL | `<= 1 h` with zero-drop rotation |
| `SEC-NF-8` | Tier `V` warm readiness | `p95 <= 500 ms` (`<= 125 ms` guest boot) |
| `SEC-NF-9` | Audit log coverage | `100%` events carry RFC 6962 inclusion proofs |
| `SEC-NF-10` | Audit tamper detection | `100%` edits/deletions caught against witness |
| `SEC-NF-11` | Tier `P` sandbox coverage | `100%` enforce Landlock, seccomp, userns, cgroups |
| `SEC-NF-12` | Egress containment | `0` out-of-allowlist connections or tool calls |
| `SEC-NF-13` | Escalation resistance | `0` escalations via board posts or peer frames |
| `SEC-NF-14` | Delegation attenuation | `0` widened capabilities or budgets |
| `SEC-NF-15` | Depth and fan-out caps | `depth <= 4` and open tasks `<= max_open_tasks` |
| `SEC-NF-16` | Envelope roll-up | Single charge at Goal scope; no sibling reuse |
| `SEC-NF-17` | Board integrity | `100%` signed posts; `0` authority changes |
| `SEC-NF-18` | Placement atomicity | `0` unfunded placements; holds survive release |
| `SEC-NF-19` | Revocation latency | Descendants refused `<= 1 s` (`p99`) |
| `SEC-NF-20` | Channel isolation | `0` direct peer sockets or shared writable mounts |
| `SEC-NF-21` | `C3` two-person rule | `0` admissions with `< 2` human MFA approvals |
| `SEC-NF-22` | Air-gapped bootstrap | `100%` offline verification with `0` egress |
| `SEC-NF-23` | Tenant isolation | `0` shared boards, brokers, or mTLS roots |
| `SEC-NF-24` | Provenance inheritance | Min trust and union markings on all derived nodes |
| `SEC-NF-25` | Untrusted gating | `0` `C2`/`C3` unlocks from untrusted findings |
| `SEC-NF-26` | Retraction race safety | `100%` descendants tainted with `0` racing calls |
| `SEC-NF-27` | Evaluator isolation | `0` worker reads of holdout cases or forged scores |
| `SEC-NF-28` | Admission caps | `0` tasks admitted above Goal/tenant cap |
| `SEC-NF-29` | Qualification integrity | `0` passes on modified binaries or missing proofs |
