#!/usr/bin/env bash
# Usage: cleanup.sh <run> [--purge-src]
# Stops the agent this run started (by its pid file, never by name) and removes its socket and any unread key file,
# so launch.sh can start the run again. --purge-src also removes the shipped source tree (the Pi tree included),
# for the end of a pass. Always keeps the evidence, the state dir, the secrets dir, serve.log and the binary.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
run=$1
purge=${2:-}
check_run "$run"
box "bash -s" <<EOF
set -u
cd ~/work/verify/$run 2>/dev/null || { echo "no run $run"; exit 0; }
if p=\$(cat serve.pid 2>/dev/null) && kill -0 "\$p" 2>/dev/null; then
  kill -TERM "\$p"
  for _ in \$(seq 1 30); do kill -0 "\$p" 2>/dev/null || break; sleep 1; done
  kill -0 "\$p" 2>/dev/null && { echo "pid \$p ignored TERM; sending KILL"; kill -KILL "\$p"; }
  echo "stopped agent serve pid \$p"
fi
if p=\$(cat probe.pid 2>/dev/null) && kill -0 "\$p" 2>/dev/null; then kill -TERM "\$p"; echo "stopped the key probe server pid \$p"; fi
rm -f serve.pid probe.pid /run/user/\$(id -u)/verify-$run-key state/broker.sock
if [ "$purge" = --purge-src ]; then rm -rf src; echo "removed the source tree"; fi
echo "kept: ~/work/verify/$run/{evidence,state,secrets,serve.log,bin}"
EOF
