#!/usr/bin/env bash
# Experimental U50 read-only candidate. This is not a release/install script.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET=armv7-unknown-linux-musleabihf
ZIG="${ZIG:-$(command -v zig)}"
RUSTUP="${RUSTUP:-$(command -v rustup)}"
CARGO="${CARGO:-$(command -v cargo)}"
mkdir -p "$ROOT/build/arm32-toolchain"
cat > "$ROOT/build/arm32-toolchain/cc" <<'SH'
#!/usr/bin/env bash
args=()
for arg in "$@"; do
  case "$arg" in --target=*) ;; *) args+=("$arg") ;; esac
done
exec "${ZIG:?}" cc -target arm-linux-musleabihf "${args[@]}"
SH
cat > "$ROOT/build/arm32-toolchain/ar" <<'SH'
#!/usr/bin/env bash
exec "${ZIG:?}" ar "$@"
SH
chmod +x "$ROOT/build/arm32-toolchain/cc" "$ROOT/build/arm32-toolchain/ar"
export ZIG
export RUSTFLAGS="${RUSTFLAGS:-} -C link-self-contained=no"
export CC_armv7_unknown_linux_musleabihf="$ROOT/build/arm32-toolchain/cc"
export AR_armv7_unknown_linux_musleabihf="$ROOT/build/arm32-toolchain/ar"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER="$ROOT/build/arm32-toolchain/cc"
"$RUSTUP" target add --toolchain 1.89.0 "$TARGET"
cd "$ROOT"
"$CARGO" +1.89.0 build --manifest-path rust/Cargo.toml --locked --release --target "$TARGET" --bin zwrt-datad
cp "rust/target/$TARGET/release/zwrt-datad" "build/zwrt-datad-armv7-candidate"
file build/zwrt-datad-armv7-candidate | grep -q "ELF 32-bit.*ARM.*statically linked.*stripped"
file build/zwrt-datad-armv7-candidate
shasum -a 256 build/zwrt-datad-armv7-candidate
