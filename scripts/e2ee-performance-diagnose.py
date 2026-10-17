"""Trace failed native processes separately; diagnostic replays are never acceptance samples."""

import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess


CORE_GROUPS = ("core-plain-first", "core-encrypted-first")
CASE = re.compile(r"(objects|history|attachment|same-ref|different-ref)-(\d+)-(plain|encrypted)-(\d+)")


def trace(command, stdout, stderr, environment):
    with subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=stdout,
                          stderr=stderr, env=environment, start_new_session=True) as process:
        try:
            return process.wait(timeout=900)
        except subprocess.TimeoutExpired:
            # The native benchmark also starts its loopback Node service. Kill
            # the entire isolated diagnostic group before collecting artifacts.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
            raise


def failed_cases(output):
    for group in CORE_GROUPS:
        for stderr in sorted((output / group).glob("*.stderr.log")):
            stem = stderr.name.removesuffix(".stderr.log")
            match = CASE.fullmatch(stem)
            # A completed native process has a JSON report, even if a budget
            # later fails. Replay only missing reports with an actual error.
            if match and stderr.stat().st_size and not (stderr.parent / f"{stem}.json").exists():
                scenario, scale, profile, _round = match.groups()
                yield group, stem, scenario, scale, profile


def diagnose(output, binary):
    directory = output / "diagnostics"
    directory.mkdir(exist_ok=True)
    result = {"scope": "One traced replay per failed native process; excluded from acceptance.", "cases": []}
    environment = {key: value for key, value in os.environ.items()
                   if key != "GRAFT_BENCHMARK_RESOURCE_HANDSHAKE"}
    for group, stem, scenario, scale, profile in failed_cases(output):
        case = directory / f"{group}-{stem}"
        case.mkdir(exist_ok=True)
        command = ["strace", "--follow-forks", "--decode-fds=path", "--stack-trace",
                   "--trace=flock", "--output", str(case / "flock.log"),
                   str(binary), str(case / "native.json"), profile, scenario, scale]
        record = {"original_group": group, "original_case": stem, "command": command}
        with (case / "stdout.log").open("w") as stdout, (case / "stderr.log").open("w") as stderr:
            try:
                record["returncode"] = trace(command, stdout, stderr, environment)
            except (OSError, subprocess.TimeoutExpired) as error:
                record["diagnostic_error"] = str(error)
        result["cases"].append(record)
    (directory / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--binary", required=True, type=Path)
    args = parser.parse_args()
    diagnose(args.output.resolve(), args.binary.resolve())
