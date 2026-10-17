#!/bin/bash
#
# Copied from the Omarchy project (https://github.com/omacom/omarchy, release v4.0.4,
# commit c668141e9c42b13c80c9ca4ea108e11708c5e8a5), bin/omarchy-pkg-present.
# Copyright (c) David Heinemeier Hansson. Licensed under the MIT License; see
# ../LICENSE. The Execute port of this script is port.program in this directory.
#

# omarchy:summary=Returns true if all of the named packages are installed on the system (or false if any of them are missing).
# omarchy:args=<packages...>

for pkg in "$@"; do
  pacman -Q -- "$pkg" &>/dev/null || exit 1
done

exit 0
