"""Fixture-only command recorder. Never forwards a removal outside owned HOME."""
import json
import os
from pathlib import Path
import sys

name = Path(sys.argv[0]).name
args = sys.argv[1:]
root = Path(os.environ["OMARCHY_TEST_ROOT"]).resolve()
home = Path(os.environ["HOME"]).resolve()
assert root.name.startswith("webapp-remove-") and root.is_dir()
assert home.parent == root and home.name == "home"
log = Path(os.environ["OMARCHY_TEST_LOG"])
assert log.parent.resolve() == root
with log.open("a") as stream:
    stream.write(json.dumps([name, *args]) + "\n")
if name == "rm":
    assert args[0] == "-f"
    for raw in args[1:]:
        target = Path(raw)
        assert target.is_absolute() and target.resolve().is_relative_to(home)
        assert target.resolve() != home
    os.execv(os.environ["OMARCHY_TEST_REAL_RM"], ["rm", *args])
if name == "omarchy-menu-select":
    sys.stdout.write(os.environ.get("OMARCHY_TEST_SELECTION", ""))
    sys.exit(int(os.environ.get("OMARCHY_TEST_MENU_STATUS", "0")))
if name == "omarchy-notification-send":
    sys.stdout.write(os.environ.get("OMARCHY_TEST_NOTIFY_OUTPUT", ""))
    sys.exit(int(os.environ.get("OMARCHY_TEST_NOTIFY_STATUS", "0")))
if name == "update-desktop-database":
    assert args == [str(home / ".local/share/applications") + "/"]
    (home / "database-refreshed").write_text("called\n")
    sys.stdout.write("suppressed cache output\n")
    sys.exit(int(os.environ.get("OMARCHY_TEST_DATABASE_STATUS", "0")))
raise AssertionError(name)
