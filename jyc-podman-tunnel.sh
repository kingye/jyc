#!/bin/bash
# jyc-podman-tunnel — Forward the jyc inspect server port from Podman Machine VM to Mac.
#
# Only needed on macOS with Podman Machine. On bare metal or Linux Docker,
# the inspect server is directly accessible on localhost:9876.
#
# Usage:
#   ./jyc-podman-tunnel        # start tunnel, then run jyc dashboard
#   ./jyc-podman-tunnel stop   # kill existing tunnel

set -e

PORT=9876

# Get Podman Machine SSH details
SSH_PORT=$(podman machine inspect 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin)[0]['SSHConfig']['Port'])")
SSH_KEY=$(podman machine inspect 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin)[0]['SSHConfig']['IdentityPath'])")
SSH_USER=$(podman machine inspect 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin)[0]['SSHConfig']['RemoteUsername'])")

if [ -z "$SSH_PORT" ] || [ -z "$SSH_KEY" ]; then
    echo "Error: Could not get Podman Machine SSH config. Is Podman Machine running?"
    exit 1
fi

stop_tunnel() {
    # Kill any existing tunnel on this port
    pids=$(lsof -ti tcp:$PORT -sTCP:LISTEN 2>/dev/null || true)
    if [ -n "$pids" ]; then
        echo "Stopping existing tunnel (PID: $pids)"
        echo "$pids" | xargs kill 2>/dev/null || true
    else
        echo "No tunnel running on port $PORT"
    fi
}

if [ "${1:-}" = "stop" ]; then
    stop_tunnel
    exit 0
fi

# Stop any existing tunnel first
stop_tunnel 2>/dev/null

# Start SSH tunnel
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -i "$SSH_KEY" -p "$SSH_PORT" \
    -L "$PORT:localhost:$PORT" \
    -N -f "$SSH_USER@localhost" 2>/dev/null

# Verify
if nc -z -w 2 localhost $PORT 2>/dev/null; then
    echo "Tunnel active: localhost:$PORT → Podman VM:$PORT"
    echo "Run: jyc dashboard"
else
    echo "Warning: Tunnel started but port $PORT not reachable."
    echo "Check that jyc monitor is running with [inspect] enabled."
fi
