#!/usr/bin/env python3
"""Enable this repository's tracked hooks without changing global Git settings."""
import subprocess
from pathlib import Path


def main():
    root = Path(
        subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True
        ).strip()
    )
    hook = root / ".githooks" / "pre-commit"
    if not hook.is_file():
        raise SystemExit(f"Missing tracked hook: {hook}")
    if not hook.stat().st_mode & 0o111:
        raise SystemExit(f"Hook is not executable: {hook}")
    subprocess.run(
        ["git", "config", "--local", "core.hooksPath", ".githooks"],
        cwd=root,
        check=True,
    )
    print("Enabled .githooks for this repository and its worktrees.")


if __name__ == "__main__":
    main()
