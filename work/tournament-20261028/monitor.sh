#!/bin/bash
# Runs ON THE BOX from a systemd user timer every 15 min. Appends one JSON line: run progress, transfer, load, disk.
# Install (not done): see README "Monitoring".
S=$HOME/.local/state/solos-swarm; OUT=$S/tournament-monitor.jsonl; mkdir -p "$S"
RUN=$(ls -t "$S/runs" | grep '^tourney-20261028-' | head -1)
PROG=null
[ -n "$RUN" ] && PROG=$(python3 "$(ls -dt $HOME/.local/share/solos-swarm/*/ | head -1)scripts/swarm_progress.py" --run-id "$RUN" --json 2>/dev/null || echo null)
G=$(cat $HOME/.local/state/transfer-guard/status.json 2>/dev/null || echo null)
printf '{"at":"%s","run":"%s","progress":%s,"guard":%s,"load":"%s","disk_free_gb":%s}\n' "$(date -u +%FT%TZ)" "$RUN" "${PROG:-null}" "$G" "$(cut -d' ' -f1-3 /proc/loadavg)" "$(df -BG --output=avail $HOME | tail -1 | tr -dc 0-9)" >> "$OUT"
echo "$G" | grep -q '"level":"ok"' || echo "$(date -u +%FT%TZ) transfer guard not ok: $G" >> "$S/tournament-alerts.log"
