#!/usr/bin/env python3
"""Compare offline pinned tokenizer vectors with a running local llama.cpp receiver."""
import argparse
import hashlib
import json
from pathlib import Path
import urllib.request


def request(base, path, value=None):
    data = None if value is None else json.dumps(value, ensure_ascii=False).encode()
    call = urllib.request.Request(base.rstrip("/") + path, data=data,
                                  headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(call, timeout=15) as reply:
        body = reply.read(4 * 1024 * 1024 + 1)
    if len(body) > 4 * 1024 * 1024:
        raise ValueError("Receiver response exceeds probe bound")
    return json.loads(body)


def probe(base, vectors, verify_usage=False):
    props = request(base, "/props")
    template_hash = hashlib.sha256(props["chat_template"].encode()).hexdigest()
    checks = []
    for case in vectors["cases"]:
        formatted = request(base, "/apply-template", {
            "messages": case["messages"], "tools": case["tools"],
            "add_generation_prompt": True,
            "chat_template_kwargs": {"enable_thinking": vectors["enable_thinking"]},
        })["prompt"]
        observed = request(base, "/tokenize", {
            "content": formatted, "add_special": False, "parse_special": True,
        })["tokens"]
        checks.append({"case": case["name"],
                       "rendered_prompt_matches": formatted == case["prompt"],
                       "token_ids_match": observed == case["tokens"],
                       "expected_tokens": len(case["tokens"]), "observed_tokens": len(observed),
                       "rendered_prompt_sha256": hashlib.sha256(formatted.encode()).hexdigest()})
        if verify_usage:
            completion = request(base, "/v1/chat/completions", {
                "model": props["model_alias"], "messages": case["messages"],
                "tools": case["tools"], "max_tokens": 1, "temperature": 0,
                "chat_template_kwargs": {"enable_thinking": vectors["enable_thinking"]},
            })
            checks[-1]["usage_prompt_tokens"] = completion["usage"]["prompt_tokens"]
            checks[-1]["usage_matches"] = completion["usage"]["prompt_tokens"] == len(case["tokens"])
            checks[-1]["completion_tokens"] = completion["usage"]["completion_tokens"]
    passed = (template_hash == vectors["template_sha256"] and
              bool(checks) and all(c["rendered_prompt_matches"] and c["token_ids_match"] and
                                  (not verify_usage or c["usage_matches"]) for c in checks))
    return {"schema_version": 1, "verdict": "PASS" if passed else "FAIL",
            "model": vectors["model"], "revision": vectors["revision"],
            "tokenizer_sha256": vectors["tokenizer_sha256"],
            "expected_template_sha256": vectors["template_sha256"],
            "observed_template_sha256": template_hash,
            "enable_thinking": vectors["enable_thinking"],
            "usage_verified": verify_usage,
            "receiver_build": props.get("build_info"), "receiver_model": props.get("model_alias"),
            "per_slot_context_tokens": props["default_generation_settings"]["n_ctx"], "checks": checks}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True)
    parser.add_argument("--vectors", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--verify-usage", action="store_true", help="Generate at most one local token per case and compare actual usage receipts")
    args = parser.parse_args()
    result = probe(args.base, json.loads(args.vectors.read_text()), args.verify_usage)
    with args.output.open("x") as handle:
        json.dump(result, handle, indent=2)
        handle.write("\n")
    print(json.dumps(result))
    raise SystemExit(0 if result["verdict"] == "PASS" else 1)
