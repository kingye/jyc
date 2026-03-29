#!/usr/bin/bash
S6_HOME="$HOME/.local/share/jyc-s6"
PROJECT_HOME="/home/jiny/projects/jyc"

cd "$PROJECT_HOME"

if [ ! -f "$PROJECT_HOME/jyc" ]; then
  echo "Error: jyc binary not found at $PROJECT_HOME/jyc"
  echo "Run: cargo build --release && cp target/release/jyc jyc"
  exit 1
fi

mkdir -p "$S6_HOME/service"

mkdir -p "$S6_HOME/s6-rc.d/user/contents.d"
cp -r "$PROJECT_HOME/s6-rc.d/jyc" "$S6_HOME/s6-rc.d/"
touch "$S6_HOME/s6-rc.d/user/contents.d/jyc"

exec "$S6_HOME/command/s6-rc-init" "$S6_HOME/service" && \
     "$S6_HOME/command/s6-rc" -u "$S6_HOME/service"