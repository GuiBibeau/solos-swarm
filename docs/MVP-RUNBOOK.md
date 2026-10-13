# MVP operator runbook

This runbook starts the expanded `mvp` runtime from a cold checkout. The historical `scripts/remote.py` preview has a separate frozen binary and checker. Its ten-agent result does not qualify a new MVP build.

The currently authorized machine is `remote-ai`. Start with ten workers and a finite Goal on that host. Separate Unix accounts exercise ownership boundaries on one host; they do not establish independent physical failure domains. The unchanged TRD 500-agent, independent-host, GPU, air-gap and numeric acceptance gates are mapped in [MVP-TRACEABILITY.md](MVP-TRACEABILITY.md).

## Build and freeze public dependencies

Build the runtime on a Linux host matching the deployment target, with the locked dependencies. macOS can run portable unit tests but cannot qualify the Linux isolation adapters. Prepare these public artifacts before launching the runtime:

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

Confirm the repository remote before using a clone URL for a different organization. Install the resulting `solos-swarm` executable (also used as `guard_binary` for `sandbox-enter`), pinned Node, Pi dependencies, prepared tokenizers/templates, instruction artifacts, tool assets, licenses and signed price files into a read-only release directory. Runtime startup performs no Cargo, npm, model or tokenizer downloads.

Prepare an offline bundle from a public staging tree. The requirements file records the actual target, features, libraries and isolation profile; it must have a nonempty `executable_artifacts` list. The importer checks ELF machine type as well as every digest and rejects credential-looking files, escaping symlinks and an existing destination.

```sh
python3 scripts/bundle.py create --source /absolute/path/public-stage --output /absolute/path/solos-release.tar --source-revision ACTUAL_GIT_REVISION --target x86_64-unknown-linux-gnu --requirements /absolute/path/bundle-requirements.json
python3 scripts/bundle.py verify /absolute/path/solos-release.tar
python3 scripts/bundle.py import /absolute/path/solos-release.tar --destination /absolute/path/immutable-release
```

Record the archive, executable, parsed runtime configuration and public dependency hashes with the release evidence. A changed binary or configuration starts a new qualification run. Private keys and provider credentials are provisioned separately from this public bundle.

## Provision ownership and identities

Use existing operator-provisioned service accounts and a real identity issuer. The runtime refuses UID 0. Provision these roles before starting it:

| Owner | Owns and can read | Runtime access |
|---|---|---|
| Control-plane service | Runtime and cluster SVID keys, encrypted Raft storage key, CP signing seed, approved policy, worker CA issuer, owned job directories | Owner-only private files and directories |
| Protected evaluator service | Independently generated Ed25519 key, protected cases, protected bundle and replay/selection state | Public leaf/CA only; framed authenticated IPC |
| Independent witness service | Witness signing seed and append-only journal | mTLS publication API only |
| Human operator | Actual OIDC login and MFA/step-up session | Named policy roles and exact-action approvals |
| Pi worker or generated program | Its bounded sandbox and scoped SVID | Normal compiled-grant broker only |

Private files use mode `0600`; private directories use `0700`. All paths in a service configuration are absolute. Runtime-owned directories must belong to its UID. The evaluator public certificate may live in an evaluator-owned `0750` directory with a `0640` public leaf; its protected key, cases and state remain in a different `0700` directory. The CP cannot write or read that private directory.

For the evaluator's separate-UID IPC, provision a dedicated group shared by the two service accounts. The evaluator owns its `0710` IPC directory and creates its socket `0660`. The CP owns the separate evaluator delivery socket directory; the runtime applies its configured evaluator group and creates that socket with bounded group traversal/connect permissions. Do not make a private evaluator corpus directory traversable to the CP or workers. The protected service checks the peer CP UID with `SO_PEERCRED`; both IPC directions also bind signatures to exact requests.

Retain the evaluator's private primary group and explicitly provision its configured delivery group with `SupplementaryGroups` when needed. Numeric systemd `User`/`Group` settings must not be assumed to load account memberships. Check the actual service PID's UID/GIDs and directory/socket access before admitting Tasks. The synthetic fixture launcher derives this delivery group from its admitted evaluator profile; it does not broaden directory or key permissions.

The workload identity is:

```text
spiffe://TRUST_DOMAIN/tenant/TENANT/goal/GOAL/node/NODE/agent/AGENT/incarnation/INCARNATION
```

Certificates contain this exact SPIFFE URI and `node-NODE` DNS name, use Ed25519, and have a maximum one-hour leaf TTL. The Goal pins the evaluator's independently generated public key in `GoalPosture.evaluator_public_key`. The CP's ability to issue a new leaf for the same name does not authorize replacing that key.

`IdentityAuthority::issue_csr` accepts a CSR from the existing evaluator key, verifies possession, and issues public certificate material only. The runtime supervisor renews near TTL/3, within the still-live Goal/grant/allocation authority. The evaluator verifies the broker-signed nonce, same key, same workload and unchanged CA, then atomically replaces its public leaf. The private key never travels to the CP. Key rotation needs a new approved Goal version; renewal is not key rotation.

## Prepare the configuration files

All service configurations are JSON and reject unknown fields. Public keys and hashes use a 32-element byte array unless a CLI explicitly accepts hex. Millisecond deadlines belong to domain/runtime objects; security, certificate and price-table validity times are Unix seconds. Confusing the two produces expired or overlong authority.

The current typed source is the schema of record:

| File | Type and required content |
|---|---|
| `runtime.json` | [`RuntimeConfig`](../src/runtime.rs): deployment ID, measured `Node`, cluster files, runtime identity, signing seed, approved policy/trust key, bounded state size, operator TLS/OIDC/JWKS, provider adapters, optional installed tools, evaluator registry, worker assets, reconciliation/lease safety and independent witness client |
| `policy.json` | `Signed<DeploymentPolicy>`: finite inventory, classes, resource limits, trusted runtime/evaluator identities, signing authorities, recipient clearances and actual `issuer|subject` root humans |
| `oidc.json` | [`OidcConfig`](../src/identity.rs): exact issuer/audience/algorithms, required MFA `amr`, allowed `acr`, session/step-up ages and bounded JWKS freshness |
| `jwks.json` | Initial JWK set for the configured issuer; the runtime must complete an authenticated live issuer/JWKS refresh before serving |
| `price.json` | `Signed<PriceTable>`: exact provider/model, currency, version, valid interval and rational rates for every billed category |
| `tools.json` | [`InstalledConfig`](../src/tools.rs): owned jobs/artifact directory and separate key, finite WASI/native limits, pinned V or explicit G assets, signed prices, finite export roots, broker-call definitions and exact target manifests |
| `witness.json` | `WitnessConfig`: TLS listener, witness-owned journal directory, immutable log ID, trusted CP public key file, independent witness seed, exact nonroot CP UID |
| Protected evaluator `bundle.json` | [`ProtectedBundle`](../src/evaluator.rs): broker public key, evaluator workload, exact Goal/version contracts and Need specs, nonempty frozen search and final sets, execution limits and pinned delivery target |

`RuntimeConfig.cluster.nodes` maps numeric node IDs to `{addr, server_name, certificate_sha256}`. Each node has an independent SVID and bounded private encrypted store. Supply the exact initial voter set once; later startup reuses the existing cluster. Framework QUIC frames are bounded at 256 MiB and encrypted storage records at 1 GiB. Configure `max_state_bytes` at no more than half the cluster frame ceiling for transaction overhead; normal admission reserves 25% of that ceiling for control/recovery. These are finite allowed capacities, not measured performance results. A single-voter local topology cannot qualify quorum failover or independent-host placement.

Raft retains log IDs in memory and reads payloads from individually authenticated encrypted entry files. Snapshots run every eight entries with no retained snapshot tail; replication batches contain one entry. Purge replaces the WAL with an authenticated index checkpoint before deleting unreferenced payload files. Existing payload-bearing WAL records and checkpoints remain readable. Preserve the entire owned storage directory, including `raft.entries`, when moving or restoring a node; copying the WAL alone is insufficient.

`RuntimeConfig.node` has `id`, `capacity`, `used`, `labels`, `capabilities`, `healthy` and `provider_rate_remaining`. Resource vectors contain `cpu_millicores`, `memory_bytes`, `provider_slots` and `rate_tokens`. Advertise measured finite capacity and only isolation capabilities present on the host. Do not advertise KVM, GPU or multiple failure domains from labels alone.

The runtime requires `reconcile_ms` between 100 and 10,000 and `lease_safety_ms` between twice that value and 30,000. Worker SVID TTL is 30–3,600 seconds; RPC lines are 1 KiB–1 MiB with bounded total output. The `workers` object names pinned Node/Pi/guard paths, a daemon-owned directory, worker issuer CA, broker SVID, installed skill digests and a finite tool definition list. Set `allow_degraded_landlock` to false for qualification. The strict Linux guard requires Landlock ABI 6, bubblewrap and a functioning user systemd manager with cgroup ownership and cancellation.

The hosted DeepSeek adapter uses `features.local=false` and exactly one credential source:

```json
{"source":"inherited_fd","fd":3}
```

or:

```json
{"source":"one_use_socket","path":"/absolute/private/credential.sock","expected_uid":1234,"timeout_ms":5000}
```

The operator launcher reads the existing Pi `deepseek` credential from its private configuration and writes only the key bytes to this one-use channel. Never copy the key into runtime JSON, command arguments, the public bundle, Pi worker auth files, worker environment or evidence. FD 3 must actually be inherited by the daemon; a systemd unit must explicitly preserve/provision that descriptor. A private peer-checked socket is usually easier for a separate launcher. The runtime bounds the credential read to 16 KiB and rejects a wrong owner, peer, multiline input or absent source.

Use a verified provider/model price table covering input, cached input, output and any other usage category the adapter reports. Admission reserves the complete upper bound before dispatch. Quorum state also reserves 128 KiB per pending, dispatched or unknown effect for bounded financial reconciliation, plus a stop/recovery margin; a dispatch is refused when that capacity cannot be retained. Price metadata is bounded before dispatch (at most 64 ASCII billing categories with 128-byte names). Large completed outputs live in the runtime's private encrypted response store with a separate `0600` key. The ledger and audit retain compact content references and semantic digests. A valid receipt is charged even when its completion is malformed, oversized or cannot be archived; evidence loss engages a stop and reports a missing body rather than undoing the fee. A lost or ambiguous upstream response remains an unknown liability; restarting is not permission to release it or retry a non-idempotent operation.

Provider concurrency is acquired within the original request deadline before admission reserves money or quota. Waiting creates no funded effect. When a slot opens, the broker checks current Task ownership, authority and the exact frozen request before admitting and dispatching it. A stopped or expired waiter releases its slot without a provider contact. The adapter never retries a dispatched request automatically.

Replay records retain content hashes and compact permit acknowledgements; the authoritative effect retains the complete exact request and payload. Normal state-capacity exhaustion defers worker startup with bounded backoff (1–30 seconds). It does not extend a Task lease, increase the state cap, or terminate the daemon's remaining workers. Existing worker/evaluator certificate renewal may use reserved control capacity only for the previously enrolled holder and still-valid authority; new identities require normal admission capacity.

After the batch startup gate opens, the worker checks current Task, grant, Goal and allocation authority before delivering its first Task prompt. Stop, withdrawal, expiry, an unavailable fresh read or an initial pipe failure enters the same child-reaping and diagnostic cleanup path. Idle prompts use that current-authority check as well; a successful terminal Task RPC suppresses further prompts.

Unused nonbillable permits are rechecked and refreshed in the same transaction as their local application. This preserves the original action, evidence, holder, grant, Task fence and deadline. An unrelated global epoch change does not strand an otherwise valid C0 operation. Changed C2/C3 review bindings require the current exact approvals; each waiting review is immutable, and retrying the same intent after approval consumes it at most once. Paid dispatch permits are never refreshed this way. The evidence verifier checks the original admission signature and the exact signed refresh audit chain.

A local adapter uses `features.local=true`, `credential=null`, the approved loopback/server endpoint, the actual per-slot context size and the same ledger/quota/receipt path. The verified Qwen3-4B profile uses `qwen3_llama_cpp_b11429`, the official pinned 4B tokenizer, the pinned GGUF template, and `enable_thinking=false`. With `--ctx-size 32768 --parallel 2`, the current tested receiver has **16,384 tokens per slot**. A fresh four-case probe matched complete prompt bytes, token IDs and usage at that size; the earlier 4,096-token probe remains historical evidence. The official HF template differs from the tested GGUF receiver template; substituting it is refused. [The bootstrap script](../scripts/bootstrap_tokenizers.py) pins both independently. Repeat the receiver conformance probe for a changed model/server/template rather than assuming equality.

The pinned receiver joins OpenAI text-content parts with newlines, retaining empty parts, and maps an assistant's null content with tool calls to an empty string. The offline renderer follows that parser behavior before applying the GGUF template, and checks exact public prompt vectors captured through `/apply-template` without inference. Non-text parts remain refused. Measure the complete coordinator and worker prompts with their actual tools and fixed acceptance contract before starting a local-model trial; overflow fails before spending.

The ten-worker reconciliation fixture requires **32,768 tokens per slot** (`--ctx-size 65536 --parallel 2`). Its complete coordinator context with Finding provenance already exceeds the earlier 16k declaration before the mandatory Task-status step. Retain that failed preflight and bind the new server/configuration to the trial. This changes the declared model slot before inference; the CP state/memory cap, provider process memory/CPU limits, seed, acceptance predicates and budgets remain fixed.

The actual-model fixture reserves 2,048 output tokens. Its complete final two-tool response, including all ten required rows and full Finding references, needs 908 tokens in compact JSON, 986 with ordinary spacing, and 1,091 when pretty printed. A 1,024-token bound could cut a valid formatted answer; retain that preflight rather than truncating the final candidate. The scripted profile keeps its original 1,024-token output bound.

Set `features.model`, the signed price model and `tokenizer.model` to the same admitted API alias (for example, `qwen3-4b` from an explicit llama.cpp `--alias`). The tokenizer revision and artifact digests independently bind the actual model assets; a Hugging Face repository name is not necessarily the API alias. An optional `provider.ca_pem` supplies a bounded public CA for a declared HTTPS receiver; proxy and redirect routing are disabled.

## Start the independent services

These commands run as the account that owns the named service. Use an operator-owned launcher or fixed service unit to start the evaluator under its own UID; the unprivileged CP cannot change UID to launch it. On the protected evaluator account:

```sh
/absolute/release/solos-swarm mvp evaluator-serve --bundle /absolute/evaluator/private/bundle.json --certificate /absolute/evaluator/public/svid.der --private-key /absolute/evaluator/private/key.der --ca /absolute/evaluator/public/ca.der --state /absolute/evaluator/private/state.json --ipc-socket /absolute/evaluator/ipc/evaluator.sock --broker-uid ACTUAL_CP_UID
```

On the witness account:

```sh
/absolute/release/solos-swarm mvp witness-serve --config /absolute/witness/witness.json
```

On the control-plane account, with its one-use credential source ready:

```sh
/absolute/release/solos-swarm mvp serve --config /absolute/controlplane/runtime.json
```

Use fixed service names, finite launch deadlines and `KillMode=control-group` in the operator's systemd units. A stopped external evaluator service retains its private state; the CP closes only its owned connection and delivery listener. A crashed evaluator dispatch is recorded unknown and is not silently rerun with new hidden cases.

Generated WASI requires an active systemd user manager for the evaluator's own UID. Its `/run/user/UID` directory must be owned by that UID with mode `0700`, and its `bus` must be an owned Unix socket. The local evaluator launcher clears ambient environment and derives only this verified user-bus context. Provision the same exact context in the external evaluator's service; use that account's bus. The candidate namespace receives no user-bus socket, corpus, key or ambient environment.

`runtime.evaluators` is a bounded registry of at most 64 Goal-scoped supervisor configurations. Each contains `process`, `delivery`, admitted `session`, `runtime_identity`, interval/request bounds, `candidate_format`, optional prepared `candidate_store` and `max_jobs_per_goal` (1–65,536, additionally bounded by the admitted budget and state ceiling). Its `process` contains the release executable, `files` paths, exact evaluator UID/GID, private IPC socket and frame/timeout ceilings. The CP reads only the public evaluator certificate and CA. The delivery origin is exactly `https://node-NODE`; the service appends `/evaluator/command` itself. The protected bundle independently pins this same origin and delivery socket.

The evaluator Session is an explicitly desired service allocation in its Goal's Swarm. It needs a compiled `tool_call` capability for operation `evaluate` and resource `evaluate/GOAL`, with the exact evidence clearance and finite lease. The runtime starts its broker only when that normal allocation is live, and skips Pi launch for that evaluator identity. Configuration alone cannot grant the evaluator authority. An evaluator module unload stops supervision and its owned broker; reload restores it only from live admitted authority.

Generated WASI is executed through the separate `wasi-worker` process under systemd CPU/memory/PID/deadline bounds, bubblewrap, strict Landlock and seccomp. Its compilation also occurs inside those bounds. The protected channel encodes module bytes as bounded base64; normal result delivery sends only the durable request ID and signed result, and resolves the original broker-signed request from retained state. Native execution requires operator-prepared Firecracker/KVM guest assets with a scoped broker identity, or the explicitly declared gVisor fallback on a host without KVM. No candidate gets hidden corpus, evaluator key or unrestricted network access.

The WASI host executor starts after filesystem/syscall sealing and enables only timers; its memory streams need no signal or network I/O driver. Failed launches retain bounded quarantined diagnostics in an immutable mode-`0600` `failure.json` inside the owned mode-`0700` job directory. Caller errors identify only its digest; private diagnostics remain available to the authorized operator.

## Recover an expired cold cluster

Normal running nodes renew their admitted cluster SVIDs before expiry and persist a private crash journal before quorum admission. If every node is offline past certificate expiry, stop every CP instance and use the explicit offline admission path. Run each request command as the account that owns that node's existing private store:

```sh
/absolute/release/solos-swarm mvp offline-startup-request --config /absolute/controlplane/runtime.json --output /absolute/operator/node-1-request.json
```

Collect one public request from every configured node into a JSON array. These requests contain inventory/membership/state hashes and certificate admissions, without Board bodies or keys. Obtain fresh CA-issued leaves for the exact existing cluster identities and install each matching private key in its node's owned identity files. Prepare `certificates.json` as a map from numeric node IDs to their fresh public DER paths. The signing account must have the rotation authority already pinned by this deployment; a new arbitrary signer is refused.

```sh
/absolute/release/solos-swarm mvp offline-startup-assemble --config /absolute/controlplane/runtime.json --requests /absolute/operator/requests.json --certificates /absolute/operator/certificates.json --epoch NEXT_MONOTONIC_EPOCH --expires-ms ABSOLUTE_UNIX_MILLISECONDS --output /absolute/operator/offline-admission.json
```

The expiry must be within five minutes. Place the same signed admission at each node's configured `cluster.offline_admission_file`, then start the nodes. Startup binds each request to its retained state, verifies the exact full inventory and recovered membership, and waits for quorum to commit the new anchors before consuming any provider credential or launching work. Partial inventories, stale state, replayed epochs, changed authorities or identity mismatches fail closed. After successful admission, a private applied marker routes subsequent starts through ordinary durable admission checks; it never grants authority itself.

## Authenticate a real operator

Request an authentication challenge over verified TLS:

```sh
curl --fail --silent --show-error --cacert /absolute/operator/server-ca.pem --request POST https://OPERATOR_HOST:PORT/operator/challenge
```

Complete the configured issuer's real login/MFA flow with that exact nonce. The verified token must have the expected audience, issuer, signature, allowed `acr`, required `amr`, fresh `auth_time`, `principal_type="human"` and the returned nonce. Supply the token through a private file/body rather than a visible shell argument:

```sh
curl --fail --silent --show-error --cacert /absolute/operator/server-ca.pem --header 'Content-Type: application/json' --data-binary @/absolute/operator/private/login.json https://OPERATOR_HOST:PORT/operator/session
```

`login.json` is `{ "nonce": "returned nonce", "token": "real issuer token" }`. Preserve the returned `session_id` privately with the token in the SDK session file `{session_id,token}`. The server consumes the nonce once. All later requests need `Authorization: Bearer TOKEN` and `x-swarm-session: SESSION_ID`; the QA SDK reads the private session file and handles these headers. A root display name, a workload certificate, a synthetic issuer or a string claiming MFA is not an operator session.

## Register the Goal and apply the ten-worker Swarm

Prepare the full immutable Goal contract before obtaining approvals. Its fields include versions of Goal/rubric/evaluation set/evaluator, evaluator SPIFFE identity, finite budget, depth, agent/open-task/Need caps and expiry. Include a protected final Need as well as the search Needs. Catalog predicates are `{"kind":"catalog-goal"}`, derived `{"kind":"catalog-task","task_id":"..."}`, and `{"kind":"catalog-final"}`. Only the independently signed held-out result may satisfy the final Need. Fixed artifacts receive a separate strict frozen-contract check on the same public case; executable candidates require disjoint final inputs.

Populate `GoalPosture.evaluator_public_key` from the **actual evaluator certificate**. Bind the entire contract before computing approvals:

```rust
bind_goal_contract(&mut posture, &spec)?;
spec.security = serde_json::to_value(&posture)?;
let registration_hash = posture.registration_hash()?;
```

`bind_goal_contract` checks Goal ID, intent and version and hashes the complete Goal with its nested `security` field normalized to null. Changing a predicate, evaluator key, rubric or version invalidates the old approval binding.

Submit `OperatorRequest {request_id, operation}` to `/operator/command` using the authenticated SDK or a private-header HTTP client. Commands are tagged by `command`, for example:

```json
{"request_id":"read-before-register","operation":{"command":"read_state","scope":{"scope":"global"}}}
```

Use `approve` to obtain server-signed exact-action human approvals. Then use `register_goal {spec,posture,approvals}`. Registration requires the applicable distinct human approvals; privileged role/binding/policy changes, Goal amendments and stop releases require two distinct privileged humans with fresh step-up. Do not create locally signed substitutes. An amendment uses the next Goal version and fresh exact-contract approvals, while retaining prior evidence, scores and liabilities.

Prepare `apply_swarm {spec}` with the approved Goal/provider/model, finite lease, placement strategy, expiry and ten worker records plus the protected evaluator service. Worker IDs, Tasks, resource reservations, capability sets, history policies and skill digests are explicit. `DesiredAgent.task` carries its narrowed `need_id`, budget, capabilities, input and expiry. Keep the evaluator service outside the ten productive Pi-worker count. A ten-worker run requires useful real inference and a live Finding contribution from each worker, not just ten processes or a large configured `max_agents`.

The placement transaction reserves the full gang's resources, budgets and allocation leases atomically. Each Pi worker claims only its admitted Task/incarnation/fence. Task delegation passes through the same authorization and reservation checks. A rejected or partially affordable gang launches no subset. Workers cannot widen grants, become evaluators or grant authority to generated code. An optional policy actor may receive a separate `tool_call` Capability with operation `audit_read` and exact resource `audit/GOAL`. That surface returns at most 32 Goal-scoped hash metadata entries with a global signed root and inclusion proofs, and requires its own current Task/fence and read clearance. It supplies no other Goal's raw events.

Admit policy interventions explicitly with a `policy_request` Capability, C0 confirmation and exact finite `scope`/`trigger` argument constraints. Operation `flag` on resource `policy/GOAL` enables `request_flag`; its typed evidence comprises `scope`, `trigger`, a nonzero 32-byte `reason_hash` and `confidence_milli` from 0 to 1,000. It records an audit flag without changing stops, grants or money. Operation `stop` on `stop/GOAL` enables `request_stop` under the same evidence validation and uses the separate control path. Neither operation accepts spend authority or permits a policy actor to release stops, refund costs or widen a Goal.

The model sees a bounded ordered Board, eligible Findings and versioned scored history. Attempts require a planning round created from that worker's successful, admitted inference. A round permits at most eight new candidates, including evaluated candidates. History keeps the full independent archive while supplying the bounded top-20 current-contract context; baseline campaigns use the latest single draft and `new_candidates=1` instead.

Pi executes broker tools in assistant order. An intermediate `submit_attempt` can continue search. For a finished coordinator, put `submit_attempt` followed by `complete_task` with its existing shard Finding reference in one assistant batch; the loop then settles without requesting another model turn. Held-out selection immediately closes new search and worker effects. An already owned Task may complete within five seconds of the immutable held-out request's issue time, bounded further by its original lease, Goal expiry and evaluator deadline. That completion may finish bookkeeping after the Goal is scored; it creates no new allocation or lease extension. Stops, retractions, fencing and module withdrawal remain effective during this drain.

Protected catalog generation is an operator/evaluator action. `mvp catalog` output includes frozen expected data and must stay in the evaluator's private directory. Worker input is the public case/task payload only. Never copy the whole generated catalog JSON into a worker prompt, Board, public fixture or run evidence.

## Observe, stop and resume

Workers with an explicitly registered `ToolCall` for operation `task_status` on `task/GOAL_ID` can read bounded, paginated current Task metadata through the `task_status` tool. The response omits Task inputs and bodies, filters transitive clearance, and records its provenance. Coordinators can use it to wait for peer completion before submitting a final candidate. The new `task_status` input type needs an explicit approved taint exemption if the Goal permits unattended inference over that metadata.

Authenticated `read_state` returns retained authoritative revision, domain revision, snapshot and signed head for privileged root viewers. Scoped viewers receive filtered state. Reads do not replay effects or advance the authoritative revision. Save the packet as a unit; a standalone JSON snapshot does not prove an authoritative readback revision.

Use `engage_stop {scope,reason}` for a Goal, agent, tenant or global stop. New effects are blocked, worker quotas and owned child processes are cancelled, and pending accounting liabilities remain. Repeating the stop is idempotent. `release_stop {scope,approvals}` requires the exact current stop binding and two privileged humans. Resume preserves versions, immutable attempts, audit history and liabilities; expired Tasks or fenced incarnations do not become usable again.

A retraction appends evidence to the retained archive, invalidates its descendants, reopens affected Needs/Attempts and blocks fresh context construction/readback from serving retired raw bodies. The archive remains inspectable to an authorized auditor. Live cache recovery rechecks current allocation/Task/grant authority, transitive markings and retracted ancestors; an old successful RPC ID does not authorize serving a retired body. A model answer or retraction cannot retroactively erase a dispatched call's receipt or unknown liability.

The broker records actual provider/target flow attribution (Goal, Task, holder, destination, operation, resource, byte counts and times). `RuntimeConfig.egress` sets finite observation limits; defaults retain a 60-second window, 10,000 records and 1,000 flags, with fan-in/fan-out thresholds of 16. Noncontact dispatch attempts do not count as observed contacts. Expected shared provider traffic is exempt from anomaly flags. A declared `audit_read` capability returns a Goal-scoped signed flow view with audit inclusion proofs; detection supplies evidence for scoped policy intervention and does not replace deterministic egress restrictions.

## Verify evidence and run campaigns

For a saved root readback packet, export `snapshot.json`, `head.json` and the trusted CP public key without altering the original files. Verify both the signed Merkle root and the checkpoint that binds the entire retained state:

```sh
/absolute/release/solos-swarm mvp verify --snapshot /absolute/evidence/snapshot.json --head /absolute/evidence/head.json --trusted-key /absolute/evidence/cp-public-key.json --output /absolute/evidence/verification.json
/absolute/release/solos-swarm mvp inspect --snapshot /absolute/evidence/snapshot.json --kind board --id GOAL_ID
/absolute/release/solos-swarm mvp inspect --snapshot /absolute/evidence/snapshot.json --kind attempt --id GOAL_ID/ATTEMPT_ID
```

Inspection is read-only and reports the authoritative revision as unknown for a saved snapshot unless the original readback packet is retained. Verification never dispatches tools or repairs evidence. Deliberately corrupted copies, wrong receipts, missing audit/checkpoint evidence and empty required selection must not pass; originals remain untouched.

`QaConfig` supplies an existing real operator session, admitted agent/controller SVIDs, independent receiver probes, finite recipe actions and a separate evidence-encryption key. Set the finite `max_response_bytes` explicitly for the planned workload (1 KiB–256 MiB). Large readback observations are retained as content-addressed encrypted pieces with a signed reconstruction manifest; the configured ceiling is still enforced. Required recipes are executed through normal APIs, with owned cleanup even on failure:

```sh
/absolute/release/solos-swarm mvp qa --config /absolute/operator/qa.json --profile integration --output /absolute/evidence/qa-run
```

The profile is selection policy, not a fabricated fixture. Unknown/shared change impact broadens the required set. Missing recipes or independent receiver observations produce `INCONCLUSIVE`, not a skipped PASS. Receiver counters must come from the actual effect destination, with a positive successful-effect control; an empty CP event list does not prove that denied traffic never arrived upstream.

For the matched 1-vs-10 and scored-history-vs-single-draft campaign, use [CAMPAIGNS.md](CAMPAIGNS.md). Freeze at least two case/seed groups across all four cells before execution, with equal budgets, exact build/config/model/tokenizer/scorer keys and versions. Configure the per-Goal evaluator registry and genuine approvals before trials. The driver verifies the currently running binary/config against its signed runtime packet and requires each trial's independent final signed verdict:

```sh
/absolute/release/solos-swarm mvp campaign --config /absolute/operator/campaign.json --output /absolute/evidence/campaign-run
```

All attempts, failures, receipts, unknown outcomes, seeds, cost/token measurements and stop/cleanup observations are retained. The driver does not stop after a favorable cell or reroll a failure. Runtime software measurement is not hardware attestation. A changed build, missing current measurement, scorer-key mismatch, cost mismatch or incomplete cleanup cannot establish a passing campaign.

## Qualify the exact release artifact

`mvp verify` checks saved evidence. Release qualification separately binds the exact public bundle, current independent report, protected profile and independently signed release attestation:

```sh
python3 scripts/qualify.py --bundle /absolute/release/solos-release.tar --report /absolute/evidence/report.json --profile /absolute/verifier/private/profile.json --attestation /absolute/verifier/public/release-attestation.json --trusted-key /absolute/verifier/public/public-key.json --verifier /absolute/verifier/solos-swarm
```

Use the actual flags shown by `python3 scripts/qualify.py --help`; promotion is an explicit separate `--promote` destination. The protected profile pins `verifier_sha256`, so supplying a replacement verifier executable cannot establish release eligibility. It also freezes the allowed providers, limits/price-policy digests, catalog/verifier versions, isolation profile, age and every mandatory scenario. Empty, skipped, failed or missing checks cannot pass.

The release report must name exactly ten distinct live `participating_agent_ids`; the independent Goal checker's `useful_agent_ids` must equal that set. Ten configured identities, an idle policy actor or an evaluator service cannot substitute for useful worker contributions. Runtime, Goal, evidence and cleanup verdicts must each pass, all held liability must be zero, and provider/resource shutdown observations must be available.

For `min_nodes > 1`, the profile's protected `independent_node_inventory` binds node IDs to distinct approved physical hardware identities. Reported `node_hardware_identities` must match that inventory and be distinct. Extra replica labels or additional processes on `remote-ai` cannot satisfy this gate. A single-host profile retains its declared single-host scope and does not qualify independent failure domains.

## Current qualification boundary

The original ten-agent DeepSeek preview is recorded in [POC-RESULTS.md](POC-RESULTS.md). Portable domain, context, evaluator, accounting, authenticated receiver and QA tests establish their named functional contracts. The actual local Qwen3-4B/llama.cpp receiver matched complete prompt bytes, token IDs and usage receipts for four pinned conformance cases; it is a CPU test and supplies no GPU-throughput qualification.

A cold-checkout production MVP run still requires the user's real OIDC issuer/MFA subjects, operator-provisioned protected evaluator and witness ownership, signed deployment/price/target policy and current approved runtime configuration. The connected production campaign and independent-host/numeric acceptance remain unrun until those real inputs and authorized machines exist. Keep those statuses pending in the evidence rather than weakening or replacing the TRD gates.
