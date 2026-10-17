#!/bin/bash
#
# Copied from the Omarchy project (https://github.com/omacom/omarchy, release v4.0.4,
# commit c668141e9c42b13c80c9ca4ea108e11708c5e8a5), bin/omarchy-toggle-theme-sync.
# Copyright (c) David Heinemeier Hansson. Licensed under the MIT License; see
# ../LICENSE. The Execute port of this script is port.program in this directory.
#

# omarchy:summary=Toggle mirroring theme changes to and from your herdr machines

omarchy-toggle herdr-theme-sync

if omarchy-toggle-enabled herdr-theme-sync; then
  omarchy-notification-send -g 󰸌 "$(omarchy-i18n themeSync.enabled)"
else
  omarchy-notification-send -g 󰸌 "$(omarchy-i18n themeSync.disabled)"
fi
