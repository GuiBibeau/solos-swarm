# Campaigns

Matched 2x2 experimental design for comparing worker parallelism and planner context policies.


## 1. 2x2 matrix

Each trial seed runs four cells under identical budgets, token limits, and model parameters:

| Cell | `workers` | `history_mode` | Context policy |
|---|---:|---|---|
| `C1` | `1` | `scored_history` | Top-K scored attempts + retractions |
| `C2` | `1` | `single_draft_refinement` | Best single draft + retractions |
| `C3` | `10` | `scored_history` | Shared board + top-K scored attempts |
| `C4` | `10` | `single_draft_refinement` | Shared board + best single draft |


## 2. Invariants

1. **Frozen trial catalog**: Seeds, caps, and model parameters lock before trial `0`.
2. **Matched cell envelopes**: Every cell gets identical `budget_microusd`, `max_calls`, `max_input_tokens`, `max_output_tokens`, and `deadline_ms`.
3. **Holdout isolation**: Iterative proposals score on `search`; `evaluator-serve` scores `final_holdout` once at cell completion.
4. **Goal-scoped IPC**: Evaluator socket binds per `goal_id` under UID `995` (`0710` dir, `0660` socket).
5. **Pre-dispatch token gate**: Contexts exceeding `max_input_tokens` under `VerifiedTokenizer` fail closed before spend.


## 3. Command

```bash
./target/release/solos-swarm mvp campaign \
  --config /path/to/campaign.json \
  --output /path/to/campaign-report.json
```


## 4. `CampaignConfig`

```json
{
  "id": "campaign-reconcile-01",
  "seed": 20260412,
  "family": "synthetic_reconciliation",
  "trials": 5,
  "model": "deepseek-chat",
  "temperature_millis": 0,
  "budget_microusd": 1000000,
  "max_calls": 20,
  "max_input_tokens": 8192,
  "max_output_tokens": 2048,
  "deadline_ms": 600000,
  "evaluator_endpoint": "https://127.0.0.1:9445",
  "evaluator_socket_root": "/var/lib/solos-swarm/evaluator/ipc",
  "evaluator_identity": {
    "certificate_path": "/var/lib/solos-swarm/cp/cp.der",
    "private_key_path": "/var/lib/solos-swarm/cp/cp.key.der",
    "ca_certificate_path": "/var/lib/solos-swarm/shared/ca.der",
    "expected_spiffe_id": "spiffe://solos.local/ns/prod/sa/evaluator-01"
  },
  "provider": {
    "id": "deepseek",
    "kind": "openai_compatible",
    "endpoint": "https://api.deepseek.com/v1/chat/completions",
    "model": "deepseek-chat",
    "credential": { "kind": "file", "path": "/run/credentials/deepseek.key" },
    "input_price_microusd_per_million": 300000,
    "output_price_microusd_per_million": 1200000,
    "max_output_tokens": 2048,
    "tokenizer": {
      "format": "huggingface_json",
      "path": "work/tokenizers/deepseek-v4.1-flash/tokenizer.json",
      "sha256": "c2fd98e9fc2bd49fd7fcce7fb8fd2ca6fb9fa065ff4dc124e2d8d594bf9610b7"
    }
  }
}
```


## 5. Scenario families (`CatalogFamily`)

| Family | Task |
|---|---|
| `synthetic_reconciliation` | 10-shard record mismatch synthesis |
| `wasm_transform` | WASI binary synthesis under fuel caps |
| `policy_repair` | ABAC and capability constraint repair |
| `log_triage` | Multi-shard incident lineage correlation |


## 6. Output fields (`CampaignReport`)

| Field | Meaning |
|---|---|
| `trials[].cells[].workers` | `1` or `10` |
| `trials[].cells[].history_mode` | `scored_history` or `single_draft_refinement` |
| `trials[].cells[].search_attempts` | Candidate evaluations on `search` split |
| `trials[].cells[].best_search_score_millis` | Peak `search` score (`0..1000`) |
| `trials[].cells[].final_holdout_score_millis` | Signed `final_holdout` score (`0..1000`) |
| `trials[].cells[].settled_microusd` | Total settled cell spend |
| `trials[].cells[].elapsed_ms` | Cell wall-clock runtime |
| `summary` | Win/tie/loss counts and median scores |
