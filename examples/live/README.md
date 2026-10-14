# Agent-controlled overdue queue monitor

Copy this folder to an owned workspace, then run there:

```sh
execute live serve queue-monitor.program --state-file queue-state.json --socket /tmp/execute-queue.sock --paused
```

In another terminal:

```sh
execute live read /bindings --socket /tmp/execute-queue.sock
execute live update /bindings/threshold --json '"10"' --socket /tmp/execute-queue.sock
execute live control resume --socket /tmp/execute-queue.sock
execute live read /runtime/last_output --socket /tmp/execute-queue.sock
execute live control stop --socket /tmp/execute-queue.sock
```

Allow one second for the first scheduled report. Replace `queue.json` with your
own job queue. Each pending job needs an ID, owner, and numeric `age_minutes`.
`queue-report.json` receives the matching jobs and count. With the supplied data,
threshold `"30"` produces one overdue job; `"10"` produces two. No network or OS
scheduler changes are involved. The timer, filtering, and save workflow are in
`queue-monitor.program`.

See [the live-control API](../../docs/live-control.md) for CRUD, atomic patches,
revision checks, lifecycle controls, listener input, and limits.

## Live widget console

```sh
execute live serve widget-console.program --socket /tmp/widget-console.sock --paused
```

Add `--headless` for a background agent. From another process:

```sh
execute live update /ui/programs/console/widgets/message/text --json '"agent input"' --socket /tmp/widget-console.sock
execute live event --json '{"type":"submit","program":"console","widget":"message"}' --socket /tmp/widget-console.sock
execute live control resume --socket /tmp/widget-console.sock
execute live read /ui/programs/console/widgets/result/text --socket /tmp/widget-console.sock
```

The form's source handler runs `printf` with the edited value. The dashboard's
table, bars, and graph can be populated under `/ui/programs/dashboard/widgets`.
Stop with `execute live control stop --socket /tmp/widget-console.sock`.
