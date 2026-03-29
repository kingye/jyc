#!/usr/bin/bash
S6_SERVICE_DIR="/run/service"

case "$1" in
  status)
    /usr/bin/s6-svstat "$S6_SERVICE_DIR/jyc"
    ;;
  restart)
    /usr/bin/s6-svc -r "$S6_SERVICE_DIR/jyc"
    ;;
  stop)
    /usr/bin/s6-svc -d "$S6_SERVICE_DIR/jyc"
    ;;
  start)
    /usr/bin/s6-svc -u "$S6_SERVICE_DIR/jyc"
    ;;
  *)
    echo "Usage: $0 {status|restart|stop|start}"
    exit 1
    ;;
esac