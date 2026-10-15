This fixture preserves `bin/omarchy-notification-time` byte for byte from
[omacom/omarchy at c668141e9c42b13c80c9ca4ea108e11708c5e8a5](https://github.com/omacom/omarchy/blob/c668141e9c42b13c80c9ca4ea108e11708c5e8a5/bin/omarchy-notification-time).
Its SHA-256 is recorded in `provenance.json` and asserted by the test. The shared
`examples/omarchy-rewrites/UPSTREAM-LICENSE` preserves the upstream license. No
upstream file is edited.

The Execute program calls the original native names `date` and
`omarchy-notification-send`. It keeps the exact format argument and notification
options. The `ignored_arguments` declaration preserves Bash's acceptance of
arbitrary unused positional arguments, including empty strings and option-like
values. Neither version reads HOME or OMARCHY_PATH itself; native tools inherit
them, PATH, TZ and locale. The runtime environment remains the caller's choice.

Bash command substitution strips all trailing LF bytes and ignores an ordinary
date failure when evaluating the notification argument. Execute explicitly keeps
date's status so execution continues, then invokes native `jq -Rsj` with a single
text-normalization expression. This adds a jq dependency. It does not evaluate
the summary as a program. All application sequencing is in the `.program` file.

Run from the project root:

```sh
python3 -m unittest discover -s tests/omarchy_rewrites -p test_01_notification_time.py -v
```

`EXECUTE_TEST_BINARY` overrides the existing reference executable. Optional
`OMARCHY_REWRITE_EVIDENCE_DIR` writes per-case JSON observations. Tests isolate
HOME, OMARCHY_PATH, cwd and PATH. Only `date` and the notification boundary are
mocked; actual jq performs the text transformation. The notification mock records
requests instead of changing the desktop. The two executions share the same
isolated fixture configuration, resetting observation logs between runs.

Cases assert exact command names/argv, stdout, exit status, notification request
records and empty HOME/OMARCHY_PATH side-effect snapshots. They cover success,
empty date output, multiline and shell-looking summary bytes, all trailing LF
removal, ignored literal argv, date status 7 with partial output and continuation,
and notification status 23. Successful executions and the ordinary date-failure
case also compare stderr exactly. Static check and explain outputs are saved
with the run evidence, including the real jq argv.

Limits: Execute adds its own diagnostic after an unhandled native failure, so
notification-failure stderr has the upstream bytes as a prefix, followed by an
Execute diagnostic. Execute discards a failing command's stdout, unlike Bash;
the failure case uses an empty stdout boundary. Spawn, signal, timeout and output
limit errors stop Execute rather than becoming a captured ordinary date exit.
The grammar has no direct shell-command-substitution or trailing-LF trim
operation, so jq is necessary for exact summary bytes within this rewrite.
Invalid UTF-8, NUL output and hostile command replacement are outside this UTF-8
fixture contract. These tests do not claim full Bash/runtime failure equivalence.
