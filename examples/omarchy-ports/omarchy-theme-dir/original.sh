#!/bin/bash
#
# Copied from the Omarchy project (https://github.com/omacom/omarchy, release v4.0.4,
# commit c668141e9c42b13c80c9ca4ea108e11708c5e8a5), bin/omarchy-theme-dir.
# Copyright (c) David Heinemeier Hansson. Licensed under the MIT License; see
# ../LICENSE. The Execute port of this script is port.program in this directory.
#

# omarchy:summary=Print the directory holding a theme, preferring a user-installed copy
# omarchy:args=<theme-name>
# omarchy:examples=omarchy theme dir tokyo-night

theme="${1:-}"

if [[ -z $theme ]]; then
  echo "Usage: omarchy-theme-dir <theme-name>" >&2
  exit 1
fi

if [[ -d $HOME/.config/omarchy/themes/$theme ]]; then
  echo "$HOME/.config/omarchy/themes/$theme"
else
  echo "$OMARCHY_PATH/themes/$theme"
fi
