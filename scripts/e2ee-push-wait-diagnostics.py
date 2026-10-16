#!/usr/bin/env python3
"""Fixed low-perturbation history diagnostics; no acceptance result or retry."""
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
import sys
import threading
import time


BASE = "7e6b0b538f67a73c27464447dcee4d2f3a1dc619"
FILES = {"scripts/e2ee-push-wait-diagnostics.py", ".github/workflows/e2ee-push-diagnostics.yml"}
PLAN = {"blocks": [{"name": "untraced", "rounds": 3}, {"name": "seccomp", "rounds": 1}],
        "order": ["encrypted", "plain"], "scenario": "history", "scale": 4097,
        "native_processes": 8, "native_operations": 72, "retries": 0,
        "sampling_interval_ms": 20, "build_profile": "release", "features": "native-crypto", "cargo_locked": True}
SCOPE = ("Three untraced encrypted-first history4097 pairs followed by one seccomp-filtered pair. "
         "Existing push phases plus 20 ms best-effort /proc telemetry are diagnostic, not exact phase CPU attribution. "
         "All samples are retained with no retry, no acceptance budget evaluation and no replacement of the 479/480 result.")


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def run(*arguments):
    return subprocess.check_output(arguments, text=True).strip()


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def optional_text(path):
    try:
        return {"text": Path(path).read_text().strip()}
    except (OSError, ValueError) as error:
        return {"unavailable": type(error).__name__}


def status_fields(pid):
    record = optional_text(f"/proc/{pid}/status")
    return dict(line.split(":", 1) for line in record.get("text", "").splitlines() if ":" in line)


def seccomp_exec(directory, binary, profile, minimum_filters):
    status = status_fields("self")
    observed = int(status.get("Seccomp_filters", "-1"))
    verified = status.get("Seccomp", "").strip() == "2" and observed >= minimum_filters
    save(directory / "seccomp-guard.json", {"pid": os.getpid(), "minimum_filters": minimum_filters,
         "observed_filters": observed, "mode": status.get("Seccomp"), "verified": verified,
         "scope": "The benchmark is exec'd only after an additional inherited seccomp filter is visible; no broad-ptrace fallback."})
    if not verified:
        raise SystemExit("seccomp filter not active; benchmark was not started")
    os.execv(str(binary), [str(binary), str(directory / "native.json"), profile, "history", "4097"])


def process_sample(pid):
    record = {"pid": pid, "threads": {}}
    for field in ["io", "schedstat", "cgroup"]:
        record[field] = optional_text(f"/proc/{pid}/{field}")
    try:
        threads = list(Path(f"/proc/{pid}/task").iterdir())
    except OSError as error:
        record["unavailable"] = type(error).__name__
        return record
    for thread in threads:
        record["threads"][thread.name] = {field: optional_text(thread / field) for field in ["stat", "schedstat", "wchan"]}
    children = optional_text(f"/proc/{pid}/task/{pid}/children")
    record["children"] = {}
    for child in children.get("text", "").split():
        record["children"][child] = {field: optional_text(f"/proc/{child}/{field}") for field in ["stat", "schedstat", "io"]}
    return record


def cgroup_path(pid):
    for line in optional_text(f"/proc/{pid}/cgroup").get("text", "").splitlines():
        if line.startswith("0::"):
            return Path("/sys/fs/cgroup") / line[3:].lstrip("/")
    return None


def sample_resources(path, shared, stopped):
    """Counter samples can miss short-lived threads; keep raw times and failures."""
    with path.open("w") as stream:
        while not stopped.is_set():
            began = time.monotonic()
            record = {"unix": time.time(), "monotonic": began,
                      "system": {name: optional_text(f"/proc/{name}") for name in ["stat", "loadavg", "pressure/cpu", "pressure/io", "pressure/memory"]}}
            pid = shared.get("native_pid")
            if pid:
                record["native"] = process_sample(pid)
                group = cgroup_path(pid)
                record["cgroup"] = ({"path": str(group), **{name: optional_text(group / name) for name in ["cpu.stat", "cpu.max", "cpu.pressure", "io.stat", "io.pressure"]}}
                                    if group else {"unavailable": "NoReadableCgroupV2Path"})
            record["sample_duration_ms"] = (time.monotonic() - began) * 1000
            stream.write(json.dumps(record) + "\n")
            stopped.wait(max(0, 0.020 - (time.monotonic() - began)))


def sample_safely(path, shared, stopped):
    try:
        sample_resources(path, shared, stopped)
    except Exception as error:
        shared["resource_sampler_error"] = f"{type(error).__name__}: {error}"


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
         "plan": PLAN, "clock_ticks_per_second": os.sysconf("SC_CLK_TCK"),
         "telemetry_scope": "20 ms target interval, recorded actual intervals. CPU ticks are quantized; schedstat or delay accounting can be disabled. Exited threads and unreadable fields remain missing, never zero-filled. PSI and cgroup counters can include other tasks.",
         "kernel_accounting": {name: optional_text(f"/proc/sys/kernel/{name}") for name in ["sched_schedstats", "task_delayacct"]},
         "baseline_seccomp": status_fields("self"),
         "captured_at_unix": time.time()})


def stop(process):
    # The leader may already have exited while its descendants own our pipes.
    # Always address the owned process group, independent of leader state.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        try:
            os.killpg(process.pid, 0)
        except ProcessLookupError:
            break
        time.sleep(0.05)
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=5)


def capture(stream, raw_path, events_path, shared=None):
    with raw_path.open("w") as raw, events_path.open("w") as events:
        for line in stream:
            received = time.time()
            raw.write(line)
            events.write(json.dumps({"received_unix": received, "line": line.rstrip("\n")}) + "\n")
            if shared is not None:
                try:
                    event = json.loads(line)
                    if event.get("event") == "begin" and isinstance(event.get("pid"), int):
                        shared["native_pid"] = event["pid"]
                except (ValueError, AttributeError):
                    pass


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


def execute_case(binary, output, block, round_number, profile):
    directory = output / f"{block}-{round_number}-{profile}"
    directory.mkdir()
    arguments = [str(binary), str(directory / "native.json"), profile, "history", "4097"]
    if block == "seccomp":
        baseline = int(status_fields("self").get("Seccomp_filters", "-1"))
        if baseline < 0:
            save(directory / "result.json", {"profile": profile, "block": block, "round": round_number,
                 "completed": False, "error": "Seccomp_filters unavailable; benchmark was not started"})
            return read_result(directory)
        arguments = ["strace", "--seccomp-bpf", "-ff", "-ttt", "-T", "-yy", "-s", "160", "-e",
                     "trace=fsync,fdatasync,flock", "-o", str(directory / "syscalls"), sys.executable,
                     str(Path(__file__).resolve()), "seccomp-exec", "--output", str(directory),
                     "--binary", str(binary), "--profile", profile, "--minimum-filters", str(baseline + 1)]
    environment = dict(os.environ)
    environment["GRAFT_PUSH_TRACE"] = "1"
    environment.pop("GRAFT_BENCHMARK_RESOURCE_HANDSHAKE", None)
    result = {"profile": profile, "block": block, "round": round_number, "command": arguments,
              "started_at_unix": time.time(), "completed": False}
    observer_cpu_start = time.process_time()
    shared = {}
    stopped = threading.Event()
    process = subprocess.Popen(arguments, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                               env=environment, start_new_session=True)
    threads = [threading.Thread(target=capture, args=(stream, directory / f"{name}.log", directory / f"{name}.received.jsonl", shared if name == "stdout" else None), daemon=True)
               for name, stream in [("stdout", process.stdout), ("stderr", process.stderr)]]
    sampler = threading.Thread(target=sample_safely, args=(directory / "resources.jsonl", shared, stopped), daemon=True)
    sampler.start()
    for thread in threads:
        thread.start()
    try:
        result["returncode"] = process.wait(timeout=300)
        assert result["returncode"] == 0, f"{profile} diagnostic process failed"
    except BaseException as error:
        result["error"] = f"{type(error).__name__}: {error}"
    finally:
        if "error" in result or process.poll() is None:
            stop(process)
        for thread in threads:
            thread.join(timeout=2)
        if any(thread.is_alive() for thread in threads):
            stop(process)
            for thread in threads:
                thread.join(timeout=2)
        stopped.set()
        sampler.join(timeout=2)
        result["streams_complete"] = all(not thread.is_alive() for thread in threads)
        result["sampler_complete"] = not sampler.is_alive()
        result["observer_cpu_ms"] = (time.process_time() - observer_cpu_start) * 1000
        if "resource_sampler_error" in shared:
            result["error"] = shared["resource_sampler_error"]
        result["finished_at_unix"] = time.time()
    if "error" not in result:
        try:
            native = json.loads((directory / "native.json").read_text())
            assert native["profile"] == profile and native["scenario"] == "history" and native["scale"] == 4097
            assert native["native_crypto_enabled"] and native["build_profile"] == "release"
            assert len(native["samples"]) == 9 and native["evidence"]["all_ids_and_payload_hashes_match"]
            if block == "seccomp":
                assert json.loads((directory / "seccomp-guard.json").read_text())["verified"]
            result["completed"] = result["streams_complete"] and result["sampler_complete"]
            result["operations"] = len(native["samples"])
        except (OSError, ValueError, KeyError, TypeError, AssertionError) as error:
            result["error"] = f"{type(error).__name__}: {error}"
    save(directory / "syscall-summary.json", syscall_summary(directory))
    save(directory / "result.json", result)
    return result


def read_result(directory):
    return json.loads((directory / "result.json").read_text())


def diagnose(output, binary):
    metadata = json.loads((output / "source.json").read_text())
    assert not run("git", "status", "--porcelain"), "source changed after capture"
    for name, digest in metadata["tracked_sha256"].items():
        assert sha(name) == digest, name
    save(output / "binaries.json", {"native": {"path": str(binary), "sha256": sha(binary)},
         "remote_dist_sha256": {str(path): sha(path) for path in sorted(Path("packages/graft-remote/dist").rglob("*")) if path.is_file()}})
    cases = []
    for block in PLAN["blocks"]:
        for round_number in range(1, block["rounds"] + 1):
            for profile in PLAN["order"]:
                cases.append(execute_case(binary, output, block["name"], round_number, profile))
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
    parser.add_argument("action", choices=("source", "run", "finalize", "seccomp-exec"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--profile", choices=("encrypted", "plain"))
    parser.add_argument("--minimum-filters", type=int)
    args = parser.parse_args()
    output = args.output.resolve()
    if args.action == "source":
        source(output)
    elif args.action == "run":
        if args.binary is None:
            parser.error("run requires --binary")
        diagnose(output, args.binary.resolve())
    elif args.action == "seccomp-exec":
        if args.binary is None or args.profile is None or args.minimum_filters is None:
            parser.error("seccomp-exec requires binary, profile and minimum-filters")
        seccomp_exec(output, args.binary.resolve(), args.profile, args.minimum_filters)
    else:
        finalize(output)
