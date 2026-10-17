"""Diagnostic selection must not rewrite or count original benchmark samples."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "diagnose", Path(__file__).with_name("e2ee-performance-diagnose.py")
)
diagnose = importlib.util.module_from_spec(spec)
spec.loader.exec_module(diagnose)


class DiagnosticTests(unittest.TestCase):
    def test_only_failed_core_native_processes_are_selected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for group in (*diagnose.CORE_GROUPS, "sdk-plain-first", "diagnostics"):
                directory = root / group
                directory.mkdir()
                (directory / "objects-100000-plain-3.stderr.log").write_text("Error: MutationBusy\n")
                (directory / "objects-100000-plain-2.stderr.log").write_text("repository identity\n")
                (directory / "objects-100000-plain-2.json").write_text("{}\n")
                (directory / "history-4097-encrypted-1.stderr.log").touch()
                (directory / "unrelated.stderr.log").write_text("failure")
            selected = list(diagnose.failed_cases(root))
            self.assertEqual(len(selected), 2)
            self.assertEqual({case[0] for case in selected}, set(diagnose.CORE_GROUPS))
            self.assertTrue(all(case[1:] == ("objects-100000-plain-3", "objects", "100000", "plain")
                                for case in selected))

    def test_replay_is_separate_and_cannot_replace_original_failed_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            group = root / "core-encrypted-first"
            group.mkdir()
            failure = group / "objects-100000-plain-3.stderr.log"
            failure.write_text("Error: MutationBusy\n")
            with patch.object(diagnose, "trace", return_value=0) as trace, \
                    patch.dict(diagnose.os.environ, {"GRAFT_BENCHMARK_RESOURCE_HANDSHAKE": "1"}), \
                    contextlib.redirect_stdout(io.StringIO()):
                diagnose.diagnose(root, Path("/candidate/core"))
            self.assertEqual(trace.call_count, 1)
            arguments, _options = trace.call_args
            self.assertEqual(arguments[0][-3:], ["plain", "objects", "100000"])
            self.assertTrue(Path(arguments[0][-4]).is_relative_to(root / "diagnostics"))
            self.assertNotIn("GRAFT_BENCHMARK_RESOURCE_HANDSHAKE", arguments[3])
            self.assertEqual(failure.read_text(), "Error: MutationBusy\n")
            self.assertFalse((group / "objects-100000-plain-3.json").exists())
            result = json.loads((root / "diagnostics/result.json").read_text())
            self.assertEqual(result["cases"][0]["returncode"], 0)

    def test_timeout_kills_and_reaps_the_entire_diagnostic_process_group(self):
        with patch.object(diagnose.subprocess, "Popen") as popen, \
                patch.object(diagnose.os, "killpg") as killpg:
            process = popen.return_value.__enter__.return_value
            process.pid = 1234
            process.wait.side_effect = [diagnose.subprocess.TimeoutExpired(["strace"], 900), -9]
            with self.assertRaises(diagnose.subprocess.TimeoutExpired):
                diagnose.trace(["strace"], io.StringIO(), io.StringIO(), {})
            self.assertTrue(popen.call_args.kwargs["start_new_session"])
            killpg.assert_called_once_with(1234, diagnose.signal.SIGKILL)
            self.assertEqual(process.wait.call_count, 2)
            self.assertEqual(process.wait.call_args.kwargs, {})


if __name__ == "__main__":
    unittest.main()
