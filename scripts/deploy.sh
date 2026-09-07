#!/usr/bin/bash
set -euo pipefail

curl -LO https://github.com/kingye/jyc/releases/download/nightly/jyc-x86_64-unknown-linux-gnu.tar.gz
tar -xzf jyc-x86_64-unknown-linux-gnu.tar.gz
jyc stop
nohup jyc serve --log-file &
