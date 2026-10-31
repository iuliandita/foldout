#!/usr/bin/env python3
import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import mock_open, patch

spec = importlib.util.spec_from_file_location("validate_release", Path(__file__).with_name("validate-release.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ValidateReleaseTest(unittest.TestCase):
    def test_tags_versions_and_ancestry(self):
        cases = [
            ("v0.1.0", "0.1.0", 0, ("develop", True)),
            ("v1.2.3", "1.2.3", 0, ("main", False)),
            ("v1.2.3-rc.1", "1.2.3-rc.1", 0, ("main", True)),
            ("v01.2.3", "01.2.3", 0, None),
            ("v1.2.3-rc.01", "1.2.3-rc.01", 0, None),
            ("v1.2.3+meta", "1.2.3+meta", 0, None),
            ("v1.2.3", "1.2.4", 0, None),
            ("v0.1.0", "0.1.0", 1, None),
        ]
        for tag, version, ancestry, expected in cases:
            with self.subTest(tag=tag, version=version, ancestry=ancestry):
                with patch.object(release.Path, "open", mock_open()), \
                     patch.object(release.tomllib, "load", return_value={"package": {"version": version}}), \
                     patch.object(release.subprocess, "check_output", return_value="abc\n"), \
                     patch.object(release.subprocess, "run") as run:
                    run.return_value.returncode = ancestry
                    if expected:
                        self.assertEqual(release.validate(tag, "abc"), expected)
                        self.assertIn(f"refs/remotes/origin/{expected[0]}", run.call_args.args[0])
                    else:
                        with self.assertRaises(ValueError):
                            release.validate(tag, "abc")

    def test_tag_must_resolve_to_checked_out_commit(self):
        with patch.object(release.Path, "open", mock_open()), \
             patch.object(release.tomllib, "load", return_value={"package": {"version": "0.1.0"}}), \
             patch.object(release.subprocess, "check_output", return_value="other\n"):
            with self.assertRaisesRegex(ValueError, "checked-out commit"):
                release.validate("v0.1.0", "abc")

    def test_non_tag_ref_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run(
                [sys.executable, "-B", str(Path(__file__).resolve().with_name("validate-release.py"))],
                cwd=directory,
                env={**os.environ, "GITHUB_REF_TYPE": "branch"},
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("select a tag for this workflow", result.stderr)


if __name__ == "__main__":
    unittest.main()
