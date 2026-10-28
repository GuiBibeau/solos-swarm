"""Controller boundary checks; these create no SSH connection or paid request."""

import hashlib
import importlib.util
import io
import json
import os
from contextlib import contextmanager
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("remote", Path(__file__).parents[1] / "scripts/remote.py")
remote = importlib.util.module_from_spec(spec)
spec.loader.exec_module(remote)


class RemoteTests(unittest.TestCase):
    def test_ssh_host_defaults_to_placeholder_and_rejects_shell_syntax(self):
        self.assertEqual(remote.EXAMPLE_SSH_HOST, "solos-node")
        self.assertEqual(remote.ssh_argv("solos-node")[-1], "solos-node")
        self.assertEqual(remote.ssh_argv("ops@cluster.example")[-1], "ops@cluster.example")
        self.assertEqual(remote.ssh_command("status", "worker-node-01")[-2], "worker-node-01")
        for value in ("solos-node;rm", "host name", "", "-oProxyCommand=x"):
            with self.assertRaises(ValueError):
                remote.ssh_argv(value)
        with patch.dict(os.environ, {"SOLOS_SWARM_SSH_TARGET": "worker-node-01"}):
            self.assertEqual(remote.default_ssh_host(), "worker-node-01")
        with patch.dict(os.environ, {}, clear=True):
            self.assertEqual(remote.default_ssh_host(), "solos-node")

    def test_run_ids_cannot_escape_owned_paths(self):
        for value in ("../another-service", "a/b", "UPPER", "", "a" * 49):
            with self.assertRaises(ValueError):
                remote.run_path(value)
        self.assertEqual(remote.unit_name("preview-42"), "solos-swarm-preview-42.service")

    def test_manifest_blocks_altered_artifact(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(remote, "DATA", Path(directory)):
            candidate_id = hashlib.sha256(b"source").hexdigest()
            candidate = remote.candidate_path(candidate_id)
            candidate.mkdir()
            binary = candidate / "runtime"
            binary.write_bytes(b"exact candidate")
            remote.write_json(candidate / "candidate.json", {"candidate_id": candidate_id, "files": remote.inventory(candidate)})
            remote.verify_candidate(candidate_id)
            binary.write_bytes(b"changed candidate")
            with self.assertRaisesRegex(ValueError, "frozen manifest"):
                remote.verify_candidate(candidate_id)

    def test_same_run_returns_existing_without_credentials_or_start(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(remote, "STATE", Path(directory)):
            request = {"run_id": "preview-42", "candidate_id": "a" * 64, "seed": 42,
                       "idle_probe": False, "deadline_seconds": None}
            path = remote.run_path(request["run_id"])
            path.mkdir(parents=True)
            remote.write_json(path / "request.json", request)
            with patch.object(remote, "status_host", return_value={"run_id": "preview-42"}), patch.object(remote, "verify_candidate") as verify:
                self.assertTrue(remote.start_host(request)["existing"])
                verify.assert_not_called()
            request["seed"] = 43
            with self.assertRaisesRegex(ValueError, "different candidate or frozen case"):
                remote.start_host(request)

    def test_credential_is_only_read_from_the_named_provider_entry(self):
        with tempfile.TemporaryDirectory() as directory:
            auth = Path(directory) / "auth.json"
            auth.write_text(json.dumps({"deepseek": {"type": "api_key", "key": "test-secret"}}))
            self.assertEqual(remote.read_deepseek_key(auth), "test-secret")
            self.assertEqual(remote.read_provider_key(auth, "deepseek"), "test-secret")
            # A provider with no entry fails loudly instead of spending against
            # a different vendor's credential.
            with self.assertRaisesRegex(ValueError, "no venice api_key entry"):
                remote.read_provider_key(auth, "venice")
            auth.write_text(json.dumps({"deepseek": {"type": "oauth", "key": "test-secret"}}))
            with self.assertRaisesRegex(ValueError, "no deepseek api_key entry"):
                remote.read_deepseek_key(auth)

    def test_goal_limits_admit_thirty_agents_and_refuse_thirty_one(self):
        goal = {"agent_count": 30, "tasks": [{} for _ in range(30)], "deadline_seconds": 14_400,
                "budget_microusd": 100_000_000}
        remote.validate_goal_limits(goal)
        goal["agent_count"] = 31
        goal["tasks"].append({})
        with self.assertRaisesRegex(ValueError, "1-30 agents"):
            remote.validate_goal_limits(goal)
        goal["agent_count"] = 30
        goal["tasks"].pop()
        goal["budget_microusd"] = 1_000_000_000_001   # one microdollar over the $1,000,000 ceiling
        with self.assertRaisesRegex(ValueError, "1-30 agents"):
            remote.validate_goal_limits(goal)

    def test_provider_follows_the_goal_upstream_over_the_legacy_default(self):
        with tempfile.TemporaryDirectory() as directory:
            goal = Path(directory) / "goal.json"
            goal.write_text(json.dumps({"model": "deepseek-flash"}))
            self.assertEqual(remote.provider_for(goal, None), "deepseek")
            goal.write_text(json.dumps({"model": "m", "upstream": {"provider": "venice"}}))
            self.assertEqual(remote.provider_for(goal, None), "venice")
            # An explicit flag wins outright.
            self.assertEqual(remote.provider_for(goal, "deepseek"), "deepseek")
            self.assertEqual(remote.provider_for(None, None), "deepseek")

    def test_start_ignores_only_ssh_hangup_after_reading_complete_request(self):
        request = {"run_id": "preview-42"}
        stdin = io.StringIO(json.dumps(request))

        def check_policy(signum, disposition):
            self.assertEqual(stdin.tell(), len(stdin.getvalue()))
            self.assertEqual((signum, disposition), (remote.signal.SIGHUP, remote.signal.SIG_IGN))

        with patch.object(remote.sys, "stdin", stdin), patch.object(remote.sys, "stdout", io.StringIO()), \
                patch.object(remote.signal, "signal", side_effect=check_policy) as signals, \
                patch.object(remote, "start_host", return_value={"existing": False}) as start:
            remote.host_main("start")
            signals.assert_called_once_with(remote.signal.SIGHUP, remote.signal.SIG_IGN)
            start.assert_called_once_with(request)

    def test_collect_leaves_the_code_scratch_dir_on_the_host(self):
        import tarfile
        with tempfile.TemporaryDirectory() as root:
            run = Path(root)
            (run / "code/agent-1").mkdir(parents=True)
            (run / "code/agent-1/scratch.bin").write_bytes(b"x" * 1000)
            (run / "code/b.txt").write_bytes(b"yy")
            (run / "results").mkdir()
            (run / "results/summary.json").write_text("{}")
            (run / "codex.json").write_text("{}")  # prefix lookalike must stay
            for include, expect_code in ((False, False), (True, True)):
                out = io.BytesIO()
                remote.collect_bundle(run, out, include)
                out.seek(0)
                with tarfile.open(fileobj=out) as bundle:
                    names = set(bundle.getnames())
                    manifest = json.load(bundle.extractfile("collect-manifest.json"))
                self.assertEqual("code/b.txt" in names, expect_code)
                self.assertIn("results/summary.json", names)
                self.assertIn("codex.json", names)
                if expect_code:
                    self.assertEqual(manifest["skipped"], [])
                else:
                    self.assertEqual(manifest["skipped"], [{"path": "code", "files": 2, "bytes": 1002}])

    def test_cancel_already_stopped_run_returns_status_without_stop(self):
        request = {"run_id": "preview-42"}
        status = {"run_id": "preview-42", "service_stopped": True, "cleanup_confirmed": True}
        stdout = io.StringIO()
        locked = False

        @contextmanager
        def run_lock(name):
            nonlocal locked
            self.assertEqual(name, "run-preview-42")
            locked = True
            yield
            locked = False

        def current_status(run_id):
            self.assertTrue(locked, "Cancellation must wait for a concurrent start's run lock")
            return status

        with patch.object(remote.sys, "stdin", io.StringIO(json.dumps(request))), \
                patch.object(remote.sys, "stdout", stdout), \
                patch.object(remote, "lock", side_effect=run_lock), \
                patch.object(remote, "status_host", side_effect=current_status) as read_status, \
                patch.object(remote.subprocess, "run") as stop:
            remote.host_main("cancel")
            stop.assert_not_called()
            read_status.assert_called_once_with(request["run_id"])
            self.assertEqual(json.loads(stdout.getvalue()), status)


if __name__ == "__main__":
    unittest.main()
