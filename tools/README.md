# Maintainer tools

Run helpers from the repository root. Prefer `make help` for everyday development;
these scripts also back CI, release packaging and documentation generation.

| Helper | Responsibility |
| --- | --- |
| `release.py` | Strict runtime SemVer, synchronized lockfiles, tag and asset-set contracts. |
| `package.py` | Native archives and Linux DEB/RPM payloads, architecture checks and deterministic archive metadata. |
| `package_smoke.py` | Safe fresh extraction and actual installed-tree execution checks. |
| `flatpak.py` | Prebuilt static payloads, sandbox checks and Flatpak bundles. |
| `publish_release.py` | Draft publication, digest verification and immutable published-release retries. |
| `build_playground.py` | Rust/WASM build, browser assets and full dependency notices. |
| `sync_site.py` | Copies canonical example sources and historical benchmark data into the showcase. |
| `capture_site.py` | Actual TUI frames using deterministic private fixture backends on macOS. |
| `check_docs.py` | Local file and fragment links in maintained documentation. |

Packaging/publication helpers are maintainer operations, not ordinary test setup.
Read [release policy](../docs/releases.md) before tagging or publishing. Screenshots
have separate optional Python dependencies; see [site maintenance](../docs/site.md).
Keep generated reports in ignored `target/` and preserve historical receipts.
