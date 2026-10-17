#!/bin/bash
#
# Copied from the Omarchy project (https://github.com/omacom/omarchy, release v4.0.4,
# commit c668141e9c42b13c80c9ca4ea108e11708c5e8a5), bin/omarchy-restart-tmux.
# Copyright (c) David Heinemeier Hansson. Licensed under the MIT License; see
# ../LICENSE. The Execute port of this script is port.program in this directory.
#

# omarchy:summary=Restart tmux if running with the latest configuration

if tmux has-session 2>/dev/null; then
  tmux source-file ~/.config/tmux/tmux.conf
fi
