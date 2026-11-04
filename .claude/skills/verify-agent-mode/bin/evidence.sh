#!/usr/bin/env bash
# Usage: evidence.sh <run> <label>
# Snapshots what the run's agent recorded, read-only, into ~/work/verify/<run>/evidence/<label>/ on the box, then
# copies it to $VERIFY_LOCAL/<run>/<label>/ on this Mac (default ~/.cache/solos-swarm-verify). Cleanup keeps both.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
run=$1
label=$2
check_run "$run"
check_run "$label"
box "bash -s" <<EOF
set -eu
cd ~/work/verify/$run
out=evidence/$label
mkdir -p \$out
bin/solos-swarm agent status --state-dir state --config agent.yaml --json > \$out/status.json
python3 - state/agent.db \$out <<'PY'
import json, sqlite3, sys
db, out = sys.argv[1], sys.argv[2]
c = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
for table, query in {
    "events": "select seq, body from events order by seq",
    "effects": "select id, lane, body, done, attempts, error from effects order by id",
    "spend": "select account, state, hold_microusd, cost_microusd, input_tokens, cached_input_tokens, output_tokens from spend order by id",
    "tickets": "select id, body from tickets",
}.items():
    with open(f"{out}/{table}.jsonl", "w") as f:
        for row in c.execute(query):
            f.write(json.dumps(row) + "\n")
PY
cp -f state/mcp/outbox.jsonl \$out/replies.jsonl 2>/dev/null || true
if grep -q '^browser:' agent.yaml; then
  bin/solos-swarm agent trail --state-dir state --config agent.yaml > \$out/trail.txt 2>&1; echo "exit \$?" >> \$out/trail.txt
  bin/solos-swarm agent trail --state-dir state --config agent.yaml --json > \$out/trail.json 2>/dev/null || true
fi
cp -f serve.log bin/REVISION \$out/
ls state/tickets/*/ 2>/dev/null > \$out/ticket-files.txt || true
echo "evidence on the box: ~/work/verify/$run/\$out"
EOF
mkdir -p "$VERIFY_LOCAL/$run"
box "tar -cf - -C ~/work/verify/$run/evidence $label" | tar -xf - -C "$VERIFY_LOCAL/$run"
echo "evidence on this Mac: $VERIFY_LOCAL/$run/$label"
