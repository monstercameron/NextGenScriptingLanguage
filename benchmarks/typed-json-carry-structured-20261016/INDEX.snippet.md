## Typed JSON carry performance evidence (separate cases)

- **Structured output:** [`csv.parse`, 1,000 calls](typed-json-carry-structured-20261016/README.md) — 48/48 paired wins, median paired ratio 0.7371 (about 26.3% lower), exact median delta −14.2212705 ms. Narrow public-runtime case; all 1,000 result tables remain live. Later integrated in commits `97df` and `c719653` on a newer base; these timings remain bound to the measured `e5fa` source.
- **Scalar comparison:** [literal `number.add`, 1,000 calls](typed-json-carry-scalar-mixed-20261016/README.md) — 27 wins / 21 losses, median paired ratio 0.97568; order-dependent and not robust.

These are distinct workloads and separate trials. Do not combine their percentages or infer general runtime performance from the structured result.
