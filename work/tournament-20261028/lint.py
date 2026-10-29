"""Pre-submit contract lint. Workers paste this verbatim into run_code (it is embedded in the protocol)."""
import json

REQ = ["id", "title", "file", "description", "severity"]
SEV = ["breakthrough", "useful", "background", "negative"]
KINDS = ["candidate", "negative-result", "trial-ledger", "method", "data-issue", "swarm-improvement",
         "wave-plan", "referee-report", "wave-digest", "final-report"]
NEED = {"candidate": ["spec_code", "claim", "metrics", "limitations", "decision", "kill_criterion"],
        "negative-result": ["claim", "metrics", "proof", "decision"],
        "trial-ledger": ["metrics"], "referee-report": ["claim", "metrics", "proof", "decision"],
        "wave-plan": ["decision", "evidence"], "swarm-improvement": ["evidence", "decision"]}


def lint(records, seed_urls):
    errs, seen = [], set()
    if not isinstance(records, list) or not records:
        return ["papers must be a non-empty list"]
    for i, r in enumerate(records):
        w = f"rec[{i}]"
        if not isinstance(r, dict):
            errs.append(w + " not an object"); continue
        for k in REQ:
            if not isinstance(r.get(k), str) or not r[k].strip():
                errs.append(f"{w} missing/empty required string field {k}")
        if r.get("id") in seen:
            errs.append(f"{w} duplicate id {r.get('id')}")
        seen.add(r.get("id"))
        if r.get("severity") not in SEV:
            errs.append(f"{w} severity must be one of {SEV}")
        if r.get("file") not in seed_urls:
            errs.append(f"{w} file must be exactly one of seed_urls")
        if r.get("kind") not in KINDS:
            errs.append(f"{w} kind must be one of {KINDS}")
        for k in NEED.get(r.get("kind"), []):
            if r.get(k) in (None, "", [], {}):
                errs.append(f"{w} kind {r.get('kind')} needs {k}")
        if any(v is None for v in r.values()):
            errs.append(f"{w} has a null value")
        blob = json.dumps(r)
        if "/work/" in blob or "/tmp/" in blob:
            errs.append(f"{w} points at an ephemeral path; inline the code instead")
        if len(r.get("spec_code", "")) > 7000:
            errs.append(f"{w} spec_code over 7000 chars")
        if r.get("kind") == "candidate":
            m = r.get("metrics") or {}
            for k in ["trials_used", "trials_budget", "per_year", "net_bps_per_trade", "n_trades", "placebo_max",
                      "top3_day_share", "top3_name_share", "decay", "beats_placebo_max", "mde_ok"]:
                if isinstance(m, dict) and k not in m:
                    errs.append(f"{w} metrics lacks {k}")
    if len(json.dumps(records)) > 150_000:
        errs.append("filing over 150 KB; shorten")
    return errs
