#!/usr/bin/bash
set -euo pipefail

curl -LO https://github.com/kingye/jyc/releases/download/nightly/jyc-x86_64-unknown-linux-gnu.tar.gz
tar -xzf jyc-x86_64-unknown-linux-gnu.tar.gz
jyc stop
pkill -f 'jyc serve' 2>/dev/null || true
sleep 3
pkill -9 -f 'jyc serve' 2>/dev/null || true
nohup jyc serve --log-file &
