#!/usr/bin/bash
S6_HOME="$HOME/.local/share/jyc-s6"

case "$1" in
  status)
    "$S6_HOME/command/s6-svstat" "$S6_HOME/service/jyc"
    ;;
  restart)
    "$S6_HOME/command/s6-svc" -r "$S6_HOME/service/jyc"
    ;;
  stop)
    "$S6_HOME/command/s6-svc" -d "$S6_HOME/service/jyc"
    ;;
  start)
    "$S6_HOME/command/s6-svc" -u "$S6_HOME/service/jyc"
    ;;
  *)
    echo "Usage: $0 {status|restart|stop|start}"
    exit 1
    ;;
esac