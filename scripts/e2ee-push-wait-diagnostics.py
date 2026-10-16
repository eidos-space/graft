#!/usr/bin/env python3
"""One predefined traced history pair; no acceptance result or sample retry."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import threading
import time


BASE = "7e6b0b538f67a73c27464447dcee4d2f3a1dc619"
FILES = {"scripts/e2ee-push-wait-diagnostics.py", ".github/workflows/e2ee-push-diagnostics.yml"}
SCOPE = ("One encrypted-first history4097 pair with existing push tracing and syscall tracing. "
         "Trace instrumentation changes timing. No acceptance budget is evaluated, no sample is retried, "
         "and these diagnostics cannot replace the retained 479/480 candidate result.")


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def run(*arguments):
    return subprocess.check_output(arguments, text=True).strip()


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def source(output):
    output.mkdir(parents=True, exist_ok=False)
    assert not run("git", "status", "--porcelain"), "source tree must be clean"
    changed = set(run("git", "diff", "--name-only", BASE, "HEAD").splitlines())
    assert changed == FILES, f"unexpected changes from candidate: {changed}"
    tracked = subprocess.check_output(["git", "ls-files", "-z"]).decode().split("\0")
    tools = {}
    for name, arguments in {"rustc": ["rustc", "-vV"], "cargo": ["cargo", "-V"],
                            "node": ["node", "-v"], "pnpm": ["pnpm", "-v"],
                            "strace": ["strace", "-V"], "python3": ["python3", "--version"]}.items():
        executable = Path(run("rustup", "which", name)) if name in ("rustc", "cargo") else Path(shutil.which(name)).resolve()
        tools[name] = {"version": run(*arguments), "executable": str(executable), "sha256": sha(executable)}
    root = Path(os.environ["TMPDIR"]).resolve()
    assert root.stat().st_mode & 0o777 == 0o700
    save(output / "source.json", {"scope": SCOPE, "source_commit": run("git", "rev-parse", "HEAD"),
         "base_candidate": BASE, "changed_files": sorted(changed), "clean": True,
         "tracked_sha256": {name: sha(name) for name in tracked if name}, "tools": tools,
         "kernel": platform.platform(), "cpu": run("lscpu"), "fixture_root": str(root),
         "fixture_mount": json.loads(run("findmnt", "--json", "--target", str(root))),
         "plan": {"rounds": 1, "order": ["encrypted", "plain"], "scenario": "history", "scale": 4097,
                  "native_processes": 2, "native_operations": 18, "retries": 0,
                  "build_profile": "release", "features": "native-crypto", "cargo_locked": True},
         "captured_at_unix": time.time()})


def stop(process):
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        except ProcessLookupError:
            process.wait()


def capture(stream, raw_path, events_path):
    with raw_path.open("w") as raw, events_path.open("w") as events:
        for line in stream:
            received = time.time()
            raw.write(line)
            events.write(json.dumps({"received_unix": received, "line": line.rstrip("\n")}) + "\n")


def syscall_summary(directory):
    totals = {}
    persistence = []
    pattern = re.compile(r"^(\d+\.\d+)\s+(\w+)\(.*<([0-9.]+)>$")
    for path in sorted(directory.glob("syscalls.*")):
        for line in path.read_text(errors="replace").splitlines():
            match = pattern.match(line)
            if not match:
                continue
            started, name, duration = match.groups()
            elapsed = float(duration)
            item = totals.setdefault(name, {"calls": 0, "summed_thread_seconds": 0, "maximum_seconds": 0})
            item["calls"] += 1
            item["summed_thread_seconds"] += elapsed
            item["maximum_seconds"] = max(item["maximum_seconds"], elapsed)
            if name in ("fsync", "fdatasync", "syncfs", "msync", "sync_file_range"):
                persistence.append({"thread_id": path.suffix[1:], "started_unix": float(started),
                                    "seconds": elapsed, "line": line})
    return {"scope": "Syscall elapsed times overlap across threads and include strace perturbation; sums are not process wall time. Raw traces retain unfinished or unparsed calls.",
            "syscalls": totals, "persistence_calls": sorted(persistence, key=lambda item: item["started_unix"])}


def execute_case(binary, output, profile):
    directory = output / profile
    directory.mkdir()
    arguments = ["strace", "-ff", "-ttt", "-T", "-yy", "-s", "160", "-e",
                 "trace=%process,fsync,fdatasync,syncfs,sync_file_range,msync,flock,poll,ppoll,select,pselect6,epoll_wait,epoll_pwait,connect,accept,accept4,shutdown,futex",
                 "-o", str(directory / "syscalls"), str(binary), str(directory / "native.json"), profile, "history", "4097"]
    environment = dict(os.environ)
    environment["GRAFT_PUSH_TRACE"] = "1"
    environment.pop("GRAFT_BENCHMARK_RESOURCE_HANDSHAKE", None)
    result = {"profile": profile, "command": arguments, "started_at_unix": time.time(), "completed": False}
    process = subprocess.Popen(arguments, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                               env=environment, start_new_session=True)
    threads = [threading.Thread(target=capture, args=(stream, directory / f"{name}.log", directory / f"{name}.received.jsonl"))
               for name, stream in [("stdout", process.stdout), ("stderr", process.stderr)]]
    for thread in threads:
        thread.start()
    try:
        result["returncode"] = process.wait(timeout=300)
        assert result["returncode"] == 0, f"{profile} diagnostic process failed"
    except BaseException as error:
        result["error"] = f"{type(error).__name__}: {error}"
    finally:
        stop(process)
        for thread in threads:
            thread.join(timeout=10)
        result["streams_complete"] = all(not thread.is_alive() for thread in threads)
        result["finished_at_unix"] = time.time()
    if "error" not in result:
        native = json.loads((directory / "native.json").read_text())
        assert native["profile"] == profile and native["scenario"] == "history" and native["scale"] == 4097
        assert native["native_crypto_enabled"] and native["build_profile"] == "release"
        assert len(native["samples"]) == 9 and native["evidence"]["all_ids_and_payload_hashes_match"]
        result["completed"] = result["streams_complete"]
        result["operations"] = len(native["samples"])
    save(directory / "syscall-summary.json", syscall_summary(directory))
    save(directory / "result.json", result)
    return result


def diagnose(output, binary):
    metadata = json.loads((output / "source.json").read_text())
    assert not run("git", "status", "--porcelain"), "source changed after capture"
    for name, digest in metadata["tracked_sha256"].items():
        assert sha(name) == digest, name
    save(output / "binaries.json", {"native": {"path": str(binary), "sha256": sha(binary)},
         "remote_dist_sha256": {str(path): sha(path) for path in sorted(Path("packages/graft-remote/dist").rglob("*")) if path.is_file()}})
    cases = []
    for profile in ["encrypted", "plain"]:
        cases.append(execute_case(binary, output, profile))
    report = {"scope": SCOPE, "base_candidate": BASE, "source_commit": metadata["source_commit"],
              "diagnostic_completed": all(case["completed"] for case in cases), "cases": cases,
              "acceptance_evaluated": False, "performance_pass_claimed": False,
              "original_failure_replaced": False, "original_failure_explained": False}
    save(output / "result.json", report)
    print(json.dumps(report, indent=2))
    if not report["diagnostic_completed"]:
        raise SystemExit(1)


def finalize(output):
    output.mkdir(parents=True, exist_ok=True)
    save(output / "sha256.json", {str(path.relative_to(output)): sha(path)
         for path in sorted(output.rglob("*")) if path.is_file() and path.name != "sha256.json"})


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("source", "run", "finalize"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--binary", type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    if args.action == "source":
        source(output)
    elif args.action == "run":
        if args.binary is None:
            parser.error("run requires --binary")
        diagnose(output, args.binary.resolve())
    else:
        finalize(output)
