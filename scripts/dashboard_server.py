#!/usr/bin/env python3
"""Serve the dashboard and a public, read-only projection of one retained swarm run."""

import argparse
import inspect
import json
import mimetypes
from pathlib import Path
import re
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote, urlsplit

DASHBOARD = Path(__file__).resolve().parent.parent / "dashboard"
NAME = re.compile(r"[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}\Z")
RUN_ID = re.compile(r"[a-z0-9][a-z0-9-]{0,47}\Z")
MAX_BYTES = 32 * 1024 * 1024
YAML_DEPENDENCY_ERROR = "YAML configuration requires PyYAML; install the dashboard Python requirements in the source environment."


class TelemetryUnavailable(Exception):
    pass


class YamlDependencyMissing(ValueError):
    pass


def project_runtime(state, config, label, updated_at, service_active=None):
    """An allowlist, never a recursive redactor: private run fields cannot cross it."""
    def text(value, limit=300):
        return value[:limit] if isinstance(value, str) else None

    def number(value):
        return value if type(value) is int and value >= 0 else None

    def records(value):
        return [item for item in value if isinstance(item, dict)] if isinstance(value, list) else []

    if not isinstance(state, dict) or not isinstance(config, dict):
        raise ValueError("Unsupported snapshot format")
    if "snapshot" in state or "domain" in state:
        raise ValueError("Unsupported snapshot format")
    if not isinstance(state.get("agents"), dict) or not isinstance(state.get("tasks"), list):
        raise ValueError("Unsupported snapshot format")
    agents = {}
    for agent_id, agent in state["agents"].items():
        if not isinstance(agent_id, str) or not isinstance(agent, dict):
            continue
        agents[agent_id[:128]] = {
            "started_ms": number(agent.get("started_ms")),
            "stopped_ms": number(agent.get("stopped_ms")),
            "model_calls": number(agent.get("model_calls")),
            "successful_model_calls": number(agent.get("successful_model_calls")),
            "status": text(agent.get("status"), 40),
        }
    tasks = [{
        "id": text(task.get("id"), 128),
        "claimed_by": text(task.get("claimed_by"), 128),
        "status": text(task.get("status"), 40),
        "budget_microusd": number(task.get("budget_microusd")),
        "fencing_token": number(task.get("fencing_token")),
    } for task in records(state.get("tasks"))]
    intents = []
    for intent in records(state.get("intents")):
        usage = intent.get("usage")
        intents.append({
            "agent_id": text(intent.get("agent_id"), 128),
            "state": text(intent.get("state"), 40),
            "hold_microusd": number(intent.get("hold_microusd")),
            "cost_microusd": number(intent.get("cost_microusd")),
            "usage": {key: number(usage.get(key)) for key in ("input_tokens", "output_tokens")} if isinstance(usage, dict) else None,
            "provider": text(intent.get("provider"), 80),
        })
    findings = []
    for finding in records(state.get("findings")):
        public = {
            "agent_id": text(finding.get("agent_id"), 128),
            "task_id": text(finding.get("task_id"), 128),
            "has_result": finding.get("result") is not None,
        }
        if isinstance(finding.get("summary"), str):
            public["summary"] = text(finding["summary"])
        findings.append(public)
    upstream = config.get("upstream") if isinstance(config.get("upstream"), dict) else {}
    capabilities = config.get("capabilities", config.get("grants", []))
    return {
        "schema": "solos-swarm.telemetry/v1",
        "source": {"kind": "runtime-file", "label": text(label, 128), "updated_at": number(updated_at), "connected": True, "service_active": service_active if type(service_active) is bool else None},
        "config": {
            "goal_id": text(config.get("goal_id") or label, 128),
            "intent": text(config.get("intent"), 1000),
            "budget_microusd": number(config.get("budget_microusd")),
            "max_agents": number(config.get("agent_count", config.get("max_agents"))),
            "model": text(upstream.get("model") or config.get("model"), 128),
            "provider": text(upstream.get("provider") or config.get("provider"), 80),
            "keys_in_workers": number(config.get("keys_in_workers")),
            "capabilities": [item[:128] for item in capabilities if isinstance(item, str)] if isinstance(capabilities, list) else [],
        },
        "state": {
            "goal_status": text(state.get("goal_status"), 40),
            "spent_microusd": number(state.get("spent_microusd")),
            "held_microusd": number(state.get("held_microusd")),
            "agents": agents, "tasks": tasks,
            "intents": intents if isinstance(state.get("intents"), list) else None,
            "findings": findings,
        },
    }


def read_file(path, optional=False):
    try:
        with Path(path).open("rb") as handle:
            data = handle.read(MAX_BYTES + 1)
    except FileNotFoundError:
        if optional:
            return None
        raise
    if len(data) > MAX_BYTES:
        raise ValueError("Run file exceeds limit")
    return data


def read_json(path, optional=False):
    data = read_file(path, optional)
    return json.loads(data) if data is not None else {}


def read_config(path, optional=False):
    data = read_file(path, optional)
    if data is None:
        return {}
    try:
        config = json.loads(data)
    except json.JSONDecodeError:
        try:
            import yaml
        except ImportError as error:
            raise YamlDependencyMissing(YAML_DEPENDENCY_ERROR) from error
        try:
            config = yaml.safe_load(data)
        except yaml.YAMLError as error:
            raise ValueError("Invalid YAML configuration") from error
    if not isinstance(config, dict):
        raise ValueError("Configuration must be an object")
    return config


def guest_reader():
    # Only this trusted projection travels to the guest; no private file is printed.
    return "\n".join([
        "import json, pathlib, re, subprocess, sys",
        "MAX_BYTES = " + str(MAX_BYTES),
        "YAML_DEPENDENCY_ERROR = " + repr(YAML_DEPENDENCY_ERROR),
        inspect.getsource(YamlDependencyMissing), inspect.getsource(project_runtime),
        inspect.getsource(read_file), inspect.getsource(read_json), inspect.getsource(read_config),
        "runs = pathlib.Path.home() / '.local/state/solos-swarm/runs'",
        "selection = sys.argv[1]",
        "if selection == '--latest':",
        "    candidates = [path for path in runs.iterdir() if re.fullmatch(r'[a-z0-9][a-z0-9-]{0,47}', path.name) and path.is_dir() and (path / 'state.json').is_file()]",
        "    root = max(candidates, key=lambda path: ((path / 'state.json').stat().st_mtime_ns, path.name))",
        "else: root = runs / selection",
        "run_id = root.name",
        "Path = pathlib.Path",
        "state = read_json(root / 'state.json')",
        "config = read_config(root / 'config.yaml', optional=True)",
        "active = None",
        "if state.get('goal_status') == 'running':",
        "    try:",
        "        result = subprocess.run(['systemctl', '--user', 'is-active', 'solos-swarm-' + run_id + '.service'], capture_output=True, text=True, timeout=1.5)",
        "        status = result.stdout.strip()",
        "        active = True if status == 'active' else False if status in ('inactive', 'failed', 'deactivating') else None",
        "    except (OSError, subprocess.TimeoutExpired): pass",
        "print(json.dumps(project_runtime(state, config, run_id, int((root / 'state.json').stat().st_mtime * 1000), active)))",
    ])


class RuntimeSource:
    def __init__(self, state=None, config=None, lima=None, run=None, latest=False):
        if bool(state) == bool(lima) or (not lima and (run or latest)):
            raise ValueError("Choose one valid state file or Lima run")
        if lima and (not NAME.fullmatch(lima) or bool(run) == bool(latest) or (run and not RUN_ID.fullmatch(run))):
            raise ValueError("Choose one valid Lima run or latest run")
        self.state = Path(state) if state else None
        self.config = Path(config) if config else self.state.with_name("config.yaml") if state else None
        self.config_optional = config is None
        self.lima, self.run, self.latest = lima, run, latest
        self.lock = threading.Lock()
        self.cached, self.error, self.checked = None, None, float("-inf")

    def read(self):
        with self.lock:
            if time.monotonic() - self.checked >= 3:
                previous_error = self.error
                try:
                    if self.lima:
                        result = subprocess.run(["limactl", "shell", self.lima, "--", "python3", "-c", guest_reader(), "--latest" if self.latest else self.run], capture_output=True, text=True, timeout=8, check=True)
                        self.cached = json.loads(result.stdout)
                        if not isinstance(self.cached, dict) or self.cached.get("schema") != "solos-swarm.telemetry/v1":
                            raise ValueError("Unsupported snapshot format")
                    else:
                        state = read_json(self.state)
                        config = read_config(self.config, optional=self.config_optional)
                        label = self.state.parent.name if self.state.name == "state.json" else self.state.name
                        self.cached = project_runtime(state, config, label, int(self.state.stat().st_mtime * 1000))
                    self.error = None
                except YamlDependencyMissing:
                    self.cached = None
                    self.error = YAML_DEPENDENCY_ERROR
                except subprocess.CalledProcessError as error:
                    self.cached = None
                    self.error = YAML_DEPENDENCY_ERROR if YAML_DEPENDENCY_ERROR in (error.stderr or "") else "Swarm telemetry is unavailable."
                except (OSError, ValueError, subprocess.SubprocessError):
                    self.cached = None
                    self.error = "Swarm telemetry is unavailable."
                if self.error == YAML_DEPENDENCY_ERROR and self.error != previous_error:
                    print(self.error, file=sys.stderr)
                self.checked = time.monotonic()
            if self.error:
                raise TelemetryUnavailable(self.error)
            return self.cached


def make_server(source, host="127.0.0.1", port=4173, dashboard=DASHBOARD):
    root = Path(dashboard).resolve()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def reply(self, code, body, content_type):
            self.send_response(code)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)

        def do_GET(self):
            allowed_hosts = {"127.0.0.1:" + str(self.server.server_port), "localhost:" + str(self.server.server_port)}
            if self.headers.get("Host", "").lower() not in allowed_hosts:
                self.reply(403, b"Local access only", "text/plain")
                return
            path = unquote(urlsplit(self.path).path)
            if path == "/api/swarm":
                try:
                    self.reply(200, json.dumps(source.read()).encode(), "application/json")
                except TelemetryUnavailable:
                    self.reply(503, b'{"error":"Swarm telemetry is unavailable."}', "application/json")
                return
            if "\\" in path or any(part.startswith(".") for part in path.split("/")):
                self.reply(404, b"Not found", "text/plain")
                return
            target = (root / (path.lstrip("/") or "index.html")).resolve()
            if not target.is_relative_to(root) or not target.is_file():
                self.reply(404, b"Not found", "text/plain")
                return
            try:
                body = target.read_bytes()
            except OSError:
                self.reply(404, b"Not found", "text/plain")
                return
            self.reply(200, body, mimetypes.guess_type(target.name)[0] or "application/octet-stream")

        do_HEAD = do_GET

        def method_not_allowed(self):
            self.reply(405, b"Method not allowed", "text/plain")

        do_POST = do_PUT = do_PATCH = do_DELETE = do_OPTIONS = method_not_allowed

    return ThreadingHTTPServer((host, port), Handler)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--state", type=Path)
    group.add_argument("--lima")
    parser.add_argument("--config", type=Path)
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument("--run")
    selection.add_argument("--latest", action="store_true", help="Follow the run with the most recently updated state file")
    parser.add_argument("--port", type=int, default=4173)
    args = parser.parse_args()
    if args.lima and (args.config or not (args.run or args.latest)):
        parser.error("--lima requires --run or --latest and cannot use --config")
    if args.state and (args.run or args.latest):
        parser.error("--run and --latest require --lima")
    try:
        source = RuntimeSource(args.state, args.config, args.lima, args.run, args.latest)
        server = make_server(source, port=args.port)
    except (ValueError, OSError):
        parser.error("Invalid source or unavailable local port")
    print(f"Dashboard: http://127.0.0.1:{server.server_port}/?source=runtime", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
