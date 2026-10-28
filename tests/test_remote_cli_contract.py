"""The driver's call into remote.py must keep working: start (with --refuse-peak), status, collect."""

import importlib.util
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("remote", Path(__file__).parents[1] / "scripts/remote.py")
remote = importlib.util.module_from_spec(spec)
spec.loader.exec_module(remote)

CANDIDATE = "a" * 64


class DriverContract(unittest.TestCase):
    def run_cli(self, argv, **patches):
        with patch("sys.argv", ["remote.py", *argv]):
            remote.main()

    def test_start_with_goal_expected_and_refuse_peak_sends_the_same_request(self):
        with tempfile.TemporaryDirectory() as directory:
            goal, expected, auth = (Path(directory) / n for n in ("goal.json", "expected.json", "auth.json"))
            goal.write_text(json.dumps({"upstream": {"provider": "deepseek"}}))
            expected.write_text("{}")
            auth.write_text(json.dumps({"deepseek": {"type": "api_key", "key": "k"}}))
            sent = []

            def call(action, request, host):
                sent.append((action, dict(request)))
                return {"needs_credential": True} if "api_key" not in request else {"run_id": request["run_id"]}
            with patch.object(remote, "call_host", side_effect=call), patch("builtins.print"):
                self.run_cli(["start", "--candidate", CANDIDATE, "--run-id", "t-w1", "--seed", "7", "--goal-file", str(goal),
                              "--expected-file", str(expected), "--auth-file", str(auth), "--refuse-peak"])
        self.assertEqual([action for action, _ in sent], ["start", "start"])
        first = sent[0][1]
        self.assertEqual((first["candidate_id"], first["run_id"], first["seed"], first["refuse_peak"]), (CANDIDATE, "t-w1", 7, True))
        self.assertIn("goal", first)
        self.assertIn("case_sha256", first)
        self.assertEqual(sent[1][1]["api_key"], "k")

    def test_status_and_collect_keep_their_arguments(self):
        with patch.object(remote, "call_host", return_value={"run_id": "t-w1"}) as call, patch("builtins.print"):
            self.run_cli(["status", "--run-id", "t-w1"])
        self.assertEqual(call.call_args.args[0], "status")
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w") as bundle:
            info = tarfile.TarInfo("state.json")
            info.size = 2
            bundle.addfile(info, io.BytesIO(b"{}"))
        done = type("R", (), {"returncode": 0, "stdout": buffer.getvalue(), "stderr": b""})()
        with tempfile.TemporaryDirectory() as directory, patch.object(remote.subprocess, "run", return_value=done), patch("builtins.print"):
            self.run_cli(["collect", "--run-id", "t-w1", "--output", str(Path(directory) / "out")])
            self.assertTrue((Path(directory) / "out/state.json").exists())

    def test_the_service_gets_the_peak_switch_and_the_widened_backstop(self):
        source = Path(remote.__file__).read_text()
        self.assertIn("--setenv=SWARM_REFUSE_PEAK=1", source)
        self.assertIn("PEAK_SLACK_SECONDS", source)


if __name__ == "__main__":
    unittest.main()
