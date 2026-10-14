# Initial Omarchy command ports

Ten `.program` files reproduce the native command interfaces of ten small
scripts from [Omarchy v4.0.4](https://github.com/omacom/omarchy/tree/c668141e9c42b13c80c9ca4ea108e11708c5e8a5/bin).
They require the **0.9.0-rc.4 or current source runtime**, an Omarchy installation and its
normal desktop session to perform real actions. The installed 0.8.0 runtime
does not support their positional input declarations.

For example:

```sh
execute examples/omarchy-ports/omarchy-notification-dismiss.program -- "Update complete"
```

```text
#!/usr/bin/env -S execute --
accept arguments as list of text from arguments
accept summary as text from argument 1
run "omarchy-shell" with arguments ["-q", "notifications", "dismiss", summary]
```

The variadic declaration accepts the extra arguments that these upstream
wrappers ignore. The summary remains one literal argument, including spaces,
newlines, wildcard characters and option-looking text. To run a file directly,
mark your copy executable and put a source-built `execute` on `PATH`.

Run the safe interface tests from the project root:

```sh
cargo build --locked --release
python3 tests/omarchy_interfaces.py
```

The tests **never invoke real Omarchy, Hyprland, service, package or desktop
commands**. They run pinned Bash source and the ports with an isolated `PATH`
containing fake backends. Thirty successful calls check exact argv, stdout,
stderr and status; ten failed calls check native status 7 and explicitly
different output handling. A missing notification summary has the same failure
status but different usage/validation output. Source hashes are checked against
[the pinned inventory](../../docs/omarchy-inventory.json); original source and
its license are retained under `tests/fixtures/omarchy-v4.0.4`.

These are interface ports, not certified replacements. Execute supervises a
child instead of preserving Bash `exec` PID identity. It enriches failure
diagnostics and discards failed finite-command stdout. This suite does not
verify foreground sessions, terminal ownership, signals, launch detachment,
actual desktop effects, or package operations. The `setsid` example proves
argument forwarding only; the fake backend never starts Nautilus. No system
commands are replaced or installed by this directory.

The remaining production gates are in
[production-readiness.md](../../docs/production-readiness.md).
