#!/usr/bin/env bash
# Hide the tmux status bar.
set -u

command -v tmux >/dev/null 2>&1 || exit 0
[ -n "${TMUX:-}" ] || exit 0

# Clear any stale session-local override, then set the global option so the
# bar hides on every session (not just the daemon's home session).
tmux set-option -u status 2>/dev/null
tmux set-option -g status off 2>/dev/null

exit 0
