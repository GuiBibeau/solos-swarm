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
    text = str(m.get("verdict") or row.get("verdict") or "").lower().replace("-", " ")
    return LABELS.get(text.strip())


def _attempt(task, finding, agent, cost, seq):
    inp = task.get("input") or {}
    rows = _rows(finding) if finding else []
    cand = next((r for r in rows if r.get("kind") == "candidate"), None)
    m = (cand or {}).get("metrics") if isinstance((cand or {}).get("metrics"), dict) else {}
    is_ref = any(r.get("kind") == "referee-report" for r in rows)
    status = "failed" if not finding else ("partial" if _is_partial(rows) else "filed")
    mins = ((agent.get("stopped_ms") or 0) - (agent.get("started_ms") or 0)) / 60000 if agent.get("stopped_ms") else None
    return {"task_id": task.get("id"), "agent_id": task.get("claimed_by"), "record_ids": [r.get("id") for r in rows if r.get("id")],
            "family": str(inp.get("family") or task.get("id", "")[:3]), "role": "referee" if is_ref else (inp.get("role") or "worker"),
            "wave": inp.get("wave"), "seq": seq, "status": status, "calls": agent.get("model_calls"),
            "cost_usd": round(cost / 1e6, 4), "minutes": round(mins, 2) if mins else None,
            "trials_used": m.get("trials_used"), "trials_budget": m.get("trials_budget"),
            "beats_placebo_max": m.get("beats_placebo_max"), "mde_ok": m.get("mde_ok"), "net_bps": m.get("net_bps_per_trade"),
            "referee": None, "verdicts": {r.get("id"): _verdict(r) for r in rows if r.get("kind") == "referee-report" and _verdict(r)}
            if is_ref else {}}


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
        fam = str((t.get("input") or {}).get("family") or t.get("id", "")[:3])
        a = _attempt(t, by_task.get(t.get("id")), (state.get("agents") or {}).get(t.get("claimed_by"), {}), cost[t.get("claimed_by")], seq[fam])
        seq[fam] += 1
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


def ingest(dirs, steps=None):
    runs = [ingest_dir(d, (steps or {}).get(str(d))) for d in dirs if (Path(d) / "state.json").exists()]
    return link_referees(runs)
