#!/bin/bash
# Build the production Rust datad with the Bootlin aarch64 musl toolchain.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TC="${DATAD_MUSL_TOOLCHAIN_DIR:-$HOME/aarch64--musl--stable-2025.08-1/bin}"
TARGET=aarch64-unknown-linux-musl
RUST_TOOLCHAIN="${DATAD_RUST_TOOLCHAIN:-1.89.0}"
RUSTUP="${RUSTUP:-$HOME/.cargo/bin/rustup}"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
CC="$TC/aarch64-linux-gcc"
AR="$TC/aarch64-linux-ar"
STRIP="$TC/aarch64-linux-strip"

cd "$ROOT"
ASSET="$(sed -n 's/^[[:space:]]*"asset":[[:space:]]*"\([^"]*\)".*/\1/p' version.json)"

[ -x "$RUSTUP" ] || { echo "rustup missing: $RUSTUP" >&2; exit 1; }
[ -x "$CARGO" ] || { echo "cargo missing: $CARGO" >&2; exit 1; }
[ -x "$CC" ] || { echo "toolchain missing: $CC" >&2; exit 1; }
[ -x "$AR" ] || { echo "toolchain missing: $AR" >&2; exit 1; }
[ -x "$STRIP" ] || { echo "toolchain missing: $STRIP" >&2; exit 1; }
[ "$ASSET" = zwrt-datad-aarch64 ] || { echo "invalid asset name in version.json" >&2; exit 1; }

if ! "$RUSTUP" toolchain list | grep -Eq "^${RUST_TOOLCHAIN}(-|[[:space:]])"; then
  "$RUSTUP" toolchain install "$RUST_TOOLCHAIN" --profile minimal
fi
"$RUSTUP" target add --toolchain "$RUST_TOOLCHAIN" "$TARGET"

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$CC"
export CC_aarch64_unknown_linux_musl="$CC"
export AR_aarch64_unknown_linux_musl="$AR"

"$CARGO" "+$RUST_TOOLCHAIN" build \
  --manifest-path rust/Cargo.toml \
  --locked --release --target "$TARGET"
"$STRIP" -o "$ASSET" "rust/target/$TARGET/release/zwrt-datad"

file "$ASSET" | grep -q 'ARM aarch64.*statically linked.*stripped' || {
  echo "release asset is not a static stripped aarch64 binary" >&2
  exit 1
}
sha256sum "$ASSET"

QEMU="${QEMU_AARCH64:-$(command -v qemu-aarch64 2>/dev/null || true)}"
if [ -n "$QEMU" ]; then
  expected="$(python3 -c 'import json; print(json.load(open("version.json"))["datad"]["version"])')"
  actual="$($QEMU "$ASSET" --version)"
  [ "$actual" = "zwrt-datad $expected" ] || {
    echo "embedded version mismatch: $actual" >&2
    exit 1
  }
fi

python3 scripts/render-installer.py "$ASSET" build/install-datad.sh
sh -n build/install-datad.sh
if [ -n "${DATAD_OTA_SIGNING_KEY_FILE:-}" ]; then
  python3 scripts/sign-update.py --key "$DATAD_OTA_SIGNING_KEY_FILE" --binary "$ASSET"
fi
echo "BUILD-OK"
