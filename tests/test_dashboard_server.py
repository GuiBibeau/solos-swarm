"""Read-only telemetry projection and HTTP boundary checks; no live swarm is started."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest
from unittest.mock import patch
from urllib.error import HTTPError
from urllib.request import Request, urlopen

MODULE = Path(__file__).resolve().parents[1] / "scripts/dashboard_server.py"
spec = importlib.util.spec_from_file_location("dashboard_server", MODULE)
server_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(server_module)


def fixture():
    state = {
        "schema_version": 1, "goal_status": "running", "spent_microusd": 0, "held_microusd": 1200,
        "agents": {"agent-00": {"started_ms": 123, "stopped_ms": None, "model_calls": 0, "successful_model_calls": 0, "status": "running", "terminal": "PRIVATE", "notes": ["PRIVATE"]}},
        "tasks": [{"id": "a00", "claimed_by": "agent-00", "status": "claimed", "budget_microusd": 3000000, "input": {"secret": "PRIVATE"}}],
        "intents": [{"agent_id": "agent-00", "state": "completed", "hold_microusd": 0, "cost_microusd": 0, "provider": "deepseek", "usage": {"input_tokens": 0, "output_tokens": 0, "private": "PRIVATE"}, "request_id": "PRIVATE"}],
        "findings": [{"agent_id": "agent-00", "task_id": "a00", "result": {"papers": ["PRIVATE"]}}],
        "board": [{"message": "PRIVATE"}], "tokens": {"agent-00": "PRIVATE"}, "artifact": {"secret": "PRIVATE"},
    }
    config = {"intent": "Inspect one fleet", "agent_count": 20, "budget_microusd": 100000000, "model": "deepseek-flash", "provider": "deepseek", "grants": ["claim_task", "board_read"], "worker_prompt": "PRIVATE", "synthesis_prompt": "PRIVATE", "api_key": "PRIVATE"}
    return state, config


class ProjectionTests(unittest.TestCase):
    def test_allowlist_preserves_real_zero_and_configured_capacity(self):
        state, config = fixture()
        packet = server_module.project_runtime(state, config, "vm-run", 456, False)
        self.assertNotIn("PRIVATE", json.dumps(packet))
        self.assertEqual(packet["schema"], "solos-swarm.telemetry/v1")
        self.assertEqual(packet["config"]["max_agents"], 20)
        self.assertIsNone(packet["config"]["keys_in_workers"])
        self.assertEqual(len(packet["state"]["agents"]), 1)
        self.assertEqual(packet["config"]["capabilities"], ["claim_task", "board_read"])
        self.assertEqual(packet["state"]["spent_microusd"], 0)
        self.assertEqual(packet["state"]["agents"]["agent-00"]["model_calls"], 0)
        self.assertIsNone(packet["state"]["tasks"][0]["fencing_token"])
        self.assertEqual(packet["state"]["findings"][0], {"agent_id": "agent-00", "task_id": "a00", "has_result": True})
        self.assertIs(packet["source"]["service_active"], False)

    def test_missing_values_remain_unknown_and_kernel_format_is_explicitly_rejected(self):
        packet = server_module.project_runtime({"agents": {}, "tasks": []}, {}, "empty", 0)
        self.assertIsNone(packet["config"]["budget_microusd"])
        self.assertIsNone(packet["state"]["spent_microusd"])
        self.assertIsNone(packet["state"]["intents"])
        self.assertIsNone(packet["source"]["service_active"])
        with self.assertRaisesRegex(ValueError, "Unsupported snapshot"):
            server_module.project_runtime({"snapshot": {"domain": {}}}, {}, "kernel", 0)

    def test_lima_is_bounded_read_only_and_cached(self):
        for vm, run in [("--help", "valid"), ("swarm", "../secret"), ("swarm;touch", "valid")]:
            with self.assertRaises(ValueError):
                server_module.RuntimeSource(lima=vm, run=run)
        packet = server_module.project_runtime(*fixture(), "vm-run", 0)
        with patch.object(server_module.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(packet), "")) as call:
            source = server_module.RuntimeSource(lima="swarm", run="vm-run")
            self.assertEqual(source.read(), source.read())
            self.assertEqual(call.call_count, 1)
            args, kwargs = call.call_args
            self.assertEqual(args[0][:6], ["limactl", "shell", "swarm", "--", "python3", "-c"])
            self.assertEqual(args[0][-1], "vm-run")
            self.assertEqual(kwargs["timeout"], 8)
            self.assertNotIn("shell", kwargs)
            self.assertIn("project_runtime(state, config", args[0][6])
            self.assertNotIn("start", args[0][:6])
        with patch.object(server_module.subprocess, "run", side_effect=subprocess.TimeoutExpired("limactl", 8)) as call:
            source = server_module.RuntimeSource(lima="swarm", run="vm-run")
            for _ in range(2):
                with self.assertRaises(server_module.TelemetryUnavailable):
                    source.read()
            self.assertEqual(call.call_count, 1)

    def test_guest_reader_executes_same_projection_without_private_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / ".local/state/solos-swarm/runs/vm-run"
            root.mkdir(parents=True)
            state, config = fixture()
            state["goal_status"] = "stopped"
            (root / "state.json").write_text(json.dumps(state))
            (root / "config.yaml").write_text(json.dumps(config))
            # Rebind home only inside this isolated subprocess fixture.
            env = dict(os.environ, HOME=directory)
            result = subprocess.run(["python3", "-c", server_module.guest_reader(), "vm-run"], capture_output=True, text=True, check=True, env=env)
            self.assertNotIn("PRIVATE", result.stdout)
            self.assertEqual(json.loads(result.stdout)["config"]["max_agents"], 20)

    def test_latest_follows_newest_state_and_ignores_incomplete_runs(self):
        with tempfile.TemporaryDirectory() as directory:
            runs = Path(directory) / ".local/state/solos-swarm/runs"
            state, config = fixture()
            state["goal_status"] = "stopped"
            for name, modified in [("older", 100), ("newer", 200), (".hidden", 900)]:
                root = runs / name
                root.mkdir(parents=True)
                (root / "state.json").write_text(json.dumps(state))
                (root / "config.yaml").write_text(json.dumps(config))
                os.utime(root / "state.json", (modified, modified))
            (runs / "incomplete").mkdir()
            env = dict(os.environ, HOME=directory)
            command = ["python3", "-c", server_module.guest_reader(), "--latest"]
            first = subprocess.run(command, capture_output=True, text=True, check=True, env=env)
            self.assertEqual(json.loads(first.stdout)["source"]["label"], "newer")
            os.utime(runs / "older/state.json", (300, 300))
            second = subprocess.run(command, capture_output=True, text=True, check=True, env=env)
            self.assertEqual(json.loads(second.stdout)["source"]["label"], "older")
        with patch.object(server_module.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, first.stdout, "")) as call:
            server_module.RuntimeSource(lima="swarm", latest=True).read()
            self.assertEqual(call.call_args.args[0][-1], "--latest")
        with self.assertRaises(ValueError):
            server_module.RuntimeSource(lima="swarm", run="older", latest=True)


class HttpTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.dashboard = self.root / "dashboard"
        self.dashboard.mkdir()
        (self.dashboard / "index.html").write_text("<title>Telemetry</title>")
        (self.dashboard / ".private").write_text("PRIVATE")
        (self.root / "outside.txt").write_text("PRIVATE")
        (self.dashboard / "escape.txt").symlink_to(self.root / "outside.txt")
        self.state = self.root / "state.json"
        state, config = fixture()
        self.state.write_text(json.dumps(state))
        (self.root / "config.yaml").write_text(json.dumps(config))
        self.source = server_module.RuntimeSource(state=self.state)
        self.server = server_module.make_server(self.source, port=0, dashboard=self.dashboard)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = "http://127.0.0.1:" + str(self.server.server_port)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.temp.cleanup()

    def test_api_and_static_routes_are_read_only(self):
        with urlopen(self.base + "/api/swarm") as response:
            self.assertEqual(response.headers["Cache-Control"], "no-store")
            self.assertIsNone(response.headers.get("Access-Control-Allow-Origin"))
            body = response.read()
            self.assertNotIn(b"PRIVATE", body)
            self.assertEqual(json.loads(body)["config"]["budget_microusd"], 100000000)
        with urlopen(Request(self.base + "/", method="HEAD")) as response:
            self.assertEqual(response.read(), b"")
            self.assertEqual(response.headers["Content-Type"], "text/html")
        for route in ["/.private", "/%2e%2e/outside.txt", "/escape.txt", "/snapshots/"]:
            with self.assertRaises(HTTPError) as error:
                urlopen(self.base + route)
            self.assertEqual(error.exception.code, 404)
            error.exception.close()
        with self.assertRaises(HTTPError) as error:
            urlopen(Request(self.base + "/api/swarm", data=b"{}", method="POST"))
        self.assertEqual(error.exception.code, 405)
        error.exception.close()
        with self.assertRaises(HTTPError) as error:
            urlopen(Request(self.base + "/api/swarm", headers={"Host": "foreign.example"}))
        self.assertEqual(error.exception.code, 403)
        error.exception.close()

    def test_missing_source_is_503_without_demo_or_path_leak(self):
        self.state.unlink()
        with self.assertRaises(HTTPError) as error:
            urlopen(self.base + "/api/swarm")
        self.assertEqual(error.exception.code, 503)
        body = error.exception.read().decode()
        self.assertEqual(json.loads(body), {"error": "Swarm telemetry is unavailable."})
        self.assertNotIn(str(self.root), body)
        self.assertNotIn("swarm-100", body)
        error.exception.close()

    def test_explicit_missing_config_is_unavailable(self):
        source = server_module.RuntimeSource(state=self.state, config=self.root / "missing-config.json")
        with self.assertRaises(server_module.TelemetryUnavailable):
            source.read()


if __name__ == "__main__":
    unittest.main()
