#!/bin/bash
set -e

# Ignore hangup signal — this script may outlive its parent process
# (e.g., when jyc deploys itself via OpenCode).
trap '' HUP

echo "=== JYC Deployment Script ==="

# Auto-detect: script is in the jyc repo directory
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
NEW_BINARY="${SCRIPT_DIR}/target/release/jyc"

# Auto-detect install path from systemd service ExecStart
INSTALL_BINARY=$(systemctl --user show jyc -p ExecStart --value | awk '{print $1}')

if [ -z "$INSTALL_BINARY" ]; then
    echo "ERROR: Cannot detect install path from systemd jyc.service"
    echo "Is the jyc.service installed? Check: systemctl --user status jyc"
    exit 1
fi

echo "Source binary: ${NEW_BINARY}"
echo "Install target: ${INSTALL_BINARY}"
echo ""

# Check if new binary exists
if [ ! -f "${NEW_BINARY}" ]; then
    echo "ERROR: New binary not found at ${NEW_BINARY}"
    echo "Run 'cargo build --release' first"
    exit 1
fi

echo "✓ New binary found: ${NEW_BINARY}"
NEW_VERSION=$("${NEW_BINARY}" --version 2>/dev/null || echo "unknown")
echo "  Version: ${NEW_VERSION}"
echo ""

# Show version of installed binary (if exists)
if [ -f "${INSTALL_BINARY}" ]; then
    echo "Installed binary: ${INSTALL_BINARY}"
    OLD_VERSION=$("${INSTALL_BINARY}" --version 2>/dev/null || echo "unknown")
    echo "  Version: ${OLD_VERSION}"
    echo ""
fi

# Wait for any pending reply to be delivered before stopping the service.
# deploy.sh is launched via systemd-run (returns immediately), and the AI
# sends a reply right after. This delay ensures the reply reaches the user.
echo "Waiting 5 seconds for pending operations..."
sleep 5

# Stop service
echo "Stopping jyc service..."
if ! systemctl --user is-active --quiet jyc; then
    echo "  Service not running (will skip stop)"
else
    systemctl --user stop jyc
    echo "  ✓ Service stopped"
fi
echo ""

# Copy new binary
echo "Copying new binary to ${INSTALL_BINARY}..."
cp "${NEW_BINARY}" "${INSTALL_BINARY}"
chmod +x "${INSTALL_BINARY}"
echo "  ✓ Binary installed"
echo ""

# Start service
echo "Starting jyc service..."
systemctl --user start jyc

# Wait and check status
sleep 2
if systemctl --user is-active --quiet jyc; then
    echo "  ✓ Service started successfully"
else
    echo "  ✗ Service failed to start"
    echo "  Check logs: journalctl --user -u jyc -n 50"
    exit 1
fi
echo ""

# Show service status
echo "Service status:"
systemctl --user status jyc --no-pager | head -3
echo ""

echo "=== Deployment complete ==="
