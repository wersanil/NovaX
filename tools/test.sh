#!/usr/bin/env bash
# Build with --features test, run in QEMU, check exit code.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ISO="$ROOT/build/novax.iso"
LOG="$ROOT/build/test-serial.log"

cargo build --release --features test -p novax_heart
"$ROOT/tools/mkiso.sh"

rm -f "$LOG"

# isa-debug-exit: QEMU exits with (value << 1) | 1.
# Success = 0x10 -> exit code 0x21 (33). Failed = 0x11 -> 0x23 (35).
set +e
qemu-system-x86_64 \
    -M q35 \
    -smp 2 \
    -m 512M \
    -display none \
    -serial "file:$LOG" \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -no-reboot \
    -boot d \
    -cdrom "$ISO"
CODE=$?
set -e

echo "--- serial log ---"
cat "$LOG"
echo "------------------"

if [ "$CODE" -eq 33 ]; then
    echo "TESTS PASSED (qemu exit=$CODE)"
    exit 0
else
    echo "TESTS FAILED (qemu exit=$CODE)"
    exit 1
fi