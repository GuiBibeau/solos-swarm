"""Replay loop: offline, deterministic. Run: python3 -m unittest tests.test_replay (or discover)."""
import json
import random
import tempfile
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parents[1]))

from replay import guard, hook, propose
from replay.ingest import ingest
from replay.policy import Adaptive, Baseline, from_spec
from replay.schema import read_history, survives_screen, write_history
from replay.simulate import Weights, replay, reward, score

BASE = Baseline().spec()
CAND = {"kind": "adaptive", "stop_after_null": 1, "per_family_max": 4, "max_rounds": 4}


def attempt(fam, seq, surv, ref=None, calls=60, cost=1.0, trials=30):
    return {"task_id": f"{fam}-{seq}", "family": fam, "role": "worker", "seq": seq, "status": "filed", "calls": calls,
            "cost_usd": cost, "minutes": 30, "trials_used": trials, "trials_budget": 40, "beats_placebo_max": surv,
            "mde_ok": surv, "net_bps": 3.0 if surv else -1.0, "referee": ref if surv else None}


def make_run(i, rng, hot_p=0.6, structured=True, n_fam=12, depth=6):
    hot = set(rng.sample(range(n_fam), 3)) if structured else set()
    atts = []
    for f in range(n_fam):
        for k in range(depth):
            p = hot_p if f in hot else (0.03 if structured else 0.2)
            s = rng.random() < p
            ref = ("confirmed" if rng.random() < (0.8 if f in hot else 0.1) else "not_confirmed") if s else None
            if not structured:
                ref = ("confirmed" if rng.random() < 1 / 3 else "not_confirmed") if s else None   # EV 0 at f=0.5
            atts.append(attempt(f"f{f:02d}", k, s, ref))
    return {"run_id": f"r{i}", "scorable": True, "attempts": atts}


def history(n, seed, **kw):
    rng = random.Random(seed)
    return [make_run(i, rng, **kw) for i in range(n)]


class Replay(unittest.TestCase):
    def test_survival_is_derived_from_counted_trials(self):
        a = attempt("f01", 0, True)
        self.assertTrue(survives_screen(a))
        self.assertFalse(survives_screen(dict(a, trials_used=41)))        # over its trial budget
        self.assertFalse(survives_screen(dict(a, trials_used=None)))      # trials not counted
        self.assertFalse(survives_screen(dict(a, beats_placebo_max=None)))
        self.assertFalse(survives_screen(dict(a, status="partial")))

    def test_baseline_two_waves_and_unexplored_is_reported(self):
        run = history(1, 1)[0]
        res = replay(from_spec(BASE), run)
        self.assertEqual(res["rounds"], 2)
        self.assertEqual(res["unexplored"], 0)
        deep = replay(Adaptive(stop_after_null=99, per_family_max=99, max_rounds=9), run)
        self.assertGreater(deep["unexplored"], 0)              # asked past the recorded depth of 6

    def test_call_cap_and_trial_budget_lose_the_result(self):
        run = {"run_id": "x", "attempts": [attempt("f00", 0, True, "confirmed", calls=80)]}
        self.assertEqual(sum(o["survived"] for o in replay(from_spec(BASE), run)["outcomes"]["f00"] if o), 1)
        capped = replay(from_spec(dict(BASE, call_cap=50)), run)
        self.assertEqual(sum(o["survived"] for o in capped["outcomes"]["f00"] if o), 0)
        self.assertLess(capped["spent_usd"], 1.0)

    def test_unrefereed_survivors_cannot_be_farmed(self):
        w = Weights()
        mk = lambda n: {"outcomes": {"f": [{"survived": True, "referee": None, "cost": 0, "minutes": 1, "trials": 0}] * n},
                        "spent_usd": 0, "trials": 0, "wall_hours": 0, "attempts_per_round": 0}
        self.assertEqual(reward(mk(3), w), reward(mk(30), w))      # capped
        refuted = mk(1); refuted["outcomes"]["f"][0] = dict(refuted["outcomes"]["f"][0], referee="not_confirmed")
        self.assertLess(reward(refuted, w), 0)

    def test_no_referee_label_leaks_to_policy(self):
        seen = []
        class Spy(Adaptive):
            def select(self, view):
                seen.extend(o for outs in view.revealed.values() for o in outs if o)
                return super().select(view)
        replay(Spy(max_rounds=3), history(1, 2)[0])
        self.assertTrue(seen and all(set(o) >= {"survived", "cost"} for o in seen))


class Guard(unittest.TestCase):
    def test_insufficient_history(self):
        v = guard.decide(history(5, 3), BASE, CAND)
        self.assertEqual(v["verdict"], "insufficient history")
        self.assertFalse(v["adopt"])
        unscored = [dict(r, scorable=False) for r in history(30, 3)]
        self.assertEqual(guard.decide(unscored, BASE, CAND)["verdict"], "insufficient history")

    def test_adopts_a_better_policy(self):
        runs = history(40, 4)
        v = guard.decide(runs, BASE, CAND)
        self.assertTrue(v["adopt"], v)
        self.assertGreater(v["held_ci"][0], 0)
        self.assertGreaterEqual(v["dev_mean_diff"], 0)

    def test_refuses_when_noise_tempts(self):
        refused, tempted = 0, 0
        quality_only = Weights(usd=0, trial=0, wall_hour=0, parallel=0)     # survivors have zero expected value
        for seed in range(10):
            runs = history(16, 100 + seed, structured=False)
            found = propose.search(runs, BASE, w=quality_only, grid={"stop_after_null": [1, 2, 3], "per_family_max": [2, 3, 4], "max_rounds": [2, 3, 4], "call_cap": [None, 60], "trial_budget": [None, 30]})
            v = guard.decide(runs, BASE, found["policy"], tuned_on=found["tuned_on"], w=quality_only)
            refused += not v["adopt"]
            tempted += found["dev_mean"] > 0 or v.get("dev_mean_diff", 0) > 0
        self.assertGreaterEqual(tempted, 5)     # the search finds dev gains in pure noise ...
        self.assertGreaterEqual(refused, 9)     # ... and the guard almost never adopts them

    def test_refuses_candidate_tuned_on_held_out(self):
        runs = history(24, 4)
        _, held = guard.split_runs(runs)
        v = guard.decide(runs, BASE, CAND, tuned_on=[held[0]["run_id"]])
        self.assertFalse(v["adopt"])
        self.assertIn("held-out", v["verdict"])

    def test_refuses_candidate_that_needs_unexplored_branches(self):
        v = guard.decide(history(24, 4), BASE, {"kind": "adaptive", "stop_after_null": 99, "per_family_max": 99, "max_rounds": 12})
        self.assertFalse(v["adopt"])
        self.assertIn("never explored", v["verdict"])

    def test_split_is_stable_when_history_grows(self):
        runs = history(24, 4)
        _, held = guard.split_runs(runs[:20])
        self.assertTrue({r["run_id"] for r in held} <= {r["run_id"] for r in guard.split_runs(runs)[1]} | {r["run_id"] for r in runs[20:]})


class IngestAndHook(unittest.TestCase):
    def evidence(self, d, with_policy=False):
        d = Path(d); d.mkdir(parents=True)
        m = {"trials_used": 30, "trials_budget": 40, "net_bps_per_trade": 2.0, "beats_placebo_max": True, "mde_ok": True}
        rec = {"id": "c1", "kind": "candidate", "severity": "useful", "description": "x", "metrics": m,
               "prompt_patch": "file early", **({"policy": CAND} if with_policy else {})}
        state = {"agents": {"agent-01": {"model_calls": 50, "started_ms": 0, "stopped_ms": 600000}, "agent-02": {"model_calls": 3}},
                 "tasks": [{"id": "t1", "claimed_by": "agent-01", "input": {"family": "f01"}},
                           {"id": "t2", "claimed_by": "agent-02", "input": {"family": "f02"}}],
                 "findings": [{"agent_id": "agent-01", "task_id": "t1", "result": {"papers": [rec]}}],
                 "intents": [{"agent_id": "agent-01", "cost_microusd": 1_500_000}], "spent_microusd": 1_500_000}
        (d / "state.json").write_text(json.dumps(state))

    def test_ingest_round_trip(self):
        with tempfile.TemporaryDirectory() as t:
            self.evidence(Path(t) / "run1")
            (run,) = ingest([Path(t) / "run1"])
            a = {x["task_id"]: x for x in run["attempts"]}
            self.assertEqual((a["t1"]["status"], a["t2"]["status"]), ("filed", "failed"))
            self.assertEqual((a["t1"]["calls"], a["t1"]["cost_usd"], a["t1"]["minutes"]), (50, 1.5, 10.0))
            self.assertTrue(survives_screen(a["t1"]) and run["filed"] == 1 and run["failed"] == 1)
            write_history([run], Path(t) / "h.jsonl")
            self.assertTrue(read_history(Path(t) / "h.jsonl")[0]["scorable"])

    def test_hook_records_verdict_and_never_applies_without_guard(self):
        with tempfile.TemporaryDirectory() as t:
            root, log = Path(t), []
            self.evidence(root / "evidence" / "c1", with_policy=True)
            v = hook.after_close("c1", root, log.append)
            self.assertEqual(v["verdict"], "insufficient history")          # one run only
            self.assertIsNone(hook.approved_policy(root, "c1"))
            self.assertEqual(log[0]["adopt"], False)
            self.assertTrue((root / "evidence" / "c1-replay-verdict.json").exists())

    def test_prompt_patch_alone_is_unscored(self):
        with tempfile.TemporaryDirectory() as t:
            self.evidence(Path(t) / "evidence" / "c1")
            v = hook.after_close("c1", t, lambda e: None)
            self.assertTrue(v["verdict"].startswith("unscored") and v["prompt_patch_present"])

    def test_approved_policy_only_after_adopt(self):
        with tempfile.TemporaryDirectory() as t:
            (Path(t) / "evidence").mkdir()
            (Path(t) / "evidence" / "c2-replay-verdict.json").write_text(json.dumps({"adopt": True, "candidate": CAND}))
            self.assertEqual(hook.approved_policy(t, "c2"), CAND)


if __name__ == "__main__":
    unittest.main()
