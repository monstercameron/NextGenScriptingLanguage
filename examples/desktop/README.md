# Backend-neutral desktop graph demo

This example uses five application components with one graph scheduler:

1. A persistent pulse source (`every 1 seconds`).
2. A clock source applet (`desktop.clock_local` or `desktop.clock_remote`).
3. A generic time formatter (`desktop.clock_format`).
4. A text widget and button widget (`tui:*` or `quickshell:*`).
5. A remote sync applet (`desktop.fixture_sync`) connected to the button's
   typed `clicked` event and served by a loopback-only local fixture.

The graph template binds the same typed source and formatter ports for all four
combinations of local/remote clock source and TUI/Quickshell renderer. The QML
file remains a generic renderer; it contains no clock formatting, sync policy,
or application behavior. The clock and formatter workers are persistent
JSON-lines applets. The remote fixture uses a pinned descriptor and no
credentials or non-loopback network access.

Validate one combination without starting a renderer or server:

```sh
python3 tools/desktop_demo.py --producer local --renderer tui --validate-only
```

Run interactively from the repository root after building `execute` and
installing the selected renderer's runtime tools:

```sh
python3 tools/desktop_demo.py --producer local --renderer tui
python3 tools/desktop_demo.py --producer remote --renderer tui
python3 tools/desktop_demo.py --producer local --renderer quickshell
python3 tools/desktop_demo.py --producer remote --renderer quickshell
```

Press the button to send a sync request to the local fixture. Press Ctrl-C to
cancel the graph; the graph owns worker and renderer lifetimes. The runner
creates copied manifests and the generated `.program` only in a private
temporary directory and removes them on exit. It does not install packages,
modify shell/compositor configuration, or qualify as native Omarchy
certification. Real Quickshell/Hyprland acceptance remains pending the separate
opt-in harness in `tools/omarchy_acceptance.py`.

The current adapter launches one Quickshell process per renderer node. This is
appropriate for the small demo and establishes explicit ownership, but it is
not a measured or resource-efficient architecture for hundreds or thousands
of widgets. A shared per-graph Quickshell session is a follow-up design item.
