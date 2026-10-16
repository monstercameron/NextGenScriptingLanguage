# Independent `recv_any` trial audit

`audit_graph_recv_any.py --source-only` checks the fixed preregistration, exact
runner/build/harness pins, both source worktrees and complete tracked build
input fingerprints, Cargo artifact rows (`fresh: false`, optimized test
profile), immutable saved-binary hashes, compiler receipts, build logs, and
source-composition patch pins. It deliberately does not inspect the active
trial attempt directory.

After the paired runner reports `status: complete`, run the script without
`--source-only` to independently validate all 102 retained invocations and
all 306 timed samples. Full mode checks its small paired summary first and
refuses to open attempt data unless the status is complete. It then checks
the exact deterministic 4096-record fixture, 8193 terminal events per sample,
both complete fanout identity streams, payload/order hashes, ports, generation,
revision, sensitivity and remote metadata, one successful producer process,
zero drops, natural completion status as asserted by the frozen harness,
per-attempt JSONL journals and output artifacts. Finally it recomputes the
AB/BA schedule, per-invocation medians, every paired delta, loss/tie counts,
and the median paired delta. It does not import the paired runner's validator.

The report measures finite `GraphSession` execution from `start` through
`run_to_completion`, including process startup, typed JSON decoding, two
bounded lossless fanout edges, two `recv_any` consumers, and terminal
collection. It is one host/run and does not establish broad workload results
or a Bash-versus-Rust comparison. The repeated status payload is not included
in raw sample JSON; the frozen compiled test asserts the producer status is
`Integer(0)` and the independent audit checks the successful test output,
status identity, sequence order, and one process launch.

Source-only validation used:

```sh
python3 -m py_compile work/graph-recv-any-independent-audit/audit_graph_recv_any.py
python3 work/graph-recv-any-independent-audit/audit_graph_recv_any.py --source-only
python3 -m unittest discover -s work/graph-recv-any-independent-audit -p 'test_*.py'
```

Full audit command after the parent confirms the paired trial is complete:

```sh
python3 work/graph-recv-any-independent-audit/audit_graph_recv_any.py \
  --report evidence/graph-recv-any-single-queue/paired-run-attempt-1/paired-report.json \
  --output evidence/graph-recv-any-single-queue/independent-audit-attempt-1.json
```
