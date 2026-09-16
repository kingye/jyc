#!/usr/bin/env bash
# Clear the status-left text written by tmux-status-on.sh; status bar stays on.
set -u

command -v tmux >/dev/null 2>&1 || exit 0
[ -n "${TMUX:-}" ] || exit 0

# Clear any stale session-local override, then set the global option so the
# text clears on every session (not just the daemon's home session).
tmux set-option -u status-left 2>/dev/null
tmux set-option -g status-left "" 2>/dev/null

exit 0
