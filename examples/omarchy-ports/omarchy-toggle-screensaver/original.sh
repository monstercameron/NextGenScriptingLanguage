#!/bin/bash
#
# Copied from the Omarchy project (https://github.com/omacom/omarchy, release v4.0.4,
# commit c668141e9c42b13c80c9ca4ea108e11708c5e8a5), bin/omarchy-toggle-screensaver.
# Copyright (c) David Heinemeier Hansson. Licensed under the MIT License; see
# ../LICENSE. The Execute port of this script is port.program in this directory.
#

# omarchy:summary=Toggle screensaver availability

omarchy-toggle screensaver-off

if omarchy-toggle-enabled screensaver-off; then
  omarchy-notification-send -g 󱄄 "$(omarchy-i18n screensaver.disabled)"
else
  omarchy-notification-send -g 󱄄 "$(omarchy-i18n screensaver.enabled)"
fi
