"""Run-history schema: one JSON object per run, one line per run (history.jsonl).

run = {schema, run_id, step, source, scorable, wall_minutes, spent_usd, model_calls, cache_hit_rate,
       agents, filed, partial, failed, attempts: [attempt]}
attempt = {task_id, agent_id, record_ids, family, role (worker|referee|other), wave, seq, status (filed|partial|failed),
           calls, cost_usd, minutes, trials_used, trials_budget, beats_placebo_max, mde_ok, net_bps, referee}
Every metric may be None when the evidence lacks it. None never counts as a pass.
"""
import json
from pathlib import Path

SCHEMA = 1
VERDICTS = ("confirmed", "not_confirmed", "invalid")


def survives_screen(a):
    """The scorer derives survival from raw metrics, never from a self-reported decision (trial counting and the
    shuffled-day placebo bar): filed, trials counted and inside budget, beat the placebo maximum, effect-size floor
    met, positive net bps."""
    t, b = a.get("trials_used"), a.get("trials_budget")
    return bool(a.get("status") == "filed" and t is not None and b is not None and t <= b
                and a.get("beats_placebo_max") is True and a.get("mde_ok") is True
                and (a.get("net_bps") or 0) > 0)


def is_scorable(run):
    """A run carries quality evidence only if some worker attempt reports counted trials. Without it every policy
    looks like pure cost, and the cheapest (do nothing) would win, so such runs are kept but not scored."""
    return any(a.get("role") == "worker" and a.get("trials_used") is not None for a in run.get("attempts", []))


def read_history(path):
    runs = [json.loads(x) for x in Path(path).read_text().splitlines() if x.strip()]
    for r in runs:
        r["scorable"] = is_scorable(r)
    return runs


def write_history(runs, path):
    Path(path).write_text("".join(json.dumps(r, sort_keys=True) + "\n" for r in runs))
