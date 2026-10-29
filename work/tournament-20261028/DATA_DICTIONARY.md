# Tournament dataset: data dictionary (v1, built by `build_dataset.py`)

Mount (read-only in the swarm sandbox as `/dataset`): the directory that contains `manifest.json`, `discovery/` and
(referee wave only) `confirm/`. Binance USDT-M perpetual futures, 50 symbols, derived from public Binance dumps
(aggregate trades, daily `metrics`, daily `bookDepth`, monthly `fundingRate`). Binance-derived only: no Phoenix
data, no Pyth/Elfa/Kalshi/Tardis liquidations (Tardis free data is first-of-month only, so there is no usable
liquidation table; use the taker-flow and max-trade columns as cascade proxies).

## Layout

    manifest.json                       symbols, first/last month, bytes, thresholds, dropped windows
    discovery/<table>/<SYMBOL>/<YYYY-MM>.parquet     months <= 2026-04   (worker waves see only this root)
    confirm/<table>/<SYMBOL>/<YYYY-MM>.parquet       months 2026-05 .. 2026-10-22  (referee wave only)
    tables: bars_1m  bars_5m  oi_5m  depth_5m  funding

Read one symbol: `pd.concat(pd.read_parquet(f) for f in sorted(glob('/dataset/discovery/bars_1m/BTCUSDT/*.parquet')))`.
All timestamps are UTC integer milliseconds (`ts_ms` = bar OPEN time). A bar is complete only at `ts_ms + 60_000`
(1m) or `+ 300_000` (5m): any feature used to trade must be shifted so it uses closed bars only.

## Hard exclusions (enforced by the builder, rows are absent, not masked)

- Nothing at or after 2026-10-23 00:00 UTC (last included day 2026-10-22).
- The three lockbox weeks of ADR-0009 with 24 h guard bands: [2026-08-16, 2026-08-25), [2026-09-06, 2026-09-15),
  [2026-09-27, 2026-10-06). They fall inside `confirm/`. Bars that would span a gap are clipped (`n_minutes` < 5 in 5m).
- Survivorship: the universe is the 50 symbols listed today; delisted and never-listed contracts are absent, and
  each symbol starts at its own listing date (see `manifest.json`). A pooled result is conditional on surviving to 2026.

## bars_1m / bars_5m (per symbol-month, dense grid between first and last trade of the month)

| column | type | meaning |
|---|---|---|
| ts_ms | int64 | bar open, UTC ms |
| open high low close | float64 | aggregate-trade prices; NaN in minutes with no trade (no forward fill) |
| volume | f32 | base-asset quantity traded |
| quote_volume | f32 | USDT notional = sum(price x qty) |
| buy_vol sell_vol | f32 | base qty by TAKER side (taker buy = `is_buyer_maker` false) |
| buy_usd sell_usd | f32 | USDT notional by taker side |
| signed_usd | f32 | buy_usd - sell_usd (signed taker flow) |
| n_agg | i32 | aggregate trades in the bar |
| n_raw | i32 | underlying raw trades = sum(last_trade_id - first_trade_id + 1) |
| n_buy_agg | i32 | aggregate trades with a taker buyer |
| large_buy_usd large_sell_usd | f32 | notional from aggregate trades >= 50,000 USDT, by taker side |
| xlarge_buy_usd xlarge_sell_usd | f32 | same for >= 250,000 USDT |
| n_large | i32 | count of aggregate trades >= 50,000 USDT |
| max_trade_usd | f32 | largest single aggregate trade in the bar |
| rv_tick | f32 | sum of squared log price changes between consecutive aggregate trades (tick realised variance) |
| has_trades | bool | false for an empty minute (all flow columns are 0, prices NaN) |
| n_minutes (5m only) | i8 | minutes present in the bucket |

Checked against Binance klines (SOLUSDT 2026-07-12): high/low/close identical; per-minute quote volume within 1.7%
(aggregate trades are stamped at their first fill, klines at the matching-engine minute).

## oi_5m (daily `metrics`, 5-minute grid; Binance daily files from 2020-09 when pulled, augment from 2026-01)

`ts_ms`, `avail_ms` (= ts_ms + 300,000: the earliest time a trader may use the row; always join on `avail_ms`),
`sum_open_interest` (contracts), `sum_open_interest_value` (USDT), `count_toptrader_long_short_ratio`,
`sum_toptrader_long_short_ratio`, `count_long_short_ratio`, `sum_taker_long_short_vol_ratio` (all f32).

## depth_5m (daily `bookDepth`, last snapshot in each 5-minute bucket; 2023-01 on where pulled, augment from 2025-10)

`ts_ms`, `avail_ms`, `depth_{bid,ask}_{1,2,5}pct_usd` (f32): resting USDT notional within 1/2/5 percent of mid.

## funding (monthly `fundingRate`; one row per settlement)

`ts_ms` (settlement calc time), `interval_h` (i8, 8 or 4 or 1: it changes per symbol over time), `rate` (f64, fraction
per interval). A long pays `rate x notional` at `ts_ms`. Earlier history needs `fetch_funding.py`; coverage per symbol
is in `manifest.json` and must be checked (a missing month means unknown funding, not zero).

## Known limitations (state them in every filing)

1 Agg-trade timestamps are first-fill times. 2 `metrics`/`bookDepth` are sampled snapshots with occasional gaps.
3 No bid/ask or order-book updates: spread/queue effects need a cost model, not a measurement. 4 Prices are perp
trade prices, not index/mark. 5 Universe survivorship (above). 6 `large_*` thresholds are fixed USDT notionals, so they
mean different things for BTC and for a small alt: normalise per symbol with trailing data only.
