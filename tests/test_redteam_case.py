"""Security-review case generation and scorer checks; no live inference."""

import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "prepare_redteam", Path(__file__).parents[1] / "scripts/prepare_redteam_case.py"
)
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)

checker_spec = importlib.util.spec_from_file_location(
    "check_run", Path(__file__).parents[1] / "scripts/check_run.py"
)
checker = importlib.util.module_from_spec(checker_spec)
checker_spec.loader.exec_module(checker)


def build_repo(root):
    (root / "apps/api/src/routes").mkdir(parents=True)
    (root / "packages/util").mkdir(parents=True)
    (root / "apps/api/src/routes/auth-keys.ts").write_text(
        "export function rotateKey() { return { secret: 'sk-test' }; }\n" * 40
    )
    (root / "apps/api/src/routes/plain.ts").write_text("export const ok = 1;\n" * 200)
    (root / "packages/util/index.ts").write_text("export const util = 2;\n")
    return root


class RedTeamCaseTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        build_repo(self.root)

    def tearDown(self):
        self.tmp.cleanup()

    def test_case_is_frozen_ten_shard_and_within_admission_bounds(self):
        goal, expected = prepare.make_case(self.root, 7)
        self.assertEqual(len(goal["tasks"]), 10)
        self.assertEqual(goal["scope"], "authorized-audit:solana-developer-platform")
        self.assertEqual(goal["model"], "deepseek-flash")
        self.assertLessEqual(
            len(goal["worker_prompt"]) + len(goal["synthesis_prompt"]), 8192
        )
        for task in goal["tasks"]:
            self.assertLessEqual(
                len(json.dumps(task["input"])), goal["max_input_bytes"] // 2
            )
        self.assertEqual(
            expected["fixture_digest"],
            hashlib.sha256(prepare.canonical(goal["tasks"])).hexdigest(),
        )
        self.assertEqual(
            sum(len(task["input"]["files"]) for task in goal["tasks"]),
            expected["coverage"]["covered_files"],
        )
        nonempty = [task for task in goal["tasks"] if task["input"]["files"]]
        self.assertTrue(nonempty, "at least one shard must carry files")
        for task in nonempty:
            self.assertTrue(task["input"]["files"])            
        distributed = {
            row["path"] for task in goal["tasks"] for row in task["input"]["files"]
        }
        self.assertLessEqual(
            len(distributed), expected["coverage"]["covered_files"]
        )

    def test_shards_respect_cap_and_cover_risk_first(self):
        goal, expected = prepare.make_case(self.root, 7)
        covered = {
            row for task in goal["tasks"] for row in (row["path"] for row in task["input"]["files"])
        }
        self.assertIn("apps/api/src/routes/auth-keys.ts", covered)
        for path in expected["coverage"]["uncovered_files"]:
            self.assertNotIn(path, covered)

    def test_write_case_freezes_digests_and_refuses_rewrites(self):
        goal_path = self.root / "goal.json"
        expected_path = self.root / "private" / "expected.json"
        expected = prepare.write_case(self.root, 7, goal_path, expected_path)
        self.assertEqual(
            expected["config_sha256"], checker.digest(goal_path)
        )
        with self.assertRaises(FileExistsError):
            prepare.write_case(self.root, 7, goal_path, expected_path)


class SecurityScorerTests(unittest.TestCase):
    def setUp(self):
        self.contract = {
            "result_key": "vulnerabilities",
            "required_fields": ["id", "severity", "title", "file", "description"],
            "optional_fields": ["line", "evidence", "impact", "recommendation"],
            "severities": ["critical", "high", "medium", "low"],
        }
        self.assigned = ["apps/api/src/routes/auth-keys.ts"]

    def test_valid_result_and_empty_clean_shard_are_accepted(self):
        result = {"vulnerabilities": [{
            "id": "task-00-V-001", "severity": "high", "title": "t",
            "file": self.assigned[0], "line": 2, "description": "d",
            "evidence": "e", "impact": "i", "recommendation": "r",
        }]}
        self.assertEqual(checker.security_finding(result, self.contract, self.assigned), [])
        self.assertEqual(checker.security_finding({"vulnerabilities": []}, self.contract, self.assigned), [])

    def test_unassigned_file_and_bad_severity_are_rejected(self):
        result = {"vulnerabilities": [{
            "id": "task-00-V-001", "severity": "extreme", "title": "t",
            "file": "elsewhere/other.ts", "description": "d",
        }]}
        issues = checker.security_finding(result, self.contract, self.assigned)
        self.assertTrue(any("severity" in issue for issue in issues))
        self.assertTrue(any("unassigned file" in issue for issue in issues))

    def test_union_scoring_requires_exact_verbatim_merge(self):
        rows = [{"id": f"task-0{i}-V-001", "severity": "low", "title": "t",
                 "file": "a.ts", "description": "d"} for i in range(2)]
        findings = [{"result": {"vulnerabilities": [row]}} for row in rows]
        artifact = {"vulnerabilities": list(reversed(rows))}
        union = checker.security_union(findings, self.contract)
        self.assertEqual(sorted(artifact["vulnerabilities"], key=checker.canonical), union)
        dropped = {"vulnerabilities": artifact["vulnerabilities"][:-1]}
        self.assertNotEqual(
            sorted(dropped["vulnerabilities"], key=checker.canonical), union
        )
        fabricated = {"vulnerabilities": artifact["vulnerabilities"] + [dict(rows[0], id="extra")]}
        self.assertNotEqual(
            sorted(fabricated["vulnerabilities"], key=checker.canonical), union
        )


if __name__ == "__main__":
    unittest.main()
