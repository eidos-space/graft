"""Fail-closed checks for retained performance evidence, without a Rust build."""

import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "evidence", Path(__file__).with_name("e2ee-performance-evidence.py")
)
evidence = importlib.util.module_from_spec(spec)
spec.loader.exec_module(evidence)


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.output = Path(self.temporary.name)
        self.source = {"commit": "candidate-sha", "tree": "tree-sha"}
        evidence.write_json(self.output / "source.json", self.source)
        self.binaries = {
            "source_commit": self.source["commit"],
            "source_manifest_sha256": evidence.sha256(self.output / "source.json"),
            "binaries": {kind: {"sha256": f"{kind}-binary"} for kind in evidence.BINARIES},
        }
        evidence.write_json(self.output / "binaries.json", self.binaries)
        for name, expected in evidence.GROUPS.items():
            directory = self.output / name
            directory.mkdir()
            rows = [{"profile": "plain" if i % 2 else "encrypted", "samples": expected["rounds"]}
                    for i in range(expected["summaries"])]
            evidence.write_json(directory / "summary.json", {
                "order": expected["order"], "rounds": expected["rounds"], "summaries": rows,
            })
            evidence.write_json(directory / "acceptance.json", {
                "passed": True,
                "checks": [{"name": "retained-budget", "actual": 1, "maximum": 2, "passed": True}],
            })
            common = {
                "native_crypto_enabled": True,
                "build_profile": "release",
                "resource_measurement": {"binary_sha256": f"{expected['kind']}-binary"},
            }
            if expected["kind"] == "core":
                index = 0
                for profile in ("plain", "encrypted"):
                    for scenario, scale, count in (
                        ("objects", 100000, 8), ("history", 4097, 9),
                        ("attachment", 7 * 1024 * 1024, 8),
                        ("attachment", 32 * 1024 * 1024, 8),
                        ("same-ref", 1, 1), ("different-ref", 1, 1),
                    ):
                        for _ in range(expected["rounds"]):
                            evidence.write_json(directory / f"raw-{index}.json", {
                                **common, "profile": profile, "scenario": scenario,
                                "scale": scale, "samples": [{} for _ in range(count)],
                            })
                            index += 1
            else:
                evidence.write_json(directory / "raw-0.json", {
                    **common, "samples": [{} for _ in range(expected["operations"])],
                })

    def finalize(self):
        with patch.object(evidence, "require_unchanged_source", return_value=self.source), \
                patch.dict(evidence.os.environ, {"GITHUB_STEP_SUMMARY": ""}), \
                contextlib.redirect_stdout(io.StringIO()):
            passed = evidence.finalize(self.output)
        result = json.loads((self.output / "result.json").read_text())
        return passed, result

    def mutate(self, relative, update):
        path = self.output / relative
        value = json.loads(path.read_text())
        update(value)
        evidence.write_json(path, value)

    def test_complete_evidence_binds_all_six_groups_and_raw_hashes(self):
        passed, result = self.finalize()
        self.assertTrue(passed)
        self.assertEqual(result["raw_reports"], 76)
        self.assertEqual(result["measured_operations"], 1240)
        self.assertEqual(len(result["groups"]), 6)
        hashes = json.loads((self.output / "sha256.json").read_text())
        self.assertEqual(hashes["result.json"], evidence.sha256(self.output / "result.json"))
        self.assertIn("core-plain-first/raw-0.json", hashes)

    def test_missing_group_fails_without_dropping_other_results(self):
        (self.output / "sdk-plain-first/summary.json").unlink()
        passed, result = self.finalize()
        self.assertFalse(passed)
        self.assertFalse(result["groups"]["sdk-plain-first"]["complete"])
        self.assertTrue(result["groups"]["hydration-encrypted-first"]["passed"])

    def test_empty_acceptance_cannot_vacuously_pass(self):
        self.mutate("sdk-plain-first/acceptance.json", lambda value: value.update(checks=[]))
        passed, result = self.finalize()
        self.assertFalse(passed)
        self.assertIn("missing", result["groups"]["sdk-plain-first"]["error"])

    def test_budget_failure_is_complete_and_retained(self):
        failure = {"name": "wall-ms", "actual": 943, "maximum": 576, "passed": False}
        self.mutate("core-plain-first/acceptance.json",
                    lambda value: value.update(passed=False, checks=[failure]))
        passed, result = self.finalize()
        self.assertFalse(passed)
        group = result["groups"]["core-plain-first"]
        self.assertTrue(group["complete"])
        self.assertEqual(group["failed_checks"], [failure])
        self.assertTrue(result["groups"]["core-encrypted-first"]["passed"])

    def test_binary_mismatch_fails(self):
        self.mutate("hydration-plain-first/raw-0.json",
                    lambda value: value["resource_measurement"].update(binary_sha256="different"))
        passed, result = self.finalize()
        self.assertFalse(passed)
        self.assertIn("executable differs", result["groups"]["hydration-plain-first"]["error"])

    def test_reduced_scale_does_not_count_as_full_core_coverage(self):
        self.mutate("core-plain-first/raw-0.json", lambda value: value.update(scale=100))
        passed, result = self.finalize()
        self.assertFalse(passed)
        self.assertIn("full-scale", result["groups"]["core-plain-first"]["error"])

    def test_changed_manifest_fails_even_if_all_groups_pass(self):
        self.mutate("binaries.json", lambda value: value.update(source_manifest_sha256="old-source"))
        passed, result = self.finalize()
        self.assertFalse(passed)
        self.assertTrue(result["errors"])

    def test_source_hash_change_is_rejected(self):
        source = {**self.source, "tracked_sha256": {"Cargo.lock": "old"}}
        evidence.write_json(self.output / "source.json", source)
        current = copy.deepcopy(source)
        current["tracked_sha256"]["Cargo.lock"] = "new"
        with patch.object(evidence, "source_identity", return_value=current):
            with self.assertRaisesRegex(RuntimeError, "tracked_sha256"):
                evidence.require_unchanged_source(self.output)


if __name__ == "__main__":
    unittest.main()
