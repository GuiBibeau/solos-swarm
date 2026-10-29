#!/usr/bin/env python3
"""Box-side pre-start gate + launcher for wave 1. Runs from a systemd user timer at 10:00 UTC and every 30 min until
16:00 UTC. Aborts (logging why, never printing secrets) unless every condition holds; at 16:00 it gives up and writes
GAVE_UP and an alert line. On success it starts the w1 run through the frozen candidate's own remote.py host code.
Later waves are started by drive.py from the controller (see README)."""
import hashlib, json, os, shutil, subprocess, sys, runpy
from datetime import datetime, timezone
from pathlib import Path

H = Path.home(); R = H / "research/regimes"; T = R / "tournament-20261028"; D = R / "tournament-data"
CAND = "9589132866589167d69a3ea2626f63472b8b5abb523a4d32e3828e5311c48459"
RUN = "tourney-20261028-w1"; S = H / ".local/state/solos-swarm"
LOG = D / "gate.log"; now = datetime.now(timezone.utc)
dry = "--dry-run" in sys.argv


def log(msg):
    D.mkdir(parents=True, exist_ok=True)
    with LOG.open("a") as f: f.write(f"{now:%FT%TZ} {msg}\n")
    print(msg)


def checks():
    bad = []
    p = H / ".local/share/solos-data/free/binance/pull.log"
    lines = p.read_text().splitlines()
    if not lines or " done futures" not in lines[-1]: bad.append("phase-1 pull not complete: " + (lines[-1][:80] if lines else "empty log"))
    if any("pause:" in l for l in lines[-20:]) and " done futures" not in (lines[-1] if lines else ""): bad.append("pull is paused by the guard")
    ok = D / "build/BUILD_OK"
    if not ok.exists(): bad.append("derived dataset BUILD_OK missing (build not finished or failed sanity check)")
    elif not (D / "views/workers/manifest.json").exists(): bad.append("workers view missing")
    g = json.loads((H / ".local/state/transfer-guard/status.json").read_text())
    if g.get("level") != "ok" or g.get("used_gb", 9999) >= 400: bad.append(f"transfer guard level={g.get('level')} used_gb={g.get('used_gb')} (need ok and < 400)")
    try:
        a = json.loads((H / ".pi/agent/auth.json").read_text()).get("deepseek", {})
        if a.get("type") != "api_key" or not str(a.get("key", a.get("apiKey", ""))).strip(): bad.append("DeepSeek api_key entry missing on the box")
    except Exception: bad.append("DeepSeek auth file unreadable or absent on the box (~/.pi/agent/auth.json)")
    if shutil.disk_usage(H).free < 100 * 2**30: bad.append("less than 100 GB free")
    act = subprocess.run(["systemctl", "--user", "list-units", "solos-swarm-*.service", "--state=active,activating", "--no-legend"], capture_output=True, text=True).stdout.strip()
    if act: bad.append("another swarm run is active: " + act.split()[0])
    for f in ("w1-goal.json", "w1-expected.json"):
        if not (T / f).exists(): bad.append(f"{f} not staged in {T}")
    return bad


def main():
    if (D / "LAUNCHED").exists(): return log("already launched; nothing to do")
    if (D / "CANCELLED").exists(): return log("CANCELLED marker present; not launching")
    bad = checks()
    if bad:
        log("GATE FAIL: " + " | ".join(bad))
        if now.hour >= 16 and not dry:
            (D / "GAVE_UP").write_text(f"{now:%FT%TZ} " + " | ".join(bad) + "\n")
            (S / "tournament-alerts.log").open("a").write(f"{now:%FT%TZ} tournament w1 GAVE UP at 16:00Z: {' | '.join(bad)}\n")
        return 2
    log("GATE PASS" + (" (dry run, not launching)" if dry else ""))
    if dry: return 0
    goal, exp = (T / "w1-goal.json").read_bytes(), (T / "w1-expected.json").read_bytes()
    cand = H / ".local/share/solos-swarm" / CAND
    ns = runpy.run_path(str(cand / "scripts/remote.py"), run_name="remote_host")
    key = json.loads((H / ".pi/agent/auth.json").read_text())["deepseek"]; key = key.get("key", key.get("apiKey"))
    req = {"run_id": RUN, "candidate_id": CAND, "seed": json.loads(goal)["seed"], "idle_probe": False, "deadline_seconds": None, "refuse_peak": True,
           "case_sha256": hashlib.sha256(goal + b"\x00" + exp).hexdigest(), "goal": goal.decode(), "expected": exp.decode(), "api_key": key}
    res = ns["start_host"](req)
    (D / "LAUNCHED").write_text(f"{now:%FT%TZ} {RUN}\n"); log(f"LAUNCHED {RUN} service_stopped={res.get('service_stopped')}")
    return 0


sys.exit(main())
