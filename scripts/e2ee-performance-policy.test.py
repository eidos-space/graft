"""Exercise performance policy boundaries without native builds or measurements."""

import copy
import importlib.util
from pathlib import Path
import unittest


spec = importlib.util.spec_from_file_location(
    "benchmark", Path(__file__).with_name("e2ee-load-benchmark.py")
)
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


def summary(profile, operation, scenario="history", scale=4097, p50=100, p95=100):
    return {
        "profile": profile, "operation": operation, "scenario": scenario, "scale": scale,
        "milliseconds": {"p50": p50, "p95": p95},
        "wire_body_bytes": {"p50": 100, "p95": 100},
        "requests": {"p50": 1, "p95": 1},
        "client_rss_peak_sampled_bytes": {"p50": 1024 * 1024, "p95": 1024 * 1024},
    }


def checks_by_name(result):
    return {check["name"]: check for check in result["checks"]}


class PolicyTests(unittest.TestCase):
    def test_bulk_percentile_boundaries_include_fixed_and_multiplier_branches(self):
        for operation in ("initial_push", "cold_clone_sync", "long_offline_push", "long_offline_fetch"):
            for baseline in (0, 100, 250, 375, 1000):
                for percentile, suffix, allowance in (("p50", "wall-ms-p50", 500), ("p95", "wall-ms", 750)):
                    with self.subTest(operation=operation, baseline=baseline, percentile=percentile):
                        median = max(3 * baseline, baseline + 500)
                        tail = max(3 * baseline, baseline + 750)
                        rows = [summary("plain", operation, p50=baseline, p95=baseline),
                                summary("encrypted", operation, p50=median, p95=tail)]
                        name = f"history/4097/{operation}/{suffix}"
                        at_limit = checks_by_name(benchmark.acceptance(rows, []))[name]
                        self.assertEqual(at_limit["maximum"], max(3 * baseline, baseline + allowance))
                        self.assertTrue(at_limit["passed"])
                        rows[1]["milliseconds"][percentile] += 0.001
                        above_limit = checks_by_name(benchmark.acceptance(rows, []))[name]
                        self.assertFalse(above_limit["passed"])

    def test_median_regression_fails_even_when_tail_passes(self):
        result = benchmark.acceptance([
            summary("plain", "long_offline_push"),
            summary("encrypted", "long_offline_push", p50=601, p95=850),
        ], [])
        self.assertFalse(result["passed"])
        failed = [check["name"] for check in result["checks"] if not check["passed"]]
        self.assertEqual(failed, ["history/4097/long_offline_push/wall-ms-p50"])

    def test_policy_is_explicit_and_does_not_mutate_retained_input(self):
        rows = [summary("plain", "long_offline_push"), summary("encrypted", "long_offline_push")]
        before = copy.deepcopy(rows)
        result = benchmark.acceptance(rows, [])
        self.assertEqual(result["acceptance_policy"]["id"], "graft-e2ee-performance-policy-v2")
        self.assertEqual(rows, before)

    def test_warm_small_limits_are_unchanged_and_have_no_median_gate(self):
        for operation in ("warm_small_push", "warm_small_fetch"):
            rows = [summary("plain", operation), summary("encrypted", operation, p50=500, p95=500)]
            encrypted = rows[1]
            encrypted["requests"]["p95"] = 8
            encrypted["wire_body_bytes"]["p95"] = 16384
            result = benchmark.acceptance(rows, [])
            self.assertTrue(result["passed"])
            checks = checks_by_name(result)
            self.assertFalse(any(name.endswith("wall-ms-p50") for name in checks))
            for metric, suffix, limit in (("milliseconds", "wall-ms", 500),
                                          ("requests", "requests", 8),
                                          ("wire_body_bytes", "wire-body", 16384)):
                with self.subTest(operation=operation, metric=metric):
                    changed = copy.deepcopy(rows)
                    changed[1][metric]["p95"] = limit + 1
                    check = checks_by_name(benchmark.acceptance(changed, []))[f"history/4097/{operation}/{suffix}"]
                    self.assertEqual(check["maximum"], limit)
                    self.assertFalse(check["passed"])

    def test_sdk_and_hydration_keep_their_existing_tail_limits(self):
        rows = [summary("plain", "initial_push"), summary("encrypted", "initial_push", p50=600, p95=600)]
        sdk = benchmark.sdk_acceptance(rows)
        self.assertTrue(sdk["passed"])
        self.assertEqual(len(sdk["checks"]), 4)
        self.assertEqual(checks_by_name(sdk)["initial_push/milliseconds"]["maximum"], 600)
        rows[1]["milliseconds"]["p95"] = 600.001
        self.assertFalse(benchmark.sdk_acceptance(rows)["passed"])
        for profile, row in zip(("plain", "encrypted"), rows):
            row["operation"] = f"{profile}/64/first/cold-client/uncached-page"
        hydration = benchmark.hydration_acceptance(rows)
        self.assertEqual(len(hydration["checks"]), 4)
        check = checks_by_name(hydration)["encrypted/64/first/cold-client/uncached-page/milliseconds"]
        self.assertEqual(check["maximum"], 600)
        self.assertFalse(check["passed"])

    def test_complete_matrix_adds_only_eighteen_median_checks(self):
        cases = (
            ("objects", 100000, ("initial_push", "cold_clone_sync", "warm_small_push", "warm_small_fetch")),
            ("history", 4097, ("long_offline_push", "long_offline_fetch", "cold_clone_sync", "warm_small_push", "warm_small_fetch")),
            ("attachment", 7 * 1024 * 1024, ("initial_push", "cold_clone_sync", "warm_small_push", "warm_small_fetch")),
            ("attachment", 32 * 1024 * 1024, ("initial_push", "cold_clone_sync", "warm_small_push", "warm_small_fetch")),
            ("same-ref", 1, ("concurrent_push",)),
            ("different-ref", 1, ("concurrent_push",)),
        )
        rows = [summary(profile, operation, scenario, scale)
                for profile in ("plain", "encrypted")
                for scenario, scale, operations in cases for operation in operations]
        reports = [{"profile": profile, "scenario": scenario, "evidence": {"orphan_pack_bytes": 0}}
                   for profile in ("plain", "encrypted") for scenario in ("same-ref", "different-ref")
                   for _ in range(3)]
        core = benchmark.acceptance(rows, reports)
        checks = checks_by_name(core)
        self.assertTrue(core["passed"])
        self.assertEqual(len(checks), 85)
        self.assertEqual(sum(name.endswith("/wall-ms-p50") for name in checks), 9)
        self.assertEqual(checks["objects/100000/initial_push/wire-body"]["maximum"], 65836)
        self.assertEqual(checks["objects/100000/cold_clone_sync/requests"]["maximum"], 2048)
        self.assertEqual(checks["objects/100000/initial_push/rss-over-plain"]["maximum"], 129 * 1024 * 1024)
        self.assertEqual(checks["attachment/rss-scaling/initial_push"]["maximum"], 16 * 1024 * 1024)
        orphan_checks = [check for check in core["checks"] if check["name"].endswith("/orphans")]
        self.assertEqual(len(orphan_checks), 12)
        for check in orphan_checks:
            self.assertEqual(check["maximum"], 4096 if check["name"].startswith("same-ref/") else 0)
        sdk_rows = [summary(profile, operation) for profile in ("plain", "encrypted")
                    for operation in ("cold_clone", "warm_small_push", "warm_small_pull", "cold_small_push", "cold_small_pull")]
        hydration_rows = [summary(profile, f"{profile}/{chunk}/{position}/{client}/{cache}")
                          for profile in ("plain", "encrypted") for chunk in (16, 64, 256)
                          for position in ("first", "frame-boundary", "tail")
                          for client in ("cold-client", "warm-client") for cache in ("uncached-page", "cached-page")]
        for row in hydration_rows:
            if row["operation"].endswith("/cached-page"):
                row["requests"] = {"p50": 0, "p95": 0}
                row["wire_body_bytes"] = {"p50": 0, "p95": 0}
        sdk = benchmark.sdk_acceptance(sdk_rows)
        hydration = benchmark.hydration_acceptance(hydration_rows)
        self.assertTrue(sdk["passed"])
        self.assertTrue(hydration["passed"])
        self.assertEqual(len(sdk["checks"]), 20)
        self.assertEqual(len(hydration["checks"]), 144)
        both_orders = [core, sdk, hydration] * 2
        self.assertEqual(sum(len(result["checks"]) for result in both_orders), 498)
        self.assertEqual(sum(check["name"].endswith("/wall-ms-p50")
                             for result in both_orders for check in result["checks"]), 18)


if __name__ == "__main__":
    unittest.main()
