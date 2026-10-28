"""Policy-as-code. A policy decides, round by round, which families get the next worker attempt, under knobs for
wave size, per-attempt call cap and trial budget, and a stop-after-N-null rule. It sees only what the replay has
revealed so far (screen survival, cost), never referee verdicts. Policies are built from small JSON specs, so a
proposal from a self-review agent is data, not code."""
from dataclasses import dataclass


@dataclass
class View:
    families: list      # all family names in the run, in stable order
    revealed: dict      # family -> list of outcomes so far; outcome = None when the family had nothing more recorded
    round: int
    spent_usd: float


def _hits(outs):
    return sum(1 for o in outs if o and o["survived"])


def _exhausted(outs):
    return bool(outs) and outs[-1] is None


class Policy:
    kind = "base"

    def __init__(self, wave_size=24, max_rounds=2, budget_usd=100.0, call_cap=None, trial_budget=None, **_):
        self.wave_size, self.max_rounds, self.budget_usd = int(wave_size), int(max_rounds), float(budget_usd)
        self.call_cap, self.trial_budget = call_cap, trial_budget

    def spec(self):
        return {"kind": self.kind, **{k: v for k, v in vars(self).items()}}

    def select(self, view):
        raise NotImplementedError


class Baseline(Policy):
    """What the tournament does today: wave 1 gives every family one worker; wave 2 repeats the families that had a
    screen survivor first (deepen), then fills the rest with second passes in family order; then stop."""
    kind = "baseline"

    def select(self, view):
        if view.round == 0:
            return view.families[:self.wave_size]
        if view.round > 1:
            return []
        live = [f for f in view.families if not _exhausted(view.revealed.get(f, []))]
        lead = [f for f in live if _hits(view.revealed.get(f, []))]
        return (lead + [f for f in live if f not in lead])[:self.wave_size]


class Adaptive(Policy):
    """Round 1 touches every family once. Later rounds back families with survivors, drop a family after
    `stop_after_null` consecutive null attempts, and never give a family more than `per_family_max` attempts."""
    kind = "adaptive"

    def __init__(self, stop_after_null=2, per_family_max=4, first_wave_all=True, **kw):
        super().__init__(**kw)
        self.stop_after_null, self.per_family_max, self.first_wave_all = int(stop_after_null), int(per_family_max), bool(first_wave_all)

    def _nulls(self, outs):
        n = 0
        for o in reversed(outs):
            if o and o["survived"]:
                break
            n += 1
        return n

    def select(self, view):
        if view.round >= self.max_rounds or view.spent_usd >= self.budget_usd:
            return []
        if view.round == 0 and self.first_wave_all:
            return view.families[:self.wave_size]
        live = [f for f in view.families if not _exhausted(view.revealed.get(f, []))
                and len(view.revealed.get(f, [])) < self.per_family_max
                and self._nulls(view.revealed.get(f, [])) < self.stop_after_null]
        live.sort(key=lambda f: (-_hits(view.revealed.get(f, [])), len(view.revealed.get(f, []))))
        return live[:self.wave_size]


KINDS = {"baseline": Baseline, "adaptive": Adaptive}


def from_spec(spec):
    spec = dict(spec)
    return KINDS[spec.pop("kind")](**spec)
