#!/usr/bin/env python3
"""One-line tournament status for the controller cron. Compatible with scripts/swarm_progress.py's counts.
Usage on the Mac:  python3 status_line.py [run_id]   (default: the newest tourney-20261028-* run)"""
import json, subprocess, sys
rid = sys.argv[1] if len(sys.argv) > 1 else None
cmd = ("S=$HOME/.local/state/solos-swarm/runs; R=" + (rid or "$(ls -t $S | grep ^tourney-20261028- | head -1)") +
       "; [ -n \"$R\" ] && python3 $(ls -dt $HOME/.local/share/solos-swarm/*/ | head -1)scripts/swarm_progress.py --run-id $R --json; "
       "echo; echo MARKERS; ls ~/research/regimes/tournament-data 2>/dev/null | tr '\\n' ' '; tail -1 ~/research/regimes/tournament-data/gate.log 2>/dev/null")
out = subprocess.run(["ssh", "-o", "BatchMode=yes", "remote-ai", cmd], capture_output=True, text=True).stdout
try:
    j = json.loads(out.split("\nMARKERS")[0]); t = j.get("totals", j)
    print(json.dumps({k: t.get(k) for k in ("goal_status", "agents", "running", "filed", "spent_usd", "cap_usd", "held_usd") if k in t} or j)[:400])
except Exception:
    print(out.strip()[-400:])
