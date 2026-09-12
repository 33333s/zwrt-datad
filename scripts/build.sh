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

python3 scripts/generate-version.py

CFLAGS="-std=c11 -Os -ffunction-sections -fdata-sections \
  -Wall -Wextra -Wno-unused-parameter -D_GNU_SOURCE -Iinclude"

CRYPTO_PREFIX="$(bash scripts/build-static-crypto.sh)"
DATAD_BUILD="$ROOT/build/datad-embedded-aarch64"
rm -rf "$DATAD_BUILD"
mkdir -p "$DATAD_BUILD"
for source in src/*.c src/neighbor/*.c; do
  object="$DATAD_BUILD/${source//\//_}.o"
  $CC $CFLAGS -DWEB_CRYPTO_STATIC -DCLOUD_EMBEDDED -DDATAD_EMBEDDED \
    -I"$CRYPTO_PREFIX/include" -c "$source" -o "$object"
done
"$TC/aarch64-linux-ar" rcs "$DATAD_BUILD/libdatad.a" "$DATAD_BUILD"/*.o
(
  cd cloud
  CGO_ENABLED=1 GOOS=linux GOARCH=arm64 CC="$CC" \
  CGO_CFLAGS="-I$ROOT/include" \
  CGO_LDFLAGS="$DATAD_BUILD/libdatad.a $CRYPTO_PREFIX/lib/libcrypto.a -lm -ldl -pthread" \
  go build -tags datad_embedded,netgo,osusergo -trimpath \
    -ldflags="-s -w -X main.version=$(python3 -c 'import json; print(json.load(open("../version.json"))["datad"]["version"])') -linkmode external -extldflags -static" \
    -o "$ROOT/zwrt-datad" .
)
echo ">> link OK"
"$TC/aarch64-linux-size" zwrt-datad
"$TC/aarch64-linux-strip" -o "$ASSET" zwrt-datad
cp "$ASSET" zwrt-datad.stripped
ls -lh zwrt-datad "$ASSET" zwrt-datad.stripped
sha256sum "$ASSET"
python3 scripts/render-installer.py "$ASSET" build/install-datad.sh
sh -n build/install-datad.sh
echo "BUILD-OK"
