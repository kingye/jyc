#!/usr/bin/bash
cd /home/jiny/projects/jyc-data

# Source environment variables from .zshrc.local
if [ -f ~/.zshrc.local ]; then
  set -a
  source ~/.zshrc.local
  set +a
fi

exec /home/jiny/projects/jyc/jyc monitor --workdir /home/jiny/projects/jyc-data --debug
