#!/bin/sh
# Requires binutils-arm-linux-gnueabihf; no firmware libraries needed to build.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
OUT=${1:-$ROOT/build/u50get}
mkdir -p "$OUT"
work=$(mktemp -d)
trap 'rm -f "$work/tiny.o" "$work/wrapper.elf"; rmdir "$work"' EXIT
arm-linux-gnueabihf-as -o "$work/tiny.o" "$ROOT/tools/u50get/tiny.S"
arm-linux-gnueabihf-ld -Ttext=0 -e 0 -o "$work/wrapper.elf" "$work/tiny.o"
arm-linux-gnueabihf-objcopy -O binary -j .text "$work/wrapper.elf" "$OUT/u50get-tiny"
chmod 700 "$OUT/u50get-tiny"
wc -c "$OUT/u50get-tiny"
sha256sum "$OUT/u50get-tiny"
