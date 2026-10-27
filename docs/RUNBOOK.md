# Runbook

Operational commands for building, staging, running, and qualifying `solos-swarm`.


## 1. Host requirements and UIDs

| Requirement | Target |
|---|---|
| Kernel | Linux `6.1+` (Landlock v3+, seccomp-BPF, cgroups v2, userns) |
| Toolchains | Rust `1.96.0`, Node `v24.18.0`, Python `3.11+` |
| Packages | `bubblewrap`, `systemd`, `squashfs-tools`, `openssl` |

| Service | UID:GID | Owned paths |
|---|---|---|
| Control plane (`mvp serve`) | `999:985` | `/var/lib/solos-swarm/cp` (`0700`) |
| Evaluator (`mvp evaluator-serve`) | `995:985` | `/var/lib/solos-swarm/evaluator/protected` (`0700`) |
| Witness (`mvp witness-serve`) | `994:983` | `/var/lib/solos-swarm/witness` (`0700`) |


## 2. Build and verify

```bash
npm ci && npm run build
cargo build --release --locked
cargo test --locked
npm test
python3 -m unittest discover -s tests -p 'test_*.py'
./target/release/solos-swarm doctor --pi-root . --node "$(command -v node)"
```


## 3. Stage tokenizers, local model, and SquashFS

```bash
# Stage SHA-256 verified tokenizers
python3 scripts/bootstrap_tokenizers.py --output work/tokenizers

# Stage llama.cpp (b11429) and Qwen3-4B-Q4_K_M.gguf (optional)
python3 scripts/local_provider.py --directory /var/lib/solos-swarm/local-provider

# Probe receiver token conformance
python3 scripts/probe_tokenizer.py \
  --base http://127.0.0.1:8080 \
  --vectors /path/to/vectors.json \
  --output /tmp/probe-report.json \
  --verify-usage

# Build read-only worker SquashFS rootfs
python3 scripts/bootstrap_sandbox.py \
  --userland /path/to/staged-userland \
  --output /var/lib/solos-swarm/rootfs \
  --size-mib 512
```


## 4. Offline bundles and startup identity

```bash
# Create signed release bundle
python3 scripts/bundle.py create \
  --source /var/lib/solos-swarm/staging \
  --output /var/lib/solos-swarm/bundle-out \
  --signing-key /path/to/release-ed25519.pem \
  --key-id release-key-01 \
  --bundle-id bundle-2026-01 \
  --created-ms 1776000000000 \
  --expires-ms 1778592000000

# Verify and import bundle
python3 scripts/bundle.py verify \
  --bundle /var/lib/solos-swarm/bundle-out \
  --trusted-keys /etc/solos-swarm/trusted-release-keys.json \
  --now-ms 1776000100000

python3 scripts/bundle.py import \
  --bundle /var/lib/solos-swarm/bundle-out \
  --trusted-keys /etc/solos-swarm/trusted-release-keys.json \
  --destination /var/lib/solos-swarm/imported \
  --state /var/lib/solos-swarm/bundle-state.json \
  --now-ms 1776000100000

# Offline SPIFFE SVID ceremony
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


## 5. Start governed daemons

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


## 6. Standalone and remote 10-worker runs

### Local 10-worker run

```bash
python3 scripts/prepare_case.py \
  --seed 20260412 \
  --goal /tmp/run-01/case/goal.json \
  --expected /tmp/eval-01/expected.json

./target/release/solos-swarm run \
  --config /tmp/run-01/case/goal.json \
  --run-dir /tmp/run-01/evidence \
  --pi-root . \
  --node "$(command -v node)" \
  --credential-file /run/credentials/deepseek.key

python3 scripts/check_run.py \
  --run-dir /tmp/run-01/evidence \
  --expected /tmp/eval-01/expected.json \
  --candidate local-preview-01
```

### Remote systemd run (`SOLOS_SWARM_SSH_TARGET`)

```bash
python3 scripts/remote.py deploy
python3 scripts/remote.py start \
  --candidate poc-candidate-01 \
  --run-id run-20260412-01 \
  --seed 20260412 \
  --auth-file ~/.pi/agent/auth.json
python3 scripts/remote.py status --run-id run-20260412-01
python3 scripts/remote.py collect \
  --run-id run-20260412-01 \
  --output ./evidence/run-20260412-01
```


## 7. Inspect, verify, and qualify

```bash
# Inspect snapshot state or causal explanation
./target/release/solos-swarm mvp inspect \
  --snapshot /var/lib/solos-swarm/cp/state-snapshot.json \
  --kind explain \
  --id intent-0042

# Offline Merkle and ledger verification
./target/release/solos-swarm mvp verify \
  --snapshot /var/lib/solos-swarm/cp/state-snapshot.json \
  --head /var/lib/solos-swarm/witness/checkpoints.json \
  --trusted-key /etc/solos-swarm/trusted-keys.json \
  --output /tmp/verify-report.json

# Run qualification profile (quick | release-small | integration | acceptance | eval)
./target/release/solos-swarm mvp qa \
  --config /etc/solos-swarm/qa-config.json \
  --profile release-small \
  --output /tmp/qa-report.json

# Gate and promote release
python3 scripts/qualify.py \
  --bundle /var/lib/solos-swarm/bundle-out \
  --report /tmp/qa-report.json \
  --profile /etc/solos-swarm/release-profile.json \
  --attestation /var/lib/solos-swarm/cp/runtime-attestation.json \
  --trusted-key /etc/solos-swarm/trusted-release-keys.json \
  --verifier ./target/release/solos-swarm \
  --promote /var/lib/solos-swarm/promoted-release.json
```
