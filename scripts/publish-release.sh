#!/bin/bash
# Publish all required files together; version metadata is a mandatory asset.
set -euo pipefail
cd "$(dirname "$0")/.."
notes_file=${1:?Usage: scripts/publish-release.sh RELEASE_NOTES_FILE}
version=$(python3 -c 'import json; print(json.load(open("version.json"))["datad"]["version"])')
asset=$(python3 -c 'import json; print(json.load(open("version.json"))["datad"]["asset"])')
[[ "$asset" == zwrt-datad-aarch64 ]]
[[ "$(git branch --show-current)" == main ]] || { echo 'Releases are only allowed from main' >&2; exit 1; }
[[ -z "$(git status --porcelain --untracked-files=no)" ]] || { echo 'Tracked source changes must be committed before release' >&2; exit 1; }
file "$asset" | grep -q 'statically linked.*stripped' || { echo 'Expected the stripped static release binary' >&2; exit 1; }
python3 - "$asset" "$version" "$(git rev-parse HEAD)" build/rust-release-provenance.json <<'PY'
import hashlib
import json
import sys

asset, version, commit, provenance_path = sys.argv[1:]
with open(provenance_path, encoding="utf-8") as handle:
    provenance = json.load(handle)
with open(asset, "rb") as handle:
    digest = hashlib.sha256(handle.read()).hexdigest()
expected = {
    "implementation": "rust",
    "version": version,
    "commit": commit,
    "asset": asset,
    "sha256": digest,
}
if provenance != expected:
    raise SystemExit(f"Rust release provenance mismatch: {provenance!r} != {expected!r}")
PY
python3 scripts/render-installer.py "$asset" build/install-datad.sh
sh -n build/install-datad.sh
: "${DATAD_OTA_SIGNING_KEY_FILE:?Set DATAD_OTA_SIGNING_KEY_FILE to the protected Ed25519 private key}"
python3 scripts/sign-update.py --key "$DATAD_OTA_SIGNING_KEY_FILE" --binary "$asset"
# Optional ARM32 (U50S) asset, built by scripts/build-arm32-candidate.sh from
# this same commit. It is uploaded next to the ARM64 assets and is not part of
# the signed OTA manifest (U50 has no OTA).
extra_assets=()
if [[ -n "${DATAD_ARMV7_BINARY:-}" ]]; then
    armv7_provenance=${DATAD_ARMV7_PROVENANCE:-build/rust-release-provenance-armv7.json}
    file "$DATAD_ARMV7_BINARY" | grep -q 'ELF 32-bit.*ARM.*statically linked.*stripped' || { echo 'Expected a stripped static ARMv7 binary' >&2; exit 1; }
    python3 - "$DATAD_ARMV7_BINARY" "$version" "$(git rev-parse HEAD)" "$armv7_provenance" <<'PY'
import hashlib
import json
import sys

binary, version, commit, provenance_path = sys.argv[1:]
with open(provenance_path, encoding="utf-8") as handle:
    provenance = json.load(handle)
with open(binary, "rb") as handle:
    digest = hashlib.sha256(handle.read()).hexdigest()
expected = {
    "implementation": "rust",
    "target": "armv7-unknown-linux-musleabihf",
    "version": version,
    "commit": commit,
    "asset": "zwrt-datad-armv7",
    "sha256": digest,
}
if provenance != expected:
    raise SystemExit(f"ARMv7 release provenance mismatch: {provenance!r} != {expected!r}")
PY
    cp "$DATAD_ARMV7_BINARY" build/zwrt-datad-armv7
    (cd build && sha256sum zwrt-datad-armv7 > zwrt-datad-armv7.sha256)
    # Separate signed manifest + pinned installer so the ARM64 update.json (and
    # every deployed ARM64 datad) is unaffected.
    python3 scripts/render-installer.py --profile armv7 build/zwrt-datad-armv7 build/install-datad-armv7.sh
    sh -n build/install-datad-armv7.sh
    python3 scripts/sign-update.py --profile armv7 --key "$DATAD_OTA_SIGNING_KEY_FILE"
    extra_assets=(build/zwrt-datad-armv7 build/zwrt-datad-armv7.sha256 build/install-datad-armv7.sh build/update-armv7.json build/update-armv7.json.sig)
fi
for required in "$asset" version.json scripts/service.sh build/install-datad.sh build/update.json build/update.json.sig; do
    [[ -s "$required" ]] || { echo "Missing required asset: $required" >&2; exit 1; }
done
gh release create "v$version" "$asset" version.json \
    scripts/service.sh build/install-datad.sh build/update.json build/update.json.sig ${extra_assets[@]+"${extra_assets[@]}"} --repo 33333s/zwrt-datad \
    --target "$(git rev-parse HEAD)" --title "zwrt-datad v$version" --notes-file "$notes_file"
