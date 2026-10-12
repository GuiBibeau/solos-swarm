"""Controller boundary checks; these create no SSH connection or paid request."""

import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("remote", Path(__file__).parents[1] / "scripts/remote.py")
remote = importlib.util.module_from_spec(spec)
spec.loader.exec_module(remote)


class RemoteTests(unittest.TestCase):
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

    def test_credential_is_only_read_from_the_deepseek_entry(self):
        with tempfile.TemporaryDirectory() as directory:
            auth = Path(directory) / "auth.json"
            auth.write_text(json.dumps({"deepseek": {"type": "api_key", "key": "test-secret"}}))
            self.assertEqual(remote.read_deepseek_key(auth), "test-secret")
            auth.write_text(json.dumps({"deepseek": {"type": "oauth", "key": "test-secret"}}))
            with self.assertRaisesRegex(ValueError, "no DeepSeek api_key"):
                remote.read_deepseek_key(auth)


if __name__ == "__main__":
    unittest.main()
