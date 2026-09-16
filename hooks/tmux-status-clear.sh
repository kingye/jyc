#!/usr/bin/env bash
# Clear the status-left text written by tmux-status-on.sh; status bar stays on.
set -u

command -v tmux >/dev/null 2>&1 || exit 0
[ -n "${TMUX:-}" ] || exit 0

tmux set-option status-left "" 2>/dev/null

exit 0
