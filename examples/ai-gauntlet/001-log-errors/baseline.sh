#!/usr/bin/env bash
# Bash baseline for examples/ai-gauntlet/001-log-errors.program.
#
# Classifies one log line with fixed keyword rules and prints exactly one of:
#   error | warn | noise | abstain
# "abstain" means no rule matched; a human or a model would have to decide.
# Input: the line as $1, or on stdin when no argument is given.
# No LLM, no network. Rules are deliberately plain keyword matches, the same
# kind of rules a shell author would write without a model.

shopt -s nocasematch

if [ "$#" -ge 1 ]; then
  line="$1"
else
  line="$(cat)"
fi

case "$line" in
  *error*|*fatal*|*panic*|*exception*|*traceback*|*segmentation\ fault*)
    printf '%s\n' error ;;
  *warn*|*deprecat*)
    printf '%s\n' warn ;;
  *debug*|*info*|*trace*|*heartbeat*|*health\ check*)
    printf '%s\n' noise ;;
  *)
    printf '%s\n' abstain ;;
esac
