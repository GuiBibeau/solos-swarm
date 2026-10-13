# solos-swarm

Rust control plane and broker for a swarm of Pi agents. The MVP implements the [TRD](docs/TRD.md) domain and security contracts: Goal, Need, Task, ordered Board, Capability, Finding and Attempt; Raft over mTLS QUIC; atomic placement and delegation; bounded provider spending; protected evaluation; and read-only evidence verification.

Workers use signed workload identities and receive no provider key. Hosted DeepSeek and a pinned local Qwen/llama.cpp server use the same admission, receipt and quota path. Generated WASI runs in a bounded separate process. Native tools use Firecracker with a scoped broker identity; gVisor is an explicit fallback only on hosts without KVM. Findings, artifacts, input markings and retractions remain part of the recorded lineage. The Board has append, bounded read and cursor APIs; operator state readback and `mvp inspect --kind board` expose its recorded entries.

Use [the MVP operator runbook](docs/MVP-RUNBOOK.md) for cold-checkout provisioning and startup, [MVP implementation and qualification](docs/MVP-TRACEABILITY.md) for all requirement mappings and remaining acceptance evidence, and [matched evaluation campaigns](docs/CAMPAIGNS.md) for the normal authenticated campaign driver. Runtime startup consumes an operator-prepared configuration, approved policy, private identity files and verified dependencies. It does not grant itself operator approval.

```sh
cargo build --locked --release
solos-swarm mvp serve --config /absolute/path/runtime.json
solos-swarm mvp witness-serve --config /absolute/path/witness.json
solos-swarm mvp qa --config /absolute/path/qa.json --profile integration --output /absolute/path/evidence
solos-swarm mvp campaign --config /absolute/path/campaign.json --output /absolute/path/campaign
```

The authorized test host is currently `remote-ai`. The earlier ten-agent DeepSeek preview passed using its frozen binary; it does not qualify a rebuilt MVP binary. Independent worker hosts/evaluator failure domains and the unchanged 500-agent, air-gap, production identity and performance acceptance gates remain separately recorded as pending. Missing evidence returns `INCONCLUSIVE`, and failed scenarios stay failed.

## Run the first preview

Local requirements: Python 3.11+, Git, OpenSSH, an existing host-key-verified `remote-ai` SSH alias, and a `deepseek` API-key entry in `~/.pi/agent/auth.json`. The host needs Rust/Cargo, Python 3, bubblewrap with unprivileged user namespaces, a working user systemd manager with lingering enabled, and the existing pinned Node install at `~/.local/opt/node-v24.18.0-linux-x64`. These commands change only solos-swarm-owned directories and units.

```sh
python3 scripts/remote.py deploy
```

Copy the returned `candidate_id` into the start command. Deployment copies source, runs `cargo build --release --locked` and `npm ci --ignore-scripts`, and freezes the binary, pinned Pi 1.1.0 dependencies, Node 24.18.0, source revision, and file digests in `candidate.json`. Repeating deployment of the same source returns the same artifact. Run startup verifies its manifest and performs no build or dependency download.

```sh
python3 scripts/remote.py start --candidate CANDIDATE_ID --run-id poc-20261012-01 --seed 20261012
python3 scripts/remote.py status --run-id poc-20261012-01
python3 scripts/remote.py collect --run-id poc-20261012-01 --output work/evidence/poc-20261012-01
```

The case and seed are frozen before inference. The Goal caps all ten agents at a total of $1, eight calls each, 2,048 output tokens per call, and ten minutes. Provider calls and spending pass through the broker. The API key travels over SSH stdin, is written to a private one-use file outside the run and candidate, and is removed when the broker reads it or when the service stops. It is absent from the worker environment, public config, and evidence. The default auth path can be changed with `--auth-file PATH`.

Start returns immediately. A detached `systemd --user` service survives an SSH disconnect. `RuntimeMaxSec`, the runtime deadline, `KillMode=control-group`, and bubblewrap's parent-death handling bound the run when the controller is unavailable. Repeating `start` with the same run ID and candidate/seed returns the existing run; changing its identity is refused. Failed runs are retained and never silently rerolled.

After receiving a complete start request, the host controller ignores SSH hangup until the manager handoff finishes. A forced `SIGKILL` or host crash during that handoff can still leave a private credential file under `~/.local/state/solos-swarm/credentials/`; recovery requires the trusted operator to confirm the matching service is stopped and remove that run's file. This setup-crash recovery is outside the first passing grade.

Collect after the service stops. Read `summary.md` and `report.json` in the retrieved evidence. A passing report requires successful runtime execution, the ten-agent Goal, exact candidate/case digests, and confirmed worker cleanup. `controller-status.json` also records systemd state and `cleanup_confirmed`. Missing or altered evidence, nine idle agents, a failed artifact, or unknown cleanup cannot qualify. Only the exact artifacts that passed are qualified, for this single-host preview.

## Stop and inspect

```sh
python3 scripts/remote.py cancel --run-id poc-20261012-01
python3 scripts/remote.py status --run-id poc-20261012-01
```

Cancellation stops only `solos-swarm-poc-20261012-01.service`; repeating cancellation after it has stopped returns its recorded status. Host cleanup removes the one-use credential, runs the independent checker, and preserves evidence. Run artifacts remain at `~/.local/state/solos-swarm/runs/RUN_ID`; protected scorer expectations remain separately under `~/.local/state/solos-swarm/evaluators/RUN_ID`; immutable candidates live under `~/.local/share/solos-swarm/CANDIDATE_ID`. Retention is manual for this preview.

Lifecycle probes use the same candidate and isolated Pi workers but make no paid model requests. They are explicitly marked non-live and cannot pass the live checker. Use distinct IDs while keeping the same seed; the first command demonstrates host-side deadline expiry, and the second pair demonstrates cancellation.

```sh
python3 scripts/remote.py start --candidate CANDIDATE_ID --run-id deadline-probe-01 --seed 20261012 --idle-probe --deadline-seconds 10
python3 scripts/remote.py start --candidate CANDIDATE_ID --run-id cancel-probe-01 --seed 20261012 --idle-probe --deadline-seconds 120
python3 scripts/remote.py cancel --run-id cancel-probe-01
```

## Local checks

```sh
python3 scripts/bootstrap_tokenizers.py --output work/tokenizers
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
python3 -m unittest discover -s tests -p 'test_*.py'
npm ci --ignore-scripts --no-audit --no-fund
npm run test:broker
```

The Python checks exercise scoring failures, altered or missing evidence, candidate integrity, and idempotent remote identity without live inference. The Pi extension checks run a real RPC subprocess against a fixture Unix-socket broker.

The legacy preview commands above retain their original behavior and evidence. The expanded runtime uses `mvp serve`, inherited file-descriptor or one-use peer-checked socket credentials, separate evaluator/witness services, and `scripts/bundle.py` for verified offline artifacts. Its implementation and qualification status are recorded separately in [MVP-TRACEABILITY.md](docs/MVP-TRACEABILITY.md); the historical preview cannot establish those new contracts.
