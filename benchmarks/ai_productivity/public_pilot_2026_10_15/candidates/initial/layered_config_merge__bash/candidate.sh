#!/usr/bin/env bash
set -euo pipefail

: "${EVAL_SETTINGS:?EVAL_SETTINGS must name the input JSON document}"
mkdir -p reports

jq -cS '
  def merge($left; $right):
    if ($left | type) == "object" and ($right | type) == "object" then
      reduce ($right | keys_unsorted[]) as $key
        ($left;
          .[$key] = if has($key) then merge(.[$key]; $right[$key]) else $right[$key] end
        )
    else
      $right
    end;
  .layers | reduce .[] as $layer ({}; merge(.; $layer))
' "$EVAL_SETTINGS" | tee reports/effective.json
