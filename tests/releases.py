#!/usr/bin/env python3
"""Version, architecture and deterministic package contracts; no publishing."""
import hashlib
import contextlib
import io
import json
import os
import re
import subprocess
from pathlib import Path
import struct
import sys
import tarfile
import tempfile
import unittest
from unittest import mock
import zipfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import release
import package
import publish_release
import package_smoke


class Releases(unittest.TestCase):
    def test_strict_semver_and_tag_source_versions(self):
        for value in ["0.0.0", "0.9.0-rc.1", "1.2.3", "1.2.3-a-1.0"]:
            self.assertEqual(release.validate(value), value)
        for value in [
            "v1.2.3",
            "1.2",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "1.2.3-01",
            "1.2.3-x..y",
            "1.2.3+a",
            "1.2.3\n",
            "../x",
        ]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                release.validate(value)
        self.assertTrue(release.version())
        self.assertEqual(release.package_version("0.9.0-rc.1"), "0.9.0~rc.1")

    def test_version_edit_only_changes_the_root_package_and_rejects_mismatch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "Cargo.toml").write_text(
                '[package]\nname = "execute"\nversion = "0.9.0-dev"\n\n[dependencies]\nother = "1"\n'
            )
            lock = 'version = 4\n\n[[package]]\nname = "execute"\nversion = "0.9.0-dev"\n\n[[package]]\nname = "other"\nversion = "1.2.3"\n'
            (root / "Cargo.lock").write_text(lock)
            release.set_version("0.9.0-rc.1", root)
            self.assertEqual(release.version(root), "0.9.0-rc.1")
            self.assertIn(
                'name = "other"\nversion = "1.2.3"', (root / "Cargo.lock").read_text()
            )
            before = (root / "Cargo.toml").read_bytes()
            (root / "Cargo.lock").write_text(lock)
            with self.assertRaises(ValueError):
                release.set_version("0.9.1", root)
            self.assertEqual((root / "Cargo.toml").read_bytes(), before)

    def test_browser_path_dependency_version_tracks_atomic_source_bumps(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "browser").mkdir()
            (root / "Cargo.toml").write_text(
                '[package]\nname = "execute"\nversion = "0.9.0-rc.4"\n'
            )
            lock = (
                'version = 4\n[[package]]\nname = "execute"\nversion = "0.9.0-rc.4"\n'
            )
            (root / "Cargo.lock").write_text(lock)
            browser = (
                lock + '\n[[package]]\nname = "execute-browser"\nversion = "0.1.0"\n'
            )
            (root / "browser/Cargo.lock").write_text(browser)
            release.set_version("0.9.0-rc.5", root)
            self.assertEqual(release.version(root), "0.9.0-rc.5")
            self.assertIn(
                'name = "execute-browser"\nversion = "0.1.0"',
                (root / "browser/Cargo.lock").read_text(),
            )
            (root / "browser/Cargo.lock").write_text(browser)
            before = [
                (root / name).read_bytes() for name in ["Cargo.toml", "Cargo.lock"]
            ]
            with self.assertRaises(ValueError):
                release.set_version("0.9.1", root)
            self.assertEqual(
                before,
                [(root / name).read_bytes() for name in ["Cargo.toml", "Cargo.lock"]],
            )

    def test_complete_asset_gate_rejects_partial_extra_and_empty_then_checksums(self):
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp)
            names = release.expected_assets("0.9.0-rc.1")
            self.assertEqual(len(names), 13)
            self.assertIn("execute-0.9.0-rc.1.cdx.json", names)
            self.assertTrue(
                all(release.re.fullmatch(r"[A-Za-z0-9._-]+", name) for name in names)
            )
            self.assertIn("execute_0.9.0-rc.1_amd64.deb", names)
            self.assertIn("execute-0.9.0-rc.1-1.x86_64.rpm", names)
            with self.assertRaises(ValueError):
                release.verify_assets(folder, "0.9.0-rc.1")
            for name in names:
                (folder / name).write_bytes(b"x" * 200)
            extra = folder / "untrusted.exe"
            extra.write_bytes(b"bad")
            with self.assertRaises(ValueError):
                release.verify_assets(folder, "0.9.0-rc.1")
            extra.unlink()
            release.verify_assets(folder, "0.9.0-rc.1")
            lines = (folder / "SHA256SUMS").read_text().splitlines()
            self.assertEqual(len(lines), 13)
            self.assertEqual({line.split("  ")[1] for line in lines}, names)
            self.assertTrue(
                all(
                    line.startswith(hashlib.sha256(b"x" * 200).hexdigest())
                    for line in lines
                )
            )

    def test_architecture_checks_reject_wrong_platform_and_dynamic_linux(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "binary"
            for target in release.TARGETS:
                arm = target.startswith("aarch64")
                data = bytearray(512)
                if "windows" in target:
                    data[:2] = b"MZ"
                    struct.pack_into("<I", data, 0x3C, 128)
                    data[128:132] = b"PE\0\0"
                    struct.pack_into("<H", data, 132, 0xAA64 if arm else 0x8664)
                    struct.pack_into("<H", data, 148, 240)
                    struct.pack_into("<H", data, 152, 0x20B)
                    struct.pack_into("<I", data, 260, 16)
                elif "linux" in target:
                    data[:6] = b"\x7fELF\x02\x01"
                    struct.pack_into("<H", data, 18, 183 if arm else 62)
                    struct.pack_into("<Q", data, 32, 64)
                    struct.pack_into("<HH", data, 54, 56, 1)
                    struct.pack_into("<I", data, 64, 1)
                else:
                    data[:4] = b"\xcf\xfa\xed\xfe"
                    struct.pack_into("<I", data, 4, 0x100000C if arm else 0x1000007)
                path.write_bytes(data)
                package.check_binary(path, target)
                if "linux" in target:
                    struct.pack_into("<I", data, 64, 3)
                    path.write_bytes(data)
                    with self.assertRaises(ValueError):
                        package.check_binary(path, target)
                path.write_bytes(b"wrong")
                with self.assertRaises((ValueError, struct.error)):
                    package.check_binary(path, target)

    def test_windows_import_gate_rejects_hidden_visual_cpp_dependencies(self):
        data = bytearray(1024)
        data[:2] = b"MZ"
        struct.pack_into("<I", data, 0x3C, 128)
        data[128:132] = b"PE\0\0"
        struct.pack_into("<HH", data, 132, 0x8664, 1)
        struct.pack_into("<H", data, 148, 240)
        struct.pack_into("<H", data, 152, 0x20B)
        struct.pack_into("<I", data, 260, 16)
        struct.pack_into("<II", data, 272, 0x1000, 40)
        struct.pack_into("<IIII", data, 400, 256, 0x1000, 256, 512)
        struct.pack_into("<IIIII", data, 512, 0, 0, 0, 0x1040, 0)
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "fixture.exe"
            for name, forbidden in [
                ("VCRUNTIME140.dll", True),
                ("MSVCP140.dll", True),
                ("KERNEL32.dll", False),
            ]:
                data[576:640] = bytes(64)
                data[576 : 576 + len(name)] = name.encode()
                binary.write_bytes(data)
                self.assertEqual(package.pe_imports(data), [name])
                if forbidden:
                    with self.assertRaisesRegex(ValueError, "non-system CRT"):
                        package.check_binary(binary, "x86_64-pc-windows-msvc")
                else:
                    package.check_binary(binary, "x86_64-pc-windows-msvc")

    def test_populate_bundles_applet_helper_with_primary_and_model_binaries(self):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            binaries = work / "release"
            binaries.mkdir()
            for name in ["execute", "execute-model", "execute-applet"]:
                (binaries / name).write_bytes(name.encode())
            root = work / "archive"
            with mock.patch.object(package, "check_binary"):
                package.populate(root, binaries, "x86_64-apple-darwin")
            for name in ["execute", "execute-model", "execute-applet"]:
                self.assertEqual((root / "bin" / name).read_bytes(), name.encode())
            system_root = work / "system"
            with mock.patch.object(package, "check_binary"):
                package.populate(
                    system_root,
                    binaries,
                    "x86_64-unknown-linux-musl",
                    system=True,
                )
            self.assertEqual(
                (system_root / "usr/bin/execute-applet").read_bytes(),
                b"execute-applet",
            )

    def test_archive_bytes_reproducible_despite_input_mtime(self):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            root = work / "execute-1.0.0"
            (root / "bin").mkdir(parents=True)
            (root / "bin/execute").write_bytes(b"fixture")
            (root / "README.md").write_text("read me")
            for extension in [".tar.gz", ".zip"]:
                a, b = work / ("a" + extension), work / ("b" + extension)
                package.archive(root, a, 1700000000)
                (root / "README.md").touch()
                package.archive(root, b, 1700000000)
                self.assertEqual(a.read_bytes(), b.read_bytes())
                if extension == ".zip":
                    with zipfile.ZipFile(a) as z:
                        self.assertEqual(
                            z.read("execute-1.0.0/bin/execute"), b"fixture"
                        )
                else:
                    with tarfile.open(a) as t:
                        self.assertEqual(
                            t.extractfile("execute-1.0.0/bin/execute").read(),
                            b"fixture",
                        )


class ArchiveInstallation(unittest.TestCase):
    def test_extraction_is_fresh_versioned_and_rejects_traversal(self):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            root = work / ("execute-" + release.version())
            root.mkdir()
            (root / "fixture.txt").write_text("owned")
            for extension in [".zip", ".tar.gz"]:
                path = work / ("fixture" + extension)
                package.archive(root, path, 1700000000)
                with package_smoke.extracted_archive(path) as extracted:
                    self.assertNotEqual(extracted.parent, work)
                    self.assertEqual((extracted / "fixture.txt").read_text(), "owned")
                    previous = extracted
                self.assertFalse(previous.exists())
            malicious = work / "malicious.zip"
            with zipfile.ZipFile(malicious, "w") as archive:
                archive.writestr("../must-not-exist", b"escape")
            with self.assertRaisesRegex(ValueError, "unsafe archive path"):
                with package_smoke.extracted_archive(malicious):
                    self.fail("unsafe archive was accepted")
            self.assertFalse((work / "must-not-exist").exists())


class Publication(unittest.TestCase):
    @contextlib.contextmanager
    def payload(self):
        with tempfile.TemporaryDirectory() as tmp:
            previous = Path.cwd()
            os.chdir(tmp)
            try:
                folder = Path("dist")
                folder.mkdir()
                value = release.version()
                for name in release.expected_assets(value):
                    (folder / name).write_bytes(b"owned artifact fixture" * 20)
                release.verify_assets(folder, value)
                # The publish job signs SHA256SUMS after verify-assets writes it.
                (folder / release.SIGNATURE).write_bytes(b"signature bundle fixture" * 20)
                with mock.patch.dict(
                    os.environ,
                    {
                        "GITHUB_REPOSITORY": "owner/repo",
                        "GITHUB_SHA": "a" * 40,
                        "GITHUB_REF": "refs/tags/v" + value,
                    },
                ), contextlib.redirect_stdout(io.StringIO()):
                    yield folder, value
            finally:
                os.chdir(previous)

    def server(self, folder, value, draft=True):
        return {
            "id": 1,
            "tag_name": "v" + value,
            "draft": draft,
            "body": "<!-- execute-release:v" + value + ":" + "a" * 40 + " -->",
            "assets": [
                {
                    "name": p.name,
                    "size": p.stat().st_size,
                    "digest": "sha256:" + hashlib.sha256(p.read_bytes()).hexdigest(),
                }
                for p in folder.iterdir()
            ],
        }

    def test_new_draft_is_found_in_paginated_listing_and_verified_by_id(self):
        with self.payload() as (folder, value):
            server = self.server(folder, value)
            missing = subprocess.CompletedProcess([], 0, "[[]]", "")
            found = subprocess.CompletedProcess([], 0, json.dumps([[], [server]]), "")
            with mock.patch.object(
                publish_release.subprocess, "run", side_effect=[missing, found]
            ) as listing, mock.patch.object(
                publish_release.subprocess,
                "check_output",
                return_value=json.dumps(server),
            ) as by_id, mock.patch.object(
                publish_release, "gh"
            ) as gh:
                publish_release.main()
                self.assertEqual(listing.call_count, 2)
                self.assertIn("--paginate", listing.call_args.args[0])
                self.assertTrue(by_id.call_args.args[0][-1].endswith("/releases/1"))
                self.assertEqual(gh.call_args_list[0].args[:2], ("release", "create"))
                if value == "0.9.0-rc.4":
                    notes = Path("target/release-notes.md").read_text()
                    self.assertIn("keep execute and execute-model together", notes)
                    self.assertNotIn("execute-applet", notes)
                self.assertEqual(
                    gh.call_args_list[-1].args[:3], ("api", "--method", "PATCH")
                )

    def test_uploaded_server_digest_is_required_before_publication(self):
        for corrupt in [False, True]:
            with self.subTest(corrupt=corrupt), self.payload() as (folder, value):
                server = self.server(folder, value)
                if corrupt:
                    server["assets"][0]["digest"] = "sha256:" + "0" * 64
                lookup = subprocess.CompletedProcess([], 0, json.dumps([[server]]), "")
                with mock.patch.object(
                    publish_release.subprocess, "run", return_value=lookup
                ), mock.patch.object(
                    publish_release.subprocess,
                    "check_output",
                    return_value=json.dumps(server),
                ), mock.patch.object(
                    publish_release, "gh"
                ) as gh:
                    if corrupt:
                        with self.assertRaisesRegex(ValueError, "digest mismatch"):
                            publish_release.main()
                        self.assertFalse(
                            any(
                                c.args[:3] == ("api", "--method", "PATCH")
                                for c in gh.call_args_list
                            )
                        )
                    else:
                        publish_release.main()
                        self.assertEqual(
                            gh.call_args_list[-1].args[:3], ("api", "--method", "PATCH")
                        )
                        request = json.loads(gh.call_args_list[-1].kwargs["input"])
                        self.assertFalse(request["draft"])
                        self.assertEqual(request["prerelease"], "-" in value)

    def test_unowned_release_wrong_tag_and_corrupt_local_payload_never_upload(self):
        with self.payload() as (folder, value):
            server = self.server(folder, value)
            server["body"] = "somebody else owns this release"
            lookup = subprocess.CompletedProcess([], 0, json.dumps([[server]]), "")
            with mock.patch.object(
                publish_release.subprocess, "run", return_value=lookup
            ), mock.patch.object(publish_release, "gh") as gh:
                with self.assertRaisesRegex(ValueError, "not owned"):
                    publish_release.main()
                gh.assert_not_called()
            with mock.patch.dict(
                os.environ, {"GITHUB_REF": "refs/heads/main"}
            ), mock.patch.object(publish_release.subprocess, "run") as api:
                with self.assertRaisesRegex(ValueError, "exact version tag"):
                    publish_release.main()
                api.assert_not_called()
            next(
                p
                for p in folder.iterdir()
                if p.name not in {release.CHECKSUMS, release.SIGNATURE}
            ).write_bytes(b"changed")
            with mock.patch.object(publish_release.subprocess, "run") as api:
                with self.assertRaisesRegex(ValueError, "checksums"):
                    publish_release.main()
                api.assert_not_called()

    def test_published_retry_compares_every_byte_without_mutating_the_release(self):
        for corrupt in [False, True]:
            with self.subTest(corrupt=corrupt), self.payload() as (folder, value):
                server = self.server(folder, value, draft=False)
                lookup = subprocess.CompletedProcess([], 0, json.dumps([[server]]), "")

                def download(*args, **kwargs):
                    self.assertEqual(args[:2], ("release", "download"))
                    destination = Path(args[args.index("--dir") + 1])
                    for path in folder.iterdir():
                        (destination / path.name).write_bytes(path.read_bytes())
                    if corrupt:
                        (destination / "SHA256SUMS").write_bytes(b"changed remotely")

                with mock.patch.object(
                    publish_release.subprocess, "run", return_value=lookup
                ), mock.patch.object(publish_release, "gh", side_effect=download) as gh:
                    if corrupt:
                        with self.assertRaisesRegex(ValueError, "never overwrite"):
                            publish_release.main()
                    else:
                        publish_release.main()
                    self.assertEqual(gh.call_count, 1)


class PackagingLayout(unittest.TestCase):
    """The installed layout that the .deb, .rpm and AUR packages share."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.work = Path(self._tmp.name)
        self.binaries = self.work / "release"
        self.binaries.mkdir()
        for name in ["execute", "execute-model", "execute-applet"]:
            (self.binaries / name).write_bytes(name.encode())

    def tearDown(self):
        self._tmp.cleanup()

    def test_system_layout_installs_completions_man_page_grammar_and_docs(self):
        root = self.work / "system"
        with mock.patch.object(package, "check_binary"):
            package.populate(root, self.binaries, "x86_64-unknown-linux-musl", system=True)
        expected = [
            "usr/share/bash-completion/completions/execute",
            "usr/share/zsh/site-functions/_execute",
            "usr/share/fish/vendor_completions.d/execute.fish",
            "usr/share/man/man1/execute.1",
            "usr/share/execute/editors/vscode/syntaxes/execute.tmLanguage.json",
            "usr/share/execute/editors/vscode/language-configuration.json",
            "usr/share/execute/docs/language.md",
            "usr/share/execute/manifests/printf.json",
        ]
        for relative in expected:
            self.assertTrue((root / relative).is_file(), relative)

    def test_archive_layout_keeps_the_same_files_under_relative_names(self):
        root = self.work / "archive"
        with mock.patch.object(package, "check_binary"):
            package.populate(root, self.binaries, "x86_64-apple-darwin")
        for relative in [
            "completions/execute.bash",
            "completions/_execute",
            "completions/execute.fish",
            "man/execute.1",
            "editors/vscode/syntaxes/execute.tmLanguage.json",
            "docs/language.md",
        ]:
            self.assertTrue((root / relative).is_file(), relative)

    def test_rpm_spec_lists_the_files_outside_share_execute(self):
        outside = [relative for _, relative in package.extra_files(True) if not relative.startswith("share/execute/")]
        self.assertEqual(
            sorted(outside),
            sorted([
                "share/bash-completion/completions/execute",
                "share/zsh/site-functions/_execute",
                "share/fish/vendor_completions.d/execute.fish",
                "share/man/man1/execute.1",
            ]),
        )


class AurPackages(unittest.TestCase):
    """The PKGBUILDs are rendered per release; their pinned files must match the inputs."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.work = Path(self._tmp.name)
        self.value = release.version()

    def tearDown(self):
        self._tmp.cleanup()

    def test_templates_install_the_manifest_directory_the_binary_searches(self):
        for name in ["execute-bin", "execute"]:
            text = (ROOT / "packaging/aur" / name / "PKGBUILD").read_text()
            self.assertIn("/usr/share/execute/manifests", text, name)
            self.assertIn('/usr/bin/execute"', text, name)
            self.assertIn("_tag=", text, name)

    def test_render_pins_version_and_checksums_of_the_inputs(self):
        import aur

        x64 = self.work / "x64.tar.gz"
        x64.write_bytes(b"binary archive bytes")
        source = self.work / "source.tar.gz"
        source.write_bytes(b"source archive bytes")
        out = self.work / "pkgs"
        aur.render(out, self.value, linux_x64=x64, source=source)

        binary = (out / "execute-bin/PKGBUILD").read_text()
        self.assertIn("_tag=" + self.value, binary)
        self.assertIn(
            "sha256sums_x86_64=('" + hashlib.sha256(b"binary archive bytes").hexdigest() + "')",
            binary,
        )
        self.assertIn("sha256sums_aarch64=('SKIP')", binary)
        self.assertTrue((out / "execute-bin" / ("execute-" + self.value + "-linux-x64.tar.gz")).is_file())

        built = (out / "execute/PKGBUILD").read_text()
        self.assertIn(
            "sha256sums=('" + hashlib.sha256(b"source archive bytes").hexdigest() + "')",
            built,
        )
        self.assertTrue((out / "execute" / ("NextGenScriptingLanguage-" + self.value + ".tar.gz")).is_file())

    def test_pkgver_has_no_hyphen_and_the_tag_keeps_it(self):
        import aur

        text = aur.pin("_tag=0.1.0\npkgver=x\n", "0.9.0-rc.5")
        self.assertIn("_tag=0.9.0-rc.5", text)
        self.assertIn("pkgver=${_tag//-/_}", text)

    def test_render_refuses_a_version_that_is_not_the_repository_version(self):
        import aur

        with self.assertRaisesRegex(ValueError, "not the repository version"):
            aur.render(self.work / "pkgs", "9.9.9")


class SbomBuild(unittest.TestCase):
    """The CycloneDX SBOM is deterministic and hashes only registry packages."""

    def metadata(self):
        return {
            "packages": [
                {"id": "root 0.1.0", "name": "execute", "version": "0.1.0", "license": "MIT"},
                {"id": "dep 1.2.3", "name": "dep", "version": "1.2.3", "license": "MIT OR Apache-2.0"},
                {"id": "gitdep 0.1.0", "name": "gitdep", "version": "0.1.0", "license": None},
            ],
            "resolve": {
                "root": "root 0.1.0",
                "nodes": [
                    {"id": "root 0.1.0", "dependencies": ["dep 1.2.3", "gitdep 0.1.0"]},
                    {"id": "dep 1.2.3", "dependencies": []},
                    {"id": "gitdep 0.1.0", "dependencies": []},
                ],
            },
        }

    def test_sbom_is_deterministic_hashes_registry_packages_and_keeps_edges(self):
        import sbom

        lock = (
            '[[package]]\nname = "dep"\nversion = "1.2.3"\nsource = "registry+x"\n'
            'checksum = "' + "a" * 64 + '"\n\n'
            '[[package]]\nname = "gitdep"\nversion = "0.1.0"\nsource = "git+x"\n'
        )
        first = sbom.build_sbom(self.metadata(), lock, "0.1.0", 1700000000)
        second = sbom.build_sbom(self.metadata(), lock, "0.1.0", 1700000000)
        self.assertEqual(first, second)
        self.assertEqual(first["bomFormat"], "CycloneDX")
        self.assertEqual(first["specVersion"], "1.5")
        self.assertEqual(first["metadata"]["timestamp"], "2023-11-14T22:13:20Z")
        by_name = {c["name"]: c for c in first["components"]}
        self.assertEqual(set(by_name), {"dep", "gitdep"})
        self.assertEqual(by_name["dep"]["hashes"], [{"alg": "SHA-256", "content": "a" * 64}])
        self.assertNotIn("hashes", by_name["gitdep"])
        root = next(d for d in first["dependencies"] if d["ref"] == "pkg:cargo/execute@0.1.0")
        self.assertEqual(
            root["dependsOn"], ["pkg:cargo/dep@1.2.3", "pkg:cargo/gitdep@0.1.0"]
        )
        self.assertTrue(first["serialNumber"].startswith("urn:uuid:"))


class WorkflowPins(unittest.TestCase):
    """Every action a workflow runs is pinned to a commit SHA, not a mutable tag."""

    USES = re.compile(r"^\s*(?:-\s+)?uses:\s*(\S+)", re.MULTILINE)
    PINNED = re.compile(r"^[^@\s]+@[0-9a-f]{40}$")

    def test_every_remote_action_is_pinned_by_commit_sha(self):
        unpinned = []
        for workflow in sorted((ROOT / ".github/workflows").glob("*.yml")):
            for match in self.USES.finditer(workflow.read_text()):
                reference = match[1]
                if reference.startswith("./") or self.PINNED.match(reference):
                    continue
                unpinned.append(workflow.name + ": " + reference)
        self.assertEqual(unpinned, [])

    def test_lint_detects_a_mutable_tag(self):
        sample = "      - uses: actions/checkout@v4\n"
        match = self.USES.search(sample)
        self.assertEqual(match[1], "actions/checkout@v4")
        self.assertIsNone(self.PINNED.match(match[1]))


class SoakAssertions(unittest.TestCase):
    """The soak's resource assertions, tested on fixed samples (samples are rss_kb, threads, fds)."""

    LIMITS = {"rss_growth_pct": 10.0, "fd_slack": 2, "thread_slack": 1}

    def test_status_parsing_reads_resident_size_and_threads(self):
        import soak

        text = "Name:\texecute\nVmRSS:\t   12345 kB\nThreads:\t7\n"
        self.assertEqual(soak.parse_status(text), (12345, 7))
        with self.assertRaises(ValueError):
            soak.parse_status("Name:\texecute\n")

    def test_flat_samples_pass(self):
        import soak

        samples = [(1000, 4, 10)] + [(1005, 4, 10)] * 6
        self.assertEqual(soak.violations(samples, 0, self.LIMITS), [])

    def test_rss_growth_fails_after_warm_up(self):
        import soak

        samples = [(1000, 4, 10), (1000, 4, 10)] + [(1200, 4, 10)] * 3
        failures = soak.violations(samples, 1, self.LIMITS)
        self.assertTrue(any("RSS grew" in failure for failure in failures), failures)

    def test_descriptor_and_thread_growth_fail(self):
        import soak

        samples = [(1000, 4, 10), (1000, 4, 10), (1000, 9, 20)]
        failures = soak.violations(samples, 1, self.LIMITS)
        self.assertTrue(any("descriptors rose" in failure for failure in failures), failures)
        self.assertTrue(any("threads rose" in failure for failure in failures), failures)

    def test_growth_during_warm_up_is_not_counted(self):
        import soak

        samples = [(500, 2, 5), (1000, 9, 30), (1000, 4, 10), (1001, 4, 10)]
        self.assertEqual(soak.violations(samples, 2, self.LIMITS), [])


class SoakRunAssertions(unittest.TestCase):
    """The graph soak compares per-run peaks: first third against last third."""

    LIMITS = {"rss_growth_pct": 10.0, "fd_slack": 2, "thread_slack": 1}

    def test_flat_run_peaks_pass(self):
        import soak

        peaks = [(2000, 5, 12)] * 9
        self.assertEqual(soak.run_peak_violations(peaks, self.LIMITS), [])

    def test_run_peaks_that_drift_upward_fail(self):
        import soak

        peaks = [(2000, 5, 12)] * 3 + [(2000, 5, 12)] * 3 + [(3000, 5, 12)] * 3
        failures = soak.run_peak_violations(peaks, self.LIMITS)
        self.assertTrue(any("grew" in failure for failure in failures), failures)

    def test_too_few_runs_is_a_failure_not_a_pass(self):
        import soak

        self.assertTrue(soak.run_peak_violations([(1, 1, 1)] * 3, self.LIMITS))


if __name__ == "__main__":
    unittest.main()
