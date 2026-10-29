#!/usr/bin/env python3
"""System-performance instrumentation for the tournament.

  perf_report.py summarize EVIDENCE_DIR --step w1      -> evidence/<step>-perf.json (also printed)
  perf_report.py report                                -> PERF-REPORT.md from every evidence/*-perf.json + launch-log.jsonl
EVIDENCE_DIR is a collected run directory (events.jsonl, state.json, agent-NN.rpc.jsonl ...).
"""
import json
import statistics
import sys
from collections import Counter, defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
STALL_S = 600


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))] if xs else None


def usage_of(body):
    if isinstance(body, str):
        try:
            body = json.loads(body)
        except ValueError:
            return {}
    return (body or {}).get("usage") or {}


def summarize(d, step=None):
    d = Path(d)
    state = json.loads((d / "state.json").read_text())
    ev = [json.loads(line) for line in (d / "events.jsonl").read_text().splitlines() if line.strip()]
    t0 = next((e["time_ms"] for e in ev if e["kind"] == "goal_registered"), ev[0]["time_ms"])
    t1 = ev[-1]["time_ms"]
    agents = state["agents"]
    findings = state["findings"]
    partial_ids, filed_ids = set(), set()
    for f in findings:
        rows = [r for r in f["result"].get("papers", []) if isinstance(r, dict)]
        is_partial = bool(rows) and all(r.get("severity") == "defer" or "PARTIAL" in str(r.get("description", ""))[:40] for r in rows)
        (partial_ids if is_partial else filed_ids).add(f["agent_id"])
    unfiled = [a for a in agents if a not in filed_ids | partial_ids]
    calls = {a: v.get("model_calls", 0) for a, v in agents.items()}
    spent = state["spent_microusd"] / 1e6
    hit = miss = out_tok = 0
    denied, blocked, tools, run_code = Counter(), 0, Counter(), Counter()
    first_submit, last_event, gaps, qwait = {}, defaultdict(lambda: t0), defaultdict(float), []
    cost_by_agent = defaultdict(int)
    errors_403 = 0
    for e in ev:
        k, dt = e["kind"], e.get("data", {})
        a = dt.get("agent_id")
        if a:
            gaps[a] = max(gaps[a], (e["time_ms"] - last_event[a]) / 1000)
            last_event[a] = e["time_ms"]
        if k == "provider_response":
            u = usage_of(dt.get("body"))
            hit += u.get("prompt_cache_hit_tokens", 0) or 0
            miss += (u.get("prompt_tokens", 0) or 0) - (u.get("prompt_cache_hit_tokens", 0) or 0)
            out_tok += u.get("completion_tokens", 0) or 0
            if "exceeds Goal limit" in json.dumps(dt)[:2000]:
                errors_403 += 1
        elif k == "provider_denied":
            denied[dt.get("reason", "?")] += 1
        elif k == "tool_budget_blocked":
            blocked += 1
        elif k in ("tool_denied",):
            denied["tool:" + str(dt.get("reason", "?"))] += 1
        elif k == "tool_completed":
            tools[dt.get("tool")] += 1
            if dt.get("tool") == "run_code":
                run_code[a] += 1
                r = dt.get("result")
                if isinstance(r, dict) and "queue_wait_ms" in r:
                    qwait.append(r["queue_wait_ms"])
            if dt.get("tool") == "submit_finding" and a not in first_submit:
                first_submit[a] = (e["time_ms"] - t0) / 60000
    for i in state.get("intents", []):
        cost_by_agent[i["agent_id"]] += i.get("cost_microusd", 0)
    n_filed = len(filed_ids)
    confirm = Counter()
    for f in d.glob("agent-*.rpc.jsonl"):
        confirm[f.name[:8]] = f.read_text(errors="ignore").count("/dataset/confirm")
    a0 = "agent-00"
    out = {
        "step": step, "wall_minutes": round((t1 - t0) / 60000, 1), "agents": len(agents), "filed": n_filed,
        "partial": len(partial_ids), "failed_unfiled": len(unfiled),
        "model_calls": sum(calls.values()), "spent_usd": round(spent, 2), "spent_per_filed_usd": round(spent / max(1, n_filed), 3),
        "calls_per_filed": round(sum(calls.values()) / max(1, n_filed), 1),
        "cache_hit_rate": round(hit / max(1, hit + miss), 4), "prompt_tokens": hit + miss, "completion_tokens": out_tok,
        "stalled_agents_gt_10min_gap": sorted(a for a, g in gaps.items() if g > STALL_S),
        "max_event_gap_s": round(max(gaps.values(), default=0)),
        "provider_denied": dict(denied), "tool_budget_blocked": blocked, "http_403_goal_limit": errors_403,
        "time_to_first_finding_min": round(min(first_submit.values()), 1) if first_submit else None,
        "time_to_median_finding_min": round(statistics.median(first_submit.values()), 1) if first_submit else None,
        "coordinator": {"calls": calls.get(a0), "run_code_calls": run_code.get(a0, 0), "spent_usd": round(cost_by_agent.get(a0, 0) / 1e6, 3),
                        "filed": a0 in filed_ids},
        "run_code": {"total": sum(run_code.values()), "queue_wait_ms_p50": pct(qwait, .5), "queue_wait_ms_p90": pct(qwait, .9),
                     "queue_wait_ms_max": max(qwait, default=None)},
        "tool_counts": dict(tools), "goal_status": state.get("goal_status"), "artifact": state.get("artifact") is not None,
        "agent_spend_usd": {a: round(c / 1e6, 3) for a, c in sorted(cost_by_agent.items())},
        "confirm_slice_mentions_by_agent": {a: c for a, c in confirm.items() if c},
    }
    if step:
        (HERE / "evidence").mkdir(exist_ok=True)
        (HERE / "evidence" / f"{step}-perf.json").write_text(json.dumps(out, indent=1))
    return out


def report():
    rows = [json.loads(p.read_text()) for p in sorted((HERE / "evidence").glob("*-perf.json"))]
    log = HERE / "launch-log.jsonl"
    L = ["# Tournament system-performance report", "",
         "| step | wall min | agents | filed | partial | failed | calls | $ spent | $/filed | calls/filed | cache hit | 403 | denied | blocked | t first finding | coord calls (run_code) |",
         "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"]
    for r in rows:
        c = r["coordinator"]
        L.append(f"| {r['step']} | {r['wall_minutes']} | {r['agents']} | {r['filed']} | {r['partial']} | {r['failed_unfiled']} | "
                 f"{r['model_calls']} | {r['spent_usd']} | {r['spent_per_filed_usd']} | {r['calls_per_filed']} | {r['cache_hit_rate']:.1%} | "
                 f"{r['http_403_goal_limit']} | {sum(r['provider_denied'].values())} | {r['tool_budget_blocked']} | "
                 f"{r['time_to_first_finding_min']} min | {c['calls']} ({c['run_code_calls']}) |")
    L += ["", f"Total ledger spend: ${sum(r['spent_usd'] for r in rows):.2f} of the $100 cap.", ""]
    for r in rows:
        L += [f"## {r['step']}", f"- stalled agents (>10 min between events): {r['stalled_agents_gt_10min_gap'] or 'none'}; longest gap {r['max_event_gap_s']} s",
              f"- run_code: {r['run_code']}", f"- denied: {r['provider_denied']}; confirm-slice mentions: {r['confirm_slice_mentions_by_agent'] or 'none'}",
              f"- goal status {r['goal_status']}, artifact present {r['artifact']}", ""]
    if log.exists():
        L += ["## Launch log", "```", log.read_text().strip(), "```"]
    (HERE / "PERF-REPORT.md").write_text("\n".join(L) + "\n")
    print("\n".join(L[:12]))


if __name__ == "__main__":
    if sys.argv[1] == "summarize":
        step = sys.argv[sys.argv.index("--step") + 1] if "--step" in sys.argv else None
        print(json.dumps(summarize(sys.argv[2], step), indent=1))
    else:
        report()
