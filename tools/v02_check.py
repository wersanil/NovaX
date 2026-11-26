#!/usr/bin/env python3
"""v0.2 integration check: mint/revoke, reply-cap, notification, respawn.

Boots QEMU, waits for the supervisor stand to complete and also checks
that the M5/M7 services still come up on the same boot.
"""

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
                data = ser.recv(8192)
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

    checks = [
        (b"v02: supervisor ready", 20.0, "supervisor did not start"),
        (b"v02: server spawned", 20.0, "server was not spawned"),
        (b"v02: client spawned", 20.0, "client was not spawned"),
        (b"v02: client request 1 -> pong", 30.0, "first synchronous call failed"),
        (b"v02: server fault badge=0x2", 30.0, "fault notification missing"),
        (b"v02: revoked shared buffer", 20.0, "SYS_CAP_REVOKE did not run"),
        (b"v02: client shm gone", 20.0, "client shared mapping was not revoked"),
        (b"v02: server respawned", 20.0, "replacement server was not spawned"),
        (b"v02: client request 2 -> pong", 30.0, "second call after respawn failed"),
        (b"v02: PASS", 20.0, "supervisor did not report PASS"),
    ]
    for needle, timeout, message in checks:
        if not wait_for(needle, timeout):
            failures.append(message)

    # Regressions: the normal shell path must still be alive on this boot.
    if not wait_for(b"shell: ready", 20.0):
        failures.append("shell regressed")
    if not wait_for(b"server: got message:", 15.0):
        failures.append("M5 IPC ping regressed")

    with open(log_path, "wb") as f:
        f.write(bytes(buf))

    if b"KERNEL PANIC" in bytes(buf):
        failures.append("kernel panic during the run")
    if b"DOUBLE FAULT" in bytes(buf):
        failures.append("double fault during the run")

    text = bytes(buf).decode("utf-8", "replace")
    print("--- serial tail ---")
    print("\n".join(text.splitlines()[-25:]))
    print("-------------------")

    if failures:
        for f in failures:
            print(f"FAIL: {f}")
        return 1
    print("V02 CHECK PASSED (mint/revoke, reply-cap, notification, respawn)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
