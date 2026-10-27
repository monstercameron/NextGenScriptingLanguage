# Execute/Bash ratio baseline

`tools/check_bash_ratio_regression.py` compares two JSON reports emitted by
`tools/bash_compare.py`. It fails when any scenario's Execute/Bash median ratio
is more than 10% worse than the stored report. It rejects reports from a
different OS family, machine architecture, manifest selection, or scenario set.

The current checkout has no retained Linux ratio report suitable for the
Ubuntu x86_64 and arm64 CI lanes. The committed
`bash-compare-macos-arm64-20261027.json` is deliberately not used as a Linux
baseline. Do not add the ratio gate to those CI lanes until the baseline reports
come from verified Linux CI runs for both architectures. After review, retain
each report as a baseline file and invoke:

```sh
python3 tools/check_bash_ratio_regression.py BASELINE.json CANDIDATE.json
```

This keeps the threshold fixed at 10% while avoiding invented or cross-platform
measurements. The existing `tools/linux-latency-budget.json` thresholds also
remain unchanged; the 8 ms ten-step cap still needs a measured fix and green
Linux CI before #518 can close. Manifest-directory rows inherit the matching
existing budget so adding coverage does not loosen the gate.
