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

    def test_pin_and_output_admission_precede_build(self):
        with patch.object(producer.subprocess, "run") as command:
            for pin, output in [("main", Path("/unused")), ("a" * 40, Path("relative"))]:
                with self.assertRaises(ValueError):
                    producer.package(pin, output, Path("/unused"), "a" * 64)
            command.assert_not_called()

    def test_prepared_admission_fails_before_execution(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent.parent / "tmp") as directory:
            root = Path(directory)
            (root / "RUNTIME.json").write_text('{"runtime_commit": "' + 'a' * 40 + '", "target": "x86_64-unknown-linux-gnu"}')
            (root / "SHA256SUMS").write_text(producer.inventory(root))
            digest = producer.sha(root / "SHA256SUMS")
            with patch.object(producer, "run") as command:
                with self.assertRaisesRegex(ValueError, "digest differs"):
                    producer.admit(root, "a" * 40, "b" * 64)
                (root / "extra").write_text("extra")
                with self.assertRaisesRegex(ValueError, "inventory is not complete"):
                    producer.admit(root, "a" * 40, digest)
                (root / "extra").unlink()
                with self.assertRaisesRegex(ValueError, "source pin or target"):
                    producer.admit(root, "a" * 40, digest)
                command.assert_not_called()

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
            for name in ["nefor", "mag", *plugins]:
                binary = binaries / name
                binary.write_text(f"#!/bin/sh\necho 'nefor fixture-{pin}'\n")
                binary.chmod(0o755)
            stage = root / "nefor-x86_64-unknown-linux-gnu"
            env = dict(os.environ, NEFOR_RELEASE_SOURCE_ROOT=str(source),
                       NEFOR_LUA_DEPENDENCIES_ROOT=str(dependencies))
            subprocess.run([str(repo / "tools/bundle-release.sh"), str(binaries), str(stage)], env=env, check=True)
            archive_script = repo / "tools/archive-release.sh"
            wrong = root / "nefor-WORK"
            wrong.mkdir()
            self.assertNotEqual(subprocess.run([str(archive_script), str(wrong)], capture_output=True).returncode, 0)
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

    def test_finalize_native_and_prepared_evidence(self):
        repo = Path(__file__).resolve().parent.parent
        with tempfile.TemporaryDirectory(dir=repo / "tmp") as directory:
            stage = Path(directory) / "nefor-aarch64-apple-darwin"
            tree = stage / "share/nefor/runtime/lua-dependencies"
            tree.mkdir(parents=True)
            (tree / "dependencies.lock").write_text("a 1-1 all hash url\n")
            (tree / "installed-packages.tsv").write_text("a\t1-1\tinstalled\n")
            (stage / "share/nefor/plugins.manifest").write_text("plugin\n")
            with patch.object(producer, "run", side_effect=["a" * 40, "b" * 40, "nefor fixture"]), patch.object(producer, "verify_native", return_value="14.0"), patch.dict(os.environ, {"NEFOR_RELEASE_PREPARED_INVENTORY_SHA256": "c" * 64}):
                metadata = producer.finalize(stage)
            self.assertEqual(metadata["minimum_macos"], "14.0")
            self.assertEqual(metadata["prepared_inventory_sha256"], "c" * 64)
            self.assertEqual(metadata["runtime_commit"], "a" * 40)
            self.assertEqual(metadata["producer_commit"], "b" * 40)
            self.assertEqual((stage / "SHA256SUMS").read_text(), producer.inventory(stage))

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
