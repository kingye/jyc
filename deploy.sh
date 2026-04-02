#!/bin/bash
set -e

# Ignore hangup signal — this script may outlive its parent process
# (e.g., when jyc deploys itself via OpenCode).
trap '' HUP

echo "=== JYC Deployment Script ==="

# Set directories (can be overridden via environment)
WORKDIR="${WORKDIR:-/home/jiny/projects/jyc-data/jiny283a/workspace/self-hosting-jyc}"
JYC_DIR="${WORKDIR}/jyc"
INSTALL_DIR="${INSTALL_DIR:-/home/jiny/projects/jyc}"

NEW_BINARY="${JYC_DIR}/target/release/jyc"
INSTALL_BINARY="${INSTALL_DIR}/jyc"

echo "Source directory: ${WORKDIR}"
echo "JYC directory: ${JYC_DIR}"
echo "Installation directory: ${INSTALL_DIR}"
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
