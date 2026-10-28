"""python3 -m replay ingest DIR... --out H | score --history H [--policy spec.json] | check --history H --proposal P"""
import argparse
import json

from . import guard, hook, propose
from .ingest import ingest, ingest_tournament
from .policy import from_spec
from .schema import read_history, write_history
from .simulate import score


def demo():
    from .synthetic import history
    runs = history(40, 4)
    spec = {"kind": "adaptive", "stop_after_null": 1, "per_family_max": 4, "max_rounds": 4}
    for n in (5, 40):
        v = guard.decide(runs[:n], hook.BASELINE, spec)
        print(json.dumps({"runs": n, **{k: v.get(k) for k in ("verdict", "adopt", "n_dev", "n_held", "dev_mean_diff", "held_mean_diff", "held_ci", "unexplored_share")}}, indent=1))
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(prog="replay")
    ap.add_argument("cmd", choices=["ingest", "ingest-tournament", "score", "check", "propose", "demo"])
    ap.add_argument("dirs", nargs="*")
    ap.add_argument("--out"); ap.add_argument("--history"); ap.add_argument("--policy"); ap.add_argument("--proposal")
    a = ap.parse_args(argv)
    if a.cmd == "demo":
        return demo()
    if a.cmd == "ingest-tournament":
        run = ingest_tournament(a.dirs[0])
        runs = ([run] if run else []) + ingest(a.dirs[1:])
        write_history(runs, a.out)
        out = {"runs": len(runs), "scorable": sum(r["scorable"] for r in runs),
               "steps": run["step"] if run else None, "attempts": len(run["attempts"]) if run else 0}
    elif a.cmd == "ingest":
        runs = ingest(a.dirs)
        write_history(runs, a.out)
        out = {"runs": len(runs), "scorable": sum(r["scorable"] for r in runs)}
    else:
        runs = read_history(a.history)
        spec = json.load(open(a.policy)) if a.policy else hook.BASELINE
        if a.cmd == "score":
            s = score(from_spec(spec), [r for r in runs if r["scorable"]])
            out = {"policy": spec, "n_scorable": len(s["rewards"]), "mean": s["mean"], "unexplored_share": s["unexplored_share"]}
        elif a.cmd == "propose":
            out = propose.search(runs, hook.BASELINE)
        else:
            prop = json.load(open(a.proposal))
            out = guard.decide(runs, hook.BASELINE, prop["policy"], tuned_on=prop.get("tuned_on"))
    print(json.dumps(out, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
