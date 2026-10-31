import importlib.machinery
import importlib.util
import unittest
from pathlib import Path

loader = importlib.machinery.SourceFileLoader("benchmark", str(Path(__file__).resolve().parents[1] / "scripts/benchmark"))
spec = importlib.util.spec_from_loader(loader.name, loader)
benchmark = importlib.util.module_from_spec(spec)
loader.exec_module(benchmark)


class ClassificationTest(unittest.TestCase):
    def test_threshold_and_reference_guard(self):
        self.assertEqual(benchmark.classify(299, 300, True), "pass")
        self.assertEqual(benchmark.classify(300, 300, True), "fail")
        self.assertEqual(benchmark.classify(None, 300, True), "unmeasured")
        self.assertEqual(benchmark.classify(1, 300, False), "unmeasured")
        result = {"failures": [], "cleanup_completed": True, "measurements": {"overlap_verified": True,
                  "idle_rss_bytes": 1, "mixed_rss_bytes": 1,
                  "http": {"catalog_http": {"p95_ms": 1}, "warm_reader_http": {"max_ms": 1}}}}
        targets = benchmark.classifications(result)
        self.assertTrue(all(x["fixture result"] == "pass" and x["reference-environment acceptance"] == "unmeasured" for x in targets.values()))
        result["measurements"]["overlap_verified"] = False
        self.assertTrue(all(x["fixture result"] == "unmeasured" for x in benchmark.classifications(result).values()))


if __name__ == "__main__":
    unittest.main()
