#!/bin/bash
#
# Copied from the Omarchy project (https://github.com/omacom/omarchy, release v4.0.4,
# commit c668141e9c42b13c80c9ca4ea108e11708c5e8a5), bin/omarchy-theme-current.
# Copyright (c) David Heinemeier Hansson. Licensed under the MIT License; see
# ../LICENSE. The Execute port of this script is port.program in this directory.
#

# omarchy:summary=Show current theme
# omarchy:examples=omarchy theme current

THEME_NAME_PATH="$HOME/.local/state/omarchy/current/theme.name"

if [[ -f $THEME_NAME_PATH ]]; then
  sed -E 's/(^|-)([a-z])/\1\u\2/g; s/-/ /g' "$THEME_NAME_PATH"
else
  echo "Unknown"
fi
