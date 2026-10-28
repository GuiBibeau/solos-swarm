#!/usr/bin/env bash
# Mac-side helper: reach the dashboard that runs on the box (loopback only) through an SSH tunnel.
#   scripts/dashboard-tunnel.sh [--host remote-ai] [--port 4173] [--start RUN_ID] [--no-open] [--dry-run]
# The server checks the Host header against its own port, so the local and remote port must be the same.
# --start RUN_ID first runs `systemctl --user start solos-dashboard@RUN_ID` on the box (template unit, see
# scripts/systemd/); without it the unit must already be running. Nothing else on the box is touched.
set -euo pipefail
host=remote-ai port=4173 start="" open=1 dry=0
while [ $# -gt 0 ]; do
  case "$1" in
    --host) host="${2:?--host needs a value}"; shift 2 ;;
    --port) port="${2:?--port needs a value}"; shift 2 ;;
    --start) start="${2:?--start needs a run id}"; shift 2 ;;
    --no-open) open=0; shift ;;
    --dry-run) dry=1; shift ;;
    -h|--help) sed -n 2,7p "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ "$port" =~ ^[0-9]+$ ]] && [ "$port" -ge 1024 ] && [ "$port" -le 65535 ] || { echo "invalid port: $port" >&2; exit 2; }
[[ "$host" =~ ^[A-Za-z0-9._@-]+$ ]] || { echo "invalid host: $host" >&2; exit 2; }
[ -z "$start" ] || [[ "$start" =~ ^[a-z0-9][a-z0-9-]{0,47}$ ]] || { echo "invalid run id: $start" >&2; exit 2; }
url="http://127.0.0.1:$port/?source=runtime"
tunnel=(ssh -N -L "$port:127.0.0.1:$port" -o ExitOnForwardFailure=yes "$host")
if [ "$dry" = 1 ]; then
  [ -z "$start" ] || echo "ssh $host systemctl --user start solos-dashboard@$start"
  echo "${tunnel[*]}"; echo "open $url"; exit 0
fi
[ -z "$start" ] || ssh -o BatchMode=yes "$host" "systemctl --user start solos-dashboard@$start"
if [ "$open" = 1 ]; then
  (sleep 2; { command -v open >/dev/null && open "$url"; } || { command -v xdg-open >/dev/null && xdg-open "$url"; } || echo "open $url") &
fi
echo "tunnel up: $url (Ctrl-C to close)"
exec "${tunnel[@]}"
