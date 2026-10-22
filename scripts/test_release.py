"""Packaging contract tests; no cargo build, network, or real executable needed."""
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock
import zipfile

import release


class ReleaseTests(unittest.TestCase):
    def test_build_invokes_only_selected_tui_binary(self):
        with tempfile.TemporaryDirectory() as temp:
            target = "x86_64-unknown-linux-gnu"
            with mock.patch.object(release, "verify_dependencies") as verify, \
                    mock.patch.object(release.subprocess, "run") as run, \
                    mock.patch.object(release.subprocess, "check_output", return_value=f"qaqh-tui {release.version()}\n"), \
                    mock.patch.object(release, "package_binary", return_value=Path(temp) / "archive") as package:
                release.build(target, release.version(), Path(temp))
                verify.assert_called_once_with(release.ROOT)
                run.assert_called_once_with([
                    "cargo", "build", "--locked", "--release", "--bin", "qaqh-tui", "--target", target,
                ], cwd=release.ROOT, check=True)
                self.assertEqual(package.call_args.args[0].name, "qaqh-tui")

    def test_tag_must_match_version_without_prefix(self):
        release.validate_tag("2.0.0", "2.0.0")
        for invalid in ("v2.0.0", "2.0.1", "2.0.0-beta.2", "../../2.0.0", "2.0.0\n"):
            with self.assertRaises(ValueError):
                release.validate_tag(invalid, "2.0.0")

    def test_pinned_dependencies_and_metadata(self):
        config = release.dependencies()
        self.assertEqual(len(config["backend"]["revision"]), 40)
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "github-output"
            release.export_metadata(release.ROOT, output)
            lines = output.read_text(encoding="utf-8").splitlines()
            self.assertIn(f"version={release.version()}", lines)
            self.assertIn(f"backend_revision={config['backend']['revision']}", lines)

    def test_exactly_four_packages_only_contain_tui_licenses_and_build_info(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary = root / "input-executable"
            binary.write_bytes(b"fake TUI executable for packaging tests\x00")
            out = root / "dist"
            for target, (label, suffix, extension) in release.TARGETS.items():
                archive = release.package_binary(binary, target, release.version(), out)
                stem = f"qaqh-tui-{release.version()}-{label}"
                expected = {f"{stem}/{name}" for name in (
                    f"qaqh-tui{suffix}", "LICENSE-MIT", "LICENSE-APACHE", "BUILD-INFO.json",
                )}
                if extension == ".zip":
                    with zipfile.ZipFile(archive) as file:
                        self.assertEqual(set(file.namelist()), expected)
                        payload = file.read(f"{stem}/qaqh-tui{suffix}")
                        info = json.loads(file.read(f"{stem}/BUILD-INFO.json"))
                else:
                    with tarfile.open(archive) as file:
                        self.assertEqual(set(file.getnames()), expected)
                        self.assertEqual(file.getmember(f"{stem}/qaqh-tui").mode, 0o755)
                        payload = file.extractfile(f"{stem}/qaqh-tui").read()
                        info = json.load(file.extractfile(f"{stem}/BUILD-INFO.json"))
                self.assertEqual(payload, binary.read_bytes())
                self.assertEqual(info["target"], target)
                before = archive.read_bytes()
                release.package_binary(binary, target, release.version(), out)
                self.assertEqual(before, archive.read_bytes(), "packaging should be deterministic")
            sums = release.collect(out, release.version()).read_text(encoding="ascii")
            self.assertEqual(len(sums.splitlines()), 4)
            victim = next(out.glob("*.zip"))
            victim.write_bytes(b"corrupted")
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                release.collect(out, release.version())

    def test_missing_binary_and_incomplete_matrix_are_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            with self.assertRaises(ValueError):
                release.package_binary(root / "missing", next(iter(release.TARGETS)), release.version(), root)
            with self.assertRaisesRegex(ValueError, "exactly four"):
                release.collect(root, release.version())


if __name__ == "__main__":
    unittest.main()
