#!/usr/bin/env python3
"""Independently score retained ten-agent live evidence without replaying work."""

import argparse
import hashlib
import json
from pathlib import Path


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def normalized(result):
    if not isinstance(result, dict) or set(result) != {"inconsistencies"}:
        raise ValueError("Result must contain only an inconsistencies array")
    rows = result["inconsistencies"]
    if not isinstance(rows, list):
        raise ValueError("inconsistencies must be an array")
    seen = set()
    for row in rows:
        if not isinstance(row, dict) or set(row) != {"id", "source_a", "source_b"}:
            raise ValueError("Malformed inconsistency record")
        if not isinstance(row["id"], str) or not row["id"] or row["id"] in seen:
            raise ValueError("Duplicate or invalid record ID")
        if type(row["source_a"]) is not int or type(row["source_b"]) is not int:
            raise ValueError("Record source values must be integers")
        seen.add(row["id"])
    return sorted(rows, key=lambda row: row["id"])


def verify_events(events):
    if not events:
        raise ValueError("Empty event log")
    previous = "0" * 64
    for index, event in enumerate(events, 1):
        if event.get("seq") != index or event.get("prev_hash") != previous:
            raise ValueError("Event order or predecessor does not match")
        if type(event.get("time_ms")) is not int or not isinstance(event.get("data"), dict):
            raise ValueError("Malformed event payload")
        payload = {key: value for key, value in event.items() if key not in {"prev_hash", "hash"}}
        observed = hashlib.sha256(previous.encode() + canonical(payload)).hexdigest()
        if observed != event.get("hash"):
            raise ValueError("Event digest does not match")
        previous = observed
    return previous


def cache_hits(usage, total):
    if not isinstance(usage, dict) or type(total) is not int or total < 0:
        raise ValueError("Invalid input usage")

    def counter(container, name):
        if name not in container:
            return None
        value = container[name]
        if type(value) is not int or value < 0:
            raise ValueError("Invalid cache usage")
        return value

    hit = counter(usage, "prompt_cache_hit_tokens")
    details = usage.get("prompt_tokens_details")
    if details is not None and not isinstance(details, dict):
        raise ValueError("Invalid prompt token details")
    nested = counter(details or {}, "cached_tokens")
    miss = counter(usage, "prompt_cache_miss_tokens")
    if hit is not None and nested is not None and hit != nested:
        raise ValueError("Conflicting cache usage")
    if miss is not None and miss > total:
        raise ValueError("Cache miss usage exceeds input")
    cached = hit if hit is not None else nested if nested is not None else total - miss if miss is not None else 0
    if cached > total or (miss is not None and miss != total - cached):
        raise ValueError("Cache usage does not match input")
    return cached


def receipt_cost(config, input_tokens, cached, output_tokens):
    values = [input_tokens, cached, output_tokens]
    if not all(type(value) is int and value >= 0 for value in values) or cached > input_tokens:
        raise ValueError("Invalid token usage")
    miss_rate = config.get("input_price_microusd_per_million")
    output_rate = config.get("output_price_microusd_per_million")
    hit_rate = config.get("cache_hit_price_microusd_per_million")
    if hit_rate is None:
        hit_rate = miss_rate
    if not (type(miss_rate) is int and 300_000 <= miss_rate <= 10_000_000
            and type(output_rate) is int and 1_200_000 <= output_rate <= 10_000_000
            and type(hit_rate) is int and 6_000 <= hit_rate <= miss_rate):
        raise ValueError("Invalid admitted prices")
    return (((input_tokens - cached) * miss_rate + cached * hit_rate + 999_999) // 1_000_000
            + (output_tokens * output_rate + 999_999) // 1_000_000)


def event_evidence(events, state, config):
    by_kind = {}
    for event in events:
        by_kind.setdefault(event["kind"], []).append(event)

    def matching(kind, **fields):
        return [event for event in by_kind.get(kind, []) if all(event["data"].get(key) == value for key, value in fields.items())]

    issues = []
    if not matching("goal_registered", config_sha256=config["config_sha256"]):
        issues.append("Goal registration missing or has wrong config digest")
    for identity in state["agents"]:
        if len(matching("agent_started", agent_id=identity)) != 1 or len(matching("agent_stopped", agent_id=identity)) != 1:
            issues.append(f"Missing or duplicate lifecycle events for {identity}")
    for intent in state["intents"]:
        if intent.get("state") != "completed":
            continue
        fields = {
            "provider_reserved": {"intent_id": intent["id"], "agent_id": intent["agent_id"], "hold_microusd": intent["hold_microusd"]},
            "provider_request": {"intent_id": intent["id"], "agent_id": intent["agent_id"]},
            "provider_dispatched": {"intent_id": intent["id"]},
            "provider_settled": {"intent_id": intent["id"], "cost_microusd": intent["cost_microusd"], "usage": intent["usage"], "request_id": intent["request_id"]},
            "provider_response": {"intent_id": intent["id"]},
        }
        sequence = [matching(kind, **data) for kind, data in fields.items()]
        if not all(len(items) == 1 for items in sequence):
            issues.append(f"Incomplete provider lifecycle for {intent['id']}")
            continue
        if [items[0]["seq"] for items in sequence] != sorted(items[0]["seq"] for items in sequence):
            issues.append(f"Provider lifecycle out of order for {intent['id']}")
        request, response = sequence[1][0]["data"].get("body", {}), sequence[4][0]["data"].get("body", {})
        try:
            usage = intent["usage"]
            total, output = usage["input_tokens"], usage["output_tokens"]
            if not isinstance(request, dict) or not isinstance(response, dict):
                raise ValueError("Malformed provider evidence")
            raw_usage = response.get("usage", {})
            cached = cache_hits(raw_usage, total)
            if (request.get("model") != config["model"] or response.get("id") != intent["request_id"]
                    or type(raw_usage.get("prompt_tokens")) is not int or raw_usage["prompt_tokens"] != total
                    or type(raw_usage.get("completion_tokens")) is not int or raw_usage["completion_tokens"] != output):
                raise ValueError("Provider receipt or usage disagrees")
            for name, expected in [("cached_input_tokens", cached), ("uncached_input_tokens", total - cached)]:
                if name in usage and (type(usage[name]) is not int or usage[name] != expected):
                    raise ValueError("Recorded cache usage disagrees with receipt")
            if config.get("cache_hit_price_microusd_per_million") is not None and cached and "cached_input_tokens" not in usage:
                raise ValueError("Discounted receipt omits cache usage")
            if intent["cost_microusd"] != receipt_cost(config, total, cached, output):
                raise ValueError("Recorded charge disagrees with receipt and prices")
        except (KeyError, TypeError, ValueError) as error:
            issues.append(f"Invalid settlement for {intent['id']}: {error}")
    for finding in state["findings"]:
        tools = matching("tool_completed", agent_id=finding.get("agent_id"))
        if not any(event["data"].get("tool") == "claim_task" and event["data"].get("result", {}).get("id") == finding.get("task_id") for event in tools):
            issues.append("Missing authoritative Task claim event")
        if not any(event["data"].get("tool") == "submit_finding" and event["data"].get("args") == {"task_id": finding.get("task_id"), "result": finding.get("result")} for event in tools):
            issues.append("Missing authoritative Finding event")
    tools = by_kind.get("tool_completed", [])
    if not any(event["data"].get("tool") == "board_post" for event in tools):
        issues.append("No brokered Board activity")
    synthesis = [event for event in tools if event["data"].get("agent_id") == "agent-00" and event["data"].get("tool") == "submit_artifact" and event["data"].get("args") == {"artifact": state.get("artifact")}]
    reads = [event for event in tools if event["data"].get("agent_id") == "agent-00" and event["data"].get("tool") == "read_findings" and event["data"].get("result", {}).get("findings") == state["findings"]]
    if len(synthesis) != 1 or not any(read["seq"] < synthesis[0]["seq"] for read in reads):
        issues.append("Coordinator must read all Findings before one recorded synthesis")
    return issues


def validate_document(name, value):
    if not isinstance(value, dict):
        raise ValueError(f"{name} must contain a JSON object")
    if name == "state.json":
        if not isinstance(value.get("agents"), dict) or not all(isinstance(agent, dict) for agent in value["agents"].values()):
            raise ValueError("Malformed agent records")
        for collection in ("tasks", "findings", "intents"):
            if not isinstance(value.get(collection), list) or not all(isinstance(item, dict) for item in value[collection]):
                raise ValueError(f"Malformed {collection} records")
        if not all(intent.get("usage") is None or isinstance(intent.get("usage"), dict) for intent in value["intents"]):
            raise ValueError("Malformed usage evidence")
    if name == "config.yaml":
        if type(value.get("budget_microusd")) is not int or value["budget_microusd"] <= 0:
            raise ValueError("Missing finite Goal spend cap")
        if not isinstance(value.get("tasks"), list):
            raise ValueError("Missing Goal tasks")
    return value


def check_run(run_dir, expected_path, runtime=None, extension=None, package_lock=None):
    run_dir = Path(run_dir).resolve()
    checks = []

    def check(name, dimension, condition, reason, missing=False):
        verdict = "PASS" if condition else "INCONCLUSIVE" if missing else "FAIL"
        checks.append({"check": name, "dimension": dimension, "verdict": verdict, "reason": reason})

    def read(name, dimension="evidence"):
        path = run_dir / name
        try:
            if not path.is_file():
                raise FileNotFoundError(name)
            result = validate_document(name, json.loads(path.read_text()))
            check(f"read:{name}", dimension, True, "Required evidence loaded")
            return result
        except (OSError, ValueError) as error:
            check(f"read:{name}", dimension, False, str(error), missing=True)
            return None

    manifest = read("manifest.json")
    state = read("state.json")
    runtime_report = read("runtime-report.json", "runtime")
    cleanup = read("cleanup.json", "cleanup")
    artifact = read("artifacts/final.json", "goal")
    config = read("config.yaml")  # prepare_case emits JSON, a valid YAML subset.
    try:
        expected = validate_document("expected.json", json.loads(Path(expected_path).read_text()))
        if not isinstance(expected.get("tasks"), dict) or not isinstance(expected.get("artifact"), dict):
            raise ValueError("Malformed private evaluator payload")
        check("private-evaluator", "evidence", True, "Protected evaluator payload loaded")
    except (OSError, ValueError) as error:
        expected = None
        check("private-evaluator", "evidence", False, str(error), missing=True)

    try:
        events = [json.loads(line) for line in (run_dir / "events.jsonl").read_text().splitlines()]
        event_head = verify_events(events)
        check("event-chain", "evidence", True, f"Verified {len(events)} entries; head={event_head}")
    except (OSError, ValueError, TypeError, AttributeError) as error:
        events = None
        check("event-chain", "evidence", False, str(error), missing=isinstance(error, OSError))

    if manifest is not None:
        check("live-provider", "runtime", manifest.get("live") is True and manifest.get("provider") == "deepseek", "A fixture or replay run cannot qualify")
        check("single-host-profile", "evidence", manifest.get("topology") == "single-host", "This checker qualifies only the single-host preview")
        check("isolation-profile", "evidence", manifest.get("isolation") == "bubblewrap-unshare-all", "Required tested isolation profile recorded")
        check("pinned-pi", "evidence", manifest.get("pi_version") == "1.1.0", "Expected Pi 1.1.0")
        for name, path in (("runtime", runtime), ("extension", extension), ("package_lock", package_lock)):
            try:
                if path is None:
                    raise FileNotFoundError(f"Missing --{name.replace('_', '-')}")
                check(f"digest:{name}", "evidence", digest(path) == manifest.get(f"{name}_sha256"), "Candidate digest must match frozen manifest")
            except OSError as error:
                check(f"digest:{name}", "evidence", False, str(error), missing=True)
        artifacts = manifest.get("artifacts", {})
        check("required-artifact-digests", "evidence", isinstance(artifacts, dict) and "config.yaml" in artifacts, "Manifest must bind the public Goal")
        if isinstance(artifacts, dict):
            for name, recorded in artifacts.items():
                try:
                    path = (run_dir / name).resolve()
                    if not path.is_relative_to(run_dir):
                        raise ValueError("Artifact path escapes run directory")
                    check(f"digest:{name}", "evidence", digest(path) == recorded, "Artifact digest must match manifest")
                except (OSError, ValueError) as error:
                    check(f"digest:{name}", "evidence", False, str(error), missing=isinstance(error, OSError))

    if config is not None and expected is not None:
        check("frozen-goal", "evidence", digest(run_dir / "config.yaml") == expected.get("config_sha256"), "Goal must match the independently frozen case")
        tasks = config.get("tasks", [])
        check("frozen-fixture", "evidence", hashlib.sha256(canonical(tasks)).hexdigest() == expected.get("fixture_digest"), "Fixture must match the protected scorer")
        check("ten-task-case", "goal", len(tasks) == 10 and len(expected.get("tasks", {})) == 10, "Exactly ten required tasks")
        for name in ("seed", "catalog_version", "scorer_version"):
            check(f"version:{name}", "evidence", config.get(name) == expected.get(name) and (manifest or {}).get(name) == expected.get(name), "Case, manifest and scorer versions must agree")
        check("allowlisted-model", "runtime", (manifest or {}).get("model") == config.get("model") == "deepseek-flash", "Expected explicitly allowlisted hosted model")

    if runtime_report is not None:
        check("runtime-outcome", "runtime", runtime_report.get("runtime_verdict") == "PASS", str(runtime_report.get("reason", "Missing runtime result")))
    if cleanup is not None:
        check("workers-stopped", "cleanup", cleanup.get("verdict") == "PASS" and cleanup.get("remaining_workers") == 0, "Cleanup must confirm no remaining run-owned workers")

    if state is not None:
        agents, intents, findings = state.get("agents", {}), state.get("intents", []), state.get("findings", [])
        identities = {f"agent-{index:02d}" for index in range(10)}
        check("ten-identities", "runtime", isinstance(agents, dict) and set(agents) == identities, "Ten distinct admitted identities required")
        check("goal-completed", "runtime", state.get("goal_status") == "completed", "Goal must have completed")
        check("settled-ledger", "evidence", state.get("held_microusd") == 0 and all(intent.get("state") in {"completed", "denied"} for intent in intents), "Unknown or unsettled holds block qualification")
        completed_intents = [intent for intent in intents if intent.get("state") == "completed"]
        receipt_ids = [intent.get("request_id") for intent in completed_intents]
        receipts = all(isinstance(receipt, str) and receipt for receipt in receipt_ids)
        check("nonreplayed-receipts", "runtime", receipts and len(set(receipt_ids)) == len(receipt_ids), "Each completed inference must have a unique upstream receipt")
        intent_ids = [intent.get("id") for intent in intents]
        intent_shape = all(isinstance(intent_id, str) and intent_id for intent_id in intent_ids)
        check("intent-identities", "evidence", intent_shape and len(set(intent_ids)) == len(intent_ids) and all(intent.get("agent_id") in identities for intent in intents), "Unique effect IDs must belong to admitted agents")
        costs = [intent.get("cost_microusd") for intent in completed_intents]
        cost_evidence = all(type(cost) is int and cost >= 0 for cost in costs)
        check("ledger-total", "evidence", cost_evidence and sum(costs) == state.get("spent_microusd"), "Settled intent costs must sum to recorded spend")
        if config is not None:
            spent = state.get("spent_microusd")
            check("spend-cap", "runtime", type(spent) is int and 0 <= spent <= config.get("budget_microusd", 0), "Recorded spend must remain inside the registered cap")
        intervals = []
        for identity in sorted(identities):
            agent = agents.get(identity, {}) if isinstance(agents, dict) else {}
            completed = [intent for intent in intents if intent.get("agent_id") == identity and intent.get("state") == "completed" and intent.get("provider") == "deepseek"]
            real = [intent for intent in completed if intent.get("request_id") and isinstance(intent.get("usage"), dict) and type(intent["usage"].get("input_tokens")) is int and intent["usage"]["input_tokens"] > 0 and type(intent["usage"].get("output_tokens")) is int and intent["usage"]["output_tokens"] > 0]
            successful = agent.get("successful_model_calls")
            check(f"inference:{identity}", "runtime", bool(real) and type(successful) is int and successful == len(completed), "Real completed inference with consistent count, receipt and usage required")
            if config is not None:
                calls = agent.get("model_calls")
                identity_intents = [intent for intent in intents if intent.get("agent_id") == identity]
                maximum = config.get("max_calls_per_agent", 0)
                check(f"turn-cap:{identity}", "runtime", type(calls) is int and type(maximum) is int and len(completed) <= calls <= maximum and calls == len(identity_intents), "Agent call count must match recorded intents and admitted turn ceiling")
            started, stopped = agent.get("started_ms"), agent.get("stopped_ms")
            lifetime = type(started) is int and type(stopped) is int and stopped > started
            check(f"lifetime:{identity}", "cleanup", lifetime, "Recorded process start and termination required")
            if lifetime:
                intervals.append((started, stopped))
            useful = []
            if expected is not None:
                for finding in findings:
                    if finding.get("agent_id") != identity:
                        continue
                    task_id = finding.get("task_id")
                    try:
                        if task_id in expected["tasks"] and normalized(finding.get("result")) == normalized(expected["tasks"][task_id]):
                            useful.append(task_id)
                    except (ValueError, TypeError, KeyError):
                        pass
            check(f"contribution:{identity}", "goal", bool(useful), "An independently correct attributed shard result is required", missing=expected is None)
        check("overlapping-lifetimes", "runtime", len(intervals) == 10 and max(start for start, _ in intervals) < min(stop for _, stop in intervals), "All ten agent lifetimes must overlap")
        task_ids = [finding.get("task_id") for finding in findings]
        scalar_task_ids = all(isinstance(task_id, str) for task_id in task_ids)
        check("one-finding-per-task", "goal", scalar_task_ids and len(task_ids) == 10 and len(set(task_ids)) == 10 and set(task_ids) == set((expected or {}).get("tasks", {})), "All ten distinct tasks must contribute exactly once", missing=expected is None)
        claims = {task.get("id"): task.get("claimed_by") for task in state["tasks"] if isinstance(task.get("id"), str)}
        check("claim-attribution", "runtime", all(isinstance(finding.get("task_id"), str) and claims.get(finding["task_id"]) == finding.get("agent_id") for finding in findings), "Finding producers must be the authoritative Task claimants")
        if artifact is not None:
            check("artifact-state-reference", "evidence", state.get("artifact") == artifact, "Persisted artifact must equal the retained final artifact")
        if events is not None and config is not None:
            try:
                issues = event_evidence(events, state, {**config, "config_sha256": digest(run_dir / "config.yaml")})
                check("event-completeness", "evidence", not issues, "; ".join(issues) or "All provider, lifecycle, Task, Finding and synthesis effects have evidence")
            except (KeyError, TypeError, ValueError, AttributeError) as error:
                check("event-completeness", "evidence", False, f"Malformed effect evidence: {error}")

    if artifact is not None and expected is not None:
        try:
            matches = normalized(artifact) == normalized(expected["artifact"])
            check("independent-artifact-score", "goal", matches, "Consolidated inconsistencies compared with private expected set")
        except (ValueError, TypeError, KeyError) as error:
            check("independent-artifact-score", "goal", False, str(error))

    dimensions = {}
    for dimension in ("runtime", "goal", "evidence", "cleanup"):
        selected = [item["verdict"] for item in checks if item["dimension"] == dimension]
        dimensions[f"{dimension}_verdict"] = "FAIL" if "FAIL" in selected else "INCONCLUSIVE" if not selected or "INCONCLUSIVE" in selected else "PASS"
    verdicts = list(dimensions.values())
    verdict = "FAIL" if "FAIL" in verdicts else "INCONCLUSIVE" if "INCONCLUSIVE" in verdicts else "PASS"
    return {
        "schema_version": 1,
        "verdict": verdict,
        **dimensions,
        "run_id": (manifest or {}).get("run_id", run_dir.name),
        "scorer_id": (expected or {}).get("scorer_id"),
        "scorer_version": (expected or {}).get("scorer_version"),
        "seed": (expected or {}).get("seed"),
        "checks": checks,
        "coverage": "single-host-ten-agent-live-preview",
        "deferred": ["500-agent/three-node acceptance", "Raft/QUIC and HA", "local-provider/air-gap", "full Tier P security", "signed audit witness", "three-family rotation"],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-dir", required=True, type=Path)
    parser.add_argument("--expected", required=True, type=Path)
    parser.add_argument("--runtime", type=Path)
    parser.add_argument("--extension", type=Path)
    parser.add_argument("--package-lock", type=Path)
    args = parser.parse_args()
    report = check_run(args.run_dir, args.expected, args.runtime, args.extension, args.package_lock)
    args.run_dir.mkdir(parents=True, exist_ok=True)
    (args.run_dir / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    lines = [f"# Run {report['run_id']}: {report['verdict']}", "", "Scope: single-host ten-agent live preview.", ""]
    lines.extend(f"- {name}: {report[name]}" for name in ("runtime_verdict", "goal_verdict", "evidence_verdict", "cleanup_verdict"))
    lines.extend(["", "Failed or missing checks:", ""])
    lines.extend(f"- {item['check']}: {item['verdict']} — {item['reason']}" for item in report["checks"] if item["verdict"] != "PASS")
    lines.extend(["", "Deferred coverage: " + "; ".join(report["deferred"]) + ".", ""])
    (args.run_dir / "summary.md").write_text("\n".join(lines))
    print(json.dumps(report))
    return {"PASS": 0, "FAIL": 1, "INCONCLUSIVE": 2}[report["verdict"]]


if __name__ == "__main__":
    raise SystemExit(main())
