#!/usr/bin/bash

# JYC startup script for systemd service.
# Reads JYC_BINARY and JYC_WORKDIR from environment variables.
# These MUST be set in ~/.zshrc.local or the systemd service EnvironmentFile.

# Source environment variables
if [ -f ~/.zshrc.local ]; then
  set -a
  source ~/.zshrc.local
  set +a
fi

# JYC_BINARY and JYC_WORKDIR must be set in environment
if [ -z "$JYC_BINARY" ]; then
  echo "ERROR: JYC_BINARY environment variable not set."
  echo "Add 'export JYC_BINARY=/path/to/jyc' to ~/.zshrc.local"
  exit 1
fi

if [ -z "$JYC_WORKDIR" ]; then
  echo "ERROR: JYC_WORKDIR environment variable not set."
  echo "Add 'export JYC_WORKDIR=/path/to/jyc-data' to ~/.zshrc.local"
  exit 1
fi

if [ ! -f "$JYC_BINARY" ]; then
  echo "ERROR: JYC_BINARY not found at: $JYC_BINARY"
  exit 1
fi

cd "$JYC_WORKDIR"
exec "$JYC_BINARY" serve --workdir "$JYC_WORKDIR" --debug
