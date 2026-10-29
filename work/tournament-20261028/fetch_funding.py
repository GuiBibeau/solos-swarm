#!/usr/bin/env python3
"""Fetch Binance USDT-M monthly fundingRate dumps (tiny: ~50 symbols x <=75 months, a few MB in all).

The phase-1 pull (binance_pull.py futures) has no fundingRate and the augment lane only holds 2026-01 on,
so this fills the earlier history. It is the ONLY network step of the tournament data pipeline, it reuses the
puller's checksum + transfer-guard pause logic, and it should be run once after phase 1 finishes.
Usage (on the box):  python3 fetch_funding.py [SYMBOL,SYMBOL...]
"""
import sys
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor

sys.path.insert(0, str(Path.home() / "research/regimes/freedata"))
import binance_pull as bp  # noqa: E402
from build_dataset import SYMBOLS  # noqa: E402

syms = sys.argv[1].split(",") if len(sys.argv) > 1 else SYMBOLS
ks = [k for s in syms for k in bp.keys(f"data/futures/um/monthly/fundingRate/{s}/") if k.endswith(".zip")]
bp.log(f"start fundingRate: {len(ks)} files")
with ThreadPoolExecutor(2) as ex:
    total = sum(ex.map(bp.fetch, ks))
bp.log(f"done fundingRate: {total/1e6:.1f} MB new")
