# Equal command plan control

This small follow-up control holds the external command plan constant across the current-source Execute binary, Bash, and Nushell. It reuses the public fixture definitions and independent output/effect checks from `benchmarks/matched_language.py`; it does not edit or replace the hand-authored public references or the completed 987 report.

The three Execute sources call `find -> sort -> tee` for inventory and `jq` with a file argument followed by `tee` for the two JSON workflows. They use the same input files and `reports/...` output paths as the checked-in Bash and Nushell scripts. Execute calls are direct and quoted, with no internal `save` stage. The harness checks those source properties and verifies Bash/Nushell sources remain byte-identical to the public references before any process is launched.

Each fixture runs four paired arms: Execute with the supplied full catalog, Execute with `catalogs/minimal-unreferenced`, Bash, and Nushell. The latter catalog contains exactly the unchanged repository `cat.json`; no controlled command references `cat`. The explicit CLI rejects a physically empty manifest directory, so this is an empty-of-referenced-applets diagnostic, not an empty catalog and not evidence about default runtime startup. Bash and Nushell each run once per fixture round because they do not read the Execute catalog; their same-round results pair with both Execute catalog arms.

An actual run is gated by `--confirm-idle-window`. Defaults are 5 warmups and 48 measured samples per arm and fixture. The harness cycles all 24 permutations of the four arms; 48 measured rounds therefore use each order twice. Warmups are excluded from those 48 rounds, and fixture order alternates by round. With six fixtures, the default produces 1,152 measured invocations plus 120 warmups. Independent fixture oracles run across every available arm before a report is created and again on every warmup/sample. The report retains source and catalog fingerprints, environment/version data, raw stdout/stderr/status/effects and elapsed wall time, sample summaries, and paired Execute catalog deltas. The driver reports wall time only; it does not infer CPU time or peak RSS. Missing languages or workload tools are recorded as unavailable. Sample counts must be multiples of 24 to preserve complete counterbalance.

Static tests do not execute the product binaries:

```sh
FLOW_THESIS_REPO_ROOT=/path/to/flow \
  python3 -m unittest discover -s benchmarks/equal_command_plan_control/tests
```

After builds and tests stop and the measurement window is explicitly declared idle, run with the current-source binary, source checkout, supplied full catalog, and a new report path:

```sh
python3 benchmarks/equal_command_plan_control/equal_command_plan.py \
  --repo-root /path/to/flow \
  --execute-source-root /path/to/current-source \
  --execute-binary /path/to/execute \
  --manifest-dir /path/to/full-manifests \
  --execute-revision REVISION-LABEL \
  --bash-binary /bin/bash \
  --nushell-binary /path/to/pinned-nu \
  --nushell-archive /path/to/pinned-nu-archive \
  --nushell-archive-sha256 PUBLISHED-ARCHIVE-SHA256 \
  --nushell-release-url https://github.com/nushell/nushell/releases/download/VERSION/ASSET \
  --report /path/to/new-equal-command-plan-report.json \
  --confirm-idle-window
```

The report path must not already exist. The harness hashes binaries, helper files, tool executables, control sources, both catalogs, and the current-source closure before and after the run, and fails if any change. It never changes source code or build outputs.

The retained report can be audited without running any product commands or timed workloads. From the repository root, run:

```sh
python3 benchmarks/equal_command_plan_control/validation/audit_equal_command_plan.py \
  --report benchmarks/equal_command_plan_control/raw/equal-command-plan-macos-arm64-58b42f6.raw.json.xz \
  --summary benchmarks/equal_command_plan_control/summary.json
```

When installed at `benchmarks/equal_command_plan_control/validation`, the auditor infers the repository root from that location. It compares each recorded fixture's expected output and input-file hashes with scenarios loaded from the checked-in `benchmarks/ai_productivity/public-development.json`. The report records the task specification SHA-256 `e02fc57cb4a40c5ee7f4deb4dc82c80f8a2f649bc614b3adb88295f3ccff1e84`; the auditor requires that exact pinned hash before accepting the fixture binding. For a copy outside the repository, pass `--repo-root /path/to/flow` explicitly.
