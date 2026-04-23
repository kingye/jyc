#!/bin/bash
# OAuth2 Forwarder Host Server
# Starts o2f-server on the host to enable browser-based OAuth2 for JYC container.
#
# Usage:
#   ./o2f-server.sh                 # Start with default port 9191
#   ./o2f-server.sh 8080             # Start with custom port
#   OAUTH2_FORWARDER_PORT=8080 ./o2f-server.sh  # Alternative: via env var
#
# Prerequisites:
#   - Node.js installed on host (for npx)
#   - npm install -g oauth2-forwarder  # One-time setup
#
# The server runs in passthrough mode by default, allowing MCP tools
# (like Jira, Figma) to use device code flows that don't follow
# standard redirect patterns.

set -e

PORT="${1:-${OAUTH2_FORWARDER_PORT:-9191}}"

echo "Starting oauth2-forwarder server on port $PORT..."

if ! command -v o2f-server &> /dev/null; then
    if ! npx --yes oauth2-forwarder --version &> /dev/null; then
        echo "ERROR: oauth2-forwarder not found."
        echo "Please install it with: npm install -g oauth2-forwarder"
        exit 1
    fi
    echo "Using npx fallback..."
    export OAUTH2_FORWARDER_PASSTHROUGH="${OAUTH2_FORWARDER_PASSTHROUGH:-true}"
    exec npx --yes oauth2-forwarder --port "$PORT"
fi

export OAUTH2_FORWARDER_PASSTHROUGH="${OAUTH2_FORWARDER_PASSTHROUGH:-true}"
exec o2f-server --port "$PORT"
