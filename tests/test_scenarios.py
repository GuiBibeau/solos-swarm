import hashlib
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def load_script(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


prepare = load_script("prepare_case")
checker = load_script("check_run")


class ScenarioTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.run = self.root / "public" / "test-run"
        self.run.mkdir(parents=True)
        self.expected_path = self.root / "private" / "expected.json"
        self.expected = prepare.write_case(self.run / "config.yaml", self.expected_path, 42)
        self.goal = json.loads((self.run / "config.yaml").read_text())
        self.bundle = self.root / "bundle"
        self.bundle.mkdir()
        self.runtime = self.bundle / "swarmd"
        self.extension = self.bundle / "broker.ts"
        self.lock = self.bundle / "package-lock.json"
        for path in (self.runtime, self.extension, self.lock):
            path.write_text("immutable test candidate " + path.name)
        agents, intents, findings, tasks = {}, [], [], []
        for index, task in enumerate(self.goal["tasks"]):
            identity = f"agent-{index:02d}"
            agents[identity] = {"started_ms": 1000 + index, "stopped_ms": 2000 + index, "successful_model_calls": 1, "model_calls": 1, "status": "stopped"}
            intents.append({"id": f"intent-{index:02d}", "agent_id": identity, "provider": "deepseek", "state": "completed", "usage": {"input_tokens": 100, "output_tokens": 30}, "request_id": f"test-receipt-{index}", "hold_microusd": 1000, "cost_microusd": 66})
            findings.append({"agent_id": identity, "task_id": task["id"], "result": self.expected["tasks"][task["id"]]})
            tasks.append({**task, "claimed_by": identity, "status": "done"})
        self.state = {"schema_version": 1, "goal_status": "completed", "agents": agents, "tasks": tasks, "findings": findings, "intents": intents, "spent_microusd": 660, "held_microusd": 0, "artifact": self.expected["artifact"]}
        self.write("state.json", self.state)
        self.write("artifacts/final.json", self.expected["artifact"])
        self.write("runtime-report.json", {"runtime_verdict": "PASS", "reason": "Test evidence", "stopped_ms": 2010})
        self.write("cleanup.json", {"verdict": "PASS", "remaining_workers": 0, "stopped_ms": 2010})
        self.manifest = {"schema_version": 1, "run_id": "test-run", "live": True, "provider": "deepseek", "model": "deepseek-flash", "topology": "single-host", "isolation": "bubblewrap-unshare-all", "pi_version": "1.1.0", "runtime_sha256": checker.digest(self.runtime), "extension_sha256": checker.digest(self.extension), "package_lock_sha256": checker.digest(self.lock), "artifacts": {"config.yaml": checker.digest(self.run / "config.yaml")}, "seed": 42, "catalog_version": self.goal["catalog_version"], "scorer_version": "1"}
        self.write("manifest.json", self.manifest)
        self.events = [{"kind": "goal_registered", "data": {"intent": self.goal["intent"], "config_sha256": self.expected["config_sha256"]}}]
        self.events.extend({"kind": "agent_started", "data": {"agent_id": identity}} for identity in agents)
        for intent, finding, task in zip(intents, findings, tasks):
            intent_id, identity = intent["id"], intent["agent_id"]
            self.events.extend([
                {"kind": "provider_reserved", "data": {"intent_id": intent_id, "agent_id": identity, "hold_microusd": intent["hold_microusd"]}},
                {"kind": "provider_request", "data": {"intent_id": intent_id, "agent_id": identity, "body": {"model": "deepseek-flash"}}},
                {"kind": "provider_dispatched", "data": {"intent_id": intent_id}},
                {"kind": "provider_settled", "data": {"intent_id": intent_id, "cost_microusd": intent["cost_microusd"], "usage": intent["usage"], "request_id": intent["request_id"]}},
                {"kind": "provider_response", "data": {"intent_id": intent_id, "body": {"id": intent["request_id"], "usage": {"prompt_tokens": 100, "completion_tokens": 30}}}},
                {"kind": "tool_completed", "data": {"agent_id": identity, "tool": "claim_task", "args": {}, "result": {"id": task["id"], "input": task["input"]}}},
                {"kind": "tool_completed", "data": {"agent_id": identity, "tool": "submit_finding", "args": {"task_id": finding["task_id"], "result": finding["result"]}, "result": {"accepted": True}}},
            ])
        self.events.extend([
            {"kind": "tool_completed", "data": {"agent_id": "agent-00", "tool": "board_post", "args": {"message": "Ten Findings ready"}, "result": {}}},
            {"kind": "tool_completed", "data": {"agent_id": "agent-00", "tool": "read_findings", "args": {}, "result": {"findings": findings}}},
            {"kind": "tool_completed", "data": {"agent_id": "agent-00", "tool": "submit_artifact", "args": {"artifact": self.expected["artifact"]}, "result": {"accepted": True}}},
        ])
        self.events.extend({"kind": "agent_stopped", "data": {"agent_id": identity}} for identity in agents)
        self.write_events()

    def write_events(self):
        previous = "0" * 64
        lines = []
        for seq, raw in enumerate(self.events, 1):
            event = {**raw, "seq": seq, "time_ms": 1000 + seq}
            event["hash"] = hashlib.sha256(previous.encode() + checker.canonical(event)).hexdigest()
            event["prev_hash"] = previous
            previous = event["hash"]
            lines.append(json.dumps(event, ensure_ascii=False))
        (self.run / "events.jsonl").write_text("\n".join(lines) + "\n")

    def write(self, name, value):
        path = self.run / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value) + "\n")

    def report(self):
        return checker.check_run(self.run, self.expected_path, self.runtime, self.extension, self.lock)

    def test_case_is_reproducible_and_each_shard_needs_useful_work(self):
        goal, expected = prepare.make_case(42)
        self.assertEqual(goal, self.goal)
        self.assertEqual(expected["fixture_digest"], self.expected["fixture_digest"])
        self.assertTrue(all(result["inconsistencies"] for result in expected["tasks"].values()))
        self.assertNotEqual(prepare.make_case(43)[1]["fixture_digest"], expected["fixture_digest"])
        with self.assertRaises(ValueError):
            prepare.make_case(-1)

    def test_private_expected_cannot_be_written_into_public_fixture_directory(self):
        with self.assertRaises(ValueError):
            prepare.write_case(self.run / "other.yaml", self.run / "expected.json", 42)

    def test_frozen_case_is_never_overwritten(self):
        with self.assertRaises(FileExistsError):
            prepare.write_case(self.run / "config.yaml", self.expected_path, 43)

    def test_complete_structural_evidence_passes(self):
        # Synthetic unit evidence validates the scorer, not a live gate invocation.
        self.assertEqual(self.report()["verdict"], "PASS")
        self.assertEqual(self.report()["coverage"], "single-host-1-30-agent-live-preview")

    def test_artifact_order_is_irrelevant(self):
        artifact = {"inconsistencies": list(reversed(self.expected["artifact"]["inconsistencies"]))}
        self.state["artifact"] = artifact
        self.write("state.json", self.state)
        self.write("artifacts/final.json", artifact)
        self.assertEqual(self.report()["goal_verdict"], "PASS")

    def test_wrong_answer_fails(self):
        artifact = {"inconsistencies": self.expected["artifact"]["inconsistencies"][:-1]}
        self.state["artifact"] = artifact
        self.write("state.json", self.state)
        self.write("artifacts/final.json", artifact)
        self.assertEqual(self.report()["goal_verdict"], "FAIL")

    def test_idle_agents_do_not_count(self):
        self.state["findings"] = [self.state["findings"][0]]
        self.write("state.json", self.state)
        self.assertEqual(self.report()["goal_verdict"], "FAIL")

    def test_successful_model_receipt_is_required_per_identity(self):
        self.state["intents"][-1]["request_id"] = None
        self.write("state.json", self.state)
        self.assertEqual(self.report()["runtime_verdict"], "FAIL")

    def test_reused_upstream_receipt_cannot_count_as_independent_call(self):
        self.state["intents"][-1]["request_id"] = self.state["intents"][0]["request_id"]
        self.write("state.json", self.state)
        self.assertEqual(self.report()["runtime_verdict"], "FAIL")

    def test_nonoverlapping_agents_fail(self):
        self.state["agents"]["agent-00"]["stopped_ms"] = 1001
        self.write("state.json", self.state)
        self.assertEqual(self.report()["runtime_verdict"], "FAIL")

    def test_uncertain_spend_blocks_qualification(self):
        self.state["intents"][0]["state"] = "unknown"
        self.state["held_microusd"] = 1000
        self.write("state.json", self.state)
        self.assertNotEqual(self.report()["verdict"], "PASS")

    def test_missing_cleanup_is_inconclusive(self):
        (self.run / "cleanup.json").unlink()
        self.assertEqual(self.report()["verdict"], "INCONCLUSIVE")

    def test_missing_artifact_is_inconclusive(self):
        (self.run / "artifacts/final.json").unlink()
        self.assertEqual(self.report()["verdict"], "INCONCLUSIVE")

    def test_missing_evaluator_is_inconclusive(self):
        self.expected_path.unlink()
        self.assertEqual(self.report()["verdict"], "INCONCLUSIVE")

    def test_malformed_state_is_inconclusive(self):
        self.write("state.json", {"agents": []})
        self.assertEqual(self.report()["verdict"], "INCONCLUSIVE")

    def test_wrong_claimant_fails(self):
        self.state["tasks"][0]["claimed_by"] = "agent-09"
        self.write("state.json", self.state)
        self.assertEqual(self.report()["runtime_verdict"], "FAIL")

    def test_wrong_settled_total_fails(self):
        self.state["spent_microusd"] = 1
        self.write("state.json", self.state)
        self.assertEqual(self.report()["evidence_verdict"], "FAIL")

    def test_altered_candidate_fails(self):
        self.runtime.write_text("altered")
        self.assertEqual(self.report()["evidence_verdict"], "FAIL")

    def test_replay_cannot_qualify(self):
        self.manifest["live"] = False
        self.write("manifest.json", self.manifest)
        self.assertEqual(self.report()["runtime_verdict"], "FAIL")

    def test_tampered_event_fails(self):
        path = self.run / "events.jsonl"
        lines = path.read_text().splitlines()
        event = json.loads(lines[0])
        event["kind"] = "changed"
        lines[0] = json.dumps(event)
        path.write_text("\n".join(lines) + "\n")
        self.assertEqual(self.report()["evidence_verdict"], "FAIL")

    def test_unicode_line_separators_inside_a_record_do_not_break_the_chain(self):
        # A fetched page can legally carry U+0085/U+2028/U+2029 inside a JSON
        # string. `str.splitlines()` treats them as record boundaries, which
        # corrupted a real run's event chain with "Unterminated string".
        self.events.insert(2, {
            "kind": "tool_completed",
            "data": {"agent_id": "agent-00", "tool": "board_post",
                     "args": {"message": "read A\u0085B C D"},
                     "result": {"seq": 1}},
        })
        self.write_events()
        self.assertEqual(self.report()["evidence_verdict"], "PASS")

    def test_omitted_event_fails_even_after_rehash(self):
        self.events = [event for event in self.events if event["kind"] != "provider_settled"]
        self.write_events()
        self.assertEqual(self.report()["evidence_verdict"], "FAIL")

    def test_empty_event_selection_cannot_pass(self):
        (self.run / "events.jsonl").write_text("")
        self.assertEqual(self.report()["evidence_verdict"], "FAIL")

    def test_duplicate_rows_and_boolean_values_are_rejected(self):
        row = self.expected["artifact"]["inconsistencies"][0]
        with self.assertRaises(ValueError):
            checker.normalized({"inconsistencies": [row, row]})
        with self.assertRaises(ValueError):
            checker.normalized({"inconsistencies": [{**row, "source_a": True}]})


if __name__ == "__main__":
    unittest.main()
