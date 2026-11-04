#!/usr/bin/env bash
# Usage: doctor.sh <run>
# Read-only: is this run's agent worth driving? Exits 1 on the first failed check.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
run=$1
check_run "$run"
box "bash -s" <<EOF
set -u
cd ~/work/verify/$run 2>/dev/null || { echo "FAIL no run directory ~/work/verify/$run"; exit 1; }
fail() { echo "FAIL \$*"; exit 1; }
p=\$(cat serve.pid 2>/dev/null) || fail "no serve.pid: was the run launched?"
kill -0 "\$p" 2>/dev/null || fail "agent serve (pid \$p) is not running; serve.log ends: \$(tail -3 serve.log)"
echo "ok   agent serve pid \$p, revision \$(cat bin/REVISION)"
test -S state/broker.sock || fail "no broker socket"
echo "ok   broker socket"
test -e state/mcp/inbox.jsonl || fail "no MCP inbox: is the mcp source in agent.yaml?"
echo "ok   MCP inbox"
test ! -e /run/user/\$(id -u)/verify-$run-key || fail "the key file was not consumed"
echo "ok   key file consumed"
status=\$(bin/solos-swarm agent status --state-dir state --config agent.yaml --json) || fail "agent status failed"
python3 -c 'import json,sys; s=json.loads(sys.argv[1]); r=s.get("router") or {}; print("ok   agent status: %d open tickets, router committed today %s of cap %s µUSD" % (len(s.get("tickets",[])), r.get("committed_today_microusd"), r.get("daily_cap_microusd")))' "\$status"
if grep -qiE 'panic|error' serve.log; then echo "warn serve.log has errors:"; grep -iE 'panic|error' serve.log | tail -3; fi
EOF
