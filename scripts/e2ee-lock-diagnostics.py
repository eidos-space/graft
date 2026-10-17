#!/usr/bin/env python3
"""Synthetic Linux directory-flock diagnostics, never performance acceptance.

Only a newly created private tree and an explicit generated tmpfiles config are
cleaned. The system's default tmpfiles configs are never loaded.
"""

import argparse
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import subprocess
import tempfile
import time


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def directory_fd(path):
    return os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)


def lock_rows(text, pid, metadata):
    expected = (os.major(metadata.st_dev), os.minor(metadata.st_dev), metadata.st_ino)
    rows = []
    for line in text.splitlines():
        fields = line.split()
        if len(fields) < 8 or fields[1:4] != ["FLOCK", "ADVISORY", "WRITE"]:
            continue
        major, minor, inode = fields[5].split(":")
        if int(fields[4]) == pid and (int(major, 16), int(minor, 16), int(inode)) == expected:
            rows.append(line)
    return rows


def stop_child(process):
    # The only signal target is this helper's own tmpfiles child. Never leave it
    # stopped if diagnostics fail or an assertion raises.
    if process.poll() is None:
        process.send_signal(signal.SIGCONT)
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def command(binary, root, config):
    # No caller-supplied cleanup path or configuration is accepted.
    assert root.is_absolute() and root.name == "private"
    assert root.parent.name.startswith("graft-tmpfiles-lock-")
    assert root.stat().st_mode & 0o777 == 0o700
    assert config.parent == root.parent and config.is_file()
    assert not any(character.isspace() for character in str(root))
    return [str(binary), "--clean", f"--prefix={root}", str(config)]


def run_cleanup(arguments, output, label):
    result = subprocess.run(arguments, capture_output=True, text=True, timeout=30)
    (output / f"{label}.stdout.log").write_text(result.stdout)
    (output / f"{label}.stderr.log").write_text(result.stderr)
    assert result.returncode == 0, f"{label} exited {result.returncode}"
    return {"command": arguments, "returncode": result.returncode}


def observe_directory_lock(binary, root, graft, config, output):
    arguments = command(binary, root, config)
    result = {"command": arguments, "observed": False}
    with (output / "scan.stdout.log").open("w") as stdout, (output / "scan.stderr.log").open("w") as stderr:
        process = subprocess.Popen(arguments, stdout=stdout, stderr=stderr)
        result["tmpfiles_pid"] = process.pid
        fd = directory_fd(graft)
        try:
            deadline = time.monotonic() + 30
            while process.poll() is None and time.monotonic() < deadline:
                locks = Path("/proc/locks").read_text()
                if not lock_rows(locks, process.pid, os.fstat(fd)):
                    time.sleep(0.001)
                    continue
                process.send_signal(signal.SIGSTOP)
                stopped_deadline = time.monotonic() + 2
                while time.monotonic() < stopped_deadline:
                    status = Path(f"/proc/{process.pid}/status").read_text()
                    if any(line.startswith("State:") and "T" in line for line in status.splitlines()):
                        break
                    time.sleep(0.001)
                else:
                    raise AssertionError("owned tmpfiles child did not stop")
                locks = Path("/proc/locks").read_text()
                rows = lock_rows(locks, process.pid, os.fstat(fd))
                assert rows, "tmpfiles released the target lock before the diagnostic snapshot"
                (output / "observed-proc-locks.txt").write_text(locks)
                result["matching_locks"] = rows
                result["owner_exe"] = os.readlink(f"/proc/{process.pid}/exe")
                result["owner_cmdline"] = Path(f"/proc/{process.pid}/cmdline").read_bytes().replace(b"\0", b" ").decode()
                result["owner_status"] = status
                assert Path(result["owner_exe"]).resolve() == binary.resolve()
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except OSError as error:
                    assert error.errno in (errno.EAGAIN, errno.EWOULDBLOCK)
                    result["gate_error_errno"] = error.errno
                else:
                    fcntl.flock(fd, fcntl.LOCK_UN)
                    raise AssertionError("exclusive directory gate unexpectedly succeeded")
                result["observed"] = True
                result["snapshot_method"] = "SIGSTOP only the identified owned tmpfiles child after observing its real kernel lock; no fabricated lock or acceptance timing."
                process.send_signal(signal.SIGCONT)
                break
            assert result["observed"], "the single tmpfiles scan did not expose its directory lock"
            result["returncode"] = process.wait(timeout=30)
            assert result["returncode"] == 0
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(fd, fcntl.LOCK_UN)
            result["gate_available_after_exit"] = True
        finally:
            stop_child(process)
            os.close(fd)
            (output / "scan-result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def protected_parent(binary, root, graft, config, output):
    sentinel = graft / "synthetic-cleanup-sentinel"
    sentinel.write_text("synthetic diagnostic data\n")
    before_count = sum(1 for path in root.rglob("*") if path.is_file())
    # tmpfiles honors directory locks when recursing into children, not on the
    # rule's directly configured root. This models a private TempDir below the
    # system /tmp rule, with a separately lockable .graft business directory.
    protected = graft.parent
    parent_fd = directory_fd(protected)
    gate_fd = directory_fd(graft)
    try:
        fcntl.flock(parent_fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
        fcntl.flock(gate_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(gate_fd, fcntl.LOCK_UN)
        result = run_cleanup(command(binary, root, config), output, "protected")
        assert sentinel.read_text() == "synthetic diagnostic data\n"
        after_count = sum(1 for path in root.rglob("*") if path.is_file())
        assert after_count == before_count
        fcntl.flock(gate_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(gate_fd, fcntl.LOCK_UN)
        result.update({"configured_cleanup_root": str(root),
                       "protected_private_child": str(protected),
                       "directly_configured_root_protection_claimed": False,
                       "files_before": before_count, "files_after": after_count,
                       "child_gate_available_with_parent_shared_lock": True,
                       "parent_shared_lock_prevented_cleanup": True})
    finally:
        os.close(gate_fd)
        os.close(parent_fd)
    # This control proves the generated rule would really clean this synthetic
    # data, rather than passing because of an ineffective age/configuration.
    result["unprotected_control"] = run_cleanup(command(binary, root, config), output, "unprotected-control")
    assert not sentinel.exists(), "unlocked age-zero control did not clean the synthetic sentinel"
    result["unprotected_control"]["sentinel_removed"] = True
    return result


def main(output):
    if platform.system() != "Linux":
        raise SystemExit("This diagnostic requires Linux; unsupported hosts are not a pass.")
    binary_name = shutil.which("systemd-tmpfiles")
    if binary_name is None:
        raise SystemExit("systemd-tmpfiles is required; missing tooling is not a pass.")
    binary = Path(binary_name).resolve()
    output.mkdir(parents=True, exist_ok=False)
    result = {"schema": "graft-e2ee-directory-lock-diagnostics-v1", "passed": False,
              "scope": "Synthetic Linux kernel flock / systemd-tmpfiles interaction only. Python flock probes the directory gate primitive; this is not a Graft performance sample or proof of the historical CI lock owner's identity.",
              "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
              "helper_sha256": sha256(__file__), "tmpfiles_sha256": sha256(binary),
              "tmpfiles_version": subprocess.check_output([str(binary), "--version"], text=True),
              "kernel": platform.platform(), "synthetic_file_count": 100000}
    try:
        with tempfile.TemporaryDirectory(prefix="graft-tmpfiles-lock-") as temporary:
            base = Path(temporary).resolve()
            root = base / "private"
            root.mkdir(mode=0o700)
            graft = root / "source" / ".graft"
            objects = graft / "objects" / "pack"
            objects.mkdir(parents=True, mode=0o700)
            graft.parent.chmod(0o700)
            for number in range(result["synthetic_file_count"]):
                fd = os.open(objects / f"synthetic-{number:06d}", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                os.close(fd)
            result["synthetic_root"] = str(root)
            result["directory_inodes"] = {str(path): {"device": path.stat().st_dev, "inode": path.stat().st_ino}
                                          for path in (root, root / "source", graft, graft / "objects", objects)}
            scan_config = base / "scan.conf"
            clean_config = base / "clean.conf"
            scan_config.write_text(f"d {root} 0700 - - 30d\n")
            clean_config.write_text(f"d {root} 0700 - - 0\n")
            result["configs"] = {"scan": scan_config.read_text(), "control": clean_config.read_text()}
            result["unprotected_scan"] = observe_directory_lock(binary, root, graft, scan_config, output)
            result["protected_parent"] = protected_parent(binary, root, graft, clean_config, output)
            result["passed"] = True
    except BaseException as error:
        result["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        manifest = {str(path.relative_to(output)): sha256(path) for path in sorted(output.rglob("*")) if path.is_file()}
        (output / "sha256.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(json.dumps(result, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    main(parser.parse_args().output.resolve())
