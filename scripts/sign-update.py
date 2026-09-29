#!/usr/bin/env python3
"""Create the canonical OTA manifest and detached Ed25519 signature."""
import argparse
import base64
import hashlib
import json
from datetime import datetime, timezone
from pathlib import Path

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

root = Path(__file__).resolve().parent.parent
p = argparse.ArgumentParser()
p.add_argument("--key", type=Path, required=True)
p.add_argument("--binary", type=Path, default=root / "zwrt-datad-aarch64")
p.add_argument("--installer", type=Path, default=root / "build/install-datad.sh")
p.add_argument("--output", type=Path, default=root / "build/update.json")
args = p.parse_args()
release = json.loads((root / "version.json").read_text())["datad"]
def artifact(path: Path):
    data = path.read_bytes()
    return {"name": path.name, "size": len(data), "sha256": hashlib.sha256(data).hexdigest()}
manifest = {
    "schema": 1,
    "version": release["version"],
    "tag": "v" + release["version"],
    "published_at": datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "artifacts": {"installer": artifact(args.installer), "binary": artifact(args.binary)},
}
raw = (json.dumps(manifest, ensure_ascii=False, separators=(",", ":"), sort_keys=True) + "\n").encode()
key = serialization.load_pem_private_key(args.key.read_bytes(), password=None)
if not isinstance(key, Ed25519PrivateKey):
    p.error("signing key is not Ed25519")
args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_bytes(raw)
args.output.with_suffix(args.output.suffix + ".sig").write_text(base64.b64encode(key.sign(raw)).decode() + "\n")
print(f"Signed OTA manifest for v{release['version']}")
