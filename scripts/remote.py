#!/usr/bin/env python3
"""Build one immutable Linux candidate, then run and inspect it over SSH."""

import argparse
import base64
import contextlib
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tarfile
import tempfile
import time

REPO = Path(__file__).resolve().parent.parent
# Neutral SSH alias. Override with --host or SOLOS_SWARM_SSH_TARGET.
EXAMPLE_SSH_HOST = "solos-node"
SSH_HOST = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.@:-]{0,127}\Z")
NODE_SOURCE = Path(os.environ["SWARM_NODE_DIR"]) if os.environ.get("SWARM_NODE_DIR") else Path.home() / ".local/opt/node-v24.18.0-linux-x64"
DATA = Path.home() / ".local/share/solos-swarm"
STATE = Path.home() / ".local/state/solos-swarm"
# With --refuse-peak the broker pauses model calls in weekday peak windows and the Goal deadline stops counting;
# the service wall-clock backstop widens by this much (one day: at most 7 h of peak per weekday plus margin).
PEAK_SLACK_SECONDS = 86_400
RUN_ID = re.compile(r"[a-z0-9][a-z0-9-]{0,47}\Z")
CANDIDATE_ID = re.compile(r"[a-f0-9]{64}\Z")
CHANNELS = ("stable", "canary")
HISTORY = 5  # previous candidates a channel pointer remembers for rollback


def digest(path):
    hasher = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            hasher.update(block)
    return hasher.hexdigest()


def inventory(root):
    result = {}
    for path in sorted(root.rglob("*")):
        name = str(path.relative_to(root))
        if name == "candidate.json":
            continue
        if path.is_symlink():
            # Installed npm bin links are allowed only inside the candidate.
            if not path.resolve().is_relative_to(root.resolve()):
                raise ValueError(f"Candidate symlink escapes its directory: {name}")
            result[name] = {"symlink": os.readlink(path)}
        elif path.is_file():
            result[name] = {"sha256": digest(path)}
    return result


def candidate_path(candidate_id):
    if not CANDIDATE_ID.fullmatch(candidate_id):
        raise ValueError("Candidate must be a lowercase SHA-256 digest")
    return DATA / candidate_id


def run_path(run_id):
    if not RUN_ID.fullmatch(run_id):
        raise ValueError("Run ID must contain 1-48 lowercase letters, digits, or hyphens")
    return STATE / "runs" / run_id


def unit_name(run_id):
    run_path(run_id)
    return f"solos-swarm-{run_id}.service"


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


@contextlib.contextmanager
def lock(name):
    directory = STATE / "locks"
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (directory / name).open("a") as handle:
        fcntl.flock(handle, fcntl.LOCK_EX)
        yield


def verify_candidate(candidate_id):
    path = candidate_path(candidate_id)
    manifest = json.loads((path / "candidate.json").read_text())
    if manifest["candidate_id"] != candidate_id or manifest["files"] != inventory(path):
        raise ValueError("Candidate artifacts do not match their frozen manifest")
    if "tree_sha256" in manifest and manifest["tree_sha256"] != tree_hash(manifest["files"]):
        raise ValueError("Candidate tree hash does not match its manifest")
    return path, manifest


def tree_hash(files):
    """One digest over every file hash in a candidate: what a channel pointer pins."""
    return hashlib.sha256(json.dumps(files, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def deploy_host():
    archive = sys.stdin.buffer.read()
    candidate_id = hashlib.sha256(archive).hexdigest()
    path = candidate_path(candidate_id)
    DATA.mkdir(parents=True, exist_ok=True, mode=0o700)
    with lock(f"candidate-{candidate_id}"):
        if path.exists():
            verify_candidate(candidate_id)
            return {"candidate_id": candidate_id, "path": str(path), "existing": True}
        staging = Path(tempfile.mkdtemp(prefix=".build-", dir=DATA))
        try:
            with tarfile.open(fileobj=io.BytesIO(archive)) as bundle:
                for member in bundle.getmembers():
                    parts = Path(member.name).parts
                    if not member.isfile() or Path(member.name).is_absolute() or ".." in parts:
                        raise ValueError("Source bundle contains an unsafe archive member")
                bundle.extractall(staging, filter="data")
            if not NODE_SOURCE.is_dir():
                raise ValueError(f"Pinned Node 24.18.0 installation is missing: {NODE_SOURCE}")
            shutil.copytree(NODE_SOURCE, staging / "node", symlinks=True)
            env = {**os.environ, "PATH": f"{staging / 'node/bin'}:{Path.home() / '.cargo/bin'}:{os.environ.get('PATH', '/usr/bin:/bin')}"}
            with (staging / "build.log").open("w") as log:
                for command in (
                    ["cargo", "build", "--release", "--locked"],
                    ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"],
                ):
                    subprocess.run(command, cwd=staging, env=env, stdout=log, stderr=log, check=True)
            binary = staging / "target/release/solos-swarm"
            if not binary.is_file():
                raise ValueError("Build did not produce target/release/solos-swarm")
            saved_binary = staging / ".runtime"
            binary.rename(saved_binary)
            shutil.rmtree(staging / "target")
            binary.parent.mkdir(parents=True)
            saved_binary.rename(binary)
            files = inventory(staging)
            manifest = {
                "schema_version": 1, "candidate_id": candidate_id, "tree_sha256": tree_hash(files),
                "source_archive_sha256": candidate_id,
                "source": json.loads((staging / "source-metadata.json").read_text()),
                "build_target": subprocess.check_output(["rustc", "-vV"], env=env, text=True).strip(),
                "node_version": subprocess.check_output([str(staging / "node/bin/node"), "--version"], text=True).strip(),
                "topology": "single-linux-host", "files": files,
            }
            write_json(staging / "candidate.json", manifest)
            for item in staging.rglob("*"):
                if not item.is_symlink():
                    item.chmod(item.stat().st_mode & ~0o222)
            staging.chmod(0o555)
            staging.rename(path)
            return {"candidate_id": candidate_id, "path": str(path), "existing": False}
        except Exception:
            # Preserve a failed build's log for diagnosis; no model call has started.
            raise RuntimeError(f"Candidate preparation failed; build evidence retained at {staging}") from None


def progress_brief(path):
    """Counts and ledger from state.json; the host runs this file standalone, so it cannot import swarm_progress."""
    try:
        state = json.loads((path / "state.json").read_text())
        cap = json.loads((path / "config.yaml").read_text()).get("budget_microusd")
    except (OSError, ValueError):
        return None
    filed = {finding["agent_id"] for finding in state["findings"]}
    deferred = {finding["agent_id"] for finding in state["findings"]
                if all(row.get("severity") == "defer" for rows in finding["result"].values() if isinstance(rows, list) for row in rows)}
    agents = state["agents"]
    return {"goal_status": state["goal_status"], "agents": len(agents),
            "running": sum(1 for name, a in agents.items() if a["status"] == "running" and name not in filed),
            "filed": len(filed - deferred), "deferred_or_partial": len(deferred),
            "spent_microusd": state["spent_microusd"], "held_microusd": state["held_microusd"], "cap_microusd": cap,
            "artifact": state["artifact"] is not None, "pause": state.get("pause"),
            "resumable": (path / "resume.json").exists(),
            "detail": "python3 scripts/swarm_progress.py --run-id " + path.name}


def status_host(run_id):
    path = run_path(run_id)
    if not path.is_dir():
        raise ValueError("Run ID does not exist")
    returncode, properties = service_properties(run_id)
    stopped = returncode == 0 and properties.get("ActiveState") in ("inactive", "failed")
    output = {"run_id": run_id, "path": str(path), "service": properties, "service_stopped": stopped,
              "request": json.loads((path / "request.json").read_text())}
    for name in ("report", "cleanup", "service-cleanup"):
        artifact = path / f"{name}.json"
        if artifact.exists():
            output[name] = json.loads(artifact.read_text())
    brief = progress_brief(path)
    if brief:
        output["progress"] = brief
    output["cleanup_confirmed"] = (stopped and output.get("cleanup", {}).get("verdict") == "PASS"
                                   and output.get("service-cleanup", {}).get("credential_removed") is True)
    return output


def validate_goal_limits(goal):
    """Refuse a frozen Goal outside the preview host's admitted envelope."""
    if (
        not 1 <= goal["agent_count"] <= 30
        or goal["agent_count"] != len(goal["tasks"])
        or not 1 <= goal["deadline_seconds"] <= 21_600
        or goal["budget_microusd"] > 1_000_000_000_000
    ):
        raise ValueError("Runs require 1-30 agents, one task per agent, deadline <=21600s, and budget <=$1000000")


def start_host(request):
    run_id = request["run_id"]
    path = run_path(run_id)
    identity = {key: request[key] for key in ("candidate_id", "seed", "run_id")}
    identity["idle_probe"] = request.get("idle_probe", False)
    identity["deadline_seconds"] = request.get("deadline_seconds")
    if request.get("refuse_peak"):
        identity["refuse_peak"] = True
    if request.get("case_sha256"):
        identity["case_sha256"] = request["case_sha256"]
    with lock(f"run-{run_id}"):
        if path.exists():
            if json.loads((path / "request.json").read_text()) != identity:
                raise ValueError("Run ID already belongs to a different candidate or frozen case")
            return {**status_host(run_id), "existing": True}
        candidate, _ = verify_candidate(request["candidate_id"])
        api_key = request.get("api_key")
        if not isinstance(api_key, str) or not api_key.strip():
            return {"needs_credential": True}
        path.mkdir(parents=True, mode=0o700)
        write_json(path / "request.json", identity)
        private = STATE / "evaluators" / run_id
        private.mkdir(parents=True, mode=0o700)
        credential = None
        try:
            if "goal" in request and "expected" in request:
                (path / "config.yaml").write_text(request["goal"])
                (private / "expected.json").write_text(request["expected"])
            else:
                subprocess.run(["python3", str(candidate / "scripts/prepare_case.py"), "--seed", str(request["seed"]),
                                "--goal", str(path / "config.yaml"), "--expected", str(private / "expected.json")],
                               stdout=(path / "prepare.log").open("w"), stderr=subprocess.STDOUT, check=True)
            goal = json.loads((path / "config.yaml").read_text())
            validate_goal_limits(goal)
            credentials = STATE / "credentials"
            credentials.mkdir(parents=True, exist_ok=True, mode=0o700)
            # shortcut: a host crash before service creation can retain this private file; use an in-memory handoff before production.
            fd, credential = tempfile.mkstemp(prefix=f"{run_id}-", dir=credentials)
            os.fchmod(fd, 0o600)
            with os.fdopen(fd, "w") as handle:
                handle.write(api_key)
            command = [str(candidate / "target/release/solos-swarm"), "run", "--config", str(path / "config.yaml"),
                       "--run-dir", str(path), "--pi-root", str(candidate), "--node", str(candidate / "node/bin/node"),
                       "--credential-file", credential]
            if identity["idle_probe"]:
                command.append("--idle-probe")
            deadline = identity["deadline_seconds"] or goal["deadline_seconds"]
            if not 1 <= deadline <= goal["deadline_seconds"]:
                raise ValueError("Probe deadline must narrow the frozen Goal deadline")
            if identity["deadline_seconds"] is not None:
                if not identity["idle_probe"]:
                    raise ValueError("A shortened deadline is available only for an explicitly non-live idle probe")
                command.extend(["--deadline-seconds", str(deadline)])
            finish = shlex.join(["python3", str(candidate / "scripts/remote.py"), "_finalize", "--run-id", run_id,
                                 "--candidate", request["candidate_id"], "--credential-file", credential])
            service_command = ["systemd-run", "--user", "--quiet", "--unit", unit_name(run_id),
                               "--property=Type=exec", "--property=Restart=no", "--property=KillMode=control-group",
                               f"--property=RuntimeMaxSec={deadline + 20 + (PEAK_SLACK_SECONDS if identity.get('refuse_peak') else 0)}", "--property=TimeoutStopSec=15",
                               "--property=SendSIGKILL=yes", "--property=UMask=0077", "--property=NoNewPrivileges=yes",
                               "--property=LimitCORE=0",
                               f"--property=WorkingDirectory={path}", f"--property=StandardOutput=append:{path / 'stdout.log'}",
                               f"--property=StandardError=append:{path / 'stderr.log'}", f"--property=ExecStopPost={finish}",
                               "--setenv=PATH=/usr/bin:/bin", *(["--setenv=SWARM_REFUSE_PEAK=1"] if identity.get("refuse_peak") else []), "--", *command]
            result = subprocess.run(service_command, capture_output=True, text=True)
            (path / "start.log").write_text(result.stdout + result.stderr)
            if result.returncode:
                Path(credential).unlink(missing_ok=True)
                raise RuntimeError("systemd refused the run; inspect start.log")
        except Exception as error:
            if credential is not None:
                Path(credential).unlink(missing_ok=True)
            write_json(path / "setup-failure.json", {"error": str(error), "time_unix": time.time()})
            raise
        return {**status_host(run_id), "existing": False}


def finalize_host(run_id, candidate_id, credential_file):
    path, candidate = run_path(run_id), candidate_path(candidate_id)
    credential = Path(credential_file)
    if not credential.parent == STATE / "credentials" or not credential.name.startswith(run_id + "-"):
        raise ValueError("Credential cleanup path is outside this run's owned state")
    credential.unlink(missing_ok=True)
    write_json(path / "service-cleanup.json", {
        "credential_removed": not credential.exists(), "service_result": os.environ.get("SERVICE_RESULT", "unknown"),
        "exit_code": os.environ.get("EXIT_CODE", "unknown"), "exit_status": os.environ.get("EXIT_STATUS", "unknown"),
        "time_unix": time.time(), "scope": "run-owned systemd control group; evidence preserved",
    })
    if not (path / "cleanup.json").exists():
        write_json(path / "cleanup.json", {"verdict": "INCONCLUSIVE", "remaining_workers": None,
                                           "reason": "Runtime did not flush its worker cleanup evidence"})
    with (path / "checker.log").open("w") as log:
        result = subprocess.run(["python3", str(candidate / "scripts/check_run.py"), "--run-dir", str(path),
                                 "--expected", str(STATE / "evaluators" / run_id / "expected.json"),
                                 "--runtime", str(candidate / "target/release/solos-swarm"),
                                 "--extension", str(candidate / "extensions/broker.ts"),
                                 "--package-lock", str(candidate / "package-lock.json")], stdout=log, stderr=log)
    write_json(path / "checker-exit.json", {"returncode": result.returncode})


# --- Candidate channels, promotion gate, drain, resume and doctor -------------
# A channel pointer is DATA/channels/<channel>.json. It pins one candidate by id
# and by the sha256 of its file tree. Runs pin their own candidate in
# request.json, so moving a pointer never touches a run in flight.


def write_atomic(path, value):
    """Write JSON so a reader sees the old file or the new one, never a mix."""
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + f".{os.getpid()}.tmp")
    with temporary.open("w") as handle:
        handle.write(json.dumps(value, indent=2, sort_keys=True) + "\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)
    descriptor = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def pointer_path(channel):
    if channel not in CHANNELS:
        raise ValueError(f"Channel must be one of {', '.join(CHANNELS)}")
    return DATA / "channels" / f"{channel}.json"


def read_pointer(channel):
    path = pointer_path(channel)
    return json.loads(path.read_text()) if path.is_file() else None


def verify_pointer(pointer):
    """The pointer's candidate must be intact and byte-identical to what was promoted."""
    _, manifest = verify_candidate(pointer["candidate_id"])
    if pointer["tree_sha256"] != tree_hash(manifest["files"]):
        raise ValueError("Candidate tree differs from the tree this pointer was promoted with")
    return manifest


def channel_log(channel, entry):
    path = pointer_path(channel).with_suffix(".log.jsonl")
    with path.open("a") as handle:
        handle.write(json.dumps({**entry, "time_unix": time.time()}) + "\n")


def current_host(request):
    channel = request.get("channel", "stable")
    pointer = read_pointer(channel)
    if pointer is None:
        raise ValueError(f"Channel {channel} has no promoted candidate")
    verify_pointer(pointer)
    return {"channel": channel, **pointer}


def make_pointer(channel, candidate_id, previous, gate, source):
    _, manifest = verify_candidate(candidate_id)
    return {"schema_version": 1, "channel": channel, "candidate_id": candidate_id,
            "tree_sha256": tree_hash(manifest["files"]), "previous": previous[:HISTORY],
            "gate": gate, "source": source, "promoted_unix": time.time()}


def service_properties(run_id):
    command = ["systemctl", "--user", "show", unit_name(run_id), "--property=LoadState,ActiveState,SubState,Result,ExecMainStatus", "--no-pager"]
    result = subprocess.run(command, capture_output=True, text=True)
    return result.returncode, dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)


def run_gate(candidate_id):
    """Integrity, the candidate's own doctor, a zero-spend smoke, then an idle lifecycle probe."""
    path, _ = verify_candidate(candidate_id)
    steps = [{"name": "integrity", "ok": True}]
    binary = str(path / "target/release/solos-swarm")
    for name, command in (
        ("doctor", [binary, "doctor", "--pi-root", str(path), "--node", str(path / "node/bin/node")]),
        ("smoke", [binary, "smoke", "--dir", tempfile.mkdtemp(prefix="gate-smoke-")]),
    ):
        result = subprocess.run(command, capture_output=True, text=True, timeout=300)
        steps.append({"name": name, "ok": result.returncode == 0, "detail": (result.stdout + result.stderr)[-300:]})
        if result.returncode:
            return {"ok": False, "steps": steps}
    steps.append(idle_probe(candidate_id))
    return {"ok": steps[-1]["ok"], "steps": steps}


def idle_probe(candidate_id, timeout=120, sleep=time.sleep):
    run_id = f"gate-{candidate_id[:10]}-{int(time.time())}"
    try:
        start_host({"run_id": run_id, "candidate_id": candidate_id, "seed": 1, "idle_probe": True,
                    "deadline_seconds": 20, "api_key": "non-live-lifecycle-probe"})
        for _ in range(timeout):
            status = status_host(run_id)
            if status["service_stopped"]:
                return {"name": "idle_probe", "ok": status["cleanup_confirmed"], "run_id": run_id}
            sleep(1)
        return {"name": "idle_probe", "ok": False, "run_id": run_id, "detail": "probe did not stop in time"}
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        return {"name": "idle_probe", "ok": False, "run_id": run_id, "detail": str(error)[:300]}


def promote_host(request, gate=None):
    channel, candidate_id = request.get("channel", "stable"), request["candidate_id"]
    with lock(f"channel-{channel}"):
        verify_candidate(candidate_id)
        old = read_pointer(channel)
        if old and old["candidate_id"] == candidate_id:
            return {"changed": False, **old}
        verdict = (gate or run_gate)(candidate_id)
        if not verdict["ok"]:
            channel_log(channel, {"event": "promotion_refused", "candidate_id": candidate_id, "gate": verdict})
            raise ValueError(f"Promotion gate failed; {channel} still points at {old['candidate_id'] if old else 'nothing'}: "
                             + ", ".join(step["name"] for step in verdict["steps"] if not step["ok"]))
        history = [old["candidate_id"], *old.get("previous", [])] if old else []
        new = make_pointer(channel, candidate_id, history, verdict, request.get("source", "operator"))
        write_atomic(pointer_path(channel), new)
        try:
            verify_pointer(read_pointer(channel))
        except (OSError, ValueError, KeyError):
            # The switch landed on something that does not verify: put the old pointer back.
            if old:
                write_atomic(pointer_path(channel), old)
            channel_log(channel, {"event": "promotion_rolled_back", "candidate_id": candidate_id})
            raise ValueError("Promoted candidate failed post-switch verification; the previous pointer was restored")
        channel_log(channel, {"event": "promoted", "candidate_id": candidate_id, "from": old and old["candidate_id"]})
        return {"changed": True, **new}


def rollback_host(request):
    channel = request.get("channel", "stable")
    with lock(f"channel-{channel}"):
        old = read_pointer(channel)
        target = request.get("candidate_id") or (old and old.get("previous") and old["previous"][0])
        if not old or not target:
            raise ValueError(f"Channel {channel} has no previous candidate to roll back to")
        verify_candidate(target)
        remaining = [item for item in old["previous"] if item != target]
        new = make_pointer(channel, target, remaining, {"ok": True, "steps": [{"name": "rollback", "ok": True}]}, "rollback")
        new["rolled_back_from"] = old["candidate_id"]
        write_atomic(pointer_path(channel), new)
        channel_log(channel, {"event": "rolled_back", "candidate_id": target, "from": old["candidate_id"]})
        return {"changed": True, **new}


def run_pins():
    """Candidates that existing runs still depend on: running, resumable or not yet checked."""
    pins = {}
    runs = STATE / "runs"
    for run in sorted(runs.iterdir()) if runs.is_dir() else []:
        try:
            candidate_id = json.loads((run / "request.json").read_text())["candidate_id"]
        except (OSError, ValueError, KeyError):
            continue
        returncode, properties = service_properties(run.name)
        running = returncode != 0 or properties.get("ActiveState") not in ("inactive", "failed")
        resumable = (run / "resume.json").exists() and not (run / "resumed.json").exists()
        if running or resumable or not (run / "checker-exit.json").exists():
            pins.setdefault(candidate_id, []).append(run.name)
    return pins


def candidates_host(request):
    pointers = {channel: read_pointer(channel) for channel in CHANNELS}
    pins = run_pins()
    rows = []
    for path in sorted(DATA.iterdir()) if DATA.is_dir() else []:
        if not CANDIDATE_ID.fullmatch(path.name) or not (path / "candidate.json").is_file():
            continue
        manifest = json.loads((path / "candidate.json").read_text())
        rows.append({"candidate_id": path.name, "revision": manifest.get("source", {}).get("revision"),
                     "dirty": manifest.get("source", {}).get("dirty"), "tree_sha256": manifest.get("tree_sha256"),
                     "current_of": [c for c, p in pointers.items() if p and p["candidate_id"] == path.name],
                     "previous_of": [c for c, p in pointers.items() if p and path.name in p.get("previous", [])],
                     "pinned_by": pins.get(path.name, [])})
    return {"candidates": rows, "pointers": pointers}


def prune_host(request):
    keep = max(1, int(request.get("keep", 3)))
    dry_run = not request.get("yes", False)
    protected = set(run_pins())
    for channel in CHANNELS:
        pointer = read_pointer(channel)
        if pointer:
            protected |= {pointer["candidate_id"], *pointer.get("previous", [])[:keep - 1]}
    removed = []
    with lock("prune"):
        for row in candidates_host({})["candidates"]:
            if row["candidate_id"] in protected:
                continue
            removed.append(row["candidate_id"])
            if not dry_run:
                path = candidate_path(row["candidate_id"])
                for item in path.rglob("*"):
                    if not item.is_symlink():
                        item.chmod(item.stat().st_mode | 0o700)
                path.chmod(0o700)
                shutil.rmtree(path)
    return {"dry_run": dry_run, "keep": keep, "removed" if not dry_run else "would_remove": removed, "protected": sorted(protected)}


def control_host(request, sleep=time.sleep):
    """Ask a running run to drain, pause or resume admissions through its control file."""
    run_id, mode = request["run_id"], request["mode"]
    if mode not in ("drain", "pause", "run"):
        raise ValueError("Mode must be drain, pause or run")
    path = run_path(run_id)
    with lock(f"run-{run_id}"):
        status = status_host(run_id)
        if status["service_stopped"]:
            raise ValueError("Run service is not running; there is nothing to " + mode)
        grace = int(request.get("grace_seconds", 120))
        write_atomic(path / "control.json", {"mode": mode, "reason": request.get("reason", "operator"),
                                             "grace_seconds": grace, "requested_unix": time.time()})
    if mode != "drain" or not request.get("wait", True):
        return {"requested": mode, "run_id": run_id}
    for _ in range(grace + 60):
        status = status_host(run_id)
        if status["service_stopped"]:
            resume = path / "resume.json"
            return {"requested": mode, "run_id": run_id, "drained": resume.exists(),
                    "resume": json.loads(resume.read_text()) if resume.exists() else None, "progress": status.get("progress")}
        sleep(1)
    raise ValueError("Run did not stop inside the drain window; it is still draining, inspect status")


def resume_host(request):
    """Validate a drained or crashed run against a candidate. Starting it needs `run --resume`."""
    run_id, candidate_id = request["run_id"], request["candidate_id"]
    path = run_path(run_id)
    status = status_host(run_id)
    if not status["service_stopped"]:
        raise ValueError("Run service is not confirmed stopped")
    candidate, _ = verify_candidate(candidate_id)
    result = subprocess.run([str(candidate / "target/release/solos-swarm"), "resume-plan", "--run-dir", str(path)],
                            capture_output=True, text=True, timeout=120)
    if result.returncode:
        raise ValueError("Candidate cannot resume this run: " + (result.stderr.strip().splitlines() or ["unknown"])[-1][:300])
    return {"run_id": run_id, "candidate_id": candidate_id, "drained": (path / "resume.json").exists(),
            "plan": json.loads(result.stdout), "executable": False,
            "reason": "The runtime has no `run --resume` yet; the plan is what it will settle and respawn"}


def check(name, ok, detail, level="fail"):
    return {"name": name, "ok": ok, "level": "ok" if ok else level, "detail": detail}


def doctor_host(request):
    """Read-only host health. Reports key presence only on the controller, never a value."""
    checks = []
    for channel in CHANNELS:
        pointer = read_pointer(channel)
        if pointer is None:
            checks.append(check(f"channel-{channel}", channel != "stable", "no promoted candidate", "warn"))
            continue
        try:
            verify_pointer(pointer)
            checks.append(check(f"channel-{channel}", True, f"{pointer['candidate_id'][:12]} verified"))
        except (OSError, ValueError, KeyError) as error:
            checks.append(check(f"channel-{channel}", False, str(error)))
    for name, directory in (("disk-data", DATA), ("disk-state", STATE)):
        probe = directory if directory.exists() else directory.parent
        free = shutil.disk_usage(probe).free / 2**30
        checks.append(check(name, free >= 10, f"{free:.1f} GiB free", "fail" if free < 2 else "warn"))
    returncode, _ = service_properties("doctor-probe")
    checks.append(check("systemd-user", returncode == 0, "user manager reachable" if returncode == 0 else "systemctl --user failed"))
    active = [run.name for run in (STATE / "runs").iterdir() if run.is_dir()] if (STATE / "runs").is_dir() else []
    for run_id in active:
        code, properties = service_properties(run_id)
        if code == 0 and properties.get("ActiveState") == "active":
            checks.append(check(f"broker-socket-{run_id}", broker_socket_alive(run_path(run_id) / "broker.sock"), "broker socket accepts connections"))
        alert = run_path(run_id) / "alert.json"
        if alert.exists():
            checks.append(check(f"alert-{run_id}", False, json.dumps(json.loads(alert.read_text()).get("pause")), "warn"))
    credentials = STATE / "credentials"
    stale = [f.name for f in credentials.iterdir() if time.time() - f.stat().st_mtime > 600] if credentials.is_dir() else []
    checks.append(check("stale-credentials", not stale, f"{len(stale)} credential files older than 10 minutes", "warn"))
    skew = abs(time.time() - request["client_time_unix"]) if "client_time_unix" in request else None
    if skew is not None:
        checks.append(check("clock", skew < 30, f"skew vs controller {skew:.1f}s", "fail" if skew > 120 else "warn"))
    checks.append(check("bubblewrap", shutil.which("bwrap") is not None, "bwrap on PATH"))
    verdict = "FAIL" if any(c["level"] == "fail" for c in checks) else "WARN" if any(c["level"] == "warn" for c in checks) else "PASS"
    return {"verdict": verdict, "checks": checks}


def broker_socket_alive(path):
    try:
        with socket.socket(socket.AF_UNIX) as probe:
            probe.settimeout(1)
            probe.connect(str(path))
        return True
    except OSError:
        return False


HOST_ACTIONS = {"candidates-list": candidates_host, "candidates-current": current_host, "candidates-promote": promote_host,
                "candidates-rollback": rollback_host, "candidates-prune": prune_host, "drain": control_host,
                "resume": resume_host, "doctor": doctor_host}


def collect_bundle(path, out, include_code=False):
    """Stream the run's evidence as a tar. The per-run code/ dir is agents' run_code scratch
    (27 GB observed per run), so it stays on the host; collect-manifest.json says what was left."""
    skipped = {"path": "code", "files": 0, "bytes": 0}
    with tarfile.open(fileobj=out, mode="w|") as bundle:
        for item in sorted(path.rglob("*")):
            if not item.is_file() or item.is_symlink():
                continue
            relative = item.relative_to(path)
            if relative.parts[0] == "code" and not include_code:
                skipped["files"] += 1
                skipped["bytes"] += item.stat().st_size
                continue
            bundle.add(item, arcname=str(relative), recursive=False)
        manifest = json.dumps({"skipped": [] if include_code else [skipped], "include_code": include_code}, indent=2).encode()
        info = tarfile.TarInfo("collect-manifest.json")
        info.size = len(manifest)
        info.mtime = int(time.time())
        bundle.addfile(info, io.BytesIO(manifest))


def host_main(action):
    if action == "deploy":
        result = deploy_host()
    elif action == "collect":
        request = json.load(sys.stdin)
        path = run_path(request["run_id"])
        status = status_host(request["run_id"])
        if not status["service_stopped"]:
            raise ValueError("Run service is not confirmed stopped; inspect status before collecting final evidence")
        write_json(path / "controller-status.json", status)
        collect_bundle(path, sys.stdout.buffer, bool(request.get("include_code")))
        return
    else:
        request = json.load(sys.stdin)
        if action == "start":
            # The manager handoff must finish even if SSH loses its client mid-start.
            signal.signal(signal.SIGHUP, signal.SIG_IGN)
            result = start_host(request)
        elif action == "status":
            result = status_host(request["run_id"])
        elif action in HOST_ACTIONS:
            result = HOST_ACTIONS[action](request)
        elif action == "cancel":
            run_id = request["run_id"]
            run_path(run_id)
            with lock(f"run-{run_id}"):
                result = status_host(run_id)  # Require this controller's owned run record.
                if not result["service_stopped"]:
                    subprocess.run(["systemctl", "--user", "stop", unit_name(run_id)], check=True)
                    result = status_host(run_id)
                if not result["service_stopped"]:
                    raise ValueError("Run-owned systemd service is not confirmed stopped")
        else:
            raise ValueError("Unknown remote operation")
    print(json.dumps(result))


def controller_key_check(auth_file, provider):
    try:
        read_provider_key(auth_file, provider)
        return check(f"provider-key-{provider}", True, "present in the Pi auth file (value not read out)")
    except (OSError, ValueError):
        return check(f"provider-key-{provider}", False, "missing from the Pi auth file")


def default_ssh_host():
    return os.environ.get("SOLOS_SWARM_SSH_TARGET") or os.environ.get("SWARM_SSH_HOST") or EXAMPLE_SSH_HOST


def ssh_argv(host):
    if not isinstance(host, str) or not SSH_HOST.fullmatch(host):
        raise ValueError("SSH host must be an alias or user@host")
    config = os.environ.get("SWARM_SSH_CONFIG")  # local Lima VM runner: ssh -F <vm ssh_config>
    return ["ssh", *(["-F", config] if config else []), "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ForwardAgent=no", "-o", "ConnectTimeout=60", "-o", "ServerAliveInterval=30", "-o", "ServerAliveCountMax=20", host]


def ssh_command(action, host):
    source = base64.b64encode(Path(__file__).read_bytes()).decode()
    loader = ("import base64, sys\nns={'__name__':'remote_host','__file__':'/remote.py'}\ntry:\n"
              f" exec(base64.b64decode('{source}'), ns)\n ns['host_main']({action!r})\n"
              "except Exception as error:\n print('remote-host: ' + str(error), file=sys.stderr)\n sys.exit(2)")
    return [*ssh_argv(host), "python3 -c " + shlex.quote(loader)]


def call_host(action, payload, host):
    result = subprocess.run(ssh_command(action, host), input=json.dumps(payload).encode(), capture_output=True)
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors="replace").strip() or "Remote operation failed")
    return json.loads(result.stdout)


def source_archive():
    files = subprocess.check_output(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=REPO).decode().split("\0")
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.PAX_FORMAT) as bundle:
        for name in sorted(set(files) - {""}):
            path = REPO / name
            parts = Path(name).parts
            # Build trees are local artifacts, not source. A stale target-linux/
            # (5.4 GB) made the archive buffer balloon before the first deploy.
            if parts and parts[0] in {"target", "target-linux", "runs", "work"}:
                continue
            if path.is_symlink() or not path.is_file() or Path(name).is_absolute() or ".." in parts:
                raise ValueError(f"Unsafe source entry: {name}")
            if path.name in ("auth.json", ".env", "credentials.json") or "node_modules" in Path(name).parts:
                raise ValueError(f"Private or generated source entry must be ignored: {name}")
            entry = bundle.gettarinfo(str(path), arcname=name)
            entry.mtime, entry.uid, entry.gid, entry.uname, entry.gname = 0, 0, 0, "", ""
            with path.open("rb") as handle:
                bundle.addfile(entry, handle)
        metadata = json.dumps({
            "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=REPO, text=True).strip(),
            "dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=REPO)),
        }, sort_keys=True).encode()
        entry = tarfile.TarInfo("source-metadata.json")
        entry.size, entry.mode = len(metadata), 0o644
        bundle.addfile(entry, io.BytesIO(metadata))
    return buffer.getvalue()


def read_provider_key(auth_file, provider):
    """The operator's provider key from the private Pi auth config. The key
    travels over SSH stdin only; it enters neither candidate nor public config."""
    entry = json.loads(auth_file.read_text()).get(provider, {})
    value = entry.get("key", entry.get("apiKey"))
    if entry.get("type") != "api_key" or not isinstance(value, str) or not value.strip():
        raise ValueError(f"Pi auth config has no {provider} api_key entry")
    return value


def read_deepseek_key(auth_file):
    return read_provider_key(auth_file, "deepseek")


def provider_for(goal_file, supplied):
    """The provider this run spends against: an explicit flag, else the Goal's
    own upstream, else the legacy DeepSeek default. Kept here so the operator's
    key is read from the matching auth entry."""
    if supplied:
        return supplied
    if goal_file:
        upstream = json.loads(goal_file.read_text()).get("upstream")
        if isinstance(upstream, dict) and isinstance(upstream.get("provider"), str):
            return upstream["provider"]
    return "deepseek"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--host", default=None, help="SSH alias or user@host. Default: solos-node. Override with SOLOS_SWARM_SSH_TARGET.")
    subparsers = parser.add_subparsers(dest="action", required=True)
    subparsers.add_parser("deploy", parents=[common], help="Build once on the selected host; freeze installed artifacts")
    start = subparsers.add_parser("start", parents=[common], help="Freeze one case and start up to thirty live agents as a bounded systemd service")
    start.add_argument("--candidate", required=True)
    start.add_argument("--run-id", required=True)
    start.add_argument("--seed", required=True, type=int)
    start.add_argument("--goal-file", type=Path, help="Frozen Goal JSON prepared locally; skips host-side prepare_case")
    start.add_argument("--expected-file", type=Path, help="Private evaluator payload paired with --goal-file")
    start.add_argument("--auth-file", type=Path, default=Path.home() / ".pi/agent/auth.json")
    start.add_argument("--provider", choices=("deepseek", "venice"),
                       help="Provider that spends for this run; defaults to the Goal's own upstream")
    start.add_argument("--refuse-peak", action="store_true", help="Broker pauses model calls Mon-Fri 01-04 and 06-10 UTC (needs a candidate built with this support)")
    start.add_argument("--idle-probe", action="store_true", help="Exercise lifecycle with no paid model calls; never qualifies")
    start.add_argument("--deadline-seconds", type=int, help="Shortened deadline for an idle lifecycle probe only")
    for action in ("status", "cancel", "collect"):
        command = subparsers.add_parser(action, parents=[common])
        command.add_argument("--run-id", required=True)
        if action == "collect":
            command.add_argument("--output", required=True, type=Path)
            command.add_argument("--include-code", action="store_true", help="Also stream the per-run code/ scratch dir (can be tens of GB)")
    candidates = subparsers.add_parser("candidates", help="Channel pointers: which candidate new runs use").add_subparsers(dest="verb", required=True)
    for verb in ("list", "current", "promote", "rollback", "prune"):
        sub = candidates.add_parser(verb, parents=[common])
        if verb != "list":
            sub.add_argument("--channel", choices=CHANNELS, default="stable")
        if verb in ("promote", "rollback"):
            sub.add_argument("--candidate", required=verb == "promote", help="Promote this candidate; rollback defaults to the previous one")
        if verb == "prune":
            sub.add_argument("--keep", type=int, default=3, help="Promoted candidates to keep, including current")
            sub.add_argument("--yes", action="store_true", help="Delete; the default is a dry run")
    for action in ("drain", "pause", "unpause"):
        control = subparsers.add_parser(action, parents=[common], help={"drain": "Stop granting tasks and model calls, let in-flight calls settle, then exit resumable", "pause": "Stop granting tasks and model calls; the run stays up", "unpause": "Reopen a paused run"}[action])
        control.add_argument("--run-id", required=True)
        control.add_argument("--reason", default="operator")
        if action == "drain":
            control.add_argument("--grace-seconds", type=int, default=120)
    resume = subparsers.add_parser("resume", parents=[common], help="Check a drained or crashed run against a candidate and print what resuming would do")
    resume.add_argument("--run-id", required=True)
    resume.add_argument("--candidate", default="current")
    doctor = subparsers.add_parser("doctor", parents=[common], help="Host and controller health; prints presence of keys, never values")
    doctor.add_argument("--auth-file", type=Path, default=Path.home() / ".pi/agent/auth.json")
    doctor.add_argument("--provider", default="deepseek")
    finalize = subparsers.add_parser("_finalize", help=argparse.SUPPRESS)
    finalize.add_argument("--run-id", required=True)
    finalize.add_argument("--candidate", required=True)
    finalize.add_argument("--credential-file", required=True)
    args = parser.parse_args()
    try:
        if args.action == "_finalize":
            finalize_host(args.run_id, args.candidate, args.credential_file)
            return
        host = args.host or default_ssh_host()
        if args.action == "deploy":
            result = subprocess.run(ssh_command("deploy", host), input=source_archive(), capture_output=True)
            if result.returncode:
                raise RuntimeError(result.stderr.decode(errors="replace").strip())
            print(result.stdout.decode().strip())
            return
        if args.action == "candidates":
            request = {"channel": getattr(args, "channel", "stable"), "keep": getattr(args, "keep", 3), "yes": getattr(args, "yes", False),
                       "candidate_id": getattr(args, "candidate", None), "source": "controller"}
            print(json.dumps(call_host("candidates-" + args.verb, request, host), indent=2))
            return
        if args.action == "doctor":
            result = call_host("doctor", {"client_time_unix": time.time()}, host)
            result["checks"].append(controller_key_check(args.auth_file, args.provider))
            print(json.dumps(result, indent=2))
            return
        if args.action in ("drain", "pause", "unpause"):
            request = {"run_id": args.run_id, "reason": args.reason, "grace_seconds": getattr(args, "grace_seconds", 120),
                       "mode": {"drain": "drain", "pause": "pause", "unpause": "run"}[args.action]}
            print(json.dumps(call_host("drain", request, host), indent=2))
            return
        if args.action == "resume":
            candidate = args.candidate
            if candidate.startswith("current"):
                candidate = call_host("candidates-current", {}, host)["candidate_id"]
            print(json.dumps(call_host("resume", {"run_id": args.run_id, "candidate_id": candidate}, host), indent=2))
            return
        run_path(args.run_id)
        request = {"run_id": args.run_id, "include_code": getattr(args, "include_code", False)}
        if args.action == "start":
            candidate = args.candidate
            if candidate == "current":
                candidate = call_host("candidates-current", {}, host)["candidate_id"]
            candidate_path(candidate)
            request.update(candidate_id=candidate, seed=args.seed)
            request.update(idle_probe=args.idle_probe, deadline_seconds=args.deadline_seconds)
            if args.refuse_peak:
                request["refuse_peak"] = True
            if args.goal_file or args.expected_file:
                if not args.goal_file or not args.expected_file:
                    parser.exit(2, "remote: --goal-file and --expected-file are required together\n")
                goal_bytes, expected_bytes = args.goal_file.read_bytes(), args.expected_file.read_bytes()
                request["case_sha256"] = hashlib.sha256(goal_bytes + b"\x00" + expected_bytes).hexdigest()
                request["goal"], request["expected"] = goal_bytes.decode(), expected_bytes.decode()
            result = call_host("start", request, host)
            if result.get("needs_credential"):
                if args.idle_probe:
                    request["api_key"] = "non-live-lifecycle-probe"
                else:
                    provider = provider_for(args.goal_file, args.provider)
                    request["api_key"] = read_provider_key(args.auth_file, provider)
                result = call_host("start", request, host)
            print(json.dumps(result, indent=2))
        elif args.action == "collect":
            result = subprocess.run(ssh_command("collect", host), input=json.dumps(request).encode(), capture_output=True)
            if result.returncode:
                raise RuntimeError(result.stderr.decode(errors="replace").strip())
            args.output.mkdir(parents=True, exist_ok=False)
            with tarfile.open(fileobj=io.BytesIO(result.stdout)) as bundle:
                for member in bundle.getmembers():
                    if not member.isfile() or Path(member.name).is_absolute() or ".." in Path(member.name).parts:
                        raise ValueError("Evidence archive contains an unsafe member")
                bundle.extractall(args.output, filter="data")
            print(json.dumps({"run_id": args.run_id, "evidence": str(args.output.resolve())}))
        else:
            print(json.dumps(call_host(args.action, request, host), indent=2))
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(2, f"remote: {error}\n")


if __name__ == "__main__":
    main()
