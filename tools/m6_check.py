#!/usr/bin/env python3
"""M6 integration check: drive the userspace serial and PS/2 servers.

Connects to QEMU's serial and monitor unix sockets, feeds bytes into COM1,
injects PS/2 keys via the monitor, and asserts on the kernel/userspace
serial log. Exit code 0 = both IRQ delivery paths work.
"""

import os
import select
import socket
import sys
import time

SERIAL_READY = b"serial: ready"
KBD_READY = b"kbd: ready"


def connect(path: str, timeout: float = 15.0) -> socket.socket:
    deadline = time.time() + timeout
    last: Exception | None = None
    while time.time() < deadline:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(path)
            s.setblocking(False)
            return s
        except OSError as e:  # socket not created yet / refused
            last = e
            time.sleep(0.1)
    raise SystemExit(f"cannot connect to {path}: {last}")


def main() -> int:
    ser_path, mon_path, log_path = sys.argv[1:4]
    ser = connect(ser_path)
    mon = connect(mon_path)

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

    if not wait_for(SERIAL_READY, 15.0):
        failures.append("serial server did not become ready")
    if not wait_for(KBD_READY, 5.0):
        failures.append("kbd server did not become ready")

    # COM1 input -> IRQ4 -> serial server echo.
    ser.sendall(b"xyz")
    if not wait_for(b"serial: rx 0x78", 5.0):
        failures.append("COM1 byte 'x' was not delivered over IRQ4")
    if not wait_for(b"serial: rx 0x7a", 5.0):
        failures.append("COM1 byte 'z' was not delivered over IRQ4")

    # PS/2 key injection -> IRQ1 -> kbd server (0x1E = 'a' make code).
    mon.sendall(b"sendkey a\n")
    if not wait_for(b"kbd: scancode 0x1e", 5.0):
        failures.append("PS/2 make code 0x1e was not delivered over IRQ1")

    with open(log_path, "wb") as f:
        f.write(bytes(buf))

    if b"KERNEL PANIC" in bytes(buf):
        failures.append("kernel panic during the run")

    text = bytes(buf).decode("utf-8", "replace")
    preview = "\n".join(text.splitlines()[-15:])
    print("--- serial tail ---")
    print(preview)
    print("-------------------")

    if failures:
        for f in failures:
            print(f"FAIL: {f}")
        return 1
    print("M6 CHECK PASSED (IRQ4 serial + IRQ1 keyboard via IPC)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
