#!/usr/bin/env python3
"""M7 integration check: shell -> init command/reply path over IPC.

Feeds commands into COM1 through QEMU's serial unix socket and asserts
on the bytes the shell writes back. Exit code 0 = IPC command round-trip.
"""

import os
import select
import socket
import sys
import time


def connect(path: str, timeout: float = 15.0) -> socket.socket:
    deadline = time.time() + timeout
    last: Exception | None = None
    while time.time() < deadline:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(path)
            s.setblocking(False)
            return s
        except OSError as e:
            last = e
            time.sleep(0.1)
    raise SystemExit(f"cannot connect to {path}: {last}")


def main() -> int:
    ser_path, log_path = sys.argv[1:3]
    ser = connect(ser_path)

    buf = bytearray()

    def pump(seconds: float) -> None:
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([ser], [], [], 0.05)
            if not r:
                continue
            try:
                data = ser.recv(4096)
            except BlockingIOError:
                continue
            if not data:
                return
            buf.extend(data)

    def wait_for(needle: bytes, timeout: float) -> bool:
        end = time.time() + timeout
        while time.time() < end:
            if needle in buf:
                return True
            pump(0.1)
        return needle in buf

    failures = []

    if not wait_for(b"shell: ready", 15.0):
        failures.append("shell did not become ready")

    ser.sendall(b"echo hello\n")
    if not wait_for(b"shell: result=hello", 5.0):
        failures.append("'echo hello' round-trip via init failed")

    ser.sendall(b"bogus\n")
    if not wait_for(b"shell: result=unknown command", 5.0):
        failures.append("unknown command handling failed")

    with open(log_path, "wb") as f:
        f.write(bytes(buf))

    if b"KERNEL PANIC" in bytes(buf):
        failures.append("kernel panic during the run")

    text = bytes(buf).decode("utf-8", "replace")
    print("--- serial tail ---")
    print("\n".join(text.splitlines()[-12:]))
    print("-------------------")

    if failures:
        for f in failures:
            print(f"FAIL: {f}")
        return 1
    print("M7 CHECK PASSED (shell -> init -> shell over IPC)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
