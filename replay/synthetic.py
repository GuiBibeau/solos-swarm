"""Synthetic histories for tests and the README worked example. Structured: three hot families per run give most
survivors and most referee confirmations; unstructured: i.i.d. noise where a survivor has zero expected value."""
import random


def attempt(fam, seq, surv, ref=None, calls=60, cost=1.0, trials=30):
    return {"task_id": f"{fam}-{seq}", "family": fam, "role": "worker", "seq": seq, "status": "filed", "calls": calls,
            "cost_usd": cost, "minutes": 30, "trials_used": trials, "trials_budget": 40, "beats_placebo_max": surv,
            "mde_ok": surv, "net_bps": 3.0 if surv else -1.0, "referee": ref if surv else None}


def make_run(i, rng, hot_p=0.6, structured=True, n_fam=12, depth=6):
    hot = set(rng.sample(range(n_fam), 3)) if structured else set()
    atts = []
    for f in range(n_fam):
        for k in range(depth):
            p = hot_p if f in hot else (0.03 if structured else 0.2)
            s = rng.random() < p
            ref = ("confirmed" if rng.random() < (0.8 if f in hot else 0.1) else "not_confirmed") if s else None
            if not structured:
                ref = ("confirmed" if rng.random() < 1 / 3 else "not_confirmed") if s else None   # EV 0 at f=0.5
            atts.append(attempt(f"f{f:02d}", k, s, ref))
    return {"run_id": f"r{i}", "scorable": True, "attempts": atts}


def history(n, seed, **kw):
    rng = random.Random(seed)
    return [make_run(i, rng, **kw) for i in range(n)]
