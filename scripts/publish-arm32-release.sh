#!/bin/bash
# Publish an ARM32 (ZTE U50S) release from the arm32 branch.
#
# ARM32 releases have their own tag namespace (tag arm32-vX.Y.Z, title
# "zwrt-datad-arm32 vX.Y.Z"), so version numbers are independent of mainline and
# can never collide with its vX.Y.Z tags. They are still never marked "latest":
# GitHub's latest release is chosen by date, and the newest non-prerelease is
# what ARM64 devices read update.json from. The ARM32 update channel is the
# rolling release `arm32-latest`, refreshed by this script, so U50 devices
# always have one stable URL.
set -euo pipefail
cd "$(dirname "$0")/.."
notes_file=${1:?Usage: scripts/publish-arm32-release.sh RELEASE_NOTES_FILE}
: "${DATAD_ARMV7_BINARY:?Set DATAD_ARMV7_BINARY to the ARMv7 binary built by scripts/build-arm32-candidate.sh}"
: "${DATAD_OTA_SIGNING_KEY_FILE:?Set DATAD_OTA_SIGNING_KEY_FILE to the protected Ed25519 private key}"
repo=33333s/zwrt-datad
channel=arm32-latest
version=$(python3 -c 'import json; print(json.load(open("version.json"))["datad"]["version"])')
[[ "$(git branch --show-current)" == arm32 ]] || { echo 'ARM32 releases are only allowed from the arm32 branch' >&2; exit 1; }
[[ -z "$(git status --porcelain --untracked-files=no)" ]] || { echo 'Tracked source changes must be committed before release' >&2; exit 1; }
tag="arm32-v$version"
if git ls-remote --exit-code --tags "https://github.com/$repo.git" "refs/tags/$tag" >/dev/null 2>&1; then
    echo "Tag $tag already exists; bump version.json first" >&2
    exit 1
fi
provenance=${DATAD_ARMV7_PROVENANCE:-build/rust-release-provenance-armv7.json}
file "$DATAD_ARMV7_BINARY" | grep -q 'ELF 32-bit.*ARM.*statically linked.*stripped' || { echo 'Expected a stripped static ARMv7 binary' >&2; exit 1; }
python3 - "$DATAD_ARMV7_BINARY" "$version" "$(git rev-parse HEAD)" "$provenance" <<'PY'
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
mkdir -p build
cp "$DATAD_ARMV7_BINARY" build/zwrt-datad-armv7
(cd build && sha256sum zwrt-datad-armv7 > zwrt-datad-armv7.sha256)
python3 scripts/render-installer.py --profile armv7 build/zwrt-datad-armv7 build/install-datad-armv7.sh
sh -n build/install-datad-armv7.sh
python3 scripts/sign-update.py --profile armv7 --key "$DATAD_OTA_SIGNING_KEY_FILE"
assets=(build/zwrt-datad-armv7 build/zwrt-datad-armv7.sha256 build/install-datad-armv7.sh build/update-armv7.json build/update-armv7.json.sig version.json)
for required in "${assets[@]}"; do
    [[ -s "$required" ]] || { echo "Missing required asset: $required" >&2; exit 1; }
done
gh release create "$tag" "${assets[@]}" --repo "$repo" \
    --target "$(git rev-parse HEAD)" --title "zwrt-datad-arm32 v$version" --notes-file "$notes_file" --latest=false
# Rolling channel: binary and installer first, the signed manifest last, so a
# device that sees the new manifest can already download what it names.
if ! gh release view "$channel" --repo "$repo" >/dev/null 2>&1; then
    gh release create "$channel" --repo "$repo" --target "$(git rev-parse HEAD)" --prerelease --latest=false \
        --title "zwrt-datad ARM32 update channel" --notes "Rolling ARM32 (U50S) update channel; do not use directly."
fi
gh release upload "$channel" build/zwrt-datad-armv7 build/install-datad-armv7.sh --repo "$repo" --clobber
gh release upload "$channel" build/update-armv7.json.sig build/update-armv7.json --repo "$repo" --clobber
gh release edit "$channel" --repo "$repo" --prerelease --latest=false \
    --notes "Rolling ARM32 (U50S) update channel; currently v$version ($(git rev-parse --short HEAD)). Versioned releases: https://github.com/$repo/releases/tag/$tag" >/dev/null
echo "https://github.com/$repo/releases/tag/$tag"
