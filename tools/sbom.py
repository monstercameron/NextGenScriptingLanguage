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


def _posix_relative(directory):
    """`directory` as a repo-relative POSIX path when it lies under ROOT or a vendor/ tree."""
    text = str(directory).replace("\\", "/").rstrip("/")
    root = str(ROOT).replace("\\", "/").rstrip("/")
    if text.lower() == root.lower():
        return "."
    if text.lower().startswith(root.lower() + "/"):
        return text[len(root) + 1:]
    parts = text.split("/")
    if "vendor" in parts:
        return "/".join(parts[parts.index("vendor"):])
    return text.lstrip("/")


def patch_source(package, workspace_members=()):
    """Repo-relative patch path for a path-sourced (vendored/patched) package, else None.

    Cargo records a `[patch.crates-io]` path replacement with `source: null` and a
    `path+file://...` id; registry and git packages carry a non-empty `source`.
    Workspace members are first-party, not vendored. Returns "" when the package is
    path-sourced but no path can be derived from its metadata."""
    if package.get("id") in set(workspace_members or ()):
        return None
    source = package.get("source")
    if isinstance(source, str) and source and not source.startswith("path+"):
        return None
    manifest = package.get("manifest_path")
    if manifest and not (isinstance(source, str) and source):
        return _posix_relative(str(Path(str(manifest)).parent))
    match = re.search(r"path\+file://([^)#]+)", str(package.get("id", "")))
    if match:
        from urllib.parse import unquote

        return _posix_relative(unquote(match[1]).rstrip("/"))
    if isinstance(source, str) and source.startswith("path+"):
        return ""
    if "source" in package and package["source"] is None:
        return ""
    return None


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


def directory_sha256(directory):
    """Hash a directory's complete file tree, including relative paths."""
    digest = hashlib.sha256()
    root = Path(directory)
    for path in sorted(item for item in root.rglob("*") if item.is_file()):
        relative = path.relative_to(root).as_posix().encode("utf-8")
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        content = path.read_bytes()
        digest.update(len(content).to_bytes(8, "big"))
        digest.update(content)
    return digest.hexdigest()


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
        if package["name"] == "iana-time-zone" and package["version"] == "0.1.65":
            digest = directory_sha256(ROOT / "vendor/iana-time-zone")
        if digest:
            component["hashes"] = [{"alg": "SHA-256", "content": digest}]
        patched_from = patch_source(package, metadata.get("workspace_members") or ())
        if patched_from is not None:
            properties = [
                {"name": "vendored", "value": "true"},
                {"name": "patched", "value": "true"},
            ]
            if patched_from:
                properties.append({"name": "patch-source", "value": patched_from})
            component["properties"] = properties
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
    workspace_members = set()
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
        workspace_members.update(document.get("workspace_members") or ())
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
    return {
        "packages": list(packages.values()),
        "workspace_members": sorted(workspace_members),
        "resolve": {"root": root_id, "nodes": list(nodes.values())},
    }


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
