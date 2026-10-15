# Releases and installation

Native runtime builds target macOS, Linux and Windows on x64 and ARM64.
Windows is a native port, without WSL. The initial multi-platform release is a
release candidate: platform packaging does not establish complete Bash or
Omarchy compatibility. See [Windows semantics](windows.md) and the
[production proof gates](production-readiness.md).

| Platform | Download | Contents / requirements |
| --- | --- | --- |
| macOS Intel / Apple Silicon | `execute-VERSION-macos-x64.tar.gz` / `macos-arm64.tar.gz` | Both binaries, manifests, license and docs. macOS 15 or newer is the tested release baseline. Unsigned and not notarized. |
| Linux x64 / ARM64 | `execute-VERSION-linux-x64.tar.gz` / `linux-arm64.tar.gz` | Static musl executables; native host tools are separate dependencies. |
| Debian / Ubuntu | `execute_VERSION_amd64.deb` / `arm64.deb` | `/usr/bin` binaries and `/usr/share/execute/manifests`. |
| Fedora / RHEL family | `execute-VERSION-1.x86_64.rpm` / `aarch64.rpm` | Same layout; payload validated on Ubuntu, with Fedora installation validation in CI. |
| Windows x64 / ARM64 | `execute-VERSION-windows-x64.zip` / `windows-arm64.zip` | Real `.exe` binaries with a static CRT, without a separate VC++ redistributable. Extract the complete directory; add its `bin` directory to PATH. Not Authenticode signed. |
| Flatpak x64 / ARM64 | `execute-VERSION-linux-x64.flatpak` / `linux-arm64.flatpak` | CLI sandbox, Freedesktop 25.08. Home files and network are permitted. Host tools and desktop services are not automatically accessible. |

Every archive contains `execute-VERSION/bin/execute` and `execute-model`
(with `.exe` on Windows). Keep both binaries together and the `manifests`
directory beside `bin`. Native packages put the manifests under
`/usr/share/execute`. Do not copy the historical repository `bin/execute` into
release assets; the pipeline builds source with `cargo build --locked --release`.

Download from [GitHub Releases](https://github.com/monstercameron/NextGenScriptingLanguage/releases).
Verify the checksum before extracting or installing:

```sh
sha256sum --check SHA256SUMS --ignore-missing
# macOS: shasum -a 256 execute-VERSION-macos-arm64.tar.gz
# Compare with that exact filename in SHA256SUMS.
```

PowerShell:

```powershell
Get-FileHash .\execute-VERSION-windows-arm64.zip -Algorithm SHA256
# Compare with that exact filename in SHA256SUMS.
Expand-Archive .\execute-VERSION-windows-arm64.zip -DestinationPath .
.\execute-VERSION\bin\execute.exe --version
```

After downloading the matching Linux package:

```sh
sudo apt install ./execute_VERSION_amd64.deb
sudo dnf install ./execute-VERSION-1.x86_64.rpm
flatpak install --user ./execute-VERSION-linux-x64.flatpak
flatpak run io.github.monstercameron.Execute check_weather.program
```

These are alternative formats; install the one appropriate for your system.
Prerelease DEB/RPM metadata versions replace the SemVer hyphen with `~` (for example
`0.9.0~rc.1`) so package managers order them before `0.9.0`. Download filenames and executable versions retain the original SemVer spelling;
GitHub normalizes tildes in asset filenames, so `~` appears only inside package metadata. Flatpak uses `beta` for prereleases
and `stable` for normal releases. Bundles do not configure an automatic update
repository. This pipeline publishes downloads to GitHub, not to Flathub or a
public apt/yum repository.

The Flatpak application runs applets inside its sandbox. Use the native archive,
DEB or RPM for arbitrary host commands, package managers and Omarchy/Hyprland
integration. There is no hidden `flatpak-spawn --host` escape. Platform applets
such as `gh`, `jq` and `ps` still need to exist where the runtime invokes them.

GitHub build provenance is attached to release artifacts:

```sh
gh attestation verify execute-VERSION-linux-x64.tar.gz \
  --repo monstercameron/NextGenScriptingLanguage
```

Checksums detect corruption; attestation identifies the repository workflow that
built an artifact. They do not replace OS code signing or a platform acceptance
study. Signing certificates, Apple notarization and Flatpak repository GPG keys
are not configured.

## Maintainer workflow

`Cargo.toml` is the canonical runtime version; root `Cargo.lock` and the
root dependency entry in `browser/Cargo.lock` must agree. The release helper
validates and updates all three together. Both native runtime binaries embed that
version. The private browser wrapper and VS Code extension version separately.

```sh
python3 tools/release.py version
python3 tools/release.py bump patch
# Or explicitly prepare a candidate:
python3 tools/release.py set-version 0.9.0-rc.5
make check
git add Cargo.toml Cargo.lock browser/Cargo.lock
git commit -m "Prepare Execute 0.9.0-rc.5"
git push origin main
# Wait for native checks and a manually dispatched Package validation run.
python3 tools/release.py verify-tag v0.9.0-rc.5
git tag -a v0.9.0-rc.5 -m "Execute 0.9.0-rc.5"
git push origin v0.9.0-rc.5
```

Release tags use strict SemVer without build metadata. Leading zeros, malformed
prereleases and source/tag mismatches fail before builds. Once published, a
version's contents must not change; fix defects in a new version. Do not move
published tags. A manually dispatched release must select an existing tag.

The release workflow runs the reusable native checks, builds six native targets,
executes extracted packages, installs DEBs, verifies RPM installation, runs both
Flatpak sandboxes, and checks the exact twelve-package asset set. Only then does
it generate `SHA256SUMS`, attest the artifacts and create a draft release. It
checks uploaded server SHA-256 digests before publication. A retry may repair
an owned draft; a published release is only accepted unchanged, byte for byte.

Actions are pinned to commit SHAs. Build jobs have read-only tokens; only the
publication job receives release and OIDC attestation permissions. Builds use
locked Rust dependencies and native hosted runners. Archive timestamps and
ownership are normalized to the source commit. This does not claim fully
reproducible compiler output or identical signatures across independent builds.

The first published multi-platform candidate is **[0.9.0-rc.4](https://github.com/monstercameron/NextGenScriptingLanguage/releases/tag/v0.9.0-rc.4)**. Its complete [validation receipt](release-validation-0.9.0-rc.4.json)
records the source commit, every package digest, native test cohorts and verified
GitHub signing identity. Earlier unpublished candidate tags retain the fixes and
failed-attempt evidence; their draft release was cleaned up.

For interrupted publication, rerun failed jobs so they reuse the tested artifacts.
A fresh rebuild may differ and must never replace a published version.

The native checks use Unix utility suites on Linux/macOS and real PE fixtures
on Windows. Package validation is also available through GitHub's **Package
validation** workflow before tagging. Inspect the linked run for the exact
source commit, platform results, artifacts and any known limitations.
