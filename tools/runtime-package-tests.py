"""Offline boundary tests for the exact-pin producer."""
import importlib.util
import json
import os
import sys
from pathlib import Path
import tempfile
import tarfile
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("producer", Path(__file__).with_name("package-release.py"))
producer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(producer)


consumer = None
if len(sys.argv) == 3 and sys.argv[1] == "--consumer":
    consumer_spec = importlib.util.spec_from_file_location("consumer", sys.argv[2])
    consumer = importlib.util.module_from_spec(consumer_spec)
    consumer_spec.loader.exec_module(consumer)
    del sys.argv[1:]


class ProducerTests(unittest.TestCase):
    def test_closed_inventory_includes_metadata_and_rejects_links(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent.parent / "tmp") as directory:
            root = Path(directory)
            (root / "RUNTIME.json").write_text("{}")
            (root / "SHA256SUMS").write_text("excluded")
            before = producer.inventory(root)
            self.assertIn("  RUNTIME.json\n", before)
            self.assertNotIn("SHA256SUMS", before)
            (root / "extra").write_text("extra")
            self.assertNotEqual(before, producer.inventory(root))
            (root / "link").symlink_to("extra")
            with self.assertRaisesRegex(ValueError, "non-regular"):
                producer.inventory(root)

    def test_exact_locked_closure(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent.parent / "tmp") as directory:
            tree = Path(directory)
            (tree / "dependencies.lock").write_text("# lock\na 1-1 all hash https://fixture/a.rock\n")
            (tree / "installed-packages.tsv").write_text("a\t1-1\tinstalled\n")
            producer.verify_packages(tree)
            (tree / "installed-packages.tsv").write_text("a\t2-1\tinstalled\n")
            with self.assertRaisesRegex(ValueError, "closure differs"):
                producer.verify_packages(tree)

    def test_arguments_fail_before_commands(self):
        with patch.object(producer.subprocess, "run") as command, patch.object(producer, "run") as query:
            for target, output, revision in [("bad", "/unused", "HEAD"), (producer.TARGET, "", "HEAD")]:
                with self.assertRaises(ValueError):
                    producer.package(target, output, revision)
            command.assert_not_called()
            query.assert_not_called()

    def test_command_all_native_targets_and_failure_cleanup(self):
        repo = Path(__file__).resolve().parent.parent
        for target, host in producer.PLATFORMS.items():
            for revision in ("HEAD", "v0.4.0", "a" * 40):
                with self.subTest(target=target, revision=revision), tempfile.TemporaryDirectory(dir=repo / "tmp") as directory:
                    root = Path(directory)
                    output = root / "output"
                    calls = []
                    def command(args, **kwargs):
                        calls.append((args, kwargs))
                        if "bundle-release.sh" in args[0]:
                            stage = Path(args[2])
                            (stage / "bin").mkdir(parents=True)
                            (stage / "share/nefor/plugins").mkdir(parents=True)
                            (stage / "bin/nefor").touch()
                        if "archive-release.sh" in args[0]:
                            stage = Path(args[1])
                            (stage / "RUNTIME.json").write_text("{}")
                            stage.with_suffix(".tar.gz").write_bytes(b"fixture")
                    with patch.object(producer, "REPO", repo), patch.object(producer.platform, "system", return_value=host[0]), patch.object(producer.platform, "machine", return_value=host[1]), patch.object(producer, "run", side_effect=["a" * 40, "", "fixture-g" + "a" * 40]), patch.object(producer.subprocess, "run", side_effect=command), patch.dict(producer.os.environ, {"CARGO_TARGET_DIR": str(root / "cache"), "NEFOR_LUA_DEPENDENCIES_ROOT": "/stale"}):
                        producer.package(target, output, revision)
                    names = [args[0] for args, _ in calls]
                    self.assertEqual(names[:3], ["git", str(repo / "tools/prepare-lua-dependencies.sh"), "cargo"])
                    build = calls[2]
                    self.assertEqual(build[0], ["cargo", "build", "--workspace", "--release", "--locked", "--target", target])
                    self.assertEqual(build[1]["env"]["NEFOR_LUA_DEPENDENCIES_ROOT"], str(output / "lua-dependencies"))
                    self.assertEqual(build[1]["env"]["NEFOR_VERSION_OVERRIDE"], "fixture-g" + "a" * 40)
                    self.assertEqual(calls[-1][0][:3], ["git", "worktree", "remove"])
                    self.assertEqual(names.count("codesign"), int(target == producer.TARGET))
                    self.assertFalse(list(output.glob("package-source-*")))

        with tempfile.TemporaryDirectory(dir=repo / "tmp") as directory:
            output = Path(directory) / "output"
            with patch.object(producer.platform, "system", return_value="Darwin"), patch.object(producer.platform, "machine", return_value="arm64"), patch.object(producer, "run", side_effect=["a" * 40, "", "fixture-g" + "a" * 40]), patch.object(producer.subprocess, "run", side_effect=[None, subprocess.CalledProcessError(1, "prepare"), None]) as command:
                with self.assertRaises(subprocess.CalledProcessError):
                    producer.package(producer.TARGET, output)
                self.assertEqual(command.call_args.args[0][:3], ["git", "worktree", "remove"])
                self.assertFalse(list(output.glob("*.tar.gz")))

    def test_stale_output_mismatch_revision_override_dirty(self):
        repo = Path(__file__).resolve().parent.parent
        with tempfile.TemporaryDirectory(dir=repo / "tmp") as directory, patch.object(producer.subprocess, "run") as command:
            root = Path(directory)
            with self.assertRaisesRegex(ValueError, "absent"):
                producer.package(producer.TARGET, root)
            with patch.object(producer.platform, "system", return_value="Other"):
                with self.assertRaisesRegex(ValueError, "native host"):
                    producer.package(producer.TARGET, root / "new")
            with patch.object(producer.platform, "system", return_value="Darwin"), patch.object(producer.platform, "machine", return_value="arm64"):
                with patch.dict(os.environ, {"NEFOR_VERSION_OVERRIDE": "bad"}):
                    with self.assertRaisesRegex(ValueError, "OVERRIDE"):
                        producer.package(producer.TARGET, root / "new")
                for revision in ("", "--help"):
                    with self.assertRaisesRegex(ValueError, "revision"):
                        producer.package(producer.TARGET, root / "new", revision)
                with patch.object(producer, "run", side_effect=subprocess.CalledProcessError(1, "git")):
                    with self.assertRaises(subprocess.CalledProcessError):
                        producer.package(producer.TARGET, root / "new", "missing")
                with patch.object(producer, "run", side_effect=["a" * 40, "dirty"]):
                    with self.assertRaisesRegex(ValueError, "commit producer"):
                        producer.package(producer.TARGET, root / "new")
            command.assert_not_called()
            self.assertFalse((root / "new").exists())

    def test_canonical_bundle_archive_provenance_and_consumer(self):
        repo = Path(__file__).resolve().parent.parent
        with tempfile.TemporaryDirectory(dir=repo / "tmp") as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            # A distinct source revision tests the prepared-input override used locally.
            for name in ("tools", "lua/nefor-pm", "mag", "plugins/example/lua", "examples/nefor-agent"):
                (source / name).mkdir(parents=True)
            plugins = subprocess.check_output([str(repo / "tools/plugin-binaries.sh")], text=True).splitlines()
            script = source / "tools/plugin-binaries.sh"
            script.write_text("#!/bin/sh\nprintf '%s\\n' " + " ".join(plugins) + "\n")
            script.chmod(0o755)
            for name in ("LICENSE", "README.md", "lua/nefor-pm/init.lua", "examples/nefor-agent/init.lua"):
                (source / name).write_text("fixture\n")
            (source / "lua/nefor-pm/dependencies.lock").write_text("a 1-1 all hash https://fixture/a.rock\n")
            subprocess.run(["git", "init", "-q", str(source)], check=True)
            subprocess.run(["git", "-C", str(source), "add", "."], check=True)
            subprocess.run(["git", "-C", str(source), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "Fixture"], check=True)
            pin = producer.run("git", "rev-parse", "HEAD", cwd=source)
            dependencies = root / "dependencies"
            dependencies.mkdir()
            (dependencies / "dependencies.lock").write_text("a 1-1 all hash https://fixture/a.rock\n")
            (dependencies / "installed-packages.tsv").write_text("a\t1-1\tinstalled\n")
            binaries = root / "binaries"
            binaries.mkdir()
            native_target = next(target for target, host in producer.PLATFORMS.items()
                                 if host == (producer.platform.system(), producer.platform.machine()))
            c_source = root / "fixture.c"
            c_source.write_text('#include <stdio.h>\nint main(void) { puts("nefor fixture-' + pin + '"); }\n')
            for name in ["nefor", "mag", *plugins]:
                binary = binaries / name
                subprocess.run(["cc", str(c_source), "-o", str(binary)], check=True)
                if native_target == producer.TARGET:
                    subprocess.run(["codesign", "--force", "--sign", "-", str(binary)], check=True)
            stage = root / f"nefor-{native_target}"
            env = dict(os.environ, NEFOR_RELEASE_SOURCE_ROOT=str(source),
                       NEFOR_LUA_DEPENDENCIES_ROOT=str(dependencies))
            subprocess.run([str(repo / "tools/bundle-release.sh"), str(binaries), str(stage)], env=env, check=True)
            archive_script = repo / "tools/archive-release.sh"
            wrong = root / "nefor-WORK"
            wrong.mkdir()
            self.assertNotEqual(subprocess.run([str(archive_script), str(wrong)], capture_output=True).returncode, 0)
            with patch.dict(os.environ, env):
                lock = stage / "share/nefor/runtime/lua-dependencies/dependencies.lock"
                original = lock.read_bytes()
                lock.write_bytes(original + b"# wrong source lock\n")
                with self.assertRaisesRegex(ValueError, "source dependency lock"):
                    producer.finalize(stage)
                lock.write_bytes(original)
                manifest = stage / "share/nefor/plugins.manifest"
                original_manifest = manifest.read_bytes()
                manifest.write_text("stale-plugin\n")
                with self.assertRaisesRegex(ValueError, "source plugin manifest"):
                    producer.finalize(stage)
                manifest.write_bytes(original_manifest)
                real_run = producer.run
                def stale_stamp(*args, **kwargs):
                    if args[-1] == "--version":
                        return "nefor stale"
                    return real_run(*args, **kwargs)
                with patch.object(producer, "run", side_effect=stale_stamp):
                    with self.assertRaisesRegex(ValueError, "engine stamp"):
                        producer.finalize(stage)
            subprocess.run([str(archive_script), str(stage)], env=env, check=True)
            metadata = json.loads((stage / "RUNTIME.json").read_text())
            self.assertEqual(metadata["runtime_commit"], pin)
            self.assertEqual(metadata["producer_commit"], producer.run("git", "rev-parse", "HEAD", cwd=repo))
            self.assertNotEqual(metadata["producer_commit"], pin)
            self.assertNotIn("prepared_inventory_sha256", metadata)
            self.assertEqual((stage / "SHA256SUMS").read_text(), producer.inventory(stage))
            archive = stage.with_suffix(".tar.gz")
            digest = producer.sha(archive)
            self.assertEqual(Path(str(archive) + ".sha256").read_text(), f"{digest}  {archive.name}\n")
            with tarfile.open(archive) as handle:
                self.assertTrue(all(p.name.split("/")[0] == stage.name for p in handle))
            if consumer is not None:
                extracted = consumer.validate(archive, digest, pin, metadata["target"], root / "validated")
                self.assertEqual((extracted / "RUNTIME.json").read_bytes(), (stage / "RUNTIME.json").read_bytes())
                with self.assertRaisesRegex(ValueError, "provenance"):
                    consumer.validate(archive, digest, "0" * 40, metadata["target"], root / "wrong-pin")
                (stage / "bin/mag").write_text("changed")
                damaged = root / "damaged.tar.gz"
                with tarfile.open(damaged, "w:gz") as handle:
                    handle.add(stage, arcname=stage.name)
                with self.assertRaisesRegex(ValueError, "inventory mismatch"):
                    consumer.validate(damaged, producer.sha(damaged), pin, metadata["target"], root / "damaged")
            self.assertNotEqual(subprocess.run([str(archive_script), str(stage)], capture_output=True).returncode, 0)

    def test_source_version_handles_tagged_and_untagged_revisions(self):
        repo = Path(__file__).resolve().parent.parent
        with tempfile.TemporaryDirectory(dir=repo / "tmp") as directory:
            source = Path(directory)
            subprocess.run(["git", "init", "-q", str(source)], check=True)
            subprocess.run(["git", "-C", str(source), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-qm", "Fixture"], check=True)
            pin = producer.run("git", "rev-parse", "HEAD", cwd=source)
            self.assertEqual(producer.source_version(source, pin), pin)
            subprocess.run(["git", "-C", str(source), "tag", "v0.4.0"], check=True)
            self.assertEqual(producer.source_version(source, pin), "0.4.0-0-g" + pin)
            with self.assertRaisesRegex(ValueError, "resolved commit"):
                producer.source_version(source, "0" * 40)

    def test_release_workflow_uses_same_public_operation(self):
        repo = Path(__file__).resolve().parent.parent
        workflow = (repo / ".github/workflows/release.yml").read_text()
        for target in producer.PLATFORMS:
            self.assertIn("target: " + target, workflow)
        self.assertIn('just package "${{ matrix.target }}" "$PWD/release-output" "$GITHUB_SHA"', workflow)
        for obsolete in ("tools/bundle-release.sh", "tools/archive-release.sh", "cargo build --release", "tools/prepare-lua-dependencies.sh"):
            self.assertNotIn(obsolete, workflow)

    def test_elf_header_rejects_stale_target(self):
        repo = Path(__file__).resolve().parent.parent
        with tempfile.TemporaryDirectory(dir=repo / "tmp") as directory:
            root = Path(directory)
            (root / "share/nefor/plugins").mkdir(parents=True)
            (root / "bin").mkdir()
            header = b"\x7fELF\x02\x01" + b"\0" * 12 + (62).to_bytes(2, "little")
            for name in ("nefor", "mag"):
                (root / "bin" / name).write_bytes(header)
            producer.verify_elf(root, "x86_64-unknown-linux-gnu")
            with self.assertRaisesRegex(ValueError, "ELF architecture"):
                producer.verify_elf(root, "aarch64-unknown-linux-gnu")

    def test_native_absolute_and_missing_dependencies_are_rejected(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent.parent / "tmp") as directory:
            root = Path(directory)
            binary = root / "engine"
            binary.write_text("fixture")
            def command(*args, **kwargs):
                if args[0] == "lipo":
                    return "arm64"
                if args[1] == "-l":
                    return "Load command 0\n cmd LC_BUILD_VERSION\n minos 14.0\n tool 3\n version 1266.8\n"
                if args[1] == "-D":
                    return "engine:"
                return "engine:\n\t" + dependency + " (compatibility version 1.0.0)"
            with patch.object(producer, "native_files", return_value=[binary]), patch.object(producer, "run", side_effect=command):
                for dependency in ("/opt/homebrew/lib/libssl.dylib", "@rpath/libssl.dylib", "@loader_path/missing.dylib"):
                    with self.assertRaises(ValueError):
                        producer.verify_native(root)
                dependency = "/usr/lib/libSystem.B.dylib"
                with patch.object(producer.subprocess, "run"):
                    self.assertEqual(producer.verify_native(root), "14.0")


unittest.main()
