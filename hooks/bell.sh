#!/usr/bin/env bash
set -u

case "$(uname -s)" in
  Darwin)
    afplay /System/Library/Sounds/Glass.aiff >/dev/null 2>&1 &
    ;;
  *)
    if command -v paplay >/dev/null 2>&1; then
      paplay /usr/share/sounds/freedesktop/stereo/complete.oga >/dev/null 2>&1 &
    elif command -v aplay >/dev/null 2>&1; then
      aplay /usr/share/sounds/alsa/Front_Center.wav >/dev/null 2>&1 &
    else
      printf '\a' >/dev/tty 2>/dev/null || printf '\a'
    fi
    ;;
esac

exit 0
