"""Turn collected run evidence (state.json, events.jsonl as written by `remote.py collect`) into history records.

Collect on the box side first (no ssh here):  python3 scripts/remote.py collect --run-id RUN --output DIR
Then:  python3 -m replay ingest DIR [DIR ...] --out history.jsonl [--step w1]
"""
import json
from collections import Counter
from pathlib import Path

from .schema import SCHEMA, VERDICTS, is_scorable

LABELS = {"confirmed": "confirmed", "not confirmed": "not_confirmed", "not_confirmed": "not_confirmed", "invalid": "invalid"}


def _rows(finding):
    res = finding.get("result") or {}
    return [r for k in ("papers", "experiments") for r in res.get(k, []) if isinstance(r, dict)]


def _is_partial(rows):
    return bool(rows) and all(r.get("severity") == "defer" or "PARTIAL" in str(r.get("description", ""))[:40] for r in rows)


def _verdict(row):
    m = row.get("metrics") if isinstance(row.get("metrics"), dict) else {}
    text = str(m.get("verdict") or row.get("verdict") or row.get("decision") or "").lower().replace("-", " ").strip()
    for key in ("not confirmed", "not_confirmed", "invalid", "confirmed"):      # longest first: "not confirmed" contains "confirmed"
        if text.startswith(key):
            return LABELS[key]
    return None


def _role(inp, rows):
    if inp.get("kind") == "referee" or any(r.get("kind") == "referee-report" for r in rows):
        return "referee"
    if inp.get("role") == "coordinator" or not inp.get("family"):
        return "other"          # collectors, closers: no family, not an attempt at a hypothesis
    return "worker"


def _trial_metrics(rows):
    """Counted trials come from candidate, negative-result and trial-ledger records; the best candidate (a survivor
    first, then the larger net bps) supplies the screen flags. A filed null with counted trials is still an attempt."""
    ms = [(r, r["metrics"]) for r in rows if isinstance(r.get("metrics"), dict) and r.get("kind") in ("candidate", "negative-result", "trial-ledger")]
    used = [m["trials_used"] for _, m in ms if isinstance(m.get("trials_used"), (int, float))]
    budget = [m["trials_budget"] for _, m in ms if isinstance(m.get("trials_budget"), (int, float))]
    cands = [m for r, m in ms if r.get("kind") == "candidate"]
    best = max(cands, key=lambda m: (m.get("beats_placebo_max") is True, m.get("net_bps_per_trade") or -1e9), default={})
    nul = bool(used) and not cands
    return {"trials_used": max(used) if used else None, "trials_budget": max(budget) if budget else None,
            "beats_placebo_max": False if nul else best.get("beats_placebo_max"), "mde_ok": False if nul else best.get("mde_ok"),
            "net_bps": best.get("net_bps_per_trade")}


def _attempt(task, finding, agent, cost, seq):
    inp = task.get("input") or {}
    rows = _rows(finding) if finding else []
    role = _role(inp, rows)
    status = "failed" if not finding else ("partial" if _is_partial(rows) else "filed")
    mins = ((agent.get("stopped_ms") or 0) - (agent.get("started_ms") or 0)) / 60000 if agent.get("stopped_ms") else None
    out = {"task_id": task.get("id"), "agent_id": task.get("claimed_by"), "record_ids": [r.get("id") for r in rows if r.get("id")],
           "family": str(inp.get("family") or ""), "role": role, "wave": inp.get("wave"), "seq": seq, "status": status,
           "calls": agent.get("model_calls"), "cost_usd": round(cost / 1e6, 4), "minutes": round(mins, 2) if mins else None,
           "referee": None, "verdicts": {}, **_trial_metrics(rows)}
    if role == "referee":
        target = (inp.get("candidate") or {}).get("id")
        v = next((_verdict(r) for r in rows if r.get("kind") == "referee-report" and _verdict(r)), None)
        out["verdicts"] = {target: v} if target and v else {}
    return out


def _times(path):
    out = []
    for line in path.read_text(errors="ignore").splitlines():
        try:
            out.append(json.loads(line)["time_ms"])
        except (ValueError, KeyError, TypeError):
            continue          # truncated or foreign line
    return out


def ingest_dir(d, step=None):
    d = Path(d)
    state = json.loads((d / "state.json").read_text())
    tasks = state.get("tasks") or []
    by_task = {f.get("task_id"): f for f in state.get("findings", [])}
    cost = Counter()
    for i in state.get("intents", []):
        cost[i.get("agent_id")] += i.get("cost_microusd", 0)
    seq, attempts = Counter(), []
    for t in tasks:
        fam = str((t.get("input") or {}).get("family") or "")
        a = _attempt(t, by_task.get(t.get("id")), (state.get("agents") or {}).get(t.get("claimed_by"), {}), cost[t.get("claimed_by")], seq[fam])
        seq[fam] += a["role"] == "worker"
        attempts.append(a)
    calls = sum((v.get("model_calls") or 0) for v in (state.get("agents") or {}).values())
    ev = d / "events.jsonl"
    ts = _times(ev) if ev.exists() else []
    run = {"schema": SCHEMA, "run_id": f"{d.parent.name}/{d.name}" if step is None else f"{d.name}:{step}", "step": step, "source": str(d),
           "wall_minutes": round((max(ts) - min(ts)) / 60000, 1) if ts else None,
           "spent_usd": round(state.get("spent_microusd", 0) / 1e6, 2), "model_calls": calls, "cache_hit_rate": None,
           "agents": len(state.get("agents") or {}), "filed": sum(a["status"] == "filed" for a in attempts),
           "partial": sum(a["status"] == "partial" for a in attempts), "failed": sum(a["status"] == "failed" for a in attempts),
           "attempts": attempts}
    run["scorable"] = is_scorable(run)
    return run


def link_referees(runs):
    """Attach referee verdicts (from referee-report records in any run) to the candidate records they name."""
    verdicts = {}
    for r in runs:
        for a in r["attempts"]:
            verdicts.update(a.get("verdicts") or {})
    for r in runs:
        for a in r["attempts"]:
            hit = [verdicts[i] for i in a.get("record_ids", []) if i in verdicts]
            a["referee"] = hit[0] if hit and hit[0] in VERDICTS else a.get("referee")
    return runs


STEPS = ("w1", "c1", "w2", "c2", "w3", "c3", "c4")


def ingest_tournament(root):
    """READ-ONLY. work/tournament-20261028/evidence/<step>/ (a collected run) plus <step>-perf.json from perf_report.py.
    The whole tournament is ONE run: families recur across waves, referee verdicts attach to wave-1/2 candidates.
    Worker attempts are ordered by (wave, task id) inside each family."""
    ev, runs = Path(root) / "evidence", []
    for step in STEPS:
        if (ev / step / "state.json").exists():
            r = ingest_dir(ev / step, step)
            perf = ev / f"{step}-perf.json"
            if perf.exists():
                r["perf"] = json.loads(perf.read_text())
            for a in r["attempts"]:
                a["wave"] = a["wave"] or int(step[1])
            runs.append(r)
    if not runs:
        return None
    attempts, seq = [], Counter()
    for a in sorted((a for r in runs for a in r["attempts"]), key=lambda a: (a["wave"] or 0, a["task_id"] or "")):
        if a["role"] == "worker":
            a["seq"], seq[a["family"]] = seq[a["family"]], seq[a["family"]] + 1
        attempts.append(a)
    perfs = [r["perf"] for r in runs if "perf" in r]
    toks = sum(p.get("prompt_tokens", 0) for p in perfs)
    run = {"schema": SCHEMA, "run_id": f"{Path(root).name}", "step": "+".join(r["step"] for r in runs), "source": str(root),
           "wall_minutes": round(sum(r["wall_minutes"] or 0 for r in runs), 1), "spent_usd": round(sum(r["spent_usd"] for r in runs), 2),
           "model_calls": sum(r["model_calls"] for r in runs),
           "cache_hit_rate": round(sum(p.get("cache_hit_rate", 0) * p.get("prompt_tokens", 0) for p in perfs) / toks, 4) if toks else None,
           "agents": sum(r["agents"] for r in runs), "filed": sum(r["filed"] for r in runs), "partial": sum(r["partial"] for r in runs),
           "failed": sum(r["failed"] for r in runs), "attempts": attempts}
    run["scorable"] = is_scorable(run)
    return link_referees([run])[0]


def ingest(dirs, steps=None):
    runs = [ingest_dir(d, (steps or {}).get(str(d))) for d in dirs if (Path(d) / "state.json").exists()]
    return link_referees(runs)
