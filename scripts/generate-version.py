#!/usr/bin/env python3
"""Generate compile-time metadata from version.json, the only version source."""
import argparse
import json
import os
import re
import tempfile
from pathlib import Path


def main():
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=root / "version.json")
    parser.add_argument("--output", type=Path, default=root / "include/datad_version_generated.h")
    args = parser.parse_args()
    try:
        document = json.loads(args.manifest.read_text())
        version = document["datad"]["version"]
        if document.get("schema") != 1 or not isinstance(version, str) or not re.fullmatch(
            r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", version
        ) or len(version) > 63:
            raise ValueError("expected schema 1 and a numeric x.y.z datad version")
    except (OSError, ValueError, TypeError, KeyError) as exc:
        parser.error(f"invalid version manifest: {exc}")
    content = (
        "/* Generated from version.json by scripts/generate-version.py. Do not edit. */\n"
        "#ifndef ZWRT_DATAD_VERSION_GENERATED_H\n"
        "#define ZWRT_DATAD_VERSION_GENERATED_H\n"
        f"#define ZWRT_DATAD_VERSION {json.dumps(version)}\n"
        "#endif\n"
    )
    if args.output.exists() and args.output.read_text() == content:
        return
    args.output.parent.mkdir(parents=True, exist_ok=True)
    pending = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", dir=args.output.parent, delete=False) as out:
            pending = Path(out.name)
            out.write(content)
        pending.chmod(0o644)
        os.replace(pending, args.output)
    finally:
        if pending is not None:
            pending.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
