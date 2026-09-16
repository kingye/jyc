#!/usr/bin/env bash
# Show the tmux status bar with a "turn complete" notice.
set -u

command -v tmux >/dev/null 2>&1 || exit 0
[ -n "${TMUX:-}" ] || exit 0

agent="${JYC_AGENT:-agent}"
topic="${JYC_TOPIC:-topic}"

tmux set-option status on 2>/dev/null
tmux set-option status-left " ✅ ${agent}·${topic} $(date +%H:%M) " 2>/dev/null

exit 0
