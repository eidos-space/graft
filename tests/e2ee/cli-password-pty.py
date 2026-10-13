"""Native CLI password-input test helper. Secret bytes enter only through stdin."""
import errno
import fcntl
import json
import os
import select
import signal
import subprocess
import sys
import termios
import time
import pty

kind, *command = sys.argv[1:]
multiple_inputs = kind.startswith(("change-", "initialize-"))
payload = sys.stdin.buffer.read(16385 if multiple_inputs else 1026)
assert not multiple_inputs or len(payload) <= 16384, "PTY input exceeded its framing budget"
secrets = [bytes(value) for value in json.loads(payload)] if multiple_inputs else [payload]
assert all(len(value) <= 1025 for value in secrets), "PTY input exceeded its budget"
secret = secrets[0]
master, slave = pty.openpty()
# macOS revokes the slave after its controlling session leader exits; the
# master retains the terminal attributes. Other Unix runners inspect the slave.
attributes_fd = master if sys.platform == "darwin" else slave
original = termios.tcgetattr(attributes_fd)

def controlling_terminal():
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)

child = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave,
                         start_new_session=True, preexec_fn=controlling_terminal)
captured = bytearray()

def collect(timeout):
    if select.select([master], [], [], timeout)[0]:
        try:
            captured.extend(os.read(master, 65536))
        except OSError as error:
            if error.errno != errno.EIO:
                raise

prompts = []
def wait_prompt(prompt):
    deadline = time.monotonic() + 15
    while prompt not in captured:
        collect(0.05)
        assert child.poll() is None, "CLI exited before its password prompt"
        assert time.monotonic() < deadline, "CLI password prompt timed out"
    hidden = termios.tcgetattr(attributes_fd)
    assert not hidden[3] & (termios.ECHO | termios.ECHONL | termios.ICANON | termios.ISIG), "password terminal was not hidden"
    prompts.append(prompt)

try:
    if kind.startswith("initialize-"):
        assert len(secrets) == 2, "initialization test requires two protected inputs"
        wait_prompt(b"New sync password: ")
        os.write(master, secrets[0] + (b"\x1b" if kind == "initialize-cancel-new" else b"\n"))
        if kind not in ("initialize-cancel-new", "initialize-invalid-new", "initialize-overlong-new"):
            wait_prompt(b"Confirm new sync password: ")
            if kind == "initialize-term-confirm":
                os.write(master, secrets[1])
                os.kill(child.pid, signal.SIGTERM)
            else:
                assert kind in ("initialize-confirm", "initialize-mismatch", "initialize-cancel-confirm", "initialize-invalid-confirm"), "unknown initialization mode"
                os.write(master, secrets[1] + (b"\x1b" if kind == "initialize-cancel-confirm" else b"\n"))
    else:
        wait_prompt(b"Sync password: ")
    if kind.startswith("initialize-"):
        pass
    elif kind.startswith("change-"):
        assert len(secrets) == 3, "password-change test requires three protected inputs"
        os.write(master, secrets[0] + b"\n")
        wait_prompt(b"New sync password: ")
        os.write(master, secrets[1] + (b"\x1b" if kind == "change-cancel-new" else b"\n"))
        if kind != "change-cancel-new":
            wait_prompt(b"Confirm new sync password: ")
            if kind == "change-term-confirm":
                os.write(master, secrets[2])
                os.kill(child.pid, signal.SIGTERM)
            else:
                assert kind in ("change-confirm", "change-mismatch", "change-cancel-confirm"), "unknown change mode"
                os.write(master, secrets[2] + (b"\x1b" if kind == "change-cancel-confirm" else b"\n"))
    elif kind == "enter":
        os.write(master, secret + b"\n")
    elif kind == "enter-edited":
        os.write(master, secret + "多".encode("utf-8") + b"\x7f\n")
    elif kind in ("ctrl-c", "ctrl-d", "ctrl-z", "escape"):
        os.write(master, secret + {"ctrl-c": b"\x03", "ctrl-d": b"\x04",
                                  "ctrl-z": b"\x1a", "escape": b"\x1b"}[kind])
    elif kind in ("sigterm", "sigint", "sighup", "sigquit", "sigtstp"):
        os.write(master, secret)
        os.kill(child.pid, {"sigterm": signal.SIGTERM, "sigint": signal.SIGINT,
                            "sighup": signal.SIGHUP, "sigquit": signal.SIGQUIT,
                            "sigtstp": signal.SIGTSTP}[kind])
    else:
        raise AssertionError("unknown PTY test mode")
    deadline = time.monotonic() + 30
    while child.poll() is None:
        collect(0.05)
        assert time.monotonic() < deadline, "CLI did not exit after password input"
    for _ in range(5):
        collect(0.01)
    assert termios.tcgetattr(attributes_fd) == original, "CLI did not restore exact terminal attributes"
    assert all(not value or value not in captured for value in secrets), "secret appeared in terminal output"
    prefix = captured[:captured.index(b"{")]
    assert prefix.replace(b"\r\n", b"\n") == b"".join(prompt + b"\n" for prompt in prompts), "unexpected password feedback appeared"
    responses = [json.loads(line) for line in captured.decode("utf-8").splitlines()
                 if line.startswith("{")]
    assert len(responses) == 1, "CLI did not return exactly one JSON result"
    print(json.dumps({"status": child.returncode, "result": responses[0],
                      "echo_disabled": True, "restored": True}))
finally:
    if child.poll() is None:
        child.kill()
    child.wait()
    os.close(master)
    os.close(slave)
