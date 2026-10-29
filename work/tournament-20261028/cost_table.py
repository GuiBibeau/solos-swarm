#!/usr/bin/env python3
"""Worst-case cost table from the goals' own numbers. Run after gen.py for all steps with synthetic evidence, or with the
planned shapes below (referee count assumed 24)."""
from common import call_cost, call_cap
import gen

SH = {"w1": (24, 1.10, 0.60), "c1": (None, None, None), "w2": (24, 1.30, 0.60), "c2": (None,) * 3, "w3": (24, 1.00, 0.60),
      "c3": (None,) * 3, "c4": (None,) * 3}
sat, noc, typ = call_cost(), call_cost(cache=False), call_cost(60_000, 3_000, 2_000)
rows, tot = [], dict(cap=0, real=0, typ=0)
print("| step | agents | envelope each | max_calls | calls to exhaust envelope (saturated, cached / uncached) | goal cap (hard ledger ceiling) | real invoice bound off-peak (50%) | expected ledger (typical call, 85% of calls) |")
print("|---|---|---|---|---|---|---|---|")
for s, c in gen.STEPS.items():
    if c["kind"] in ("worker", "referee"):
        n, env, col = SH[s]; cap = n * env + col; calls = call_cap(env); agents = n + 1
        exp = n * calls * 0.85 * typ + 0.4; envs = f"${env:.2f} (+${col:.2f} collector)"
    else:
        cap = c["coord"] + c["rsi"]; calls = call_cap(c["rsi"]); agents = 2; envs = f"${c['coord']:.2f} coord / ${c['rsi']:.2f} RSI"
        exp = 0.4 * cap
    print(f"| {s} | {agents} | {envs} | {calls} | {env_calls if False else ''}{int((env if c['kind'] in ('worker','referee') else c['rsi'])/sat)} / {int((env if c['kind'] in ('worker','referee') else c['rsi'])/noc)} | ${cap:.2f} | ${cap*0.5:.2f} | ${exp:.2f} |")
    tot["cap"] += cap; tot["real"] += cap * 0.5; tot["typ"] += exp
print(f"| **total** | | | | | **${tot['cap']:.2f}** (reserve ${100-tot['cap']:.2f}) | **${tot['real']:.2f}** | **${tot['typ']:.2f}** |")
print(f"\nPer-call ledger cost: saturated context 135k tokens with cache ${sat:.4f}; same without cache ${noc:.4f}; typical (60k ctx, 3k new, 2k out) ${typ:.4f}.")
print(f"Worst case at peak by mistake: ledger output rate 1.20 < peak 1.32 $/M, so the invoice could exceed the ledger by under 10% of the output share; drive.py refuses to start in peak.")
