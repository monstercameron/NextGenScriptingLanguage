"""Test-only argv recorder; delegates every operation to an unchanged native CLI."""
import json
import os
from pathlib import Path
import sys

tool = Path(sys.argv[0]).name
with open(os.environ["OMARCHY_TEST_ARGV_LOG"], "a", encoding="utf-8") as log:
    log.write(json.dumps([tool, *sys.argv[1:]]) + "\n")
real_tool = json.loads(os.environ["OMARCHY_TEST_REAL_TOOLS"])[tool]
os.execv(real_tool, [tool, *sys.argv[1:]])
