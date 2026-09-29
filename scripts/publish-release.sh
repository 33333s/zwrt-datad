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
for required in "$asset" version.json scripts/service.sh build/install-datad.sh build/update.json build/update.json.sig; do
    [[ -s "$required" ]] || { echo "Missing required asset: $required" >&2; exit 1; }
done
gh release create "v$version" "$asset" version.json \
    scripts/service.sh build/install-datad.sh build/update.json build/update.json.sig --repo 33333s/zwrt-datad \
    --target "$(git rev-parse HEAD)" --title "zwrt-datad v$version" --notes-file "$notes_file"
