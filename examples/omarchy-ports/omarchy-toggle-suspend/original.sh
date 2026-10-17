#!/bin/bash
#
# Copied from the Omarchy project (https://github.com/omacom/omarchy, release v4.0.4,
# commit c668141e9c42b13c80c9ca4ea108e11708c5e8a5), bin/omarchy-toggle-suspend.
# Copyright (c) David Heinemeier Hansson. Licensed under the MIT License; see
# ../LICENSE. The Execute port of this script is port.program in this directory.
#

# omarchy:summary=Toggle suspend availability in the system menu

if omarchy-sleep-disabled; then
  omarchy-notification-send -g 󰒲 "$(omarchy-i18n suspend.turnedOffOnMachine)"
  exit 0
fi

omarchy-toggle suspend-off

if omarchy-toggle-enabled suspend-off; then
  omarchy-notification-send -g 󰒲 "$(omarchy-i18n suspend.removedFromMenu)"
else
  omarchy-notification-send -g 󰒲 "$(omarchy-i18n suspend.availableInMenu)"
fi
