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
npm ci && npm run test:broker
cargo build --release --locked
cargo test --locked
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
# Create a bundle tar from a prepared source (the output must be outside the source)
python3 scripts/bundle.py create \
  --source /var/lib/solos-swarm/staging \
  --output /var/lib/solos-swarm/bundle-out/candidate.tar \
  --source-revision "$(git rev-parse HEAD)" \
  --target x86_64-unknown-linux-gnu \
  --requirements /path/to/requirements.json

# Verify and import bundle
python3 scripts/bundle.py verify /var/lib/solos-swarm/bundle-out/candidate.tar

python3 scripts/bundle.py import /var/lib/solos-swarm/bundle-out/candidate.tar \
  --destination /var/lib/solos-swarm/imported

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
  --expected /tmp/eval-01/expected.json
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

### Live-probe Goals: the `fetch` policy

A Goal that grants `fetch_url` lets its agents reach the network through the broker. Its `scope` is a label and its
prompt a request, so the Goal can also carry a `fetch` policy, which the broker enforces on every call:

```json
"fetch": {
  "hosts": ["zelix-front-dev.fly.dev", "zelix-deck.vercel.app"],
  "methods": ["GET", "HEAD", "OPTIONS"],
  "per_agent_rps": 1,
  "follow_redirects_off_list": false
}
```

| Field | Meaning |
|---|---|
| `hosts` (required, 1 to 64) | The hosts a request may name: an exact name, `*.example.com` for any name under that suffix (never the bare suffix; `*.com` is refused), or an IP address. Case and a trailing dot do not matter; a port is not part of the match. |
| `methods` | The methods allowed (`GET HEAD POST PUT PATCH DELETE OPTIONS`). Left out, every method `fetch_url` supports. |
| `per_agent_rps` | Calls per second one agent may make, 0.01 to 1000. A call that follows redirects counts once. Left out, no limit. |
| `follow_redirects_off_list` | Default `false`: a redirect to a host off the list is refused and not followed. `true` follows it, though the URL the agent asked for must still be on the list. |
| `allow_private_networks` | Default `false`: see below. |

The broker judges the URL the agent asked for, then every redirect hop before it is requested. A refused call sends
nothing. The agent gets a tool result, not an HTTP error, so a swarm worker can read it:

```json
{"refused": true, "reason": "redirect_off_list", "error": "fetch_url refused: ... redirects to https://accounts.google.com/..., whose host ... is not on this Goal's fetch allowlist ...", "url": "https://accounts.google.com/..."}
```

`reason` is `host_not_allowed`, `method_not_allowed`, `redirect_off_list`, `rate_limited` (with `retry_after_ms`),
`private_address` or `header_not_allowed` (a `Host` header of the agent's own, which would name another site on a
listed address; under a policy the broker sets `Host` from the URL). Each refusal is a `fetch_refused` event in the run's `events.jsonl` (agent, requested URL, the URL
refused, hop, reason), and `scripts/check_run.py` re-checks every completed `fetch_url` against the policy as a
`fetch-scope` check. A policy needs the `fetch_url` grant, `Goal::validate` reads every field, and an unknown key or
a missing `hosts` is refused, so a typo cannot leave a Goal looking restricted.

**Private addresses.** Every swarm `fetch_url`, with a policy or without one, refuses an address that is loopback,
private (RFC 1918), link-local (`169.254.0.0/16`, the cloud metadata address), carrier-grade NAT or Tailscale
(`100.64.0.0/10`), multicast, reserved or documentation, and an IPv6 address that wraps one of those. A host name is
resolved once per hop, all its addresses must be public, and the connection is then pinned to them, so a name that
answers differently the second time cannot get past the check. Set `allow_private_networks: true` in a policy to
reach one on purpose, such as a staging service on the same network. Without a policy there is no way to allow it:
a Goal that must probe a private host names it in a policy.

**Without a policy** the Goal behaves as before: any public host, any method, no rate limit. The one change is the
private-address refusal above. Agent mode's own `fetch_url` (`src/agent/broker.rs`) does not go through this guard
yet, so it has no private-address check.

**Audit scopes.** `scope` must be on the deployment allowlist: `synthetic-fixtures-only`, `public-web-research` and the
`authorized-audit:<target>` scopes, each a reviewed authorization named in `src/broker.rs`
(`AUDIT_SCOPES_BEFORE_FETCH_POLICY`, `AUDIT_SCOPES`). All share one envelope (deadline, spend and call limits). A
target added to `AUDIT_SCOPES`, such as `authorized-audit:zelix`, must carry a `fetch` policy when its Goal grants
`fetch_url`; the two scopes frozen before the policy existed keep running without one.

`scripts/prepare_platform_case.py` emits the policy for the live platform red team (`--fetch-host`,
`--fetch-methods`, `--fetch-rps`, `--allow-rpc`). A swarm Ticket in agent mode sets the same `fetch:` key in its Goal
template. `scripts/prepare_redteam_case.py` builds a source-review Goal that grants no `fetch_url`, so it has none.


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
