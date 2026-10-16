#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

: "${EVAL_TARGET:?EVAL_TARGET must name the selected directory}"
mkdir -p reports

shopt -s dotglob nullglob
paths=("$EVAL_TARGET"/*)
names=()
for path in "${paths[@]}"; do
  if [[ -f "$path" && ! -L "$path" ]]; then
    name=${path##*/}
    i=${#names[@]}
    while ((i > 0)) && [[ "$name" < "${names[i-1]}" ]]; do
      names[i]=${names[i-1]}
      i=$((i - 1))
    done
    names[i]=$name
  fi
done

if ((${#names[@]})); then
  printf '%s\n' "${names[@]}" | tee reports/inventory.txt
else
  : > reports/inventory.txt
fi
