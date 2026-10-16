#!/usr/bin/env python3
"""Create an opt-in manifest-catalog v1 beside its source manifests.

The deterministic packer accepts only JSON whitespace and finite JSON
constants, and rejects duplicate object keys at every nesting level.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


FORMAT = "execute.manifest-catalog"
VERSION = 1
JSON_WHITESPACE = " \t\r\n"


def reject_nonfinite_constant(token: str) -> None:
    raise ValueError(f"non-finite JSON constant is not supported: {token}")


def reject_duplicate_object_keys(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON object key: {key!r}")
        result[key] = value
    return result


def pack_directory(manifest_dir: Path, output_path: Path) -> int:
    if manifest_dir.is_symlink():
        raise ValueError("manifest directory may not be a symlink")
    root = manifest_dir.resolve(strict=True)
    if not root.is_dir():
        raise ValueError(f"manifest source is not a directory: {manifest_dir}")

    output_path = Path(output_path)
    if output_path.suffix.lower() == ".json":
        raise ValueError(
            "catalog output must not use .json; Registry::load(directory) reads those files"
        )
    if output_path.is_symlink():
        raise ValueError("catalog output may not be a symlink")
    output_parent = output_path.parent.resolve(strict=True)
    if output_parent != root:
        raise ValueError(
            "catalog output must be in the source manifest directory to preserve module roots"
        )

    sources = sorted(root.glob("*.json"), key=lambda source: source.name)
    if not sources:
        raise ValueError(f"no JSON manifests found in {root}")

    raw_manifests: list[str] = []
    for source in sources:
        if source.is_symlink():
            raise ValueError(f"symlink manifest origins are unsupported: {source.name}")
        if not source.is_file():
            raise ValueError(f"manifest entry is not a regular file: {source.name}")
        raw = source.read_text(encoding="utf-8").strip(JSON_WHITESPACE)
        value = json.loads(
            raw,
            parse_constant=reject_nonfinite_constant,
            object_pairs_hook=reject_duplicate_object_keys,
        )
        if not isinstance(value, dict):
            raise ValueError(f"manifest must be a JSON object: {source.name}")
        raw_manifests.append(raw)

    destination = output_parent / output_path.name
    if destination.exists() and any(destination.samefile(source) for source in sources):
        raise ValueError("catalog output must not alias a source manifest")
    envelope = (
        f'{{"format":{json.dumps(FORMAT)},"version":{VERSION},"manifests":['
        + ",".join(raw_manifests)
        + "]}\n"
    )
    with destination.open("w", encoding="utf-8", newline="\n") as output:
        output.write(envelope)
    return len(sources)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest_dir", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        count = pack_directory(args.manifest_dir, args.output)
    except (OSError, ValueError) as error:
        print(f"pack_manifest_catalog: {error}", file=sys.stderr)
        return 2
    print(f"packed {count} manifests into {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
