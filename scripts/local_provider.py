#!/usr/bin/env python3
"""Prepare verified public inference assets before an offline deployment starts."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import tarfile
import urllib.request

ASSETS = {
    "llama-b11429-bin-ubuntu-x64.tar.gz": (
        "https://github.com/ggml-org/llama.cpp/releases/download/b11429/llama-b11429-bin-ubuntu-x64.tar.gz",
        "f6d25dde8f51133143d1453da4fd5f73b145127177612a283bf7995957af3392", 17693462),
    "Qwen3-4B-Q4_K_M.gguf": (
        "https://huggingface.co/Qwen/Qwen3-4B-GGUF/resolve/bc640142c66e1fdd12af0bd68f40445458f3869b/Qwen3-4B-Q4_K_M.gguf",
        "7485fe6f11af29433bc51cab58009521f205840f5b4ae3a32fa7f92e8534fdf5", 2497280256),
}
SERVER_SHA256 = "5faf410bbbc00a1b94b09ee01623699f255365628dfa73b1ee552d9e52a2231d"

def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()

def verify_extraction(archive_path, root):
    """Check the whole dynamic-library tree before executing any extracted binary."""
    expected = set()
    with tarfile.open(archive_path, "r:gz") as archive:
        for member in archive.getmembers():
            relative = Path(member.name)
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError("executable archive escapes its destination")
            if relative == Path("."):
                continue
            expected.add(relative)
            expected.update(parent for parent in relative.parents if parent != Path("."))
            target = root / relative
            metadata = target.lstat()
            if member.isdir():
                if not stat.S_ISDIR(metadata.st_mode):
                    raise ValueError("extracted directory differs from pinned archive")
            elif member.issym():
                if (Path(member.linkname).name != member.linkname or member.linkname in (".", "..")
                        or not stat.S_ISLNK(metadata.st_mode) or os.readlink(target) != member.linkname):
                    raise ValueError("extracted library link differs from pinned archive")
            elif member.isfile() and not member.islnk():
                if not stat.S_ISREG(metadata.st_mode) or metadata.st_size != member.size:
                    raise ValueError("extracted library/binary differs from pinned archive")
                source = archive.extractfile(member)
                pinned = hashlib.sha256()
                with source:
                    for chunk in iter(lambda: source.read(1024 * 1024), b""):
                        pinned.update(chunk)
                if digest(target) != pinned.hexdigest() or (metadata.st_mode & 0o111) != (member.mode & 0o111):
                    raise ValueError("extracted library/binary differs from pinned archive")
            else:
                raise ValueError("unexpected executable archive entry")
    if {path.relative_to(root) for path in root.rglob("*")} != expected:
        raise ValueError("unexpected file in pinned executable tree")

def stage(directory):
    directory = directory.resolve()
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    for name, (url, expected, size) in ASSETS.items():
        target = directory / name
        if target.exists():
            if target.stat().st_size != size or digest(target) != expected:
                raise ValueError(f"existing artifact differs from pinned manifest: {name}")
            continue
        temporary = directory / (name + ".part")
        h = hashlib.sha256()
        count = 0
        try:
            with urllib.request.urlopen(url, timeout=60) as source, temporary.open("wb") as output:
                while chunk := source.read(1024 * 1024):
                    count += len(chunk)
                    if count > size:
                        raise ValueError("artifact exceeds pinned size")
                    h.update(chunk)
                    output.write(chunk)
                output.flush()
                os.fsync(output.fileno())
            if count != size or h.hexdigest() != expected:
                raise ValueError(f"artifact digest mismatch: {name}")
            temporary.chmod(0o444)
            temporary.rename(target)
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    executable_root = directory / "llama-b11429"
    if not executable_root.exists():
        temporary_root = directory / "extract-llama-b11429"
        temporary_root.mkdir(mode=0o700, exist_ok=True)
        if any(temporary_root.iterdir()):
            raise ValueError("incomplete extraction must be inspected before retry")
        with tarfile.open(directory / "llama-b11429-bin-ubuntu-x64.tar.gz", "r:gz") as archive:
            for member in archive.getmembers():
                if member.islnk() or member.isdev() or not (member.isfile() or member.isdir() or member.issym()):
                    raise ValueError("unexpected executable archive entry")
                if member.name.startswith("/") or ".." in Path(member.name).parts:
                    raise ValueError("executable archive escapes its destination")
                if member.issym() and (Path(member.linkname).name != member.linkname or member.linkname in (".", "..")):
                    raise ValueError("executable symlink must reference a sibling library")
            archive.extractall(temporary_root, filter="data")
        temporary_root.rename(executable_root)
    executables = list(executable_root.rglob("llama-server"))
    if len(executables) != 1:
        raise ValueError("pinned release must contain exactly one llama-server")
    server = executables[0]
    verify_extraction(directory / "llama-b11429-bin-ubuntu-x64.tar.gz", executable_root)
    if server.is_symlink() or digest(server) != SERVER_SHA256:
        raise ValueError("extracted llama-server differs from the pinned release")
    version = subprocess.run([str(server), "--version"], check=True, capture_output=True, text=True, timeout=15)
    manifest = {"version": 1, "provider": "llama.cpp", "server_release": "b11429", "model_revision": "bc640142c66e1fdd12af0bd68f40445458f3869b",
                "server": str(server), "model": str(directory / "Qwen3-4B-Q4_K_M.gguf"), "hardware": "cpu",
                "assets": {name: {"sha256": values[1], "size": values[2]} for name, values in ASSETS.items()},
                "server_sha256": digest(server), "server_version": version.stdout + version.stderr}
    (directory / "manifest.json").write_text(json.dumps(manifest, sort_keys=True, indent=2) + "\n")
    return manifest

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(stage(args.directory), sort_keys=True))

if __name__ == "__main__":
    main()
