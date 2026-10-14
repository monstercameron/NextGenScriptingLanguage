#!/usr/bin/env python3
"""Build the bounded Rust WASM host and local-only browser assets. Install deps first."""
from pathlib import Path
import subprocess
import json
import os

ROOT = Path(__file__).resolve().parents[1]
subprocess.run(
    [
        os.environ.get("CARGO", "cargo"),
        "build",
        "--locked",
        "--release",
        "--target",
        "wasm32-unknown-unknown",
    ],
    cwd=ROOT / "browser",
    check=True,
)
subprocess.run(
    [os.environ.get("NPM", "npm"), "run", "build"], cwd=ROOT / "web", check=True
)

# Bundle full license texts rather than only short minifier banners.
metadata = json.loads(
    subprocess.check_output(
        [
            os.environ.get("CARGO", "cargo"),
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            "wasm32-unknown-unknown",
        ],
        cwd=ROOT / "browser",
        text=True,
    )
)
nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
selected = set()


def visit(package):
    if package in selected:
        return
    selected.add(package)
    for dependency in nodes[package]["deps"]:
        visit(dependency["pkg"])


visit(metadata["resolve"]["root"])
notices = ["Execute browser playground — third-party license texts\n"]
for package in sorted(
    metadata["packages"], key=lambda package: (package["name"], package["version"])
):
    if package["id"] not in selected:
        continue
    folder = Path(package["manifest_path"]).parent
    licenses = sorted(
        {*folder.glob("LICENSE*"), *folder.glob("COPYING*"), *folder.glob("NOTICE*")}
    )
    if package.get("license_file"):
        licenses.append(folder / package["license_file"])
    if package["name"] == "execute-browser":
        licenses = [ROOT / "LICENSE"]
    if not licenses:
        raise RuntimeError("No license text found for " + package["name"])
    notices.append(
        "\n=== "
        + package["name"]
        + " "
        + package["version"]
        + " ("
        + (package.get("license") or "see license")
        + ") ===\n"
    )
    for license_path in dict.fromkeys(licenses):
        if license_path.is_file():
            notices.append(license_path.read_text() + "\n")
for package in ["@xterm/xterm", "@xterm/addon-fit"]:
    folder = ROOT / "web/node_modules" / package
    version = json.loads((folder / "package.json").read_text())["version"]
    notices.append(
        "\n=== "
        + package
        + " "
        + version
        + " ===\n"
        + (folder / "LICENSE").read_text()
        + "\n"
    )
(ROOT / "docs/playground/THIRD_PARTY.txt").write_text("".join(notices))
