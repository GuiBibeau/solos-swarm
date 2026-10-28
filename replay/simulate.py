"""Replay simulator and reward. A run is a set of recorded attempts per family. A policy asks for the next attempt
in a family and gets the recorded one back; nothing is paid. Replay CANNOT evaluate branches the online run never
explored: a request beyond the recorded attempts returns nothing, is counted in `unexplored`, and the guard refuses
candidates that lean on it."""
from collections import defaultdict
from dataclasses import dataclass

from .policy import View
from .schema import survives_screen


@dataclass
class Weights:
    confirmed: float = 1.0       # referee-confirmed survivor
    refuted: float = 0.5         # penalty per referee-refuted survivor (false positive)
    unrefereed: float = 0.2      # value of a screen survivor nobody refereed ...
    unrefereed_cap: int = 3      # ... and at most this many count: more false positives cannot buy reward
    trial: float = 0.001         # per counted trial (multiplicity)
    usd: float = 0.03            # per dollar
    wall_hour: float = 0.05      # per wall-clock hour
    parallel: float = 0.005      # bonus per attempt per round, capped at `parallel_cap`
    parallel_cap: int = 24


def families_of(run):
    fams = defaultdict(list)
    for a in sorted((a for a in run["attempts"] if a.get("role") == "worker"), key=lambda a: (a.get("seq") or 0)):
        fams[a["family"]].append(a)
    return dict(sorted(fams.items()))


def _apply(a, pol):
    """Cap transforms: an attempt over the call cap or trial budget would not have finished, so it yields no survivor."""
    a, cost, lost = dict(a), a.get("cost_usd") or 0.0, False
    for have, cap in ((a.get("calls"), pol.call_cap), (a.get("trials_used"), pol.trial_budget)):
        if cap is not None and have and have > cap:
            cost, lost = cost * cap / have, True
    if lost:
        a["status"] = "partial"
    return {"survived": survives_screen(a), "cost": cost, "referee": a.get("referee"), "minutes": a.get("minutes"),
            "trials": min(a.get("trials_used") or 0, pol.trial_budget or 10 ** 9)}


def replay(policy, run):
    fams = families_of(run)
    names, rev = list(fams), {f: [] for f in fams}
    spent = trials = unexplored = attempts = 0
    wall_min, rounds, per_round = 0.0, 0, []
    for rnd in range(policy.max_rounds):
        picks = policy.select(View(names, rev, rnd, spent))[:policy.wave_size]
        if not picks:
            break
        rounds, launched, longest = rounds + 1, 0, 0.0
        for f in picks:
            if spent >= policy.budget_usd:
                break
            i = len(rev[f])
            if i >= len(fams[f]) or (i and rev[f][-1] is None):
                rev[f].append(None)
                unexplored += 1
                continue
            o = _apply(fams[f][i], policy)
            rev[f].append(o)
            spent, trials, launched, attempts = spent + o["cost"], trials + o["trials"], launched + 1, attempts + 1
            longest = max(longest, o["minutes"] or 1.0)
        wall_min += longest
        per_round.append(launched)
    return {"outcomes": rev, "spent_usd": spent, "trials": trials, "rounds": rounds, "attempts": attempts,
            "unexplored": unexplored, "wall_hours": wall_min / 60, "attempts_per_round": sum(per_round) / max(1, rounds)}


def reward(res, w=Weights()):
    surv = [o for outs in res["outcomes"].values() for o in outs if o and o["survived"]]
    conf = sum(o["referee"] == "confirmed" for o in surv)
    ref = sum(o["referee"] in ("not_confirmed", "invalid") for o in surv)
    unref = min(w.unrefereed_cap, sum(o["referee"] is None for o in surv))
    return (w.confirmed * conf + w.unrefereed * unref - w.refuted * ref - w.trial * res["trials"] - w.usd * res["spent_usd"]
            - w.wall_hour * res["wall_hours"] + w.parallel * min(w.parallel_cap, res["attempts_per_round"]))


def score(policy, runs, w=Weights()):
    """Per-run rewards (list, same order as runs) plus the share of requests replay could not answer."""
    out, unx, req = [], 0, 0
    for r in runs:
        res = replay(policy, r)
        out.append(reward(res, w))
        unx, req = unx + res["unexplored"], req + res["unexplored"] + res["attempts"]
    return {"rewards": out, "mean": sum(out) / len(out) if out else 0.0, "unexplored_share": unx / req if req else 0.0}
