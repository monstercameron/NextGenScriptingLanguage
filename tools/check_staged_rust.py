#!/usr/bin/env python3
"""Check the Git index's owned Rust sources using the same Rust tools as CI."""
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tempfile


def rust_change(path):
    path = PurePosixPath(path)
    return (
        str(path)
        in {
            "Cargo.toml",
            "Cargo.lock",
            "browser/Cargo.toml",
            "browser/Cargo.lock",
            ".cargo/config.toml",
            "rustfmt.toml",
            ".rustfmt.toml",
            "clippy.toml",
            ".clippy.toml",
            "browser/rustfmt.toml",
            "browser/.rustfmt.toml",
            "browser/clippy.toml",
            "browser/.clippy.toml",
        }
        or path.suffix == ".rs"
        and path.parts[0] in {"src", "browser", "examples", "tests"}
    )


def run(command, root, env):
    print("Rust staged check: " + " ".join(command), flush=True)
    subprocess.run(command, cwd=root, env=env, check=True)


def main():
    root = Path(
        subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True
        ).strip()
    )
    changed = (
        subprocess.check_output(
            ["git", "diff", "--cached", "--name-only", "-z", "--diff-filter=ACDMRTUXB"],
            cwd=root,
        )
        .decode("utf-8", errors="surrogateescape")
        .split("\0")
    )
    if not any(rust_change(path) for path in changed if path):
        return 0

    cargo = os.environ.get("CARGO", "cargo")
    rustfmt = os.environ.get("RUSTFMT", "rustfmt")
    for label, executable in [("CARGO", cargo), ("RUSTFMT", rustfmt)]:
        if shutil.which(executable) is None:
            raise SystemExit(
                f"Rust staged check: missing {executable!r}; install Rust with rustfmt and Clippy, or set {label} to its executable path."
            )
    env = os.environ.copy()
    # Reuse build output across snapshots, while keeping it outside the index.
    target = Path(env.get("CARGO_TARGET_DIR", root / "target" / "pre-commit"))
    env["CARGO_TARGET_DIR"] = str(target if target.is_absolute() else root / target)
    with tempfile.TemporaryDirectory(prefix="execute-staged-rust-") as temporary:
        snapshot = Path(temporary)
        # checkout-index reads the index (including an alternate GIT_INDEX_FILE),
        # preserves modes and paths, and never reads unstaged source contents.
        subprocess.run(
            ["git", "checkout-index", "--all", f"--prefix={snapshot}{os.sep}"],
            cwd=root,
            check=True,
        )
        run([cargo, "fmt", "--check"], snapshot, env)
        run(
            [cargo, "fmt", "--manifest-path", "browser/Cargo.toml", "--check"],
            snapshot,
            env,
        )
        run(
            [rustfmt, "--edition", "2021", "--check", "src/runtime/shared.rs"],
            snapshot,
            env,
        )
        # Offline prevents a commit from fetching dependencies. A new clone must
        # install the documented toolchain/target and fetch locked dependencies once.
        run(
            [
                cargo,
                "clippy",
                "--offline",
                "--locked",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
            snapshot,
            env,
        )
        run(
            [
                cargo,
                "clippy",
                "--manifest-path",
                "browser/Cargo.toml",
                "--offline",
                "--locked",
                "--target",
                "wasm32-unknown-unknown",
                "--",
                "-D",
                "warnings",
            ],
            snapshot,
            env,
        )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except subprocess.CalledProcessError as error:
        print(
            "Rust staged check failed. Fix and stage the affected content; the hook does not modify files. Browser checks require the wasm32-unknown-unknown target.",
            file=sys.stderr,
        )
        sys.exit(error.returncode)
