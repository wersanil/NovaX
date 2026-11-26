#!/usr/bin/env bash
# Milestone 9 integration check: virtio-net driver in userspace + ICMP ping.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SER="$ROOT/build/m9-serial.sock"
SER_LOG="$ROOT/build/m9-serial.log"
QEMU_LOG="$ROOT/build/m9-qemu.log"

cargo build --release -p novax_heart
"$ROOT/tools/mkiso.sh"

rm -f "$SER" "$SER_LOG" "$QEMU_LOG"

qemu-system-x86_64 \
    -M q35 \
    -smp 2 \
    -m 512M \
    -display none \
    -serial "unix:$SER,server,nowait" \
    -monitor none \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -no-reboot \
    -boot d \
    -cdrom "$ROOT/build/novax.iso" \
    -netdev user,id=n0 \
    -device virtio-net-pci,netdev=n0,disable-modern=on \
    >"$QEMU_LOG" 2>&1 &
QPID=$!

cleanup() {
    kill "$QPID" 2>/dev/null || true
    wait "$QPID" 2>/dev/null || true
}
trap cleanup EXIT

python3 "$ROOT/tools/m9_check.py" "$SER" "$SER_LOG"
