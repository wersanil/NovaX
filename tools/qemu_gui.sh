#!/usr/bin/env bash
# Boot novax.iso in a graphical QEMU window.
#
# The kernel still mirrors its console to COM1; `-serial mon:stdio` keeps the
# serial log visible in the terminal. Set QEMU_DISPLAY (for example `sdl`,
# `gtk` or `none`) to override QEMU's default display backend.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ISO="$ROOT/build/novax.iso"
[ -f "$ISO" ] || { echo "run tools/mkiso.sh first" >&2; exit 1; }

DISPLAY_ARGS=()
if [ -n "${QEMU_DISPLAY:-}" ]; then
    DISPLAY_ARGS=(-display "$QEMU_DISPLAY")
fi

exec qemu-system-x86_64 \
    -M q35 \
    -smp 2 \
    -m 512M \
    -vga std \
    "${DISPLAY_ARGS[@]}" \
    -serial mon:stdio \
    -no-reboot \
    -no-shutdown \
    -boot d \
    -cdrom "$ISO" \
    -netdev user,id=n0 \
    -device virtio-net-pci,netdev=n0,disable-modern=on \
    "$@"
