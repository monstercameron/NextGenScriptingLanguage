# Demo: an Omarchy-style theme refresh

This demo runs a theme refresh end to end with fake desktop tools. It touches no real
compositor and needs no network and no models. The fake `hyprctl` lives in
`fixtures/bin/`.

`theme-refresh.program` does four things:

1. Creates a theme directory and renders `templates/*.tpl` into it with `text.render`.
2. Points the current-theme link at the new directory with `fs.ensure_symlink`.
3. Reloads the compositor with `hyprctl reload`. This is the step that can fail.
4. Prints the active theme.

Its leading `# execute-policy: allow writes process undeclared` header declares the approvals the
author expects: the program writes files and starts a process. The direct commands
`hyprctl` and `printf` state no effects of their own, so the gate counts them as
`undeclared`, and the header approves that openly. The reload fails until
the config is fixed, so a run stops there, and the checkpoint lets the run continue.

## Run the whole loop

```sh
sh examples/demo/walkthrough.sh
```

The script uses the `execute` on your `PATH`; set `EXECUTE` to choose another binary.
It works in a scratch folder, prints one numbered heading per step, and exits non-zero
at the first step that does not behave as expected. It walks:

1. **write**: the program, with its policy header, goes into the work folder.
2. **validate**: `execute check` runs the static checks with no effects.
3. **what-if**: `execute run --what-if` prints the plan and its fingerprint. Nothing runs.
4. **confirm**: `execute run --confirm FINGERPRINT --checkpoint DIR` runs exactly the
   plan that was reviewed. The reload fails, and the link already points at the new theme.
5. **repair**: `execute repair DIR --op 'replace-command LINE ...'` retries the reload
   with `--recover`. The repair is a text edit to the program, validated, shown as a
   diff, and recorded in the checkpoint folder. Only the failed step may change.
6. **resume**: `execute resume --checkpoint DIR` continues from the failed step.
7. **verify**: the link, the rendered template and the two reload calls are checked.

The fake compositor accepts `reload --recover`, which is the repair the walkthrough
applies. Setting `DEMO_RELOAD_OK=1` also makes it succeed, as a hand fix would.

## Inputs

| Input | Meaning |
| --- | --- |
| `theme_name` | Directory name for the theme, for example `dark`. |
| `themes_root` | Directory that holds the theme directories. |
| `current_link` | Path of the current-theme symbolic link. |
| `templates` | Glob of templates, for example `examples/demo/templates/*.tpl`. |
| `values_json` | JSON object of template values, for example `{"background":"#1e1e2e","foreground":"#cdd6f4"}`. |

Each input carries a `described as` description, which `execute inspect` reports.

## The same steps by hand

Run these from the repository root. `PATH` gets the fake tools first, and `DEMO_LOG`
records their calls.

```sh
export PATH="$PWD/examples/demo/fixtures/bin:$PATH" DEMO_LOG=/tmp/demo.log
DEMO_INPUTS="--input theme_name=dark --input themes_root=/tmp/demo/themes \
  --input current_link=/tmp/demo/current/theme \
  --input templates=$PWD/examples/demo/templates/*.tpl \
  --input values_json={\"background\":\"#1e1e2e\",\"foreground\":\"#cdd6f4\"}"

execute check examples/demo/theme-refresh.program $DEMO_INPUTS
execute run examples/demo/theme-refresh.program $DEMO_INPUTS --what-if
execute run examples/demo/theme-refresh.program $DEMO_INPUTS --confirm FINGERPRINT --checkpoint /tmp/demo/state
execute repair /tmp/demo/state --op 'replace-command 23 run "hyprctl" with arguments ["reload", "--recover"]'
execute resume examples/demo/theme-refresh.program $DEMO_INPUTS --checkpoint /tmp/demo/state
```

The line number in the repair is the `run "hyprctl"` line of the program. The walkthrough
finds it with `grep`.

## What exists and what does not

- `check` validates the program. `check --json` prints the diagnostic record, and each
  diagnostic may carry suggested fixes.
- `explain` prints one line per resolved step. `explain --json` gives the same plan as
  JSON, and `--policy-header` merges the program's policy header into its approvals.
- `run --checkpoint` and `resume --checkpoint` record completed steps and restart from the
  failed one. The checkpoint records the program text and its inputs. A repair may change
  only the failed or unexecuted suffix, so completed steps keep their fingerprints.
- The fake `hyprctl` stands in for the compositor. A real reload needs a running
  Hyprland session, which CI does not have.
- The CI tests run the walkthrough (`tests/demo_walkthrough.rs`) and the step-by-step
  checks (`tests/lane_a_demo.rs`) on Unix only, because the fake tool is a shell script.
  On other platforms those tests are skipped.
