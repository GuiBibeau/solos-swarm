#!/usr/bin/env python3
"""Sanity check of the built dataset; writes BUILD_OK (with a checksum of the file inventory) only if it passes.
Usage: verify_build.py BUILD_DIR   (box, fm-repo venv)"""
import calendar, hashlib, json, sys
from pathlib import Path
import pyarrow.parquet as pq
sys.path.insert(0, str(Path(__file__).resolve().parent))
from build_dataset import END_MS, LOCK_MS, SYMBOLS

b = Path(sys.argv[1]); errs = []
man = json.loads((b / "manifest.json").read_text())
if len(man["symbols"]) < 45: errs.append(f"only {len(man['symbols'])} symbols")
inv, rows = hashlib.sha256(), 0
for f in sorted(b.glob("*/bars_1m/*/*.parquet")):
    m = pq.ParquetFile(f); n = m.metadata.num_rows; rows += n
    y, mo = map(int, f.stem.split("-")); full = calendar.monthrange(y, mo)[1] * 1440
    if n > full: errs.append(f"{f.name} rows {n} > {full}")
    ts = m.read(columns=["ts_ms"]).column(0).to_numpy()
    if ts.max() >= END_MS: errs.append(f"{f} has data after cutoff")
    if any(((ts >= a) & (ts < c)).any() for a, c in LOCK_MS): errs.append(f"{f} has lockbox rows")
    if f.parts[-4] == "discovery" and f.stem >= "2026-05": errs.append(f"{f} in discovery root")
    inv.update(f"{f.relative_to(b)}:{f.stat().st_size}:{n}\n".encode())
for sym in ("BTCUSDT", "ETHUSDT", "SOLUSDT"):
    ms = sorted(p.stem for p in b.glob(f"*/bars_1m/{sym}/*.parquet"))
    if not ms or ms[0] > "2021-06" or ms[-1] != "2026-10": errs.append(f"{sym} months {ms[:1]}..{ms[-1:]}")
if rows < 30_000_000: errs.append(f"total 1m rows {rows} implausibly low")
if list((b / "views/workers").glob("confirm")) if (b / "views/workers").exists() else False: errs.append("confirm in workers view")
if errs:
    print(json.dumps({"ok": False, "errors": errs[:20]})); sys.exit(1)
(b / "BUILD_OK").write_text(json.dumps({"rows_1m": rows, "inventory_sha256": inv.hexdigest(), "symbols": len(man["symbols"])}))
print(json.dumps({"ok": True, "rows_1m": rows, "inventory_sha256": inv.hexdigest()}))
