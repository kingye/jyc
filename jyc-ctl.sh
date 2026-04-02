#!/usr/bin/bash

case "$1" in
  status)
    systemctl --user status jyc
    ;;
  logs)
    journalctl --user -u jyc -n 100 -f
    ;;
  restart)
    systemctl --user restart jyc
    ;;
  stop)
    systemctl --user stop jyc
    ;;
  start)
    systemctl --user start jyc
    ;;
  *)
    echo "Usage: $0 {status|logs|restart|stop|start}"
    echo ""
    echo "Commands:"
    echo "  status   - Show service status"
    echo "  logs     - Follow service logs"
    echo "  restart  - Restart service (after deployment)"
    echo "  stop     - Stop service"
    echo "  start    - Start service"
    exit 1
    ;;
esac