# Packed manifest catalog representation evidence

This publication bundle preserves two distinct same-binary comparisons for the explicit, versioned packed manifest catalog integrated in commit `d66e412687d92b9cb21b1bc271ed36211055aa91`. The measurements do not establish a general runtime improvement or faster unchanged defaults.

## Library load comparison

The ignored release benchmark compares `Registry::load(directory)` with the explicit `Registry::load_catalog(file)` using the same test binary, the same 75 direct manifest JSON files, five loads per side per duration, five warmup pairs, and 48 measured pairs. The result was 48 packed wins, 0 losses, and 0 ties. Median per-load time was 4.2668 ms for directory mode and 0.4287 ms for packed mode; the median paired packed/directory ratio was 0.10748. This is a physical library representation comparison, not a CLI/default-startup result.

The API preregistration contains 77 `manifests/` JSON path hashes because two nested descriptor files are hashed with the source tree. The loader fixture and benchmark count are the 75 direct manifests only. The archive contains a reconstruction of the harness's exact packed bytes, verified against the recorded catalog-data SHA-256.
The CLI fixture contains byte-identical direct manifest inputs: its 75 file SHA-256 values match the library report's `source_inputs` set. Their normalized path/hash set SHA-256 is recorded in `identities.json`.

## Explicit CLI comparison

The CLI run compares `--manifest-dir` and `--manifest-catalog` on the same saved release binary and a copied 75-manifest fixture. It contains two fixed cases: `check --json` on a fixed `printf` workflow and `applets list`. There were five warmup and 48 measured pairs per case, with 24 AB and 24 BA orders. Every invocation's status, stdout, stderr, and parsed valid output matched the directory preflight oracle and the paired selector result. An independent read-only audit recomputed the summaries from the raw invocation journal.

| CLI case | Directory median | Packed median | Median paired packed/directory | Wins | Losses | Ties |
|---|---:|---:|---:|---:|---:|---:|
| `check --json` | 8.9750 ms | 5.8484 ms | 0.6559 | 47 | 1 | 0 |
| `applets list` | 12.3862 ms | 9.7487 ms | 0.7872 | 46 | 2 | 0 |

All three CLI losses are retained in `identities.json` and the raw paired report: one `check_json` loss and two `applets_list` losses. Their individual timings and zero-based pair indexes are:

- `check_json` pair index 23: directory 8,420,333 ns; packed 8,425,208 ns; packed/directory 1.000579.
- `applets_list` pair index 5: directory 12,555,500 ns; packed 69,540,500 ns; packed/directory 5.538648.
- `applets_list` pair index 7: directory 29,662,667 ns; packed 31,305,500 ns; packed/directory 1.055384.

These results cover explicit selector use for the recorded host, binary, two commands, and one 75-manifest fixture. `--manifest-catalog` is opt-in; when omitted, the existing directory/default selection remains in force. This bundle makes no claim that normal commands became faster by default.

## Reproduction and audit inventory

- `evidence/library/` preserves the API preregistration, all 48 raw measured pairs, run and build receipts, the focused 8-test log, and three adversarial source-review records. No test binary is included.
- `evidence/cli/` preserves CLI preregistration, paired report, independent raw-journal audit, build receipt, 8-test and 5-test logs, source identity, and the staged native/WASM integration checks. No executable binary is included.
- `source/` contains the loader and CLI source patches, the integrated commit patch, the exact library and CLI benchmark harness sources, the packer source, and the CLI journal writer test.
- `raw-artifacts.tar.gz` contains the full CLI raw invocation journal, its exact owned fixture, the library raw pair report and captured stdout/stderr, and 75-file library fixture plus its exact packed catalog. `raw-artifacts.manifest.json` inventories every member with its uncompressed size and SHA-256.
- `raw-artifacts.SHA256SUMS` verifies the raw archive. `bundle.SHA256SUMS` verifies every other bundle file except itself.

All binary identities are recorded in the original reports, preregistrations, independent audit, and build receipts. The binary files and vendored source trees are intentionally not included.
