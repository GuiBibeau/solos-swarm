"""Tournament evidence layout (evidence/<step>/ + <step>-perf.json), read-only, offline."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parents[1]))

from replay import guard, hook
from replay.ingest import ingest_tournament
from replay.schema import survives_screen

M = {"trials_used": 30, "trials_budget": 40, "net_bps_per_trade": 4.0, "beats_placebo_max": True, "mde_ok": True}


def step(root, name, tasks, findings, cost=1_000_000):
    d = Path(root) / "evidence" / name
    d.mkdir(parents=True)
    agents = {t["claimed_by"]: {"model_calls": 40, "started_ms": 0, "stopped_ms": 1_200_000} for t in tasks}
    (d / "state.json").write_text(json.dumps({"agents": agents, "tasks": tasks, "findings": findings, "spent_microusd": cost * len(tasks),
                                              "intents": [{"agent_id": t["claimed_by"], "cost_microusd": cost} for t in tasks]}))
    (d.parent / f"{name}-perf.json").write_text(json.dumps({"cache_hit_rate": 0.8, "prompt_tokens": 1000}))


def task(tid, **inp):
    return {"id": tid, "claimed_by": "agent-" + tid, "input": inp}


def fnd(tid, *rows):
    return {"agent_id": "agent-" + tid, "task_id": tid, "result": {"papers": list(rows)}}


class Tournament(unittest.TestCase):
    def build(self, root):
        step(root, "w1", [task("w100", role="coordinator"), task("w101", role="worker", family="f01"), task("w102", role="worker", family="f02")],
             [fnd("w101", {"id": "c1", "kind": "candidate", "metrics": M}),
              fnd("w102", {"id": "n1", "kind": "negative-result", "metrics": {"trials_used": 20, "trials_budget": 40}})])
        step(root, "w2", [task("w201", role="worker", family="f01")], [fnd("w201", {"id": "c2", "kind": "candidate", "metrics": dict(M, beats_placebo_max=False)})])
        step(root, "w3", [task("r301", role="worker", kind="referee", candidate={"id": "c1"})],
             [fnd("r301", {"id": "ref1", "kind": "referee-report", "metrics": {"verdict": "not confirmed"}, "decision": "not confirmed"})])

    def test_one_run_with_ordered_attempts_and_referee_link(self):
        with tempfile.TemporaryDirectory() as t:
            self.build(t)
            run = ingest_tournament(t)
        self.assertEqual(run["step"], "w1+w2+w3")
        workers = [a for a in run["attempts"] if a["role"] == "worker"]
        f01 = [a for a in workers if a["family"] == "f01"]
        self.assertEqual([a["seq"] for a in f01], [0, 1])                      # wave 1 then wave 2
        self.assertTrue(survives_screen(f01[0]) and not survives_screen(f01[1]))
        self.assertEqual(f01[0]["referee"], "not_confirmed")
        null = next(a for a in workers if a["family"] == "f02")                # filed null with counted trials
        self.assertEqual((null["trials_used"], null["beats_placebo_max"], survives_screen(null)), (20, False, False))
        self.assertEqual({a["role"] for a in run["attempts"]}, {"worker", "referee", "other"})
        self.assertTrue(run["scorable"])
        self.assertEqual(run["cache_hit_rate"], 0.8)

    def test_reads_only(self):
        with tempfile.TemporaryDirectory() as t:
            self.build(t)
            before = sorted(str(p) for p in Path(t).rglob("*"))
            ingest_tournament(t)
            self.assertEqual(before, sorted(str(p) for p in Path(t).rglob("*")))

    def test_first_real_history_is_insufficient(self):
        with tempfile.TemporaryDirectory() as t:
            self.build(t)
            run = ingest_tournament(t)
        v = guard.decide([run], hook.BASELINE, {"kind": "adaptive"})
        self.assertEqual(v["verdict"], "insufficient history")

    def test_missing_evidence_is_none(self):
        with tempfile.TemporaryDirectory() as t:
            self.assertIsNone(ingest_tournament(t))


if __name__ == "__main__":
    unittest.main()
