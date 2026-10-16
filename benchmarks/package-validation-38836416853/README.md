# Execute 0.9.0-rc.5 package-validation evidence

This compact bundle records package validation for source commit `58b42f65d8c4de171baf8f3049ab70cf37c51af4` only. GitHub Actions run [38836416853](https://github.com/monstercameron/NextGenScriptingLanguage/actions/runs/38836416853) completed successfully: all six native target jobs and both Flatpak jobs passed. The per-job logs show each checkout fetched the same source SHA.

`verification.json` records the run, eight job IDs, eight artifact IDs and GitHub artifact digests, the artifact-to-asset mapping, and the SHA-256 of each downloaded asset. `job-artifact-bindings.json` makes the per-job checkout SHA, artifact ID/name/digest, and filenames explicit. `SHA256SUMS` contains the exact twelve asset hashes. `asset-audit.json` binds that list to `tools/release.py`'s expected set at the same source commit and records that no names were missing or extra. No package binaries or full CI logs are included.

`macos-arm64-local-summary.json` summarizes the separate local macOS ARM64 archive validation on the same commit: 20 checks and 21 subprocess steps passed. It covers archive and extracted-root smoke checks, applet catalog lookup, a shebang executed from a path containing spaces, and a scratch file transaction rollback that restored the exact original bytes.

This is validation evidence, not a release. No `0.9.0-rc.5` release was published by these checks. The results apply only to commit `58b42f6`; newer source revisions need their own validation before their distribution status is updated.
