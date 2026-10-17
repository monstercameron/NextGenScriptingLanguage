#!/bin/bash
#
# Copied from the Omarchy project (https://github.com/omacom/omarchy, release v4.0.4,
# commit c668141e9c42b13c80c9ca4ea108e11708c5e8a5), bin/omarchy-hyprland-toggle-disabled.
# Copyright (c) David Heinemeier Hansson. Licensed under the MIT License; see
# ../LICENSE. The Execute port of this script is port.program in this directory.
#

# omarchy:summary=Check if a Hyprland toggle is currently disabled (missing).
# omarchy:args=<flag-name>

[[ ! -f "$HOME/.local/state/omarchy/toggles/hypr/$1.lua" ]]
