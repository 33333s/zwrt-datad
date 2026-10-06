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

# Signaling worker: small glibc ARM binary embedded into the main binary
# (embedded only when DATAD_DIAG_WORKER is exported for the cargo build).
cat > "$ROOT/build/arm32-toolchain/cc-glibc" <<SH
#!/usr/bin/env bash
exec "\${ZIG:?}" cc -target arm-linux-gnueabihf "\$@"
SH
chmod +x "$ROOT/build/arm32-toolchain/cc-glibc"
"$ROOT/build/arm32-toolchain/cc-glibc" -Os -Wl,--strip-all -fno-stack-protector   -o "$ROOT/build/u50-diag-worker" "$ROOT/rust/u50_diag_worker.c" -ldl
file "$ROOT/build/u50-diag-worker" | grep -q "ELF 32-bit.*ARM"
export DATAD_DIAG_WORKER="$ROOT/build/u50-diag-worker"

# Signaling streamer: same embed scheme; adds the loopback TCP fan-out.
"$ROOT/build/arm32-toolchain/cc-glibc" -Os -Wl,--strip-all -fno-stack-protector -o "$ROOT/build/u50-diag-streamer" "$ROOT/rust/u50_diag_streamer.c" -ldl
file "$ROOT/build/u50-diag-streamer" | grep -q "ELF 32-bit.*ARM"
export DATAD_DIAG_STREAMER="$ROOT/build/u50-diag-streamer"

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

# Release provenance for the ARMv7 asset (checked by scripts/publish-release.sh).
VERSION="$(python3 -c 'import json; print(json.load(open("version.json"))["datad"]["version"])')"
python3 - "$VERSION" "$(git rev-parse HEAD)" "$(shasum -a 256 build/zwrt-datad-armv7-candidate | awk '{print $1}')" >build/rust-release-provenance-armv7.json <<'PY'
import json
import sys

version, commit, sha256 = sys.argv[1:]
json.dump(
    {
        "implementation": "rust",
        "target": "armv7-unknown-linux-musleabihf",
        "version": version,
        "commit": commit,
        "asset": "zwrt-datad-armv7",
        "sha256": sha256,
    },
    sys.stdout,
    indent=2,
    sort_keys=True,
)
sys.stdout.write("\n")
PY
