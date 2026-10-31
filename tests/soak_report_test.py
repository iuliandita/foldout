import importlib.machinery
import importlib.util
import json
import subprocess
import sys
from pathlib import Path
import tempfile
import unittest

loader = importlib.machinery.SourceFileLoader("soak", str(Path(__file__).resolve().parents[1] / "scripts/soak"))
spec = importlib.util.spec_from_loader(loader.name, loader)
soak = importlib.util.module_from_spec(spec)
loader.exec_module(soak)


def evidence():
    return {"schema_version": 1, "fixture_class": "owned loopback",
            "artifact": {"git_commit": "a"*40, "binary_sha256": "b"*64,
                         "dirty_tree": False, "build_profile": "debug; soak-fixtures"},
            "configured_urls": ["http://127.0.0.1:8787", "http://127.0.0.1:8788"],
            "fixture_requests": [{"method": "GET", "path": "/owned.cbz", "mode": None,
                                  "range": "bytes=274-", "if_range": '"owned"'}],
            "client_submissions": 1,
            "window": {"started_at": "2026-10-31T00:00:00+00:00", "ended_at": "2026-10-31T00:03:00+00:00",
                       "requested_seconds": 180, "elapsed_seconds": 180},
            "checks": dict.fromkeys(soak.REQUIRED, True), "cycles_completed": 1,
            "transitions": [{"at_seconds": 1, "kind": "cycle", "id": "1", "state": "completed"}],
            "originals_before": {"fixture": "a"*64}, "originals_after": {"fixture": "a"*64},
            "invariants": {"original_hashes_unchanged": True, "loopback_configuration": True},
            "failures": [], "cleanup_completed": True}


class ClassificationTest(unittest.TestCase):
    def test_all_pass_and_bounded_observation(self):
        report = evidence()
        self.assertEqual(soak.classifications(report), {"fixture result": "pass",
            "multi-day observation": "unmeasured", "real-household acceptance": "unmeasured"})
        report["window"].update(requested_seconds=172800, elapsed_seconds=172800)
        self.assertEqual(soak.classifications(report)["multi-day observation"], "pass")
        report["window"]["elapsed_seconds"] = 172799
        self.assertEqual(soak.classifications(report)["fixture result"], "unmeasured")
        report["checks"]["import_recovery"] = False
        self.assertEqual(soak.classifications(report)["fixture result"], "unmeasured")

    def test_invariant_violations_fail(self):
        for field, value in [("failures", ["unexpected submission"]), ("cleanup_completed", False),
                             ("invariants", {"original_hashes_unchanged": False, "loopback_configuration": True}),
                             ("client_submissions", 2),
                             ("originals_after", {"fixture": "b"*64})]:
            with self.subTest(field=field):
                report = evidence()
                report[field] = value
                self.assertEqual(soak.classifications(report)["fixture result"], "fail")

    def test_household_claims_cannot_promote_acceptance(self):
        report = evidence()
        for claim in [True, "pass", {"authorization": "asserted", "observation": "72h", "result": "pass"}]:
            report["real_household"] = claim
            report["real-household acceptance"] = "pass"
            self.assertEqual(soak.classifications(report)["real-household acceptance"], "unmeasured")

    def test_malformed_or_missing_evidence_invalid(self):
        examples = [None, {}, [], "pass"]
        for key in evidence():
            value = evidence()
            del value[key]
            examples.append(value)
        for field, value in [("checks", {"direct_resume": True}), ("failures", None),
                             ("transitions", []), ("transitions", [None]),
                             ("originals_before", {}), ("originals_after", {"fixture": "x"*64}),
                             ("cycles_completed", True), ("cleanup_completed", "yes")]:
            report = evidence()
            report[field] = value
            examples.append(report)
        for value in [float("nan"), float("inf"), -1, True, "180"]:
            report = evidence()
            report["window"]["elapsed_seconds"] = value
            examples.append(report)
        template = Path(__file__).resolve().parents[1] / "docs/soak-report-template.json"
        examples.append(json.loads(template.read_text()))
        for report in examples:
            with self.subTest(report=report):
                self.assertEqual(soak.classifications(report)["fixture result"], "invalid")

    def test_safety_guards_refuse_external_origins_and_existing_state(self):
        for value in ["http://example.invalid:80", "http://192.0.2.1:8787", "http://localhost:8787",
                      "https://127.0.0.1:8787", "http://127.0.0.1:8787/path", "http://user@127.0.0.1:8787",
                      "http://127.0.0.1:8787?x=y"]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                soak.loopback(value)
        self.assertEqual(soak.loopback("http://127.0.0.1:8787/"), "http://127.0.0.1:8787")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            binary = path / "fixture-binary"
            binary.write_text("#!/bin/sh\nprintf '%s\\n' 'libraryd [soak-fixtures debug]'\n")
            binary.chmod(0o700)
            command = [sys.executable, "-I", "-B", str(Path(soak.__file__)), "--binary", str(binary)]
            for candidate in [path, path / "dangling"]:
                if candidate != path:
                    candidate.symlink_to(path / "absent")
                result = subprocess.run(command + ["--state-dir", str(candidate)], capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("File exists", result.stderr)
            result = subprocess.run(command + ["--state-dir", str(path / "new"),
                                               "--listen-url", "http://192.0.2.1:8787"],
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("loopback", result.stderr)
            self.assertFalse((path / "new").exists())
            binary.write_text("#!/bin/sh\nprintf '%s\\n' 'libraryd ordinary debug'\n")
            result = subprocess.run(command + ["--state-dir", str(path / "new")], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("binary lacks soak-fixtures", result.stderr)
            self.assertFalse((path / "new").exists())
