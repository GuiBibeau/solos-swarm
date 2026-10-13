#!/usr/bin/env python3
"""Operator/build bootstrap only: fetch and verify immutable public tokenizer assets."""
import argparse
import hashlib
import json
from pathlib import Path
import urllib.request

MODELS = {
    "deepseek-v41": {
        "model": "deepseek-ai/DeepSeek-V4.1-Flash",
        "revision": "5254448103b6254fc646dacbca1cbbf20e9216c1",
        "license": "MIT",
        "files": {
            "tokenizer.json": "c90dfa01249db1be4245780a052ede752e1361c612ac6d08e2bdada7d599476b",
            "tokenizer_config.json": "6ac8c8dc065ed118161d02dd532749ae3f52c243deac27872134fae2f50d8547",
            "chat_template.jinja": "d959d804d5101b79a49b6ff1bf3c54cd5affa9a5a78c503d925f3090040b31c3",
            "LICENSE": "f2c6c602815669d292889e5be8c802f2ed950653b77999b1584e8e6aed25d040",
        },
    },
    "qwen3-06b": {
        "model": "Qwen/Qwen3-0.6B",
        "revision": "c1899de289a04d12100db370d81485cdf75e47ca",
        "license": "Apache-2.0",
        "extracted_template_sha256": "a55ee1b1660128b7098723e0abcd92caa0788061051c62d51cbe87d9cf1974d8",
        "files": {
            "tokenizer.json": "aeb13307a71acd8fe81861d94ad54ab689df773318809eed3cbe794b4492dae4",
            "tokenizer_config.json": "d5d09f07b48c3086c508b30d1c9114bd1189145b74e982a265350c923acd8101",
            "LICENSE": "832dd9e00a68dd83b3c3fb9f5588dad7dcf337a0db50f7d9483f310cd292e92e",
        },
    },
    "qwen3-4b": {
        "model": "Qwen/Qwen3-4B",
        "revision": "1cfa9a7208912126459214e8b04321603b3df60c",
        "license": "Apache-2.0",
        "extracted_template_sha256": "a55ee1b1660128b7098723e0abcd92caa0788061051c62d51cbe87d9cf1974d8",
        "files": {
            "tokenizer.json": "aeb13307a71acd8fe81861d94ad54ab689df773318809eed3cbe794b4492dae4",
            "tokenizer_config.json": "d5d09f07b48c3086c508b30d1c9114bd1189145b74e982a265350c923acd8101",
            "config.json": "8ba006f74fecfaaeb392872a60f4a480e7ec9860153d2e1b769ec81f9a147f8a",
            "LICENSE": "832dd9e00a68dd83b3c3fb9f5588dad7dcf337a0db50f7d9483f310cd292e92e",
        },
    },
}


def bootstrap(output):
    manifests = []
    for name, spec in MODELS.items():
        root = output / name
        root.mkdir(parents=True, exist_ok=True)
        manifest = {key: spec[key] for key in ("model", "revision", "license")}
        manifest["artifacts"] = {}
        for file, expected in spec["files"].items():
            path = root / file
            source = f"https://huggingface.co/{spec['model']}/resolve/{spec['revision']}/{file}"
            if path.exists():
                content = path.read_bytes()
            else:
                with urllib.request.urlopen(source, timeout=60) as response:
                    content = response.read(32 * 1024 * 1024 + 1)
            if len(content) > 32 * 1024 * 1024 or hashlib.sha256(content).hexdigest() != expected:
                raise ValueError(f"Pinned public artifact mismatch: {name}/{file}")
            if not path.exists():
                with path.open("xb") as handle:
                    handle.write(content)
            manifest["artifacts"][file] = {"sha256": expected, "bytes": len(content), "source": source}
        if "extracted_template_sha256" in spec:
            template = json.loads((root / "tokenizer_config.json").read_text())["chat_template"].encode()
            expected = spec["extracted_template_sha256"]
            if hashlib.sha256(template).hexdigest() != expected:
                raise ValueError("Pinned Qwen template extraction mismatch")
            path = root / "chat_template.jinja"
            if path.exists() and path.read_bytes() != template:
                raise ValueError("Existing Qwen template is altered")
            if not path.exists():
                with path.open("xb") as handle:
                    handle.write(template)
            manifest["artifacts"][path.name] = {"sha256": expected, "bytes": len(template), "source": manifest["artifacts"]["tokenizer_config.json"]["source"], "extracted_field": "chat_template"}
        if name == "qwen3-4b":
            template = (Path(__file__).resolve().parents[1] / "assets/tokenizers/qwen3-4b-gguf-chat-template.jinja").read_bytes()
            expected = "57f1fd00f0013a2be96aa79b857391f27e23df5b5f847072b524c897e24d0361"
            if hashlib.sha256(template).hexdigest() != expected:
                raise ValueError("Pinned GGUF template mismatch")
            path = root / "gguf_chat_template.jinja"
            if path.exists() and path.read_bytes() != template:
                raise ValueError("Existing GGUF template is altered")
            if not path.exists():
                with path.open("xb") as handle:
                    handle.write(template)
            manifest["artifacts"][path.name] = {"sha256": expected, "bytes": len(template), "source": "https://huggingface.co/Qwen/Qwen3-4B-GGUF/tree/bc640142c66e1fdd12af0bd68f40445458f3869b", "extracted_field": "tokenizer.chat_template", "adapter_profile": "llama.cpp-b11429", "enable_thinking": False}
        (root / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        manifests.append(manifest)
    return manifests


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path("work/tokenizers"))
    args = parser.parse_args()
    print(json.dumps(bootstrap(args.output)))
