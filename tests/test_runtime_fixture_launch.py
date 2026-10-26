"""Fixture launcher process-identity checks; no service or credential is created."""

import json
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


LAUNCHER = Path(__file__).parents[1] / "scripts/runtime_fixture_launch.py"


class RuntimeFixtureLaunchTests(unittest.TestCase):
    def launch(self, runtime, calls):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            root = directory / "ss-launcher-case"
            build = directory / "build"
            binary = directory / "solos-swarm"

            def run(command, **kwargs):
                command = [str(value) for value in command]
                calls.append((command, kwargs))
                self.assertTrue(kwargs["check"])
                if command[0] == str(build / "fixture-preparer"):
                    self.assertFalse(root.exists(), "The preparer must use a fresh fixture")
                    (root / "cp").mkdir(parents=True)
                    (root / "cp/runtime.json").write_text(json.dumps(runtime))
                return subprocess.CompletedProcess(command, 0)

            with patch.object(sys, "argv", [str(LAUNCHER), str(root), str(build), str(binary)]), \
                    patch.object(subprocess, "run", side_effect=run):
                runpy.run_path(str(LAUNCHER), run_name="__main__")

    @staticmethod
    def prepared_runtime(gid=984):
        return {"evaluator": None, "evaluators": [{"process": {"uid": 995, "gid": gid}}]}

    def test_evaluator_gets_configured_supplemental_group_without_changing_primary_or_permissions(self):
        calls = []
        self.launch(self.prepared_runtime(), calls)
        services = [command for command, _ in calls if "systemd-run" in command]
        self.assertEqual(len(services), 4)
        evaluator = next(command for command in services if any(
            argument.endswith("-evaluator") and argument.startswith("--unit=")
            for argument in command
        ))
        self.assertIn("--property=User=995", evaluator)
        self.assertIn("--property=Group=985", evaluator)
        self.assertIn("--property=SupplementaryGroups=984", evaluator)
        self.assertEqual(sum(argument.startswith("--property=SupplementaryGroups=")
                             for argument in evaluator), 1)
        for command in services:
            if command != evaluator:
                self.assertFalse(any(argument.startswith("--property=SupplementaryGroups=")
                                     for argument in command))

        # The delivery correction adds group membership instead of making protected files public.
        owners = [(command[3], Path(command[4]).name) for command, _ in calls
                  if command[:3] == ["sudo", "chown", "-R"]]
        self.assertEqual(owners, [("999:985", "cp"), ("999:985", "shared"),
                                  ("994:983", "witness"), ("995:985", "evaluator")])
        modes = [(command[2], Path(command[3]).name) for command, _ in calls
                 if command[:2] == ["sudo", "chmod"]]
        self.assertEqual(modes, [("711", "ss-launcher-case"), ("711", "evaluator"),
                                ("710", "public"), ("710", "ipc"),
                                ("640", "evaluator.der"), ("640", "ca.der")])
        self.assertFalse(any("protected/key.der" in command[-1] and "chmod" in command
                             for command, _ in calls))

    def test_supplemental_group_is_derived_from_the_prepared_profile(self):
        calls = []
        self.launch(self.prepared_runtime(986), calls)
        preparer = calls[0]
        self.assertEqual(preparer[1]["env"]["SOLOS_RUNTIME_SMOKE_EVAL_GID"], "984")
        evaluator = next(command for command, _ in calls if
                         "--unit=ss-launcher-case-evaluator" in command)
        self.assertIn("--property=SupplementaryGroups=986", evaluator)
        self.assertNotIn("--property=SupplementaryGroups=984", evaluator)

    def test_missing_or_invalid_configured_group_starts_no_service(self):
        missing = self.prepared_runtime()
        del missing["evaluators"][0]["process"]["gid"]
        cases = [missing, {"evaluator": None, "evaluators": []}]
        cases.extend(self.prepared_runtime(gid) for gid in (None, False, 0, -1, "984", 2**32 - 1, 2**32))
        for runtime in cases:
            with self.subTest(runtime=runtime):
                calls = []
                with self.assertRaises((AssertionError, KeyError, TypeError, ValueError)):
                    self.launch(runtime, calls)
                self.assertTrue(calls, "The test must exercise the real preparation path")
                self.assertFalse(any("systemd-run" in command for command, _ in calls),
                                 "Invalid prepared authority must fail before starting services")


if __name__ == "__main__":
    unittest.main()
