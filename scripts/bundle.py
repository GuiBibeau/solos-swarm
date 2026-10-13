#!/usr/bin/env python3
"""Create, verify, and import compiled artifacts; import never builds or downloads."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import struct
import tarfile
import tempfile


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def sha(path):
    hasher = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            hasher.update(block)
    return hasher.hexdigest()


def safe_name(name):
    return bool(name) and not Path(name).is_absolute() and ".." not in Path(name).parts and "\0" not in name


def inventory(source):
    entries = {}
    for path in sorted(source.rglob("*")):
        name = str(path.relative_to(source))
        if name == "bundle-manifest.json":
            raise ValueError("Prepared source must not supply its own bundle manifest")
        if path.name in {"auth.json", ".env", "credentials.json"} or path.name.endswith((".key", "private-key.pem")):
            raise ValueError("Private credentials must be staged separately from the public bundle")
        if path.is_symlink():
            if not path.resolve().is_relative_to(source.resolve()):
                raise ValueError("Artifact symlink escapes the prepared tree")
            entries[name] = {"symlink": os.readlink(path)}
        elif path.is_file():
            entries[name] = {"sha256": sha(path), "size": path.stat().st_size, "mode": path.stat().st_mode & 0o555}
    if not entries:
        raise ValueError("An empty artifact bundle cannot qualify")
    return entries


def validate_executables(target, requirements, entries, read_header):
    machines = {"x86_64-unknown-linux-gnu": 62, "x86_64-unknown-linux-musl": 62,
                "aarch64-unknown-linux-gnu": 183, "aarch64-unknown-linux-musl": 183}
    if target not in machines:
        raise ValueError("A supported compiled Linux build target is required")
    binaries = requirements.get("executable_artifacts", [])
    if not binaries or not isinstance(binaries, list) or len(set(binaries)) != len(binaries):
        raise ValueError("Nonempty immutable executable artifact selection is required")
    for name in binaries:
        entry = entries.get(name)
        if not entry or "symlink" in entry or not entry.get("mode", 0) & 0o111:
            raise ValueError("Required Linux executable is absent or not executable")
        header = read_header(name)
        if len(header) < 64 or header[:6] != b"\x7fELF\x02\x01" or struct.unpack_from("<HH", header, 16) not in {(2, machines[target]), (3, machines[target])}:
            raise ValueError("Linux ELF architecture or executable format differs from declared target")


def create(source, output, revision, target, requirements):
    source, output = Path(source).resolve(), Path(output).resolve()
    if output.is_relative_to(source):
        raise ValueError("Bundle output must be outside its prepared source")
    if not revision or not target or not isinstance(requirements, dict):
        raise ValueError("Source revision, build target and requirements are mandatory")
    entries = inventory(source)
    mandatory = requirements.get("required_artifacts", [])
    if not mandatory or any(name not in entries for name in mandatory):
        raise ValueError("Missing required artifact or empty required-artifact selection")
    if not requirements.get("features") or not requirements.get("library_requirements") or not requirements.get("isolation_profile"):
        raise ValueError("Build features, library requirements and isolation profile must be recorded")
    def header(name):
        with (source / name).open("rb") as handle:
            return handle.read(64)
    validate_executables(target, requirements, entries, header)
    manifest = {"schema_version": 1, "source_revision": revision, "build_target": target,
                "requirements": requirements, "artifacts": entries}
    manifest["manifest_sha256"] = hashlib.sha256(canonical(manifest)).hexdigest()
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("xb") as destination, tarfile.open(fileobj=destination, mode="w", format=tarfile.PAX_FORMAT) as archive:
        for name, entry in entries.items():
            info = archive.gettarinfo(str(source / name), arcname=name)
            info.uid, info.gid, info.uname, info.gname, info.mtime = 0, 0, "", "", 0
            if "mode" in entry:
                info.mode = entry["mode"]
            if info.isfile():
                with (source / name).open("rb") as handle:
                    archive.addfile(info, handle)
            else:
                archive.addfile(info)
        body = canonical(manifest) + b"\n"
        info = tarfile.TarInfo("bundle-manifest.json")
        info.mode, info.size = 0o444, len(body)
        archive.addfile(info, io.BytesIO(body))
    verify(output)
    return {"archive_sha256": sha(output), "manifest_sha256": manifest["manifest_sha256"], "output": str(output)}


def verify(path):
    with tarfile.open(path, "r:") as archive:
        members = archive.getmembers()
        if len(members) > 100_000 or len({member.name for member in members}) != len(members):
            raise ValueError("Bundle has duplicate members or exceeds file-count bound")
        if any(not safe_name(member.name) or not (member.isfile() or member.issym()) for member in members):
            raise ValueError("Bundle contains an unsafe archive member")
        info = archive.getmember("bundle-manifest.json")
        if not info.isfile() or info.size > 32 * 1024 * 1024:
            raise ValueError("Bundle manifest exceeds its bound")
        manifest = json.load(archive.extractfile(info))
        expected_hash = manifest.pop("manifest_sha256")
        if expected_hash != hashlib.sha256(canonical(manifest)).hexdigest():
            raise ValueError("Manifest digest mismatch")
        manifest["manifest_sha256"] = expected_hash
        entries = manifest["artifacts"]
        if set(entries) != {member.name for member in members if member.name != "bundle-manifest.json"}:
            raise ValueError("Artifact selection differs from its frozen manifest")
        for name, entry in entries.items():
            member = archive.getmember(name)
            if "symlink" in entry:
                target = Path(name).parent / entry["symlink"]
                normalized = os.path.normpath(str(target))
                if not member.issym() or member.linkname != entry["symlink"] or not safe_name(normalized):
                    raise ValueError("Unsafe or altered artifact symlink")
            else:
                if not member.isfile() or member.size != entry["size"] or member.mode & 0o777 != entry["mode"]:
                    raise ValueError("Artifact shape or permissions changed")
                digest = hashlib.sha256()
                handle = archive.extractfile(member)
                for block in iter(lambda: handle.read(1024 * 1024), b""):
                    digest.update(block)
                if digest.hexdigest() != entry["sha256"]:
                    raise ValueError("Artifact digest mismatch")
        required = manifest["requirements"].get("required_artifacts", [])
        if not required or any(name not in entries for name in required):
            raise ValueError("Missing mandatory artifact")
        validate_executables(manifest["build_target"], manifest["requirements"], entries,
                             lambda name: archive.extractfile(archive.getmember(name)).read(64))
    return manifest


def import_bundle(path, destination):
    manifest = verify(path)
    destination = Path(destination).resolve()
    if destination.exists():
        raise ValueError("Import destination already exists; do not overwrite a candidate")
    destination.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=".bundle-", dir=destination.parent))
    try:
        with tarfile.open(path, "r:") as archive:
            archive.extractall(staging, filter="data")
        for name, entry in manifest["artifacts"].items():
            target = staging / name
            if "symlink" not in entry:
                if sha(target) != entry["sha256"]:
                    raise ValueError("Imported artifact differs from verified bytes")
                target.chmod(entry["mode"])
        staging.rename(destination)
    except Exception:
        for target in staging.rglob("*"):
            if not target.is_symlink():
                target.chmod(target.stat().st_mode | 0o700)
        shutil.rmtree(staging)
        raise
    return {"archive_sha256": sha(path), "manifest_sha256": manifest["manifest_sha256"], "destination": str(destination)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="action", required=True)
    make = commands.add_parser("create")
    make.add_argument("--source", type=Path, required=True)
    make.add_argument("--output", type=Path, required=True)
    make.add_argument("--source-revision", required=True)
    make.add_argument("--target", required=True)
    make.add_argument("--requirements", type=Path, required=True)
    check = commands.add_parser("verify")
    check.add_argument("bundle", type=Path)
    install = commands.add_parser("import")
    install.add_argument("bundle", type=Path)
    install.add_argument("--destination", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.action == "create":
            result = create(args.source, args.output, args.source_revision, args.target, json.loads(args.requirements.read_text()))
        elif args.action == "verify":
            result = {"archive_sha256": sha(args.bundle), "manifest": verify(args.bundle)}
        else:
            result = import_bundle(args.bundle, args.destination)
        print(json.dumps(result, indent=2))
    except (OSError, ValueError, KeyError, tarfile.TarError) as error:
        parser.exit(2, f"bundle: {error}\n")


if __name__ == "__main__":
    main()
