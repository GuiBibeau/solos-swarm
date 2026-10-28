#!/usr/bin/env python3
"""Read-only progress summary of one run from its state.json (and config.yaml).

    python3 scripts/swarm_progress.py RUN_DIR [--json]
    python3 scripts/swarm_progress.py --run-id NAME      # ~/.local/state/solos-swarm/NAME on the host

Shows agents running/filed/deferred/partial, the ledger against the cap, and spend per task
against its envelope. The ledger is the conservative one; see `cache_hit_price` in IMPROVEMENTS.md.
"""

import argparse
import json
from pathlib import Path

STATE = Path.home() / ".local/state/solos-swarm"


def load(path, default=None):
    try:
        return json.loads(Path(path).read_text())
    except (OSError, ValueError):
        return default


def record_kind(finding):
    """filed | partial | defer | empty for one Finding."""
    rows = [row for value in (finding.get("result") or {}).values() if isinstance(value, list) for row in value]
    if not rows:
        return "empty"
    if all(isinstance(row, dict) and row.get("severity") == "defer" for row in rows):
        return "partial" if all("PARTIAL" in str(row.get("description", "")) for row in rows) else "defer"
    return "filed"


def summarize(run_dir):
    run_dir = Path(run_dir)
    state = load(run_dir / "state.json")
    if state is None:
        raise ValueError(f"no readable state.json in {run_dir}")
    config = load(run_dir / "config.yaml", {})  # JSON despite the extension
    cap = config.get("budget_microusd")
    count = config.get("agent_count") or len(state["agents"]) or 1
    findings = {f["agent_id"]: f for f in state["findings"]}
    spend, calls = {}, {}
    for intent in state["intents"]:
        cost = intent["cost_microusd"] if intent["state"] == "completed" else intent["hold_microusd"]
        spend[intent["agent_id"]] = spend.get(intent["agent_id"], 0) + cost
        calls[intent["agent_id"]] = calls.get(intent["agent_id"], 0) + 1
    tasks = {t["claimed_by"]: t for t in state["tasks"] if t.get("claimed_by")}
    rows, counts = [], {"running": 0, "filed": 0, "partial": 0, "defer": 0, "empty": 0, "stopped_unfiled": 0}
    for agent_id, agent in sorted(state["agents"].items()):
        task = tasks.get(agent_id) or {}
        envelope = task.get("budget_microusd") or ((cap or 0) // count)
        kind = record_kind(findings[agent_id]) if agent_id in findings else None
        if kind:
            counts[kind] += 1
        elif agent["status"] == "running":
            counts["running"] += 1
        else:
            counts["stopped_unfiled"] += 1
        rows.append({"agent": agent_id, "task": task.get("id"), "state": kind or agent["status"],
                     "model_calls": agent["model_calls"], "run_code": agent.get("run_code_calls"),
                     "spent_microusd": spend.get(agent_id, 0), "envelope_microusd": envelope,
                     "used": round(spend.get(agent_id, 0) / envelope, 3) if envelope else None,
                     "terminal": agent.get("terminal")})
    return {"goal_status": state["goal_status"], "agents": len(state["agents"]), "counts": counts,
            "findings": len(findings), "artifact": state["artifact"] is not None,
            "spent_microusd": state["spent_microusd"], "held_microusd": state["held_microusd"],
            "cap_microusd": cap, "tasks": rows}


def render(summary):
    usd = lambda value: "-" if value is None else f"${value / 1e6:,.2f}"
    counts = ", ".join(f"{name} {n}" for name, n in summary["counts"].items() if n)
    cap = summary["cap_microusd"]
    pct = f" ({summary['spent_microusd'] / cap:.0%})" if cap else ""
    lines = [f"goal {summary['goal_status']}: {summary['agents']} agents: {counts or 'none'}",
             f"ledger {usd(summary['spent_microusd'])} spent + {usd(summary['held_microusd'])} held of cap {usd(cap)}{pct}",
             f"findings {summary['findings']}, artifact {'present' if summary['artifact'] else 'absent'}", "",
             f"{'agent':9} {'task':5} {'state':16} {'calls':>5} {'code':>5} {'spent':>8} {'envelope':>9} {'used':>5}"]
    for row in summary["tasks"]:
        used = "-" if row["used"] is None else f"{row['used']:.0%}"
        lines.append(f"{row['agent']:9} {str(row['task'] or '-'):5} {row['state']:16} {row['model_calls']:>5} "
                     f"{str(row['run_code'] if row['run_code'] is not None else '-'):>5} {usd(row['spent_microusd']):>8} "
                     f"{usd(row['envelope_microusd']):>9} {used:>5}")
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("run_dir", nargs="?", type=Path)
    parser.add_argument("--run-id")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    if bool(args.run_dir) == bool(args.run_id):
        parser.error("give RUN_DIR or --run-id")
    try:
        summary = summarize(args.run_dir or STATE / args.run_id)
    except ValueError as error:
        parser.exit(2, f"swarm_progress: {error}\n")
    print(json.dumps(summary, indent=2) if args.json else render(summary))


if __name__ == "__main__":
    main()
