"""Recovery forking: evidence must follow the agent that actually claimed the Task."""

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


recover = load_script("recover_run")


def transcript(url, body):
    """One completed fetch plus one substantive conclusion, as Pi writes them."""
    start = {"type": "tool_execution_start", "toolCallId": "call-1", "toolName": "fetch_url",
             "args": {"url": url}}
    payload = {"outcome": "ok", "response": {"status": 200, "final_url": url, "body": body}}
    end = {"type": "tool_execution_end", "toolCallId": "call-1", "toolName": "fetch_url",
           "isError": False, "result": {"content": [{"type": "text", "text": json.dumps(payload)}]}}
    message = {"type": "message_end", "message": {"role": "assistant", "content": [
        {"type": "text", "text": "Read the theorem statement and its assumptions in full."}]}}
    return "\n".join(json.dumps(record, ensure_ascii=False) for record in (start, end, message)) + "\n"


class RecoverTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.run_dir = root / "run"
        self.run_dir.mkdir()
        # Claims raced: lens-04 belongs to agent-07, lens-07 to agent-14.
        state = {"schema_version": 1, "tasks": [
            {"id": "lens-04", "input": {}, "claimed_by": "agent-07", "status": "claimed"},
            {"id": "lens-07", "input": {}, "claimed_by": "agent-14", "status": "completed"},
        ]}
        (self.run_dir / "state.json").write_text(json.dumps(state))
        (self.run_dir / "agent-07.rpc.jsonl").write_text(
            transcript("https://example.test/lens-04", "A\u0085B\x7fC D keeps one record"))
        (self.run_dir / "agent-14.rpc.jsonl").write_text(
            transcript("https://example.test/lens-07", "C D keeps one record"))
        self.goal = {"schema_version": 1, "catalog_version": "case-v1", "worker_prompt": "Read and file.",
                     "synthesis_prompt": "Synthesize.", "tasks": [
                         {"id": "lens-04", "input": {}}, {"id": "lens-07", "input": {}}]}
        self.expected = {"schema_version": 1, "catalog_version": "case-v1", "config_sha256": "old",
                         "fixture_digest": "old", "tasks": {"lens-04": {"files": ["a"]}, "lens-07": {"files": ["b"]}}}
        self.goal_path = root / "goal.json"
        self.expected_path = root / "expected.json"
        self.out_path = root / "recovered.json"
        self.goal_path.write_text(json.dumps(self.goal, indent=2) + "\n")
        self.expected_path.write_text(json.dumps(self.expected, indent=2) + "\n")

    def test_evidence_follows_the_claiming_agent_not_the_task_index(self):
        result = recover.recover(self.run_dir, self.expected_path, 42, self.goal_path, self.out_path)
        self.assertEqual(result["recovered_tasks"], 2)
        self.assertEqual(result["total_observations"], 2)
        expected = json.loads(self.out_path.read_text())
        goal = json.loads(self.goal_path.read_text())
        self.assertEqual(expected["recovered_from"]["summary"]["lens-04"]["agent"], "agent-07")
        self.assertEqual(expected["recovered_from"]["summary"]["lens-07"]["agent"], "agent-14")
        self.assertEqual(expected["tasks"]["lens-04"]["recovered"]["observations"][0]["url"],
                         "https://example.test/lens-04")
        self.assertEqual(expected["tasks"]["lens-07"]["recovered"]["observations"][0]["url"],
                         "https://example.test/lens-07")
        self.assertEqual({task["id"]: task["input"]["prior_evidence"]["observations"][0]["url"] for task in goal["tasks"]},
                         {"lens-04": "https://example.test/lens-04",
                          "lens-07": "https://example.test/lens-07"})
        self.assertTrue(all(task["input"]["prior_evidence"]["conclusions"] for task in goal["tasks"]))
        self.assertIn("prior_evidence", goal["worker_prompt"])
        self.assertIn("recovered-v1", expected["catalog_version"])
        # The private payload must bind the bytes actually written to the Goal.
        self.assertEqual(expected["config_sha256"], hashlib.sha256(self.goal_path.read_bytes()).hexdigest())
        body = expected["tasks"]["lens-04"]["recovered"]["observations"][0]["body_excerpt"]
        # A fetched body may contain DEL, C1 and Unicode separators; JSON allows
        # them raw but the runtime parses the Goal as YAML, which does not.
        for control in ("\u0085", "\x7f", " "):
            self.assertNotIn(control, body)
        self.assertIn("A B C D keeps one record", body)
        self.assertNotIn("\x7f", self.goal_path.read_text())
        self.assertNotIn("\u0085", self.goal_path.read_text())

    def test_one_agent_claiming_two_tasks_fails_loudly(self):
        state = json.loads((self.run_dir / "state.json").read_text())
        for task in state["tasks"]:
            task["claimed_by"] = "agent-07"
        (self.run_dir / "state.json").write_text(json.dumps(state))
        with self.assertRaisesRegex(ValueError, "one agent claims more than one Task"):
            recover.recover(self.run_dir, self.expected_path, 42, self.goal_path, self.out_path)


if __name__ == "__main__":
    unittest.main()
