#!/bin/bash
# After build_dataset.py has finished into $BUILD. Two read-only views, hardlinked (no extra disk):
#   views/workers  = manifest + dictionary + discovery/            (months <= 2026-04)
#   views/referees = the same + confirm/ + raw_check/ (3 symbols x 2 days of raw aggregate trades)
set -euo pipefail
BUILD=${1:-$HOME/research/regimes/tournament-data/build}; V=$(dirname "$BUILD")/views
HERE=$(cd "$(dirname "$0")" && pwd); rm -rf "$V"; mkdir -p "$V/workers" "$V/referees/raw_check"
for r in workers referees; do cp -al "$BUILD/discovery" "$V/$r/discovery"; cp "$BUILD/manifest.json" "$HERE/DATA_DICTIONARY.md" "$V/$r/"; done
cp -al "$BUILD/confirm" "$V/referees/confirm"
A=$HOME/.local/share/solos-data/augment/tables/binance/aggTrades
for s in BTCUSDT SOLUSDT WIFUSDT; do mkdir -p "$V/referees/raw_check/$s"; for d in 2026-03-10 2026-04-20; do cp "$A/$s/$d.parquet" "$V/referees/raw_check/$s/"; done; done
test ! -e "$V/workers/confirm"; du -sh "$V"/*; echo views ok
