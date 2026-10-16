#!/usr/bin/env python3
"""Bind unchanged E2EE load gates to a clean CI source and complete raw evidence."""

import argparse
import collections
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys


BINARIES = {
    "core": "e2ee_load_benchmark",
    "sdk": "e2ee_sync_benchmark",
    "hydration": "e2ee_sqlite_hydration_benchmark",
}
GROUPS = {
    f"{kind}-{order}": {
        "kind": kind,
        "order": order,
        "rounds": 3 if kind == "core" else 5,
        "reports": 36 if kind == "core" else 1,
        "operations": {"core": 210, "sdk": 50, "hydration": 360}[kind],
        "summaries": {"core": 38, "sdk": 10, "hydration": 72}[kind],
    }
    for kind in BINARIES
    for order in ("plain-first", "encrypted-first")
}
SCOPE = (
    "Clean Linux native-crypto release examples against a synthetic loopback "
    "HTTP Remote with an in-memory backend. No production WAN/TLS, physical "
    "second-device, signed-client, or other-platform performance claim. A pass "
    "does not explain or close previously observed tail-latency failures."
)


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def source_identity():
    status = command("git", "status", "--porcelain", "--untracked-files=all")
    if status:
        raise RuntimeError(f"performance evidence requires a clean checkout:\n{status}")
    paths = subprocess.check_output(["git", "ls-files", "-z"]).decode().split("\0")
    return {
        "commit": command("git", "rev-parse", "HEAD"),
        "tree": command("git", "rev-parse", "HEAD^{tree}"),
        "clean": True,
        "tracked_sha256": {path: sha256(Path(path)) for path in paths if path},
    }


def capture_source(output):
    output.mkdir(parents=True, exist_ok=False)
    identity = source_identity()
    identity.update({
        "schema": "graft-e2ee-performance-source-v1",
        "captured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "scope": SCOPE,
        "runner": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "cpu": command("lscpu"),
            "image_os": os.environ.get("ImageOS"),
            "image_version": os.environ.get("ImageVersion"),
        },
        "tools": {
            "rustc": command("rustc", "-Vv"),
            "cargo": command("cargo", "-V"),
            "node": command("node", "--version"),
            "pnpm": command("pnpm", "--version"),
            "python": sys.version,
            "clang": command("clang", "--version"),
        },
        "tool_executables": {
            name: {"path": str(Path(path).resolve()), "sha256": sha256(Path(path).resolve())}
            for name, path in {
                "rustc": command("rustup", "which", "rustc"),
                "cargo": command("rustup", "which", "cargo"),
                "node": shutil.which("node"),
                "pnpm": shutil.which("pnpm"),
                "python": sys.executable,
                "clang": shutil.which("clang"),
            }.items()
        },
        "build": {
            "profile": "release",
            "core_features": "native-crypto",
            "sdk_features": "native-crypto,bundled-sqlite",
            "locked": True,
            "rust_build_cache": False,
            "environment": {key: os.environ.get(key) for key in (
                "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL", "CARGO_PROFILE_RELEASE_DEBUG",
            )},
        },
        "github": {key: os.environ.get(key) for key in (
            "GITHUB_REPOSITORY", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_EVENT_NAME",
        )},
        "matrix": GROUPS,
    })
    write_json(output / "source.json", identity)


def require_unchanged_source(output):
    source = json.loads((output / "source.json").read_text())
    current = source_identity()
    for key in ("commit", "tree", "tracked_sha256"):
        if source[key] != current[key]:
            raise RuntimeError(f"source changed after capture: {key}")
    return source


def capture_binaries(output, target):
    source = require_unchanged_source(output)
    remote = Path("packages/graft-remote/dist")
    if not (remote / "index.js").is_file():
        raise RuntimeError("compiled loopback Remote is missing")
    write_json(output / "binaries.json", {
        "source_commit": source["commit"],
        "source_manifest_sha256": sha256(output / "source.json"),
        "binaries": {
            kind: {"name": name, "sha256": sha256(target / "release/examples" / name)}
            for kind, name in BINARIES.items()
        },
        "remote_dist_sha256": {
            str(path): sha256(path) for path in sorted(remote.rglob("*")) if path.is_file()
        },
    })


def inspect_group(output, name, expected, binaries):
    directory = output / name
    summary = json.loads((directory / "summary.json").read_text())
    acceptance = json.loads((directory / "acceptance.json").read_text())
    if summary["order"] != expected["order"] or summary["rounds"] != expected["rounds"]:
        raise ValueError("wrong pair order or round count")
    rows = summary["summaries"]
    if len(rows) != expected["summaries"] or {row["profile"] for row in rows} != {"plain", "encrypted"}:
        raise ValueError("incomplete paired summary coverage")
    if any(row["samples"] < expected["rounds"] for row in rows):
        raise ValueError("insufficient samples in a summary case")
    checks = acceptance["checks"]
    if not checks or any(type(check.get("passed")) is not bool for check in checks):
        raise ValueError("missing or invalid acceptance checks")
    if acceptance.get("passed") is not all(check["passed"] for check in checks):
        raise ValueError("acceptance result disagrees with retained checks")
    paths = sorted(path for path in directory.glob("*.json")
                   if path.name not in ("summary.json", "acceptance.json"))
    if len(paths) != expected["reports"]:
        raise ValueError("incomplete raw process reports")
    reports = [json.loads(path.read_text()) for path in paths]
    operations = sum(len(report["samples"]) for report in reports)
    if operations != expected["operations"]:
        raise ValueError(f"incomplete measured operations: {operations}")
    binary_hash = binaries["binaries"][expected["kind"]]["sha256"]
    for report in reports:
        if report.get("native_crypto_enabled") is not True:
            raise ValueError("raw report did not enable native-crypto")
        if report.get("build_profile", "release") != "release":
            raise ValueError("raw report came from a debug build")
        if report["resource_measurement"]["binary_sha256"] != binary_hash:
            raise ValueError("raw report executable differs from captured binary")
    if expected["kind"] == "core":
        cases = collections.Counter((r["profile"], r["scenario"], r["scale"]) for r in reports)
        expected_cases = {
            (profile, scenario, scale): expected["rounds"]
            for profile in ("plain", "encrypted")
            for scenario, scale in (("objects", 100000), ("history", 4097),
                                    ("attachment", 7 * 1024 * 1024),
                                    ("attachment", 32 * 1024 * 1024),
                                    ("same-ref", 1), ("different-ref", 1))
        }
        if cases != expected_cases:
            raise ValueError("raw reports omitted or reduced a full-scale core case")
    return {
        "complete": True,
        "passed": acceptance["passed"],
        "raw_reports": len(reports),
        "measured_operations": operations,
        "checks": len(checks),
        "failed_checks": [check for check in checks if not check["passed"]],
        "acceptance_path": f"{name}/acceptance.json",
        "summary_path": f"{name}/summary.json",
    }


def finalize(output):
    output.mkdir(parents=True, exist_ok=True)
    result = {"schema": "graft-e2ee-performance-evidence-v1", "scope": SCOPE, "groups": {}, "errors": []}
    binaries = {}
    try:
        source = require_unchanged_source(output)
        result["source_commit"] = source["commit"]
        binaries = json.loads((output / "binaries.json").read_text())
        if (binaries["source_commit"] != source["commit"]
                or binaries["source_manifest_sha256"] != sha256(output / "source.json")):
            raise ValueError("binary provenance is not bound to this source manifest")
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.CalledProcessError) as error:
        result["errors"].append(str(error))
    for name, expected in GROUPS.items():
        try:
            result["groups"][name] = inspect_group(output, name, expected, binaries)
        except (OSError, ValueError, KeyError, TypeError) as error:
            result["groups"][name] = {"complete": False, "passed": False, "error": str(error)}
    result["passed"] = not result["errors"] and all(group["passed"] for group in result["groups"].values())
    for key in ("raw_reports", "measured_operations", "checks"):
        result[key] = sum(group.get(key, 0) for group in result["groups"].values())
    write_json(output / "result.json", result)
    # Logs, individual failed checks, raw native/resource data, and provenance
    # remain independently inspectable even when the overall result is false.
    write_json(output / "sha256.json", {
        str(path.relative_to(output)): sha256(path)
        for path in sorted(output.rglob("*"))
        if path.is_file() and path.name != "sha256.json"
    })
    lines = ["## E2EE performance evidence", "", SCOPE, "",
             f"Candidate: `{result.get('source_commit', 'unverified')}`", "",
             "| Group | Complete | Existing budgets |",
             "| --- | --- | --- |"]
    lines.extend(f"| {name} | {group['complete']} | {'pass' if group['passed'] else 'FAIL'} |"
                 for name, group in result["groups"].items())
    lines.extend(["", "All groups run once; no retry or failed-sample removal.", ""])
    rendered = "\n".join(lines)
    print(rendered)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as stream:
            stream.write(rendered)
    return result["passed"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("source", "binaries", "finalize"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--target", type=Path)
    args = parser.parse_args()
    if args.action == "source":
        capture_source(args.output)
    elif args.action == "binaries":
        if args.target is None:
            parser.error("binaries requires --target")
        capture_binaries(args.output, args.target)
    elif not finalize(args.output):
        sys.exit(1)


if __name__ == "__main__":
    main()
