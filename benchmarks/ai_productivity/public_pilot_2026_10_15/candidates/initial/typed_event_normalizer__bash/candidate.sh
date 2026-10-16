#!/usr/bin/env bash
set -euo pipefail

mkdir -p reports
jq -c '{event_id: .id, occurred_at: .at, kind: .type, source: .source, actor: (if has("actor") then .actor else null end)}' "$EVAL_EVENTS" | tee reports/normalized.jsonl
