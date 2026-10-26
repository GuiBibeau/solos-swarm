#!/usr/bin/env python3
"""Fail-closed promotion of the exact artifact covered by a protected attestation."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import time

import bundle

MANDATORY = {"registration-over-ceiling", "claim-race", "denied-provider-receiver", "unknown-outcome", "missing-usage", "pause-stop", "module-unload", "finding-retraction", "verify-no-replay", "missing-receipt", "missing-event", "empty-selection", "setup-failure", "explain-revision"}


class GateError(Exception):
    def __init__(self, reason, verdict="FAIL"):
        super().__init__(reason)
        self.verdict = verdict


def validate_report(report, profile, archive_sha, now):
    if not report.get("checks") or not report.get("scenarios"):
        raise GateError("Missing required evidence or empty selection", "INCONCLUSIVE")
    if profile.get("agent_count") != 10 or not MANDATORY.issubset(set(profile.get("required_scenarios", []))):
        raise GateError("Protected release profile omits mandatory controls")
    if report.get("bundle_sha256") != archive_sha or report.get("scope") != profile.get("scope"):
        raise GateError("Report covers another artifact or scope")
    if any(report.get(name) != "PASS" for name in ("runtime_verdict", "goal_verdict", "evidence_verdict", "cleanup_verdict")):
        raise GateError("A required result dimension did not pass")
    participants = report.get("participating_agent_ids", [])
    if report.get("agent_count") != 10 or len(participants) != 10 or len(set(participants)) != 10 or report.get("live") is not True:
        raise GateError("Ten distinct useful live agents are required")
    if set(report.get("useful_agent_ids", [])) != set(participants):
        raise GateError("Independent Goal checker must identify every useful participant", "INCONCLUSIVE")
    if report.get("provider_available") is not True or report.get("cleanup_confirmed") is not True or report.get("held_microusd") != 0:
        raise GateError("Provider, settlement or resource shutdown unconfirmed", "INCONCLUSIVE")
    nodes = report.get("independent_node_ids", [])
    if not isinstance(profile.get("min_nodes"), int) or profile["min_nodes"] < 1 or len(set(nodes)) < profile["min_nodes"]:
        raise GateError("Declared topology is unavailable", "INCONCLUSIVE")
    if profile["min_nodes"] > 1:
        inventory = profile.get("independent_node_inventory", {})
        identities = report.get("node_hardware_identities", {})
        approved = [identities[node] for node in set(nodes)
                    if node in identities and inventory.get(node) == identities[node]]
        if len(approved) < profile["min_nodes"] or len(set(approved)) < profile["min_nodes"]:
            raise GateError("Replica labels cannot substitute for protected independent-host inventory", "INCONCLUSIVE")
    if report.get("provider") not in profile.get("allowed_providers", []):
        raise GateError("Provider outside protected release profile")
    for key in ("limits_digest", "price_policy_digest", "catalog_version", "verifier_version", "isolation_profile"):
        if not profile.get(key) or report.get(key) != profile[key]:
            raise GateError("Frozen configuration/version mismatch: " + key)
    maximum_age = profile.get("max_age_seconds")
    timestamp = report.get("completed_unix")
    if not isinstance(maximum_age, int) or not 1 <= maximum_age <= 86400 or not isinstance(timestamp, (int, float)) or not now - maximum_age <= timestamp <= now + 60:
        raise GateError("Stale or missing completion time")
    if any(check.get("verdict") != "PASS" for check in report["checks"]):
        raise GateError("A required independent check failed or was skipped")
    observations = {entry["id"]: entry for entry in report["scenarios"]}
    if len(observations) != len(report["scenarios"]) or any(entry.get("verdict") != "PASS" for entry in report["scenarios"]):
        raise GateError("Duplicate, failed, skipped or inconclusive scenario evidence")
    for required in profile["required_scenarios"]:
        observed = observations.get(required)
        if not observed or observed.get("verdict") != "PASS" or not observed.get("evidence_refs"):
            raise GateError("Required scenario lacks passing independent evidence: " + required, "INCONCLUSIVE")


def qualify(args):
    bundle.verify(args.bundle)
    archive_sha = bundle.sha(args.bundle)
    report = json.loads(args.report.read_text())
    profile = json.loads(args.profile.read_text())
    if profile.get("verifier_sha256") != bundle.sha(args.verifier):
        raise GateError("Verifier differs from the protected compiled verifier identity")
    validate_report(report, profile, archive_sha, time.time())
    signed = json.loads(args.attestation.read_text())
    payload = signed["payload"]
    expected = {"bundle_sha256": archive_sha, "report_sha256": bundle.sha(args.report), "profile_sha256": bundle.sha(args.profile), "scope": profile["scope"]}
    if any(payload.get(key) != value for key, value in expected.items()):
        raise GateError("Attestation binds different artifacts, evidence or protected profile")
    result = subprocess.run([str(args.verifier), "mvp", "verify-signature", "--input", str(args.attestation),
                             "--trusted-key", str(args.trusted_key), "--domain", "release-qualification"], capture_output=True, text=True)
    if result.returncode:
        raise GateError("Protected attestation signature verification failed")
    output = {"verdict": "PASS", **expected, "artifact_promotion": "not-requested"}
    if args.promote:
        args.promote.mkdir(parents=True, exist_ok=False)
        promoted = args.promote / args.bundle.name
        shutil.copyfile(args.bundle, promoted)
        if bundle.sha(promoted) != archive_sha:
            raise GateError("Promoted bytes differ from the qualified artifact")
        shutil.copyfile(args.attestation, args.promote / "release-attestation.json")
        (args.promote / "qualification.json").write_text(json.dumps(output, indent=2) + "\n")
        output["artifact_promotion"] = str(promoted.resolve())
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("bundle", "report", "profile", "attestation", "trusted-key", "verifier"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--promote", type=Path)
    args = parser.parse_args()
    try:
        print(json.dumps(qualify(args), indent=2))
    except GateError as error:
        print(json.dumps({"verdict": error.verdict, "reason": str(error)}))
        raise SystemExit(1 if error.verdict == "FAIL" else 2)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(json.dumps({"verdict": "INCONCLUSIVE", "reason": str(error)}))
        raise SystemExit(2)


if __name__ == "__main__":
    main()
