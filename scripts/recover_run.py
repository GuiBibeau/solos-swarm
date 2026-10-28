#!/usr/bin/env python3
"""Recover a dead swarm run by forking its captured probes back into the next Case.

A run can die with every worker's observations intact: the per-agent `*.rpc.jsonl`
transcripts record each `fetch_url` argument and result, and the assistant text
records what the worker concluded from them. Nothing in that is derivable again
for free, and the target is live, so a rerun re-probes the same URLs at cost and
may see different answers.

This tool does not pretend the old run is complete. It builds a NEW Goal whose
tasks carry the old run's evidence under `prior_evidence`, and whose prompts tell
the worker to treat it as already-observed. That keeps the fixture_digest honest:
the new Goal is a new frozen case, and the recovery is visible in the task input
rather than smuggled into an oracle.

What is and is not recovered:

  * recovered: every completed `fetch_url` (url, method, status, final_url,
    redirects, selected headers) and the worker's own conclusions;
  * NOT recovered: the model's internal reasoning, any tool call that never
    completed, and anything the worker saw only inside a failed turn;
  * re-observed: the worker re-fetches anything it wants to trust, because a live
    target may have changed since the transcript was written.

Usage:
    recover_run.py --run-dir RUN --expected EXPECTED --seed N \\
                   --goal NEW_GOAL --out EXPECTED
"""

import argparse
import hashlib
import json
import re
import unicodedata
from pathlib import Path

# Response body is the bulkiest part of a transcript and the least useful to
# replay wholesale; the shell is megabytes of markup. Keep a bounded slice so the
# next Goal's per-turn budget is not consumed by history.
BODY_CHARS = 900
MAX_OBSERVATIONS = 40
MAX_CONCLUSIONS = 6


def clean_text(text):
    """Make fetched text safe to embed in the Goal's JSON-as-YAML document.

    A live body may contain any character, including DEL, C1 controls and the
    Unicode line/paragraph separators. JSON permits them raw; the runtime parses
    its config with a YAML reader, which rejects control characters, so an
    unsanitized excerpt makes the whole Goal unreadable. Keep tab and newline
    (they are valid escapes) and replace every other control with a space.
    """
    return "".join(
        " "
        if unicodedata.category(ch) in ("Cc", "Zl", "Zp") and ch not in "\n\t"
        else ch
        for ch in text
    )


def clean(value):
    """Recursively sanitize untrusted strings before they enter the case."""
    if isinstance(value, str):
        return clean_text(value)
    if isinstance(value, list):
        return [clean(item) for item in value]
    if isinstance(value, dict):
        return {key: clean(item) for key, item in value.items()}
    return value


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def _inner(result):
    """Unwrap the broker's envelope: `{content:[{type:text,text:<json>}]}`.

    The tool result arrives already parsed, and only the innermost text is JSON,
    so both layers must be unwrapped rather than string-parsed once.
    """
    outer = result
    if isinstance(outer, str):
        try:
            outer = json.loads(outer)
        except (TypeError, ValueError):
            return None
    blocks = outer.get("content") if isinstance(outer, dict) else None
    if not isinstance(blocks, list) or not blocks:
        return None
    text = blocks[0].get("text") if isinstance(blocks[0], dict) else None
    try:
        return json.loads(text) if isinstance(text, str) else None
    except (TypeError, ValueError):
        return None


def read_transcript(path):
    """Return (observations, conclusions) for one worker.

    Observations are the completed network probes, deduplicated by (method, url)
    with the LAST result kept, because a worker often re-probes to confirm and the
    later answer is the one it reasoned about.
    """
    observations, conclusions = {}, []
    if not path.exists():
        return [], []
    # Arguments ride on the START record and the result on the END record, joined
    # by toolCallId. Reading either alone silently yields null args.
    calls = {}
    for line in path.read_text(errors="replace").split("\n"):
        try:
            record = json.loads(line)
        except ValueError:
            continue
        kind = record.get("type")
        if kind == "tool_execution_start":
            if isinstance(record.get("args"), dict):
                calls[record.get("toolCallId")] = record["args"]
            continue
        if kind == "tool_execution_end" and record.get("toolName") == "fetch_url":
            args = calls.get(record.get("toolCallId")) or {}
            url = args.get("url")
            if not isinstance(url, str):
                continue
            method = str(args.get("method") or "GET").upper()
            entry = {"url": url, "method": method}
            if record.get("isError"):
                entry["error"] = str(record.get("result"))[:200]
                observations[(method, url)] = entry
                continue
            payload = _inner(record.get("result"))
            if not isinstance(payload, dict):
                continue
            if payload.get("outcome") == "error":
                entry["error"] = str(payload.get("reason"))[:200]
            else:
                response = payload.get("response") or payload
                entry["status"] = response.get("status")
                entry["final_url"] = response.get("final_url")
                if response.get("redirects"):
                    entry["redirected_to"] = [
                        r.get("to") for r in response["redirects"] if isinstance(r, dict)
                    ][:3]
                headers = response.get("headers")
                if isinstance(headers, dict):
                    keep = {
                        k.lower(): v
                        for k, v in headers.items()
                        if k.lower()
                        in (
                            "content-type", "server", "x-matched-path",
                            "x-clerk-auth-status", "x-clerk-auth-reason",
                            "x-vercel-id", "cache-control", "vary",
                            "access-control-allow-origin",
                            "strict-transport-security",
                            "content-security-policy",
                        )
                    }
                    if keep:
                        entry["headers"] = keep
                body = response.get("body")
                if isinstance(body, str) and body:
                    entry["body_excerpt"] = body[:BODY_CHARS]
            observations[(method, url)] = entry
        elif kind == "message_end":
            message = record.get("message") or {}
            if message.get("role") != "assistant":
                continue
            text = " ".join(
                part.get("text", "")
                for part in (message.get("content") or [])
                if isinstance(part, dict) and part.get("type") == "text"
            ).strip()
            # Drop the empty bookkeeping turns the nudge loop produced.
            if len(text) > 40:
                conclusions.append(text)
    return list(observations.values()), conclusions[-MAX_CONCLUSIONS:]


def recover(run_dir, expected_path, seed, goal_path, out_path):
    run_dir, expected_path = Path(run_dir).resolve(), Path(expected_path).resolve()
    goal_path, out_path = Path(goal_path).resolve(), Path(out_path).resolve()
    expected = json.loads(expected_path.read_text())
    tasks = expected.get("tasks") or {}
    if not tasks:
        raise ValueError("evaluator payload has no tasks to recover into")

    # Claims race: `agent-07` does not necessarily own `lens-07`. The transcript
    # belongs to the agent that actually claimed the Task, and attaching it to
    # the index-named Task would silently give one Task another agent's probes
    # while losing the real observer's evidence.
    state = {}
    if (run_dir / "state.json").is_file():
        state = json.loads((run_dir / "state.json").read_text())
    claimed = {}
    for task in state.get("tasks", []):
        owner, task_id = task.get("claimed_by"), task.get("id")
        if isinstance(owner, str) and isinstance(task_id, str) and owner:
            if owner in claimed.values():
                raise ValueError(f"one agent claims more than one Task: {owner}")
            claimed[task_id] = owner

    recovered, summary = {}, {}
    for task_id in sorted(tasks):
        agent = claimed.get(task_id)
        if agent is None:
            suffix = task_id.rsplit("-", 1)[-1]
            if suffix.isdigit():
                agent = f"agent-{int(suffix):02d}"
        path = run_dir / f"{agent}.rpc.jsonl" if agent else run_dir / "missing.rpc.jsonl"
        observations, conclusions = read_transcript(path)
        recovered[task_id] = clean(observations[:MAX_OBSERVATIONS])
        summary[task_id] = {
            "agent": agent,
            "observations": len(recovered[task_id]),
            "conclusions": len(conclusions),
        }
        tasks[task_id]["recovered"] = {
            "observations": recovered[task_id],
            "conclusions": clean(conclusions[-MAX_CONCLUSIONS:]),
            "note": (
                "Observed during an earlier run that died before filing. Treat as "
                "already-seen evidence, not as ground truth: re-fetch anything you "
                "rely on, because the target is live and may have changed."
            ),
        }

    # The Goal's tasks must carry the same recovery, so the worker prompt can
    # point at it. The case builder owns the Goal, so it is patched here by the
    # same field name and the two files stay consistent by construction.
    goal = json.loads(goal_path.read_text())
    for task in goal.get("tasks", []):
        task["input"]["prior_evidence"] = tasks[task["id"]]["recovered"]
    goal["via_recovered_run"] = {
        "run_dir": run_dir.name,
        "recovered_at_task_count": len(recovered),
        "note": "Goal forked from a run that died before filing any Finding.",
    }
    # The seed stays, so the tasks and their hypotheses are unchanged apart from
    # the added evidence; the versions move because the input shape did.
    goal["catalog_version"] = goal["catalog_version"].replace("-v1", "-recovered-v1")
    expected["catalog_version"] = expected["catalog_version"].replace("-v1", "-recovered-v1")
    # Recovery must be visible to the worker even when this Goal's prompt never
    # used the old "Call claim_task once." phrasing, which was the only text the
    # previous implementation patched.
    recovery_note = (
        " Your Task input carries `prior_evidence`: probes already observed by "
        "an earlier run that died before filing. Treat them as leads, not ground "
        "truth - re-fetch what you rely on, then file. Start from them instead "
        "of re-discovering the target."
    )
    if "prior_evidence" not in goal["worker_prompt"]:
        goal["worker_prompt"] = goal["worker_prompt"].replace(
            "Call claim_task once.", "Call claim_task once." + recovery_note
        )
    if "prior_evidence" not in goal["worker_prompt"]:
        goal["worker_prompt"] += recovery_note
    expected["recovered_from"] = {"run_dir": run_dir.name, "summary": summary}
    # The private payload's digest of the PUBLIC goal must be recomputed.
    goal_bytes = json.dumps(goal, indent=2, ensure_ascii=False).encode() + b"\n"
    expected["config_sha256"] = sha256(goal_bytes)
    expected["fixture_digest"] = sha256(canonical(goal["tasks"]))
    goal_path.write_text(goal_bytes.decode())
    out_path.write_text(json.dumps(expected, indent=2, ensure_ascii=False) + "\n")
    return {
        "recovered_tasks": sum(1 for t in recovered.values() if t),
        "total_observations": sum(len(t) for t in recovered.values()),
        "with_conclusions": sum(1 for t in summary.values() if t["conclusions"]),
        "config_sha256": expected["config_sha256"],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-dir", required=True, type=Path)
    parser.add_argument("--expected", required=True, type=Path)
    parser.add_argument("--goal", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--seed", required=True, type=int)
    args = parser.parse_args()
    try:
        result = recover(args.run_dir, args.expected, args.seed, args.goal, args.out)
    except (OSError, ValueError) as error:
        parser.exit(2, f"recover_run: {error}\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
