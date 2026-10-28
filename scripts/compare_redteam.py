#!/usr/bin/env python3
"""Compare a completed swarm red-team run with the 2026-09-10 DeepSec baseline."""

import argparse
import json
from pathlib import Path

SEVERITY_MAP = {"HIGH_BUG": "critical", "HIGH": "high", "MEDIUM": "medium", "BUG": "low"}
STOP = set("a an and are as at be by for from in into is it its not of on or that the to was were with without".split())


def tokens(text):
    return {word.strip(".,:;()[]{}'\"`").lower() for word in text.split() if len(word) > 3} - STOP


def load_swarm(run_dir):
    state = json.loads((Path(run_dir) / "state.json").read_text())
    rows = []
    for finding in state.get("findings", []):
        result = finding.get("result", {})
        for row in result.get("vulnerabilities", []):
            rows.append({**row, "agent_id": finding.get("agent_id"), "task_id": finding.get("task_id")})
    artifact = (state.get("artifact") or {}).get("vulnerabilities", [])
    intents = state.get("intents", [])
    spent = state.get("spent_microusd", 0)
    calls = len([i for i in intents if i.get("state") == "completed"])
    return rows, artifact, calls, spent


def load_baseline(directory):
    records = json.loads((Path(directory) / "findings-records.json").read_text())
    rows = []
    for record in records:
        revalidation = record.get("revalidation")
        verdict = revalidation.get("verdict") if isinstance(revalidation, dict) else None
        rows.append({
            "id": record["findingId"],
            "severity": SEVERITY_MAP.get(record.get("severity"), record.get("severity", "?").lower()),
            "title": record.get("title", ""),
            "file": record.get("filePath", ""),
            "true_positive": verdict == "true-positive",
            "tokens": tokens(record.get("title", "") + " " + record.get("description", "")),
        })
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-dir", required=True, type=Path)
    parser.add_argument("--baseline-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    rows, artifact, calls, spent = load_swarm(args.run_dir)
    for row in rows:
        row["tokens"] = tokens(row.get("title", "") + " " + row.get("description", ""))
    baseline = load_baseline(args.baseline_dir)

    def by_severity(items):
        counts = {}
        for row in items:
            counts[row["severity"]] = counts.get(row["severity"], 0) + 1
        return {name: counts.get(name, 0) for name in ("critical", "high", "medium", "low")}

    swarm_files = {row["file"] for row in rows if row.get("file")}
    baseline_files = {row["file"] for row in baseline if row.get("file")}
    shared_files = sorted(swarm_files & baseline_files)

    matches = []
    for swarm_row in rows:
        best, best_score = None, 0.0
        for base_row in baseline:
            if base_row["file"] != swarm_row.get("file"):
                continue
            union = swarm_row["tokens"] | base_row["tokens"]
            score = len(swarm_row["tokens"] & base_row["tokens"]) / len(union) if union else 0.0
            if score > best_score:
                best, best_score = base_row, score
        if best is not None and best_score >= 0.12:
            matches.append({"swarm": {k: v for k, v in swarm_row.items() if k != "tokens"},
                            "baseline": {k: v for k, v in best.items() if k != "tokens"},
                            "jaccard": round(best_score, 3)})

    report = {
        "swarm": {
            "findings": len(rows), "artifact_rows": len(artifact),
            "severity": by_severity(rows), "files_flagged": len(swarm_files),
            "settled_calls": calls, "spend_usd": round(spent / 1e6, 4),
        },
        "baseline": {
            "records": len(baseline),
            "true_positives": sum(1 for row in baseline if row["true_positive"]),
            "severity_all": by_severity(baseline),
            "severity_true_positive": by_severity([r for r in baseline if r["true_positive"]]),
            "files_flagged": len(baseline_files),
        },
        "file_overlap": {"shared_files": len(shared_files), "paths": shared_files},
        "matched_findings": matches,
    }
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k != "matched_findings"}, indent=2))
    print(f"matched findings (same file, token similarity >= 0.12): {len(matches)}")
    for match in sorted(matches, key=lambda m: -m["jaccard"])[:15]:
        print(f"  [{match['jaccard']:.2f}] swarm[{match['swarm']['severity']}] {match['swarm']['title'][:70]}")
        print(f"        baseline[{match['baseline']['severity']}|{'TP' if match['baseline']['true_positive'] else 'not-TP'}] {match['baseline']['title'][:70]}")


if __name__ == "__main__":
    main()
