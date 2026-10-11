# TRD v1: solos-swarm

## 1. Problem
TODO (Scribe)

## 2. Users and use cases
TODO (Scribe)

## 3. Goals and non-goals
TODO (Scribe)

## 4. Prior art (what we borrow, what we drop)
TODO (Scribe)

## 5. Architecture overview
TODO (Scribe)

## 6. Runtime and networking (Kernel)

This section specifies how solos-swarm places, runs, connects and recovers roughly 500 cooperating agents on a multi-machine Linux cluster. It is a requirements statement, not a design freeze. Every numeric value below is a proposed target to validate in the PoC, not a measured claim.

### 6.1 Terminology

- **Node**: one Linux host running the node agent (`swarmd`).
- **Agent**: one logical LLM-driven worker with an identity, a mailbox, a budget and a lifecycle. Agents are lightweight in-process tasks inside `swarmd`, not OS processes or containers by default. Process or sandbox isolation is a per-agent option owned by section 7.
- **Swarm spec**: a declarative document that describes the desired set of agents, their resource and budget requests, their placement constraints and their topology (who may talk to whom).
- **Control plane**: the replicated components that store desired and observed state and make placement decisions.
- **Data plane**: agent-to-agent and agent-to-provider traffic. It does not pass through the control plane.

### 6.2 Scheduler and placement

The scheduler maps agents in a swarm spec to nodes. We borrow structure from three systems and drop the parts that don't fit.

**From the Kubernetes scheduling framework, we borrow** the split into a *scheduling cycle* (pick a node) and a *binding cycle* (commit the decision). Scheduling cycles run one at a time and binding cycles can run concurrently. We also borrow its pluggable extension points: queue sort, filter (remove infeasible nodes), score plus normalize (rank feasible nodes), reserve/unreserve (hold resources before binding, with an idempotent unreserve), and permit (approve, deny or wait). The solos-swarm scheduler MUST expose the same phases as a Rust trait set so that placement policy is modular. The PoC ships filter plugins for capacity, labels and node health, plus score plugins for bin-packing and spread. **We drop** the per-Pod object model and volume and CSI plugins, and we don't schedule one unit at a time when a gang is requested.

**From Nomad, we borrow** the evaluation/plan model. Any change in desired state (a new or updated swarm spec) or observed state (a node is lost) creates an *evaluation*. Scheduler workers turn an evaluation into a *plan*: the allocations to create, update or stop. The leader serializes plans through a *plan queue* and rejects all or part of any plan that would over-subscribe a node. This lets several scheduler workers run optimistically without locks, which matters when 500 agents land at once. We also borrow Nomad's two placement phases: a feasibility pass, then ranking by bin-packing with an automatic anti-affinity term so that replicas of the same role don't share a node. **We drop** Nomad's task drivers (Docker, exec, Java) for v1. The only driver is the in-process agent runtime.

**From Ray placement groups, we borrow** gang scheduling. A swarm or sub-swarm (for example "one red team of 40 agents plus a coordinator") is reserved as a list of *bundles*, and the reservation is atomic: if any bundle can't be placed, nothing is reserved and the group stays pending. We adopt Ray's four strategies: PACK, STRICT_PACK, SPREAD and STRICT_SPREAD. We also adopt its fault behaviour: creation is atomic, but after a node loss the lost bundles are rescheduled at elevated priority, and the group may run partially until they are. **We drop** Ray's object store and its implicit task-level scheduling.

**Resources** in the PoC are: CPU millicores, memory, concurrent provider calls (a per-node outbound concurrency slot), and token budget per time window. Token budget is a first-class schedulable resource. A node that is close to a provider rate limit MUST be scored lower.

**Requirements**
- R-SCH-1: Placement MUST be declarative and reconcile toward the swarm spec continuously.
- R-SCH-2: Gang reservation of N bundles MUST be all-or-nothing.
- R-SCH-3: The scheduler MUST support plugin-based filter and score phases.
- R-SCH-4: The leader MUST validate plans against current node capacity before committing (optimistic concurrency).

### 6.3 Agent lifecycle

Each agent has one authoritative lifecycle state in the control plane and a mirrored local state on its node:

`Pending -> Scheduled -> Starting -> Running <-> Paused -> Draining -> Stopped`, with failure branches `Running -> Failed`, `Starting -> Failed` and `* -> Lost` (the node is unreachable and its lease has expired).

- **Pending**: accepted, not yet placed (or waiting on a gang).
- **Scheduled**: a plan has bound it to a node. The node has not acknowledged yet.
- **Starting**: the node has loaded identity, credentials scope and persisted intent log, and is replaying unfinished intents (6.9).
- **Running**: consuming its mailbox and allowed to call providers and tools.
- **Paused**: mailbox retained, no new side effects. Used for budget exhaustion and operator holds.
- **Draining**: no new work accepted. In-flight intents finish or are checkpointed, then the agent hands off.
- **Stopped / Failed / Lost**: terminal for this incarnation. A restart policy (never, on-failure, always) creates a new incarnation with a higher incarnation number.

Transitions are written to the control-plane store. A node MUST NOT move an agent to Running unless it holds a valid lease for that agent's allocation (6.9).

### 6.4 Control plane and data plane split

The control plane holds desired state (swarm specs), observed state (node membership, allocations, agent lifecycle), leases and fencing counters. It is small, strongly consistent and low-throughput. The data plane carries agent messages and provider calls. It is high-throughput and partition-tolerant, and it never routes through the control-plane leader.

**State store options**

1. **Embedded Raft using openraft.** openraft is a Rust Raft implementation with dynamic membership, leader leases, snapshots and pluggable storage and network traits. Its API is documented as unstable before 1.0, so upgrades can break. The benefit is a single static binary with no external dependency, and Raft traffic can share our QUIC transport.
2. **External etcd.** etcd v3 offers a gRPC KV with multi-version revisions, watch streams that resume from a revision, If/Then/Else transactions for compare-and-swap, and TTL leases whose expiry deletes attached keys. Its global 64-bit revision is a ready-made monotonic counter for fencing tokens. The cost is a second system to deploy and operate, plus a Go dependency on every target.

**Proposal**: the PoC uses embedded openraft (3 or 5 voters) behind a `StateStore` trait that mirrors the etcd primitives we need: KV with revisions, watch-from-revision, CAS transactions and TTL leases. An etcd backend implementing that trait is a non-goal for the PoC but is kept possible, so the openraft API risk stays contained. Open question for Gui: is an external etcd acceptable in target deployments?

### 6.5 Inter-node transport

**Option A: QUIC via quinn.** QUIC (RFC 9000) provides many independent streams per connection with no head-of-line blocking across streams, TLS 1.3 built into the handshake, credit-based flow control at both stream and connection level, peer-controlled limits on concurrent streams, connection migration, and optional unreliable datagrams. quinn is a userspace Rust implementation with a Tokio runtime binding, and it exposes connection and path statistics. Cheap streams map naturally to "one stream per agent conversation" or "one stream per message".

**Option B: gRPC via tonic over HTTP/2.** This is mature, has typed IDL and broad tooling, and etcd itself uses it. It runs over TCP, so all multiplexed streams share one TCP byte stream and one loss stalls all of them.

**Proposal**: QUIC/quinn for the data plane and for Raft replication, with a thin framed protocol (length-prefixed, versioned schema). Keep a tonic gRPC surface for the external operator API (CLI and UI), where tooling matters more than latency. **mTLS is mandatory on every inter-node connection.** Both ends present certificates and the node identity comes from the certificate. Certificate issuance and rotation belong to section 7 (Forge). QUIC 0-RTT MUST be disabled for state-changing messages, because RFC 9000 states that 0-RTT data has no replay protection.

### 6.6 Service discovery and membership

**Option A: gossip (SWIM).** The foca crate implements SWIM with the Infection and Suspicion extensions. It is `no_std`-capable and transport-agnostic: it emits bytes for us to send. It supports custom identities (which can carry a shard or version), incarnation numbers for refuting suspicion, and custom broadcasts, so agent-directory updates can piggyback on membership traffic.

**Option B: central registry.** Nodes and agents register in the control-plane store, and peers watch it.

**Proposal**: a hybrid. Use foca gossip over QUIC datagrams for *node liveness and fast failure suspicion*, because it scales without load on the leader. Use the Raft store as the *authoritative agent directory*, mapping agent id to node, incarnation and lease. Gossip only suggests that a node is down. Only the control plane declares it Lost, after its lease expires. Each node caches the directory through a watch, so routing a message to an agent is a local lookup.

### 6.7 Backpressure and flow control

Unbounded queues are banned on the data path.

- Every agent mailbox is a bounded `tokio::sync::mpsc` channel. When it is full, `send` waits, which is backpressure. Senders that must not block use `try_send` and get an explicit `Busy` error that the caller handles (retry with jitter, shed, or escalate).
- Across nodes, QUIC stream and connection flow-control credit and MAX_STREAMS limits bound the bytes and concurrency in flight per peer. The receiving node only grants credit as agents consume their mailboxes, so a slow agent pushes back to its remote senders and not onto node memory.
- An application-level credit window per (sender agent, receiver agent) pair stops one noisy agent from using up a shared connection.
- Provider calls go through a per-node, per-provider limiter (concurrency plus tokens per minute). Agents whose budget is exhausted move to Paused, not into a retry loop.
- Every queue exports depth and wait-time metrics.

### 6.8 Rust runtime

- **tokio** is the async runtime. quinn, tonic, openraft and kameo all integrate with it, and its bounded mpsc provides the backpressure primitive above.
- **Actor layer, compared briefly:**
  - **ractor**: Erlang gen_server style. Supervision trees, priority channels (signals, stop and supervision ahead of regular messages), process groups for broadcast and dispatch, and an optional cluster feature.
  - **kameo**: Tokio-based. Supervision, bounded or unbounded mailboxes, a console for watching mailbox backpressure. Its distributed mode runs on libp2p.
  - **actix**: mature, but tied to its own actor system and runtime conventions. It is less aligned with a Tokio-first, custom-transport design.
- **Proposal**: build agents as ractor-style supervised actors on tokio, with our own QUIC transport rather than an actor framework's built-in clustering, because we need to control mTLS, flow control and the routing table. The choice between ractor and kameo is a PoC spike. The decision criteria are idle memory per actor, the mailbox API and supervision ergonomics.

### 6.9 Failure and partition handling

- **Leases.** Each allocation (agent on node) holds a TTL lease in the control-plane store, renewed by the node. If renewal fails, the node MUST stop side effects for that agent before the lease could expire, measured with a monotonic clock and a safety margin. Expiry moves the agent to Lost and triggers rescheduling.
- **Fencing tokens.** Every lease grant returns a strictly increasing token (the store revision). Every side-effecting call to a shared resource (control-plane writes, tool executors, shared artifact stores) carries the token, and the resource rejects tokens lower than the highest it has seen. This follows Kleppmann's argument that leases alone are unsafe under process pauses and network delay. Correctness must not depend on timing.
- **At-least-once with idempotency keys (Solos Engine intent pattern).** Before any side effect (provider call, tool call, outbound message), the agent writes an *intent* record with a stable `intentId` to its durable intent log: `{intentId, agentId, incarnation, fencingToken, kind, payloadHash, state: pending}`. Only then does it perform the effect, passing `intentId` as the idempotency key, and afterwards it marks the intent `done` with the result reference. On restart or reschedule, the Starting phase replays `pending` intents. Receivers deduplicate by `intentId`. The result is at-least-once delivery with effectively-once effects wherever the receiver honours the key.
- **Partitions.** A minority-side control-plane replica can't commit, so no new placements happen there. Nodes cut off from the quorum keep Running agents only until their lease safety margin runs out, then Pause them. When the partition heals, stale incarnations are fenced out by token.

### 6.10 Observability

- Tracing through OpenTelemetry (`tracing` plus an OTLP exporter). One trace per task, with spans for scheduling, message hop (sender node, receiver node, queue wait), provider call and tool call. W3C trace context travels in every message header.
- Metrics: scheduler latency histograms, queue depths, QUIC connection stats (RTT, loss and congestion window, from quinn), Raft metrics from openraft, lease renewals and misses.
- Per-agent accounting: input and output tokens, model and provider, cost estimate and wall time, aggregated per agent, swarm and node, and enforced against budgets. Attribute names should follow the OpenTelemetry GenAI semantic conventions (now maintained in a separate repository) where they apply.

### 6.11 Performance: what to measure and how (proposed targets to validate in PoC)

All values below are **proposed targets to validate in PoC**, not claims. Section 9 references this table. Test bed: N = 3, 5 and 10 Linux nodes on one L2/L3 network. Record the hardware with each result. Agents use a mock provider with fixed latency so that LLM variance doesn't pollute the measurements.

| Metric | How to measure | Proposed target |
|---|---|---|
| Scheduling latency, 500-agent swarm | Time from spec submit to all 500 agents Running. Also per-agent submit to Scheduled, p50/p99, from trace spans | All Running ≤ 10 s on 5 nodes. Per-agent Scheduled p99 ≤ 500 ms |
| Gang atomicity | Submit an infeasible gang. Check that nothing is reserved | 0 partial reservations |
| Control-plane convergence after node loss | Kill -9 one node (and separately cut it off with iptables). Time to all affected agents Running elsewhere | ≤ lease TTL + 10 s |
| Leader failover | Kill the Raft leader. Time until the next committed write | ≤ 5 s |
| Message RTT, same node | Ping-pong between two agents, 1 KiB payload, 10^5 samples, p50/p99 | p99 ≤ 1 ms |
| Message RTT, cross node | Same, with agents on different nodes over QUIC with mTLS | p99 ≤ 5 ms on LAN |
| Throughput per node | Saturating fan-in of 1 KiB messages. Record msgs/s at which p99 RTT doubles | ≥ 50k msgs/s per node |
| Idle agent memory | RSS delta of `swarmd` spawning 1,000 idle agents, divided by 1,000 | ≤ 256 KiB per agent (excluding LLM context) |
| Backpressure safety | One stalled receiver under 10x overload. Watch node RSS and other agents' RTT | Bounded RSS. Other agents' p99 within 2x baseline |
| Duplicate side effects | Crash an agent mid-intent 1,000 times, then count duplicate effects at an idempotent mock receiver | 0 duplicates |

### 6.12 PoC acceptance criteria (runtime and networking)

1. A 500-agent swarm spec deploys across ≥ 3 nodes using gang reservation and PACK/SPREAD strategies, and reaches Running.
2. All inter-node traffic (data plane and Raft) runs over QUIC with mutual TLS. A plaintext or unauthenticated peer is rejected.
3. Killing one worker node or the Raft leader recovers automatically with no manual step. Displaced agents resume from their intent logs.
4. The fencing test passes: a paused, then resumed, stale node can't commit a side effect.
5. Every data-plane queue is bounded, and the backpressure test passes.
6. Traces show end-to-end spans across nodes. Per-agent token and cost totals are reported.
7. A reproducible benchmark harness reports every metric in 6.11, with results recorded against the proposed targets (pass, fail or revise).

### 6.13 Sources (fetched)

- https://kubernetes.io/docs/concepts/scheduling-eviction/scheduling-framework/
- https://developer.hashicorp.com/nomad/docs/concepts/scheduling/how-scheduling-works
- https://docs.ray.io/en/latest/ray-core/scheduling/placement-group.html
- https://docs.rs/openraft/latest/openraft/
- https://etcd.io/docs/v3.5/learning/api/
- https://www.rfc-editor.org/rfc/rfc9000.html
- https://docs.rs/quinn/latest/quinn/
- https://docs.rs/foca/latest/foca/
- https://docs.rs/tokio/latest/tokio/sync/mpsc/index.html
- https://docs.rs/ractor/latest/ractor/
- https://docs.rs/kameo/latest/kameo/
- https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html
- https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/ (page says the GenAI conventions moved to a separate repo)


## 7. Security and trust (Forge)
TODO (Forge)

## 8. Provider abstraction
TODO (Scribe)

## 9. Requirements, functional and non-functional (with perf targets)
TODO (Scribe). Perf targets: see section 6.11.

## 10. PoC scope and acceptance criteria
TODO (Scribe)

## 11. Risks
TODO (Scribe)

## 12. Open questions for Gui
TODO (Scribe)

## 13. Sources
TODO (Scribe)
