#!/usr/bin/env bash
# Show the tmux status bar with a "turn complete" notice.
set -u

command -v tmux >/dev/null 2>&1 || exit 0
[ -n "${TMUX:-}" ] || exit 0

agent="${JYC_AGENT:-agent}"
topic="${JYC_TOPIC:-topic}"

# Clear any stale session-local override, then set the global option so the
# notice shows on every session (not just the daemon's home session).
tmux set-option -u status 2>/dev/null
tmux set-option -u status-left 2>/dev/null
tmux set-option -g status on 2>/dev/null
tmux set-option -g status-left " ✅ ${agent}·${topic} $(date +%H:%M) " 2>/dev/null

exit 0
