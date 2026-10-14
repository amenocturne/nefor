#!/usr/bin/env python3
"""Produce a native release from an exact private detached source checkout."""
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import tempfile
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
    if sha(tree / "dependencies.lock") != sha(source / "lua/nefor-pm/dependencies.lock"):
        raise ValueError("source dependency lock differs")
    plugins = (stage / "share/nefor/plugins.manifest").read_text().splitlines()
    if plugins != run(str(source / "tools/plugin-binaries.sh")).splitlines():
        raise ValueError("source plugin manifest differs")
    for binary in [stage / "bin/nefor", stage / "bin/mag", *(stage / "share/nefor/plugins" / name for name in plugins)]:
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError(f"missing release executable: {binary}")
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
    if not metadata["engine_version"].endswith(runtime_commit):
        raise ValueError("engine stamp differs from source pin")
    if target != TARGET:
        verify_elf(stage, target)
    (stage / "RUNTIME.json").write_text(json.dumps(metadata, indent=2, sort_keys=True) + "\n")
    (stage / "SHA256SUMS").write_text(inventory(stage))
    return metadata


PLATFORMS = {
    "aarch64-apple-darwin": ("Darwin", "arm64"),
    "x86_64-unknown-linux-gnu": ("Linux", "x86_64"),
    "aarch64-unknown-linux-gnu": ("Linux", "aarch64"),
}
REPO = Path(__file__).resolve().parent.parent


def verify_elf(root, target):
    # Check header bytes rather than executing or trusting a metadata label.
    machine = {"x86_64-unknown-linux-gnu": 62, "aarch64-unknown-linux-gnu": 183}[target]
    required = {root / "bin/nefor", root / "bin/mag"}
    required.update((root / "share/nefor/plugins").iterdir())
    for path in root.rglob("*"):
        if not path.is_file():
            continue
        with path.open("rb") as handle:
            header = handle.read(20)
        if path in required or path.suffix == ".so" or header.startswith(b"\x7fELF"):
            if (len(header) < 20 or header[:6] != b"\x7fELF\x02\x01"
                    or int.from_bytes(header[18:20], "little") != machine):
                raise ValueError(f"wrong ELF architecture: {path}")


def source_version(source, pin):
    # Historical build.rs uses abbreviated describe output. Stamp from the pinned
    # Git checkout ourselves, never from a caller-supplied override.
    version = run("git", "describe", "--tags", "--always", "--long", "--abbrev=40",
                  "--match", "v[0-9]*.[0-9]*.[0-9]*", cwd=source).removeprefix("v")
    if not version.endswith(pin):
        raise ValueError("source version does not identify resolved commit")
    return version


def package(target, output, revision="HEAD"):
    repo = REPO
    if target not in PLATFORMS:
        raise ValueError(f"unsupported release target: {target}")
    if not str(output):
        raise ValueError("output directory is required")
    output = Path(output)
    if not output.is_absolute():
        output = repo / output
    # Do not resolve the leaf: an existing dangling symlink is not absent.
    output = output.parent.resolve() / output.name
    if output.exists() or output.is_symlink():
        raise ValueError("output must be an absent directory")
    if (platform.system(), platform.machine()) != PLATFORMS[target]:
        raise ValueError(f"package requires native host {target}; cross packaging is unsupported")
    if "NEFOR_VERSION_OVERRIDE" in os.environ:
        raise ValueError("NEFOR_VERSION_OVERRIDE is not supported for packaging")
    if not revision or revision.startswith("-"):
        raise ValueError("invalid source revision")
    pin = run("git", "rev-parse", "--verify", "--end-of-options", revision + "^{commit}", cwd=repo)
    if not re.fullmatch("[a-f0-9]{40}", pin):
        raise ValueError("source revision must resolve to a full commit")
    if run("git", "status", "--porcelain", "--untracked-files=normal", cwd=repo):
        raise ValueError("commit producer changes before packaging")
    cache = Path(os.environ.get("CARGO_TARGET_DIR", repo / "target/package"))
    if not cache.is_absolute():
        cache = repo / cache
    cache = cache.resolve()
    if cache == output or output in cache.parents:
        raise ValueError("Cargo cache must not be inside package output")
    for tool in ["file", *(["lipo", "otool", "codesign"] if target == TARGET else [])]:
        if shutil.which(tool) is None:
            raise ValueError(f"packaging requires installed {tool}")
    output.mkdir(parents=True)
    env = dict(os.environ, CARGO_TARGET_DIR=str(cache),
               NEFOR_LUA_DEPENDENCIES_ROOT=str(output / "lua-dependencies"))
    # Producer tools remain authoritative; all payload/build inputs come from pin.
    with tempfile.TemporaryDirectory(prefix="package-source-", dir=output) as temporary:
        source = Path(temporary) / "source"
        added = False
        try:
            subprocess.run(["git", "worktree", "add", "--detach", str(source), pin], cwd=repo, check=True)
            added = True
            env["NEFOR_RELEASE_SOURCE_ROOT"] = str(source)
            env["NEFOR_VERSION_OVERRIDE"] = source_version(source, pin)
            subprocess.run([str(repo / "tools/prepare-lua-dependencies.sh"),
                            str(source / "lua/nefor-pm/nefor-pm-dev-1.rockspec"),
                            str(source / "lua/nefor-pm/dependencies.lock"),
                            env["NEFOR_LUA_DEPENDENCIES_ROOT"]], cwd=source, env=env, check=True)
            subprocess.run(["cargo", "build", "--workspace", "--release", "--locked", "--target", target],
                           cwd=source, env=env, check=True)
            binaries = cache / target / "release"
            stage = output / f"nefor-{target}"
            subprocess.run([str(repo / "tools/bundle-release.sh"), str(binaries), str(stage)],
                           cwd=repo, env=env, check=True)
            if target == TARGET:
                for path in [*(stage / "bin").iterdir(), *(stage / "share/nefor/plugins").iterdir()]:
                    subprocess.run(["codesign", "--force", "--sign", "-", str(path)], check=True)
            subprocess.run([str(repo / "tools/archive-release.sh"), str(stage)], cwd=repo, env=env, check=True)
            metadata = json.loads((stage / "RUNTIME.json").read_text())
            print(json.dumps(metadata, indent=2))
            print(f"archive: {stage.with_suffix('.tar.gz')}")
            print(f"archive sha256: {sha(stage.with_suffix('.tar.gz'))}")
        finally:
            if added:
                subprocess.run(["git", "worktree", "remove", "--force", str(source)], cwd=repo, check=True)


if __name__ == "__main__":
    try:
        if len(sys.argv) == 3 and sys.argv[1] == "--finalize":
            finalize(Path(sys.argv[2]))
        elif len(sys.argv) in (3, 4):
            package(*sys.argv[1:])
        else:
            raise ValueError("usage: package-release.py TARGET OUTPUT [REVISION] | --finalize RELEASE_ROOT")
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
