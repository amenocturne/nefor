"""Offline boundary tests for the exact-pin producer."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("producer", Path(__file__).with_name("package-runtime.py"))
producer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(producer)


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
                    producer.package(pin, output)
            command.assert_not_called()

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
