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


def package(pin, output):
    if not re.fullmatch("[a-f0-9]{40}", pin):
        raise ValueError("runtime pin must be a full lowercase commit ID")
    if not output.is_absolute() or output.exists():
        raise ValueError("output must be an absent absolute directory")
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        raise ValueError("package-runtime currently supports native macOS arm64 only")
    repo = Path(__file__).resolve().parent.parent
    producer = run("git", "rev-parse", "HEAD", cwd=repo)
    if run("git", "status", "--porcelain", "--untracked-files=normal", cwd=repo):
        raise ValueError("commit producer changes before packaging")
    if run("git", "rev-parse", f"{pin}^{{commit}}", cwd=repo) != pin:
        raise ValueError("runtime pin not available in producer repository")
    build = repo / "tmp" / "runtime-build" / pin
    source = build / "source"
    build.mkdir(parents=True, exist_ok=True)
    if not source.exists():
        subprocess.run(["git", "clone", "--no-hardlinks", "--no-checkout", str(repo), str(source)], check=True)
        subprocess.run(["git", "checkout", "--detach", pin], cwd=source, check=True)
    if run("git", "rev-parse", "HEAD", cwd=source) != pin or run("git", "status", "--porcelain", cwd=source):
        raise ValueError("private source checkout is not clean at the requested pin")
    os.environ["MACOSX_DEPLOYMENT_TARGET"] = "14.0"
    # Build caches belong exclusively to this producer and exact source revision.
    os.environ.pop("CARGO_TARGET_DIR", None)
    os.environ["CARGO_HOME"] = str(build / "cargo-home")
    os.environ["RUSTUP_AUTO_INSTALL"] = "0"
    os.environ["TMPDIR"] = str(build / "temporary")
    Path(os.environ["TMPDIR"]).mkdir(exist_ok=True)
    subprocess.run(["just", "preflight-source-install"], cwd=source, check=True)
    subprocess.run(["just", "prepare-distribution-lua-dependencies"], cwd=source, check=True)
    subprocess.run(["cargo", "build", "--workspace", "--release", "--locked", "--target", TARGET], cwd=source, check=True)
    stage = build / "distribution"
    if stage.exists():
        shutil.rmtree(stage)
    env = dict(os.environ, NEFOR_LUA_DEPENDENCIES_ROOT=str(source / "target/lua-dependencies"))
    subprocess.run([str(source / "tools/bundle-release.sh"), str(source / f"target/{TARGET}/release"), str(stage)], env=env, check=True)
    # LuaRocks creates symlinks into its own immutable tree. Dereference only
    # internal files; absolute build/store links are never exported.
    for path in sorted(stage.rglob("*")):
        if path.is_symlink():
            resolved = path.resolve()
            if not resolved.is_relative_to(stage.resolve()) or not resolved.is_file():
                raise ValueError(f"unexplained bundle symlink: {path}")
            data, mode = resolved.read_bytes(), resolved.stat().st_mode
            path.unlink()
            path.write_bytes(data)
            path.chmod(mode)
    tree = stage / "share/nefor/runtime/lua-dependencies"
    verify_packages(tree)
    for path in native_files(stage):
        subprocess.run(["codesign", "--force", "--sign", "-", str(path)], check=True)
    minimum = verify_native(stage)
    metadata = dict(schema_version=1, kind="nefor-runtime", runtime_commit=pin,
                    producer_commit=producer, target=TARGET, os="Darwin", arch="arm64",
                    minimum_macos=minimum, lua_abi="5.4",
                    dependencies_lock_sha256=sha(tree / "dependencies.lock"),
                    engine_version=run(str(stage / "bin/nefor"), "--version"),
                    plugins=(stage / "share/nefor/plugins.manifest").read_text().splitlines())
    (stage / "RUNTIME.json").write_text(json.dumps(metadata, indent=2, sort_keys=True) + "\n")
    (stage / "SHA256SUMS").write_text(inventory(stage))
    output.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(stage, output)
    if inventory(output) != (output / "SHA256SUMS").read_text():
        raise ValueError("output inventory differs after transfer")
    print(json.dumps(metadata, indent=2))
    print(f"SHA256SUMS sha256: {sha(output / 'SHA256SUMS')}")


if __name__ == "__main__":
    try:
        if len(sys.argv) != 3:
            raise ValueError("usage: package-runtime.py <full-runtime-pin> <absolute-output-directory>")
        package(sys.argv[1], Path(sys.argv[2]))
    except (ValueError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
