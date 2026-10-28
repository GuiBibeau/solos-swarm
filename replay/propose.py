"""Policy development step: grid search over adaptive-policy knobs on DEV runs only (never held-out ones)."""
import itertools

from .guard import split_runs
from .policy import from_spec
from .simulate import Weights, score

GRID = {"stop_after_null": [1, 2, 3], "per_family_max": [2, 3, 4], "max_rounds": [2, 3, 4], "call_cap": [None, 60, 90],
        "trial_budget": [None, 30, 40]}


def search(runs, baseline, w=Weights(), grid=GRID, **split):
    dev, _ = split_runs([r for r in runs if r.get("scorable")], **split)
    best = None
    for vals in itertools.product(*grid.values()):
        spec = {**{k: v for k, v in baseline.items() if k != "kind"}, **dict(zip(grid, vals)), "kind": "adaptive"}
        m = score(from_spec(spec), dev, w)["mean"]
        if best is None or m > best[0]:
            best = (m, spec)
    return {"policy": best[1], "tuned_on": [r["run_id"] for r in dev], "dev_mean": best[0]}
