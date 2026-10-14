#!/usr/bin/env python3
"""Build the bounded Rust WASM host and local-only browser assets. Install deps first."""
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
subprocess.run(
    ["cargo", "build", "--locked", "--release", "--target", "wasm32-unknown-unknown"],
    cwd=ROOT / "browser",
    check=True,
)
subprocess.run(["npm", "run", "build"], cwd=ROOT / "web", check=True)
