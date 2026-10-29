"""Offline checks: every generated goal satisfies the broker's Goal::validate rules (mirrored from src/broker.rs and
scripts/remote.py), and the generator handles synthetic wave evidence. Run: python3 test_goals.py"""
import json, shutil, tempfile, unittest
from pathlib import Path
import common, gen, lint


def validate(g):
    assert g["schema_version"] == 1 and g["intent"]
    assert g["worker_prompt"] and len((g["worker_prompt"] + g["synthesis_prompt"]).encode()) <= 8192
    assert g["scope"] == "public-web-research" and g["model"] == "deepseek-flash"
    assert 1 <= g["agent_count"] <= 30 and len(g["tasks"]) == g["agent_count"]
    assert 1 <= g["deadline_seconds"] <= 21_600 and 1 <= g["budget_microusd"] <= 100_000_000
    assert sum(t["budget_microusd"] for t in g["tasks"]) <= g["budget_microusd"]
    assert 1 <= g["max_calls_per_agent"] <= 10_000
    assert g["input_price_microusd_per_million"] >= 300_000 and g["output_price_microusd_per_million"] >= 1_200_000
    assert 30_000 <= g["cache_hit_price_microusd_per_million"] <= g["input_price_microusd_per_million"]
    req = {"claim_task", "board_read", "board_post", "submit_finding", "read_findings", "submit_artifact"}
    assert req <= set(g["grants"]) and set(g["grants"]) <= req | {"fetch_url", "run_code"}
    ids = [t["id"] for t in g["tasks"]]; assert len(set(ids)) == len(ids)
    assert sum(1 for t in g["tasks"] if t["input"]["role"] == "coordinator") == 1
    for t in g["tasks"]:
        assert len(json.dumps(t["input"]).encode()) <= g["max_input_bytes"] // 2
        assert t["input"]["file"] in t["input"]["seed_urls"]
    # one hold at the saturated context must fit every envelope with room for a call
    hold = (600_000 + 4096) * 0.30 / 1e6 + g["max_output_tokens"] * 1.2 / 1e6
    assert all(t["budget_microusd"] / 1e6 > hold for t in g["tasks"]), hold


def fake_recs(prefix, n, cand_every=3):
    recs = []
    for i in range(n):
        k = "candidate" if i % cand_every == 0 else "negative-result"
        r = {"id": f"{prefix}{i}", "title": f"T{i}", "file": common.SEEDS[0], "description": "d", "severity": "useful" if k == "candidate" else "negative",
             "kind": k, "claim": "c", "decision": "defer", "metrics": {"trials_used": 30, "trials_budget": 40}}
        if k == "candidate":
            r.update(spec_code="def f(): pass", limitations="l", kill_criterion="k", metrics={
                "trials_used": 30, "trials_budget": 40, "per_year": {}, "net_bps_per_trade": 2.0 + i, "n_trades": 500,
                "placebo_max": 0.5, "top3_day_share": .1, "top3_name_share": .2, "decay": 0, "beats_placebo_max": True, "mde_ok": True})
        else:
            r["proof"] = "code"
        recs.append(r)
    return recs


def write_state(d, recs):
    d.mkdir(parents=True, exist_ok=True)
    (d / "state.json").write_text(json.dumps({"findings": [{"agent_id": "agent-01", "task_id": "x", "result": {"papers": recs}}]}))


class T(unittest.TestCase):
    def test_all_steps(self):
        tmp = Path(tempfile.mkdtemp()); gen.HERE = tmp
        write_state(tmp / "evidence/w1", fake_recs("a", 30)); write_state(tmp / "evidence/c1", [
            {"id": "p", "title": "p", "file": common.SEEDS[0], "description": "d", "severity": "background", "kind": "wave-plan",
             "decision": "x", "evidence": "y", "assignments": [{"title": "t", "family": "f05", "kind": "deepen", "lead_ids": ["a0"], "mission": "m", "trial_budget": 50}],
             "prompt_patch": "be brief"}])
        write_state(tmp / "evidence/w2", fake_recs("b", 30)); write_state(tmp / "evidence/c2", [])
        write_state(tmp / "evidence/w3", fake_recs("r", 5)); write_state(tmp / "evidence/c3", [])
        for step in gen.STEPS:
            s = gen.main(step)
            g = json.loads((tmp / f"{step}-goal.json").read_text()); validate(g)
        shutil.rmtree(tmp)

    def test_lint(self):
        good = fake_recs("g", 4)
        self.assertEqual(lint.lint(good, common.SEEDS), [])
        bad = [dict(good[0])]; del bad[0]["description"]
        self.assertTrue(lint.lint(bad, common.SEEDS))
        self.assertTrue(lint.lint([dict(good[0], proof="/work/x.py")], common.SEEDS))

    def test_budget_sum(self):
        caps = {"w1": 27.0, "c1": 2.5, "w2": 31.8, "c2": 2.5, "w3": 24.6, "c3": 2.5, "c4": 3.2}
        self.assertLessEqual(sum(caps.values()), 95.0)


if __name__ == "__main__":
    unittest.main()
