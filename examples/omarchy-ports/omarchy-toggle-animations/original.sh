#!/bin/bash
#
# Copied from the Omarchy project (https://github.com/omacom/omarchy, release v4.0.4,
# commit c668141e9c42b13c80c9ca4ea108e11708c5e8a5), bin/omarchy-toggle-animations.
# Copyright (c) David Heinemeier Hansson. Licensed under the MIT License; see
# ../LICENSE. The Execute port of this script is port.program in this directory.
#

# omarchy:summary=Toggle animations, blur and shadows, which are slow without a GPU
# omarchy:args=[toggle|on|off]
# omarchy:examples=omarchy toggle animations | omarchy toggle animations off | omarchy toggle animations on

# The flag records animations being off, so on and off swap on the way through.
case ${1:-toggle} in
toggle) omarchy-hyprland-toggle no-animations toggle ;;
on) omarchy-hyprland-toggle no-animations off ;;
off) omarchy-hyprland-toggle no-animations on ;;
*)
  echo "Usage: omarchy-toggle-animations [toggle|on|off]" >&2
  exit 1
  ;;
esac

if omarchy-hyprland-toggle-enabled no-animations; then
  omarchy-notification-send -g 󰗘 "$(omarchy-i18n animations.disabled)"
else
  omarchy-notification-send -g 󰗘 "$(omarchy-i18n animations.enabled)"
fi
