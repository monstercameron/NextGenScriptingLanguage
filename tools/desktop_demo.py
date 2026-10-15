#!/usr/bin/env python3
"""Run or statically validate the backend-neutral desktop graph example."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_EXECUTE = ROOT / "target" / "debug" / "execute"
REMOTE_APPLETS = {
    "desktop.clock_remote": "descriptors/desktop.clock_remote.descriptor.json",
    "desktop.fixture_sync": "descriptors/desktop.fixture_sync.descriptor.json",
}


def load_fixture_server():
    path = ROOT / "examples" / "desktop" / "fixture_server.py"
    spec = importlib.util.spec_from_file_location("execute_desktop_fixture", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load the local remote fixture")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.FixtureServer


def write_run_inputs(directory: Path, producer: str, renderer: str, port: int) -> tuple[Path, Path]:
    manifests = directory / "manifests"
    shutil.copytree(ROOT / "manifests", manifests)
    for applet, descriptor_name in REMOTE_APPLETS.items():
        manifest_path = manifests / f"{applet}.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        descriptor_path = manifests / descriptor_name
        descriptor_hash = hashlib.sha256(descriptor_path.read_bytes()).hexdigest()
        remote = manifest["contract"]["remote"]
        remote["endpoint"] = f"http://127.0.0.1:{port}"
        remote["descriptor_sha256"] = descriptor_hash
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")

    template_path = ROOT / "examples" / "desktop" / "clock.program.in"
    source = template_path.read_text(encoding="utf-8")
    replacements = {
        "{{CLOCK_SOURCE}}": f"desktop.clock_{producer}",
        "{{RENDERER}}": renderer,
    }
    for marker, replacement in replacements.items():
        expected_occurrences = 2 if marker == "{{RENDERER}}" else 1
        if source.count(marker) != expected_occurrences:
            raise RuntimeError(f"graph template must contain expected marker {marker!r}")
        source = source.replace(marker, replacement)
    program = directory / "clock.program"
    program.write_text(source, encoding="utf-8")
    return manifests, program


def run_validation(executable: Path, manifests: Path, program: Path) -> int:
    completed = subprocess.run(
        [str(executable), "check", str(program), "--manifest-dir", str(manifests)],
        cwd=ROOT,
        check=False,
    )
    return completed.returncode


def run_demo(executable: Path, manifests: Path, program: Path, fixture) -> int:
    server_thread = threading.Thread(target=fixture.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True)
    server_thread.start()
    process: subprocess.Popen[bytes] | None = None
    interrupted = False
    try:
        process = subprocess.Popen(
            [str(executable), "run", str(program), "--manifest-dir", str(manifests)],
            cwd=ROOT,
            start_new_session=True,
        )
        try:
            return_code = process.wait()
        except KeyboardInterrupt:
            interrupted = True
            print("Stopping graph and its owned renderer/workers…", file=sys.stderr)
            try:
                os.killpg(process.pid, signal.SIGINT)
            except ProcessLookupError:
                pass
            try:
                return_code = process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                return_code = process.wait()
    finally:
        fixture.shutdown()
        fixture.server_close()
        server_thread.join(timeout=3)
    print(f"Local fixture accepted {fixture.sync_count} sync request(s).", file=sys.stderr)
    return 0 if interrupted else return_code


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--producer", choices=("local", "remote"), required=True)
    parser.add_argument("--renderer", choices=("tui", "quickshell"), required=True)
    parser.add_argument("--execute", type=Path, default=DEFAULT_EXECUTE)
    parser.add_argument("--validate-only", action="store_true", help="parse and type-check without starting a server or renderer")
    args = parser.parse_args()

    executable = args.execute.expanduser().resolve()
    if not executable.is_file() or not os.access(executable, os.X_OK):
        parser.error(f"execute binary is missing or not executable: {executable}")

    with tempfile.TemporaryDirectory(prefix="execute-desktop-demo-") as raw_directory:
        directory = Path(raw_directory)
        os.chmod(directory, 0o700)
        if args.validate_only:
            manifests, program = write_run_inputs(directory, args.producer, args.renderer, 18765)
            return run_validation(executable, manifests, program)

        fixture_class = load_fixture_server()
        fixture = fixture_class(("127.0.0.1", 0))
        port = fixture.server_address[1]
        manifests, program = write_run_inputs(directory, args.producer, args.renderer, port)
        print(f"Using {args.producer} clock source and {args.renderer} renderer; Ctrl-C stops the graph.", file=sys.stderr)
        return run_demo(executable, manifests, program, fixture)


if __name__ == "__main__":
    raise SystemExit(main())
