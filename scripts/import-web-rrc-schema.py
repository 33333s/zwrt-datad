#!/usr/bin/env python3
"""Import only the reproducible ASN.1 graph and its license from a UFIPanel export."""
import gzip
import hashlib
from pathlib import Path
import re
import sys
import zipfile

root = Path(__file__).resolve().parents[1]
with zipfile.ZipFile(sys.argv[1]) as bundle:
    suffix = "data/zwrt/signal/RrcSchemaData.kt"
    names = [name for name in bundle.namelist() if name.endswith(suffix)]
    if len(names) != 1:
        raise SystemExit("Expected one RrcSchemaData.kt")
    source = bundle.read(names[0]).decode("utf-8")
    parts = re.findall(r'private fun part(\d+)\(\) = "([A-Za-z0-9+/=]+)"', source)
    import base64
    graph = gzip.decompress(base64.b64decode("".join(value for _, value in sorted(parts, key=lambda p: int(p[0])))))
    expected = re.search(r"SHA-256: ([0-9a-f]{64})", source)[1]
    if hashlib.sha256(graph).hexdigest() != expected:
        raise SystemExit("ASN.1 graph digest mismatch")
    output = root / "web"
    (output / "licenses").mkdir(parents=True, exist_ok=True)
    (output / "rrc-schema.json.gz").write_bytes(gzip.compress(graph, mtime=0))
    for name in ("RRC_SCHEMA_LICENSE.txt", "RRC_SCHEMA_NOTICE.txt"):
        origin = names[0].rsplit("/", 1)[0] + "/" + name
        (output / "licenses" / name).write_bytes(bundle.read(origin))
    print(f"Imported RRC schema: {expected}; {len(graph)} bytes uncompressed")
