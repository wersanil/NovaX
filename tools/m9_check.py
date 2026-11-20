#!/usr/bin/env python3
"""M9 integration check: virtio-net in userspace, ARP + ICMP ping.

Boots QEMU with a user-mode network backend, waits for the driver to
report link-up and a successful ping to the slirp gateway (10.0.2.2),
and checks that M5-M8 output is still present.
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

    if not wait_for(b"Milestone 9: virtio-net driver spawned", 20.0):
        failures.append("kernel did not spawn the virtio-net driver")
    if not wait_for(b"net: mac ", 20.0):
        failures.append("driver did not report its MAC address")
    if not wait_for(b"net: link up", 20.0):
        failures.append("driver did not reach link-up")
    if not wait_for(b"net: arp reply from 10.0.2.2", 30.0):
        failures.append("ARP resolution of 10.0.2.2 failed")
    if not wait_for(b"net: ping 10.0.2.2 ok", 30.0):
        failures.append("ICMP ping to 10.0.2.2 failed")

    # Regressions: userspace services must still come up on the same boot.
    if not wait_for(b"shell: ready", 15.0):
        failures.append("shell regressed")
    if not wait_for(b"server: got message:", 10.0):
        failures.append("M5 IPC ping regressed")

    with open(log_path, "wb") as f:
        f.write(bytes(buf))

    if b"KERNEL PANIC" in bytes(buf):
        failures.append("kernel panic during the run")
    if b"DOUBLE FAULT" in bytes(buf):
        failures.append("double fault during the run")

    text = bytes(buf).decode("utf-8", "replace")
    print("--- serial tail ---")
    print("\n".join(text.splitlines()[-20:]))
    print("-------------------")

    if failures:
        for f in failures:
            print(f"FAIL: {f}")
        return 1
    print("M9 CHECK PASSED (virtio-net ARP + ICMP ping, M5-M8 regressions)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
