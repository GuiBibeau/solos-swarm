"""python3 -m replay ingest DIR... --out H | score --history H [--policy spec.json] | check --history H --proposal P"""
import argparse
import json

from . import guard, hook, propose
from .ingest import ingest
from .policy import from_spec
from .schema import read_history, write_history
from .simulate import score


def main(argv=None):
    ap = argparse.ArgumentParser(prog="replay")
    ap.add_argument("cmd", choices=["ingest", "score", "check", "propose"])
    ap.add_argument("dirs", nargs="*")
    ap.add_argument("--out"); ap.add_argument("--history"); ap.add_argument("--policy"); ap.add_argument("--proposal")
    a = ap.parse_args(argv)
    if a.cmd == "ingest":
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
