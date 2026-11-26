#!/usr/bin/env python3
"""M8 integration check: SMP bring-up plus M5/M6/M7 regression.

Boots with two vCPUs, reads the serial stream until both APs report and
both pinned kernel demo workers have run on distinct CPUs, then exercises
the M7 shell round-trip over IPC.
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

    if not wait_for(b"smp: 2 cpu(s) reported by limine", 15.0):
        failures.append("limine did not report 2 CPUs")
    if not wait_for(b"smp: cpu1 online", 15.0):
        failures.append("AP1 did not come online")
    if not wait_for(b"smp: worker A running on cpu 0", 15.0):
        failures.append("CPU0 worker did not run on cpu 0")
    if not wait_for(b"smp: worker B running on cpu 1", 15.0):
        failures.append("CPU1 worker did not run on cpu 1")

    # Regression: userspace drivers and the M7 IPC path must still work.
    if not wait_for(b"serial: ready", 15.0):
        failures.append("serial driver did not become ready")
    if not wait_for(b"kbd: ready", 15.0):
        failures.append("keyboard driver did not become ready")
    # Two CPUs write to the serial line concurrently, so a userspace
    # message can interleave with driver output. Match the prefix, then
    # look for the echoed payload in a short window after it.
    if not wait_for(b"server: got message:", 10.0):
        failures.append("M5 ping round-trip regressed")
    else:
        idx = buf.find(b"server: got message:")
        if b"ping" not in buf[idx:idx + 96]:
            failures.append("M5 ping payload not echoed by server")
    if not wait_for(b"shell: ready", 15.0):
        failures.append("shell did not become ready")

    ser.sendall(b"echo hello\n")
    if not wait_for(b"shell: result=hello", 8.0):
        failures.append("'echo hello' via init regressed")

    with open(log_path, "wb") as f:
        f.write(bytes(buf))

    if b"KERNEL PANIC" in bytes(buf):
        failures.append("kernel panic during the run")
    if b"DOUBLE FAULT" in bytes(buf):
        failures.append("double fault during the run")

    text = bytes(buf).decode("utf-8", "replace")
    print("--- serial tail ---")
    print("\n".join(text.splitlines()[-18:]))
    print("-------------------")

    if failures:
        for f in failures:
            print(f"FAIL: {f}")
        return 1
    print("M8 CHECK PASSED (2 CPUs scheduling + M5/M6/M7 regressions)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
