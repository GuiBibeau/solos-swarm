"""Optional drive.py hook: after a close step, ingest evidence, run the guard on the proposal, record the verdict.
The proposal is never applied here: `approved_policy()` returns a spec only when a stored verdict says adopt."""
import json
from pathlib import Path

from .guard import decide
from .ingest import ingest
from .policy import Baseline
from .schema import read_history, write_history

BASELINE = Baseline().spec()


def proposal_from(evidence_dir, override=None):
    """A proposal is {"prompt_patch": str, "policy": {...}?, "tuned_on": [...]?}. Prompt text has no replay semantics."""
    if override and Path(override).exists():
        return json.loads(Path(override).read_text())
    st = Path(evidence_dir) / "state.json"
    prop = {}
    for f in (json.loads(st.read_text()).get("findings", []) if st.exists() else []):
        for r in (f.get("result") or {}).get("papers", []):
            if isinstance(r, dict) and r.get("prompt_patch"):
                prop.setdefault("prompt_patch", str(r["prompt_patch"])[:500])
            if isinstance(r, dict) and isinstance(r.get("policy"), dict):
                prop["policy"] = r["policy"]
    return prop


def after_close(step, root, log, override=None, **kw):
    """root = the tournament dir (evidence/<step>/ collected runs, evidence/replay-history.jsonl accumulated)."""
    root = Path(root)
    hist = root / "evidence" / "replay-history.jsonl"
    runs = {r["run_id"]: r for r in (read_history(hist) if hist.exists() else [])}
    for d in sorted((root / "evidence").glob("*/state.json")):
        for r in ingest([d.parent], {str(d.parent): d.parent.name}):
            runs[r["run_id"]] = r
    write_history(list(runs.values()), hist)
    prop = proposal_from(root / "evidence" / step, override or root / f"replay-proposal-{step}.json")
    if not prop.get("policy"):
        verdict = {"adopt": False, "verdict": "unscored: no policy proposal (a prompt_patch alone cannot be replayed)",
                   "prompt_patch_present": bool(prop.get("prompt_patch")), "n_runs": len(runs)}
    else:
        verdict = decide(list(runs.values()), BASELINE, prop["policy"], tuned_on=prop.get("tuned_on"), **kw)
    (root / "evidence" / f"{step}-replay-verdict.json").write_text(json.dumps(verdict, indent=1))
    log({"step": step, "replay_verdict": verdict["verdict"], "adopt": verdict["adopt"]})
    return verdict


def approved_policy(root, step):
    p = Path(root) / "evidence" / f"{step}-replay-verdict.json"
    if not p.exists():
        return None
    v = json.loads(p.read_text())
    return v.get("candidate") if v.get("adopt") else None
