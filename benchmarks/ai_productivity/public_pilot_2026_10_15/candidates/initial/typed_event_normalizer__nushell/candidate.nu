let json_lines = open --raw $env.EVAL_EVENTS | lines | each {|line|
    let event = $line | from json
    let actor = if "actor" in ($event | columns) { $event.actor } else { null }
    {
        event_id: $event.id,
        occurred_at: $event.at,
        kind: $event.type,
        source: $event.source,
        actor: $actor
    } | to json
}
let output = if ($json_lines | length) > 0 {
    ($json_lines | str join "\n") + "\n"
} else {
    ""
}
mkdir reports
$output | save --raw reports/normalized.jsonl
print --no-newline $output
