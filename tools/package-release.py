#!/usr/bin/env python3
"""Produce a native release from an exact private detached source checkout."""
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys

TARGET = "aarch64-apple-darwin"
SYSTEM = ("/usr/lib/", "/System/Library/")


def run(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def version_tuple(value):
    if not re.fullmatch(r"\d+(?:\.\d+){0,2}", value):
        raise ValueError(f"invalid macOS version: {value}")
    return tuple(int(part) for part in value.split(".")) + (0,) * (3 - len(value.split(".")))


def inventory(root):
    files = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if path.is_symlink() or not (path.is_file() or path.is_dir()):
            raise ValueError(f"non-regular bundle entry: {relative}")
        if any(c in relative for c in "\n\r\t\\") or any(p in (".", "..") for p in relative.split("/")):
            raise ValueError(f"unsafe inventory path: {relative}")
        if path.is_file() and relative != "SHA256SUMS":
            files.append(f"{sha(path)}  {relative}\n")
    return "".join(files)


def locked_packages(lock):
    rows = [line.split() for line in lock.read_text().splitlines() if line and not line.startswith("#")]
    return sorted((row[0], row[1]) for row in rows)


def verify_packages(tree):
    installed = sorted(tuple(line.split()[:2]) for line in (tree / "installed-packages.tsv").read_text().splitlines() if line)
    if installed != locked_packages(tree / "dependencies.lock"):
        raise ValueError("installed Lua closure differs from dependencies.lock")


def native_files(root):
    for path in sorted(root.rglob("*")):
        if path.is_file() and "Mach-O" in run("file", "-b", str(path)):
            yield path


def verify_native(root):
    """Validate every Mach-O, including recursive loader-relative libraries."""
    floor = version_tuple("14.0")
    files = list(native_files(root))
    if not files:
        raise ValueError("runtime has no Mach-O binaries")
    for path in files:
        if run("lipo", "-archs", str(path)) != "arm64":
            raise ValueError(f"wrong native architecture: {path}")
        commands = run("otool", "-l", str(path))
        # 'version' also occurs in dylib load commands; read only OS commands.
        versions = []
        for block in commands.split("Load command"):
            if "LC_BUILD_VERSION" in block:
                versions += re.findall(r"\bminos\s+(\d+(?:\.\d+){1,2})", block)
            elif "LC_VERSION_MIN_MACOSX" in block:
                versions += re.findall(r"\bversion\s+(\d+(?:\.\d+){1,2})", block)
        if not versions:
            raise ValueError(f"missing native deployment version: {path}")
        floor = max(floor, *(version_tuple(v) for v in versions))
        library_id = run("otool", "-D", str(path)).splitlines()[1:]
        for line in run("otool", "-L", str(path)).splitlines()[1:]:
            dependency = line.strip().split(" (compatibility version", 1)[0]
            if dependency in library_id or dependency.startswith(SYSTEM):
                continue
            if not dependency.startswith("@loader_path/"):
                raise ValueError(f"external/unresolved dependency {dependency}: {path}")
            resolved = (path.parent / dependency[len("@loader_path/"):]).resolve()
            if not resolved.is_relative_to(root.resolve()) or not resolved.is_file():
                raise ValueError(f"missing/escaping native dependency {dependency}: {path}")
        subprocess.run(["codesign", "--verify", "--strict", str(path)], check=True)
    return ".".join(str(v) for v in floor[:2]) + (f".{floor[2]}" if floor[2] else "")


def admit(prepared, pin, expected_inventory):
    if sha(prepared / "SHA256SUMS") != expected_inventory:
        raise ValueError("prepared inventory digest differs")
    if inventory(prepared) != (prepared / "SHA256SUMS").read_text():
        raise ValueError("prepared inventory is not complete or hashes differ")
    metadata = json.loads((prepared / "RUNTIME.json").read_text())
    if metadata["runtime_commit"] != pin or metadata["target"] != TARGET:
        raise ValueError("prepared source pin or target differs")
    if metadata["lua_abi"] != "5.4" or metadata["os"] != "Darwin" or metadata["arch"] != "arm64":
        raise ValueError("prepared platform or ABI differs")
    tree = prepared / "share/nefor/runtime/lua-dependencies"
    verify_packages(tree)
    if sha(tree / "dependencies.lock") != metadata["dependencies_lock_sha256"]:
        raise ValueError("prepared dependency lock differs")
    if run(str(prepared / "bin/nefor"), "--version") != metadata["engine_version"] or not metadata["engine_version"].endswith(pin):
        raise ValueError("engine stamp differs from source pin")
    if verify_native(prepared) != metadata["minimum_macos"]:
        raise ValueError("native deployment floor differs")
    return metadata


def finalize(stage):
    """Close canonical release provenance and inventory immediately before tar."""
    target = stage.name.removeprefix("nefor-")
    platforms = {
        "aarch64-apple-darwin": ("Darwin", "arm64"),
        "x86_64-unknown-linux-gnu": ("Linux", "x86_64"),
        "aarch64-unknown-linux-gnu": ("Linux", "aarch64"),
    }
    if target not in platforms:
        raise ValueError(f"unsupported release target: {target}")
    repo = Path(__file__).resolve().parent.parent
    source = Path(os.environ.get("NEFOR_RELEASE_SOURCE_ROOT", repo))
    runtime_commit = run("git", "rev-parse", "HEAD", cwd=source)
    producer_commit = run("git", "rev-parse", "HEAD", cwd=repo)
    if any(not re.fullmatch("[a-f0-9]{40}", pin) for pin in (runtime_commit, producer_commit)):
        raise ValueError("release revisions must be full commit IDs")
    tree = stage / "share/nefor/runtime/lua-dependencies"
    verify_packages(tree)
    plugins = (stage / "share/nefor/plugins.manifest").read_text().splitlines()
    if not plugins or len(set(plugins)) != len(plugins):
        raise ValueError("release plugin manifest is empty or duplicated")
    metadata = {
        "schema_version": 1,
        "kind": "nefor-runtime",
        "runtime_commit": runtime_commit,
        "producer_commit": producer_commit,
        "target": target,
        "os": platforms[target][0],
        "arch": platforms[target][1],
        "lua_abi": "5.4",
        "dependencies_lock_sha256": sha(tree / "dependencies.lock"),
        "plugins": plugins,
        "engine_version": run(str(stage.resolve() / "bin/nefor"), "--version"),
    }
    if target == TARGET:
        metadata["minimum_macos"] = verify_native(stage)
    prepared_inventory = os.environ.get("NEFOR_RELEASE_PREPARED_INVENTORY_SHA256")
    if prepared_inventory is not None:
        if not re.fullmatch("[a-f0-9]{64}", prepared_inventory):
            raise ValueError("prepared inventory digest must be explicit SHA256")
        metadata["prepared_inventory_sha256"] = prepared_inventory
    (stage / "RUNTIME.json").write_text(json.dumps(metadata, indent=2, sort_keys=True) + "\n")
    (stage / "SHA256SUMS").write_text(inventory(stage))
    return metadata


def package(pin, output, prepared, expected_inventory):
    if not re.fullmatch("[a-f0-9]{40}", pin):
        raise ValueError("runtime pin must be a full lowercase commit ID")
    if not output.is_absolute() or output.exists():
        raise ValueError("output must be an absent absolute directory")
    if not re.fullmatch("[a-f0-9]{64}", expected_inventory):
        raise ValueError("prepared inventory digest must be explicit SHA256")
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        raise ValueError("release reuse supports native macOS arm64 only")
    repo = Path(__file__).resolve().parent.parent
    if run("git", "status", "--porcelain", "--untracked-files=normal", cwd=repo):
        raise ValueError("commit producer changes before packaging")
    admit(prepared, pin, expected_inventory)
    source = repo / "tmp/runtime-build" / pin / "source"
    if run("git", "rev-parse", "HEAD", cwd=source) != pin or run("git", "status", "--porcelain", cwd=source):
        raise ValueError("private source checkout is not clean at requested pin")
    output.mkdir(parents=True)
    binaries = output / "inputs"
    binaries.mkdir()
    for path in list((prepared / "bin").iterdir()) + list((prepared / "share/nefor/plugins").iterdir()):
        shutil.copy2(path, binaries / path.name)
    stage = output / f"nefor-{TARGET}"
    env = dict(os.environ, NEFOR_RELEASE_SOURCE_ROOT=str(source),
               NEFOR_LUA_DEPENDENCIES_ROOT=str(prepared / "share/nefor/runtime/lua-dependencies"))
    subprocess.run([str(repo / "tools/bundle-release.sh"), str(binaries), str(stage)], env=env, check=True)
    # Reassembly must preserve ALL prepared payload bytes, not just engine/hash.
    def payload(root):
        return {p.relative_to(root).as_posix(): sha(p) for p in root.rglob("*")
                if p.is_file() and p.relative_to(root).as_posix() not in ("RUNTIME.json", "SHA256SUMS")}
    if payload(stage) != payload(prepared):
        raise ValueError("canonical pinned-source layout differs from prepared payload")
    env["NEFOR_RELEASE_PREPARED_INVENTORY_SHA256"] = expected_inventory
    subprocess.run([str(repo / "tools/archive-release.sh"), str(stage)], env=env, check=True)
    metadata = json.loads((stage / "RUNTIME.json").read_text())
    shutil.rmtree(binaries)
    print(json.dumps(metadata, indent=2))
    print(f"archive sha256: {sha(stage.with_suffix('.tar.gz'))}")


if __name__ == "__main__":
    try:
        if len(sys.argv) == 3 and sys.argv[1] == "--finalize":
            finalize(Path(sys.argv[2]))
        elif len(sys.argv) == 5:
            package(sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3]).resolve(), sys.argv[4])
        else:
            raise ValueError("usage: package-release.py <source-pin> <absent-output-dir> <prepared-runtime> <inventory-sha256> | --finalize <release-root>")
    except (ValueError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
