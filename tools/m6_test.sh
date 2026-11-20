#!/usr/bin/env bash
# Milestone 6 integration check: userspace serial (IRQ4) + PS/2 kbd (IRQ1).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SER="$ROOT/build/m6-serial.sock"
MON="$ROOT/build/m6-mon.sock"
SER_LOG="$ROOT/build/m6-serial.log"
QEMU_LOG="$ROOT/build/m6-qemu.log"

cargo build --release -p novax_heart
"$ROOT/tools/mkiso.sh"

rm -f "$SER" "$MON" "$SER_LOG" "$QEMU_LOG"

qemu-system-x86_64 \
    -M q35 \
    -smp 2 \
    -m 512M \
    -display none \
    -serial "unix:$SER,server,nowait" \
    -monitor "unix:$MON,server,nowait" \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -no-reboot \
    -boot d \
    -cdrom "$ROOT/build/novax.iso" \
    >"$QEMU_LOG" 2>&1 &
QPID=$!

cleanup() {
    kill "$QPID" 2>/dev/null || true
    wait "$QPID" 2>/dev/null || true
}
trap cleanup EXIT

python3 "$ROOT/tools/m6_check.py" "$SER" "$MON" "$SER_LOG"
