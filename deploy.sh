#!/usr/bin/bash
# JYC deploy: download nightly, swap binary in place, stop old daemon, start new.
#
# Self-bootstrapping: this script is designed to be invoked from inside the
# running jyc (e.g., as a /deploy custom command). When `jyc stop` SIGTERMs
# the parent (running jyc), this script keeps running because it's a separate
# process; once the parent exits, we start the new jyc detached via nohup.
#
# Must be run from the directory holding the jyc binary (the symlink
# /usr/local/bin/jyc should point here).
set -euo pipefail

LOGFILE="$PWD/jyc.log"
PIDFILE="$PWD/jyc.pid"
TARBALL_URL="https://github.com/kingye/jyc/releases/download/nightly/jyc-x86_64-unknown-linux-gnu.tar.gz"

echo "=== JYC Deployment ==="
echo "Install path: $PWD"
echo ""

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

# 1. Download
echo "Downloading nightly ..."
curl -fsSL -o "$TMPDIR/jyc.tar.gz" "$TARBALL_URL"

# 2. Extract (overwrites ./jyc; /usr/local/bin/jyc symlink untouched)
echo "Extracting ..."
tar xzf "$TMPDIR/jyc.tar.gz" -C .

# 3. Stop existing daemon (SIGTERMs our parent; we survive because we're separate)
echo "Stopping jyc ..."
if [ -x /usr/local/bin/jyc ] && [ -f "$PIDFILE" ]; then
  /usr/local/bin/jyc stop || echo "  (stop returned non-zero — continuing)"
else
  echo "  (no PID file or no installed binary — skipping)"
fi

# Wait for old PID file to disappear (= old jyc fully exited, socket released).
for i in 1 2 3 4 5 6 7 8 9 10; do
  [ ! -f "$PIDFILE" ] && break
  sleep 1
done

# 4. Start new daemon detached. The new jyc writes its own PID file.
echo "Starting jyc ..."
nohup ./jyc serve --workdir "$PWD" >> "$LOGFILE" 2>&1 &
NEW_PID=$!
disown "$NEW_PID" 2>/dev/null || true

sleep 2
if kill -0 "$NEW_PID" 2>/dev/null; then
  echo "  Started (PID $NEW_PID)"
else
  echo "  Failed to start — see $LOGFILE" >&2
  exit 1
fi

echo ""
echo "=== Done ==="