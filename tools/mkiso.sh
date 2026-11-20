#!/usr/bin/env bash
# Build novax.iso from the release kernel + limine.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIMINE="$ROOT/third_party/limine"
ISO_ROOT="$ROOT/build/iso"
KERNEL="$ROOT/target/x86_64-unknown-none/release/novax_heart"
OUT="$ROOT/build/novax.iso"

[ -f "$KERNEL" ] || { echo "kernel not built: $KERNEL" >&2; exit 1; }
[ -d "$LIMINE" ] || { echo "run tools/fetch_limine.sh first" >&2; exit 1; }

rm -rf "$ISO_ROOT" "$OUT"
mkdir -p "$ISO_ROOT/boot" "$ISO_ROOT/EFI/BOOT"

cp "$KERNEL"                       "$ISO_ROOT/boot/novax_heart"
cp "$ROOT/boot/limine/limine.conf" "$ISO_ROOT/boot/limine.conf"
cp "$LIMINE/limine-bios.sys"       "$ISO_ROOT/boot/"
cp "$LIMINE/limine-bios-cd.bin"    "$ISO_ROOT/boot/"
cp "$LIMINE/limine-uefi-cd.bin"    "$ISO_ROOT/boot/"
cp "$LIMINE/BOOTX64.EFI"           "$ISO_ROOT/EFI/BOOT/"

xorriso -as mkisofs \
    -b boot/limine-bios-cd.bin \
    -no-emul-boot -boot-load-size 4 -boot-info-table \
    --efi-boot boot/limine-uefi-cd.bin \
    -efi-boot-part --efi-boot-image --protective-msdos-label \
    "$ISO_ROOT" -o "$OUT"

"$LIMINE/limine" bios-install "$OUT"
echo "ISO: $OUT"