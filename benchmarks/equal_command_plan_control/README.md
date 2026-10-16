# Equal command plan control results

This retained control compares Execute revision `58b42f65d8c4de171baf8f3049ab70cf37c51af4`, Bash, and Nushell on six public whole-workflow fixtures with aligned external command plans. The driver, workload sources, tests, and catalog are laid out at the path intended for integration: `benchmarks/equal_command_plan_control/`. See [RUNNING.md](RUNNING.md) for test and rerun instructions.

| Fixture | Execute full | Execute inert | Paired inert − full | Bash | Nushell |
|---|---:|---:|---:|---:|---:|
| Inventory: names, spaces, hidden files | 27.784 / 52.677 | 24.505 / 45.706 | −3.298 / 14.701 | 21.223 / 41.743 | 34.961 / 63.148 |
| Inventory: second directory | 26.547 / 52.824 | 23.789 / 50.176 | −2.824 / 11.266 | 19.803 / 77.865 | 36.119 / 57.017 |
| Layered merge: nested object | 16.123 / 33.113 | 12.885 / 30.030 | −3.087 / 14.371 | 11.563 / 33.613 | 24.802 / 47.745 |
| Layered merge: empty layers | 15.847 / 49.748 | 12.283 / 25.997 | −3.493 / 1.656 | 11.478 / 37.835 | 24.375 / 52.054 |
| Event normalization: optional actor fields | 15.910 / 33.954 | 12.416 / 27.422 | −3.372 / 1.283 | 11.269 / 21.853 | 23.753 / 60.027 |
| Event normalization: different event content | 16.257 / 26.330 | 12.279 / 19.879 | −3.758 / 0.217 | 11.346 / 26.747 | 25.053 / 58.346 |

Each cell shows median / nearest-rank p95 wall time in milliseconds across 48 measured samples. “Paired inert − full” is the per-round difference between Execute’s one-entry inert-catalog and full-catalog arms; its p95 is computed over the paired differences. The measured medians place full-catalog Execute between Bash and Nushell on all six fixtures. The paired median deltas range from −2.824 to −3.758 ms in these cases.

The Bash and Nushell arms use the preserved public authored scripts. Execute uses the same `find -> sort -> tee` plan for inventory and `jq` with an input-file argument followed by `tee` for the JSON workflows. Each arm uses the same fixture inputs and output paths; the Execute programs have no internal `save` stage in these cases. The inert catalog contains exactly one valid, unreferenced `cat.json`. It is a catalog-load diagnostic and does not represent the default catalog or default runtime behavior.

The campaign balanced all 24 four-arm orders twice per fixture. It retained five warmup rounds per fixture, 1,152 measured invocations, 120 warmups, and 24 correctness preflights. The oracle checks exact status, stdout, stderr, and workspace effects before measurements and on every invocation. The independent auditor also binds all six expected fixtures to the checked-in public task specification using `matched_language.load_scenarios`; the raw report records and the audit pins that specification's SHA-256 as `e02fc57cb4a40c5ee7f4deb4dc82c80f8a2f649bc614b3adb88295f3ccff1e84`.

The run was recorded on macOS 26.6.2 arm64 with Python 3.9.6, GNU Bash 3.2.57, and Nushell 0.116.0. Before/after artifact fingerprints are identical. This is current-source whole-workflow wall-time evidence for these six fixtures on this machine. It makes no cross-revision speedup claim, no universal Bash-speed claim, no CPU-time or RSS claim, and no default-catalog inference. It does not measure internal-save or durability differences.

The lossless raw report is [raw/equal-command-plan-macos-arm64-58b42f6.raw.json.xz](raw/equal-command-plan-macos-arm64-58b42f6.raw.json.xz). Its decompressed size is 2,570,251 bytes with SHA-256 `3d29d62a2f0a8ed4b9b8263d4bb0d3bb4fc57b3c98f0dda9d0b826a2b3851b0f`; the XZ file is 50,360 bytes with SHA-256 `23f446da1b632f0cf39a7a0f17c690b363acada4ab6f403eb972afd028c44512`. [summary.json](summary.json) contains independently recomputed statistics and the report, source, catalog, and binary fingerprints.
