This fixture preserves `bin/omarchy-notification-time` byte for byte from
[omacom/omarchy at c668141e9c42b13c80c9ca4ea108e11708c5e8a5](https://github.com/omacom/omarchy/blob/c668141e9c42b13c80c9ca4ea108e11708c5e8a5/bin/omarchy-notification-time).
Its SHA-256 is recorded in `provenance.json` and asserted by the test. The shared
`examples/omarchy-rewrites/UPSTREAM-LICENSE` preserves the upstream license. No
upstream file is edited.

The Execute program calls only the native name `omarchy-notification-send`. It
reads the injectable clock with `time.now`, renders the upstream format with
`time.format` in zone `local`, and keeps the notification options. The
`ignored_arguments` declaration preserves Bash's acceptance of arbitrary unused
positional arguments, including empty strings and option-like values. Neither
version reads HOME or OMARCHY_PATH itself; native tools inherit them, PATH, TZ
and locale. The runtime environment remains the caller's choice. `EXECUTE_NOW`
or `execute ... --now EPOCH` pins the clock; the mock `date` still serves only
the Bash original so both sides render the same fixed instant.

Bash command substitution strips all trailing LF bytes and ignores an ordinary
date failure when evaluating the notification argument. The rewrite spawns no
`date` and no `jq`: `time.format` emits text with no trailing newline, so no
stripping is needed. An empty or failing `date` mock therefore no longer changes
the rewrite's notification; those cases assert the divergence explicitly. It
does not evaluate the summary as a program. All application sequencing is in
the `.program` file.

Run from the project root:

```sh
python3 -m unittest discover -s tests/omarchy_rewrites -p test_01_notification_time.py -v
```

`EXECUTE_TEST_BINARY` overrides the existing reference executable. Optional
`OMARCHY_REWRITE_EVIDENCE_DIR` writes per-case JSON observations. Tests isolate
HOME, OMARCHY_PATH, cwd and PATH. Only the Bash original's `date` and the
notification boundary are mocked. The notification mock records requests
instead of changing the desktop. The two executions share the same isolated
fixture configuration, resetting observation logs between runs.

Cases assert exact command names/argv, stdout, exit status, notification request
records and empty HOME/OMARCHY_PATH side-effect snapshots. They cover the fixed
clock matching the Bash original, extra trailing LFs, shell-looking summary
bytes, ignored literal argv, empty and failing date mocks diverging to the
fixed-clock summary, notification status 23, and `--now` pinning without
ambient state. Successful executions compare stderr exactly.

Limits: Execute adds its own diagnostic after an unhandled native failure, so
notification-failure stderr has the upstream bytes as a prefix, followed by an
Execute diagnostic. Execute discards a failing command's stdout, unlike Bash;
the failure case uses an empty stdout boundary. Invalid UTF-8, NUL output and
hostile command replacement are outside this UTF-8 fixture contract. These
tests do not claim full Bash/runtime failure equivalence.
