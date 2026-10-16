# Literal JSON argument hosted Linux replication

This staged workflow runs one fixed Linux replication of the literal-only JSON argument runtime change. It builds both frozen source arms from base commit `b824aeaa7bccaa4d3d75d3fc6a363a99138287d4`, applies their hash-pinned source patches, and saves separate Cargo release library-test receipts and immutable binaries. The frozen paired runner then performs three warmup pairs followed by 48 measured pairs, alternating arm order. Each invocation has a 120-second cap; there are no retries and the runner has a three-hour total budget.

The experiment uses one benchmark program that repeatedly calls a numeric-add task with literal JSON arguments. Its fixed output and call-count oracle requires output `3`, 1,000 completed calls, and zero child launches. The runner sets zero inner warmups and three inner samples. It records all raw attempts and reports paired candidate-minus-baseline deltas descriptively; it makes no confidence-interval or general-workload claim.

The staged tree includes the exact runner and tests from `work/literal-json-paired-runner`, the identical harness and workload sources copied from both frozen arms, and XZ-compressed source patches generated from those frozen working trees. The helper checks each compressed archive and decompressed patch against separate SHA-256 pins. `hosted_support.py` applies each patch to a clean base checkout, fingerprints build inputs, performs one release library-test build per arm in separate fresh targets, and retains Cargo JSONL, stderr, command, tool-version, target freshness and receipt evidence. It never starts the benchmark itself.

The workflow is `workflow_dispatch` only and has no source, profile, workload, sample-count or timeout override. The paired runner owns the fixed protocol. This directory is staged for review only; it has not been copied into the main worktree or dispatched.

Offline Python contract tests:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -B -m unittest discover -s benchmarks/runtime_json_arguments -p 'test_resolved_json_arguments_pair.py'
PYTHONDONTWRITEBYTECODE=1 python3 -B -m unittest discover -s benchmarks/runtime_json_arguments -p 'test_hosted_support.py'
```
