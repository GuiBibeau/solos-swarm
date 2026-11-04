#!/usr/bin/env bash
# Usage: mcp.sh <run> request "<text>" [<thread>]
#        mcp.sh <run> replies <thread> [<kind> <timeout_s>]
#        mcp.sh <run> status
# Talks to the run's agent as the person mcp:verifier through `solos-swarm mcp --operator`, the same stdio MCP
# server Claude Code or Codex would start. `replies` with a kind (ack, attention, result) waits for it.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
run=$1
shift
check_run "$run"
args=$(printf ' %q' "$run" "$@")
box "python3 -${args}" < "$SKILL_DIR/bin/mcp_client.py"
