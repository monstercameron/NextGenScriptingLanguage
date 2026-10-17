#!/bin/bash
#
# Copied from the Omarchy project (https://github.com/omacom/omarchy, release v4.0.4,
# commit c668141e9c42b13c80c9ca4ea108e11708c5e8a5), bin/omarchy-toggle-bar.
# Copyright (c) David Heinemeier Hansson. Licensed under the MIT License; see
# ../LICENSE. The Execute port of this script is port.program in this directory.
#

# omarchy:summary=Toggle bar visibility without killing the Omarchy shell (on hides the bar, off shows it)
# omarchy:args=[toggle|on|off]
# omarchy:examples=omarchy toggle bar | omarchy toggle bar off | omarchy toggle bar on

omarchy-toggle bar-off "${1:-toggle}"

# The shell's watch on the toggles directory can miss flag changes that land in
# quick succession, stranding the bar off screen until the shell restarts.
# Nudge the bar to re-read the flag; quiet best-effort so the toggle still
# works when the shell is not up.
omarchy-shell -q omarchy.bar syncHidden
