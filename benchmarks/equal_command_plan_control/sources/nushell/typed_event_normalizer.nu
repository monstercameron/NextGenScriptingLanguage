^jq -c '{event_id: .id, occurred_at: .at, kind: .type, source: .source, actor: (.actor // null)}' $env.EVAL_EVENTS | ^tee reports/normalized.jsonl
