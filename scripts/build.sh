#!/bin/bash
# Build zwrt-datad with the local Bootlin aarch64 musl toolchain.
# Usage: wsl -- bash -lc 'bash /mnt/d/.../zwrt-datad/scripts/build.sh'
set -e

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TC="$HOME/aarch64--musl--stable-2025.08-1/bin"
CC="$TC/aarch64-linux-gcc"
cd "$ROOT"
ASSET="$(sed -n 's/^[[:space:]]*"asset":[[:space:]]*"\([^"]*\)".*/\1/p' version.json)"

[ -x "$CC" ] || { echo "toolchain missing: $CC"; exit 1; }
[ -n "$ASSET" ] || { echo "invalid asset name in version.json"; exit 1; }

CFLAGS="-std=c11 -Os -ffunction-sections -fdata-sections \
  -Wall -Wextra -Wno-unused-parameter -D_GNU_SOURCE -Iinclude"

$CC $CFLAGS src/*.c -static -Wl,--gc-sections -o zwrt-datad
echo ">> link OK"
"$TC/aarch64-linux-size" zwrt-datad
"$TC/aarch64-linux-strip" -o "$ASSET" zwrt-datad
cp "$ASSET" zwrt-datad.stripped
ls -lh zwrt-datad "$ASSET" zwrt-datad.stripped
sha256sum "$ASSET"
echo "BUILD-OK"
