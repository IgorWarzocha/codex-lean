#!/bin/sh
# Python reports expected incompatibility. This boundary also handles startup
# failures and timeouts without giving pacman or Omarchy a failed hook status.
if /usr/bin/timeout --signal=INT --kill-after=5s 120s /usr/bin/python3 -E -s -B \
  "$(/usr/bin/dirname "$0")/git_update.py" --user "${1-}" --personality-file "${2-}"; then
  exit 0
fi

message='Codex personality hook failed or timed out. The app update completed, but personality may not have been reapplied. Check journalctl -t codex-desktop-personality.'
printf '%s\n' "$message" >&2
/usr/bin/timeout 5s /usr/bin/logger --tag codex-desktop-personality --priority user.warning -- "$message" ||
  printf '%s\n' 'Codex personality: journal logging unavailable' >&2
uid=$(/usr/bin/id -u -- "${1-}") || exit 0
if [ -S "/run/user/$uid/bus" ]; then
  /usr/bin/timeout 5s /usr/bin/runuser --user "$1" -- /usr/bin/env -i \
    "XDG_RUNTIME_DIR=/run/user/$uid" "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$uid/bus" \
    /usr/bin/notify-send --app-name='Codex personality' --urgency=critical --expire-time=0 \
    'Codex personality needs attention' "$message" ||
    printf '%s\n' 'Codex personality: desktop notification unavailable' >&2
else
  printf '%s\n' 'Codex personality: no desktop session to notify' >&2
fi
exit 0
