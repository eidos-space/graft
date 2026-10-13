#!/usr/bin/env python3
"""Run isolated native load processes and retain raw, sampled resource evidence."""
import argparse
import collections
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import subprocess
import sys
import threading
import time


def process_sample(pid):
    result = subprocess.run(["ps", "-o", "rss=,time=", "-p", str(pid)], capture_output=True, text=True)
    if result.returncode or not result.stdout.strip():
        return None
    memory, cpu = result.stdout.strip().split()
    days = 0
    if "-" in cpu:
        day, cpu = cpu.split("-")
        days = int(day)
    seconds = 0.0
    for part in cpu.split(":"):
        seconds = seconds * 60 + float(part)
    return {"rss_bytes": int(memory) * 1024, "cpu_seconds": seconds + days * 86400}


def run(binary, directory, profile, scenario, scale, round_number, interval, paired=False):
    started = datetime.datetime.now(datetime.timezone.utc).isoformat()
    with binary.open("rb") as executable:
        binary_sha256 = hashlib.file_digest(executable, "sha256").hexdigest()
    stem = f"{scenario}-{scale}-{profile}-{round_number}"
    report = directory / f"{stem}.json"
    with (directory / f"{stem}.stderr.log").open("w") as stderr:
        command = [str(binary), str(report), str(scale), profile] if paired else [str(binary), str(report), profile, scenario, str(scale)]
        process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True,
                                   env={**os.environ, "GRAFT_BENCHMARK_RESOURCE_HANDSHAKE": "1"})
        stop = threading.Event()
        lock = threading.Lock()
        sampling_lock = threading.Lock()
        state = {"active": None, "peak": 0}
        phases = collections.defaultdict(list)

        def sample_locked():
            with lock:
                phase = state["active"]
            value = process_sample(process.pid)
            if value is None:
                return
            with lock:
                state["peak"] = max(state["peak"], value["rss_bytes"])
                if phase is not None and state["active"] is phase:
                    phase.setdefault("first", value)
                    phase["last"] = value
                    phase["peak"] = max(phase.get("peak", 0), value["rss_bytes"])
                    phase["samples"] = phase.get("samples", 0) + 1

        def sample():
            with sampling_lock:
                sample_locked()

        def acknowledge():
            process.stdin.write("continue\n")
            process.stdin.flush()

        def monitor():
            while not stop.is_set():
                sample()
                stop.wait(interval)

        thread = threading.Thread(target=monitor, daemon=True)
        thread.start()
        for line in process.stdout:
            event = json.loads(line)
            if event.get("resource_handshake") is not True:
                process.kill()
                stop.set()
                thread.join()
                process.wait()
                raise RuntimeError("benchmark binary lacks resource boundary handshake; rebuild before measuring")
            if event["event"] == "begin":
                with lock:
                    phase = {}
                    phases[event["operation"]].append(phase)
                    state["active"] = phase
                sample()
                acknowledge()
            elif event["event"] == "end":
                # Joining the in-flight sampler before the final snapshot and
                # acknowledgement prevents validation/next-phase CPU and RSS
                # from entering this operation's resource interval.
                with sampling_lock:
                    sample_locked()
                    with lock:
                        state["active"] = None
                acknowledge()
        _, status, usage = os.wait4(process.pid, 0)
        process.returncode = os.waitstatus_to_exitcode(status)
        stop.set()
        thread.join()
        if process.returncode:
            raise RuntimeError(f"{stem} failed ({process.returncode}); see {stderr.name}")
    native = json.loads(report.read_text())
    for sample_record in native["samples"]:
        metric = sample_record.get("metrics", sample_record)
        metric["wire_body_bytes"] = metric["request_body_bytes"] + metric["response_body_bytes"]
        if "server_cpu_microseconds" in metric:
            metric["server_cpu_microseconds_total"] = sum(metric["server_cpu_microseconds"].values())
        phase = phases[metric.get("resource_operation_name", metric.get("operation"))].pop(0)
        metric["client_rss_sample_count"] = phase.get("samples", 0)
        if "first" in phase:
            metric["client_rss_start_bytes"] = phase["first"]["rss_bytes"]
            metric["client_rss_peak_sampled_bytes"] = phase["peak"]
            metric["client_rss_growth_sampled_bytes"] = max(0, phase["peak"] - phase["first"]["rss_bytes"])
            metric["client_cpu_sampled_seconds"] = max(0, phase["last"]["cpu_seconds"] - phase["first"]["cpu_seconds"])
        metric["sample_round"] = round_number
    native["resource_measurement"] = {
        "started_at_utc": started, "finished_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "binary_sha256": binary_sha256,
        "os": platform.platform(), "sampling_interval_seconds": interval,
        "phase_boundary_protocol": "native begin/end stdin ACK; work timer excludes ACK; final sample completes before native validation continues",
        "client_rss_peak_sampled_bytes": state["peak"],
        "process_lifetime_maxrss_bytes": usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024),
        "process_wait4_user_seconds": usage.ru_utime, "process_wait4_system_seconds": usage.ru_stime,
        "limitations": ["ps sampling can miss transient peaks and very short CPU work", "ps TIME is centisecond-resolution process CPU", "wait4 lifetime usage includes setup/KDF and can include reaped children; it is not per-operation native crypto CPU", "RSS includes allocator retention, canonical metadata, runtime and HTTP buffers; not only plaintext", "sampler ps subprocesses perturb the workload"],
    }
    report.write_text(json.dumps(native, indent=2) + "\n")
    print(f"{stem}: verified ({len(native['samples'])} operations)", flush=True)
    return native


def percentile(values, fraction):
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)]


def acceptance(summaries, reports):
    """Declared local release regression gates, not hosted latency guarantees."""
    checks = []
    lookup = {(s["profile"], s["scenario"], s["scale"], s["operation"]): s for s in summaries}
    def check(name, actual, maximum):
        checks.append({"name": name, "actual": actual, "maximum": maximum, "passed": actual <= maximum})
    for (profile, scenario, scale, operation), encrypted in lookup.items():
        if profile != "encrypted":
            continue
        plain = lookup[("plain", scenario, scale, operation)]
        label = f"{scenario}/{scale}/{operation}"
        if operation in ["initial_push", "cold_clone_sync", "long_offline_push", "long_offline_fetch"]:
            check(label + "/wire-body", encrypted["wire_body_bytes"]["p95"], 3 * plain["wire_body_bytes"]["p95"] + 65536)
            check(label + "/wall-ms", encrypted["milliseconds"]["p95"], max(3 * plain["milliseconds"]["p95"], plain["milliseconds"]["p95"] + 500))
        if operation in ["warm_small_push", "warm_small_fetch"]:
            check(label + "/requests", encrypted["requests"]["p95"], 8)
            check(label + "/wire-body", encrypted["wire_body_bytes"]["p95"], 16384)
            check(label + "/wall-ms", encrypted["milliseconds"]["p95"], 500)
        if operation == "cold_clone_sync" and scenario == "objects":
            check(label + "/requests", encrypted["requests"]["p95"], 2048)
        if "client_rss_peak_sampled_bytes" in encrypted and "client_rss_peak_sampled_bytes" in plain:
            check(label + "/rss-over-plain", encrypted["client_rss_peak_sampled_bytes"]["p95"], plain["client_rss_peak_sampled_bytes"]["p95"] + 128 * 1024 * 1024)
    for operation in ["initial_push", "cold_clone_sync"]:
        small = lookup.get(("encrypted", "attachment", 7 * 1024 * 1024, operation))
        large = lookup.get(("encrypted", "attachment", 32 * 1024 * 1024, operation))
        if small and large:
            check(f"attachment/rss-scaling/{operation}", large["client_rss_peak_sampled_bytes"]["p95"] - small["client_rss_peak_sampled_bytes"]["p95"], 16 * 1024 * 1024)
    for index, report in enumerate(reports):
        evidence = report["evidence"]
        if report["scenario"] == "same-ref":
            check(f"same-ref/{report['profile']}/{index}/orphans", evidence["orphan_pack_bytes"], 4096)
        if report["scenario"] == "different-ref":
            check(f"different-ref/{report['profile']}/{index}/orphans", evidence["orphan_pack_bytes"], 0)
    return {"scope": "synthetic native core loopback loads, same-host paired release; sampled memory, not production tail guarantees", "passed": all(c["passed"] for c in checks), "checks": checks}


def hydration_acceptance(summaries):
    """Physical page gates account for compressed-frame and chunk quantization."""
    checks = []
    plain = {sample["operation"].removeprefix("plain/"): sample for sample in summaries if sample["profile"] == "plain"}
    for encrypted in summaries:
        if encrypted["profile"] != "encrypted":
            continue
        identity = encrypted["operation"].removeprefix("encrypted/")
        baseline = plain[identity]
        chunk_kib, _, client, cache = identity.split("/")
        cached = cache == "cached-page"
        limits = {
            "requests": 0 if cached else 5 if client == "cold-client" else 3,
            "wire_body_bytes": 0 if cached else baseline["wire_body_bytes"]["p95"] + 2 * int(chunk_kib) * 1024 + 4096,
            "milliseconds": max(3 * baseline["milliseconds"]["p95"], baseline["milliseconds"]["p95"] + 500),
            "client_rss_peak_sampled_bytes": baseline["client_rss_peak_sampled_bytes"]["p95"] + 128 * 1024 * 1024,
        }
        for field, maximum in limits.items():
            actual = encrypted[field]["p95"]
            checks.append({"name": encrypted["operation"] + "/" + field, "actual": actual, "maximum": maximum, "passed": actual <= maximum})
    return {"scope": "physical 4-KiB SQLite page hydration; bounded compressed-frame reads and authenticated chunk rounding; same-host paired release; sampled RSS", "passed": bool(checks) and all(c["passed"] for c in checks), "checks": checks}


def sdk_acceptance(summaries):
    """Paired budgets for actual changed SQLite/WAL rows, including materialization."""
    checks = []
    plain = {sample["operation"]: sample for sample in summaries if sample["profile"] == "plain"}
    for encrypted in summaries:
        if encrypted["profile"] != "encrypted":
            continue
        baseline = plain[encrypted["operation"]]
        limits = {
            "wire_body_bytes": 3 * baseline["wire_body_bytes"]["p95"] + 65536,
            "milliseconds": max(3 * baseline["milliseconds"]["p95"], baseline["milliseconds"]["p95"] + 500),
            "requests": 4 * baseline["requests"]["p95"] + 16,
            "client_rss_peak_sampled_bytes": baseline["client_rss_peak_sampled_bytes"]["p95"] + 128 * 1024 * 1024,
        }
        for field, maximum in limits.items():
            actual = encrypted[field]["p95"]
            checks.append({"name": encrypted["operation"] + "/" + field, "actual": actual, "maximum": maximum, "passed": actual <= maximum})
    return {"scope": "actual SDK SQLite/WAL row updates and materialized clone; same-host paired release; sampled RSS", "passed": all(c["passed"] for c in checks), "checks": checks}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/examples/e2ee_load_benchmark"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--interval", type=float, default=0.1)
    parser.add_argument("--quick", action="store_true")
    parser.add_argument("--scenario", choices=["objects", "history", "attachment", "same-ref", "different-ref"])
    parser.add_argument("--order", choices=["plain-first", "encrypted-first"], default="plain-first")
    parser.add_argument("--paired", action="store_true", help="sample a paired SDK/hydration example accepting REPORT ROUNDS ORDER")
    args = parser.parse_args()
    if args.rounds < 1 or args.interval < 0.02:
        parser.error("rounds >= 1 and interval >= 0.02 required")
    args.output.mkdir(parents=True, exist_ok=False)
    scenarios = [("objects", 100 if args.quick else 100000), ("history", 20 if args.quick else 4097), ("attachment", 1024 * 1024 if args.quick else 7 * 1024 * 1024), ("attachment", 2 * 1024 * 1024 if args.quick else 32 * 1024 * 1024), ("same-ref", 1), ("different-ref", 1)]
    reports = []
    if args.paired:
        reports.append(run(args.binary.resolve(), args.output.resolve(), args.order, args.binary.stem, args.rounds, 1, args.interval, paired=True))
    for scenario, scale in [] if args.paired else scenarios:
        if args.scenario and args.scenario != scenario:
            continue
        for round_number in range(1, args.rounds + 1):
            profiles = ["plain", "encrypted"] if args.order == "plain-first" else ["encrypted", "plain"]
            for profile in profiles:
                reports.append(run(args.binary.resolve(), args.output.resolve(), profile, scenario, scale, round_number, args.interval))
    groups = collections.defaultdict(list)
    for report in reports:
        for sample in report["samples"]:
            metric = sample.get("metrics", sample)
            groups[(sample.get("profile", report.get("profile")), report.get("scenario", args.binary.stem), report.get("scale", 0), sample["operation"])].append(metric)
    summaries = []
    for (profile, scenario, scale, operation), samples in groups.items():
        summary = {"profile": profile, "scenario": scenario, "scale": scale, "operation": operation, "samples": len(samples)}
        for field in ["milliseconds", "requests", "request_body_bytes", "response_body_bytes", "wire_body_bytes", "client_rss_peak_sampled_bytes", "client_rss_growth_sampled_bytes", "client_cpu_sampled_seconds", "server_cpu_microseconds_total"]:
            values = [sample[field] for sample in samples if field in sample]
            if values:
                summary[field] = {"p50": percentile(values, 0.5), "p95": percentile(values, 0.95)}
        summaries.append(summary)
    (args.output / "summary.json").write_text(json.dumps({"schema": "graft-e2ee-load-summary-v1", "order": args.order, "rounds": args.rounds, "discarded_warmups_per_case": max(report.get("warmup_rounds", 0) for report in reports), "percentiles": "nearest-rank; exploratory samples, not production tail estimates", "summaries": summaries}, indent=2) + "\n")
    result = None
    if not args.paired:
        result = acceptance(summaries, reports)
    elif args.binary.stem == "e2ee_sync_benchmark":
        result = sdk_acceptance(summaries)
    elif args.binary.stem == "e2ee_sqlite_hydration_benchmark":
        result = hydration_acceptance(summaries)
    if result is not None:
        (args.output / "acceptance.json").write_text(json.dumps(result, indent=2) + "\n")
        if not result["passed"]:
            raise RuntimeError("load acceptance budgets failed; inspect acceptance.json and retained raw reports")


if __name__ == "__main__":
    main()
