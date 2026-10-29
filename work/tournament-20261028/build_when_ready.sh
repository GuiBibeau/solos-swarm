#!/bin/bash
# Box, hourly via timer (and safe to re-run). Builds the derived dataset only after the Binance phase-1 pull has
# finished and while the transfer guard is not paused. Limits: 2 processes, nice 19, 8 GB cgroup cap, no network
# (except the tiny fetch_funding step, which respects the guard itself).
set -u
R=$HOME/research/regimes; T=$R/tournament-20261028; D=$R/tournament-data; B=$D/build
PY=$R/fm-repo/.venv/bin/python; LOG=$D/build.log; mkdir -p "$D"
exec 9>"$D/.build.lock"; flock -n 9 || { echo "$(date -u +%FT%TZ) build already running" >> "$LOG"; exit 0; }
[ -f "$B/BUILD_OK" ] && exit 0
P=$HOME/.local/share/solos-data/free/binance/pull.log
tail -1 "$P" | grep -q " done futures" || { echo "$(date -u +%FT%TZ) wait: pull not finished: $(tail -1 "$P" | cut -c1-90)" >> "$LOG"; exit 0; }
# pull done means the download is not paused; the build itself reads local files only.
echo "$(date -u +%FT%TZ) build start" >> "$LOG"
cd "$T" || exit 1
# tiny network step; its own guard pause (PAUSE_GB default 600) must not stall the build, so cap it at 5 min
timeout 300 $PY fetch_funding.py >> "$LOG" 2>&1 || echo "funding history NOT fetched; after the guard resets run: fetch_funding.py then build_dataset.py --out $B --only funding" >> "$LOG"
for i in 0 1; do
  systemd-run --user --wait --quiet -p MemoryMax=4G -p CPUQuota=100% -p Nice=19 -p IOSchedulingClass=idle \
    $PY build_dataset.py --out "$B" --part $i/2 >> "$LOG" 2>&1 &
done
wait
$PY build_dataset.py --out "$B" --manifest-only >> "$LOG" 2>&1
./assemble_views.sh "$B" >> "$LOG" 2>&1 && $PY verify_build.py "$B" >> "$LOG" 2>&1
echo "$(date -u +%FT%TZ) build end: $([ -f $B/BUILD_OK ] && echo OK || echo FAILED)" >> "$LOG"
