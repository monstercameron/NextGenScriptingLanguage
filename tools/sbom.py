#!/usr/bin/env python3
"""Write a deterministic CycloneDX 1.5 SBOM for the locked Rust dependency graph.

Components come from `cargo metadata --locked`; registry checksums come from Cargo.lock.
The timestamp is SOURCE_DATE_EPOCH and the serial number is derived from the content, so
the same inputs always produce the same bytes. Python 3.9+."""
import argparse
import hashlib
import json
import os
import re
import subprocess
import uuid
from datetime import datetime, timezone
from pathlib import Path

from release import ROOT, TARGETS, version

RELEASE_TARGETS = tuple(TARGETS)

SPEC_VERSION = "1.5"
TOOL = {"vendor": "Execute maintainers", "name": "tools/sbom.py", "version": "1"}


def purl(name, value):
    return "pkg:cargo/" + name + "@" + value


def node_edges(node):
    """Return Cargo resolve edges across current and older metadata schemas."""
    if "deps" in node:
        return node["deps"]
    return [
        {"pkg": package_id, "dep_kinds": [{"kind": None}]}
        for package_id in node.get("dependencies", [])
    ]


def lock_checksums(lock_text):
    """Map (name, version) to the registry checksum recorded in Cargo.lock."""
    checksums = {}
    for block in lock_text.split("[[package]]")[1:]:
        name = re.search(r'^name = "([^"]+)"$', block, re.MULTILINE)
        found = re.search(r'^version = "([^"]+)"$', block, re.MULTILINE)
        checksum = re.search(r'^checksum = "([0-9a-f]{64})"$', block, re.MULTILINE)
        if name and found and checksum:
            checksums[(name[1], found[1])] = checksum[1]
    return checksums


def build_sbom(metadata, lock_text, value, epoch, license_expression="MIT"):
    """The SBOM as a dict. `metadata` is `cargo metadata` output for the locked graph."""
    checksums = lock_checksums(lock_text)
    by_id = {package["id"]: package for package in metadata["packages"]}
    root_id = metadata["resolve"]["root"]
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    refs = {}
    for node in nodes.values():
        package = by_id[node["id"]]
        refs[node["id"]] = purl(package["name"], package["version"])

    # Include only the root binary's normal/build dependency closure, never dev edges.
    included = set()
    pending = [root_id]
    while pending:
        current = pending.pop()
        if current in included:
            continue
        included.add(current)
        for dependency in node_edges(nodes[current]):
            kinds = dependency.get("dep_kinds") or [{"kind": None}]
            if any(kind.get("kind") in (None, "normal", "build") for kind in kinds):
                pending.append(dependency["pkg"])

    components = []
    for node in nodes.values():
        if node["id"] not in included:
            continue
        if node["id"] == root_id:
            continue
        package = by_id[node["id"]]
        component = {
            "type": "library",
            "bom-ref": refs[node["id"]],
            "name": package["name"],
            "version": package["version"],
            "purl": refs[node["id"]],
        }
        if package.get("license"):
            component["licenses"] = [{"expression": normalize_license(package["license"])}]
        digest = checksums.get((package["name"], package["version"]))
        if digest:
            component["hashes"] = [{"alg": "SHA-256", "content": digest}]
        components.append(component)
    components.sort(key=lambda component: component["bom-ref"])

    dependencies = []
    for node in nodes.values():
        if node["id"] not in included:
            continue
        dependencies_for_node = {
            dependency["pkg"]
            for dependency in node_edges(node)
            if dependency["pkg"] in included
            and any(
                kind.get("kind") in (None, "normal", "build")
                for kind in (dependency.get("dep_kinds") or [{"kind": None}])
            )
        }
        depends_on = sorted({refs[dep] for dep in dependencies_for_node if dep in refs})
        dependencies.append({"ref": refs[node["id"]], "dependsOn": depends_on})
    dependencies.sort(key=lambda entry: entry["ref"])

    root_ref = refs[root_id]
    root = {
        "type": "application",
        "bom-ref": root_ref,
        "name": "execute",
        "version": value,
        "purl": root_ref,
        "licenses": [{"expression": normalize_license(license_expression)}],
    }
    body = {"components": components, "dependencies": dependencies, "root": root}
    digest = hashlib.sha256(
        json.dumps(body, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    timestamp = datetime.fromtimestamp(epoch, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    return {
        "bomFormat": "CycloneDX",
        "specVersion": SPEC_VERSION,
        "serialNumber": "urn:uuid:" + str(uuid.uuid5(uuid.NAMESPACE_URL, digest)),
        "version": 1,
        "metadata": {"timestamp": timestamp, "tools": [TOOL], "component": root},
        "components": components,
        "dependencies": dependencies,
    }


def normalize_license(value):
    """Translate Cargo's legacy slash-separated dual license notation to SPDX OR."""
    return re.sub(r"\s*/\s*", " OR ", value.strip())


def source_date_epoch(root=ROOT):
    value = os.environ.get("SOURCE_DATE_EPOCH")
    if value:
        return int(value)
    return int(
        subprocess.check_output(["git", "show", "-s", "--format=%ct"], cwd=root, text=True).strip()
    )


def release_metadata():
    """Union the full-feature resolve graph across every shipped target."""
    packages = {}
    nodes = {}
    root_id = None
    for target in RELEASE_TARGETS:
        document = json.loads(
            subprocess.check_output(
                [
                    "cargo", "metadata", "--locked", "--features", "full",
                    "--filter-platform", target, "--format-version", "1",
                ],
                cwd=ROOT,
                text=True,
            )
        )
        root_id = root_id or document["resolve"]["root"]
        packages.update({package["id"]: package for package in document["packages"]})
        for node in document["resolve"]["nodes"]:
            if node["id"] not in nodes:
                nodes[node["id"]] = node
            else:
                existing = nodes[node["id"]]
                dependencies = {
                    json.dumps(dependency, sort_keys=True): dependency
                    for dependency in node_edges(existing) + node_edges(node)
                }
                existing["deps"] = [dependencies[key] for key in sorted(dependencies)]
    return {"packages": list(packages.values()), "resolve": {"root": root_id, "nodes": list(nodes.values())}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    metadata = release_metadata()
    value = version()
    sbom = build_sbom(
        metadata,
        (ROOT / "Cargo.lock").read_text(),
        value,
        source_date_epoch(),
    )
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(sbom, indent=2, sort_keys=True) + "\n")
    print("Wrote " + str(args.out) + " with " + str(len(sbom["components"])) + " components")


if __name__ == "__main__":
    main()
