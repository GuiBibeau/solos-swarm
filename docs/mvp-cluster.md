# Run the mvp cluster path

The `solos-swarm mvp` commands run the original cluster path: a control plane on Raft over mTLS QUIC, an
isolated evaluator and a witness, each under its own Unix user. This path is frozen, not deleted
([ADR 0001](adr/0001-hexagonal-ports-and-two-modes.md)). Single-host swarm runs use `scripts/remote.py` instead,
as the [runbook](RUNBOOK.md) describes. The design is in [ARCHITECTURE.md](ARCHITECTURE.md).

The paths under `/var/lib/solos-swarm` and `/etc/solos-swarm` below are examples. Use your own layout.

## Prepare the host

| Requirement | Target |
|---|---|
| Kernel | Linux with Landlock ABI 6 to 9 (Linux 6.12 or later), seccomp-BPF, cgroups v2 and user namespaces. Tier P refuses another Landlock ABI unless the sandbox policy sets `allow_degraded_landlock` (`src/sandbox.rs`). |
| Toolchains | Rust `1.96.0`, Node `v24.18.0`, Python `3.11+` |
| Packages | `bubblewrap`, `systemd`, `e2fsprogs` (`mkfs.ext4 -d`), `openssl` |

Each daemon runs as its own user and owns one directory:

| Service | UID:GID | Owned path |
|---|---|---|
| Control plane (`mvp serve`) | `999:985` | `/var/lib/solos-swarm/cp` (`0700`) |
| Evaluator (`mvp evaluator-serve`) | `995:985` | `/var/lib/solos-swarm/evaluator/protected` (`0700`) |
| Witness (`mvp witness-serve`) | `994:983` | `/var/lib/solos-swarm/witness` (`0700`) |

## Build and test

```bash
npm ci --ignore-scripts --no-audit --no-fund && npm run test:broker
cargo build --release --locked
cargo nextest run --workspace      # what CI runs, in shards. Plain `cargo test --locked` also works.
python3 -m unittest discover -s tests -p 'test_*.py'
./target/release/solos-swarm doctor --pi-root . --node "$(command -v node)"
```

The Rust tests need the pinned tokenizers in `work/tokenizers` (next section). The Python tests need PyYAML from
`scripts/requirements-dashboard.txt`. Three raft tests assert on the wall clock and can fail on a busy machine. CI
runs them one at a time, in the `ci-isolated` profile of `.config/nextest.toml`.

## Stage tokenizers, the local model and the sandbox image

```bash
# The SHA-256-verified tokenizers
python3 scripts/bootstrap_tokenizers.py --output work/tokenizers

# Optional: llama.cpp b11429 and Qwen3-4B-Q4_K_M.gguf for the local provider
python3 scripts/local_provider.py --directory /var/lib/solos-swarm/local-provider

# Check that a receiver counts tokens the way the tokenizer does
python3 scripts/probe_tokenizer.py \
	--base http://127.0.0.1:8080 \
	--vectors /path/to/vectors.json \
	--output /tmp/probe-report.json \
	--verify-usage

# The pinned read-only ext4 guest image for microVM workers, with its manifest
python3 scripts/bootstrap_sandbox.py \
	--userland /path/to/staged-userland \
	--output /var/lib/solos-swarm/rootfs.ext4 \
	--size-mib 512
```

## Make an offline bundle and the startup identity

```bash
# Create a bundle tar from a prepared source. The output must be outside the source.
python3 scripts/bundle.py create \
	--source /var/lib/solos-swarm/staging \
	--output /var/lib/solos-swarm/bundle-out/candidate.tar \
	--source-revision "$(git rev-parse HEAD)" \
	--target x86_64-unknown-linux-gnu \
	--requirements /path/to/requirements.json

# Verify the bundle, then import it
python3 scripts/bundle.py verify /var/lib/solos-swarm/bundle-out/candidate.tar
python3 scripts/bundle.py import /var/lib/solos-swarm/bundle-out/candidate.tar \
	--destination /var/lib/solos-swarm/imported

# The offline SPIFFE SVID ceremony: write the requests, have them signed offline, then assemble the bundle
./target/release/solos-swarm mvp offline-startup-request \
	--config /var/lib/solos-swarm/cp/runtime.json \
	--output /tmp/startup-requests.json

./target/release/solos-swarm mvp offline-startup-assemble \
	--config /var/lib/solos-swarm/cp/runtime.json \
	--requests /tmp/startup-requests.json \
	--certificates /tmp/issued-certs.json \
	--epoch 1 \
	--expires-ms 1776003600000 \
	--output /var/lib/solos-swarm/cp/offline-startup-bundle.json
```

## Start the daemons

Start them in this order, each as its own user:

```bash
# 1. Witness (UID 994)
./target/release/solos-swarm mvp witness-serve \
	--config /var/lib/solos-swarm/witness/witness.json

# 2. Evaluator (UID 995)
./target/release/solos-swarm mvp evaluator-serve \
	--bundle /var/lib/solos-swarm/evaluator/protected/bundle.json \
	--certificate /var/lib/solos-swarm/evaluator/public/evaluator.der \
	--private-key /var/lib/solos-swarm/evaluator/protected/key.der \
	--ca /var/lib/solos-swarm/evaluator/public/ca.der \
	--state /var/lib/solos-swarm/evaluator/protected/state/private.json \
	--ipc-socket /var/lib/solos-swarm/evaluator/ipc/service.sock \
	--broker-uid 999

# 3. Control plane (UID 999)
./target/release/solos-swarm mvp serve \
	--config /var/lib/solos-swarm/cp/runtime.json
```

## Inspect, verify and qualify

```bash
# Inspect the snapshot state, or the causal explanation of one record
./target/release/solos-swarm mvp inspect \
	--snapshot /var/lib/solos-swarm/cp/state-snapshot.json \
	--kind explain \
	--id intent-0042

# Verify the Merkle tree and the ledger offline
./target/release/solos-swarm mvp verify \
	--snapshot /var/lib/solos-swarm/cp/state-snapshot.json \
	--head /var/lib/solos-swarm/witness/checkpoints.json \
	--trusted-key /etc/solos-swarm/trusted-keys.json \
	--output /tmp/verify-report.json

# Run a qualification profile: quick (the default), release-small, integration, acceptance or eval
./target/release/solos-swarm mvp qa \
	--config /etc/solos-swarm/qa-config.json \
	--profile release-small \
	--output /tmp/qa-report.json

# Gate the release and promote it
python3 scripts/qualify.py \
	--bundle /var/lib/solos-swarm/bundle-out \
	--report /tmp/qa-report.json \
	--profile /etc/solos-swarm/release-profile.json \
	--attestation /var/lib/solos-swarm/cp/runtime-attestation.json \
	--trusted-key /etc/solos-swarm/trusted-release-keys.json \
	--verifier ./target/release/solos-swarm \
	--promote /var/lib/solos-swarm/promoted-release.json
```

To compare 1 and 10 workers under matched budgets, run a campaign: see [CAMPAIGNS.md](CAMPAIGNS.md).
