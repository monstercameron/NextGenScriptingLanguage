# Archived one-off experiment workflows (issue #441)

These workflows are manual-only (`workflow_dispatch`) one-off experiment
replications. They were moved out of `.github/workflows/` so a push or pull
request no longer pays for them and the workflow list contains only workflows
that run on a schedule or trigger (plus reusable workflows called by one of
those).

They are kept here as evidence, with their frozen inputs and pinned actions
intact, and can still be run by copying one back under `.github/workflows/`
and dispatching it manually:

- `codegen-experiments.yml` — Linux dynamic/static-PIE/musl comparison.
- `emit-perf-hosted-replication.yml` — fixed 48-pair emit benchmark replication.
- `graph-collector-e2e-hosted-replication.yml` — fixed 48-pair graph transport
  end-to-end comparison.
- `graph-recv-any-hosted-paired.yml` — Linux hosted paired `recv_any`
  benchmark.
- `literal-json-arguments-hosted-replication.yml` — fixed 48-pair literal
  argument comparison.
- `mixed-runtime-probe.yml` — mixed runtime lifecycle probe (bounds checked by
  `tests/mixed_stress_evidence.py`).
- `profile.yml` — perf and strace hot-path profile.
- `real-bash-100-runner-contract.yml` — fixture-only runner self-test.

Do not add new workflows here. New experiments belong in a branch, and only
workflows with a schedule or trigger belong in `.github/workflows/`.
