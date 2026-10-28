"""Deployment guard: adopt a candidate policy only if it is at least as good as the baseline on the replay set AND on
runs that were not used to propose it, with a bootstrap CI over runs, and only with enough history."""
import hashlib
import math
import random

from .policy import from_spec
from .simulate import Weights, score


def split_runs(runs, held_frac=0.3, min_held=6, salt="replay-v1"):
    """Deterministic dev/held-out split by hash of run_id (stable as history grows by appending)."""
    ranked = sorted(runs, key=lambda r: hashlib.sha256((salt + r["run_id"]).encode()).hexdigest())
    k = max(min_held, math.ceil(held_frac * len(runs)))
    return ranked[k:], ranked[:k]


def bootstrap_ci(xs, level=0.9, n=2000, seed=0):
    rng, m = random.Random(seed), len(xs)
    means = sorted(sum(xs[rng.randrange(m)] for _ in range(m)) / m for _ in range(n))
    lo, hi = means[int((1 - level) / 2 * n)], means[min(n - 1, int((1 + level) / 2 * n))]
    return lo, hi


def decide(runs, baseline, candidate, w=Weights(), tuned_on=None, min_runs=12, min_held=6, held_frac=0.3,
           level=0.95, max_unexplored=0.10, seed=0):
    scored = [r for r in runs if r.get("scorable")]
    v = {"adopt": False, "n_runs": len(runs), "n_scorable": len(scored), "min_runs": min_runs,
         "baseline": baseline, "candidate": candidate,
         "limits": "replay cannot evaluate branches the online runs never explored; history is small and mostly null"}
    if len(scored) < max(min_runs, 2 * min_held):
        return dict(v, verdict="insufficient history")
    dev, held = split_runs(scored, held_frac, min_held)
    if tuned_on is not None and {r["run_id"] for r in held} & set(tuned_on):
        return dict(v, verdict="refused: candidate was tuned on held-out runs")
    b, c = from_spec(baseline), from_spec(candidate)
    sb, sc = score(b, scored, w), score(c, scored, w)
    d = [x - y for x, y in zip(sc["rewards"], sb["rewards"])]
    ids = [r["run_id"] for r in scored]
    held_ids = {r["run_id"] for r in held}
    dd = [x for x, i in zip(d, ids) if i not in held_ids]
    hd = [x for x, i in zip(d, ids) if i in held_ids]
    lo, hi = bootstrap_ci(hd, level, seed=seed)
    v.update(n_dev=len(dd), n_held=len(hd), dev_mean_diff=sum(dd) / len(dd), held_mean_diff=sum(hd) / len(hd),
             held_ci=[lo, hi], ci_level=level, unexplored_share=sc["unexplored_share"],
             baseline_mean=sb["mean"], candidate_mean=sc["mean"])
    if sc["unexplored_share"] > max_unexplored:
        return dict(v, verdict="refused: candidate relies on branches never explored")
    if v["dev_mean_diff"] < 0 or v["held_mean_diff"] <= 0:
        return dict(v, verdict="refused: not better than baseline on dev and held-out runs")
    if lo < 0:
        return dict(v, verdict="refused: held-out improvement not distinguishable from noise")
    return dict(v, adopt=True, verdict="adopt")
