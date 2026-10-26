# MVP operator runbook

Start `mvp serve` from a cold checkout. The historical preview in [POC-RESULTS.md](POC-RESULTS.md) uses a frozen binary and does not qualify a new build.

SSH target: `--host` or `SOLOS_SSH_HOST`. Example alias: `remote-ai`.

| Scope | What it shows |
|---|---|
| One host, separate UIDs | Ownership boundaries on that host |
| `min_nodes > 1` | Distinct hardware IDs in `independent_node_inventory` |
| Preview report | That candidate only |

Pending gates (500 agents, independent hosts, GPU, air-gap, numeric targets): [MVP-TRACEABILITY.md](MVP-TRACEABILITY.md).

## Build

Linux host matching the deployment target. macOS can run portable unit tests.

```sh
git clone https://github.com/GuiBibeau/solos-swarm.git
cd solos-swarm
cargo build --locked --release
npm ci --ignore-scripts --no-audit --no-fund
python3 scripts/bootstrap_tokenizers.py --output work/tokenizers
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
python3 -m unittest discover -s tests -p 'test_*.py'
npm run test:broker
```

Install `solos-swarm` (also `guard_binary` for `sandbox-enter`), pinned Node, Pi dependencies, tokenizers, templates, tool assets, licenses, and signed prices into a read-only release directory. Startup does not download Cargo, npm, models, or tokenizers.

## Offline bundle

`executable_artifacts` must be nonempty. Import checks ELF machine type, every digest, credential-looking names, escaping symlinks, and an existing destination.

```sh
python3 scripts/bundle.py create --source /absolute/path/public-stage --output /absolute/path/solos-release.tar --source-revision ACTUAL_GIT_REVISION --target x86_64-unknown-linux-gnu --requirements /absolute/path/bundle-requirements.json
python3 scripts/bundle.py verify /absolute/path/solos-release.tar
python3 scripts/bundle.py import /absolute/path/solos-release.tar --destination /absolute/path/immutable-release
```

Record archive, executable, runtime config, and public dependency hashes. A changed binary or config is a new qualification. Keys stay out of the public bundle.

## Accounts

The runtime refuses UID 0. Private files `0600`. Private directories `0700`. Config paths are absolute and owned by the service UID.

| Owner | Holds | Others see |
|---|---|---|
| Control plane | SVID keys, Raft storage key, signing seed, policy, worker CA, job dirs | Owner-only files |
| Evaluator | Ed25519 key, cases, bundle, selection state | Public leaf and CA; signed IPC |
| Witness | Signing seed, append-only journal | mTLS publication API |
| Operator | OIDC login and MFA session | Named roles and exact-action approvals |
| Worker | Sandbox and scoped SVID | Compiled-grant broker |

Evaluator IPC: shared group, evaluator-owned `0710` directory, socket `0660`. CP owns the delivery socket directory. Do not make the corpus directory traversable to the CP. Both directions bind signatures to the request; the evaluator checks the CP UID with `SO_PEERCRED`.

Set `SupplementaryGroups` when numeric systemd `User`/`Group` does not load memberships. Check the live PID's UID/GIDs before admitting Tasks.

## Identity

```text
spiffe://TRUST_DOMAIN/tenant/TENANT/goal/GOAL/node/NODE/agent/AGENT/incarnation/INCARNATION
```

| Rule | Value |
|---|---|
| Certificate | SPIFFE URI plus `node-NODE` DNS SAN |
| Key | Ed25519 |
| Leaf TTL | At most 1 hour |
| Renewal | Near TTL/3, inside the live Goal, grant, and allocation |
| Evaluator key | Pinned in `GoalPosture.evaluator_public_key` |
| Rotation | New approved Goal version; renewal is not rotation |

`IdentityAuthority::issue_csr` checks possession and returns public material only. The private key stays on the evaluator.

## Configuration

JSON. Unknown fields are rejected. Public keys and hashes are 32-byte arrays unless a CLI accepts hex. Domain deadlines are milliseconds. Certificate and price validity are Unix seconds.

| File | Content |
|---|---|
| `runtime.json` | [`RuntimeConfig`](../src/runtime.rs): deployment, node, cluster, identity, policy, providers, tools, evaluators, witness |
| `policy.json` | `Signed<DeploymentPolicy>` |
| `oidc.json` | [`OidcConfig`](../src/identity.rs) |
| `jwks.json` | Initial JWK set; live refresh required before serving |
| `price.json` | `Signed<PriceTable>` |
| `tools.json` | [`InstalledConfig`](../src/tools.rs) |
| `witness.json` | `WitnessConfig` |
| Evaluator `bundle.json` | [`ProtectedBundle`](../src/evaluator.rs) |

Set `runtime.run_id` per qualification. Do not reuse it after a binary, model, or config change.

Diagnostics default off. Loopback OTLP only:

```json
{"run_id":"operator-chosen-unique-run","diagnostics":{"otlp_http_endpoint":"http://127.0.0.1:4318","queue_size":512,"batch_size":64,"export_interval_ms":250,"export_timeout_ms":1000,"shutdown_timeout_ms":3000}}
```

Export posts OTLP JSON to `/v1/traces` and `/v1/metrics`. Non-loopback names, credentials, query strings, redirects, proxies, and ambient OTEL config are rejected. Export failure does not delete ledger, audit, or provenance records.

| Cluster limit | Value |
|---|---|
| QUIC frame | 256 MiB |
| Encrypted storage record | 1 GiB |
| `max_state_bytes` | At most half the frame ceiling |
| Admission reserve | 25% of that ceiling |
| Raft | Payloads in authenticated entry files; snapshot every 8 entries; one entry per batch |
| `reconcile_ms` | 100 to 10,000 |
| `lease_safety_ms` | 2x reconcile, at most 30,000 |
| Worker SVID TTL | 30 to 3,600 seconds |
| RPC line | 1 KiB to 1 MiB |

`RuntimeConfig.cluster.nodes` maps node IDs to `{addr, server_name, certificate_sha256}`. Supply the initial voter set once. Preserve the whole storage directory, including `raft.entries`, on restore.

`RuntimeConfig.node` fields: `id`, `capacity`, `used`, `labels`, `capabilities`, `healthy`, `provider_rate_remaining`. Resource vector: `cpu_millicores`, `memory_bytes`, `provider_slots`, `rate_tokens`. Advertise only isolation the host has.

`workers` names pinned Node, Pi, guard, issuer CA, broker SVID, skill digests, and tool definitions. Qualification sets `allow_degraded_landlock` to false. The Linux guard needs Landlock ABI 6, bubblewrap, and a user systemd manager with cgroup ownership.

## Credentials and providers

Exactly one hosted credential source when `features.local=false`:

```json
{"source":"inherited_fd","fd":3}
```

```json
{"source":"one_use_socket","path":"/absolute/private/credential.sock","expected_uid":1234,"timeout_ms":5000}
```

| Rule | Limit |
|---|---|
| Credential read | 16 KiB; owner, peer, and single line checked |
| Price metadata | At most 64 ASCII categories, 128-byte names |
| Reconciliation reserve | 128 KiB per pending, dispatched, or unknown effect |
| Dispatch retry | None; the adapter does not retry automatically |
| Slot wait | Inside the original deadline; no funded effect while queued |

A monetary-cap denial records `budget_exhausted` and a version-bound pause. Cleanup does not clear it. A new call needs a new ID, current authority, signed prices, and its own deadline.

Local adapter: `features.local=true`, `credential=null`, approved loopback endpoint, same ledger path.

| Profile | Slot size |
|---|---|
| Qwen3-4B conformance (`--ctx-size 32768 --parallel 2`) | 16,384 tokens |
| Ten-worker reconciliation fixture (`--ctx-size 65536 --parallel 2`) | 32,768 tokens |
| Actual-model fixture output reserve | 2,048 tokens |
| Scripted profile output bound | 1,024 tokens |

Tokenizer id `qwen3_llama_cpp_b11429`, pinned 4B tokenizer, pinned GGUF template, `enable_thinking=false`. The HF template is a different artifact. Probe with [bootstrap_tokenizers.py](../scripts/bootstrap_tokenizers.py) and `scripts/probe_tokenizer.py` after any model, server, or template change.

`features.model`, the signed price model, and `tokenizer.model` share one API alias. Optional `provider.ca_pem` is a bounded public CA for a declared HTTPS receiver. Proxies and redirects stay disabled.

## Start

Evaluator account:

```sh
/absolute/release/solos-swarm mvp evaluator-serve --bundle /absolute/evaluator/private/bundle.json --certificate /absolute/evaluator/public/svid.der --private-key /absolute/evaluator/private/key.der --ca /absolute/evaluator/public/ca.der --state /absolute/evaluator/private/state.json --ipc-socket /absolute/evaluator/ipc/evaluator.sock --broker-uid ACTUAL_CP_UID
```

Witness account:

```sh
/absolute/release/solos-swarm mvp witness-serve --config /absolute/witness/witness.json
```

Control plane, credential channel ready:

```sh
/absolute/release/solos-swarm mvp serve --config /absolute/controlplane/runtime.json
```

Systemd: fixed unit names, finite deadlines, `KillMode=control-group`. A crashed evaluator dispatch stays unknown.

`runtime.evaluators`: at most 64 Goal-scoped supervisors. `max_jobs_per_goal` is 1 to 65,536 and also bounded by budget and state. Delivery origin is `https://node-NODE`; the service appends `/evaluator/command`. The evaluator Session needs a compiled `tool_call` for operation `evaluate` and resource `evaluate/GOAL`.

Generated WASI runs in `wasi-worker` under systemd, bubblewrap, Landlock, and seccomp. Native tools need prepared Firecracker assets, or the declared gVisor fallback without KVM. Failed launches keep mode-`0600` `failure.json` in the mode-`0700` job directory.

Evaluator user bus: `/run/user/UID` owned by that UID, mode `0700`. The candidate namespace gets no user bus, corpus, key, or ambient environment.

## Cold cluster past certificate expiry

Stop every control-plane process. Each node owner:

```sh
/absolute/release/solos-swarm mvp offline-startup-request --config /absolute/controlplane/runtime.json --output /absolute/operator/node-1-request.json
```

Assemble one JSON array of those requests. Install fresh leaves for the existing identities. `certificates.json` maps node IDs to public DER paths. Signer must already be pinned. Expiry within five minutes.

```sh
/absolute/release/solos-swarm mvp offline-startup-assemble --config /absolute/controlplane/runtime.json --requests /absolute/operator/requests.json --certificates /absolute/operator/certificates.json --epoch NEXT_MONOTONIC_EPOCH --expires-ms ABSOLUTE_UNIX_MILLISECONDS --output /absolute/operator/offline-admission.json
```

Place that file at each node's `cluster.offline_admission_file`, then start. Quorum commits the anchors before any provider credential or worker launch.

## Operator session

```sh
curl --fail --silent --show-error --cacert /absolute/operator/server-ca.pem --request POST https://OPERATOR_HOST:PORT/operator/challenge
curl --fail --silent --show-error --cacert /absolute/operator/server-ca.pem --header 'Content-Type: application/json' --data-binary @/absolute/operator/private/login.json https://OPERATOR_HOST:PORT/operator/session
```

`login.json`: `{ "nonce": "returned nonce", "token": "real issuer token" }`. Keep `{session_id,token}`. Later calls send `Authorization: Bearer TOKEN` and `x-swarm-session: SESSION_ID`.

Token checks: audience, issuer, signature, `acr`, required `amr`, fresh `auth_time`, `principal_type="human"`, nonce. The nonce is single use.

## Goal and swarm

Bind the contract before approvals:

```rust
bind_goal_contract(&mut posture, &spec)?;
spec.security = serde_json::to_value(&posture)?;
let registration_hash = posture.registration_hash()?;
```

Catalog predicates: `{"kind":"catalog-goal"}`, `{"kind":"catalog-task","task_id":"..."}`, `{"kind":"catalog-final"}`. Only an independently signed held-out result satisfies the final Need.

```json
{"request_id":"read-before-register","operation":{"command":"read_state","scope":{"scope":"global"}}}
```

| Command | Requirement |
|---|---|
| `approve` | Server-signed exact-action approval |
| `register_goal` | Distinct human approvals for the bound contract |
| `apply_swarm` | Ten workers plus the evaluator service, outside the worker count |
| Privileged amend / stop release | Two distinct privileged humans, fresh step-up |

Placement reserves the gang atomically. A partial gang does not launch. History context is the top 20; a planning round admits at most 8 new candidates. Baseline refinement uses the latest draft and `new_candidates=1`.

`mvp catalog` output, including expected data, stays in the evaluator's private directory. Workers receive the public case only.

## Observe and stop

| Action | Effect |
|---|---|
| `task_status` on `task/GOAL_ID` | At most 128 metadata rows; no Task bodies |
| `read_state` | Authoritative revision, snapshot, signed head |
| `engage_stop` | Blocks new effects; liabilities remain |
| `release_stop` | Exact stop binding plus two privileged humans |
| Retraction | Invalidates descendants; archive stays for auditors |

`mvp inspect --snapshot /absolute/readback.json --kind explain --id GOAL/TASK` reads a saved record. A bare snapshot reports authoritative revision as unknown.

Flow defaults: 60-second window, 10,000 records, 1,000 flags, fan-in/fan-out threshold 16. Shared provider traffic is exempt from anomaly flags.

## Evidence, QA, campaigns

```sh
/absolute/release/solos-swarm mvp verify --snapshot /absolute/evidence/snapshot.json --head /absolute/evidence/head.json --trusted-key /absolute/evidence/cp-public-key.json --output /absolute/evidence/verification.json
/absolute/release/solos-swarm mvp inspect --snapshot /absolute/evidence/snapshot.json --kind board --id GOAL_ID
/absolute/release/solos-swarm mvp inspect --snapshot /absolute/evidence/snapshot.json --kind attempt --id GOAL_ID/ATTEMPT_ID
/absolute/release/solos-swarm mvp qa --config /absolute/operator/qa.json --profile integration --output /absolute/evidence/qa-run
/absolute/release/solos-swarm mvp campaign --config /absolute/operator/campaign.json --output /absolute/evidence/campaign-run
```

`QaConfig.max_response_bytes`: 1 KiB to 256 MiB. Missing recipes or receiver observations are `INCONCLUSIVE`. Campaign rules: [CAMPAIGNS.md](CAMPAIGNS.md).

## Release qualification

```sh
python3 scripts/qualify.py --bundle /absolute/release/solos-release.tar --report /absolute/evidence/report.json --profile /absolute/verifier/private/profile.json --attestation /absolute/verifier/public/release-attestation.json --trusted-key /absolute/verifier/public/public-key.json --verifier /absolute/verifier/solos-swarm
```

`--promote` is a separate destination. The profile pins `verifier_sha256`. The report names exactly ten distinct live `participating_agent_ids`, equal to `useful_agent_ids`. Held liability is zero. For `min_nodes > 1`, `node_hardware_identities` match `independent_node_inventory` and are distinct. Extra processes on one host, including the example alias `remote-ai`, do not satisfy that gate.
