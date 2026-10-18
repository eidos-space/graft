"""Reference budget failures are reports; native failures must still propagate."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("benchmark", Path(__file__).with_name("e2ee-load-benchmark.py"))
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


def native_result(binary, directory, profile, scenario, scale, *args, **kwargs):
    return {"profile": profile, "scenario": scenario, "scale": scale,
            "samples": [{"operation": "initial_push", "milliseconds": 100}]}


class ReportOnlyTests(unittest.TestCase):
    def execute(self, root, extra, native=native_result):
        with patch("sys.argv", ["benchmark", "--output", str(root), "--rounds", "1", "--scenario", "objects", *extra]), \
             patch.object(benchmark, "run", side_effect=native), \
             patch.object(benchmark, "acceptance", return_value={"passed": False, "checks": [{"passed": False}]}):
            benchmark.main()

    def test_reference_failure_is_retained_without_failing_report_only(self):
        with tempfile.TemporaryDirectory() as parent:
            root = Path(parent) / "report"
            self.execute(root, ["--report-only"])
            self.assertFalse(json.loads((root / "acceptance.json").read_text())["passed"])
            self.assertEqual(json.loads((root / "summary.json").read_text())["rounds"], 1)

    def test_explicit_diagnostic_mode_keeps_existing_budget_enforcement(self):
        with tempfile.TemporaryDirectory() as parent:
            with self.assertRaisesRegex(RuntimeError, "acceptance budgets failed"):
                self.execute(Path(parent) / "enforced", [])

    def test_report_only_does_not_swallow_native_failure(self):
        def failed(*args, **kwargs):
            raise RuntimeError("native process failed")
        with tempfile.TemporaryDirectory() as parent:
            with self.assertRaisesRegex(RuntimeError, "native process failed"):
                self.execute(Path(parent) / "failed", ["--report-only"], failed)


if __name__ == "__main__":
    unittest.main()
