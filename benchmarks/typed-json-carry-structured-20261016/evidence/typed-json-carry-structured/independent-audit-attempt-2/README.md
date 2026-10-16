# Independent audit, exact median precision

This attempt preserves the complete source/build/preregistration/raw-invocation audit from attempt 1 and additionally writes even-count medians as exact decimal strings. It reads the frozen artifacts and recomputes every timing summary; it does not build or execute Rust products.

From the campaign root, reproduce with a fresh destination:

```sh
python3 work/audit_typed_json_carry_structured_trial.py \
  --output evidence/typed-json-carry-structured/independent-audit-attempt-3/audit.json
```

Audit script SHA-256: `dbe38c38557cda980682e724ca709504bea793a3fe573a650b7e4d45cf01e815`  
This receipt SHA-256: `36b8eec7645b98f836e1f00c9655135f6ecd9e653f7097de0c42b71b31d89b0f`  
Raw paired report SHA-256: `82c9b545187b57c40e42820812d1ec0bf547fe6b4f94f83728114f1f5f8e3146`  
Preregistration SHA-256: `b2425cfd8a798da4a5269f60b97e8d66faa1fb243caaa1a7e2edbfd24bcecf11`

All 102 attempts succeeded; 3 pairs were warmups and 48 were measured. Measured order was balanced at 24 AB and 24 BA. The candidate won all 48 pairs. The exact median candidate-minus-baseline delta is **-14,221,270.5 ns** (the original runner summary stores the truncated integer -14,221,270 ns). The median of paired candidate/baseline ratios is 0.7371399229, corresponding to 26.286% lower durations on this workload. Exact median invocation durations are 54,809,833.5 ns for baseline and 40,102,312.5 ns for candidate.

The evidence is limited to the mechanistic public-runtime workload of 1,000 uncached in-process `csv.parse` calls over a fixed 64-row by 4-column CSV. All 1,000 distinct result tables remain live because the DSL requires unique binding names. This is not evidence of faster CLI startup, default catalog loading, or general runtime performance.

The adjacent `audit.json` retains all invocation samples, pair medians/deltas/ratios/orders, source provenance, and 3,916 independently passed checks.
