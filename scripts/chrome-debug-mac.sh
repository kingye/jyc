#!/usr/bin/env bash
# Kill all Chrome instances, then relaunch one with remote debugging on :9222.
set -euo pipefail

pkill -f "Google Chrome" 2>/dev/null || true
sleep 1 # let it release the SingletonLock in the user-data-dir

"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --remote-debugging-port=9222 \
  --user-data-dir="$HOME/ChromeDebug" &
