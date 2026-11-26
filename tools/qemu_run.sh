#!/usr/bin/env bash
# Boot novax.iso in QEMU, serial multiplexed into the terminal.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ISO="$ROOT/build/novax.iso"
[ -f "$ISO" ] || { echo "run tools/mkiso.sh first" >&2; exit 1; }

exec qemu-system-x86_64 \
    -M q35 \
    -smp 2 \
    -m 512M \
    -nographic \
    -no-reboot \
    -no-shutdown \
    -boot d \
    -cdrom "$ISO" \
    -netdev user,id=n0 \
    -device virtio-net-pci,netdev=n0,disable-modern=on \
    "$@"