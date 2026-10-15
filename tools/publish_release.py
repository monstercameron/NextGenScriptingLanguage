#!/usr/bin/env python3
"""Publish a complete validated artifact set through an owned draft release."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
from release import expected_assets, version


def gh(*args, **kwargs):
    return subprocess.run(["gh", *map(str, args)], check=True, **kwargs)


def lookup_release(repo, tag):
    # GitHub's /releases/tags endpoint does not return drafts, even to their
    # author. Authenticated release listing includes drafts; paginate so old
    # published releases are also found without creating duplicate drafts.
    lookup = subprocess.run(
        [
            "gh",
            "api",
            "--paginate",
            "--slurp",
            "repos/" + repo + "/releases?per_page=100",
        ],
        capture_output=True,
        text=True,
    )
    if lookup.returncode:
        raise ValueError("release lookup failed: " + lookup.stderr)
    matches = [
        item
        for page in json.loads(lookup.stdout)
        for item in page
        if item["tag_name"] == tag
    ]
    if len(matches) > 1:
        raise ValueError(
            "multiple releases use the same tag; refusing ambiguous ownership"
        )
    return matches[0] if matches else None


def main():
    value = version()
    tag = "v" + value
    repo = os.environ["GITHUB_REPOSITORY"]
    commit = os.environ["GITHUB_SHA"]
    if os.environ.get("GITHUB_REF") != "refs/tags/" + tag:
        raise ValueError("publication must run for the exact version tag")
    folder = Path("dist")
    expected = expected_assets(value) | {"SHA256SUMS"}
    if {p.name for p in folder.iterdir()} != expected:
        raise ValueError("publication asset set is incomplete")
    lines = (folder / "SHA256SUMS").read_text().splitlines()
    actual = {
        p.name: hashlib.sha256(p.read_bytes()).hexdigest()
        for p in folder.iterdir()
        if p.name != "SHA256SUMS"
    }
    if lines != [actual[n] + "  " + n for n in sorted(actual)]:
        raise ValueError("checksums do not match publication payloads")
    marker = "<!-- execute-release:" + tag + ":" + commit + " -->"
    existing = lookup_release(repo, tag)
    if existing and marker not in existing.get("body", ""):
        raise ValueError(
            "refusing to mutate a release not owned by this exact tag/commit"
        )
    if existing and not existing["draft"]:
        # Immutable retry: compare every published byte; never replace published assets.
        with tempfile.TemporaryDirectory() as tmp:
            gh("release", "download", tag, "--repo", repo, "--dir", tmp)
            files = {p.name: p for p in Path(tmp).iterdir()}
            if set(files) != expected or any(
                files[n].read_bytes() != (folder / n).read_bytes() for n in expected
            ):
                raise ValueError(
                    "published release differs; create a new version, never overwrite it"
                )
        print("Published release already matches every asset; unchanged")
        return
    if not existing:
        notes = Path("target/release-notes.md")
        notes.parent.mkdir(exist_ok=True)
        # The published rc4 archives predate the generic applet helper. Keep a
        # newly generated retry/draft for that historical version accurate.
        bundled_helpers = (
            "keep execute and execute-model together"
            if value == "0.9.0-rc.4"
            else "keep execute, execute-model and execute-applet together"
        )
        notes.write_text(
            "Execute "
            + value
            + "\n\nNative macOS, Linux and Windows builds, including x64 and ARM64. Linux also includes DEB, RPM and sandboxed Flatpak bundles.\n\nDownload the archive for your platform; "
            + bundled_helpers
            + ". Verify SHA256SUMS and GitHub artifact provenance before installation.\n\n"
            + ("This is a release candidate. " if "-" in value else "")
            + "Windows is native, without WSL. The Flatpak edition runs inside its sandbox; use native packages for full host automation. Binaries are not Apple-notarized or Authenticode-signed.\n\n[Installation and release policy](https://github.com/"
            + repo
            + "/blob/"
            + tag
            + "/docs/releases.md).\n\n"
            + marker
            + "\n"
        )
        gh(
            "release",
            "create",
            tag,
            "--repo",
            repo,
            "--verify-tag",
            "--draft",
            "--title",
            "Execute " + value,
            "--notes-file",
            notes,
        )
        for delay in [0, 0.25, 0.5, 1]:
            if delay:
                time.sleep(delay)
            existing = lookup_release(repo, tag)
            if existing:
                break
        if (
            not existing
            or not existing["draft"]
            or marker not in existing.get("body", "")
        ):
            raise ValueError(
                "created draft could not be verified; leaving it unpublished"
            )
    gh(
        "release",
        "upload",
        tag,
        "--repo",
        repo,
        "--clobber",
        *[folder / name for name in sorted(expected)]
    )
    # Check server digests, not merely a successful HTTP response or asset count.
    release = json.loads(
        subprocess.check_output(
            ["gh", "api", "repos/" + repo + "/releases/" + str(existing["id"])],
            text=True,
        )
    )
    assets = {a["name"]: a for a in release["assets"]}
    if set(assets) != expected:
        raise ValueError("draft asset set differs after upload")
    for name in expected:
        local = folder / name
        digest = "sha256:" + hashlib.sha256(local.read_bytes()).hexdigest()
        if (
            assets[name]["size"] != local.stat().st_size
            or assets[name].get("digest") != digest
        ):
            raise ValueError("server digest mismatch: " + name)
    gh(
        "api",
        "--method",
        "PATCH",
        "repos/" + repo + "/releases/" + str(release["id"]),
        "--input",
        "-",
        input=json.dumps(
            {
                "draft": False,
                "prerelease": "-" in value,
                "make_latest": "false" if "-" in value else "true",
            }
        ),
        text=True,
        stdout=subprocess.DEVNULL,
    )
    print(
        "Published complete release: https://github.com/"
        + repo
        + "/releases/tag/"
        + tag
    )


if __name__ == "__main__":
    main()
