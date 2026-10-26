"""Progress summary reads only state.json/config.yaml; no SSH, no paid request."""

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

root = Path(__file__).parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name, root / f"scripts/{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


progress, remote = load("swarm_progress"), load("remote")


def finding(agent, severity, description=""):
    return {"agent_id": agent, "task_id": agent, "result": {"papers": [{"id": agent, "severity": severity, "description": description}]}}


def run_dir(directory):
    agent = lambda status, calls: {"status": status, "model_calls": calls, "run_code_calls": calls - 1, "terminal": None}
    state = {"goal_status": "running", "artifact": None, "spent_microusd": 7_000_000, "held_microusd": 0,
             "agents": {f"agent-0{i}": agent("running", 10 * (i + 1)) for i in range(4)},
             "tasks": [{"id": f"a0{i}", "claimed_by": f"agent-0{i}", "budget_microusd": 5_000_000 if i < 3 else None} for i in range(4)],
             "findings": [finding("agent-00", "useful"), finding("agent-01", "defer", "PARTIAL: out of budget"), finding("agent-02", "defer", "Deferred before execution")],
             "intents": [{"agent_id": "agent-00", "state": "completed", "cost_microusd": 2_000_000, "hold_microusd": 0},
                         {"agent_id": "agent-03", "state": "dispatched", "cost_microusd": 0, "hold_microusd": 500_000}]}
    (directory / "state.json").write_text(json.dumps(state))
    (directory / "config.yaml").write_text(json.dumps({"budget_microusd": 20_000_000, "agent_count": 4}))
    return state


class ProgressTests(unittest.TestCase):
    def test_counts_ledger_and_envelopes(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir(Path(directory))
            summary = progress.summarize(directory)
        self.assertEqual({k: v for k, v in summary["counts"].items() if v}, {"filed": 1, "partial": 1, "defer": 1, "running": 1})
        self.assertEqual(summary["cap_microusd"], 20_000_000)
        by_agent = {row["agent"]: row for row in summary["tasks"]}
        self.assertEqual(by_agent["agent-00"]["used"], 0.4)
        self.assertEqual(by_agent["agent-03"]["envelope_microusd"], 5_000_000, "fair share when the task has no envelope")
        self.assertEqual(by_agent["agent-03"]["spent_microusd"], 500_000, "a live hold counts")
        text = progress.render(summary)
        self.assertIn("ledger $7.00 spent", text)
        self.assertIn("(35%)", text)

    def test_missing_state_is_a_clean_error(self):
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(ValueError):
            progress.summarize(directory)

    def test_remote_status_brief_matches(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir(Path(directory))
            brief = remote.progress_brief(Path(directory))
            self.assertIsNone(remote.progress_brief(Path(directory) / "absent"))
        self.assertEqual((brief["running"], brief["filed"], brief["deferred_or_partial"]), (1, 1, 2))
        self.assertEqual(brief["cap_microusd"], 20_000_000)


if __name__ == "__main__":
    unittest.main()
