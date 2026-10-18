#!/bin/sh
# Walk the whole loop with the theme refresh: write the program, validate it, review
# its plan with --what-if, run exactly that plan with --confirm, watch the run fail at
# the compositor reload, repair the failed step, and resume from the checkpoint.
#
# No network and no models. The compositor is the fake `hyprctl` in fixtures/bin.
# Usage: sh examples/demo/walkthrough.sh   (set EXECUTE to choose the binary)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
execute=${EXECUTE:-execute}
work=$(mktemp -d "${TMPDIR:-/tmp}/execute-demo.XXXXXX")
program="$work/theme-refresh.program"
checkpoint="$work/checkpoint"
themes="$work/themes"
link="$work/current/theme"
templates="$here/templates/*.tpl"
values='{"background":"#1e1e2e","foreground":"#cdd6f4"}'
export PATH="$here/fixtures/bin:$PATH"
export DEMO_LOG="$work/calls.log"

finish() {
  status=$?
  if [ "$status" -eq 0 ]; then
    rm -rf "$work"
  else
    echo "demo failed; the work folder is kept at $work" >&2
  fi
}
trap finish EXIT

fail() {
  echo "demo check failed: $*" >&2
  exit 1
}

# The demo's inputs, passed to every verb, so the plan and its fingerprint stay the same.
run_execute() {
  verb=$1
  shift
  "$execute" "$verb" "$program" --manifest-dir "$repo/manifests" \
    --input theme_name=dark \
    --input themes_root="$themes" \
    --input current_link="$link" \
    --input templates="$templates" \
    --input values_json="$values" \
    "$@"
}

echo "== 1. write: the author's program, with its policy header, goes in the work folder"
cp "$here/theme-refresh.program" "$program"
mkdir -p "$themes" "$work/current"
head -n 1 "$program" | grep -q '^# execute-policy: allow writes process undeclared$' \
  || fail "the program does not declare its approvals"

echo "== 2. validate: static checks, no effects"
run_execute check >/dev/null || fail "check rejected the program"

echo "== 3. what-if: the plan and its fingerprint, nothing runs"
run_execute run --what-if > "$work/what-if.json" || fail "--what-if failed"
fingerprint=$(sed -n 's/.*"fingerprint": "\([0-9a-f]*\)".*/\1/p' "$work/what-if.json")
[ -n "$fingerprint" ] || fail "--what-if printed no fingerprint"
[ ! -e "$link" ] || fail "--what-if created the link"
echo "   fingerprint $fingerprint"

echo "== 4. confirm: run exactly the plan that was reviewed, with a checkpoint"
if run_execute run --confirm "$fingerprint" --checkpoint "$checkpoint" 2>"$work/failed.err"; then
  fail "the reload was expected to fail"
fi
grep -q "reload failed" "$work/failed.err" || fail "the failure is not the reload"
[ "$(readlink "$link")" = "$themes/dark" ] || fail "the link swap did not happen before the failure"
echo "   the reload failed; the link already points at $themes/dark"

echo "== 5. repair: retry the failed reload with --recover, as a structured edit"
line=$(grep -n '^run "hyprctl"' "$program" | head -n 1 | cut -d: -f1)
[ -n "$line" ] || fail "the reload line was not found"
"$execute" repair "$checkpoint" --manifest-dir "$repo/manifests" \
  --op "replace-command $line run \"hyprctl\" with arguments [\"reload\", \"--recover\"]" \
  > "$work/repair.out" || fail "the repair was refused"
grep -q "repair 1 applied" "$work/repair.out" || fail "the repair was not recorded"

echo "== 6. resume: continue from the failed step"
# The failed reload is a native command, so its outcome is not retry-safe: running it
# again is an explicit decision, made here with --retry-uncertain.
out=$(run_execute resume --checkpoint "$checkpoint" --retry-uncertain) || fail "the resumed run failed"
printf '%s\n' "$out" | grep -q "theme dark active" || fail "the resumed run did not finish"

echo "== 7. verify: the link, the rendered files and the compositor calls"
[ "$(readlink "$link")" = "$themes/dark" ] || fail "the link does not point at the new theme"
grep -q '#1e1e2e' "$themes/dark/alacritty.toml" || fail "the template was not rendered"
reloads=$(grep -c '^hyprctl reload' "$DEMO_LOG" || true)
[ "$reloads" -eq 2 ] || fail "expected two reload calls, saw $reloads"
echo "   link -> $themes/dark, $reloads reload calls, rendered files present"
echo "walkthrough complete"
