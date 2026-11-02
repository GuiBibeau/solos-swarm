# ADR 0001: One hexagon, two modes

- Status: proposed
- Date: 2026-11-02
- Plan: [docs/agent-mode/README.md](../agent-mode/README.md)

## Context

solos-swarm runs batch swarms: one Goal, N sandboxed Pi agents, a deadline, one artifact (`solos-swarm run`). We
also want the long-lived "super-agent" described in DeepSeek's *Exploring Super-agent Design* (November 2026) and
its `dsh-superagent-prototype`. That design has a Router that receives events from chat and GitHub (the prototype
ships GitHub and Feishu Sources; Slack and schedules are ours to add),
isolated Worker sessions per task, two-stage event filtering, Thread subscriptions with pass-through, async tools,
separate compute, state and execution, and a Watchdog.

The live `run` path is about 5k lines (`src/main.rs`, `src/broker.rs`, `src/broker_control.rs`, `src/code.rs`,
`src/progress.rs`). It hard-wires Pi (a free `sandbox()` function returning a `bwrap` `Command`), one hosted
OpenAI-compatible upstream (`provider_for` always builds `OpenAiCompatible` behind the existing `broker::Provider`
trait), a Goal-shaped `Store` that holds the event log, the ledger and the
swarm's task board together, and `agent-NN` identities. The `mvp` cluster path (about 55k lines) has its own copies
of the provider trait, broker, Pi launcher, extension and ledger. `run` uses it only for
`provider::completion_usage`.

## Decision

1. **Ports and adapters.** The domain cores do no I/O and depend only on ports. Every technology sits in an
   adapter behind a port: a harness (Pi, Claude Code, Codex, DeepSeek Harness), a model upstream (DeepSeek,
   Venice, llama.cpp), a Source or Channel (Slack, GitHub, CLI), an execution environment (local `bwrap`, a remote
   node), or a store.
2. **Two application cores, one shared kernel.** Swarm mode (Goal, Task, Board, Finding, Artifact) and agent mode
   (Trigger, Thread, Eval, Ticket, Assignment) are separate cores. Both run on the same adapters: the broker as the
   single keyless model gate, the ledger, the harness adapters, the sandbox and `run_code`, the hash-chained event
   log, pause/drain control, progress notices and the dashboard. Neither mode keeps a private copy of any of them.
3. **Swarm is also an execution plan.** An agent-mode Ticket runs as `Plan::Session` (one worker) or
   `Plan::Swarm { agents }`. A swarm Plan runs the same Goal machinery that `solos-swarm run` uses, as a library
   call, under the Ticket's budget. `solos-swarm run` stays as a thin composition root for batch use.
4. **One workspace, one binary, one CI.** The pure agent domain is the workspace crate `crates/agent-core`
   (`solos-agent-core`). Its `tests/boundary.rs` refuses any dependency outside `serde`, `serde_json` and `sha2`, and
   any `std::` path outside an allowlist of pure modules (a source scan, not a proof). Adapters and the swarm core stay in the root crate until they are
   stable enough to split. `default-members` makes `cargo test` and `cargo clippy` cover both.
5. **Functional core, effect queue.** Agent-core commands return their external work as `Effect`s committed in the
   same `ChangeSet` as the state change ("stored first, done after"). Adapters perform effects and report facts back
   through Engine commands. The prototype's core is synchronous too, but its ports enqueue work as calls. Returning
   Effects as data lets tests and the fuzzer assert on exactly what would leave the core, with no fakes.
6. **The swarm path moves first.** Each shared port gets its first consumer in swarm mode, with a parity check,
   before agent mode uses it. Swarm mode must not regress at any step.

## Consequences

- Swarm runs and their external readers (`check_run.py`, `swarm_progress.py`, `dashboard_server.py`,
  `recover_run.py`, `replay/`) keep their file formats through the refactor. That is a gate on every step.
- The shared kernel has to drop swarm-specific assumptions: `agent-NN` ids, "`agent-00` is the coordinator", the
  one-account ledger, and the domain text in `nudge()`/`deferred_record`. They become a principal (account, role,
  grants) and Goal data.
- The `mvp` stack is frozen, not deleted. Its local-provider support, QUIC mTLS transport, SPIFFE identities and
  scoped tool credentials are harvested as adapters (local model, BYOM node link, tool credentials) when a phase
  needs them.
- More moving parts in one workspace: CI time grows with the second crate (it is small and pure).

## Alternatives considered

- **Separate repositories or binaries per mode.** Rejected: the broker, ledger and sandbox would fork, which is the
  `run`/`mvp` duplication again.
- **Build agent mode inside DeepSeek Harness as a plugin, like the prototype.** Rejected as the core, accepted as an
  adapter: solos-swarm's value is the governed kernel (budgets, keyless broker, sandbox, audit chain). The host
  harness can be a Router or worker behind the harness port.
- **A full workspace split now** (`swarm-core`, `ports`, `adapters-*`). Deferred: the `run` code is too entangled
  to split mechanically, and splitting before the seams are extracted only moves the tangle.
