#!/usr/bin/env python3
"""Wave driver for the tournament. Does nothing paid unless LAUNCH_APPROVED exists next to it.

  drive.py plan [--now ISO]     estimate the schedule (pause mode: active-time simulation; --legacy: off-peak starts)
  drive.py run --candidate ID   run the remaining steps in order, resumable (state in drive-state.json)
  drive.py pause|resume         operator pause: touch/remove <run-dir>/PAUSE on the box for the running step

Pause mode (default, candidate cb075442+): every start passes --refuse-peak, so the broker itself refuses model calls
Mon-Fri 01-04 and 06-10 UTC (agents wait; no call, hold or token is bought at peak) and the Goal deadline counts only
unpaused time; steps may span peak windows and are bounded by budget. --legacy: old behaviour for candidate 4ec772e2:
never start a goal whose deadline overlaps a peak window (01-04 and 06-10 UTC, Mon-Fri); never
start when the box transfer guard is not ok; never start a step if spent-so-far plus the step's cap would pass $100;
switch the dataset root per step (workers never get confirm/); collect evidence, summarize performance, regenerate
the next goal, and commit generated files locally (never pushes). Run it under `caffeinate -dimsu` on the Mac.
"""
import argparse, json, subprocess, sys, time
from datetime import datetime, timedelta, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
STATE = HERE / "drive-state.json"      # {"done": [...], "running": {"step", "run_id"}}; a restarted driver resumes polling
REPO = HERE.parent.parent
ORDER = ["w1", "c1", "w2", "c2", "w3", "c3", "c4"]
VIEWS = "$HOME/research/regimes/tournament-data"      # remote $HOME, expanded by the box shell; views/workers, views/referees
MARGIN_S = 600
SSH = ["ssh", "-o", "BatchMode=yes", "remote-ai"]


def is_peak(t):
    return t.weekday() < 5 and (1 <= t.hour < 4 or 6 <= t.hour < 10)


def window_ok(start, secs):
    t = start
    while t <= start + timedelta(seconds=secs + MARGIN_S):
        if is_peak(t):
            return False
        t += timedelta(minutes=5)
    return True


def earliest(now, secs):
    t = now.replace(second=0, microsecond=0) + timedelta(minutes=(5 - now.minute % 5) % 5)
    while not window_ok(t, secs):
        t += timedelta(minutes=5)
    return t


def deadlines():
    sys.path.insert(0, str(HERE)); import gen
    return {s: gen.STEPS[s]["deadline"] for s in ORDER}


ACTIVE_H = {"w1": 2.0, "c1": 0.4, "w2": 2.0, "c2": 0.4, "w3": 1.5, "c3": 0.4, "c4": 0.4}   # estimates, budget-bound


def plan_pause(now):
    """Minute-by-minute simulation: a step needs ACTIVE_H of off-peak time; peak minutes do not count."""
    t, out = now.replace(second=0, microsecond=0), []
    for s in ORDER:
        start, need = t, int(ACTIVE_H[s] * 60)
        while need > 0:
            if not is_peak(t):
                need -= 1
            t += timedelta(minutes=1)
        out.append((s, start, t)); t += timedelta(minutes=10)
    return out


def plan(now):
    dl, t, out = deadlines(), now, []
    for s in ORDER:
        t = earliest(t, dl[s]); end = t + timedelta(seconds=dl[s])
        out.append((s, t, end)); t = end + timedelta(minutes=15)
    return out


def sh(cmd, **k):
    return subprocess.run(cmd, cwd=REPO, capture_output=True, text=True, **k)


def guard_ok():
    r = subprocess.run(SSH + ["cat ~/.local/state/transfer-guard/status.json"], capture_output=True, text=True)
    return json.loads(r.stdout).get("level") == "ok"


def data_ready(max_gb):
    """Pull done, BUILD_OK, workers view, guard ok and used_gb under max_gb (tonight 800; the gate script keeps 400)."""
    cmd = ("D=$HOME/research/regimes/tournament-data; grep -q ' done futures' ~/.local/share/solos-data/free/binance/pull.log "
           "&& test -f $D/build/BUILD_OK && test -f $D/views/workers/manifest.json && echo READY")
    if "READY" not in subprocess.run(SSH + [cmd], capture_output=True, text=True).stdout:
        return False
    g = json.loads(subprocess.run(SSH + ["cat ~/.local/state/transfer-guard/status.json"], capture_output=True, text=True).stdout)
    return g.get("level") == "ok" and g.get("used_gb", 9999) < max_gb


def spent():
    return sum(json.loads(p.read_text())["spent_usd"] for p in (HERE / "evidence").glob("*-perf.json"))


def log(ev):
    with (HERE / "launch-log.jsonl").open("a") as f:
        f.write(json.dumps(dict(ev, at=datetime.now(timezone.utc).isoformat(timespec="seconds"))) + "\n")


def run_step(step, cand, runid, max_gb=800, legacy=False):
    import gen
    goal = json.loads((HERE / f"{step}-goal.json").read_text())
    cap = goal["budget_microusd"] / 1e6
    assert spent() + cap <= 100.0, f"cap guard: spent {spent()} + {cap}"
    now = datetime.now(timezone.utc)
    while not ((legacy is False or window_ok(now, goal["deadline_seconds"])) and guard_ok() and (step != "w1" or data_ready(max_gb))):
        log({"step": step, "wait": "peak window, transfer guard or data not ready"}); time.sleep(300); now = datetime.now(timezone.utc)
    root = "workers" if step.startswith("w") and step != "w3" else "referees"
    sh(SSH + [f"test -f {VIEWS}/views/{root}/manifest.json && systemctl --user set-environment SWARM_CODE_DATASET_ROOT={VIEWS}/views/{root}"], check=True)
    sh(["git", "add", "-f", str(HERE)]); sh(["git", "commit", "-qm", f"tournament: frozen {step} goal (pre-launch)"])
    r = sh(["python3", "scripts/remote.py", "start", "--candidate", cand, "--run-id", runid, "--seed", str(goal["seed"]),
            "--goal-file", str(HERE / f"{step}-goal.json"), "--expected-file", str(HERE / f"{step}-expected.json")]
           + ([] if legacy else ["--refuse-peak"]))
    state = json.loads(STATE.read_text()) if STATE.exists() else {"done": []}
    state["running"] = {"step": step, "run_id": runid, "candidate": cand}; STATE.write_text(json.dumps(state))
    log({"step": step, "run_id": runid, "root": root, "cap_usd": cap, "start_rc": r.returncode, "out": r.stdout[-400:]})
    if r.returncode:
        sys.exit(f"start failed: {r.stderr}")
    while True:
        time.sleep(300)
        st = json.loads(sh(["python3", "scripts/remote.py", "status", "--run-id", runid]).stdout or "{}")
        log({"step": step, "progress": st.get("progress")})
        if st.get("service_stopped"):
            break
    out = HERE / "evidence" / step
    sh(["python3", "scripts/remote.py", "collect", "--run-id", runid, "--output", str(out)], check=True)
    sh(["python3", str(HERE / "perf_report.py"), "summarize", str(out), "--step", step], check=True)
    sh(["git", "add", "-f", str(HERE)]); sh(["git", "commit", "-qm", f"tournament: {step} evidence summary"])


def load_state():
    return json.loads(STATE.read_text()) if STATE.exists() else {"done": []}


def box_pause(on):
    st = load_state().get("running")
    if not st:
        sys.exit("no running step in drive-state.json")
    f = f"$HOME/.local/state/solos-swarm/runs/{st['run_id']}/PAUSE"
    subprocess.run(SSH + [f"touch {f}" if on else f"rm -f {f}"], check=True)
    print(("paused " if on else "resumed ") + st["run_id"])


def main():
    ap = argparse.ArgumentParser(); ap.add_argument("cmd"); ap.add_argument("--now"); ap.add_argument("--candidate")
    ap.add_argument("--legacy", action="store_true", help="candidate 4ec772e2 without pause support: peak-avoiding deadlines")
    a = ap.parse_args()
    if a.cmd == "plan":
        now = (datetime.fromisoformat(a.now) if a.now else datetime.now(timezone.utc)).astimezone(timezone.utc)
        for s, t, e in (plan(now) if a.legacy else plan_pause(now)):
            print(f"{s}  start {t:%a %Y-%m-%d %H:%M}Z  est. end {e:%a %Y-%m-%d %H:%M}Z")
        return
    if a.cmd in ("pause", "resume"):
        return box_pause(a.cmd == "pause")
    appr = HERE / "LAUNCH_APPROVED"
    if not appr.exists() or appr.read_text().strip() != a.candidate:
        sys.exit("refused: create LAUNCH_APPROVED containing the candidate id (operator approval)")
    sys.path.insert(0, str(HERE)); import gen
    state = load_state()
    for step in ORDER:
        if step in state["done"]:
            continue
        running = state.get("running")
        if not (running and running["step"] == step):         # a restarted driver re-attaches instead of regenerating
            if not (HERE / f"{step}-goal.json").exists() or step != "w1":
                if gen.main(step) is None:
                    state["done"].append(step); STATE.write_text(json.dumps(state)); continue
        cand = (running or {}).get("candidate", a.candidate) if (running and running["step"] == step) else a.candidate  # a running step keeps the candidate it started on
        run_step(step, cand, f"tourney-20261028-{step}", legacy=a.legacy)
        state = load_state(); state["done"].append(step); state.pop("running", None); STATE.write_text(json.dumps(state))


if __name__ == "__main__":
    main()
