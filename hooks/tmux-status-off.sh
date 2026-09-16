#!/usr/bin/env bash
# Hide the tmux status bar.
set -u

command -v tmux >/dev/null 2>&1 || exit 0
[ -n "${TMUX:-}" ] || exit 0

tmux set-option status off 2>/dev/null

exit 0
