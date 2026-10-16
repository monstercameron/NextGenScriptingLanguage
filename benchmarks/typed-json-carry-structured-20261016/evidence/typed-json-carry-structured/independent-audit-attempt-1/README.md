# Independent audit: structured CSV typed-carry trial

This audit independently rechecks the frozen source trees, build receipts and Cargo JSONL artifact rows, preregistration, runner pins, all 102 raw invocations, their exact stdout/stderr/raw-report bytes, semantic oracle values, fixed AB/BA schedule, and the 48 measured-pair deltas and ratios. The audit does not build or execute the Rust product.

From the campaign root, rerun the read-only audit with a new output path:

```sh
python3 work/audit_typed_json_carry_structured_trial.py \
  --output evidence/typed-json-carry-structured/independent-audit-attempt-2/audit.json
```

The standalone script is `work/audit_typed_json_carry_structured_trial.py`. Its SHA-256 is `f8863789691322ba82802a59cbf4d1a6fa0d2e4d9d043897c2f632d7c9cc4749`.

The trial has 3 warmup pairs and 48 measured pairs, with measured order balanced at 24 AB and 24 BA. All 102 invocations succeeded and passed their output/runtime oracles. The candidate won 48/48 measured pairs. The independently recomputed median candidate-minus-baseline delta is -14,221,270 ns; the median of the 48 paired candidate/baseline ratios is 0.7371399229 (26.286% lower). Median invocation durations are 54,809,833 ns for baseline and 40,102,312 ns for candidate.

Interpretation is limited to this mechanistic public-runtime workload: 1,000 uncached in-process `csv.parse` calls over a fixed 64-row, 4-column CSV. The DSL requires distinct binding names, so all 1,000 result tables remain live. This evidence does not establish faster CLI startup, default catalog loading, or general runtime performance.

## Frozen inputs

- Raw paired report SHA-256: `82c9b545187b57c40e42820812d1ec0bf547fe6b4f94f83728114f1f5f8e3146`
- Preregistration SHA-256: `b2425cfd8a798da4a5269f60b97e8d66faa1fb243caaa1a7e2edbfd24bcecf11`
- Baseline and candidate source base: `e5fa5162b9bdca608f7f3c9d0d273db40a93c17b`
- Product patch SHA-256: `99650ce4d9c2a2d4c8418e52bc3dbbda2f99a2a745e54c48332f9dc6a1958bb9`
- Source bindings SHA-256: `dc9fc282efbcf2cb791eb2e1fba5a5e4b72d2c638bd97ef0eee5e9c300a04dd6`
- Build driver SHA-256: `72b67e06ca7d658bf01b5166f2d6312cdbd6c21e62ae982f4b97b23ccc05ebaa`
- Paired runner SHA-256: `8fb1f0b624179d9fd62c0aa2f538b08c75aa44a826466592022e4d922bb3136c`

The adjacent `audit.json` retains all recomputed invocation samples, every measured pair's arm medians/delta/ratio/order, source file maps and build provenance, and the 3,916 passing audit checks.
