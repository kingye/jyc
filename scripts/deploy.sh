#!/usr/bin/bash
# JYC deploy: download nightly, swap binary in place, stop old daemon, start new.
#
# Self-bootstrapping: this script is designed to be invoked from inside the
# running jyc (e.g., as a /deploy custom command). When SIGTERMs the parent
# (running jyc), this script keeps running because it's a separate process;
# once the parent exits, we start the new jyc detached via nohup.
#
# CWD-independent: resolves the install dir from PATH (via `command -v jyc`),
# so it works whether invoked from the install dir, a topic workspace, or
# anywhere else. The previous version assumed CWD = install dir, which only
# held for the manual `cd /usr/local/bin && ./deploy.sh` invocation.
set -euo pipefail

JYC_BIN="$(command -v jyc)"
if [[ -z "$JYC_BIN" ]]; then
    echo "ERROR: 'jyc' not found on PATH" >&2
    exit 1
fi
INSTALL_DIR="$(dirname "$JYC_BIN")"
LOGFILE="$INSTALL_DIR/jyc.log"
PIDFILE="$INSTALL_DIR/jyc.pid"
TARBALL_URL="https://github.com/kingye/jyc/releases/download/nightly/jyc-x86_64-unknown-linux-gnu.tar.gz"

echo "=== JYC Deployment ==="
echo "Install path: $INSTALL_DIR"
echo ""

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

# 1. Download
echo "Downloading nightly ..."
curl -fsSL -o "$TMPDIR/jyc.tar.gz" "$TARBALL_URL"

# 2. Extract (overwrites $INSTALL_DIR/jyc; the symlink at /usr/local/bin/jyc
# is preserved because the tarball only contains the ./jyc binary)
echo "Extracting ..."
tar xzf "$TMPDIR/jyc.tar.gz" -C "$INSTALL_DIR"

# 3. Stop existing daemon — drive the kill ourselves rather than going
# through `jyc stop`. The latter removes the PID file even when SIGTERM
# timed out and returns Ok(()) (stop.rs:68-87), which used to leave us
# with a half-dead old jyc + new jyc both running. Now the PID file
# disappears only when the old jyc's PidFileGuard runs (= process
# exited), or we escalate to SIGKILL after a 10s grace period.
echo "Stopping jyc ..."
if [[ -f "$PIDFILE" ]]; then
    OLD_PID="$(cat "$PIDFILE")"
    if [[ -n "$OLD_PID" ]] && kill -0 "$OLD_PID" 2>/dev/null; then
        echo "  Sending SIGTERM to PID $OLD_PID"
        kill -TERM "$OLD_PID" 2>/dev/null || true
        for i in 1 2 3 4 5 6 7 8 9 10; do
            [[ ! -f "$PIDFILE" ]] && break
            sleep 1
        done
        if [[ -f "$PIDFILE" ]]; then
            echo "  SIGTERM timed out — sending SIGKILL"
            kill -KILL "$OLD_PID" 2>/dev/null || true
            sleep 1
        fi
    fi
    rm -f "$PIDFILE"
else
    echo "  (no PID file — skipping)"
fi

# 4. Start new daemon detached. The new jyc writes its own PID file
# within ~100ms of startup; the sleep+check below catches startup failures.
echo "Starting jyc ..."
nohup "$JYC_BIN" serve --workdir "$INSTALL_DIR" >> "$LOGFILE" 2>&1 &
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