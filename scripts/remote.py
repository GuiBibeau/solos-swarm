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
import subprocess
import sys
import tarfile
import tempfile
import time

REPO = Path(__file__).resolve().parent.parent
SSH = ["ssh", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ForwardAgent=no", "-o", "ConnectTimeout=15", "remote-ai"]
NODE_SOURCE = Path.home() / ".local/opt/node-v24.18.0-linux-x64"
DATA = Path.home() / ".local/share/solos-swarm"
STATE = Path.home() / ".local/state/solos-swarm"
RUN_ID = re.compile(r"[a-z0-9][a-z0-9-]{0,47}\Z")
CANDIDATE_ID = re.compile(r"[a-f0-9]{64}\Z")


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
    return path, manifest


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
            manifest = {
                "schema_version": 1, "candidate_id": candidate_id,
                "source_archive_sha256": candidate_id,
                "source": json.loads((staging / "source-metadata.json").read_text()),
                "build_target": subprocess.check_output(["rustc", "-vV"], env=env, text=True).strip(),
                "node_version": subprocess.check_output([str(staging / "node/bin/node"), "--version"], text=True).strip(),
                "topology": "single-linux-host", "files": inventory(staging),
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


def status_host(run_id):
    path = run_path(run_id)
    if not path.is_dir():
        raise ValueError("Run ID does not exist")
    command = ["systemctl", "--user", "show", unit_name(run_id), "--property=LoadState,ActiveState,SubState,Result,ExecMainStatus", "--no-pager"]
    result = subprocess.run(command, capture_output=True, text=True)
    properties = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
    stopped = result.returncode == 0 and properties.get("ActiveState") in ("inactive", "failed")
    output = {"run_id": run_id, "path": str(path), "service": properties, "service_stopped": stopped,
              "request": json.loads((path / "request.json").read_text())}
    for name in ("report", "cleanup", "service-cleanup"):
        artifact = path / f"{name}.json"
        if artifact.exists():
            output[name] = json.loads(artifact.read_text())
    output["cleanup_confirmed"] = (stopped and output.get("cleanup", {}).get("verdict") == "PASS"
                                   and output.get("service-cleanup", {}).get("credential_removed") is True)
    return output


def start_host(request):
    run_id = request["run_id"]
    path = run_path(run_id)
    identity = {key: request[key] for key in ("candidate_id", "seed", "run_id")}
    identity["idle_probe"] = request.get("idle_probe", False)
    identity["deadline_seconds"] = request.get("deadline_seconds")
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
            subprocess.run(["python3", str(candidate / "scripts/prepare_case.py"), "--seed", str(request["seed"]),
                            "--goal", str(path / "config.yaml"), "--expected", str(private / "expected.json")],
                           stdout=(path / "prepare.log").open("w"), stderr=subprocess.STDOUT, check=True)
            goal = json.loads((path / "config.yaml").read_text())
            if goal["agent_count"] != 10 or not 1 <= goal["deadline_seconds"] <= 1200 or goal["budget_microusd"] > 1_000_000:
                raise ValueError("First preview requires ten agents, deadline <=1200s, and budget <=$1")
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
                               f"--property=RuntimeMaxSec={deadline + 20}", "--property=TimeoutStopSec=15",
                               "--property=SendSIGKILL=yes", "--property=UMask=0077", "--property=NoNewPrivileges=yes",
                               "--property=LimitCORE=0",
                               f"--property=WorkingDirectory={path}", f"--property=StandardOutput=append:{path / 'stdout.log'}",
                               f"--property=StandardError=append:{path / 'stderr.log'}", f"--property=ExecStopPost={finish}",
                               "--setenv=PATH=/usr/bin:/bin", "--", *command]
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
        with tarfile.open(fileobj=sys.stdout.buffer, mode="w|") as bundle:
            for item in sorted(path.rglob("*")):
                if item.is_file() and not item.is_symlink():
                    bundle.add(item, arcname=str(item.relative_to(path)), recursive=False)
        return
    else:
        request = json.load(sys.stdin)
        if action == "start":
            # The manager handoff must finish even if SSH loses its client mid-start.
            signal.signal(signal.SIGHUP, signal.SIG_IGN)
            result = start_host(request)
        elif action == "status":
            result = status_host(request["run_id"])
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


def ssh_command(action):
    source = base64.b64encode(Path(__file__).read_bytes()).decode()
    loader = ("import base64, sys\nns={'__name__':'remote_host','__file__':'/remote.py'}\ntry:\n"
              f" exec(base64.b64decode('{source}'), ns)\n ns['host_main']({action!r})\n"
              "except Exception as error:\n print('remote-host: ' + str(error), file=sys.stderr)\n sys.exit(2)")
    return [*SSH, "python3 -c " + shlex.quote(loader)]


def call_host(action, payload):
    result = subprocess.run(ssh_command(action), input=json.dumps(payload).encode(), capture_output=True)
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors="replace").strip() or "Remote operation failed")
    return json.loads(result.stdout)


def source_archive():
    files = subprocess.check_output(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=REPO).decode().split("\0")
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.PAX_FORMAT) as bundle:
        for name in sorted(set(files) - {""}):
            path = REPO / name
            if path.is_symlink() or not path.is_file() or Path(name).is_absolute() or ".." in Path(name).parts:
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


def read_deepseek_key(auth_file):
    entry = json.loads(auth_file.read_text()).get("deepseek", {})
    value = entry.get("key", entry.get("apiKey"))
    if entry.get("type") != "api_key" or not isinstance(value, str) or not value.strip():
        raise ValueError("Pi auth config has no DeepSeek api_key entry")
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="action", required=True)
    subparsers.add_parser("deploy", help="Build once on the approved host; freeze installed artifacts")
    start = subparsers.add_parser("start", help="Freeze one case and start ten live agents as a bounded systemd service")
    start.add_argument("--candidate", required=True)
    start.add_argument("--run-id", required=True)
    start.add_argument("--seed", required=True, type=int)
    start.add_argument("--auth-file", type=Path, default=Path.home() / ".pi/agent/auth.json")
    start.add_argument("--idle-probe", action="store_true", help="Exercise lifecycle with no paid model calls; never qualifies")
    start.add_argument("--deadline-seconds", type=int, help="Shortened deadline for an idle lifecycle probe only")
    for action in ("status", "cancel", "collect"):
        command = subparsers.add_parser(action)
        command.add_argument("--run-id", required=True)
        if action == "collect":
            command.add_argument("--output", required=True, type=Path)
    finalize = subparsers.add_parser("_finalize", help=argparse.SUPPRESS)
    finalize.add_argument("--run-id", required=True)
    finalize.add_argument("--candidate", required=True)
    finalize.add_argument("--credential-file", required=True)
    args = parser.parse_args()
    try:
        if args.action == "_finalize":
            finalize_host(args.run_id, args.candidate, args.credential_file)
            return
        if args.action == "deploy":
            result = subprocess.run(ssh_command("deploy"), input=source_archive(), capture_output=True)
            if result.returncode:
                raise RuntimeError(result.stderr.decode(errors="replace").strip())
            print(result.stdout.decode().strip())
            return
        run_path(args.run_id)
        request = {"run_id": args.run_id}
        if args.action == "start":
            candidate_path(args.candidate)
            request.update(candidate_id=args.candidate, seed=args.seed)
            request.update(idle_probe=args.idle_probe, deadline_seconds=args.deadline_seconds)
            result = call_host("start", request)
            if result.get("needs_credential"):
                request["api_key"] = "non-live-lifecycle-probe" if args.idle_probe else read_deepseek_key(args.auth_file)
                result = call_host("start", request)
            print(json.dumps(result, indent=2))
        elif args.action == "collect":
            result = subprocess.run(ssh_command("collect"), input=json.dumps(request).encode(), capture_output=True)
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
            print(json.dumps(call_host(args.action, request), indent=2))
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(2, f"remote: {error}\n")


if __name__ == "__main__":
    main()
