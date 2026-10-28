# solos-swarm

<p align="center">
  <img src="docs/assets/banner.svg" alt="solos-swarm: Governed control plane for multi-node agent swarms" width="100%" />
</p>

[![ci](https://img.shields.io/github/actions/workflow/status/GuiBibeau/solos-swarm/ci.yml?branch=main&label=ci&color=27272A&labelColor=121215)](https://github.com/GuiBibeau/solos-swarm/actions/workflows/ci.yml)
[![rust](https://img.shields.io/badge/rust-1.96.0%20%C2%B7%202024-27272A?labelColor=121215)](Cargo.toml)
[![node](https://img.shields.io/badge/node-24.18.0-27272A?labelColor=121215)](package.json)
[![pi](https://img.shields.io/badge/pi--coding--agent-1.1.0-27272A?labelColor=121215)](package.json)
[![license](https://img.shields.io/badge/license-Apache--2.0-27272A?labelColor=121215)](LICENSE)

A Rust control plane, scheduler, and capability broker for governed agent swarms across Linux clusters.

## What is in `main`

- **`solos-swarm run`**: a single-host run of N sandboxed Pi agents against one Goal, with a model broker that enforces
  a microUSD budget (conservative holds, cache-aware settlement), per-task envelopes, and a hash-chained event log.
- **`scripts/remote.py`**: the controller for running on a Linux host over SSH: deploy immutable candidates, start,
  status, collect and cancel runs, plus channels, drain and doctor.
- **Pause mode**: model calls can be held in peak-price windows or by an operator; deadlines count unpaused time.
  [docs/pause-mode.md](docs/pause-mode.md)
- **Self-update**: `stable`/`canary` candidate channels with a promotion gate, rollback, prune, drain, doctor and
  smoke. `run --resume` (re-spawning agents of a drained run) is not implemented.
  [docs/self-update-operations.md](docs/self-update-operations.md)
- **Dashboard** and a Mac SSH-tunnel script. [docs/dashboard.md](docs/dashboard.md)
- **Replay-scored policy loop** for the tournament self-review. [replay/README.md](replay/README.md)
- **Tournament runbook** from the first live run. [docs/running-a-tournament.md](docs/running-a-tournament.md)
- The original `mvp` cluster path (Raft over QUIC, witness, evaluator), documented in `docs/ARCHITECTURE.md`.

See [CHANGELOG.md](CHANGELOG.md) for what changed and when.

## Architecture

<p align="center">
  <img src="docs/assets/architecture.svg" alt="solos-swarm architecture" width="100%" />
</p>

## Primitives

| Primitive | Role |
|---|---|
| `Goal` | Top-level intent, microUSD budget cap, deadline, and signed operator approval. |
| `Need` | Verifiable sub-requirement with an acceptance predicate and rubric binding. |
| `Task` | Fenced work unit carrying a monotonic token, lease, and trace context. |
| `Board` | Revision-ordered coordination log with bounded reads, cursors, and retractions. |
| `Capability Lease` | Signed `DispatchPermit` bound to workload SVID, `Goal`, fence, and epoch. |
| `Finding & Attempt` | Broker-signed worker outputs and scored candidates with provenance lineage. |
| `Spend Ledger` | Pre-dispatch peak microUSD hold and once-only provider receipt settlement. |
| `Witness & Evaluator` | Isolated oracle scorer and append-only Merkle `SignedTreeHead` witness. |
| `Raft over mTLS QUIC` | OpenRaft state machine over Quinn TLS 1.3 with SPIFFE peer validation. |

## Runtimes

| Runtime | Isolation |
|---|---|
| `Pi Agent Sandbox` | Pinned Pi 1.1.0 inside `bwrap`, Landlock, `seccomp`, and Unix RPC socket. |
| `WASI Worker` | Separate `wasmtime` process with deterministic fuel and memory caps. |
| `Firecracker MicroVM` | KVM microVMs with scoped broker SVID over vsock (`gVisor` fallback). |
| `Model Broker` | Keyless worker gate for DeepSeek API and local Qwen / `llama.cpp`. |

## Quickstart

```sh
# Build and verify contracts
python3 scripts/bootstrap_tokenizers.py --output work/tokenizers
cargo build --locked --release
cargo test --locked -- --test-threads=1
python3 -m unittest discover -s tests -p 'test_*.py'
npm ci --ignore-scripts --no-audit --no-fund && npm run test:broker
```

```sh
# Run on a Linux host from the controller (needs SSH access, bubblewrap and a provider key in the Pi auth file)
python3 scripts/remote.py doctor                                    # host and controller health, no spend
python3 scripts/remote.py deploy                                    # build an immutable candidate; prints its id
python3 scripts/remote.py candidates promote --candidate ID         # gate (integrity, doctor, smoke, idle probe) then switch
python3 scripts/remote.py start --candidate current --run-id RUN --seed 1 --goal-file goal.json --expected-file expected.json --refuse-peak
python3 scripts/remote.py status --run-id RUN
python3 scripts/remote.py collect --run-id RUN --output evidence/RUN    # leaves the code/ scratch dir on the host
```

```sh
# Launch witness, evaluator, runtime, and QA
solos-swarm mvp witness-serve --config /path/to/witness.json
solos-swarm mvp evaluator-serve --bundle /path/to/bundle.json --certificate /path/to/eval.der   --private-key /path/to/eval.key --ca /path/to/ca.pem --state /path/to/state.json   --ipc-socket /path/to/evaluator.sock --broker-uid 1001
solos-swarm mvp serve --config /path/to/runtime.json
solos-swarm mvp qa --config /path/to/qa.json --profile integration --output /path/to/evidence
```

```sh
# Inspect state and verify evidence offline
solos-swarm mvp inspect --snapshot /path/to/snapshot.json --kind board --id GOAL_ID
solos-swarm mvp verify --snapshot /path/to/snapshot.json --head /path/to/head.json   --trusted-key /path/to/public-key.json --output /path/to/verification.json

# Follow the latest local Lima swarm (read-only)
python3 scripts/dashboard_server.py --lima swarm --latest

# Or inspect a retained run from local files
python3 scripts/dashboard_server.py --state /path/to/run/state.json --config /path/to/run/config.yaml
```

![Swarm dashboard — demo](dashboard/assets/dashboard.png)

The dashboard follows the selected run's agents, model, budget, and receipts. It polls every five seconds; `--latest` follows the newest state file. The server binds to `127.0.0.1` and never starts agents or makes model calls.

For non-JSON YAML configs, install `scripts/requirements-dashboard.txt` in the reader's Python environment (the Lima guest for `--lima`). For a local virtual environment: `python3 -m venv work/dashboard-python && work/dashboard-python/bin/pip install -r scripts/requirements-dashboard.txt`; then use `work/dashboard-python/bin/python` for the server and Python checks.

`Live` requires both a fresh state file and a confirmed active service. Retained or unavailable sources show `Snapshot`, `Stale`, or `Unavailable`; missing measurements stay `—`. Orbit, lattice, and helix change the layout of the same recorded workers. Use `?demo=1` for the bundled example presets, or Import JSON for a saved snapshot.

### Viewing the dashboard of a run on the remote box

Full notes: [`docs/dashboard.md`](docs/dashboard.md).

The server has no authentication, so it is never bound to a public interface: it listens on `127.0.0.1` on the box and you reach it through an SSH tunnel from your Mac. It has no token option; the Host-header check only admits `127.0.0.1:<its port>` and `localhost:<its port>`, which is why the tunnel uses the same port on both ends.

```sh
# one-time, on the box: install the template unit (scripts/systemd/solos-dashboard@.service), never enabled
mkdir -p ~/.config/systemd/user && cp scripts/systemd/solos-dashboard@.service ~/.config/systemd/user/ && systemctl --user daemon-reload
# on the Mac: start the unit for one run, open the tunnel, open the browser
scripts/dashboard-tunnel.sh --start RUN_ID            # = ssh -N -L 4173:127.0.0.1:4173 remote-ai, then http://127.0.0.1:4173/?source=runtime
scripts/dashboard-tunnel.sh --dry-run                 # print the commands only
```

The unit reads the run's `state.json` and `config.yaml` read-only and starts no agents. The box has no source checkout, so the unit runs `dashboard_server.py` from a deployed candidate (`~/.local/share/solos-swarm/<id>/scripts/`): put `DASHBOARD_CANDIDATE=<id>` in `~/.config/solos-swarm/dashboard.env` on the box before starting it.

### Replay-scored policy loop

`replay/` records run history, scores candidate exploration policies on it without model calls, and gates adoption behind a held-out check; see [`replay/README.md`](replay/README.md). Tests: `python3 -m unittest discover -s tests -p 'test_replay.py'`.

Dashboard checks: `node --test tests/dashboard-spatial.test.mjs tests/dashboard-telemetry.test.mjs` and `python3 -m unittest discover -s tests -p 'test_dashboard_server.py'`.

## Documentation

| Document | Scope |
|---|---|
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Domain model, Raft over mTLS QUIC, Tier-P sandboxes, and spend ledger. |
| [`docs/RUNBOOK.md`](docs/RUNBOOK.md) | Identity provisioning, offline startup, cluster recovery, and operations. |
| [`docs/CAMPAIGNS.md`](docs/CAMPAIGNS.md) | Matched 1-vs-10 worker evaluation design and schemas. |
| [`docs/pause-mode.md`](docs/pause-mode.md) | `control.json`, `PAUSE`, `SWARM_REFUSE_PEAK`, the 503 reply, unpaused deadlines. |
| [`docs/self-update-operations.md`](docs/self-update-operations.md) | Channels, promote/rollback/prune, drain, doctor, smoke, outage pause. |
| [`docs/self-update-and-graceful-restart.md`](docs/self-update-and-graceful-restart.md) | Design and failure modes behind the above. |
| [`docs/dashboard.md`](docs/dashboard.md) | Dashboard server and the Mac tunnel script. |
| [`docs/running-a-tournament.md`](docs/running-a-tournament.md) | Runbook from the first live tournament. |
| [`replay/README.md`](replay/README.md) | Replay-scored policy loop. |
| [`docs/rust-style.md`](docs/rust-style.md) | Rust style, lints and the gates CI runs. |
| [`CHANGELOG.md`](CHANGELOG.md) | PRs #6-#14 and what they added. |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | Live 10-agent DeepSeek results, microbenchmarks, and acceptance SLOs. |
| [`dashboard/index.html`](dashboard/index.html) | Spatial dashboard for runtime telemetry and saved snapshots. |
| [`skills/verify-solos-swarm/SKILL.md`](skills/verify-solos-swarm/SKILL.md) | Release qualification workflow for immutable candidates. |

## License

Apache-2.0. Copyright 2026 Gui Bibeau.
