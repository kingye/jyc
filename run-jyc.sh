#!/usr/bin/bash

# JYC startup script for systemd service.
# Auto-detects paths from systemd service configuration.
# Environment variables are sourced from ~/.zshrc.local.

# Source environment variables
if [ -f ~/.zshrc.local ]; then
  set -a
  source ~/.zshrc.local
  set +a
fi

# Auto-detect JYC binary and workdir from systemd service
# These can be overridden via environment variables:
#   JYC_BINARY - path to jyc binary
#   JYC_WORKDIR - path to jyc data directory
JYC_BINARY="${JYC_BINARY:-$(which jyc 2>/dev/null)}"
JYC_WORKDIR="${JYC_WORKDIR:-$(pwd)}"

if [ -z "$JYC_BINARY" ] || [ ! -f "$JYC_BINARY" ]; then
  echo "ERROR: jyc binary not found. Set JYC_BINARY or ensure jyc is in PATH."
  exit 1
fi

cd "$JYC_WORKDIR"
exec "$JYC_BINARY" monitor --workdir "$JYC_WORKDIR" --debug
