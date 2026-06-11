#!/bin/bash
# Build u60-datad with the local Bootlin aarch64 musl toolchain.
# Usage: wsl -- bash -lc 'bash /mnt/d/.../u60-datad/scripts/build.sh'
set -e

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TC="$HOME/aarch64--musl--stable-2025.08-1/bin"
CC="$TC/aarch64-linux-gcc"
cd "$ROOT"

[ -x "$CC" ] || { echo "toolchain missing: $CC"; exit 1; }

CFLAGS="-std=c11 -Os -ffunction-sections -fdata-sections \
  -Wall -Wextra -Wno-unused-parameter -D_GNU_SOURCE -Iinclude"

$CC $CFLAGS src/*.c -static -Wl,--gc-sections -o u60-datad
echo ">> link OK"
"$TC/aarch64-linux-size" u60-datad
"$TC/aarch64-linux-strip" -o u60-datad.stripped u60-datad 2>/dev/null || true
ls -lh u60-datad u60-datad.stripped 2>/dev/null
echo "BUILD-OK"
