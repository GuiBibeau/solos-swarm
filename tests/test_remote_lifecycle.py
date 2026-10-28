"""Candidate channels, promotion gate, drain, resume and doctor. Offline: no SSH, no systemd, no spend."""

import hashlib
import importlib.util
import json
import os
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("remote", Path(__file__).parents[1] / "scripts/remote.py")
remote = importlib.util.module_from_spec(spec)
spec.loader.exec_module(remote)

PASS = {"ok": True, "steps": [{"name": "smoke", "ok": True}]}
FAIL = {"ok": False, "steps": [{"name": "integrity", "ok": True}, {"name": "idle_probe", "ok": False}]}
STOPPED = (0, {"ActiveState": "inactive"})


class Host(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.data, self.state = root / "data", root / "state"
        self.data.mkdir()
        self.state.mkdir()
        for patcher in (patch.object(remote, "DATA", self.data), patch.object(remote, "STATE", self.state),
                        patch.object(remote, "service_properties", side_effect=lambda run_id: STOPPED)):
            patcher.start()
            self.addCleanup(patcher.stop)
        self.addCleanup(self.tmp.cleanup)

    def candidate(self, name):
        candidate_id = hashlib.sha256(name.encode()).hexdigest()
        path = self.data / candidate_id
        path.mkdir()
        (path / "runtime").write_bytes(name.encode())
        files = remote.inventory(path)
        remote.write_json(path / "candidate.json", {"candidate_id": candidate_id, "files": files,
                                                    "tree_sha256": remote.tree_hash(files), "source": {"revision": name}})
        return candidate_id

    def make_run(self, run_id, candidate_id, **markers):
        path = self.state / "runs" / run_id
        path.mkdir(parents=True)
        remote.write_json(path / "request.json", {"candidate_id": candidate_id, "run_id": run_id, "seed": 1})
        for marker in markers:
            (path / marker).write_text("{}")
        return path

    def promote(self, candidate_id, gate=PASS, channel="stable"):
        return remote.promote_host({"candidate_id": candidate_id, "channel": channel}, gate=lambda _: gate)


class PointerTests(Host):
    def test_promotion_pins_the_tree_and_keeps_history_for_rollback(self):
        a, b = self.candidate("a"), self.candidate("b")
        self.promote(a)
        pointer = self.promote(b)
        self.assertEqual((pointer["candidate_id"], pointer["previous"]), (b, [a]))
        self.assertEqual(remote.current_host({})["candidate_id"], b)
        self.assertEqual(remote.rollback_host({})["candidate_id"], a)
        self.assertEqual(remote.read_pointer("stable")["rolled_back_from"], b)

    def test_switch_is_atomic_and_never_edits_a_pinned_run(self):
        a, b = self.candidate("a"), self.candidate("b")
        self.promote(a)
        pinned = self.make_run("live-1", a)
        before = (pinned / "request.json").read_bytes()
        with patch.object(remote.os, "replace", side_effect=OSError("disk full at the switch")):
            with self.assertRaises(OSError):
                self.promote(b)
        self.assertEqual(remote.current_host({})["candidate_id"], a, "a failed switch leaves the old pointer whole")
        self.promote(b)
        self.assertEqual((pinned / "request.json").read_bytes(), before, "a running run keeps its candidate")
        self.assertEqual(json.loads(before)["candidate_id"], a)

    def test_failed_gate_leaves_current_unchanged_and_says_why(self):
        a, b = self.candidate("a"), self.candidate("b")
        self.promote(a)
        with self.assertRaisesRegex(ValueError, "idle_probe"):
            self.promote(b, gate=FAIL)
        self.assertEqual(remote.current_host({})["candidate_id"], a)
        log = (self.data / "channels/stable.log.jsonl").read_text()
        self.assertIn("promotion_refused", log)

    def test_post_switch_verification_failure_restores_the_previous_pointer(self):
        a, b = self.candidate("a"), self.candidate("b")
        self.promote(a)
        real = remote.verify_pointer
        calls = []

        def flaky(pointer):
            calls.append(pointer["candidate_id"])
            if pointer["candidate_id"] == b:
                raise ValueError("tree changed under the pointer")
            return real(pointer)
        with patch.object(remote, "verify_pointer", flaky):
            with self.assertRaisesRegex(ValueError, "previous pointer was restored"):
                self.promote(b)
        self.assertEqual(remote.read_pointer("stable")["candidate_id"], a)

    def test_a_tampered_candidate_no_longer_verifies_through_its_pointer(self):
        a = self.candidate("a")
        self.promote(a)
        runtime = self.data / a / "runtime"
        runtime.chmod(0o644)
        runtime.write_bytes(b"swapped")
        with self.assertRaisesRegex(ValueError, "frozen manifest"):
            remote.current_host({})

    def test_an_unknown_channel_and_an_unverified_candidate_are_refused(self):
        with self.assertRaisesRegex(ValueError, "Channel"):
            remote.read_pointer("nightly")
        with self.assertRaises(OSError):
            self.promote("f" * 64)

    def test_channels_are_independent(self):
        a, b = self.candidate("a"), self.candidate("b")
        self.promote(a)
        self.promote(b, channel="canary")
        self.assertEqual((remote.current_host({"channel": "stable"})["candidate_id"], remote.current_host({"channel": "canary"})["candidate_id"]), (a, b))


class GateTests(Host):
    def test_the_idle_probe_decides_the_gate_and_cleanup_is_required(self):
        a = self.candidate("a")
        with patch.object(remote, "start_host"), patch.object(remote, "status_host", return_value={"service_stopped": True, "cleanup_confirmed": True}):
            self.assertTrue(remote.idle_probe(a)["ok"])
        with patch.object(remote, "start_host"), patch.object(remote, "status_host", return_value={"service_stopped": True, "cleanup_confirmed": False}):
            self.assertFalse(remote.idle_probe(a)["ok"])
        with patch.object(remote, "start_host", side_effect=RuntimeError("systemd refused the run")):
            self.assertIn("systemd refused", remote.idle_probe(a)["detail"])

    def test_the_gate_stops_at_the_first_failing_step_before_any_probe(self):
        a = self.candidate("a")
        failing = type("R", (), {"returncode": 1, "stdout": "", "stderr": "bwrap missing"})()
        with patch.object(remote.subprocess, "run", return_value=failing), patch.object(remote, "idle_probe") as probe:
            verdict = remote.run_gate(a)
        self.assertFalse(verdict["ok"])
        probe.assert_not_called()


class PruneTests(Host):
    def test_prune_keeps_current_previous_and_every_candidate_a_run_still_needs(self):
        ids = [self.candidate(name) for name in "abcdef"]
        for candidate_id in ids[:3]:
            self.promote(candidate_id)                        # c current, b previous, a older
        self.make_run("done", ids[3], **{"checker-exit.json": 1})  # finished and checked: not a pin
        self.make_run("drained", ids[4], **{"resume.json": 1, "checker-exit.json": 1})
        self.make_run("unchecked", ids[5])
        dry = remote.prune_host({"keep": 2})
        self.assertTrue(dry["dry_run"])
        self.assertEqual(sorted(dry["would_remove"]), sorted([ids[0], ids[3]]))
        self.assertTrue(all((self.data / i).exists() for i in ids))
        done = remote.prune_host({"keep": 2, "yes": True})
        self.assertEqual(sorted(done["removed"]), sorted([ids[0], ids[3]]))
        self.assertEqual([i for i in ids if (self.data / i).exists()], ids[1:3] + ids[4:])


class ControlTests(Host):
    def test_drain_writes_the_control_file_then_waits_for_the_stop(self):
        a = self.candidate("a")
        path = self.make_run("run-1", a)
        states = iter([{"service_stopped": False}, {"service_stopped": False}, {"service_stopped": True, "progress": {"held_microusd": 0}}])
        remote.write_json(path / "resume.json", {"held_microusd": 0, "last_seq": 9})
        with patch.object(remote, "status_host", side_effect=lambda run_id: next(states)):
            result = remote.control_host({"run_id": "run-1", "mode": "drain", "grace_seconds": 5}, sleep=lambda _: None)
        control = json.loads((path / "control.json").read_text())
        self.assertEqual((control["mode"], control["grace_seconds"]), ("drain", 5))
        self.assertTrue(result["drained"])
        self.assertEqual(result["resume"]["held_microusd"], 0)

    def test_a_stopped_run_cannot_be_drained_and_modes_are_closed(self):
        a = self.candidate("a")
        self.make_run("run-1", a)
        with patch.object(remote, "status_host", return_value={"service_stopped": True}):
            with self.assertRaisesRegex(ValueError, "not running"):
                remote.control_host({"run_id": "run-1", "mode": "drain"})
        with self.assertRaisesRegex(ValueError, "Mode"):
            remote.control_host({"run_id": "run-1", "mode": "kill"})

    def test_resume_runs_the_candidates_own_plan_and_never_starts_anything(self):
        a = self.candidate("a")
        self.make_run("run-1", a, **{"resume.json": 1})
        plan = {"last_seq": 40, "charge_microusd": 0, "respawn_agents": ["agent-01"]}
        done = type("R", (), {"returncode": 0, "stdout": json.dumps(plan), "stderr": ""})()
        with patch.object(remote, "status_host", return_value={"service_stopped": True}), patch.object(remote.subprocess, "run", return_value=done) as run:
            result = remote.resume_host({"run_id": "run-1", "candidate_id": a})
        self.assertEqual(run.call_args.args[0][1:3], ["resume-plan", "--run-dir"])
        self.assertEqual((result["plan"], result["executable"]), (plan, False))
        bad = type("R", (), {"returncode": 1, "stdout": "", "stderr": "state schema 2 is not resumable"})()
        with patch.object(remote, "status_host", return_value={"service_stopped": True}), patch.object(remote.subprocess, "run", return_value=bad):
            with self.assertRaisesRegex(ValueError, "schema 2"):
                remote.resume_host({"run_id": "run-1", "candidate_id": a})
        with patch.object(remote, "status_host", return_value={"service_stopped": False}):
            with self.assertRaisesRegex(ValueError, "stopped"):
                remote.resume_host({"run_id": "run-1", "candidate_id": a})


class DoctorTests(Host):
    def test_doctor_reports_each_failure_without_a_secret(self):
        a = self.candidate("a")
        self.promote(a)
        (self.state / "credentials").mkdir()
        old = self.state / "credentials/run-1-abc"
        old.write_text("sk-secret-value")
        os.utime(old, (time.time() - 3600,) * 2)
        with patch.object(remote.shutil, "which", return_value=None):
            report = remote.doctor_host({"client_time_unix": time.time() - 500})
        by_name = {c["name"]: c for c in report["checks"]}
        self.assertEqual(report["verdict"], "FAIL")
        self.assertEqual(by_name["clock"]["level"], "fail")
        self.assertEqual(by_name["stale-credentials"]["level"], "warn")
        self.assertFalse(by_name["bubblewrap"]["ok"])
        self.assertTrue(by_name["channel-stable"]["ok"])
        self.assertNotIn("sk-secret-value", json.dumps(report))

    def test_doctor_fails_a_channel_whose_candidate_was_altered(self):
        a = self.candidate("a")
        self.promote(a)
        (self.data / a / "runtime").write_bytes(b"x")
        report = remote.doctor_host({})
        self.assertFalse({c["name"]: c for c in report["checks"]}["channel-stable"]["ok"])

    def test_controller_key_check_reports_presence_only(self):
        auth = Path(self.tmp.name) / "auth.json"
        auth.write_text(json.dumps({"deepseek": {"type": "api_key", "key": "sk-live-value"}}))
        present = remote.controller_key_check(auth, "deepseek")
        self.assertTrue(present["ok"])
        self.assertNotIn("sk-live-value", json.dumps(present))
        self.assertFalse(remote.controller_key_check(auth, "venice")["ok"])
        self.assertFalse(remote.controller_key_check(Path(self.tmp.name) / "none.json", "deepseek")["ok"])


if __name__ == "__main__":
    unittest.main()
