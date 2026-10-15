#!/bin/bash
set -euo pipefail
jq -c '{event_id: .id, occurred_at: .at, kind: .type, source: .source, actor: (.actor // null)}' "$EVAL_EVENTS" | tee reports/normalized.jsonl
