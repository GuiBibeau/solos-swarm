#!/usr/bin/env python3
"""Build the compact derived dataset for the 2026-10-28 discovery tournament.

Offline (no network), resumable (one shard = one symbol x month, written to .tmp then renamed),
memory-bounded (streamed 64 MB CSV blocks, partial aggregates by minute), single process, meant to
run as `nice -n 19 ionice -c3 python3 build_dataset.py ...`. Run on the box with the fm-repo venv.

Sources (read-only):
  A  free/binance/futures/um/monthly/aggTrades/<SYM>/<SYM>-aggTrades-YYYY-MM.zip   (<= 2025-09, CSV)
  B  augment/tables/binance/aggTrades/<SYM>/YYYY-MM-DD.parquet                      (>= 2025-10)
  C  free/binance/futures/um/daily/metrics/<SYM>/*.zip  + augment/tables/binance/metrics (>= 2026-01)
  D  free/binance/futures/um/daily/bookDepth/<SYM>/*.zip + augment/tables/binance/bookDepth (>= 2025-10)
  E  free/binance/futures/um/monthly/fundingRate/<SYM>/*.zip (fetch_funding.py) + augment fundingRate (>= 2026-01)

Hard exclusions applied to EVERY table (see DATA_DICTIONARY.md): nothing at or after 2026-10-23 UTC, and the
three lockbox weeks of ADR-0009 with their 24 h guard bands are dropped. Rows before/after are simply absent.
"""
import argparse
import calendar
import glob
import json
import os
import sys
import time
import zipfile
from datetime import datetime, timezone
from pathlib import Path

import numpy as np
import pandas as pd
import pyarrow as pa
import pyarrow.csv as pacsv
import pyarrow.parquet as pq

HOME = Path.home()
FREE = HOME / ".local/share/solos-data/free/binance/futures/um"
AUG = HOME / ".local/share/solos-data/augment/tables/binance"
LAST_DAY = "2026-10-22"                      # inclusive; 2026-10-23 and later never enter the dataset
CONFIRM_FROM = "2026-05"                     # months >= this go to confirm/, never to the discovery root
LARGE_USD = 50_000.0
XLARGE_USD = 250_000.0
LOCKBOX = [("2026-08-16", "2026-08-25"), ("2026-09-06", "2026-09-15"), ("2026-09-27", "2026-10-06")]  # [a, b) UTC, guard incl.
SYMBOLS = ("BTCUSDT ETHUSDT SOLUSDT XRPUSDT DOGEUSDT BNBUSDT HYPEUSDT ZECUSDT SUIUSDT TAOUSDT ENAUSDT NEARUSDT "
           "ADAUSDT TRXUSDT XLMUSDT LINKUSDT AVAXUSDT LTCUSDT BCHUSDT DOTUSDT AAVEUSDT UNIUSDT WLDUSDT ONDOUSDT "
           "RENDERUSDT VIRTUALUSDT FETUSDT JUPUSDT JTOUSDT PUMPUSDT FARTCOINUSDT MORPHOUSDT PENGUUSDT WIFUSDT "
           "1000PEPEUSDT 1000BONKUSDT ARBUSDT OPUSDT APTUSDT INJUSDT TIAUSDT SEIUSDT FILUSDT ETCUSDT HBARUSDT "
           "TONUSDT KAITOUSDT XPLUSDT LITUSDT MONUSDT").split()
CSV_COLS = ["agg_trade_id", "price", "quantity", "first_trade_id", "last_trade_id", "transact_time", "is_buyer_maker"]


def ms(day):
    return int(datetime.strptime(day, "%Y-%m-%d").replace(tzinfo=timezone.utc).timestamp() * 1000)


LOCK_MS = [(ms(a), ms(b)) for a, b in LOCKBOX]
END_MS = ms(LAST_DAY) + 86_400_000           # exclusive


def keep_mask(t_ms):
    """True where a millisecond timestamp may enter the dataset."""
    ok = np.asarray(t_ms) < END_MS
    for a, b in LOCK_MS:
        ok &= ~((np.asarray(t_ms) >= a) & (np.asarray(t_ms) < b))
    return ok


def months(first, last):
    y, m = map(int, first.split("-"))
    while f"{y:04d}-{m:02d}" <= last:
        yield f"{y:04d}-{m:02d}"
        y, m = (y + 1, 1) if m == 12 else (y, m + 1)


def root_for(out, month):
    return out / ("confirm" if month >= CONFIRM_FROM else "discovery")


def write_atomic(df, path):
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp")
    pq.write_table(pa.Table.from_pandas(df, preserve_index=False), tmp, compression="zstd", compression_level=6)
    tmp.rename(path)


# ---------------------------------------------------------------- aggTrades -> 1-minute bars
def tick_chunk(t, price, qty, first_id, last_id, maker, prev_price):
    """Partial per-minute aggregates for one time-ordered chunk of aggregate trades."""
    usd = price * qty
    buy = ~maker                              # taker bought when the buyer was not the maker
    lp = np.log(price)
    lr = np.diff(lp, prepend=np.log(prev_price) if prev_price else lp[0])
    df = pd.DataFrame({
        "m": t // 60_000, "p": price, "q": qty, "usd": usd,
        "buy_vol": np.where(buy, qty, 0.0), "sell_vol": np.where(buy, 0.0, qty),
        "buy_usd": np.where(buy, usd, 0.0), "sell_usd": np.where(buy, 0.0, usd),
        "n_agg": 1, "n_raw": last_id - first_id + 1, "n_buy_agg": buy.astype(np.int64),
        "large_buy_usd": np.where(buy & (usd >= LARGE_USD), usd, 0.0),
        "large_sell_usd": np.where(~buy & (usd >= LARGE_USD), usd, 0.0),
        "xlarge_buy_usd": np.where(buy & (usd >= XLARGE_USD), usd, 0.0),
        "xlarge_sell_usd": np.where(~buy & (usd >= XLARGE_USD), usd, 0.0),
        "n_large": (usd >= LARGE_USD).astype(np.int64), "max_trade_usd": usd,
        "rv_tick": lr * lr,
    })
    g = df.groupby("m", sort=True)
    sums = g.sum()[["q", "usd", "buy_vol", "sell_vol", "buy_usd", "sell_usd", "n_agg", "n_raw", "n_buy_agg",
                    "large_buy_usd", "large_sell_usd", "xlarge_buy_usd", "xlarge_sell_usd", "n_large", "rv_tick"]]
    out = sums.rename(columns={"q": "volume", "usd": "quote_volume"})
    out["open"], out["high"], out["low"], out["close"] = g.p.first(), g.p.max(), g.p.min(), g.p.last()
    out["max_trade_usd"] = g.max_trade_usd.max()
    return out, float(price[-1])


def merge_partials(parts):
    df = pd.concat(parts)
    g = df.groupby(level=0, sort=True)
    agg = g.sum()
    agg["open"], agg["high"], agg["low"], agg["close"] = g.open.first(), g.high.max(), g.low.min(), g.close.last()
    agg["max_trade_usd"] = g.max_trade_usd.max()
    return agg


def csv_batches(zpath):
    zf = zipfile.ZipFile(zpath)
    name = zf.namelist()[0]
    head = zf.open(name).read(64).decode()
    skip = 1 if head.startswith("agg_trade_id") else 0
    ro = pacsv.ReadOptions(column_names=CSV_COLS, skip_rows=skip, block_size=64 << 20, use_threads=False)
    co = pacsv.ConvertOptions(true_values=["true", "True"], false_values=["false", "False"], column_types={
        "price": pa.float64(), "quantity": pa.float64(), "first_trade_id": pa.int64(), "last_trade_id": pa.int64(),
        "transact_time": pa.int64(), "is_buyer_maker": pa.bool_(), "agg_trade_id": pa.int64()})
    for b in pacsv.open_csv(zf.open(name), read_options=ro, convert_options=co):
        yield (b.column("transact_time").to_numpy(), b.column("price").to_numpy(), b.column("quantity").to_numpy(),
               b.column("first_trade_id").to_numpy(), b.column("last_trade_id").to_numpy(),
               b.column("is_buyer_maker").to_numpy(zero_copy_only=False))


def parquet_batches(files):
    cols = ["transact_time_ms", "price", "quantity", "first_trade_id", "last_trade_id", "is_buyer_maker"]
    for f in files:
        pf = pq.ParquetFile(f)
        for b in pf.iter_batches(batch_size=2_000_000, columns=cols):
            yield (b.column(0).to_numpy(), b.column(1).to_numpy(), b.column(2).to_numpy(), b.column(3).to_numpy(),
                   b.column(4).to_numpy(), b.column(5).to_numpy(zero_copy_only=False))


def bars_for_month(sym, month):
    z = FREE / f"monthly/aggTrades/{sym}/{sym}-aggTrades-{month}.zip"
    if z.exists():
        batches = csv_batches(z)
    else:
        files = sorted(glob.glob(str(AUG / f"aggTrades/{sym}/{month}-*.parquet")))
        files = [f for f in files if Path(f).stem <= LAST_DAY]
        if not files:
            return None
        batches = parquet_batches(files)
    parts, prev = [], None
    for t, p, q, fi, la, mk in batches:
        t = np.where(t > 10**14, t // 1000, t)     # microsecond stamps, if any
        k = keep_mask(t)
        if not k.any():
            continue
        t, p, q, fi, la, mk = t[k], p[k], q[k], fi[k], la[k], mk[k]
        part, prev = tick_chunk(t, p, q, fi, la, mk, prev)
        parts.append(part)
    if not parts:
        return None
    return merge_partials(parts)


def densify(agg, month):
    y, mo = map(int, month.split("-"))
    first_min = ms(f"{month}-01") // 60_000
    last_min = (ms(f"{month}-{calendar.monthrange(y, mo)[1]:02d}") + 86_400_000) // 60_000 - 1
    lo, hi = max(int(agg.index.min()), first_min), min(int(agg.index.max()), last_min)
    df = agg.reindex(pd.RangeIndex(lo, hi + 1, name="m"))
    zero = [c for c in df.columns if c not in ("open", "high", "low", "close")]
    df["has_trades"] = df.n_agg.notna()
    df[zero] = df[zero].fillna(0.0)
    df["ts_ms"] = df.index.values.astype(np.int64) * 60_000
    df = df[keep_mask(df.ts_ms.values)]
    return df.reset_index(drop=True)


F32 = ["volume", "quote_volume", "buy_vol", "sell_vol", "buy_usd", "sell_usd", "large_buy_usd", "large_sell_usd",
       "xlarge_buy_usd", "xlarge_sell_usd", "max_trade_usd", "rv_tick"]
I32 = ["n_agg", "n_raw", "n_buy_agg", "n_large"]


def finish_1m(df):
    df = df.copy()
    df[F32] = df[F32].astype("float32")
    df[I32] = df[I32].astype("int32")
    df["signed_usd"] = (df.buy_usd - df.sell_usd).astype("float32")
    cols = ["ts_ms", "open", "high", "low", "close", "volume", "quote_volume", "buy_vol", "sell_vol", "buy_usd",
            "sell_usd", "signed_usd", "n_agg", "n_raw", "n_buy_agg", "large_buy_usd", "large_sell_usd",
            "xlarge_buy_usd", "xlarge_sell_usd", "n_large", "max_trade_usd", "rv_tick", "has_trades"]
    return df[cols]


def to_5m(d1):
    d = d1.copy()
    d["b"] = d.ts_ms // 300_000
    g = d.groupby("b", sort=True)
    sums = [c for c in d.columns if c not in ("ts_ms", "open", "high", "low", "close", "max_trade_usd", "b",
                                              "has_trades")]
    out = g[sums].sum()
    out["open"], out["high"], out["low"], out["close"] = g.open.first(), g.high.max(), g.low.min(), g.close.last()
    out["max_trade_usd"] = g.max_trade_usd.max()
    out["has_trades"] = g.has_trades.max()
    out["n_minutes"] = g.size().astype("int8")                 # < 5 where lockbox/cutoff clipped the bucket
    out["ts_ms"] = out.index.values.astype(np.int64) * 300_000
    out = out.reset_index(drop=True)
    out[F32] = out[F32].astype("float32")
    out[I32] = out[I32].astype("int32")
    return out[["ts_ms"] + [c for c in out.columns if c != "ts_ms"]]


# ---------------------------------------------------------------- side tables
def day_files(table, sym, month):
    zs = sorted(glob.glob(str(FREE / f"daily/{table}/{sym}/{sym}-{table}-{month}-*.zip")))
    ps = sorted(glob.glob(str(AUG / f"{table}/{sym}/{month}-*.parquet")))
    seen = {Path(z).stem[-10:] for z in zs}
    return zs, [p for p in ps if Path(p).stem not in seen and Path(p).stem <= LAST_DAY]


def read_day(path):
    if str(path).endswith(".zip"):
        zf = zipfile.ZipFile(path)
        return pd.read_csv(zf.open(zf.namelist()[0]))
    return pq.read_table(path).to_pandas()


def stamp_ms(s):
    if np.issubdtype(s.dtype, np.datetime64):
        return s.values.astype("datetime64[ms]").astype(np.int64)
    return pd.to_datetime(s, utc=True).dt.tz_localize(None).values.astype("datetime64[ms]").astype(np.int64)


def metrics_for_month(sym, month):
    zs, ps = day_files("metrics", sym, month)
    frames = []
    for f in zs + ps:
        d = read_day(f)
        d["ts_ms"] = stamp_ms(d["create_time"] if "create_time" in d else d["ts"])
        frames.append(d)
    if not frames:
        return None
    d = pd.concat(frames).drop_duplicates("ts_ms").sort_values("ts_ms")
    d = d[keep_mask(d.ts_ms.values)]
    keep = ["sum_open_interest", "sum_open_interest_value", "count_toptrader_long_short_ratio",
            "sum_toptrader_long_short_ratio", "count_long_short_ratio", "sum_taker_long_short_vol_ratio"]
    out = d[["ts_ms"] + keep].reset_index(drop=True)
    out[keep] = out[keep].astype("float32")
    out["avail_ms"] = out.ts_ms + 300_000        # conservative: usable one bar after its stamp
    return out


def depth_for_month(sym, month):
    zs, ps = day_files("bookDepth", sym, month)
    pcts, frames = (-5.0, -2.0, -1.0, 1.0, 2.0, 5.0), []
    for f in zs + ps:
        d = read_day(f)
        d["ts_ms"] = stamp_ms(d["timestamp"] if "timestamp" in d else d["ts"])
        d = d[d.percentage.isin(pcts)]
        d["b"] = d.ts_ms // 300_000
        last = d.groupby(["b", "percentage"]).notional.last().unstack()
        last.columns = [f"depth_{'bid' if c < 0 else 'ask'}_{int(abs(c))}pct_usd" for c in last.columns]
        frames.append(last)
    if not frames:
        return None
    d = pd.concat(frames).sort_index()
    d["ts_ms"] = d.index.values.astype(np.int64) * 300_000
    d = d[keep_mask(d.ts_ms.values)].reset_index(drop=True)
    cols = [c for c in d.columns if c != "ts_ms"]
    d[cols] = d[cols].astype("float32")
    d["avail_ms"] = d.ts_ms + 300_000
    return d[["ts_ms", "avail_ms"] + cols]


def funding_for_month(sym, month):
    frames = []
    z = FREE / f"monthly/fundingRate/{sym}/{sym}-fundingRate-{month}.zip"
    if z.exists():
        d = pd.read_csv(zipfile.ZipFile(z).open(zipfile.ZipFile(z).namelist()[0]))
        frames.append(pd.DataFrame({"ts_ms": d.calc_time.astype("int64"), "interval_h": d.get("funding_interval_hours", 8),
                                    "rate": d.last_funding_rate.astype("float64")}))
    p = AUG / f"fundingRate/{sym}/{month}.parquet"
    if p.exists():
        d = pq.read_table(p).to_pandas()
        frames.append(pd.DataFrame({"ts_ms": d.calc_time_ms.astype("int64"), "interval_h": d.funding_interval_hours,
                                    "rate": d.last_funding_rate.astype("float64")}))
    if not frames:
        return None
    d = pd.concat(frames).drop_duplicates("ts_ms").sort_values("ts_ms")
    d = d[keep_mask(d.ts_ms.values)].reset_index(drop=True)
    d["interval_h"] = d.interval_h.fillna(8).astype("int8")
    return d


TABLES = {"bars_1m": None, "bars_5m": None, "oi_5m": metrics_for_month, "depth_5m": depth_for_month,
          "funding": funding_for_month}


def do_shard(out, sym, month, rec, only=None):
    if month > LAST_DAY[:7]:
        return
    base = root_for(out, month)
    marker = base / ".done" / f"{sym}-{month}"
    if marker.exists() and not only:
        rec["skipped"] += 1
        return
    t0, wrote = time.time(), []
    agg = None if only else bars_for_month(sym, month)
    if agg is not None and len(agg):
        d1 = finish_1m(densify(agg, month))
        if len(d1):
            write_atomic(d1, base / "bars_1m" / sym / f"{month}.parquet")
            write_atomic(to_5m(d1), base / "bars_5m" / sym / f"{month}.parquet")
            wrote += ["bars_1m", "bars_5m"]
    for name, fn in TABLES.items():
        if fn is None or (only and name != only):
            continue
        df = fn(sym, month)
        if df is not None and len(df):
            write_atomic(df, base / name / sym / f"{month}.parquet")
            wrote.append(name)
    marker.parent.mkdir(parents=True, exist_ok=True)
    marker.write_text(json.dumps({"tables": wrote, "seconds": round(time.time() - t0, 1)}))
    rec["built"] += 1
    print(json.dumps({"shard": f"{sym}-{month}", "tables": wrote, "s": round(time.time() - t0, 1)}), flush=True)


def source_months(sym):
    ms_ = set()
    for z in glob.glob(str(FREE / f"monthly/aggTrades/{sym}/*.zip")):
        ms_.add(Path(z).stem[-7:])
    for p in glob.glob(str(AUG / f"aggTrades/{sym}/*.parquet")):
        ms_.add(Path(p).stem[:7])
    return sorted(m for m in ms_ if m <= LAST_DAY[:7])


def manifest(out):
    info = {"built_utc": datetime.now(timezone.utc).isoformat(timespec="seconds"), "last_day": LAST_DAY,
            "confirm_from_month": CONFIRM_FROM, "lockbox_dropped": LOCKBOX, "symbols": {}, "tables": list(TABLES),
            "thresholds_usd": {"large": LARGE_USD, "xlarge": XLARGE_USD}}
    for sym in SYMBOLS:
        files = sorted(glob.glob(str(out / "*/bars_1m" / sym / "*.parquet")))
        if files:
            ms_ = sorted(Path(f).stem for f in files)
            info["symbols"][sym] = {"first_month": ms_[0], "last_month": ms_[-1], "n_months": len(ms_)}
    for root in ("discovery", "confirm"):
        size = sum(f.stat().st_size for f in (out / root).rglob("*.parquet")) if (out / root).exists() else 0
        info[f"{root}_bytes"] = size
    (out / "manifest.json").write_text(json.dumps(info, indent=1))
    return info


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--symbols", nargs="*", default=SYMBOLS)
    ap.add_argument("--months", nargs="*", help="restrict to these YYYY-MM (testing)")
    ap.add_argument("--manifest-only", action="store_true")
    ap.add_argument("--only", choices=["oi_5m", "depth_5m", "funding"], help="rebuild just this side table (ignores .done markers)")
    ap.add_argument("--part", default="0/1", help="i/n: build only every n-th symbol (run two parts in parallel at most)")
    a = ap.parse_args()
    pa.set_cpu_count(1)
    pa.set_io_thread_count(1)
    os.environ.setdefault("OMP_NUM_THREADS", "1")
    a.out.mkdir(parents=True, exist_ok=True)
    rec = {"built": 0, "skipped": 0}
    if not a.manifest_only:
        i, n = map(int, a.part.split("/"))
        for sym in a.symbols[i::n]:
            for month in (a.months or source_months(sym)):
                do_shard(a.out, sym, month, rec, a.only)
    info = manifest(a.out)
    print(json.dumps({"summary": rec, "symbols": len(info["symbols"]), "discovery_bytes": info["discovery_bytes"],
                      "confirm_bytes": info["confirm_bytes"]}))


if __name__ == "__main__":
    sys.exit(main())
