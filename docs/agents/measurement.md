# Measurement of the agent pack: model-free half

This page records the model-free part of the measurement for issue #432. The issue asks for a
tested onboarding pack for agents, measured by having an agent port Omarchy scripts with and
without the pack. Only the model-free half is committed here. No model has been run, and no
transcripts exist yet.

## What is measured

The ten `.program` files under `examples/omarchy-ports/` are the ported programs. For each one
the table records:

- **Parses**: whether `flow::parser::parse` accepts the source.
- **Diagnostics**: how many diagnostics the static checker reports for the file.
- **Errors**: how many of those have severity `error`. A program passes `execute check` when
  this is zero, which is the rule `check --json` uses for its `ok` field.
- **Codes**: the distinct diagnostic codes reported, or `none`.

The checker is the one `execute check` runs: `flow::diagnostics::analyze_source_at` on the
source, with the file path passed so that called files resolve.

## Limits of this measurement

- The checker runs against an empty applet registry (`Registry::default()`). The default
  manifest directory is refused by the trust gate on the machine where the test was written, so
  diagnostics that depend on applet manifests are not in the table. A run of `execute check`
  with the bundled or repository manifests can report more.
- The table says what the checker reports today. It says nothing about whether a model, given
  the pack, would write these programs on the first try.
- Not measured: first-pass `check` validity by an agent, the compare-suite pass rate, and any
  comparison with and without the pack. These need model runs and are a later step.

## Table

| Program | Parses | Diagnostics | Errors | Codes |
|---|---|---|---|---|
| examples/omarchy-ports/omarchy-hyprland-window-gaps-toggle.program | yes | 0 | 0 | none |
| examples/omarchy-ports/omarchy-launch-nautilus.program | yes | 0 | 0 | none |
| examples/omarchy-ports/omarchy-launch-terminal-herdr.program | yes | 0 | 0 | none |
| examples/omarchy-ports/omarchy-menu-clipboard.program | yes | 0 | 0 | none |
| examples/omarchy-ports/omarchy-menu-emoji.program | yes | 0 | 0 | none |
| examples/omarchy-ports/omarchy-notification-dismiss.program | yes | 0 | 0 | none |
| examples/omarchy-ports/omarchy-notification-weather.program | yes | 0 | 0 | none |
| examples/omarchy-ports/omarchy-powerprofiles-init.program | yes | 0 | 0 | none |
| examples/omarchy-ports/omarchy-remove-gaming-xbox-cloud.program | yes | 0 | 0 | none |
| examples/omarchy-ports/omarchy-restart-hyprsunset.program | yes | 0 | 0 | none |

Every program parses and produces no diagnostics under the empty registry. The zero counts come
from this configuration and do not show that the programs were checked against the applet
manifests, so they are not evidence of a clean check in the full CLI sense.
