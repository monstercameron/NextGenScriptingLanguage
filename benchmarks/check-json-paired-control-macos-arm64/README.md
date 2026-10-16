# Paired `check --json` control — macOS ARM64

This bundle preserves three completed paired measurements comparing the saved source baseline with a reviewed CLI change that reuses the already-loaded static-check program/catalog data when producing JSON. The measured workload is deliberately narrow: `execute check --json` validates a 1-step or 100-step program statically against either 75 manifests or an inert one-manifest catalog. Each program contains a quoted, nonexistent command and ends with a save to a forbidden path. Static checking must neither invoke the command nor create the save output.

## Result

Candidate change is percentage versus baseline; negative values are lower. Medians and nearest-rank p95 are end-to-end process latency in milliseconds. Each row has 48 measured pairs.

| Run | Steps | Median baseline → candidate | Median change | p95 baseline → candidate | Candidate faster pairs |
| --- | ---: | ---: | ---: | ---: | ---: |
| Initial | 1 | 11.511 → 8.377 | −27.2% | 24.716 → 45.004 | 44 / 48 |
| Initial | 100 | 11.465 → 8.591 | −25.1% | 16.179 → 14.171 | 43 / 48 |
| Repeat 2 | 1 | 11.052 → 8.401 | −24.0% | 17.896 → 12.188 | 46 / 48 |
| Repeat 2 | 100 | 11.533 → 8.486 | −26.4% | 21.804 → 18.040 | 46 / 48 |
| Repeat 3 | 1 | 11.339 → 8.376 | −26.1% | 21.584 → 14.380 | 45 / 48 |
| Repeat 3 | 100 | 11.304 → 8.421 | −25.5% | 16.987 → 10.889 | 47 / 48 |

All six full-catalog JSON medians are lower by 24.0–27.2%, and the candidate is faster in 271 of 288 paired samples. The initial one-step p95 regression is retained: 45.004 ms versus 24.716 ms, with a 261.007 ms candidate maximum. The same cell had lower p95 in both registered follow-up runs. Five of six p95 values are lower; this evidence does not support a global tail-latency claim.

The controls are mixed. Across the six observations in each group, median changes range from −6.5% to +2.3% for ordinary check with the full catalog, −7.2% to +0.1% for JSON with the inert catalog, and −1.0% to +6.5% for ordinary check with the inert catalog. The result is specific to static `check --json` with this 75-manifest workload; it says nothing about applet execution, general runtime speed, CPU use, or memory use.

## Collection and provenance

Each run contains eight cells, 48 baseline/candidate pairs per cell, five excluded warmups per side per cell, and one preflight per side per cell. The three reports retain 2,304 measured process invocations, 240 warmups, and 48 preflights (2,592 total). Each process had a 30-second timeout. The registered alternating order, raw stdout/stderr bytes, statuses, per-cell summaries, and fixture effects are retained. All three runs passed the independent JSON/ordinary-check output oracle, exact paired-byte parity, status, and no-effects checks.

The baseline source revision is `58b42f65d8c4de171baf8f3049ab70cf37c51af4`; its Execute binary SHA-256 is `c1b39031fa4f7924093179c1aeb57fe92d01ad85e1ecb834b0d72968c309ed69`. The candidate was the reviewed staged source tree `ed2d0882d3d799cae321dca4f95fe6e6767f46b8`; its Execute binary SHA-256 is `5a2d2d0eea72e7154f4cca26aaa7fa12e8329423917c2e22d45f346fe8b6a331`. The two helper binary hashes, full source and fixture file maps, environment, and remaining artifact pins are in [`registration/source-artifact-pins.json`](registration/source-artifact-pins.json). The reviewed candidate patch is [losslessly compressed candidate patch](evidence/check-json-reuse-final.patch.xz).

The first run is the initial report; `repeat-2` and `repeat-3` were registered after the first run exposed its one-step p95 regression. All three runs remain in the record, but the repeated block is not a wholly predeclared independent multi-run study. The evidence is from one macOS ARM64 host, without formal confidence intervals or other-platform performance results. See the full independent review in [`registration/adversarial-review.json`](registration/adversarial-review.json); it also records separate desktop and package CI context, which is not latency evidence.

The CLI optimization was subsequently merged as [commit `519eb37`](https://github.com/monstercameron/NextGenScriptingLanguage/commit/519eb37959191ca692647fa2e66c435d7771a86f); the measurements identify the earlier isolated candidate tree rather than that later integration commit.

The two binaries were built in different checkout directories. This is a paired whole-artifact comparison, with ordinary-check and inert-catalog controls; it does not isolate an individual Rust function. Original binaries are identified by hash but are not distributed here. Rebuilding the same source on another host or in another directory may produce different binary hashes and requires a separately registered measurement with new pins.

The original preregistration, premeasurement retention amendment, fixed-replication registration/receipt, exact source-control driver and tests, original existing `source_pair.py` harness, and frozen fixture catalogs/programs are included. The premeasurement amendment records five focused fake-runner tests passing before the first measurement. The executable binaries are not bundled; use the hashes above and the full pins before any separate reproduction.

## Offline verification

From this directory, run:

```sh
python3 audit.py
```

The stdlib-only auditor verifies `SHA256SUMS`, decompresses each raw report and checks its original SHA-256, recomputes sample and paired statistics, checks all raw output/status/effect records against the independent expected-output contract, verifies fixtures and registration receipts, and compares `summary.json` with its independent recomputation. It launches no product process and performs no timing.

The retained driver tests are in `control/test_control.py`. They need the pinned baseline source checkout (for the existing `benchmarks/source_pair.py`) and can be run with:

```sh
CHECK_JSON_PRIMARY_ROOT=/path/to/baseline-58b42f65d8c4de171baf8f3049ab70cf37c51af4 \
  python3 -m unittest discover -s control -p 'test_control.py'
```

To reproduce a measurement, obtain both pinned source trees and already-built Execute/helper binaries, verify them against the pins, select a new nonexistent report path, and use the retained driver. It refuses to overwrite reports and requires the explicit idle-window flag. Do not use `--prepare-fixtures`: the original frozen fixtures are included.

```sh
xz -dc evidence/check-json-reuse-final.patch.xz > /path/to/candidate.patch
python3 control/driver.py \
  --primary-root /path/to/baseline-58b42f65d8c4de171baf8f3049ab70cf37c51af4 \
  --candidate-root /path/to/reviewed-candidate-ed2d0882d3d799cae321dca4f95fe6e6767f46b8 \
  --baseline-binary /path/to/baseline/target/release/execute \
  --candidate-binary /path/to/candidate/target/release/execute \
  --candidate-patch /path/to/candidate.patch \
  --report /path/to/new-unique-report.json \
  --confirm-idle-window
```

The original report hashes after XZ decompression are:

| Raw report | SHA-256 |
| --- | --- |
| Initial | `63e2e10227541f97eada1d4d79c391a0ac92c1afb16e08d94900a8d91e466f4b` |
| Repeat 2 | `022c790c3c0c05f43a6f7479d3280c6a14cad3092e2ad3b60bc6689e53e68310` |
| Repeat 3 | `ca04d8b04da8207790b3f7ce775de316f03a1b2cf4f2319da95bbf2317f4dd7b` |
