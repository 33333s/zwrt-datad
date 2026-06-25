#!/bin/bash
# Build zwrt-datad with the local Bootlin aarch64 musl toolchain.
# Usage: wsl -- bash -lc 'bash /mnt/d/.../zwrt-datad/scripts/build.sh'
set -e

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TC="$HOME/aarch64--musl--stable-2025.08-1/bin"
CC="$TC/aarch64-linux-gcc"
cd "$ROOT"

[ -x "$CC" ] || { echo "toolchain missing: $CC"; exit 1; }

CFLAGS="-std=c11 -Os -ffunction-sections -fdata-sections \
  -Wall -Wextra -Wno-unused-parameter -D_GNU_SOURCE -Iinclude"

$CC $CFLAGS src/*.c -static -Wl,--gc-sections -o zwrt-datad
echo ">> link OK"
"$TC/aarch64-linux-size" zwrt-datad
"$TC/aarch64-linux-strip" -o zwrt-datad.stripped zwrt-datad 2>/dev/null || true
ls -lh zwrt-datad zwrt-datad.stripped 2>/dev/null
echo "BUILD-OK"
