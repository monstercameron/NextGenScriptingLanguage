# Demo: an Omarchy-style theme refresh

This demo runs a theme refresh end to end with fake desktop tools. It touches no real
compositor, and the fake `hyprctl` lives in `fixtures/bin/`.

`theme-refresh.program` does four things:

1. Creates a theme directory and renders `templates/*.tpl` into it with `text.render`.
2. Points the current-theme link at the new directory with `fs.ensure_symlink`.
3. Reloads the compositor with `hyprctl reload`. This is the step that can fail.
4. Prints the active theme.

The reload fails until the fake compositor is fixed. A checkpointed run stops there,
and `execute resume` continues from the failed step once it succeeds.

## Inputs

| Input | Meaning |
| --- | --- |
| `theme_name` | Directory name for the theme, for example `dark`. |
| `themes_root` | Directory that holds the theme directories. |
| `current_link` | Path of the current-theme symbolic link. |
| `templates` | Glob of templates, for example `examples/demo/templates/*.tpl`. |
| `values_json` | JSON object of template values, for example `{"background":"#1e1e2e","foreground":"#cdd6f4"}`. |

## Walk-through

Run these from the repository root. `PATH` gets the fake tools first, and
`DEMO_LOG` records their calls.

```sh
export PATH="$PWD/examples/demo/fixtures/bin:$PATH" DEMO_LOG=/tmp/demo.log
DEMO_INPUTS="--input theme_name=dark --input themes_root=/tmp/demo/themes \
  --input current_link=/tmp/demo/current/theme \
  --input templates=$PWD/examples/demo/templates/*.tpl \
  --input values_json={\"background\":\"#1e1e2e\",\"foreground\":\"#cdd6f4\"}"

# 1. Check: static validation, no applets or effects.
execute check examples/demo/theme-refresh.program $DEMO_INPUTS

# 2. Explain: the resolved steps, without running them.
execute explain examples/demo/theme-refresh.program $DEMO_INPUTS

# 3. Run with a checkpoint. The reload fails, so the run stops after the link swap.
execute run examples/demo/theme-refresh.program $DEMO_INPUTS --checkpoint /tmp/demo/state

# 4. Fix the compositor and resume from the failed step.
DEMO_RELOAD_OK=1 execute resume examples/demo/theme-refresh.program $DEMO_INPUTS --checkpoint /tmp/demo/state
```

After step 4 the link points at `/tmp/demo/themes/dark`, and the rendered files hold the
values you passed.

## What exists and what does not

- `check` validates the program. `check --json` prints the diagnostic record.
- `explain` prints text: one line per resolved step. A machine-readable `explain --json`
  is planned as issue #23 and is not in this tree, so the walk-through uses text.
- `run --checkpoint` and `resume --checkpoint` record completed steps and restart from the
  failed one. The checkpoint records the program text and its inputs. Editing a completed
  step invalidates the checkpoint, so fix the environment rather than the program.
- The fake `hyprctl` stands in for the compositor. A real reload needs a running
  Hyprland session, which CI does not have.
- The CI test (`tests/lane_a_demo.rs`) runs this walk-through on Unix only, because the
  fake tool is a shell script. On other platforms the test is skipped.
