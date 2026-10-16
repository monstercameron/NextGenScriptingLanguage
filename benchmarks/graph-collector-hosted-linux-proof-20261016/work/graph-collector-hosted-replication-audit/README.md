# Hosted graph E2E ZIP audit

This is a read-only audit for the fixed Linux Actions run `38878319193` at workflow commit `7ce98bfbbba025f4c07f1bee991d32ff5d958e60`. It hashes every ZIP member, validates source and build provenance against the frozen local checkouts and receipts, and recomputes the 3 warmup plus 48 measured AB/BA pairs from the raw sample medians.

The tool does not extract the artifact. It streams all ZIP members for SHA-256 and CRC validation, compares every retained fixture byte-for-byte, then reads one raw report at a time. The frozen independent sample auditor receives only that single report and its fixture/journal in a temporary directory; the directory is removed before the next report. No Cargo build or saved Linux binary is run.

Run from the campaign directory after the root agent has saved the original Actions artifact ZIP and confirmed the run completed:

```sh
python3 -B work/graph-collector-hosted-replication-audit/audit_hosted_graph_transport_e2e_zip.py \
  --artifact-zip evidence/graph-transport-e2e/hosted-linux-38878319193-1.zip \
  --output evidence/graph-transport-e2e/hosted-linux-38878319193-independent-audit.json
```

The command refuses to overwrite the audit receipt. It requires the existing baseline and candidate worktrees, exact frozen runner/helper/harness/patch files, and the prior independent per-sample oracle auditor. A complete report will include each ZIP member's size, CRC, and SHA-256; all 102 attempt hashes and per-sample durations; all 48 recomputed pair ratios, wins, losses, and ties; and explicit scope limits. It does not claim a general runtime speedup.

Offline checks for this auditor:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -B -m unittest discover \
  -s work/graph-collector-hosted-replication-audit -p 'test_*.py'
```
