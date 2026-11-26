#!/usr/bin/env bash
# Клонирует исходники limine и собирает хост-утилиту `limine`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/third_party/limine"
BRANCH="v8.x-binary"

if [ ! -d "$DEST/.git" ]; then
    mkdir -p "$ROOT/third_party"
    git clone --branch="$BRANCH" --depth 1 \
        https://github.com/limine-bootloader/limine.git "$DEST"
fi

echo "Building limine host tool..."
make -C "$DEST" limine

# Проверяем, что все необходимые файлы на месте.
for f in limine limine-bios.sys limine-bios-cd.bin limine-uefi-cd.bin BOOTX64.EFI; do
    [ -e "$DEST/$f" ] || { echo "missing $f in $DEST" >&2; exit 1; }
done

echo "limine ready at $DEST"