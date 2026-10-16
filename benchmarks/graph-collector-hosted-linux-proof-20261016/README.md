# Hosted Linux collector E2E proof package

This source-and-evidence package preserves one successful, fixed-workload hosted Linux replication for the graph collector queue change. It is a narrow end-to-end result, not a general application or runtime performance claim.

The audited run is GitHub Actions run **38878319193**, workflow commit **`7ce98bfbbba025f4c07f1bee991d32ff5d958e60`**, on **Linux x86_64** (`x86_64-unknown-linux-gnu`). The workload sends 4,096 fixed-size text records through a `NativeStream` producer (`/bin/cat`) to two consumers using lossless edges of capacity 8. The frozen protocol used three warmup pairs and 48 measured pairs, alternating AB/BA order; each side collected three raw samples, with a 120-second invocation cap and no retries.

All 102 invocations completed and passed the exact end-to-end payload, ordering, metadata, lifecycle and terminal-event oracles. The candidate won all 48 measured pairs, with zero losses and ties. The median paired candidate/baseline ratio was **0.038756924889937844** (candidate-minus-baseline median **−8,123,707,880.5 ns**). The baseline median of invocation medians was **8,451,007,088.5 ns**; the candidate median was **327,505,723.5 ns**. This measures the fixed 4,096-record workload only.

The full original hosted ZIP is `evidence/hosted-linux-run-38878319193.zip`. It is 31,262,576 bytes with SHA-256 `8ac90ccb74df38a94aff688223bd6b4851af29925d79b4e7990819a96a40ef0e`. `evidence/hosted-linux-independent-audit.json` records the independent raw audit and per-member hashes. It is accompanied by the ZIP-streaming auditor, its tests, the frozen preregistration, exact workflow/helper/runner/harness sources, the compressed candidate patch, build receipts/logs, source fingerprints and both transport source snapshots. The portable `verify_bundle.py` checks every packaged file and streams every member in the original ZIP; it does not extract the expanded evidence or execute a product binary. `audit/auditor-repair-history.json` records the known earlier audit failures and repair details. Intermediate auditor source copies and full terminal logs were unavailable in the source campaign at packaging time; the history note distinguishes recorded hashes from an unverified reconstructed hash. The exact per-sample oracle and campaign-relative auditor/runner paths are also included so the eight-test offline suite can run from this package. Reproducing the full independent audit still requires the two complete pinned source checkouts; this bundle only includes their exact transport source files and source-fingerprint receipts.

Two earlier hosted runs are preserved under `evidence/preflight-failures/`. Run 38871182433 (workflow commit `0a5f1c572ef14689a13ac66e712b8b681ead4701`) stopped after the baseline build because the helper called a nonexistent runner method. Run 38872584428 (workflow commit `05ab086d69c91f28d88af5d822dff77dcc5b5bdc`) stopped after both arms built because the helper wrote the build-environment receipt to the wrong location. Neither run had a timed benchmark attempt. Their failure logs, dispatch records, source-preservation receipts, changed helper/workflow versions, and nonbinary build metadata are retained. The final corrected run is the only Linux timed run in this package.

A separate **macOS arm64** run is linked [here](../graph-collector-queue-macos-arm64-20261016/README.md). It used a different host and is a separate replication; its results are not pooled with the Linux run or used to compute the Linux metrics above. `references/macos-arm64-separate-trial-pointer.json` records the separate audit's digest and platform/result summary.

## Reproduction and limits

Run the bundle verifier with Python 3's standard library:

```sh
python3 verify_bundle.py
```

The eight packaged auditor tests can be run with `python3 -B -m unittest discover -s work/graph-collector-hosted-replication-audit -p 'test_*.py'`. The bundle verifier validates the package inventory, source pins, compressed patch digest and every hosted ZIP member without unpacking the archive. The full ZIP contains saved test binaries and raw trial evidence; this package does not run them. The auditor itself ran read-only and did not execute the Linux test binary or run Cargo locally.

This package does not establish performance on other workloads, machines, architectures, or operating systems. The hosted evidence is one Linux x86_64 replication. The macOS arm64 evidence remains a distinct comparison. No claim is made about general representativeness beyond this workload.

## Independent review

The first adversarial review found that an older test log showed five tests while the final source had eight, and that package discovery lacked a campaign-relative runner path. The package now retains that five-test output as a stale-layout attempt, includes the pinned sources at the expected campaign-relative paths, and records a fresh **8/8** offline test run. It also includes the per-sample oracle source and pins it in the verifier. The follow-up review reports both findings resolved with no remaining material blockers. The original review and follow-up are preserved in `review/`; the final manifest includes both receipts.
