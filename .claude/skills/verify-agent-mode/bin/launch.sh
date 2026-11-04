#!/usr/bin/env bash
# Usage: launch.sh <run> [overlay.yaml...]
# Starts `agent serve` for <run> on the box with a real DeepSeek Router and Pi workers, from an empty state dir.
# The config is the skill's agent.verify.yaml with each overlay appended (an overlay adds top-level sections such
# as vault: or browser:; relative paths in it resolve against ~/work/verify/<run>/). The DeepSeek key goes from this
# Mac's ~/.pi/agent/auth.json over ssh stdin into a 0600 file on the box's tmpfs, which serve reads once and deletes.
# Ready means: the process is up, the broker socket and the MCP inbox exist, and the key file is gone.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
run=$1
shift
check_run "$run"
config=$(mktemp)
trap 'rm -f "$config"' EXIT
cat "$SKILL_DIR/agent.verify.yaml" "$@" > "$config"
hours=${VERIFY_HOURS:-2}
box "test -x ~/work/verify/$run/bin/solos-swarm" || { echo "no binary for $run: run ship.sh first" >&2; exit 1; }
if box "test -f ~/work/verify/$run/serve.pid && kill -0 \$(cat ~/work/verify/$run/serve.pid) 2>/dev/null"; then
	echo "an agent already serves run $run; run cleanup.sh $run first" >&2
	exit 1
fi
box "rm -rf ~/work/verify/$run/state; cat > ~/work/verify/$run/agent.yaml" < "$config"
box "set -e; cd ~/work/verify/$run; mkdir -p secrets; chmod 700 secrets
	if grep -q '^vault:' agent.yaml && ! test -e secrets/vault.enc; then bin/solos-swarm vault init --config agent.yaml; fi
	if grep -q '^browser:' agent.yaml && ! test -e secrets/audit.key; then bin/solos-swarm browser init-key --out secrets/audit.key; fi"
python3 -I -c 'import json,os,sys; sys.stdout.write(json.load(open(os.path.expanduser("~/.pi/agent/auth.json")))["deepseek"]["key"].strip())' |
	box "umask 077; cat > /run/user/\$(id -u)/verify-$run-key"
box "cd ~/work/verify/$run && setsid -f bash -c 'echo \$\$ > serve.pid; exec timeout ${hours}h bin/solos-swarm agent serve --config agent.yaml --state-dir state --pi-root src --node \$HOME/.local/bin/node --credential-file /run/user/\$(id -u)/verify-$run-key > serve.log 2>&1 < /dev/null' > /dev/null 2>&1 < /dev/null"
for _ in $(seq 1 60); do
	state=$(box "cd ~/work/verify/$run; p=\$(cat serve.pid 2>/dev/null); if ! kill -0 \$p 2>/dev/null; then echo dead; elif test -S state/broker.sock && test -e state/mcp/inbox.jsonl && ! test -e /run/user/\$(id -u)/verify-$run-key; then echo ready; else echo starting; fi")
	case $state in
	ready) echo "run $run ready (pid $(box "cat ~/work/verify/$run/serve.pid"), stops itself after ${hours}h)"; exit 0 ;;
	dead) echo "agent serve exited during start:" >&2; box "tail -20 ~/work/verify/$run/serve.log" >&2; box "rm -f /run/user/\$(id -u)/verify-$run-key"; exit 1 ;;
	esac
	sleep 2
done
echo "run $run not ready after 120 s; see serve.log" >&2
exit 1
