#!/usr/bin/env python3
"""Produce a pinned guest image from an explicitly supplied minimal Python Linux tree."""
import argparse
import hashlib
import json
import pathlib
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--userland", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--size-mib", type=int, default=512)
    args = parser.parse_args()
    source = args.userland.resolve()
    output = args.output.absolute()
    if source == pathlib.Path("/") or not (source / "usr/bin/python3").exists() or output.exists() or not 256 <= args.size_mib <= 8192:
        parser.error("separate prepared Python userland, fresh output and bounded image size required")
    if any((source / forbidden).exists() for forbidden in ["root/.ssh", "root/.pi", "root/.codex", "home/debian/.pi", "home/debian/.ssh"]):
        parser.error("guest tree contains an ambient operator home")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="solos-guest-build-", dir=output.parent) as temporary:
        tree = pathlib.Path(temporary) / "root"
        shutil.copytree(source, tree, symlinks=True)
        for name in ["proc", "sys", "dev", "tmp", "work", "identity", "sbin"]:
            (tree / name).mkdir(exist_ok=True)
        agent = pathlib.Path(__file__).resolve().parent.parent / "assets/guest-agent.py"
        shutil.copyfile(agent, tree / "sbin/solos-guest-agent")
        (tree / "sbin/solos-guest-agent").chmod(0o755)
        library=tree / "usr/lib/solos"
        library.mkdir(parents=True,exist_ok=True)
        shutil.copyfile(agent.with_name("guest-broker.py"),library / "swarm_broker.py")
        site_packages=tree / "usr/lib/python3/dist-packages"
        site_packages.mkdir(parents=True,exist_ok=True)
        shutil.copyfile(agent.with_name("guest-broker.py"),site_packages / "swarm_broker.py")
        image = pathlib.Path(temporary) / "rootfs.ext4"
        with image.open("wb") as stream:
            stream.truncate(args.size_mib * 1024 * 1024)
        subprocess.run(["mkfs.ext4", "-q", "-F", "-d", str(tree), str(image)], check=True)
        image.chmod(0o444)
        image.rename(output)
    digest = hashlib.sha256(output.read_bytes()).hexdigest()
    record = {"path": str(output), "sha256": digest, "guest_agent_sha256": hashlib.sha256(agent.read_bytes()).hexdigest(), "network_interfaces": [], "readonly_rootfs": True}
    output.with_suffix(output.suffix + ".manifest.json").write_text(json.dumps(record, sort_keys=True, indent=2) + "\n")
    print(json.dumps(record, sort_keys=True))


if __name__ == "__main__":
    main()
