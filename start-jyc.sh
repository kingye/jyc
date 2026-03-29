#!/usr/bin/bash
PROJECT_HOME="/home/jiny/projects/jyc"
S6_RC_D="/etc/s6-rc/s6-rc.d"
S6_SERVICE_DIR="/run/service"

cd "$PROJECT_HOME"

if [ ! -f "$PROJECT_HOME/jyc" ]; then
  echo "Error: jyc binary not found at $PROJECT_HOME/jyc"
  echo "Run: cargo build --release && cp target/release/jyc jyc"
  exit 1
fi

mkdir -p "$S6_RC_D/user/contents.d"
cp -r "$PROJECT_HOME/s6-rc.d/jyc" "$S6_RC_D/"
touch "$S6_RC_D/user/contents.d/jyc"

exec /usr/bin/s6-rc-init "$S6_SERVICE_DIR" && \
     /usr/bin/s6-rc -u "$S6_SERVICE_DIR"