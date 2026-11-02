# Demo: an Omarchy-style theme refresh

This demo runs a theme refresh end to end with fake desktop tools. It touches no real
compositor and needs no network and no models. The fake `hyprctl` lives in
`fixtures/bin/`.

`theme-refresh.program` does four things:

1. Creates a theme directory and renders `templates/*.tpl` into it with `text.render`.
2. Points the current-theme link at the new directory with `fs.ensure_symlink`.
3. Reloads the compositor with `hyprctl reload`. This is the step that can fail.
4. Prints the active theme.

Its leading `# execute-policy: allow reads writes process undeclared` header declares the categories the
author intends to use: the program reads templates, writes files and starts a process. The header never approves
anything; approvals come only from `--allow`. The direct commands
`hyprctl` and `printf` state no effects of their own, so the gate counts them as
`undeclared`, and the header declares them openly. The reload fails until
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
6. **resume**: `execute resume --checkpoint DIR --retry-uncertain` continues from the
   failed step. The reload is a native command that is not retry-safe, so running it
   again needs `--retry-uncertain`.
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
demo_root=$(mktemp -d "${TMPDIR:-/tmp}/execute-demo-manual.XXXXXX")
mkdir -p "$demo_root/themes" "$demo_root/current"
program="$demo_root/theme-refresh.program"
cp examples/demo/theme-refresh.program "$program"
templates="$PWD/examples/demo/templates/*.tpl"
values_json='{"background":"#1e1e2e","foreground":"#cdd6f4"}'

# This function uses POSIX shell argument passing, so each input remains one
# argument in sh, Bash and zsh. The quoted glob is passed literally to fs.glob.
with_demo_inputs() {
  "$@" \
    --input theme_name=dark \
    --input "themes_root=$demo_root/themes" \
    --input "current_link=$demo_root/current/theme" \
    --input "templates=$templates" \
    --input "values_json=$values_json"
}

with_demo_inputs execute check "$program"
with_demo_inputs execute run "$program" --what-if
with_demo_inputs execute run "$program" --confirm FINGERPRINT --checkpoint "$demo_root/state"
execute repair "$demo_root/state" --op 'replace-command 23 run "hyprctl" with arguments ["reload", "--recover"]'
with_demo_inputs execute resume "$program" --checkpoint "$demo_root/state" --retry-uncertain
```

The line number in the repair is the `run "hyprctl"` line of the program. The walkthrough
finds it with `grep`.

## What exists and what does not

- `check` validates the program. `check --json` prints the diagnostic record, and each
  diagnostic may carry suggested fixes.
- `explain` prints one line per resolved step. `explain --json` gives the same plan as
  JSON, and `--policy-header` makes the program's policy header a declaration: any category it does not declare is a violation.
- `run --checkpoint` and `resume --checkpoint` record completed steps and restart from the
  failed one. The checkpoint records the program text and its inputs. A repair may change
  only the failed or unexecuted suffix, so completed steps keep their fingerprints.
- The fake `hyprctl` stands in for the compositor. A real reload needs a running
  Hyprland session, which CI does not have.
- The CI tests run the walkthrough (`tests/demo_walkthrough.rs`) and the step-by-step
  checks (`tests/lane_a_demo.rs`) on Unix only, because the fake tool is a shell script.
  On other platforms those tests are skipped.
