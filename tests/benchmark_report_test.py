import importlib.machinery
import importlib.util
import unittest
from pathlib import Path

loader = importlib.machinery.SourceFileLoader("benchmark", str(Path(__file__).resolve().parents[1] / "scripts/benchmark"))
spec = importlib.util.spec_from_loader(loader.name, loader)
benchmark = importlib.util.module_from_spec(spec)
loader.exec_module(benchmark)


class ClassificationTest(unittest.TestCase):
    def test_thresholds_and_guards(self):
        self.assertEqual(benchmark.classify(299, 300, True), "pass")
        self.assertEqual(benchmark.classify(300, 300, True), "fail")
        self.assertEqual(benchmark.classify(None, 300, True), "unmeasured")
        result = {"failures": [], "cleanup_completed": True, "measurements": {
            "overlap_verified": True, "idle_rss_bytes": 1, "idle_settle_seconds": 3,
            "children_accounting": "available; child processes not exercised",
            "mixed_rss_bytes": 1, "corpus": {"files": 50000, "total_bytes": 100},
            "http": {"catalog_http": {"p95_ms": 1, "count": 100},
                     "warm_reader_http": {"max_ms": 1, "count": 100}}}}
        targets = benchmark.classifications(result, "release")
        self.assertTrue(all(x["reference-environment acceptance"] == "unmeasured" for x in targets.values()))
        self.assertEqual(targets["query p95 < 300 ms at five sessions"]["fixture result"], "pass")
        self.assertEqual(targets["mixed RSS < 1 GiB"]["fixture result"], "unmeasured")
        result["measurements"]["http"]["catalog_http"]["p95_ms"] = 300
        self.assertEqual(benchmark.classifications(result, "release")["query p95 < 300 ms at five sessions"]["fixture result"], "fail")
        result["measurements"]["http"]["catalog_http"]["count"] = 99
        self.assertEqual(benchmark.classifications(result, "release")["query p95 < 300 ms at five sessions"]["fixture result"], "unmeasured")
        for profile, files in [("debug", 50000), ("release", 1000)]:
            result["measurements"]["corpus"]["files"] = files
            self.assertTrue(all(x["fixture result"] == "unmeasured" for x in benchmark.classifications(result, profile).values()))
        result["measurements"]["corpus"]["files"] = 50000
        result["measurements"]["children_accounting"] = "unavailable; 0 children counted"
        self.assertEqual(benchmark.classifications(result, "release")["idle RSS < 250 MiB"]["fixture result"], "unmeasured")
        result["measurements"]["children_accounting"] = "available; child processes not exercised"
        result["measurements"]["idle_settle_seconds"] = 0
        self.assertEqual(benchmark.classifications(result, "release")["idle RSS < 250 MiB"]["fixture result"], "unmeasured")
        result["measurements"]["overlap_verified"] = False
        self.assertTrue(all(x["fixture result"] == "unmeasured" for x in benchmark.classifications(result, "release").values()))
