#!/usr/bin/env bash
# Embedded Web build; --candidate also stages the verified input for local packaging.
set -euo pipefail
case "${1:-}" in ""|--candidate) ;; *) echo "Usage: $0 [--candidate]" >&2; exit 2 ;; esac
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/build/web-ui/armv7"
TARGET=armv7-unknown-linux-musleabihf
ZIG="${ZIG:-$(command -v zig)}"
test -f "$ROOT/web/dist/embedded.rs" || { echo 'Run npm ci && npm run build in web/ first.' >&2; exit 1; }
mkdir -p "$OUT/toolchain"
cat > "$OUT/toolchain/cc" <<'SH'
#!/usr/bin/env bash
args=()
for arg in "$@"; do
  case "$arg" in --target=*) ;; *) args+=("$arg") ;; esac
done
exec "${ZIG:?}" cc -target arm-linux-musleabihf "${args[@]}"
SH
cat > "$OUT/toolchain/ar" <<'SH'
#!/usr/bin/env bash
exec "${ZIG:?}" ar "$@"
SH
chmod +x "$OUT/toolchain/cc" "$OUT/toolchain/ar"
export ZIG
"$ZIG" cc -target arm-linux-gnueabihf -Os -Wl,--strip-all -fno-stack-protector -o "$OUT/diag-worker" "$ROOT/rust/u50_diag_worker.c" -ldl
"$ZIG" cc -target arm-linux-gnueabihf -Os -Wl,--strip-all -fno-stack-protector -o "$OUT/diag-streamer" "$ROOT/rust/u50_diag_streamer.c" -ldl -pthread
"$ZIG" cc -target arm-linux-gnueabihf.2.31 -mcpu=cortex_a7 -std=c11 -Os -fPIC -pie -s -Wall -Wextra -Werror "$ROOT/rust/efs_worker.c" -ldl -o "$OUT/efs-worker"
export DATAD_DIAG_WORKER="$OUT/diag-worker" DATAD_DIAG_STREAMER="$OUT/diag-streamer" DATAD_EFS_WORKER="$OUT/efs-worker"
export RUSTFLAGS="${RUSTFLAGS:-} -C link-self-contained=no"
export CC_armv7_unknown_linux_musleabihf="$OUT/toolchain/cc"
export AR_armv7_unknown_linux_musleabihf="$OUT/toolchain/ar"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER="$OUT/toolchain/cc"
rustup target add --toolchain 1.89.0 "$TARGET"
cd "$ROOT"
cargo +1.89.0 build --manifest-path rust/Cargo.toml --locked --release --target "$TARGET" --features web-ui --bin zwrt-datad
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/rust/target}"
cp "$TARGET_DIR/$TARGET/release/zwrt-datad" "$OUT/zwrt-datad-armv7-web"
file "$OUT/zwrt-datad-armv7-web" | grep -q 'ELF 32-bit.*ARM.*statically linked.*stripped'
cd "$OUT"
sha256sum zwrt-datad-armv7-web > SHA256SUMS
file zwrt-datad-armv7-web
cat SHA256SUMS

# Record the feature set and browser resources beside this exact binary.
python3 - "$ROOT" "$OUT" <<'PY'
import hashlib,json,subprocess,sys
from pathlib import Path
root,out=map(Path,sys.argv[1:])
binary=out/'zwrt-datad-armv7-web'
manifest=json.loads((root/'web/dist/asset-manifest.json').read_text())
assets=[(route,mime,hashlib.sha256((root/'web/dist'/name).read_bytes()).hexdigest()) for route,name,mime in manifest]
info={
 'implementation':'rust','target':'armv7-unknown-linux-musleabihf',
 'version':json.loads((root/'version.json').read_text())['datad']['version'],
 'commit':subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip(),
 'source_dirty':bool(subprocess.check_output(['git','status','--porcelain'],cwd=root,text=True).strip()),
 'asset':'zwrt-datad-armv7','sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),
 'bytes':binary.stat().st_size,'features':['web-ui'],
 'web_assets':len(assets),
 'web_assets_sha256':hashlib.sha256(json.dumps(assets,ensure_ascii=True,sort_keys=True).encode()).hexdigest(),
}
(out/'build-info.json').write_text(json.dumps(info,indent=2,sort_keys=True)+'\n')
PY
if [ "${1:-}" = --candidate ]; then
    cp "$OUT/zwrt-datad-armv7-web" "$ROOT/build/zwrt-datad-armv7-candidate"
    cp "$OUT/build-info.json" "$ROOT/build/rust-release-provenance-armv7.json"
    echo 'Staged Web-enabled candidate and matching provenance for local packaging.'
fi
