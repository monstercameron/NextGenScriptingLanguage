# Desktop graph acceptance

`desktop_graph_acceptance.py` is an opt-in live acceptance gate for the
backend-neutral desktop graph. It runs the checked-in graph through
`execute run` twice, once with the local clock worker and once with the remote
clock applet served by the loopback fixture. Each run must start the expected
clock and formatter workers, open exactly two graph-owned Quickshell windows,
and route `Tab`/`Return` input through a Hyprland-verified focused window until
the fixture's `sync_count` advances. The harness does not speak the renderer
protocol.

After the button route succeeds, the harness sends SIGINT to the Execute PID
alone. The accepted graph-driver cancellation path returns exit status 1 with
`graph session cancelled` after joining graph tasks. The harness records each
owned descendant using its Linux PID and `/proc` start time, then requires the
Execute process, Quickshell renderers, and applet workers it observed to be
gone. A cancellation timeout, any fallback signal, or any surviving identity
fails the run; an unreadable or malformed process identity is recorded as
unknown and also fails teardown. Cleanup actions remain in the receipt.

The live command is guarded by both `--run-in-disposable-arch` and the same
disposable-session environment marker used by the desktop smoke. It checks an
Arch Wayland session, Hyprland, Quickshell, `wtype`, D-Bus, logind and the
user-owned `XDG_RUNTIME_DIR` before it starts the graph. It creates generated
manifests, graph input and logs below a private temporary directory in that
runtime directory, then removes the directory during cleanup. OS, environment,
runtime-directory, binary, tool-availability and disposable-marker checks run
before any Hyprland, D-Bus or logind probe; a static preflight failure returns
without invoking those session commands.

From the repository root in the disposable Arch guest, after building the
candidate binary:

```sh
EXECUTE_OMARCHY_DISPOSABLE=1 \
EXECUTE_OMARCHY_SESSION_ID=disposable-graph-run \
python3 tools/desktop_graph_acceptance.py \
  --run-in-disposable-arch \
  --execute target/release/execute \
  --evidence /var/tmp/execute-desktop-ci/artifacts/graph-evidence.json
```

Without the explicit flag and both environment values, the harness prints a
pending report and runs no desktop commands. Evidence files are created
exclusively and are not overwritten. This gate proves graph-to-Quickshell
button routing and observed process cleanup on one disposable Arch/Hyprland
session. The disposable guest flow runs it only after the direct Quickshell
acceptance succeeds, retains its stdout, stderr and `graph-evidence.json`
separately, and withholds `READY` unless both scenarios prove a sync-counter
increase and clean natural cancellation teardown. The host gate independently
checks the graph report's actual local/remote scenario fields. This does not
certify Omarchy, physical input, display changes, login/logout, compositor
reload, deployment or session-wide recovery.
