#!/usr/bin/env python3
"""Single source version and immutable release asset contracts. Python 3.9+."""
import argparse
import hashlib
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SEMVER = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?\Z"
)
TARGETS = {
    "x86_64-unknown-linux-musl": ("linux-x64", "amd64", "x86_64"),
    "aarch64-unknown-linux-musl": ("linux-arm64", "arm64", "aarch64"),
    "x86_64-apple-darwin": ("macos-x64", None, None),
    "aarch64-apple-darwin": ("macos-arm64", None, None),
    "x86_64-pc-windows-msvc": ("windows-x64", None, None),
    "aarch64-pc-windows-msvc": ("windows-arm64", None, None),
}


def validate(version):
    match = SEMVER.fullmatch(version)
    if not match:
        raise ValueError(
            "expected MAJOR.MINOR.PATCH[-PRERELEASE]; release build metadata is not supported"
        )
    if match[4] and any(
        p.isdigit() and len(p) > 1 and p[0] == "0" for p in match[4].split(".")
    ):
        raise ValueError("numeric prerelease identifiers cannot have leading zeros")
    return version


def version(root=ROOT):
    cargo = re.search(
        r'(?ms)^\[package\]\n.*?^version = "([^"]+)"$',
        (root / "Cargo.toml").read_text(),
    )
    lock = re.search(
        r'(?m)^name = "execute"\nversion = "([^"]+)"$',
        (root / "Cargo.lock").read_text(),
    )
    if not cargo or not lock or cargo[1] != lock[1]:
        raise ValueError("Cargo.toml and Cargo.lock Execute versions disagree")
    browser_lock = root / "browser/Cargo.lock"
    if browser_lock.exists():
        browser = re.search(
            r'(?m)^name = "execute"\nversion = "([^"]+)"$', browser_lock.read_text()
        )
        if not browser or browser[1] != cargo[1]:
            raise ValueError(
                "browser/Cargo.lock Execute version disagrees with Cargo.toml"
            )
    fuzz_lock = root / "fuzz/Cargo.lock"
    if fuzz_lock.exists():
        fuzz = re.search(
            r'(?m)^name = "execute"\nversion = "([^"]+)"$', fuzz_lock.read_text()
        )
        if not fuzz or fuzz[1] != cargo[1]:
            raise ValueError("fuzz/Cargo.lock Execute version disagrees with Cargo.toml")
    return validate(cargo[1])


def set_version(value, root=ROOT):
    validate(value)
    # Validate both preimages before writing either file; dependency versions stay intact.
    version(root)
    edits = [
        ("Cargo.toml", r'(?ms)(^\[package\]\n.*?^version = ")[^"]+("$)'),
        ("Cargo.lock", r'(?m)(^name = "execute"\nversion = ")[^"]+("$)'),
    ]
    if (root / "browser/Cargo.lock").exists():
        edits.append(
            ("browser/Cargo.lock", r'(?m)(^name = "execute"\nversion = ")[^"]+("$)')
        )
    if (root / "fuzz/Cargo.lock").exists():
        edits.append(("fuzz/Cargo.lock", r'(?m)(^name = "execute"\nversion = ")[^"]+("$)'))
    replacements = []
    for filename, pattern in edits:
        p = root / filename
        text, count = re.subn(
            pattern, lambda m: m[1] + value + m[2], p.read_text(), count=1
        )
        if count != 1:
            raise ValueError("cannot locate canonical version in " + filename)
        replacements.append((p, text))
    for p, text in replacements:
        p.write_text(text)


def package_version(value):
    return validate(value).replace("-", "~", 1)


def bumped_version(value, part):
    """Return the next release version; a prerelease patch candidate releases as-is."""
    validate(value)
    if part not in ("major", "minor", "patch"):
        raise ValueError("part must be major, minor, or patch")
    core, separator, _prerelease = value.partition("-")
    if separator and part == "patch":
        return core
    parts = [int(piece) for piece in core.split(".")]
    index = ("major", "minor", "patch").index(part)
    parts[index] += 1
    parts[index + 1 :] = [0] * (2 - index)
    return ".".join(map(str, parts))


def expected_assets(value):
    validate(value)
    names = set()
    for target, (label, deb, rpm) in TARGETS.items():
        stem = "execute-" + value + "-" + label
        names.add(stem + (".zip" if "windows" in label else ".tar.gz"))
        if deb:
            names.add("execute_" + value + "_" + deb + ".deb")
            names.add("execute-" + value + "-1." + rpm + ".rpm")
            names.add(stem + ".flatpak")
    # CycloneDX SBOM of the locked dependency graph (tools/sbom.py).
    names.add("execute-" + value + ".cdx.json")
    return names


# SHA256SUMS covers the expected assets; its Sigstore bundle signs SHA256SUMS in the
# publish job. Both are published with the assets and are checked against the same set.
CHECKSUMS = "SHA256SUMS"
SIGNATURE = "SHA256SUMS.sigstore.json"


def publication_assets(value):
    return expected_assets(value) | {CHECKSUMS, SIGNATURE}


def verify_assets(folder, value):
    actual = {p.name for p in folder.iterdir() if p.is_file()}
    expected = expected_assets(value)
    if actual != expected:
        raise ValueError(
            "asset set mismatch; missing="
            + repr(sorted(expected - actual))
            + "; unexpected="
            + repr(sorted(actual - expected))
        )
    for p in folder.iterdir():
        if p.stat().st_size < 100:
            raise ValueError("empty or truncated package: " + p.name)
    checksums = "".join(
        hashlib.sha256((folder / name).read_bytes()).hexdigest() + "  " + name + "\n"
        for name in sorted(expected)
    )
    (folder / "SHA256SUMS").write_text(checksums)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("version")
    sub.add_parser("package-version")
    tag = sub.add_parser("verify-tag")
    tag.add_argument("tag")
    setting = sub.add_parser("set-version")
    setting.add_argument("version")
    bump = sub.add_parser("bump")
    bump.add_argument("part", choices=["major", "minor", "patch"])
    assets = sub.add_parser("verify-assets")
    assets.add_argument("directory", type=Path)
    args = parser.parse_args()
    value = version()
    if args.command == "version":
        print(value)
    elif args.command == "package-version":
        print(package_version(value))
    elif args.command == "verify-tag":
        if args.tag != "v" + value:
            parser.error("tag must be exactly v" + value)
        print(value)
    elif args.command == "set-version":
        set_version(args.version)
        print(args.version)
    elif args.command == "bump":
        new = bumped_version(value, args.part)
        set_version(new)
        print(new)
    elif args.command == "verify-assets":
        verify_assets(args.directory, value)


if __name__ == "__main__":
    main()
