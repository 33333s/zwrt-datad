#!/usr/bin/env python3
"""Build the standalone, pinned installer from the release's exact inputs."""
import argparse
import hashlib
import json
import re
from pathlib import Path

root = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser()
parser.add_argument("binary", type=Path)
parser.add_argument("output", type=Path)
parser.add_argument("--url", default="https://pan.ericsfj.com/d/github%20releases/zwrt-datad/zwrt-datad-aarch64?sign=ZclbVT-Ki4a-_Fr6FG57o0JE15dxKYdheBOae8yWr_g=:0")
args = parser.parse_args()
manifest_bytes = (root / "version.json").read_bytes()
document = json.loads(manifest_bytes)
manifest = document["datad"]
if document.get("schema") != 1 or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", manifest["version"]) or manifest["asset"] != "zwrt-datad-aarch64":
    parser.error("invalid release version/schema/asset name")
binary = args.binary.read_bytes()
if not (binary[:6] == b"\x7fELF\x02\x01" and binary[18:20] == b"\xb7\x00"):
    parser.error("expected a little-endian ELF64 AArch64 release binary")
if not args.url.startswith("https://") or any(c in args.url for c in "'\n\r"):
    parser.error("expected an HTTPS download URL without shell delimiters")
values = {
    "VERSION": manifest["version"],
    "SHA256": hashlib.sha256(binary).hexdigest(),
    "DOWNLOAD_URL": args.url,
    "SERVICE": (root / "scripts/service.sh").read_text(),
    "MANIFEST": manifest_bytes.decode(),
    "LICENSE": (root / "OPENSSL-LICENSE.txt").read_text(),
    "RC_AWK": (root / "scripts/rc-local-datad.awk").read_text(),
}
text = (root / "scripts/install.sh.in").read_text()
for key, value in values.items():
    marker = "@@" + key + "@@"
    if text.count(marker) != 1:
        parser.error("expected one template marker " + marker)
    text = text.replace(marker, value.rstrip("\n"))
args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_text(text)
args.output.chmod(0o755)
print(f"Installer v{manifest['version']}: {args.output} (binary SHA-256 {values['SHA256']})")
