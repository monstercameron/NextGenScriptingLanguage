#!/usr/bin/env python3
"""Print a release binary's size and fail if it exceeds the recorded budget.

The budget file holds {"max_bytes": N}. A null budget reports the size without
enforcing it. Python 3.9+.
"""
import argparse
import json
import sys
from pathlib import Path


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("binary", type=Path)
    parser.add_argument("budget", type=Path)
    args = parser.parse_args(argv)

    if not args.binary.is_file():
        print(f"error: {args.binary} is not a built executable", file=sys.stderr)
        return 1
    size = args.binary.stat().st_size
    budget = json.loads(args.budget.read_text(encoding="utf-8"))["max_bytes"]
    print(f"{args.binary}: {size} bytes")
    if budget is None:
        print(f"no size budget recorded in {args.budget}; size is reported, not enforced")
        return 0
    if size > budget:
        print(
            f"error: {size} bytes exceeds the {budget} byte budget in {args.budget}",
            file=sys.stderr,
        )
        return 1
    print(f"within budget: {size} <= {budget} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
